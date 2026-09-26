//! 活动列表页：展示服务端下发的一次性活动（UML 活动），是进入 [`EventScene`] 的入口。
//!
//! 与章节合辑页的区别在于**数据来源与生命周期**：章节是内置静态数据，而活动列表
//! 进入页面时才向服务端查询（见 [`EventPage::fetch_task`]），因此页面必须先处理“加载中/加载失败”。
//!
//! 交互上同样采用“卡片放大到全屏”的转场衔接场景切换，但有两处明显不同：
//! - 转场绘制在 [`Page::render`] 内而不是 [`Page::render_top`]，所以它不会盖住标题栏；
//! - 转场期间的返回键被本页吞掉（见 [`EventPage::on_back_pressed`]），避免动画与出栈互相干扰。
//!
//! 列表本身**不翻页**：一次查询取回全部活动，滚动位置与当前项索引在本地互相推导。

use super::{Illustration, Page, SharedState};
use crate::{
    client::{Client, Event},
    icons::Icons,
    scene::EventScene,
};
use anyhow::Result;
use macroquad::prelude::*;
use nalgebra::Rotation2;
use prpr::{
    core::Tweenable,
    ext::{semi_black, semi_white, RectExt, SafeTexture, ScaleType},
    scene::{show_error, NextScene},
    task::Task,
    ui::{button_hit_large, DRectButton, RectButton, Scroll, Ui},
};
use std::{borrow::Cow, sync::Arc};

/// “卡片放大到全屏”的时长（秒）。
///
/// 正反两个方向共用这一个常量：从活动返回时 `tr_from` 仍保留着离开前记录的卡片矩形，
/// 因此收回动画的路径与正向完全一致，只有时间方向相反（见 [`EventPage::render`] 里的 `grow`）。
const TRANSIT_TIME: f32 = 0.5;
/// 封面在转场过程中与终态之间的羽化余量，用于让放大结束时边缘不过于生硬。
const ILLU_FEATHER: f32 = 0.4;

/// 列表里的一项活动。
struct Item {
    /// 服务端返回的活动数据（标题、封面文件、UML 布局等），转场完成后要整体移交给 [`EventScene`]。
    event: Event,
    /// 活动封面。用 `from_file` 取**原图**，因为点击后会被放大到整屏。
    illu: Illustration,
    /// 命中区域 + 按压缩放动画的载体。
    btn: DRectButton,
}

// 活动项是“数据 + 句柄”的简单包装，构造时只建立异步加载而不等待。
impl Item {
    /// 由服务端返回的活动数据构造列表项。
    ///
    /// 按钮显式 `no_sound()`：点击后立刻进入整屏转场，此时再播“点击音”会与场景切换的声音冲突。
    pub fn new(event: Event) -> Self {
        let illu = Illustration::from_file(event.illustration.clone());
        Self {
            event,
            illu,
            btn: DRectButton::new().no_sound(),
        }
    }
}

/// 活动列表页。
pub struct EventPage {
    /// 拉取活动列表的一次性任务。
    ///
    /// 它同时充当“加载中”标志（见 [`EventPage::loading`]）：`Some` 表示还在请求，
    /// 页面据此显示全屏加载指示并屏蔽输入；`None` 表示请求已结束（成功或失败）。
    /// 注意失败后不会再重试，页面会停留在空列表状态。
    fetch_task: Option<Task<Result<Vec<Event>>>>,
    /// 纵向滚动容器；吸附步长在 `render` 里设为“一屏一项”。
    scroll: Scroll,
    /// 已加载的活动列表；`None` 表示尚未拿到数据（可能仍在加载，也可能已加载失败）。
    events: Option<Vec<Item>>,
    /// 当前居中显示的那一项的索引。
    ///
    /// 它有两个来源：`render` 每帧从滚动偏移反推（保证与视觉一致），
    /// 以及上/下按钮点击时主动设置（随后由滚动吸附把画面带过去）。
    index: usize,

    /// 向下翻一屏的按钮（绘制成旋转的返回箭头）。
    btn_down: RectButton,
    /// 向上翻一屏的按钮。
    btn_up: RectButton,

