use crate::{
    ext::{RectExt, SafeTexture, ScaleType},
    ui::Ui,
};
use macroquad::prelude::*;
use std::{
    mem::ManuallyDrop,
    rc::{Rc, Weak},
};

/// 进场/退场动画时长（秒）。
/// 两者共用同一常量：入场用三次缓出、退场用三次缓入，节奏一致才不显得突兀；
/// 0.8s 短于默认的 2s 显示时长，保证消息在“完全就位”之后还留有时间被读清。
pub const OUT_TIME: f32 = 0.8;
/// 消息条与屏幕右边/上边的留白（归一化长度），同时用作消息条内部图标与文字的间距。
pub const PADDING: f32 = 0.02;

/// 消息的语义类型，决定配色与图标。
/// `#[repr(u8)]` 的目的：让 `as u8` 得到稳定且连续的下标，
/// 从而可以直接索引宿主注入的图标数组（见 `BillBoard::render`）。
#[derive(Default, Clone)]
#[repr(u8)]
pub enum MessageKind {
    /// 普通信息（蓝色）：中性提示，不需要玩家动作。
    #[default]
    Info,
    /// 警告（橙色）：操作可能失败或结果非预期，但流程可继续。
    Warn,
    /// 成功（绿色）：操作已完成。
    Ok,
    /// 错误（红色）：操作失败，需要玩家注意。
    Error,
}

// 配色与语义类型一一对应；用 Material 风格色值保证四种状态彼此可区分。
impl MessageKind {
    /// 语义对应的主题色（全部不透明，避免与半透明遮罩叠加后变色难辨）。
    pub fn color(&self) -> Color {
        match self {
            Self::Info => Color::new(0.16, 0.71, 0.96, 1.),
            Self::Warn => Color::new(1., 0.66, 0.15, 1.),
            Self::Ok => Color::new(0.4, 0.73, 0.42, 1.),
            Self::Error => Color::new(0.96, 0.26, 0.21, 1.),
        }
    }
}

/// 单条消息的全部跨帧状态。
/// 由于 `BillBoard` 每帧原地更新消息（`retain_mut`），所有动画中间量都必须存在这里，
/// 不能放在局部变量中；`BillBoard::render` 是唯一推进这些字段的地方。
pub struct Message {
    /// 消息正文。
    content: String,
    /// 创建时刻（秒），作为入场动画与进度条的起点。
    time: f32,
    /// 过期时刻（秒，等于 `time + duration`）。可被提前改写以立即触发退场
    /// （例如句柄被丢弃时）。
    end_time: f32,
    /// 当前显示位置，单位是“行”，浮点以便插值。
    position: f32,
    /// 目标位置（第几条）。`position` 会指数逼近它，从而在消息被插入/移除时平滑让位。
    target_position: f32,
    /// 上次更新 `position` 的时间，用于计算与帧率无关的指数平滑系数。
    last_time: f32,
    /// 上一次测量出的消息条宽度（归一化），用于决定入场从多远处滑入、退场滑出多远。
    width: f32,
    /// 消息语义类型，决定颜色与图标。
    kind: MessageKind,
    /// 外部句柄的弱引用：`strong_count() == 0` 意味着调用方已丢弃 `MessageHandle`，
    /// 消息应立即退场（见 `BillBoard::render`）。
    handle: Weak<()>,
}

// 构造需要同时产出消息与句柄，因此用关联函数而非 `Default`。
impl Message {
    /// 创建消息与配对的 `MessageHandle`。
    /// `Rc::new(())` 只是“句柄是否仍存在”的标记，真正的生命周期由强引用计数表达：
    /// 消息侧只持有 `Weak`，因此句柄一旦被显式 `cancel`（强引用归零），消息即可察觉。
    /// `time` 取当前时间、`duration` 决定 `end_time`；初始位置为 0，由 `BillBoard::add` 随后改写。
    pub fn new(content: String, time: f32, duration: f32, kind: MessageKind) -> (Self, MessageHandle) {
        let rc = Rc::new(());
        let handle = Rc::downgrade(&rc);
        (
            Self {
                content,
                time,
                end_time: time + duration,
                position: 0.,
                target_position: 0.,
                last_time: time,
                width: 0.,
                kind,
                handle,
            },
            MessageHandle(Some(ManuallyDrop::new(rc))),
        )
    }
}

