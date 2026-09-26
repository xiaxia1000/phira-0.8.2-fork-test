//! 可复用的谱面网格列表视图（`ChartsView`）。
//!
//! 设计目的是**复用**：曲库页、活动详情页（UML 中的 `Collection` 元素）、收藏页等
//! 都嵌入同一个 `ChartsView` 来展示谱面网格，差异仅在于数据源与几个交互开关
//! （`allow_edit`/`allow_multi_select`/`can_refresh`）以及调用方对 `clicked_special`、
//! `multi_select`、`take_movement` 等公开字段的响应方式。
//!
//! 关键机制：
//! - **布局模型**：把视口宽度按 `row_num` 等分为列、以 `row_height` 为行高，卡片在每格内留
//!   `CHART_PADDING` 的四周内边距；总高度 = 行数 × `row_height`。
//! - **虚拟滚动/可见区裁剪**：列表可能有上千项，不能全部绘制/命中，故每帧用
//!   `charts_display_range` 依据滚动偏移算出可见行并换算成卡片索引区间；区间外的格子会被
//!   `invalidate` 清掉残留的按钮/长按状态，避免「看不见却能点到」。
//! - **缩略图**：由 `ChartItem` 异步加载，加载完成前用占位（`ChartItem::illu` 的 settled 状态）。
//! - **滚动约定**：先 `scroll.size(...)` 声明视口，再 `scroll.render(ui, |ui| -> (content_w, content_h))`
//!   在其中绘制内容并返回内容总尺寸；`render_release_to_refresh` 负责下拉刷新提示。
//! - **交互**：点击打开单曲页（带图钉式转场 `TransitState`）、长按弹出 `Popup` 菜单
//!   （选多选/移动）、支持多选与「选封面」两种特殊模式。
//!
//! 与 `LibraryPage`/`CollectionPage` 的契约：调用方负责提供/更新 `charts`（`set`/`clear`）、
//! 在转场完成后经 `next_scene` 接管场景切换、读取 `clicked_special`/`take_movement`/
//! `multi_select` 等字段来同步自身状态或发起网络请求。

prpr_l10n::tl_file!("charts_view");

