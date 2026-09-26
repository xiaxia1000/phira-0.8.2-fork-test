//! 章节合辑页：横向排列的章节卡片列表（目前只有内置章节，如“第一章”）。
//!
//! 本页只是**入口**：章节内部的曲目列表与难度选择由 [`ChapterScene`] 承担。
//! 两者之间用一段“卡片放大到铺满全屏”的转场衔接——点击后本页先自己播动画，
//! 动画播完（[`CollectionPage::ok_to_transit`] 置位）才把场景交出去，
//! 这样引擎切换场景的那一帧被动画完全遮住，用户看不到 UI 重构的痕迹。
//!
//! 由于章节内容需要读谱面文件，场景构造是异步的，且可能在动画开始前就并行准备
//! （[`CollectionPage::scene_task`]）。

prpr_l10n::tl_file!("collection");

use super::{Illustration, NextPage, Page, SharedState};
use crate::{icons::Icons, load_res_tex, resource::rtl, scene::ChapterScene};
use anyhow::Result;
use macroquad::prelude::*;
use prpr::{
    core::Tweenable,
    ext::{poll_future, semi_black, semi_white, LocalTask, RectExt, SafeTexture},
    scene::NextScene,
    ui::{RectButton, Scroll, Ui},
};
use std::{borrow::Cow, sync::Arc};
use tap::Tap;

/// 列表里的一张章节卡片。
///
/// 卡片数据是**内置且静态**的（不来自服务端），因此构造时就把封面准备好，
/// 不涉及分页或刷新逻辑。
struct CollectionItem {
    /// 章节标识，会原样传给 [`ChapterScene::new`] 决定加载哪些曲目。
    /// `"@"` 是保留值，代表“敬请期待”占位卡（见 [`CollectionPage::new`]）。
    id: String,
    /// 封面图。
    illu: Illustration,
    /// 显示标题（已本地化）。
    title: String,
    /// 命中区域。每帧在 `render` 里由布局实测确定（为 `RectButton` 的惯例），
    /// 因此 `touch` 必须能在尚未渲染过的情况下安全返回 `false`。
    btn: RectButton,
}

/// “卡片放大到全屏”的转场状态。
///
/// 之所以要把这些字段记下来而不是每帧重算：转场过程中列表已经不可见，
/// 无法再从布局里推出起点矩形，只能在渲染卡片的那一帧把几何信息“抓拍”下来
/// （这就是 [`CollectionPage::transit_id`] 存在的理由）。
struct Transit {
    /// 卡片在列表坐标下的矩形，作为动画的起点。
    r: Rect,
    /// 本帧封面纹理被映射到的矩形（比卡片略大，且含视差横移）。
    ///
    /// 放大动画要把它与全屏矩形插值，才能保证封面在放大过程中不出现拉伸错位。
    ir: Rect,
    /// 转场开始时间；**负值表示反向**（从全屏收回卡片）。
    ///
    /// 起点时间与方向共用这一个字段（取 `abs()` 得到时间），
    /// 因此读到负值时不要误以为是时间倒流。
    t: f32,
    /// 参与放大的封面原图（不是缩略图）。
    illu: SafeTexture,
}

/// 章节合辑页。
pub struct CollectionPage {
    /// 首页共用的图标资源，用于构造 `ChapterScene`。
    icons: Arc<Icons>,

    /// 章节卡片列表（含末尾的占位卡）。
    colls: Vec<CollectionItem>,
    /// 横向滚动容器，带按卡片宽度吸附的步长。
    scroll: Scroll,

    /// 待交给 `MainScene` 的栈操作请求，`take` 后即空。
    next_page: Option<NextPage>,
    /// 待交给 `MainScene` 的场景切换请求，只在转场动画播完后才生效。
    next_scene: Option<NextScene>,

