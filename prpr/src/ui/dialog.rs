//! 模态对话框。
//!
//! 这里的「模态」不是靠状态标志实现的，而是靠**触摸分发顺序**：
//! [`Dialog::show`] 把自身塞进 `scene::DIALOG` 这个 thread-local，
//! 而引擎主循环在分发触摸时先问 `DIALOG`、只有当它为空时才把事件交给当前场景
//! （见 `scene.rs` 中 `DIALOG.with(...)` 的那段 `retain_mut`）。渲染顺序同理，
//! 对话框在所有场景内容之后绘制，因此天然盖在最上层。
//!
//! 为什么必须让对话框「抢在场景前面」处理触摸：对话框在视觉上完全遮挡了底层场景，
//! 如果没有优先级，用户点「确定」时会同时点到对话框后面被遮住的按钮，
//! 触发一连串看不见的副作用（切场景、删除谱面等等）。用全局 thread-local 而不是
//! 把 `Dialog` 塞进各个场景结构体，则是为了让任何深层的 UI 辅助代码都能就地弹出对话框，
//! 不必把 `&mut` 一路透传到调用栈顶端。
//!
//! 同一时刻只允许存在一个对话框：新的 `show()` 会直接覆盖旧的（不会排队）。

prpr_l10n::tl_file!("dialog");

use super::{DRectButton, RectButton, Scroll, Ui};
use crate::{core::BOLD_FONT, ext::RectExt, scene::show_message};
use anyhow::Error;
use macroquad::prelude::*;

/// 对话框宽度占设计空间宽度的比例（0.5 即半屏宽）。
const WIDTH_RADIO: f32 = 0.5;
/// 对话框高度上限占设计空间高度的比例。
/// 超过此比例时高度被截断，正文改由内部的 [`Scroll`] 滚动查看——
/// 这也是对话框正文必须套 `Scroll` 的原因：错误信息可能长达上千行。
const HEIGHT_RATIO: f32 = 0.7;

/// 按钮/正文点击的回调签名。
///
/// 参数是**点击位置编号**：`>= 0` 表示第几个按钮，`-1` 表示点在窗口外，
/// `-2` 表示点在正文上。返回值 `true` 表示「保持对话框打开」，`false` 表示关闭。
/// 这个隐式契约没有类型层面的强制，改动时必须与 [`Dialog::touch`] 中的调用处同步。
type DialogListener = dyn FnMut(&mut Dialog, i32) -> bool;
/// 链接行点击的回调签名，参数为链接行的下标。
type LinkListener = dyn FnMut(usize);

/// 一个模态对话框。
///
/// `#[must_use]` 是为了防止调用方构造完却忘了调用 [`Dialog::show`]——
/// 构造出的对话框本身不会显示，必须显式入队。
#[must_use]
pub struct Dialog {
    /// 标题文本，渲染时使用粗体 ([`BOLD_FONT`])，与正文形成视觉层级。
    title: String,
    /// 正文文本，支持多行；过长时由内部 [`Scroll`] 承载。
    message: String,
    /// 底部按钮的文字列表。长度必须与 `rect_buttons` 一致（见 [`Dialog::set_buttons`]）。
    buttons: Vec<String>,
    /// listener function returns `false` to close the dialog, `true` to keep it open
    /// the parameter is the *index* of the button clicked, `-1` for outside click, `-2` for text
    listener: Option<Box<DialogListener>>,

    /// Clickable link rows drawn below the message body. Each entry is
    /// `(label, url)`; `on_link` is invoked with the row index when tapped.
    links: Vec<(String, String)>,
    /// 链接行被点击时的回调，收到行下标。
    on_link: Option<Box<LinkListener>>,
    /// 链接行的命中区域，在 [`Dialog::links`] 里按行数一次性建好；
    /// 与 `links` 必须**一一对应且同序**，否则渲染与命中会错位。
    link_buttons: Vec<RectButton>,

    /// 正文整体的命中区域，用于把「点在正文上」上报为 `-2`。
    text_btn: RectButton,