use crate::{
    client::{Chart, ChartRef},
    dir, get_data, get_data_mut,
    icons::Icons,
    page::{ChartItem, Fader, CHOOSE_COVER, CHOSEN_COVER},
    popup::Popup,
    save_data,
    scene::{render_release_to_refresh, SongScene, MP_PANEL},
};
use anyhow::Result;
use core::f32;
use macroquad::prelude::*;
use prpr::{
    core::{Tweenable, BOLD_FONT},
    ext::{semi_black, RectExt, SafeTexture},
    scene::{show_message, NextScene},
    ui::{button_hit, button_hit_large, DRectButton, LongTouchState, Scroll, Ui},
};
use std::{
    ops::Range,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// 全局「谱面列表需要刷新」标志。
/// 任何一处改动谱面数据（如删除、上传完成）后置位，各页面在 `need_update` 中一次性读取并清零，
/// 从而触发重新拉取；用原子量是为了让后台任务也能安全置位。
pub static NEED_UPDATE: AtomicBool = AtomicBool::new(false);

/// 卡片相对格子尺寸的四周内边距比例：让卡片之间留出间隙，避免相邻缩略图贴在一起。
const CHART_PADDING: f32 = 0.013;
/// 从单曲页返回后卡片淡入的时长（配合 `back_fade_in` 状态使用）。
const BACK_FADE_IN_TIME: f32 = 0.2;

/// 打开/返回单曲页的转场时长。
/// 开启「减少动态效果」时返回 `None`，表示转场瞬时完成（不播放放大/收起动画）。
fn transit_time() -> Option<f32> {
    if get_data().prefer_reduced_motion {
        None
    } else {
        Some(0.4)
    }
}

/// 网格中的一张卡片。
///
/// `chart` 为 `None` 时表示「特殊格」：不是谱面，而是列表装饰/表头（渲染为抽象占位图并置
/// `clicked_special`），移动谱面时需把表头排除在外（见 `has_header`）。
pub struct ChartDisplayItem {
    /// 谱面数据；`None` 表示特殊格（表头/占位）。
    pub chart: Option<ChartItem>,
    /// 右上角/左上角角标字符：`+` = 稳定请求，`*` = 未审核。
    symbol: Option<char>,
    /// 卡片命中按钮。
    btn: DRectButton,
    /// 长按状态，用于触发操作菜单。
    long_touch: LongTouchState,
}

// 卡片的构造。远端谱面与本地谱面都统一成 `ChartItem`，故上层无需区分来源。
impl ChartDisplayItem {
    /// 以给定谱面与角标构造卡片（并初始化空按钮与长按状态）。
    pub fn new(chart: Option<ChartItem>, symbol: Option<char>) -> Self {
        Self {
            chart,
            symbol,
            btn: DRectButton::new(),
            long_touch: LongTouchState::default(),
        }
    }

    /// 由远端谱面 `Chart` 构造卡片，并按谱面状态推导角标：
    /// 稳定请求显示 `+`，未审核显示 `*`，否则无角标。
    pub fn from_remote(chart: &Chart) -> Self {
        Self::new(
            Some(ChartItem::from_remote(chart)),
            if chart.stable_request {
                Some('+')
            } else if !chart.reviewed {
                Some('*')
            } else {
                None
            },
        )
    }
}

/// 打开单曲页的转场状态：让被点击的卡片先「放大铺满」到全屏再切换场景，返回时反向收起。
struct TransitState {
    /// 触发的卡片索引（用于在渲染中定位并回填 `rect`）。
    id: u32,
    /// 卡片在屏幕上的全局矩形；渲染时回填，供转场动画作为缩放起点。
    rect: Option<Rect>,
    /// 谱面数据（转场期间用其缩略图继续绘制，保证画面连续）。
    chart: ChartItem,
    /// 转场开始时间。
    start_time: f32,
    /// 待切换的目标场景（曲目场景）；转场完成后由 `next_scene` 取出。
    next_scene: Option<NextScene>,
    /// 是否处于返回（收起）阶段。
    back: bool,
    /// 转场是否已完成（此时 `next_scene` 可被取走）。
    done: bool,
    /// 返回阶段是否同时删除该谱面。
    delete: bool,
}

/// 谱面网格列表视图。
///
/// 由数据（`charts`）、布局参数（`row_num`/`row_height`）、交互开关与转场/菜单状态构成；
/// 具体开关的含义见各字段注释。调用方按「`touch` → `update` → `render`」的次序驱动它。
pub struct ChartsView {
    /// 网格滚动容器。
    scroll: Scroll,
    /// 列表填充/切换时的淡入淡出。
    fader: Fader,

    /// 共享的图标集。
    icons: Arc<Icons>,
    /// 8 张评级图标（按评级索引），供打开的单曲场景使用。
    rank_icons: [SafeTexture; 8],

    /// 从单曲页返回后需要淡入的卡片 `(id, 起始时间)`。
    back_fade_in: Option<(u32, f32)>,

    /// 当前打开单曲页的转场状态。
    transit: Option<TransitState>,
    /// 网格数据；`None` 表示尚未加载（渲染 loading 动画）。
    pub charts: Option<Vec<ChartDisplayItem>>,

    /// 每行的列数。
    pub row_num: u32,
    /// 每行高度（相对屏幕）。
    pub row_height: f32,

    /// 是否允许下拉刷新（列表已在顶部时下拉可触发刷新）。
    pub can_refresh: bool,

    /// 本次触摸是否点到了特殊格（供调用方响应，如切换分类）。
    pub clicked_special: bool,

    /// 是否允许编辑（长按菜单中出现「移动」选项）。
    pub allow_edit: bool,
    /// 是否允许多选（长按菜单中出现「选择」选项）。
    pub allow_multi_select: bool,
    /// 正在被长按操作（作为移动源）的卡片索引。
    editing_chart: Option<usize>,
    /// 长按弹出的操作菜单（选择/移动到…）。
    chart_menu: Popup,
    /// 需要在 `render` 时弹出菜单（`show` 需要 `Ui` 上下文，故延迟到渲染阶段）。
    need_show_chart_menu: bool,
    /// 移动操作是否在等待用户点击目标卡片：`Some(true)` 表示「移动到其后」，`Some(false)` 表示「之前」。
    edit_move_state: Option<bool>,
    /// 待上报的移动结果 `(from, to)`，供调用方同步自身持有的数据顺序。
    movement: Option<(usize, usize)>,

    /// 已多选的谱面引用集合；`Some` 表示处于多选模式。
    pub multi_select: Option<Vec<ChartRef>>,
}

// 视图的构造、数据装载与状态查询。
// 调用契约：外部负责把数据塞进 `set`，再从 `clicked_special`/`take_movement`/`multi_select` 等
// 公开状态读取交互结果；转场完成后由外部调用 `next_scene` 接管场景切换。
impl ChartsView {
    /// 构造视图。
    ///
    /// 默认参数：每行 4 列、行高 `0.3`、允许下拉刷新、允许多选、不允许编辑。
    /// `fader` 的距离设为 `0.06`，即列表内容淡入时伴随极小的位移，观感更柔和。
    pub fn new(icons: Arc<Icons>, rank_icons: [SafeTexture; 8]) -> Self {
        // 滚动与淡入
        Self {
            scroll: Scroll::new(),
            fader: Fader::new().with_distance(0.06),

            // 共享资源
            icons,
            rank_icons,

            back_fade_in: None,

            // 转场与数据（初始未加载）
            transit: None,
            charts: None,

            // 布局与开关默认值
            row_num: 4,
            row_height: 0.3,

            can_refresh: true,

            clicked_special: false,

            // 编辑/多选/菜单状态
            allow_edit: false,
            allow_multi_select: true,
            editing_chart: None,
            chart_menu: Popup::new(),
            need_show_chart_menu: false,
            edit_move_state: None,
            movement: None,

            multi_select: None,
        }
    }

    /// 设置是否允许编辑；关闭时一并清空正在进行的移动操作状态，
    /// 避免开关切换后仍残留「等待选择目标」的半成品交互。
    pub fn allow_edit(&mut self, allow: bool) {
        self.allow_edit = allow;
        if !allow {
            self.edit_move_state = None;
            self.movement = None;
        }
    }

    /// 取出并清空待上报的移动结果（一次性读取），供调用方同步数据顺序。
    pub fn take_movement(&mut self) -> Option<(usize, usize)> {
        self.movement.take()
    }

    /// 计算当前可见的行区间，并换算成卡片索引区间（虚拟滚动/可见区裁剪的核心）。
    ///
    /// 算法：以滚动偏移 `sy` 为视口顶部的「内容坐标」，起始行 = `sy / row_height`；
    /// 结束行 = `(sy + 视口高) / row_height` 向上取整，再额外多算一行（`end_line + 1`），
    /// 为半行露出/快速滚动留出边界容错，避免边缘卡片在一帧内反复进出导致闪烁。
    /// 返回值是卡片索引（行号 × `row_num`），供渲染与命中裁剪使用。
    fn charts_display_range(&self, content_size: (f32, f32)) -> Range<u32> {
        let sy = self.scroll.y_scroller.offset;
        let start_line = (sy / self.row_height) as u32;
        let end_line = ((sy + content_size.1) / self.row_height).ceil() as u32;
        (start_line * self.row_num)..((end_line + 1) * self.row_num)
    }

    /// 清空数据，使视图回到 loading 状态（例如切换分类/页签时）。
    pub fn clear(&mut self) {
        self.charts = None;
    }

    /// 装载新数据并播放淡入：先把 `charts` 写入，再让 `fader` 从起点开始一次淡入，
    /// 这样切换页面/刷新后内容不会突兀地出现。
    pub fn set(&mut self, t: f32, charts: Vec<ChartDisplayItem>) {
        self.charts = Some(charts);
        self.fader.sub(t);
    }

    /// 把滚动位置复位到顶部（例如重新加载列表时）。
    pub fn reset_scroll(&mut self) {
        self.scroll.y_scroller.reset();
    }

    /// 是否正处于打开/返回单曲页的转场中（调用方据此决定是否屏蔽其它输入）。
    pub fn transiting(&self) -> bool {
        self.transit.is_some()
    }

    /// 由单曲页返回时调用：把当前转场切到返回（收起）阶段并重新计时；
    /// `delete` 为 true 时在收起完成后删除该谱面。
    pub fn on_result(&mut self, t: f32, delete: bool) {
        if let Some(transit) = &mut self.transit {
            transit.start_time = t;
            transit.back = true;
            transit.done = false;
            transit.delete = delete;
        }
    }

    /// 读取并清空全局刷新标志（一次性），返回是否需要重新拉取谱面列表。
    pub fn need_update(&self) -> bool {
        NEED_UPDATE.fetch_and(false, Ordering::Relaxed)
    }

    /// 处理触摸，返回事件是否被消费（true 表示本视图已接管，调用方不应再响应）。
    ///
    /// 命中优先级自上而下，先命中者独占：
    /// ① 长按菜单展开时由菜单独占输入；
    /// ② 交给 `Scroll`：若被判定为滚动，则清空所有卡片的按下/长按残留状态；
    /// ③ 触点不在滚动区域内则不响应；
    /// ④ 逐卡片判定（按当前模式分派到房间选曲/移动目标/多选切换/选封面/打开单曲页/长按弹菜单，
    ///    特殊格则置 `clicked_special`）；
    /// ⑤ 处理移动结果：把源卡片从 `charts` 取出并插入到目标位，并把位移换算成不含表头的
    ///    下标经 `movement` 上报给调用方。
    ///
    /// # Errors
    /// 打开单曲场景时可能需要解析本地谱面目录（`dir::charts()`），失败时向上返回错误。
    pub fn touch(&mut self, touch: &Touch, t: f32, rt: f32) -> Result<bool> {
        // ① 菜单展开时由菜单独占输入
        if self.chart_menu.showing() {
            self.chart_menu.touch(touch, t);
            return Ok(true);
        }
        // ② 滚动优先于点击/长按（判定逻辑见下方注释）
        if self.scroll.touch(touch, t) {
            // Scroll took over the gesture (user is scrolling, not clicking / long-pressing).
            // Clear any pending long-touch state so a fast flick doesn't leave a stale
            // start time behind that later gets misjudged as a long click.
            if let Some(charts) = &mut self.charts {
                for item in charts.iter_mut() {
                    item.long_touch.reset();
                    item.btn.inner.cancel();
                }
            }
            return Ok(true);
        }
        // ③ 触点不在滚动区域内 → 本视图不响应
        if !self.scroll.contains(touch) {
            return Ok(false);
        }
        // ④ 逐卡片判定；`movement` 用于暂存「点目标卡片完成移动」的结果，循环结束后统一处理
        let mut movement = None;
        if let Some(charts) = &mut self.charts {
            for (id, item) in charts.iter_mut().enumerate() {
                if let Some(chart) = &item.chart {
                    if item.btn.touch(touch, t) {
                        item.long_touch.reset();
                        let handled_by_mp = MP_PANEL.with(|it| {
                            if let Some(panel) = it.borrow_mut().as_mut() {
                                if panel.in_room() {
                                    if let Some(id) = chart.info.id {
                                        panel.select_chart(id);
                                        panel.show(rt);
                                    } else {
                                        use crate::mp::{mtl, L10N_LOCAL};
                                        show_message(mtl!("select-chart-local")).error();
                                    }
                                    return true;
                                }
                            }
                            false
                        });
                        if handled_by_mp {
                            button_hit_large();
                            continue;
                        }
                        if let Some(after) = self.edit_move_state.take() {
                            button_hit();
                            movement = Some((id, after));
                            continue;
                        }
                        if let Some(sel) = &mut self.multi_select {
                            button_hit();
                            let r = chart.to_bare_ref();
                            let mut removed = false;
                            sel.retain(|it| {
                                if it == &r {
                                    removed = true;
                                    false
                                } else {
                                    true
                                }
                            });
                            if !removed {
                                sel.push(r);
                            }
                            continue;
                        }
                        if CHOOSE_COVER.load(Ordering::Relaxed) {
                            button_hit();
                            CHOSEN_COVER.with(|it| {
                                *it.borrow_mut() = Some(if let Some(id) = chart.info.id {
                                    Ok(id)
                                } else {
                                    Err(chart.local_path.clone().unwrap())
                                });
                            });
                            continue;
                        }

                        button_hit_large();
                        let download_path = chart.info.id.map(|it| format!("download/{it}"));
                        let scene = SongScene::new(
                            chart.clone(),
                            if let Some(path) = &chart.local_path {
                                Some(path.clone())
                            } else {
                                let path = download_path.clone().unwrap();
                                if Path::new(&format!("{}/{path}", dir::charts()?)).exists() {
                                    Some(path)
                                } else {
                                    None
                                }
                            },
                            Arc::clone(&self.icons),
                            self.rank_icons.clone(),
                            chart
                                .local_path
                                .as_ref()
                                .and_then(|path| get_data().charts.iter().find(|it| &it.local_path == path).map(|it| it.mods))
                                .unwrap_or_default(),
                        );
                        self.transit = Some(TransitState {
                            id: id as _,
                            rect: None,
                            chart: chart.clone(),
                            start_time: t,
                            next_scene: Some(NextScene::Overlay(Box::new(scene))),
                            back: false,
                            done: false,
                            delete: false,
                        });
                        return Ok(true);
                    }
                    if self.allow_multi_select && self.multi_select.is_none() && item.btn.long_touch(touch, t, &mut item.long_touch) {
                        self.scroll.y_scroller.halt();
                        self.editing_chart = Some(id);
                        let mut options = vec![tl!("select").into_owned()];
                        if self.allow_edit {
                            options.extend([
                                tl!("move-to-first").into_owned(),
                                tl!("move-to-last").into_owned(),
                                tl!("move-before").into_owned(),
                                tl!("move-after").into_owned(),
                            ]);
                        }
                        self.chart_menu.set_options(options);
                        self.chart_menu.set_selected(usize::MAX);
                        self.need_show_chart_menu = true;
                        return Ok(true);
                    }
                } else if item.btn.touch(touch, t) {
                    self.editing_chart = None;
                    self.edit_move_state = None;
                    button_hit_large();
                    self.clicked_special = true;
                }
            }
        }
        // ⑤ 移动收尾：把源卡片取出并插入到目标位；`to` 需按「源与目标的相对位置」修正 1 位
        //    （`after` 表示插入到目标之后），并断言不落在表头之前。
        //    上报给调用方的下标再减去表头偏移，使其与外部数据（不含表头）的下标对齐。
        if let Some((id, after)) = movement {
            let has_header = self.has_header();
            let editing = self.editing_chart.unwrap();
            let to = if after {
                id + (id < editing) as usize
            } else {
                id - (id > editing) as usize
            };
            if let Some(charts) = &mut self.charts {
                let chart = charts.remove(editing);
                charts.insert(to, chart);
                // 不变量：插入位置必须在表头之后（表头固定为列表首项）
                assert!(to >= has_header as usize);
                self.movement = Some((editing - has_header as usize, to - has_header as usize));
            }
        }
        Ok(false)
    }

    /// 列表首项是否为「表头/特殊格」（`chart` 为 `None`）。
    /// 移动谱面时需把表头排除在下标换算之外，故 `movement` 上报的下标要减去它。
    fn has_header(&self) -> bool {
        self.charts.as_ref().is_some_and(|it| it.first().is_some_and(|item| item.chart.is_none()))
    }

    /// 推进一帧逻辑，返回是否触发了下拉刷新（供调用方据此重新拉取数据）。
    ///
    /// 阶段：① 记录下拉刷新是否触发；② 推进菜单与滚动；③ 长按判定（处理「按住不动」而未产生
    /// 移动事件的补判路径，与 `touch` 里的长按判定互为补充）；④ 处理菜单选择结果（多选/移到首尾/
    /// 进入「选择目标」状态）；⑤ 推进转场动画，返回阶段按 `delete` 删除谱面并置 `NEED_UPDATE`；
    /// ⑥ 让所有缩略图动画 settle 到当前时刻（触发延迟加载/淡入）。
    ///
    /// # Errors
    /// 删除谱面时涉及文件系统操作（`remove_dir_all`）与数据落盘（`save_data`），失败时向上返回错误。
    pub fn update(&mut self, t: f32) -> Result<bool> {
        // ① 下拉刷新判定（列表已拉到顶且仍在下拉）
        let refreshed = self.can_refresh && self.scroll.y_scroller.pulled;
        // ② 推进菜单与滚动
        self.chart_menu.update(t);
        self.scroll.update(t);
        // ③ 长按判定：与 `touch` 中的长按判定配合，覆盖「按住不动」的场景
        if self.allow_multi_select && self.multi_select.is_none() {
            if let Some(charts) = &mut self.charts {
                for (id, item) in charts.iter_mut().enumerate() {
                    if item.chart.is_some() && item.btn.update_long_touch(t, &mut item.long_touch) {
                        self.scroll.y_scroller.halt();
                        self.editing_chart = Some(id);
                        let mut options = vec![tl!("select").into_owned()];
                        if self.allow_edit {
                            options.extend([
                                tl!("move-to-first").into_owned(),
                                tl!("move-to-last").into_owned(),
                                tl!("move-before").into_owned(),
                                tl!("move-after").into_owned(),
                            ]);
                        }
                        self.chart_menu.set_options(options);
                        self.chart_menu.set_selected(usize::MAX);
                        self.need_show_chart_menu = true;
                        break;
                    }
                }
            }
        }
        // ④ 菜单选择结果：0=进入多选，1=移到最前，2=移到最后，3/4=进入「选择目标」状态
        if self.chart_menu.changed() {
            let has_header = self.has_header();
            let editing = self.editing_chart.unwrap();
            match self.chart_menu.selected() {
                0 => {
                    let chart = self.charts.as_ref().unwrap()[editing].chart.as_ref().unwrap();
                    self.multi_select = Some([chart.to_bare_ref()].into());
                }
                1 => {
                    self.movement = Some((editing - has_header as usize, 0));
                    if let Some(charts) = &mut self.charts {
                        let chart = charts.remove(editing);
                        charts.insert(has_header as usize, chart);
                    }
                }
                2 => {
                    self.movement = Some((editing - has_header as usize, self.charts.as_ref().unwrap().len() - 1 - has_header as usize));
                    if let Some(charts) = &mut self.charts {
                        let chart = charts.remove(editing);
                        charts.push(chart);
                    }
                }
                3 | 4 => {
                    self.edit_move_state = Some(self.chart_menu.selected() == 4);
                    show_message(tl!("choose-target"));
                }
                _ => {}
            }
        }
        // ⑤ 推进转场；返回阶段按需删除谱面（删除后清除全局刷新标志）
        if let Some(transit) = &mut self.transit {
            transit.chart.illu.settle(t);
            if t > transit.start_time + transit_time().unwrap_or_default() {
                if transit.back {
                    if transit.delete {
                        let data = get_data_mut();
                        let item = &self.charts.as_ref().unwrap()[transit.id as usize];
                        let path = if let Some(path) = &item.chart.as_ref().unwrap().local_path {
                            path.clone()
                        } else {
                            format!("download/{}", item.chart.as_ref().unwrap().info.id.unwrap())
                        };
                        std::fs::remove_dir_all(format!("{}/{path}", dir::charts()?))?;

                        if let Some(chart) = data.find_chart_by_path(path.as_str()) {
                            data.charts.remove(chart);
                        }

                        save_data()?;
                        NEED_UPDATE.store(true, Ordering::SeqCst);
                    } else {
                        // 非删除的返回：记录起始时间，让该卡片随后淡入
                        self.back_fade_in = Some((transit.id, t));
                    }
                    self.transit = None;
                } else {
                    transit.done = true;
                }
            }
        }

        // ⑥ 推进每张缩略图的动画状态
        if let Some(charts) = &mut self.charts {
            for chart in charts {
                if let Some(chart) = &mut chart.chart {
                    chart.illu.settle(t);
                }
            }
        }

        Ok(refreshed)
    }

    /// 绘制网格。
    ///
    /// 阶段：① 由滚动偏移算出可见卡片区间；② 数据未加载时画 loading、空列表时画提示并返回；
    /// ③ 进入滚动作用域（`dx/dy` 平移到视口原点，`size` 声明视口，`render` 内返回内容总尺寸），
    /// 并在其中绘制下拉刷新提示；④ 计算列宽/行高与卡片内边距（卡片矩形在格子内四周收缩
    /// `CHART_PADDING`）；⑤ 用 `hgrids` 逐格绘制并做可见区裁剪：区间外的格子调用 `invalidate`
    /// 清除残留命中状态，可见格渲染缩略图、标题、等级、角标与多选序号（特殊格画抽象占位图），
    /// 命中当前转场卡片时回填其全局矩形、需要时弹出长按菜单；⑥ 最后在顶层渲染长按菜单。
    pub fn render(&mut self, ui: &mut Ui, r: Rect, t: f32) {
        // ① 可见区间（虚拟滚动裁剪）
        let content_size = (r.w, r.h);
        let range = self.charts_display_range(content_size);
        // ② 未加载 / 空列表占位
        let Some(charts) = &mut self.charts else {
            let ct = r.center();
            ui.loading(ct.x, ct.y, t, WHITE, ());
            return;
        };
        if charts.is_empty() {
            let ct = r.center();
            ui.text(ttl!("list-empty")).pos(ct.x, ct.y).anchor(0.5, 0.5).no_baseline().draw();
            return;
        }
        // ③ 进入滚动作用域：先把坐标系平移到视口左上角，再让 Scroll 绘制内容
        ui.scope(|ui| {
            ui.dx(r.x);
            ui.dy(r.y);
            let off = self.scroll.y_scroller.offset;
            self.scroll.size(content_size);
            self.scroll.render(ui, |ui| {
                if self.can_refresh {
                    render_release_to_refresh(ui, r.w / 2., off);
                }
                // ④ 栅格布局：列宽 = 视口宽 / 列数，行高 = row_height，卡片在格子内留内边距
                let cw = r.w / self.row_num as f32;
                let ch = self.row_height;
                let p = CHART_PADDING;
                let r = Rect::new(p, p, cw - p * 2., ch - p * 2.);
                self.fader.reset();
                self.fader.for_sub(|f| {
                    // ⑤ 逐格绘制（hgrids 提供单元格坐标与索引）
                    ui.hgrids(content_size.0, ch, self.row_num, charts.len() as u32, |ui, id| {
                        if let Some(transit) = &mut self.transit {
                            if transit.id == id {
                                // 记录转场卡片矩形，供放大/收起动画作为起点/终点
                                transit.rect = Some(ui.rect_to_global(r));
                            }
                        }
                        if self.editing_chart == Some(id as usize) && self.need_show_chart_menu {
                            self.need_show_chart_menu = false;
                            self.chart_menu.set_auto_adjust(Some(ui.screen_rect().nonuniform_feather(-0.03, -0.05)));
                            self.chart_menu.show(ui, t, Rect::new(cw * 2. / 3., ch * 2. / 3., 0.35, 0.4));
                        }
                        // 可见区裁剪：区间外的格子不绘制，并清掉按钮的按下/长按残留
                        if !range.contains(&id) {
                            if let Some(item) = charts.get_mut(id as usize) {
                                item.btn.invalidate();
                            }
                            return;
                        }
                        f.render(ui, t, |ui| {
                            let mut c = WHITE;

                            let item = &mut charts[id as usize];

                            item.btn.render_shadow(ui, r, t, |ui, path| {
                                let selected_color = Color::from_rgba(30, 136, 229, 255);

                                if let Some(chart) = &mut item.chart {
                                    // 多选模式下：命中选中集合时先铺一层蓝色高亮
                                    let selected = self.multi_select.as_ref().and_then(|set| {
                                        let chart_ref = chart.to_bare_ref();
                                        set.iter().position(|it| it == &chart_ref)
                                    });
                                    if selected.is_some() {
                                        ui.fill_path(&r.feather(0.008).rounded(0.003), selected_color);
                                    }

                                    // 缩略图：notify 触发延迟加载，shading 负责加载完成后的淡入
                                    chart.illu.notify();
                                    ui.fill_path(&path, semi_black(c.a));
                                    ui.fill_path(&path, chart.illu.shading(r.feather(0.01), t));
                                    // 从单曲页返回后的卡片淡入（进度用 BACK_FADE_IN_TIME 归一）
                                    if let Some((that_id, start_time)) = &self.back_fade_in {
                                        if id == *that_id {
                                            let p = ((t - start_time) / BACK_FADE_IN_TIME).max(0.);
                                            if p > 1. || get_data().prefer_reduced_motion {
                                                self.back_fade_in = None;
                                            } else {
                                                ui.fill_path(&path, semi_black(0.55 * (1. - p)));
                                                c.a *= p;
                                            }
                                        }
                                    }

                                    // 底部渐隐遮罩：让标题文字在浅色封面上也能看清
                                    ui.fill_path(&path, (semi_black(0.4 * c.a), (0., 0.), semi_black(0.8 * c.a), (0., ch)));

                                    // 等级文字（右上角）；若等级文本不含 "Lv." 则补上难度数值
                                    let info = &chart.info;
                                    let mut level = info.level.clone();
                                    if !level.contains("Lv.") {
                                        use std::fmt::Write;
                                        write!(&mut level, " Lv.{}", info.difficulty as i32).unwrap();
                                    }
                                    let mut t = ui
                                        .text(level)
                                        .pos(r.right() - 0.016, r.y + 0.016)
                                        .max_width(r.w * 2. / 3.)
                                        .anchor(1., 0.)
                                        .size(0.52 * r.w / cw)
                                        .color(c);
                                    let ms = t.measure();
                                    t.ui.fill_path(
                                        &ms.feather(0.008).rounded(0.01),
                                        Color {
                                            a: c.a * 0.7,
                                            ..t.ui.background()
                                        },
                                    );
                                    t.draw();
                                    // 曲名（左下角，字号随卡片宽度等比缩放）
                                    ui.text(&info.name)
                                        .pos(r.x + 0.01, r.bottom() - 0.02)
                                        .max_width(r.w)
                                        .anchor(0., 1.)
                                        .size(0.6 * r.w / cw)
                                        .color(c)
                                        .draw();
                                    // 角标（左上角）：+ = 稳定请求，* = 未审核
                                    if let Some(symbol) = item.symbol {
                                        ui.text(symbol.to_string())
                                            .pos(r.x + 0.01, r.y + 0.01)
                                            .size(0.8 * r.w / cw)
                                            .color(c)
                                            .draw();
                                    }

                                    // 多选序号：命中集合中的位置 +1，居中叠画
                                    if let Some(pos) = selected {
                                        ui.fill_path(&path, Color { a: 0.4, ..selected_color });
                                        let ct = r.center();
                                        ui.text((pos + 1).to_string())
                                            .pos(ct.x, ct.y)
                                            .anchor(0.5, 0.5)
                                            .no_baseline()
                                            .size(1.2 * r.w / cw)
                                            .color(WHITE)
                                            .draw_using(&BOLD_FONT);
                                    }
                                } else {
                                    // 特殊格：抽象占位图 + 半透明遮罩 + 居中标签文字
                                    ui.fill_path(&path, (*self.icons.r#abstract, r));
                                    ui.fill_path(&path, semi_black(0.2));
                                    let ct = r.center();
                                    use crate::page::coll::{tl as page_tl, L10N_LOCAL};
                                    ui.text(page_tl!("label"))
                                        .pos(ct.x, ct.y)
                                        .anchor(0.5, 0.5)
                                        .no_baseline()
                                        .size(0.7)
                                        .draw_using(&BOLD_FONT);
                                }
                            });
                        });
                    })
                })
            });
        });
        // ⑥ 菜单需覆盖在网格之上，故在最后（顶层）渲染
        self.chart_menu.render(ui, t, 1.);
    }

    /// 在顶层绘制转场中的卡片（放大/收起动画）。
    ///
    /// 用记录到的卡片矩形与当前全屏矩形做 `Rect::tween` 插值得到中间矩形，并对圆角随进度
    /// 收缩（`0.02 * (1-p)`），再叠一层半透明黑使过渡更沉浸。进度 `p` 先取四次方
    /// （`powi(4)`）放慢末端，形成「先快后缓」的手感；返回阶段反向（`1 - p`）。
    pub fn render_top(&mut self, ui: &mut Ui, t: f32) {
        if let Some(transit) = &self.transit {
            if let Some(fr) = transit.rect {
                let p = transit_time().map_or(1., |tt| ((t - transit.start_time) / tt).clamp(0., 1.));
                let p = (1. - p).powi(4);
                let p = if transit.back { p } else { 1. - p };
                let r = Rect::tween(&fr, &ui.screen_rect(), p);
                let path = r.rounded(0.02 * (1. - p));
                ui.fill_path(&path, (*transit.chart.illu.texture.1, r.feather(0.01 * (1. - p))));
                ui.fill_path(&path, semi_black(0.55));
            }
        }
    }

    /// 若转场已完成，取出并返回待切换的目标场景（一次性消费，返回后转场状态保留但不重复给出）。
    pub fn next_scene(&mut self) -> Option<NextScene> {
        if let Some(transit) = &mut self.transit {
            if transit.done {
                return transit.next_scene.take();
            }
        }
        None
    }
}