    /// 已点击但还没抓到起点矩形的卡片 id。
    ///
    /// 点击发生在 `touch`，而起点矩形只有 `render` 才知道，于是用 id 在这里“过一手”。
    transit_id: Option<String>,
    /// 正在播放的转场状态。
    transit: Option<Transit>,
    /// 异步构造 [`ChapterScene`] 的任务；在点击的同一帧启动，动画期间并行加载。
    scene_task: LocalTask<Result<NextScene>>,
    /// 转场动画是否已播到可以交出场景的程度（由 `render_top` 置位、`next_scene` 消费）。
    ok_to_transit: bool,

    /// 是否从未进入过本页。首次进入没有“外层页面被弹出”的前情，也就不该播收回动画。
    first_in: bool,
}

// 构造与几何常量。整个页面的尺寸都由这三个常量推导，改宽度时记得同步 `scroll` 的吸附步长。
impl CollectionPage {
    /// 卡片宽度（UI 单位，屏幕半宽为 1）。
    const WIDTH: f32 = 0.5;
    /// 卡片高度。宽高比取 1:1.26，接近 A4 竖版，符合“章节封面”的直觉。
    const HEIGHT: f32 = 0.63;
    /// 相邻卡片之间的水平间距。它同时决定了滚动吸附的步长，因此不可与 [`Self::WIDTH`] 任意搭配。
    const PAD: f32 = 0.06;

    /// 创建章节合辑页：写入内置章节列表并准备滚动容器。
    ///
    /// 章节数据目前**硬编码**（只有 `c1`），因为内置章节的曲目清单编译在资源包里；
    /// 末尾固定追加一张 id 为 `"@"` 的灰色占位卡（文案“敬请期待”），
    /// 让列表的横向滚动有“后面还有内容”的暗示，同时该卡在 `touch` 中被显式排除、不可点击。
    ///
    /// # Errors
    /// 章节封面资源加载失败时返回错误（`load_res_tex` 内部失败）。
    pub async fn new(icons: Arc<Icons>) -> Result<Self> {
        Ok(Self {
            icons,

            colls: {
                let mut res = {
                    // `rtl!` 会展开为 `L10N_LOCAL.with(..)`，这个名字来自 `resource` 模块的
                    // `tl_file!("resource" rtl)`。此处局部 `use` 就是为了把 resource 的
                    // `L10N_LOCAL` 引入作用域（并遮蔽本模块 `collection.ftl` 生成的那个），
                    // 使章节标题与 `ChapterScene` 里的标题取到同一份翻译。
                    use crate::resource::L10N_LOCAL;
                    vec![CollectionItem {
                        id: "c1".to_owned(),
                        illu: Illustration::from_done(load_res_tex("res/chap/c1/cover").await),
                        title: rtl!("chap-c1").into_owned(),
                        btn: RectButton::new(),
                    }]
                };
                res.push(CollectionItem {
                    id: "@".to_owned(),
                    // 占位卡没有真实封面，用一个 1×1 的浅灰色纹理铺满即可。
                    illu: Illustration::from_done(Texture2D::from_rgba8(1, 1, &[211, 211, 211, 255]).into()),
                    title: tl!("wait-for-more").into_owned(),
                    btn: RectButton::new(),
                });
                res
            },
            // 横向滚动 + 步长 = 卡片宽 + 间距：松手后自动吸附到“整卡对齐”的位置，
            // 避免停在两张卡片中间这种别扭的状态。
            scroll: Scroll::new().horizontal().tap_mut(|it| it.x_scroller.step = Self::WIDTH + Self::PAD),

            next_page: None,
            next_scene: None,

            transit_id: None,
            transit: None,
            scene_task: None,
            ok_to_transit: false,

            first_in: true,
        })
    }
}