    /// 缓存的窗口高度。
    ///
    /// `None` 表示尚未测量，第一次 [`Dialog::render`] 时按文本实测高度计算并写入。
    /// 之所以要缓存：一是文本测量（换行排版）开销不小、不该每帧重算；
    /// 二是高度必须跨帧稳定，否则同一段文本在两次测量间出现微小差异会让窗口上下抖动。
    h: Option<f32>,

    /// 正文的滚动容器。存在于此有两个原因：长文本（如 anyhow 的完整错误链）必须可滚动，
    /// 以及在窗口高度被 `HEIGHT_RATIO` 截断后仍能访问全部内容。
    scroll: Scroll,
    /// 窗口的**全局**矩形，仅在 `render` 之后有值。
    /// 用于区分「点在窗口内」与「点在窗口外」，即 `-1` 与 `-2` 的判定依据。
    window_rect: Option<Rect>,
    /// 底部按钮的绘制对象（带圆角与投影的 `DRectButton`）。
    rect_buttons: Vec<DRectButton>,

    /// 倒计时开始的时间戳；`0.0` 是「尚未开始」的哨兵值，
    /// 由 [`Dialog::update`] 在第一次更新时填入。
    countdown_start: f64,
    /// 倒计时秒数；`<= 0` 表示不启用倒计时（默认 `-1`）。
    countdown_seconds: i32,
}

// 实现语义：给出一套「最朴素」的默认值——标题为通用的「提示」、正文为空、
// 只有一个「确定」按钮、没有回调。`countdown_seconds = -1` 与
// `countdown_start = 0.0` 一同表示「未启用倒计时」。
// 注意 `rect_buttons` 预先放了一个元素，与默认的单个按钮对应；
// 通过 `set_buttons` 改按钮数量时会同步重建，但直接写结构体字段（如 `error`）时必须自己维护。
impl Default for Dialog {
    fn default() -> Self {
        Self {
            title: tl!("notice").to_string(),
            message: String::new(),
            buttons: vec![tl!("ok").to_string()],
            listener: None,

            links: Vec::new(),
            on_link: None,
            link_buttons: Vec::new(),

            text_btn: RectButton::new(),

            h: None,

            scroll: Scroll::new(),
            window_rect: None,
            rect_buttons: vec![DRectButton::new()],

            countdown_start: 0.0,
            countdown_seconds: -1,
        }
    }
}