    /// 转场起点：当前项在全屏坐标系下的矩形，由 `render` 记录。
    ///
    /// 页面离开后它不会被清空，正是靠这一点，从活动返回时才能播放“从全屏收回该卡片”的反向动画。
    tr_from: Rect,
    /// 转场开始时间；`NaN` 表示没有转场，**符号表示方向**（正为放大进入，负为收回）。
    tr_start: f32,

    /// 是否尚未进入过本页。首次进入没有可收回的前情，只清标记不启动反向动画。
    first_in: bool,

    /// 待交给 `MainScene` 的场景切换请求；由转场动画播完后置位。
    next_scene: Option<NextScene>,

    /// 通用图标资源。
    icons: Arc<Icons>,
    /// 8 个段位图标，构造 [`EventScene`] 时需要一并移交。
    rank_icons: [SafeTexture; 8],
}

// 构造与查询状态。本页自身不持有任何“活动”数据，数据完全来自 `fetch_task`。
impl EventPage {
    /// 活动标题距卡片左下角的边距（UI 单位）。绘制与转场两处都要用，故抽成常量避免不一致。
    pub const LB_PAD: f32 = 0.05;

    /// 创建活动列表页，并在构造时**立即**发起列表查询。
    ///
    /// 之所以在构造函数里就发请求（而不是等 `enter`）：页面对象在点击入口的同一时刻创建，
    /// 越早开始网络请求，用户看到加载指示的时间就越短。
    ///
    /// 只取响应的 `.0`（活动数组）而丢弃 `.1`（总数）——本页不做服务端翻页，
    /// 一次取回全部活动后在本地滚动。
    pub fn new(icons: Arc<Icons>, rank_icons: [SafeTexture; 8]) -> Self {
        Self {
            fetch_task: Some(Task::new(async move { Ok(Client::query().send().await?.0) })),
            scroll: Scroll::new(),
            events: None,
            index: 0,

            btn_down: RectButton::new(),
            btn_up: RectButton::new(),

            tr_from: Rect::default(),
            tr_start: f32::NAN,

            first_in: true,

            next_scene: None,

            icons,
            rank_icons,
        }
    }

    /// 列表是否仍在加载中（等价于“请求任务还没被取走”）。
    ///
    /// 加载期间本页会屏蔽全部输入并显示全屏加载指示，避免用户在空列表上乱点。
    fn loading(&self) -> bool {
        self.fetch_task.is_some()
    }
}