// 本页在页面栈中的行为约定：
// - 不请求压栈/弹栈（`next_page` 恒为默认值），只会在**转场动画播完之后**请求切换到 `ChapterScene`；
// - 缩放动画分两段播放：正向（点击进入，`transit.t > 0`）在 `render_top` 里放大到全屏，
//   反向（从章节返回，`transit.t < 0`）把全屏收回到卡片位置；
// - 反向动画期间本页已经重新成为栈顶，因此 `enter` 里那行 `t = -s.rt` 就是“收回动画”的启动点。
impl Page for CollectionPage {
    /// 重新成为栈顶时启动“卡片收回”动画。
    ///
    /// 首次进入时没有可收回的 [`Transit`]（列表还没渲染过），所以只清掉 `first_in` 标记；
    /// 之后每次从章节返回都会走 `else` 分支——这里 `unwrap` 是安全的，
    /// 因为离开本页的唯一途径就是点击卡片，而点击必然已经建立了 `transit`。
    fn enter(&mut self, s: &mut SharedState) -> Result<()> {
        if self.first_in {
            self.first_in = false;
        } else {
            self.transit.as_mut().unwrap().t = -s.rt;
        }

        Ok(())
    }

    /// 标题栏文案，取自本模块的 `collection.ftl`，随语言切换实时生效。
    fn label(&self) -> Cow<'static, str> {
        tl!("label")
    }

    /// 处理一次触摸：优先交给滚动容器，其次做卡片命中；命中即启动章节场景的异步构造。
    ///
    /// 顺序很关键——`Scroll` 会把“已经越过拖动阈值”的触摸吞掉，
    /// 因此横向拖拽列表时不会被误判成点击某张卡片（[`Scroll::touch`] 返回 `true` 即直接放行到此为止）。
    ///
    /// 占位卡（`"@"`）即使被点中也不做任何事，但仍返回 `true` 消费掉这次事件，
    /// 免得事件继续穿透到其他处理器。
    ///
    /// 章节场景的构造被立刻扔进 [`CollectionPage::scene_task`]：它要读谱面包，
    /// 需要与后面的放大动画**并行**进行，动画播完时正好可以交出结果。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        let rt = s.rt;

        // 阶段一：滚动优先。拖动中或是滚动本身的命中都直接消费事件。
        if self.scroll.touch(touch, rt) {
            return Ok(true);
        }
        // 阶段二：仅在没有转场时才允许点击，避免动画途中重复进入章节。
        if self.transit.is_none() {
            for coll in &mut self.colls {
                if coll.btn.touch(touch) {
                    if coll.id != "@" {
                        // 阶段三：记录待处理的 id（起点矩形要等渲染时才知道），
                        // 并把场景构造所需的一切按值移进异步任务。
                        // 注意 `illu` 取 `.1`（原图）：放大到全屏必须用高分辨率纹理。
                        self.transit_id = Some(coll.id.clone());
                        let id = coll.id.clone();
                        let icons = Arc::clone(&self.icons);
                        let rank_icons = s.icons.clone();
                        let illu = coll.illu.texture.1.clone();
                        self.scene_task = Some(Box::pin(async move {
                            let scene = ChapterScene::new(id, icons, rank_icons, illu).await?;
                            Ok(NextScene::Overlay(Box::new(scene)))
                        }));
                    }
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// 每帧推进：滚动惯性与吸附、插图结算、场景构造任务轮询。
    ///
    /// 时间轴的选择体现了两条约定：滚动用**真实时间** `rt`（转场/暂停时列表仍应自然减速），
    /// 而插图淡入用**游戏时间** `t`（与 `Illustration::settle` 记录的时钟保持一致）。
    ///
    /// 任务完成时把结果写进 [`CollectionPage::next_scene`]，但**不立即**生效：
    /// `next_scene` 还要等 `render_top` 把动画播完才允许被取走，这是转场时序的关键一环。
    fn update(&mut self, s: &mut SharedState) -> Result<()> {
        let t = s.t;

        // 阶段一：滚动容器的物理推进（惯性、越界回弹、吸附）
        self.scroll.update(s.rt);
        // 阶段二：结算章节封面，让 `Illustration::from_done` 的封面也能走淡入
        for coll in &mut self.colls {
            coll.illu.settle(t);
        }
        // 阶段三：章节场景是否已经构造完成（读包与动画并行，动画结束时通常已就绪）
        if let Some(task) = &mut self.scene_task {
            if let Some(res) = poll_future(task.as_mut()) {
                self.next_scene = Some(res?);
                self.scene_task = None;
            }
        }
        Ok(())
    }

    /// 绘制横向卡片列表。
    ///
    /// 布局上做了一次“先推到屏幕外再靠滚动偏移拉回来”的处理：把列表坐标系原点挪到屏幕左侧之外
    /// （`dx(-1.)`）并把纵向也对齐到可视区顶部（`dy(-ui.top)`），
    /// 于是每张卡片的 x 可以直接累加，而滚动偏移 `cur` 只影响它相对屏幕中心的位置；
    /// 这样切到横向滚动时就不需要为“居中”单独做一次坐标换算。
    ///
    /// 视差与聚焦：`off` 是卡片相对滚动中心的距离（单位是“一张卡”），
    /// 封面纹理的采样窗口按 `off/4` 横移，使两侧卡片看起来略微偏向中心；
    /// 标题则按 `off` 调整字号与亮度，越靠近中心越大越亮。
    ///
    /// 一个重要副作用：命中区域 `btn` 与转场起点矩形都是在**这里**确定的。
    /// 也就是说，本页必须先渲染过一次，`touch` 才可能命中任何东西。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        let rt = s.rt;

        s.render_fader(ui, |ui| {
            // 阶段一：声明滚动容器的可视区为整屏（宽 2、高 2×top，top 为半高）
            self.scroll.size((2., ui.top * 2.));
            ui.scope(|ui| {
                ui.dx(-1.);
                ui.dy(-ui.top);
                let cur = self.scroll.x_scroller.offset;
                self.scroll.render(ui, |ui| {
                    ui.dx(1.);
                    ui.dy(ui.top);
                    let mut x = 0.;
                    let step = Self::WIDTH + Self::PAD;
                    // 阶段二：逐卡片绘制。x 累加步长，off 用于视差与聚焦
                    for coll in &mut self.colls {
                        let off = (x - cur) / step;
                        let r = Rect::new(x - Self::WIDTH / 2., -Self::HEIGHT / 2., Self::WIDTH, Self::HEIGHT);
                        // 命中区域必须在渲染时登记：`touch` 依赖上一次渲染留下的矩形
                        coll.btn.set(ui, r);
                        ui.fill_rect(r, BLACK);
                        // 纹理目标矩形刻意在横向上外扩得比卡片更大（0.4 对 0.2）：
                        // 横向相邻卡片靠得近，且下一步还要横向平移纹理，外扩提供了平移余量，
                        // 否则平移后卡片边缘会露出未覆盖的区域。
                        let mut ir = r.nonuniform_feather(0.4, 0.2);
                        ir.x += off / 4.;
                        ui.fill_rect(r, coll.illu.shading(ir, t));
                        ui.fill_rect(r, semi_black(0.2));
                        // 阶段三：如果这张卡是待转场的目标，在此“抓拍”几何信息。
                        // 必须用当前帧实测的 ir（含视差偏移），否则放大动画会从错误的位置开始。
                        if self.transit_id.as_ref() == Some(&coll.id) {
                            self.transit = Some(Transit {
                                r,
                                ir,
                                t: rt,
                                illu: coll.illu.texture.1.clone(),
                            });
                            self.transit_id = None;
                        }
                        // 阶段四：标题随离中心的距离缩放与提亮，形成“中间一张被选中”的暗示
                        let p = 1. - off.abs() * step;
                        ui.text(&coll.title)
                            .pos(r.x + 0.02, r.y + 0.02)
                            .max_width(r.w - 0.04)
                            .size(0.5 + p * 0.34)
                            .color(semi_white(1. - (1. - p) * 0.8))
                            .draw();
                        x += step;
                    }

                    // 阶段五：回报内容尺寸。宽度 = 首尾卡片中心距 + 左右各留半屏，
                    // 使第一张与最后一张都能被滚到屏幕正中；高度取卡片高。
                    (step * (self.colls.len() - 1) as f32 + 2., Self::HEIGHT)
                });
            })
        });
        Ok(())
    }

    /// 绘制“卡片 ↔ 全屏”的缩放转场。
    ///
    /// 放在 `render_top` 而不是 `render` 是必要的：它必须盖住标题栏与返回按钮，
    /// 而只有最上层回调能获得这个绘制层级。
    ///
    /// 动画语义由 [`Transit::t`] 的符号区分：
    /// - 正向（`t > 0`，点击进入）：0.5 秒内把卡片矩形插值到全屏，播完置位
    ///   [`CollectionPage::ok_to_transit`]，随后 `next_scene` 才把场景交出去；
    /// - 反向（`t < 0`，从章节返回）：0.3 秒内把全屏收回卡片位置。
    ///   两条时长不对称是刻意的——进入要“隆重”，返回要“干脆”，否则连续返回会显得拖沓。
    ///
    /// 横纵进度不同步：纵向取 `p / 0.45`，比横向更早到达 1，
    /// 于是画面先撑满高度、再补齐宽度，比等比缩放更有“展开”的动感。
    fn render_top(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        if let Some(tr) = &self.transit {
            // 阶段一：求线性进度。`t` 的绝对值才是起始时间，符号代表方向。
            let p = ((s.rt - tr.t.abs()) / (if tr.t < 0. { 0.3 } else { 0.5 })).min(1.);
            // 阶段二：缓动。两种情况都走三次曲线，让插值参数“起步快、收尾慢”：
            // 正向参数由 0 走到 1，表现为迅速展开、接近全屏时收住；
            // 反向参数由 1 走回 0，表现为迅速收缩、贴近卡片位置时落位。
            let p = if tr.t < 0. { (1. - p).powi(3) } else { 1. - (1. - p).powi(3) };

            // 阶段三：把卡片矩形与纹理矩形分别插值到全屏，并绘制
            let xp = p;
            let yp = (p / 0.45).min(1.);
            let sr = ui.screen_rect();
            let r = tr.r;
            let r = Rect::new(f32::tween(&r.x, &sr.x, xp), f32::tween(&r.y, &sr.y, yp), f32::tween(&r.w, &sr.w, xp), f32::tween(&r.h, &sr.h, yp));
            let ir = Rect::tween(&tr.ir, &sr, xp);
            ui.fill_rect(r, (*tr.illu, ir));
            // 越接近全屏压得越暗：为下一屏（章节目录）的出场做视觉过渡
            ui.fill_rect(r, semi_black(0.2 + 0.1 * p));
            // 阶段四：终点处理。正向播完才允许切场景；反向归零则丢弃转场状态。
            if p >= 1. && tr.t > 0. {
                self.ok_to_transit = true;
            }
            if p <= 0. && tr.t < 0. {
                self.transit = None;
            }
        }
        Ok(())
    }

    /// 取走待处理的栈请求（本页目前从不设置它，恒为 [`NextPage::None`]）。
    fn next_page(&mut self) -> NextPage {
        self.next_page.take().unwrap_or_default()
    }

    /// 交出章节场景，但**仅在缩放动画播完之后**。
    ///
    /// 这是本页与 `MainScene` 之间最关键的时序约束：场景构造可能在动画一开始就完成了，
    /// 但那时要立刻切换会露出 UI 重构的过程，所以结果被压在 `next_scene` 里，
    /// 直到 `render_top` 置位 `ok_to_transit` 才放行。取出后立即清除标志，保证只交付一次。
    fn next_scene(&mut self, _s: &mut SharedState) -> NextScene {
        if self.ok_to_transit {
            self.ok_to_transit = false;
            return self.next_scene.take().unwrap_or_default();
        }
        NextScene::None
    }
}