/// 消息的取消句柄：只有显式调用 `cancel()` 才会让消息提前退场。
/// 内部持有唯一的强引用 `Rc<()>`，消息侧只留 `Weak`，于是 `strong_count()`
/// 就成了“句柄是否仍被持有”的判定依据（见 `BillBoard::render`）。
///
/// 用 `ManuallyDrop` 包裹的意图是：**直接 drop 句柄不会取消消息**。
/// 该句柄通常作为临时变量出现（`show_message(..).handle()` 之后往往不再使用），
/// 若 drop 即取消，就等于“随手接住的返回值会把消息立刻撤掉”，与直觉不符。
/// 代价是忘记 `cancel()` 时这一个 `Rc<()>` 会泄漏，但泄漏量恒定且极小。
pub struct MessageHandle(Option<ManuallyDrop<Rc<()>>>);
// 幂等：`cancel` 内部先 `take`，因此重复调用与 drop 后的再调用都是空操作。
impl MessageHandle {
    /// 取消消息，使其立即开始退场（`BillBoard::render` 会把它的 `end_time` 提前到当前帧）。
    pub fn cancel(&mut self) {
        if let Some(rc) = self.0.take() {
            ManuallyDrop::into_inner(rc);
        }
    }
}

/// 右上角消息提示条（Toast）的管理面板：按加入顺序堆叠显示若干条 [`Message`]。
/// 每帧由场景调用 `render`，由它统一负责插入、让位、超时与退场，外部只需 `add` 即可，
/// 因此调用方不需要（也无法）管理每条消息的时序。
pub struct BillBoard {
    /// 活跃消息，按加入顺序排列：数组下标即默认的显示行号。
    messages: Vec<Message>,
    /// 四种类型对应的图标纹理，按 `MessageKind as u8` 的顺序排列；
    /// `None` 表示宿主尚未注入，此时只绘制色块（消息仍可用）。
    icons: Option<[SafeTexture; 4]>,
}

// 默认即空面板，图标需另行注入。
impl Default for BillBoard {
    fn default() -> Self {
        Self::new()
    }
}