impl Dialog {
    /// 构造最简单的通知框：沿用默认标题与单个「确定」按钮，只需要一段正文。
    ///
    /// 没有设置 `listener`，因此 [`Dialog::touch`] 里会走「无回调即关闭」的分支——
    /// 点任意按钮都会关掉它。这也是「纯提示」场景最常用的构造方式。
    pub fn simple(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Default::default()
        }
    }

    /// 与 [`Dialog::simple`] 相同，但允许自定义标题。
    ///
    /// 与 `simple` 拆成两个函数而不是让标题可选，是为了在调用点直接看出语义：
    /// 只想提示用 `simple`，需要明确标题（如「更新说明」「确认删除」）用 `plain`。
    pub fn plain(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            ..Default::default()
        }
    }

    /// 构造错误对话框：标题固定为「错误」，按钮为「复制错误信息 + 确定」。
    ///
    /// 三个不同于 `plain` 的地方：
    /// - 正文用 `format!("{error:?}")` 而不是 `Display`：`anyhow::Error` 的 `Debug`
    ///   会打印完整的错误链（每一层 `context` 与 backtrace 标记），而 `Display` 只有最外层一句，
    ///   对排查问题几乎没有帮助；用户要复制给开发者的正是这份完整信息。
    /// - 自带 `listener`：下标 0（复制按钮）把文本写进系统剪贴板并弹出「已复制」提示；
    ///   回调最后返回 `false`，因此**复制后对话框也会关闭**，这是刻意的行为——
    ///   避免用户点完复制以为已经处理完，却还留着一个窗口挡路。
    /// - 直接写 `buttons` 字段并手动准备 `rect_buttons: vec![...; 2]`，
    ///   因为这里绕过了 `set_buttons`，两处长度必须自己对齐。
    pub fn error(error: Error) -> Self {
        let error = format!("{error:?}");
        Self {
            title: tl!("error").to_string(),
            message: error.clone(),
            buttons: vec![tl!("error-copy").to_string(), tl!("ok").to_string()],
            listener: Some(Box::new(move |_dialog, pos| {
                if pos == 0 {
                    // # Safety
                    // 调用 `get_internal_gl()` 会取用 macroquad 的全局 GL 上下文引用，
                    // 仅当上下文已初始化（即运行在引擎的更新/渲染循环内）时才有效。
                    // 本闭包只可能由主循环的触摸分发路径触发，因此该前提总成立。
                    unsafe { get_internal_gl() }.quad_context.clipboard_set(&error);
                    show_message(tl!("error-copied")).ok();
                }
                false
            })),

            rect_buttons: vec![DRectButton::new(); 2],
            ..Default::default()
        }
    }

    /// builder 风格地替换标题。
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// builder 风格地替换正文。
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.set_message(message);
        self
    }

    /// 就地替换正文（供回调内部动态更新文本，例如显示进度）。
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }

    /// builder 风格地替换按钮列表。
    pub fn buttons(mut self, buttons: Vec<String>) -> Self {
        self.set_buttons(buttons);
        self
    }

    /// 替换按钮列表，并**同步重建**对应的 `rect_buttons`。
    ///
    /// 重建而非增量调整，是因为按钮数量变化会改变每个按钮的宽度与位置
    /// （渲染时按数量均分宽度），旧的命中区域全部失效；
    /// 顺带也会丢弃这些按钮的按压动画状态，所以应在 `show` 之前调用。
    pub fn set_buttons(&mut self, buttons: Vec<String>) {
        self.buttons = buttons;
        self.rect_buttons = vec![DRectButton::new(); self.buttons.len()];
    }

    /// 设置点击回调。
    ///
    /// 闭包拿到 `&mut Dialog` 是为了能在回调里就地修改对话框内容（改文本、按钮等）；
    /// 由于回调期间无法再调用 `show()`（会与借用冲突），需要换对话框时应借助外部状态。
    pub fn listener(mut self, f: impl FnMut(&mut Dialog, i32) -> bool + 'static) -> Self {
        self.listener = Some(Box::new(f));
        self
    }

    /// Adds clickable link rows drawn below the message body. Each entry is
    /// `(label, url)`; tapping a row invokes `on_link` with its index.
    /// 命中区域在此按行数一次性建好，因此 `links` 一旦设置就不再变更。
    pub fn links(mut self, links: Vec<(String, String)>) -> Self {
        self.link_buttons = (0..links.len()).map(|_| RectButton::new()).collect();
        self.links = links;
        self
    }

    /// Sets the callback fired when a link row is tapped, receiving its index.
    /// 回调只收到下标而不直接收到 URL，是让调用方自行决定用系统浏览器打开、
    /// 还是在应用内部跳转（例如跳转到本地谱面页）。
    pub fn on_link(mut self, f: impl FnMut(usize) + 'static) -> Self {
        self.on_link = Some(Box::new(f));
        self
    }

    /// 设置倒计时秒数：倒计时结束前，除第一个按钮外的按钮都不可点击。
    ///
    /// 用于「危险操作」二次确认，逼用户等几秒冷静一下；保留下标 0 可用，
    /// 是为了让「取消」这类安全操作始终可点。
    pub fn countdown(mut self, seconds: i32) -> Self {
        self.countdown_seconds = seconds;
        self
    }

    /// 把对话框挂到全局 `DIALOG` 槽位上，使其在下一帧开始接收触摸并被绘制。
    ///
    /// 采用「写入 thread-local」而不是直接返回一个需要场景自己持有的对象，
    /// 是为了让任何代码（包括场景内部很深层的辅助函数）都能就地弹出对话框，
    /// 无需把 `&mut Dialog` 一路透传回顶层。代价是全局单例语义：
    /// 同一时刻只能有一个对话框，后写入的直接覆盖先写入的。
    ///
    /// 这里用 `*it.borrow_mut() = Some(self)` 覆盖而不是 `.get_or_insert()`：
    /// 覆盖即「新对话框抢占屏幕」，避免旧对话框（例如已经过时的错误提示）
    /// 一直霸占着导致新提示弹不出来。
    pub fn show(self) {
        crate::scene::DIALOG.with(|it| *it.borrow_mut() = Some(self));
    }

    /// 处理一次触摸并返回「对话框是否继续存活」。
    ///
    /// # Returns
    /// `true` 表示保留对话框；`false` 表示应当关闭它。
    /// 调用方（`scene.rs` 的主循环）依据该返回值决定是否把 `DIALOG` 清空。
    ///
    /// 回调约定（隐式契约，必须与 `listener` 的使用方保持一致）：
    /// - `listener(self, index as i32)`，`index >= 0` 为被点击按钮的下标；
    /// - 回调返回 `false` → 本次交互要求关闭对话框；返回 `true` → 保持打开；
    /// - 回调还可能收到 `-1`（点击落在窗口之外）与 `-2`（点击落在正文上）；
    /// - 若从未设置 `listener`，点击任意按钮都直接关闭（见下面 `exit = true` 的分支）。
    ///
    /// 处理顺序即优先级：按钮 → 链接行 → 正文 → 窗口外。
    /// 链接必须早于正文命中，否则链接会被正文整体区域吞掉。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        // 先把事件交给内部滚动容器，长正文才能用手指滑动查看。
        self.scroll.touch(touch, t);
        let mut exit = false;
        // 阶段一：按钮。倒计时未结束时下标 > 0 的按钮被屏蔽（保留下标 0 作「取消」）。
        for (index, btn) in self.rect_buttons.iter_mut().enumerate() {
            let blocked = self.countdown_seconds > 0 && index > 0 && (self.countdown_seconds as f64 - (t as f64 - self.countdown_start)) > 0.0;
            if !blocked && btn.touch(touch, t) {
                if let Some(mut listener) = self.listener.take() {
                    // 先把 listener 取出再调用、调用完放回，是为了绕开借用检查：
                    // 回调需要 `&mut self`，而 `self.listener` 也是 `self` 的一部分。
                    // 若回调内部 panic，listener 会留在 `None`，随后点击按钮将退化为直接关闭。
                    if !listener(self, index as i32) {
                        exit = true;
                    }
                    self.listener = Some(listener);
                    break;
                } else {
                    exit = true;
                    break;
                }
            }
        }
        // Link rows sit inside the message body, below the message text. Test
        // them before the whole-body `text_btn` so a link tap never falls
        // through to the `-2` text click or the `-1` outside-click close.
        // 阶段二：链接行。命中即消费本次触摸并保持对话框打开——
        // 链接是「了解详情」，不应该顺手把提示框关掉。
        for (index, btn) in self.link_buttons.iter_mut().enumerate() {
            if btn.touch(touch) {
                if let Some(cb) = self.on_link.as_mut() {
                    cb(index);
                }
                return true; // consume the touch, keep the dialog open
            }
        }
        // 阶段三：正文。只上报 `-2`，不改 `exit`——点正文默认不关闭对话框。
        if self.text_btn.touch(touch) {
            if let Some(mut listener) = self.listener.take() {
                listener(self, -2);
                self.listener = Some(listener);
            }
        }
        if exit {
            return false;
        }

        // 阶段四：窗口外的点击。只有在**按下**就落在窗口外时才算「点外部」
        // （`touch.phase != TouchPhase::Started` 时放行），否则手指从窗口内滑到窗口外
        // 松手也会把对话框关掉，手感很差。此时允许 `listener` 用 `true` 否决关闭，
        // 用于实现「必须做出选择」的强制确认框。
        if self
            .window_rect
            .is_none_or(|rect| rect.contains(touch.position) || touch.phase != TouchPhase::Started)
        {
            true
        } else {
            if let Some(mut listener) = self.listener.take() {
                let result = listener(self, -1);
                self.listener = Some(listener);
                if result {
                    return true;
                }
            }
            false
        }
    }

    /// 每帧更新：推进正文滚动，并在首次调用时给倒计时打上起始时间戳。
    ///
    /// 用 `countdown_start == 0.0` 作为「未开始」的哨兵，好处是不必再加一个布尔字段；
    /// 边界情形是：若倒计时恰好从游戏时间 0.0 开始，该判断会持续成立，
    /// 每帧都把起点重置为当前时间，倒计时将永远不结束。实际运行中触摸事件的时间戳
    /// 不会精确等于 0.0（帧时间在此之前已推进），因此该缺陷只在理论上存在。
    pub fn update(&mut self, t: f32) {
        self.scroll.update(t);
        if self.countdown_seconds > 0 && self.countdown_start == 0.0 {
            self.countdown_start = t as f64;
        }
    }

    /// 绘制对话框。
    ///
    /// # Arguments
    /// * `t` — 当前时间，用于按钮按压动画与倒计时剩余秒数的显示
    ///
    /// 布局约定：窗口以**当前 UI 原点为中心**（`wr.x/y` 各减去自身一半宽高），
    /// 因此调用方不需要自己定位；这隐含要求渲染对话框时 UI 原点位于屏幕中心，
    /// 否则窗口会画偏。
    ///
    /// # Panics
    /// 若 `buttons` 为空，下面计算按钮宽度时会除以 0 得到 `NaN`（不 panic，
    /// 但布局 NaN 会导致按钮不显示）；正常使用中按钮数恒 >= 1。
    pub fn render(&mut self, ui: &mut Ui, t: f32) {
        // 阶段一：遮罩。铺满全屏的半透明黑，既压暗背景突出对话框，
        // 也在视觉上表达「底层不可交互」。它同时是 outside-click 命中判定的背景。
        ui.fill_rect(ui.screen_rect(), Color::new(0., 0., 0., 0.6));

        // 阶段二：确定窗口尺寸。`mh` 是高度上限（超出的部分交给内部 Scroll）。
        let mh = ui.top * 2. * HEIGHT_RATIO;
        let s = 0.02;
        let pad = 0.02;
        let bh = 0.09;

        // 高度只在首帧测量一次并缓存（见字段 `h` 的说明）。
        // 组成：正文实测高度（按 2*WIDTH_RADIO 减去两侧内边距作为换行宽度）
        // + 标题高度 + 按钮条 + 0.22 的固定余量（标题上下留白等）+ 链接行高度。
        // 链接行这里按每行 0.08 估算，与实际绘制时的行高略有出入，
        // 但由于超出部分会落到 Scroll 里，这点误差只影响窗口初始高度观感。
        if self.h.is_none() {
            let link_h = self.links.len() as f32 * 0.08;
            self.h = Some(
                (ui.text(&self.message)
                    .size(0.5)
                    .max_width(2. * WIDTH_RADIO - pad * 3.)
                    .multiline()
                    .measure()
                    .h
                    + ui.text(&self.title).size(0.95).no_baseline().measure().h
                    + bh
                    + 0.22
                    + link_h)
                    .min(mh),
            );
        }
        let mut wr = Rect::new(0., 0., 2. * WIDTH_RADIO, self.h.unwrap());
        wr.x = -wr.w / 2.;
        wr.y = -wr.h / 2.;
        // 记录全局矩形：`touch` 里判定「点外部」用的就是它。
        // 必须在 apply 了当前 UI 变换之后取全局坐标，否则在缩放/平移过的 UI 下会误判。
        self.window_rect = Some(ui.rect_to_global(wr));
        ui.fill_path(&wr.rounded(0.01), ui.background());

        ui.scope(|ui| {
            let s = 0.01;
            let pad = 0.02;
            // `h` 累计本 scope 已用掉的高度，用于最后算出正文滚动区还剩多少空间。
            // 这正是 `dy!` 宏存在的原因——每次下移都要同步记账，
            // 否则下面的 `scroll.size` 无法知道剩余高度。
            let mut h = 0.;
            macro_rules! dy {
                ($val:expr) => {{
                    let dy = $val;
                    h += dy;
                    ui.dy(dy);
                }};
            }
            // 阶段三：标题。使用 BOLD_FONT 与 0.95 的大字号，与 0.5 的正文
            // 拉开层级；`dy!` 累计的高度里也包含了这段。
            dy!(wr.y + s * 3.);
            let r = ui
                .text(&self.title)
                .pos(wr.x + pad * 2., 0.)
                .anchor(0., 0.)
                .size(0.95)
                .max_width(wr.w - pad * 2.)
                .no_baseline()
                .draw_using(&BOLD_FONT);
            dy!(r.h + s * 2.);
            // 剩余高度 = 窗口底边 - 已用高度 - 按钮条高度 - 两侧留白。
            // 这个值就是正文可视区高度，超出部分由 Scroll 负责滚动。
            self.scroll.size((wr.w - pad * 2., wr.bottom() - h - bh - s * 2.));
            ui.dx(wr.x + pad);
            self.scroll.render(ui, |ui| {
                // 阶段四：正文。多行绘制，并把整块文本矩形注册为命中区 `text_btn`，
                // 以便区分「点正文（-2）」与「点窗口内的空白」。
                let r = ui
                    .text(&self.message)
                    .pos(pad, 0.)
                    .size(0.5)
                    .max_width(wr.w - pad * 3.)
                    .multiline()
                    .draw();
                self.text_btn.set(ui, r);
                ui.dy(r.h + 0.04);

                // 阶段五：链接行。用强调色画文字，并在文字下方补一条 0.004 粗的横线
                // 作为下划线（字体本身不带下划线样式，只能手工画）。
                // 命中区用 `feather(0.012)` 向外扩张：文字行高很小，
                // 不放大命中区会很难点中（代码注释中的 generous tap target 即指此）。
                let accent = ui.accent();
                let mut link_h = 0.;
                for ((label, _url), btn) in self.links.iter().zip(self.link_buttons.iter_mut()) {
                    let lr = ui
                        .text(label)
                        .pos(pad, 0.)
                        .anchor(0., 0.)
                        .size(0.45)
                        .max_width(wr.w - pad * 3.)
                        .color(accent)
                        .draw();
                    // underline
                    ui.fill_rect(Rect::new(lr.x, lr.bottom() + 0.005, lr.w, 0.004), accent);
                    // generous tap target
                    btn.set(ui, lr.feather(0.012));
                    let dh = lr.h + 0.03;
                    link_h += dh;
                    ui.dy(dh);
                }

                // 返回内容实测尺寸，供 Scroll 计算可滚动余量（约定见 `Scroll::render`）。
                // 注意这里返回的是正文宽度，而本对话框的 Scroll 是垂直滚动、
                // 只用到返回的高度部分。
                (r.w, r.h + 0.04 + link_h)
            });
        });
        ui.scope(|ui| {
            // 阶段六：按钮条。按钮之间以及两端各留一个 pad，因此总宽要减去
            // `buttons.len() + 1` 份 pad 再均分。
            let bw = (wr.w - pad * (self.buttons.len() + 1) as f32) / self.buttons.len() as f32;
            let mut r = Rect::new(wr.x + pad, wr.bottom() - s - bh, bw, bh);
            let n = self.buttons.len();
            for (i, (text, btn)) in self.buttons.iter().zip(self.rect_buttons.iter_mut()).enumerate() {
                // 倒计时只显示在**最后一个**按钮上（即确认键），与 `touch` 中
                // 「屏蔽 index > 0」的规则呼应；用 `ceil()` 让读数在最后一秒显示 1 而不是 0，
                // `.max(0)` 兜住倒计时结束时可能出现的 -0。
                let display_text = if self.countdown_seconds > 0 && i == n - 1 {
                    let remaining = self.countdown_seconds as f64 - (t as f64 - self.countdown_start);
                    let remaining = (remaining.max(0.0).ceil() as i32).max(0);
                    if remaining > 0 {
                        format!("{} ({})", text, remaining)
                    } else {
                        text.clone()
                    }
                } else {
                    text.clone()
                };
                // 最后一个参数 `chosen = true` 表示按钮采用强调（主操作）配色。
                btn.render_text(ui, r, t, display_text, 0.5, true);
                r.x += bw + pad;
            }
        });
    }
}