// 本页在页面栈中的行为约定：
// - 加载中一律拒绝输入并显示全屏加载指示（`render_top`），避免在空列表上误触；
// - 点击活动卡片后先播 0.5 秒的放大动画，动画结束的那一帧才把 [`EventScene`] 交出去；
// - 从活动返回时复用上次记录的 `tr_from` 播放收回动画，因此卡片位置与离开时完全一致；
// - 转场期间吞掉返回键（`on_back_pressed`），防止“动画未播完就被弹栈”导致状态错乱。
impl Page for EventPage {
    /// 标题栏文案。
    ///
    /// 刻意从**活动场景**的语言文件（`crate::scene::event`）取词而不是本模块：
    /// 标题要与其后进入的活动页保持一致，也便于两处共用同一份译文。
    fn label(&self) -> Cow<'static, str> {
        use crate::scene::event::{tl, L10N_LOCAL};
        tl!("label")
    }

    /// 重新成为栈顶时启动“从全屏收回卡片”的动画（首次进入只清标记）。
    ///
    /// 用负时间编码方向：`render` 里的 `grow = tr_start > 0` 据此决定是放大还是收回，
    /// 并且只有正向动画播完才会真正切换场景。
    fn enter(&mut self, s: &mut SharedState) -> Result<()> {
        if self.first_in {
            self.first_in = false;
        } else {
            self.tr_start = -s.rt;
        }
        Ok(())
    }

    /// 处理触摸：加载中屏蔽一切输入，否则依次尝试滚动、卡片点击、上/下翻页按钮。
    ///
    /// 几个刻意的取舍：
    /// - 加载期间直接返回 `true`：`MainScene` 在页面已消费输入时不会再处理返回键，
    ///   所以加载中连返回都被吞掉（用户无法中途退出本页）。这是刻意取舍：
    ///   宁可短暂“卡住”，也不要在列表未就绪时让界面进入难以预期的状态；
    /// - 滚动优先于卡片点击：开始拖动后由 [`Scroll`] 接管事件，滑动不会误触成点击；
    /// - 点击卡片时立刻 `halt()` 掉滚动惯性：否则卡片还会继续滑走，
    ///   而转场起点 [`EventPage::tr_from`] 是上一帧记录的，画面与起点就会错位；
    /// - 转场时间用**真实时间** `s.rt`（与 `render` 中的判定一致），滚动与按钮动画则用游戏时间 `t`。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        let t = s.t;
        // 阶段一：加载中——全部输入视为已被消费
        if self.loading() {
            return Ok(true);
        }
        // 阶段二：滚动容器优先（拖动时不会命中卡片）
        if self.scroll.touch(touch, t) {
            return Ok(true);
        }
        if let Some(events) = &mut self.events {
            // 阶段三：卡片命中。仅在无转场时响应，防止动画途中重复进入
            for item in events.iter_mut() {
                if self.tr_start.is_nan() && item.btn.touch(touch, t) {
                    button_hit_large();
                    self.scroll.y_scroller.halt();
                    self.tr_start = s.rt;
                    return Ok(true);
                }
            }
            // 阶段四：上/下翻页按钮。它们只改动吸附目标，不直接改滚动位置，
            // 位移交给滚动器的吸附动画完成。
            if self.btn_up.touch(touch) {
                self.index = self.index.saturating_sub(1);
                self.scroll.y_scroller.goto_step(self.index);
                return Ok(true);
            }
            if self.btn_down.touch(touch) {
                self.index = (self.index + 2).min(events.len()) - 1;
                self.scroll.y_scroller.goto_step(self.index);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 每帧推进：滚动、封面结算、以及活动列表请求的轮询。
    ///
    /// 请求只处理一次：无论成功还是失败，`fetch_task` 都会被置回 `None`。
    /// 失败时只弹一个错误提示并**保留空列表**——本页没有重试入口，
    /// 用户需要退出再进入才能重新请求（`new` 里会重新发起）。
    fn update(&mut self, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        // 阶段一：滚动推进（吸附与惯性）。这里用游戏时间，与 `touch` 中的 `t` 保持一致
        self.scroll.update(t);
        // 阶段二：已有列表时逐项结算封面
        if let Some(events) = &mut self.events {
            for item in events {
                item.illu.settle(t);
            }
        }
        // 阶段三：请求结果。注意 `take()` 在任务未就绪时返回 `None`，此时什么都不做
        if let Some(task) = &mut self.fetch_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        use crate::scene::event::{tl, L10N_LOCAL};
                        show_error(err.context(tl!("load-list-failed")));
                    }
                    Ok(val) => {
                        self.events = Some(val.into_iter().map(Item::new).collect());
                    }
                }
                self.fetch_task = None;
            }
        }
        Ok(())
    }

    /// 绘制活动列表：一屏一项的大卡片，最后再叠加转场动画。
    ///
    /// 三段职责（顺序即绘制层次）：
    /// 1. 主体列表——吸附步长设为“整屏高”，使滚动停止时总有一项正好占满屏幕；
    /// 2. 底部翻页按钮与空列表提示——它们也在 `render_fader` 内，会随转场一起淡出；
    /// 3. 转场动画——画在**本函数内部**而不是 `render_top`，因此它会和普通内容一起
    ///    参与页面的位移与透明度，也不会盖住标题栏。
    ///
    /// 一个关键的副作用发生在第一段：每项的命中区域 `btn` 与转场起点 `tr_from`
    /// 都在这里登记，所以“渲染过一帧”是本页可交互的前提。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;

        // 阶段一：吸附步长 = 一整屏。每项占满一屏，翻页按钮与滚动吸附才有一致的对齐点
        self.scroll.y_scroller.step = ui.top * 2.;

        s.render_fader(ui, |ui| {
            if let Some(events) = &mut self.events {
                ui.scope(|ui| {
                    ui.dx(-1.);
                    ui.dy(-ui.top);
                    self.scroll.size((2., ui.top * 2.));
                    self.scroll.render(ui, |ui| {
                        ui.dx(1.);
                        ui.dy(ui.top);
                        // 逐项绘制。注意：滚动容器裁剪掉的“屏幕外项”同样会走到这里，
                        // 所以 `notify` 实际上等于**一次性放行全部项的封面加载**，
                        // 而不是只加载可见的那一项。
                        for (index, item) in events.iter_mut().enumerate() {
                            item.illu.notify();
                            let ca = item.illu.alpha(t);
                            // 用负的 feather 把全屏收窄，让卡片四周露出一点相邻项，
                            // 从而暗示“这是一条可以上下滑动的列表”，而不是一页静态画面
                            let r = ui.screen_rect().nonuniform_feather(-0.24, -0.144);
                            // `render_shadow` 负责路径（圆角 + 阴影 + 按压反馈），回调只填内容。
                            // 整体套 `ui.alpha(ca)`：封面与标题一起淡入，避免标题先出现、图片后到
                            item.btn.render_shadow(ui, r, t, |ui, path| {
                                ui.alpha(ca, |ui| {
                                    ui.fill_path(&path, item.illu.shading(r.feather(ILLU_FEATHER), t));
                                    ui.fill_path(&path, semi_black(0.4 * if item.illu.task.is_some() { 1. } else { ca }));
                                });
                                ui.text(&item.event.name)
                                    .pos(r.x + Self::LB_PAD, r.bottom() - Self::LB_PAD)
                                    .anchor(0., 1.)
                                    .size(1.3)
                                    .draw();
                            });
                            // 记录当前项的全局矩形，作为之后放大/收回动画的起点。
                            // 用 `rect_to_global` 是因为 `r` 还处在滚动造成的局部变换里，
                            // 转场动画在全局坐标系中播放，必须先换算。
                            if index == self.index {
                                self.tr_from = ui.rect_to_global(r);
                            }
                            // 每项向后推进一整屏的高度
                            ui.dy(ui.top * 2.);
                        }
                        // 内容总高 = 项数 × 一屏高；宽度按整屏给（内容不横向滚动）
                        (2., events.len() as f32 * ui.top * 2.)
                    });
                });
            }
        });

        if let Some(events) = &self.events {
            // 阶段二：上下翻页箭头与空列表提示。
            // 单独再起一个 `render_fader`（而不并入上面那段）：画在滚动容器之外，
            // 才能不受列表那套局部坐标变换与裁剪的影响，稳定钉在屏幕上下边缘。
            s.render_fader(ui, |ui| {
                let d = ui.top - 0.057;
                let s = 0.04;
                // 绘制用的基准方块位于 (-d, 0)，再用 ±90° 旋转把它分别送到屏幕正中偏上/偏下，
                // 于是同一个箭头图标既是“向上翻”也是“向下翻”，无需额外资源
                let r = Rect::new(-d, 0., 0., 0.).feather(s);
                self.btn_up.set(ui, Rect::new(0., -d, 0., 0.).feather(s));
                self.btn_down.set(ui, Rect::new(0., d, 0., 0.).feather(s));
                // 由滚动偏移反推当前吸附到的项：步长与内容尺寸一致，因此结果天然落在 [0, len-1]
                self.index = (self.scroll.y_scroller.offset / (ui.top * 2.)).round() as usize;
                ui.with(Rotation2::new(std::f32::consts::FRAC_PI_2).into(), |ui| {
                    // 已经到头（第一项）时把箭头压暗到 0.3，作为“不可再翻”的视觉提示
                    ui.fill_rect(r, (*self.icons.back, r, ScaleType::CropCenter, semi_white(if self.index == 0 { 0.3 } else { 1. })));
                });
                ui.with(Rotation2::new(-std::f32::consts::FRAC_PI_2).into(), |ui| {
                    ui.fill_rect(r, (*self.icons.back, r, ScaleType::CropCenter, semi_white(if self.index + 1 >= events.len() { 0.3 } else { 1. })));
                });
                // 服务器返回空列表时给一句说明，否则用户只会看到两个可点的箭头
                if events.is_empty() {
                    ui.text(ttl!("list-empty")).anchor(0.5, 0.5).no_baseline().size(1.4).draw();
                }
            });
        }

        // 阶段三：转场动画。画在 `render` 内部，因此会随页面本身的转场一起位移/淡入淡出。
        // `tr_start` 非 NaN 就代表有动画在跑；起点 `tr_from` 是上一帧记录下来的卡片矩形，
        // 它跨页面保留，所以从活动返回时也能直接复用它播反向动画。
        if !self.tr_start.is_nan() {
            // `unwrap` 的前提：`tr_start` 只可能由“点击列表项”置位，那时 `events` 必然是 `Some`
            let item = &self.events.as_ref().unwrap()[self.index];
            let p = if self.tr_start.is_nan() {
                1.
            } else {
                // 时间基准取绝对值的含义：正负号只表达方向，不代表时间原点不同
                let p = ((s.rt - self.tr_start.abs()) / TRANSIT_TIME).min(1.);
                let grow = self.tr_start > 0.;
                // 终点判定：只有正向（进入活动）才真正交出场景；
                // 反向（从活动返回）到此只需清掉动画状态，因为页面本身已经回到栈顶。
                // 注意这里会把 `tr_start` 置回 NaN（即“动画结束”）。
                if p >= 1. {
                    if grow {
                        self.next_scene = Some(NextScene::Overlay(Box::new(EventScene::new(
                            item.event.clone(),
                            item.illu.clone(),
                            Arc::clone(&self.icons),
                            self.rank_icons.clone(),
                        ))));
                    }
                    self.tr_start = f32::NAN;
                }
                // 四次方缓动：比三次更“急进缓收”，让放大过程更有冲劲。
                // 正向把参数从 0 推到 1（放大），反向则从 1 退回 0（收回卡片）。
                let p = (1. - p).powi(4);
                if grow {
                    1. - p
                } else {
                    p
                }
            };
            // 圆角随放大收敛到 0：终态是无圆角的全屏矩形，与活动场景的画面无缝衔接
            let r = Rect::tween(&self.tr_from, &ui.screen_rect(), p);
            let path = r.rounded(0.02 * (1. - p));
            // 纹理映射矩形比卡片略大，随放大收敛到全屏；这样起始时封面是被轻微裁切的，
            // 放大过程中不会在边缘露出未覆盖的区域
            ui.fill_path(&path, item.illu.shading(r.feather((1. - p) * ILLU_FEATHER), t));
            // 固定压暗 0.4：活动名的白字要盖在任意封面上，必须保证可读性
            ui.fill_path(&path, semi_black(0.4));
            ui.text(&item.event.name)
                .pos(r.x + Self::LB_PAD, r.bottom() - Self::LB_PAD)
                .anchor(0., 1.)
                .size(1.3 + p * 0.2)
                .draw();
        }
        Ok(())
    }

    /// 最上层覆盖：加载期间显示全屏加载指示（会盖住标题栏与返回按钮）。
    ///
    /// 只在请求尚未结束时出现；一旦 `fetch_task` 被取走（无论成功失败）就不再显示。
    fn render_top(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        if self.loading() {
            ui.full_loading_simple(t);
        }
        Ok(())
    }

    /// 返回键是否由本页处理：转场进行中一律吞掉。
    ///
    /// 动机是保护动画与栈状态的一致性——若动画途中就把本页弹掉，
    /// `tr_start`/`tr_from`/`next_scene` 会留下一组互相矛盾的值，
    /// 再次进入本页时可能立刻播放一段来历不明的动画。
    ///
    /// 注意这里只是“吞掉按键”，并不会取消已经开始的正向转场：动画仍会播完，
    /// 并且照样把活动场景交出去（用户需再按一次返回才能退出活动）。
    fn on_back_pressed(&mut self, _s: &mut SharedState) -> bool {
        !self.tr_start.is_nan()
    }

    /// 取走待切换的活动场景（仅在正向转场播完的那一帧有值，取出后不会重复交付）。
    fn next_scene(&mut self, _s: &mut SharedState) -> NextScene {
        self.next_scene.take().unwrap_or_default()
    }
}