// 面板的生命周期管理：添加消息、注入资源、逐帧渲染。
impl BillBoard {
    /// 创建空面板（未注入图标）。
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            icons: None,
        }
    }

    /// 注入按 `MessageKind` 顺序排列的 4 个图标。
    /// 之所以由外部注入而不在此加载：纹理加载涉及资源路径与错误处理，
    /// 而 UI 库不应强制某种资源包布局；同时也让宿主可以按平台替换图标。
    pub fn set_icons(&mut self, icons: [SafeTexture; 4]) {
        self.icons = Some(icons);
    }

    /// 追加一条消息。
    /// 新消息的起始位置直接设为当前条数（排在最下方），随即成为它的动画起点——
    /// 于是它在入场时会从下方“滑”到自己的位置；实际插值在 `render` 中完成。
    pub fn add(&mut self, mut msg: Message) {
        msg.position = self.messages.len() as f32;
        msg.target_position = msg.position;
        self.messages.push(msg);
    }

    /// 绘制所有消息并推进各自的动画。
    ///
    /// 动画模型：每条消息有两条独立的时间轴——
    /// - `position`（行号）以 0.1s 时间常数指数逼近 `target_position`，实现插入/移除时的平滑让位；
    /// - `width`（已进入屏幕的宽度）在入场时从整屏宽度收拢、退场时向左滑出，配合三次缓和曲线。
    ///
    /// 布局以右上角为基准：`rt` 是消息条右边界，`tp` 是第一条消息的顶边，
    /// 每条占据 `rh = h + 0.02` 的高度。
    pub fn render(&mut self, ui: &mut Ui, t: f32) {
        // 基准布局：`rt` 是消息条右边界、`tp` 是第一条消息的顶边；
        // `h` 为消息条高、`pd` 为横向内边距、`rh` 为相邻两条的行高（比条高多 0.02 作为间隙）。
        let rt = 1. - PADDING;
        let tp = -ui.top + PADDING;
        let h = 0.1;
        let pd = 0.014;
        let rh = h + 0.02;
        let mut pos = 0;
        // 用 `retain_mut` 一次遍历同时完成“推进动画”与“剔除已退场的消息”：
        // 闭包返回 false 即从列表中移除，省去额外的过滤遍历与中间集合。
        self.messages.retain_mut(|msg| {
            // 句柄已被丢弃（强引用归零）而消息还没过期：把过期时间提前到当前帧，
            // 让它走与自然过期完全相同的退场动画，而不是被生硬地删掉。
            if msg.end_time > t && msg.handle.strong_count() == 0 {
                msg.end_time = t;
            }
            // 计算本帧消息条的右边界 `rt`，分两支：已过期走退场、否则走入场/停留。
            // 用局部同名变量遮蔽外层的 `rt`，是为了一眼看出“本条消息的右边界”。
            let rt = if t >= msg.end_time {
                // 退场：进度超过 1 即视为完全滑出，直接剔除（这是唯一的移除时机）。
                let p = (t - msg.end_time) / OUT_TIME;
                if p > 1. {
                    return false;
                }
                // 三次缓出：开始快、结束慢，滑出更自然；`msg.width` 是滑出距离，
                // 另有 0.2 的余量已计入（见下方 `msg.width` 的赋值），保证完全离开屏幕。
                let p = 1. - (1. - p).powi(3);
                rt + msg.width * p
            } else {
                // 仍存活：占用一个行号，后续消息依次下移。
                msg.target_position = pos as f32;
                pos += 1;
                if msg.width == 0. {
                    // 首帧尚未测量出宽度：先假定“整屏宽度”作为起点，
                    // 这样第一次出现时必定从屏幕右外侧滑入，不会出现闪现。
                    3.
                } else {
                    // 入场：三次缓出同样用于缩短滑入距离，`(1-p)^3` 使它在接近终点时减速。
                    let p = ((t - msg.time) / OUT_TIME).min(1.);
                    let p = (1. - p).powi(3);
                    rt + msg.width * p
                }
            };
            // 位置平滑：以 0.1s 为“半衰期”做指数插值，系数由帧间隔推导，因此与帧率无关。
            // 用当前时间与上次更新时间的差值作指数，即使帧率抖动也不会改变收敛速度。
            let p = (0.5_f32).powf((t - msg.last_time) / 0.1);
            msg.position = msg.position * p + msg.target_position * (1. - p);
            msg.last_time = t;
            // 本条消息的顶边 = 首行顶边 + 行号 × 行高，`position` 是插值后的行号。
            let tp = tp + msg.position * rh;
            // 只测量不绘制：先拿到文字尺寸才能反推消息条尺寸；
            // 文字右对齐（`anchor(1., ..)`）并垂直居中，`max_width` 限制宽度使超长消息
            // 被省略号截断，而不是横跨整个屏幕。
            let mut tx = ui
                .text(&msg.content)
                .pos(rt - pd, tp + h / 2.)
                .anchor(1., 0.5)
                .no_baseline()
                .size(0.64)
                .max_width(0.8);
            let r = tx.measure();
            // 把文字矩形向左右扩展成消息条：左侧多留 `h + pd` 给图标区与内边距。
            let mut r = Rect::new(r.x - pd - h, tp, r.w + pd * 2. + h, h);
            // 记录宽度用于滑入/滑出距离；额外加 0.2 是余量，
            // 保证退场进度到 1 之前消息条已完全滑出屏幕右侧。
            msg.width = r.w + 0.2;
            // 消息条底色取自语义类型，玩家一眼即可区分信息/警告/成功/错误。
            tx.ui.fill_rect(r, msg.kind.color());
            // 进度条：贴在底部的细白条，长度按剩余时间比例缩短，
            // 让玩家直观知道还有多久消失；退场后剩余时间无意义，故仅未过期时绘制。
            if t < msg.end_time {
                tx.ui.fill_rect(
                    Rect::new(r.x, r.bottom() - 0.01, r.w * (1. - (t - msg.time) / (msg.end_time - msg.time)), 0.01),
                    Color::new(1., 1., 1., 0.3),
                );
            }
            // 左侧图标区：先把矩形收窄成一个正方形（宽 = 高），铺半透明白底作为图标衬底。
            r.w = h;
            tx.ui.fill_rect(r, Color::new(1., 1., 1., 0.4));
            // 图标按 `kind as u8` 索引；`feather(-0.02)` 向内收缩留出衬底边距，
            // 用 `Fit` 缩放保证图标完整可见（不裁剪）。未注入图标时只留上面的白色方块。
            if let Some(icons) = self.icons.as_ref() {
                let r = r.feather(-0.02);
                tx.ui.fill_rect(r, (*icons[msg.kind.clone() as u8 as usize], r, ScaleType::Fit));
            }
            // 测量完成后才真正绘制文字，位置基于最终确定的消息条布局。
            tx.draw();
            true
        });
    }
}
