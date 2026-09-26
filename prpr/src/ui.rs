//! UI utilities.
//!
//! 本模块是 prpr 的即时模式（immediate-mode）UI 组件库主体：控件不保留持久的对象树，
//! 每帧由场景调用时立即绘制并处理触摸；唯一的跨帧控件状态是 thread_local 的 `STATE`，
//! 它只回答“某个控件当前正被哪个手指按住”这一个问题。
//!
//! 坐标系约定：UI 一律使用**归一化坐标**，`x ∈ [-1, 1]`，`y` 从屏幕顶部的 `-top`
//! 到底部的 `+top`（`top = 视口高 / 视口宽`，见 `Ui::new`）。由于 y 轴已经朝下，
//! 换算到 GL 裁剪空间时无需再翻转。因此 `Gravity` 表达的是“内容相对容器贴在哪个锚点”，
//! 而不是字面意义上的物理重力方向。
//!
//! 绘制分为两条路径，最终都通过 `quad_gl.geometry` 提交 draw call：
//! - 矩形/圆/路径等图元先经 lyon 镶嵌成三角形，再由 `Shading` 为每个顶点着色（见 `VertexBuilder`）；
//! - 文字由自维护字形图集的 `TextPainter` 逐字形提交 quad（见 `text` 子模块）。
prpr_l10n::tl_file!("scene" ttl);
// 消息提示条（右上角 Toast）。
mod billboard;
pub use billboard::{BillBoard, Message, MessageHandle, MessageKind};

// 谱面信息卡片。
mod chart_info;
pub use chart_info::*;

// 对话框（错误/确认弹窗）。
mod dialog;
pub use dialog::Dialog;

// 滚动容器：把超出可视区的绘制重定向到独立视口。
mod scroll;
// `InputBox`/`InputMode` 是“请求宿主弹出输入框”所需的描述类型，UI 层只负责请求与接收文本。
use inputbox::{InputBox, InputMode};
pub use scroll::*;

// 判定偏移分析图表。
mod offset_analysis;
pub use offset_analysis::*;

// 顶点着色规则（纯色/渐变/纹理等），决定 `VertexBuilder` 怎样为每个顶点生成颜色。
mod shading;
pub use shading::*;

// 圆角矩形阴影、圆角裁剪等基于自定义 shader 的效果。
mod shadow;
pub use shadow::*;

// 文字绘制与字形图集。
mod text;
pub use text::{DrawText, TextPainter};

// 重新导出字体类型，使调用方无需直接依赖 glyph_brush。
pub use glyph_brush::ab_glyph::FontArc;

use crate::{
    core::{Matrix, Point, Vector},
    ext::{get_viewport, nalgebra_to_glm, semi_black, semi_white, source_of_image, RectExt, SafeTexture, ScaleType},
    judge::Judge,
    scene::{request_input, return_input, show_error, take_input},
};
use core::f32;
use lyon::{
    lyon_tessellation::{
        BuffersBuilder, FillOptions, FillTessellator, FillVertex, FillVertexConstructor, StrokeOptions, StrokeTessellator, StrokeVertex,
        StrokeVertexConstructor, VertexBuffers,
    },
    math as lm,
    path::{LineCap, Path, PathEvent},
};
use macroquad::prelude::*;
use miniquad::PassAction;
use sasa::{AudioManager, PlaySfxParams, Sfx};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    ops::{Deref, DerefMut, Range},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

/// 无障碍开关：为 `true` 时关闭按压缩放、消息滑入等装饰性动效，
/// 但保留承载信息的动画（loading 转圈、进度条）。由宿主（phira）从设置写入，UI 各处只读。
/// 用 `Relaxed` 顺序即可：它只影响观感，不存在需要与其它数据一起观察的依赖关系。
/// 读取方：`DRectButton::build`/`progress`（跳过缩放动画）。
pub static PREFER_REDUCED_MOTION: AtomicBool = AtomicBool::new(false);
/// UI 音效（按钮点击等）的全局音量。
/// 以 `f32` 的**位模式**存入 `AtomicU32`，因为 std 没有稳定的 `AtomicF32`；
/// 读写两侧必须严格配对使用 `f32::to_bits`/`f32::from_bits`（见 `button_hit` 等）。
/// 读取方：`button_hit`/`button_hit_large`/`list_switch`；默认 1.0 表示不衰减。
pub static UI_SFX_VOLUME: AtomicU32 = AtomicU32::new(1.0f32.to_bits());

/// 二维锚点标记，用一个 u8 位掩码同时编码水平与垂直分量（内部字段即该掩码）。
/// 水平分量占低 2 位、垂直分量占第 2~3 位（读取时 `>> 2 & 3`）。
/// 水平方向之所以从 0 开始（`LEFT = 0`），是为了让 `value()` 可以把 0/1/2
/// 直接当作 0/0.5/1 的百分比系数使用，无需再做减法偏移。
#[derive(Default, Clone, Copy)]
pub struct Gravity(u8);

// 常量按位定义：`TOP`/`BOTTOM` 复用水平方向的数值空间，因此必须按位或组合。
impl Gravity {
    /// 水平：靠左（系数 0）。
    pub const LEFT: u8 = 0;
    /// 水平：居中（系数 0.5）。
    pub const HCENTER: u8 = 1;
    /// 水平：靠右（系数 1）。
    pub const RIGHT: u8 = 2;
    /// 垂直：靠上（系数 0）。
    pub const TOP: u8 = 0;
    /// 垂直：居中（系数 0.5）。
    pub const VCENTER: u8 = 4;
    /// 垂直：靠下（系数 1）。
    pub const BOTTOM: u8 = 8;

    /// 左上角，等价于水平/垂直默认值（0），用作 `Default` 的语义名。
    pub const BEGIN: u8 = Self::LEFT | Self::TOP;
    /// 正中心，最常用的居中锚点。
    pub const CENTER: u8 = Self::HCENTER | Self::VCENTER;
    /// 右下角。
    pub const END: u8 = Self::RIGHT | Self::BOTTOM;

    /// 把分量（0/1/2）映射为 0/0.5/1 的百分比系数。
    /// 越界时 `unreachable!()`：掩码只有 2 位，出现 3 说明调用方绕过了位掩码约定。
    fn value(mode: u8) -> f32 {
        match mode {
            0 => 0.,
            1 => 0.5,
            2 => 1.,
            _ => unreachable!(),
        }
    }

    /// 求内容相对容器的对齐偏移：把 `total - content` 的剩余空间按锚点比例分配。
    /// 例如 `CENTER` 返回居中所需偏移，`END` 返回内容贴住右下角所需偏移。
    /// 参数与返回值都以**归一化长度**为单位，因此与分辨率无关。
    pub fn offset(&self, total: (f32, f32), content: (f32, f32)) -> (f32, f32) {
        (Self::value(self.0 & 3) * (total.0 - content.0), Self::value((self.0 >> 2) & 3) * (total.1 - content.1))
    }

    /// `offset` 的逆运算：已知容器内某点的绝对坐标，反推内容的起始位置。
    /// 供“按锚点摆放内容”的布局代码使用。
    pub fn from_point(&self, point: (f32, f32), content: (f32, f32)) -> (f32, f32) {
        (point.0 - content.0 * Self::value(self.0 & 3), point.1 - content.1 * Self::value((self.0 >> 2) & 3))
    }
}

// 允许把裸位掩码直接当作锚点使用（如 `0.into()` 表示左上角），便于与 `Gravity::CENTER` 等常量混用。
impl From<u8> for Gravity {
    /// 以位掩码构造，不做合法性校验（越界值会在 `value()` 处暴露）。
    fn from(val: u8) -> Self {
        Self(val)
    }
}

/// 把 lyon 镶嵌产出的顶点按 `Shading` 规则转成 macroquad `Vertex` 的构造器。
/// 三个（元组）字段依次是：
/// - `Matrix`：把局部坐标投到归一化坐标的逻辑变换（取自 `Ui::transform`）；
/// - `T`：着色器本体，决定顶点的颜色/纹理；
/// - `f32`：整体 alpha 乘子（来自 `Ui::alpha` 的嵌套累乘）。
struct ShadedConstructor<T: Shading>(Matrix, pub T, f32);
// lyon 的填充顶点构造器桥接：位置保持归一化坐标，具体颜色/Uv 由 `Shading` 决定。
impl<T: Shading> FillVertexConstructor<Vertex> for ShadedConstructor<T> {
    fn new_vertex(&mut self, vertex: FillVertex) -> Vertex {
        let pos = vertex.position();
        self.1.new_vertex(&self.0, &Point::new(pos.x, pos.y), self.2)
    }
}
// 描边复用与填充完全相同的顶点生成逻辑，保证同一路径的描边与填充颜色/变换严格一致。
impl<T: Shading> StrokeVertexConstructor<Vertex> for ShadedConstructor<T> {
    fn new_vertex(&mut self, vertex: StrokeVertex) -> Vertex {
        let pos = vertex.position();
        self.1.new_vertex(&self.0, &Point::new(pos.x, pos.y), self.2)
    }
}

/// 手工构造三角形顶点并直接提交给 GL。
/// 与 lyon 路径的分工：矩形等平凡图元的三角化不需要镶嵌，走这里可以省掉
/// tessellator 的开销与容差计算；索引使用 `u16`，因此单次构建的顶点数上限为 65535
/// （UI 图元远小于该量级）。
pub struct VertexBuilder<T: Shading> {
    /// 顶点从局部坐标到归一化坐标的变换，创建时从 `Ui::transform` 快照而来
    /// （快照而非引用，是为了让 `commit` 与 `Ui` 的可变借用解耦）。
    matrix: Matrix,
    /// 已累积的顶点。
    vertices: Vec<Vertex>,
    /// 顶点索引，每 3 个元素构成一个三角形。
    indices: Vec<u16>,
    /// 顶点着色规则（纯色/渐变/纹理等）。
    shading: T,
    /// 全局透明度乘子，来自 `Ui::alpha`。
    alpha: f32,
}

// 仅由 `Ui::builder` 创建，保证 matrix/alpha 与当前 UI 状态一致。
impl<T: Shading> VertexBuilder<T> {
    /// 创建空缓冲，容量留待 push 时自然增长（单次绘制的顶点数很小）。
    fn new(matrix: Matrix, shading: T, alpha: f32) -> Self {
        Self {
            matrix,
            vertices: Vec::new(),
            indices: Vec::new(),
            shading,
            alpha,
        }
    }

    /// 追加一个**局部坐标**顶点；位置变换与着色都在此完成，
    /// 因此调用方传入的坐标无需关心当前 `Ui::transform`。
    pub fn add(&mut self, x: f32, y: f32) {
        self.vertices.push(self.shading.new_vertex(&self.matrix, &Point::new(x, y), self.alpha));
    }

    /// 追加一个三角形，参数是 3 个顶点在 `vertices` 中的下标（顺序决定正面朝向，
    /// 默认关闭面剔除，因此逆序也不会被丢弃）。
    pub fn triangle(&mut self, x: u16, y: u16, z: u16) {
        self.indices.push(x);
        self.indices.push(y);
        self.indices.push(z);
    }

    /// 把缓冲提交给 `quad_gl`，即一次独立的 draw call（这也是本项目的主要提交点之一）。
    /// 先设置纹理与图元类型再提交：`quad_gl` 会把这批顶点并入当前批次，
    /// 因此**调用方不应依赖提交后立即执行**；同时要注意 `unsafe` 的内部 GL 访问是
    /// 安全的，因为 UI 只在持有 GL 上下文的主线程上运行。
    pub fn commit(&self) {
        // SAFETY: UI 的绘制只可能发生在持有 GL 上下文的主线程/渲染线程内，
        // 且此刻没有其它借用 `gl` 的引用存在（`commit` 只读 `self` 的缓冲）。
        let gl = unsafe { get_internal_gl() }.quad_gl;
        gl.texture(self.shading.texture());
        gl.draw_mode(DrawMode::Triangles);
        gl.geometry(&self.vertices, &self.indices);
    }
}

/// 长按手势的跨帧状态：记录按下位置与起始时间。
/// 之所以与 `RectButton` 分离：`RectButton` 只保存“哪个手指按住了我”，
/// 而长按计时需要由调用方在帧间持有（一个 `LongTouchState` 可被多个按钮复用）。
#[derive(Default)]
pub struct LongTouchState {
    /// `Some((起始位置, 起始时间))` 表示长按计时进行中；`None` 表示未在计时。
    start: Option<(Vec2, f32)>,
}
// 复位即清空计时，用于手指移动超过阈值、抬起或被取消时中断长按判定。
impl LongTouchState {
    /// 中断当前长按计时。调用后需要重新满足长按条件才会再次触发。
    pub fn reset(&mut self) {
        self.start = None;
    }
}

/// 命中四边形的按钮状态机：只回答“是否被按住 / 是否完成点击”，**不含任何绘制**。
/// 之所以把命中区域存成 4 个顶点而非轴对齐矩形，是为了支持被 `transform`
/// 旋转或斜切后的按钮（例如沿圆弧排布的按钮）。
#[derive(Clone, Copy)]
pub struct RectButton {
    /// 命中四边形的 4 个顶点，按顺时针/逆时针依次为左上、右上、右下、左下，
    /// 且已投影到**全局归一化坐标**（见 `set`）。`None` 表示本帧尚未调用 `set`，
    /// 此时 `contains` 恒为 `false`，避免使用上一帧的过期区域误判。
    pts: Option<[Vec2; 4]>,
    /// 当前按住本按钮的触摸 id；`None` 表示未被按住。
    /// 只记 id 而不记时间，因此需要长按/拖动时由调用方另持状态。
    id: Option<u64>,
}

// 默认即“未绑定任何区域”的空按钮。
impl Default for RectButton {
    fn default() -> Self {
        Self::new()
    }
}

// 交互状态机。所有方法都是纯逻辑，可在没有 UI 上下文的地方测试。
impl RectButton {
    /// 创建未绑定的按钮（`pts`/`id` 均为空），需要先 `set` 才能命中。
    pub fn new() -> Self {
        Self { pts: None, id: None }
    }

    /// 是否有手指正按住本按钮，用于绘制按下态。
    pub fn touching(&self) -> bool {
        self.id.is_some()
    }

    /// 放弃当前按住状态但不触发点击（例如父容器被滚动/隐藏时）。
    pub fn cancel(&mut self) {
        self.id = None;
    }

    /// 判断点是否落在命中四边形内部。
    /// 实现用相邻边的二维叉积（`perp_dot`）检查四条边是否同号：凸四边形内/外的点
    /// 在同一条有向边上的叉积符号恒定，因此“四条边同号”即“点在四边形内”。
    /// 相比轴对齐 AABB 判定，它既精确又天然支持被旋转/缩放的按钮；
    /// 允许全正或全负则兼容两种顶点环绕方向，避免依赖调用方的顶点顺序。
    pub fn contains(&self, pos: Vec2) -> bool {
        if let Some([a, b, c, d]) = self.pts {
            let abp = (b - a).perp_dot(pos - a);
            let bcp = (c - b).perp_dot(pos - b);
            let cdp = (d - c).perp_dot(pos - c);
            let dap = (a - d).perp_dot(pos - d);
            (abp >= 0. && bcp >= 0. && cdp >= 0. && dap >= 0.) || (abp <= 0. && bcp <= 0. && cdp <= 0. && dap <= 0.)
        } else {
            false
        }
    }

    /// 用当前 UI 状态把**局部**矩形投影成全局命中四边形。
    ///
    /// 关键点在于必须同时乘上 `ui.transform` 与 `ui.gl_transform`：
    /// 触摸坐标是屏幕空间的，而实际顶点会依次经过“逻辑变换”和“GL 模型矩阵”，
    /// 只乘其中一个就会在 `with_gl`（把 UI 渲染到纹理/整体位移）时出现命中偏移。
    /// 末尾的 `pos.xy() / pos.w` 是透视除法，使非仿射（含透视）投影也能正确命中；
    /// 对常规仿射矩阵而言 `w` 恒为 1，属零成本兼容。
    pub fn set(&mut self, ui: &mut Ui, rect: Rect) {
        let mat = nalgebra_to_glm(&ui.transform) * ui.gl_transform;
        let tr = |x: f32, y: f32| {
            let pos = mat * vec4(x, y, 0., 1.);
            pos.xy() / pos.w
        };
        self.pts = Some([
            tr(rect.x, rect.y),
            tr(rect.right(), rect.y),
            tr(rect.right(), rect.bottom()),
            tr(rect.x, rect.bottom()),
        ]);
    }

    /// 处理一次触摸事件，返回本次是否构成“完成点击”。
    /// 判定语义（比“按下即响应”更严格，可避免误触与滑动手势冲突）：
    /// - `Started`：落在区域内才捕获该手指（后到的手指抢占会替换 id）；
    /// - `Moved`/`Stationary`：已捕获的手指移出区域即取消，移回也不会自动恢复；
    /// - `Cancelled`：直接释放；
    /// - `Ended`：id 匹配**且仍位于区域内**才算点击成功——因此“按下后拖开再松手”不会触发。
    pub fn touch(&mut self, touch: &Touch) -> bool {
        let inside = self.contains(touch.position);
        match touch.phase {
            TouchPhase::Started => {
                if inside {
                    self.id = Some(touch.id);
                }
            }
            TouchPhase::Moved | TouchPhase::Stationary => {
                if self.id == Some(touch.id) && !inside {
                    self.id = None;
                }
            }
            TouchPhase::Cancelled => {
                self.id = None;
            }
            TouchPhase::Ended => {
                if self.id.take() == Some(touch.id) && inside {
                    return true;
                }
            }
        }
        false
    }

    /// 长按检测：手指按住不动达到 0.5 秒且仍在按钮内时返回 `true`（每次按住只触发一次）。
    /// 阈值取 0.5s 是因为它短于常见的“整理思路”停顿、又明显长于点击的抖动时间；
    /// 位移超过 0.02 即认为用户想滑动而非长按，会中止计时。
    /// 触发时**不**释放 `id`，因此长按后抬起仍可能再走一次 `touch` 的点击分支，
    /// 调用方需要自行用一个“已长按”标志来决定是否吞掉随后的点击。
    pub fn long_touch(&mut self, touch: &Touch, t: f32, state: &mut LongTouchState) -> bool {
        match touch.phase {
            TouchPhase::Started => {
                if self.id == Some(touch.id) {
                    state.start = Some((touch.position, t));
                }
            }
            TouchPhase::Moved | TouchPhase::Stationary => {
                if self.id == Some(touch.id) {
                    if let Some((start_pos, start_time)) = state.start {
                        if (touch.position - start_pos).length() > 0.02 {
                            state.reset();
                        } else if t > start_time + 0.5 {
                            state.reset();
                            return true;
                        }
                    }
                }
            }
            TouchPhase::Cancelled => {
                if self.id == Some(touch.id) {
                    state.reset();
                }
            }
            TouchPhase::Ended => {
                if self.id.take() == Some(touch.id) {
                    state.reset();
                }
            }
        }
        false
    }

    /// 无触摸事件时的长按轮询。
    /// 必要性：手指完全静止时系统不会持续产生 `Moved` 事件，若只依赖 `long_touch`
    /// 就会漏掉“按住不动”这一最常见的长按形态；调用方需每帧调用本方法推进计时。
    /// 与 `long_touch` 不同，这里不检查位置（静止即视为合法）。
    pub fn update_long_touch(&self, t: f32, state: &mut LongTouchState) -> bool {
        if self.id.is_some() {
            if let Some((_, start_time)) = state.start {
                if t > start_time + 0.5 {
                    state.reset();
                    return true;
                }
            }
        }
        false
    }
}

/// “Drawable Rect Button”：在 [`RectButton`] 的命中逻辑之上加入按压缩放/阴影动画的按钮。
/// 分工：`RectButton` 只做交互判定（不绘制），`DRectButton` 负责把内容画进“按压缩小后”
/// 的坐标系，并内置若干常用按钮外观（文字居中/左对齐/输入框样式）。
/// 这两个名字与命名习惯不一致，但历史上长期如此，改动会波及大量调用点。
#[derive(Clone)]
pub struct DRectButton {
    /// 底层命中状态机，可直接访问以便复用其判定结果。
    pub inner: RectButton,
    /// 上一帧是否被按住。用于检测按下/抬起的**边沿**并据此重置动画起点，
    /// 因此它必须在每帧的 `touch` 之后保持最新。
    last_touching: bool,
    /// 动画起点时间；`None` 表示已停在终态（`progress()` 返回 1）。
    start_time: Option<f32>,
    /// 阴影参数（圆角半径/高度/基色），可通过 `with_*` 方法定制。
    pub config: ShadowConfig,
    /// 额外的收缩量。当前不参与渲染（`build` 中相关计算已被注释），
    /// 仅由 `with_delta` 记录，供将来恢复“按下时略微缩小命中区”的行为。
    delta: f32,
    /// 点击成功时是否播放按键音，`no_sound()` 可关闭。
    play_sound: bool,
}
// 默认按钮等价于 `new()`（默认阴影 + 播放音效）。
impl Default for DRectButton {
    fn default() -> Self {
        Self::new()
    }
}
// 动画与绘制。所有 `render_*` 方法都通过 `build` 间接更新命中区域，
// 因此“先 build/render 再 touch”是调用约定。
impl DRectButton {
    /// 按下/抬起的过渡时长（秒）。
    /// 0.2s 的取法：足够短以不产生迟滞感，又长到能被人眼捕捉到“弹性”；
    /// 更短会显得生硬、更长会让连续点击时动画互相追赶。
    pub const TIME: f32 = 0.2;

    /// 创建带默认阴影与音效的按钮。
    pub fn new() -> Self {
        Self {
            inner: RectButton::new(),
            last_touching: false,
            start_time: None,
            config: ShadowConfig::default(),
            delta: -0.006,
            play_sound: true,
        }
    }

    /// 在“按压缩小后”的坐标系中执行 `f` 绘制内容，并同步刷新命中区域。
    ///
    /// 缩放模型：未按下/已松开时 `progress` 为 1（缩放因子 1），按下时趋近 0
    /// （缩放因子 `1 - 0.04`，即最多收缩 4%）。取 4% 是为了让按压反馈“看得见但不夸张”，
    /// 再大就会让文字与相邻元素显得在跳动。
    /// 缩放围绕矩形中心进行（先平移到原点、缩放、再平移回去），保证按钮中心不动，
    /// 从而与固定位置的文字/图标保持对齐。
    /// 注意命中区域用的是**未缩放**的 `r`（`set` 在此函数开头调用），
    /// 因此手指不必精确落在缩小后的图形上也能触发，触摸容错更好。
    /// 开启 `PREFER_REDUCED_MOTION` 时直接按原尺寸绘制，跳过节流动画。
    pub fn build(&mut self, ui: &mut Ui, t: f32, r: Rect, f: impl FnOnce(&mut Ui, Path)) {
        self.inner.set(ui, r);
        // let r = r.feather((1. - self.progress(t)) * self.delta);
        let ct = r.center();
        let ct = Vector::new(ct.x, ct.y);
        if PREFER_REDUCED_MOTION.load(Ordering::Relaxed) {
            f(ui, r.rounded(self.config.radius));
            return;
        }
        ui.with(
            Matrix::new_translation(&-ct)
                .append_scaling(1. - (1. - self.progress(t)) * 0.04)
                .append_translation(&ct),
            |ui| {
                f(ui, r.rounded(self.config.radius));
            },
        );
    }

    /// 清空缓存的命中区域，强制下一次 `build` 用当前变换重新投影。
    /// 用于布局在 `build` 之后被外部改写、或控件被移动到另一个容器（变换不同）的情形。
    pub fn invalidate(&mut self) {
        self.inner.pts = None;
    }

    /// 绘制“内容 + 随按压变化的圆角阴影”。
    /// 阴影的 elevation 与下移量都乘以 `progress`：按下时阴影收缩并上移，
    /// 模拟按钮贴近背景（“被按下去”）的视觉，抬起时阴影重新铺开。
    /// 内容本身由 `f` 绘制，且与阴影共享 `build` 的缩放坐标系，保证两者一起动。
    pub fn render_shadow(&mut self, ui: &mut Ui, r: Rect, t: f32, f: impl FnOnce(&mut Ui, Path)) {
        let p = self.progress(t);
        let config = ShadowConfig {
            elevation: self.config.elevation * p,
            radius: self.config.radius,
            ..self.config
        };
        ui.scope(|ui| {
            ui.dy((1. - p) * 0.004);
            self.build(ui, t, r, |ui, path| {
                rounded_rect_shadow(ui, r, &config);
                f(ui, path);
            });
        });
    }

    /// 以按钮样式绘制**居中**文字：选中态为白底深色字，默认态为半透明黑底白字。
    /// `size` 是逻辑字号，`chosen` 只影响配色、不影响布局。
    /// 关于 `oh`：`(1 - r.h / oh)^1.3` 是历史上为“按下时按比例缩小字号”预留的反比补偿
    /// （`build` 早期会把缩小后的矩形交给这里）。现在缩放改由变换矩阵承担、`r` 始终是原矩形，
    /// 因此 `r.h == oh`、该系数恒为 1（不生效）；保留原样以免改变既有观感。
    pub fn render_text<'a>(&mut self, ui: &mut Ui, r: Rect, t: f32, text: impl Into<Cow<'a, str>>, size: f32, chosen: bool) {
        let oh = r.h;
        self.build(ui, t, r, |ui, path| {
            let ct = r.center();
            ui.fill_path(&path, if chosen { WHITE } else { semi_black(0.4) });
            ui.text(text)
                .pos(ct.x, ct.y)
                .anchor(0.5, 0.5)
                .no_baseline()
                .size(size * (1. - (1. - r.h / oh).powf(1.3)))
                .max_width(r.w)
                .color(if chosen { Color::new(0.3, 0.3, 0.3, 1.) } else { WHITE })
                .draw();
        });
    }

    /// 与 `render_text` 完全一致，只是文字颜色由调用方给出，
    /// 用于按状态着色（例如危险操作用红色、禁用态用灰色）。
    /// 参数较多，显式允许 clippy 的 `too_many_arguments`。
    #[allow(clippy::too_many_arguments)]
    pub fn render_text_color<'a>(&mut self, ui: &mut Ui, r: Rect, t: f32, text: impl Into<Cow<'a, str>>, size: f32, chosen: bool, color: Color) {
        let oh = r.h;
        self.build(ui, t, r, |ui, path| {
            let ct = r.center();
            ui.fill_path(&path, if chosen { WHITE } else { semi_black(0.4) });
            ui.text(text)
                .pos(ct.x, ct.y)
                .anchor(0.5, 0.5)
                .no_baseline()
                .size(size * (1. - (1. - r.h / oh).powf(1.3)))
                .max_width(r.w)
                .color(color)
                .draw();
        });
    }

    /// 以**左对齐**方式绘制文字，并接收额外 `alpha`（用于占位提示、淡入淡出等）。
    /// 左侧内缩 0.02、最大宽度为按钮宽减 0.04，保证文字与按钮边缘留出对称留白；
    /// `size * r.h / oh` 与 `render_text` 中的补偿同源（`r.h == oh`，当前恒等于 `size`）。
    #[allow(clippy::too_many_arguments)]
    pub fn render_text_left<'a>(&mut self, ui: &mut Ui, r: Rect, t: f32, alpha: f32, text: impl Into<Cow<'a, str>>, size: f32, chosen: bool) {
        let oh = r.h;
        self.build(ui, t, r, |ui, path| {
            ui.fill_path(&path, if chosen { WHITE } else { semi_black(0.4) });
            ui.text(text)
                .pos(r.x + 0.02, r.center().y)
                .anchor(0., 0.5)
                .max_width(r.w - 0.04)
                .no_baseline()
                .size(size * r.h / oh)
                .color(if chosen { Color::new(0.3, 0.3, 0.3, alpha) } else { semi_white(alpha) })
                .draw();
        });
    }

    /// 绘制输入框外观：`text` 为已输入内容，`hint` 为占位提示。
    /// 用“内容是否为空（`trim`）”来决定显示哪一个，并以 0.7 alpha 区分占位提示，
    /// 这样无需额外的聚焦/空态标志即可表达“提示中”与“已输入”。
    /// `#[inline]`：只做一次分支转发，内联后可省去一次跨方法调用。
    #[inline]
    pub fn render_input<'a>(&mut self, ui: &mut Ui, r: Rect, t: f32, text: impl Into<Cow<'a, str>>, hint: impl Into<Cow<'a, str>>, size: f32) {
        let text = text.into();
        if text.trim().is_empty() {
            self.render_text_left(ui, r, t, 0.7, hint, size, false);
        } else {
            self.render_text_left(ui, r, t, 1., text, size, false);
        }
    }

    /// 关闭点击音效并返回自身，用于链式配置（如列表项按钮不应发声）。
    #[inline]
    pub fn no_sound(mut self) -> Self {
        self.play_sound = false;
        self
    }

    /// 设置圆角半径（归一化长度），返回自身以便链式配置。
    #[inline]
    pub fn with_radius(mut self, radius: f32) -> Self {
        self.config.radius = radius;
        self
    }

    /// 设置阴影高度。越大阴影越“远”、越明显；按下时会按 `progress` 收缩到 0。
    #[inline]
    pub fn with_elevation(mut self, elevation: f32) -> Self {
        self.config.elevation = elevation;
        self
    }

    /// 设置阴影基色强度（用于深/浅主题下控制阴影的可见度）。
    #[inline]
    pub fn with_base(mut self, base: f32) -> Self {
        self.config.base = base;
        self
    }

    /// 覆盖额外的收缩量 `delta`（当前不参与渲染，见字段说明）。
    #[inline]
    pub fn with_delta(mut self, delta: f32) -> Self {
        self.delta = delta;
        self
    }

    /// 计算按压动画进度并顺带推进内部计时器，返回 `0`（完全按下）到 `1`（完全弹起）之间的值。
    ///
    /// 用“当前时间 - 起点”除以 `TIME` 得到线性进度，再由 `last_touching` 决定是否取
    /// `1 - p` 来反向播放——这样只需要一个计时器就能同时表达按下与抬起，无需两套状态。
    /// 超过 `TIME` 或启用 `PREFER_REDUCED_MOTION` 时清空起点，避免数值无意义地增长。
    /// 本方法会修改内部状态，因此需要 `&mut self`。
    pub fn progress(&mut self, t: f32) -> f32 {
        if self.start_time.as_ref().is_some_and(|it| t > *it + Self::TIME) || PREFER_REDUCED_MOTION.load(Ordering::Relaxed) {
            self.start_time = None;
        }
        let p = if let Some(time) = &self.start_time {
            (t - time) / Self::TIME
        } else {
            1.
        };
        if self.last_touching {
            1. - p
        } else {
            p
        }
    }

    /// 转发触摸给底层 `RectButton` 并驱动按压动画：每当“是否被按住”发生跳变，
    /// 就把动画起点重置为当前时间，使按下与抬起都从零开始过渡——而不是接续上一段
    /// 动画的中间位置，这样快速连点时进度不会错乱。
    /// 点击成功且未关闭音效时播放按键音。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        let res = self.inner.touch(touch);
        let touching = self.inner.touching();
        if self.last_touching != touching {
            self.last_touching = touching;
            self.start_time = Some(t);
        }
        if res && self.play_sound {
            button_hit();
        }
        res
    }

    /// 长按版本：除底层判定外，还会把动画切到“刚刚弹起”状态并播放音效，
    /// 让长按成功也有可听见/可见的反馈（长按通常意味着一项破坏性或重量级操作）。
    pub fn long_touch(&mut self, touch: &Touch, t: f32, state: &mut LongTouchState) -> bool {
        if self.inner.long_touch(touch, t, state) {
            self.last_touching = false;
            self.start_time = Some(t);
            if self.play_sound {
                button_hit();
            }
            true
        } else {
            false
        }
    }

    /// 每帧轮询的长按版本，行为与 `long_touch` 一致；
    /// 之所以需要它，见 `RectButton::update_long_touch`（手指静止时没有触摸事件）。
    pub fn update_long_touch(&mut self, t: f32, state: &mut LongTouchState) -> bool {
        if self.inner.update_long_touch(t, state) {
            self.last_touching = false;
            self.start_time = Some(t);
            if self.play_sound {
                button_hit();
            }
            true
        } else {
            false
        }
    }
}

/// 水平拖动条控件的**交互状态**（不含布局，布局由 `Slider::render` 每帧重算）。
/// 值本身由调用方通过 `&mut f32` 传入传出，控件只在触摸时改写它。
/// 提供两条互补的操作路径：
/// - 两侧的 ± 步进按钮（各是一个 `DRectButton`），适合精确微调；
/// - 拖动圆形手柄，适合大范围快速调整（会吸附到 `step` 的整数倍）。
pub struct Slider {
    /// 允许的取值范围（闭区间）。
    range: Range<f32>,
    /// 步进/吸附粒度：既决定 ± 按钮每次的增量，也决定拖动后的取整。
    step: f32,

    /// 左侧“-”按钮。
    btn_dec: DRectButton,
    /// 右侧“+”按钮。
    btn_inc: DRectButton,

    /// 拖动状态：`(触摸 id, 起始 x, 是否已越过解锁阈值)`。
    /// 记录“是否已解锁”是为了避免用户误点轨道就把值改掉（见 `THRESHOLD`）。
    touch: Option<(u64, f32, bool)>,
    /// 滑轨的全局矩形，由 `render` 写入；拖动时把全局触摸坐标换算为比例就依赖它。
    rect: Rect,
    /// 手柄中心的全局 x。`render` 每帧更新，供 `touch` 判定手指是否落在手柄上；
    /// 初值为 `INFINITY` 以保证首帧不会被误命中。
    pos: f32,
}

// 构造与交互。`RADIUS`/`THRESHOLD` 是唯一的两个可调常量，刻意保持私有以免外部依赖其数值。
impl Slider {
    /// 手柄圆的半径（归一化长度）。0.028 与 `Ui::slider` 里的视觉半径保持一致的量级，
    /// 使手柄看起来明显比轨道粗、易于按中。
    const RADIUS: f32 = 0.028;
    /// 触发拖动所需的水平位移阈值。
    /// 设阈值的用意：单纯点一下轨道不应改值（否则用户想滚动页面时极易误改参数），
    /// 必须先沿水平方向移动超过该距离，才认为是明确的拖动意图。
    const THRESHOLD: f32 = 0.05;

    /// 用取值范围与步长创建滑块。
    /// `pos` 初始化为 `f32::INFINITY`，确保第一帧 `touch` 不会把手柄判定在原点处。
    /// `with_delta(-0.002)` 让 ± 按钮的按下反馈略小于默认值（它们本身很小）。
    pub fn new(range: Range<f32>, step: f32) -> Self {
        Self {
            range,
            step,

            btn_dec: DRectButton::new().with_delta(-0.002),
            btn_inc: DRectButton::new().with_delta(-0.002),

            touch: None,
            rect: Rect::default(),
            pos: f32::INFINITY,
        }
    }

    /// 处理一次触摸，可能就地改写 `dst`（由调用方传入的当前值）。
    ///
    /// 返回值是三态协议，用于多控件共享同一批触摸时确定优先级：
    /// - `Some(true)`：值已改变（调用方可据此刷新预览/发网络请求）；
    /// - `Some(false)`：本控件**吞掉**了这次触摸但值没变（例如正在拖动、或点中手柄但未越过阈值），
    ///   调用方不应再把它交给其它控件；
    /// - `None`：与本控件无关，请交给其它控件处理。
    ///
    /// 优先级顺序为 ± 按钮 > 手柄拖动，因为按钮位于滑轨之外、互不重叠。
    /// 拖动时的取整用 `round()` 而非 `floor()`：让吸附发生在最近的刻度上，避免“只能往下取”的偏置。
    pub fn touch(&mut self, touch: &Touch, t: f32, dst: &mut f32) -> Option<bool> {
        if self.btn_dec.touch(touch, t) {
            *dst = (*dst - self.step).max(self.range.start);
            return Some(true);
        }
        if self.btn_inc.touch(touch, t) {
            *dst = (*dst + self.step).min(self.range.end);
            return Some(true);
        }
        if let Some((id, start_pos, unlocked)) = &mut self.touch {
            if touch.id == *id {
                match touch.phase {
                    TouchPhase::Started | TouchPhase::Moved | TouchPhase::Stationary => {
                        if (touch.position.x - *start_pos).abs() >= Self::THRESHOLD {
                            *unlocked = true;
                        }
                        if *unlocked {
                            let p = (touch.position.x - self.rect.x) / self.rect.w;
                            let p = p.clamp(0., 1.);
                            let p = self.range.start + (self.range.end - self.range.start) * p;
                            *dst = (p / self.step).round() * self.step;
                            return Some(true);
                        }
                    }
                    TouchPhase::Cancelled | TouchPhase::Ended => {
                        self.touch = None;
                    }
                }
                return Some(false);
            }
        } else if touch.phase == TouchPhase::Started {
            let pos = (self.pos, self.rect.center().y);
            if (touch.position.x - pos.0).hypot(touch.position.y - pos.1) <= Self::RADIUS {
                self.touch = Some((touch.id, touch.position.x, false));
                return Some(false);
            }
        }
        None
    }

    /// 绘制滑轨、手柄与两侧 ± 按钮。
    ///
    /// 布局约定：以传入的 `r` 为基准向左加宽（`0.1 + 0.2*w` 让出标签空间）、
    /// 向右扩展 1.2 倍宽度，使两侧的 ± 按钮有地方摆放；`p` 是当前值在
    /// `range` 中的**原始值**（不是比例），转换成比例在此完成。
    /// 所有与触摸有关的状态都以全局坐标保存（`self.rect`/`self.pos`），
    /// 因此本方法可以处于任意嵌套变换下而不影响命中判定。
    pub fn render(&mut self, ui: &mut Ui, mut r: Rect, t: f32, p: f32, text: String) {
        // 阶段 1：扩展矩形以容纳两侧按钮，并统一计算垂直中心线 `cy`（所有元素都对齐到它）。
        r.x -= 0.1;
        r.x -= r.w * 0.2;
        r.w *= 1.2;
        let pad = 0.04;
        let size = 0.026;
        let cy = r.center().y;
        // 阶段 2：绘制左右两个 ± 按钮。用零尺寸矩形 `feather(size)` 生成边长 2*size 的正方形，
        // 比起手算边长更能保证与 `pad` 留白的关系清晰。
        self.btn_dec
            .render_text(ui, Rect::new(r.x - pad - size, cy, 0., 0.).feather(size), t, "-", 0.7, true);
        self.btn_inc
            .render_text(ui, Rect::new(r.right() + pad + size, cy, 0., 0.).feather(size), t, "+", 0.7, true);
        // 阶段 3：记录滑轨的全局矩形，供拖动的命中判定使用（必须投影到全局，因为触摸坐标是全局的）。
        self.rect = ui.rect_to_global(r);
        // 阶段 4：滑轨左侧的标签文字，右对齐到滑轨外的留白处，垂直居中。
        ui.text(text)
            .pos(r.x - (pad + size) * 2., cy)
            .anchor(1., 0.5)
            .no_baseline()
            .size(0.6)
            .draw();
        // 阶段 5：把取值映射为 0..1 的比例，得到手柄中心；同时记录全局 x 供 `touch` 判定。
        let p = (p - self.range.start) / (self.range.end - self.range.start);
        let pos = (r.x + r.w * p, cy);
        self.pos = ui.to_global(pos).0;
        // 阶段 6：画两段滑轨——已走过的部分用背景色加深、未走过的部分用半透明白，
        // 形成“填充进度”的观感。线帽临时改为 Round 让两段在交界处圆润衔接，
        // 画完必须恢复为 Square，否则会泄漏状态影响后续其它控件。
        use lyon::math::point;
        ui.stroke_options = ui.stroke_options.with_line_cap(LineCap::Round);
        ui.stroke_path(
            &{
                let mut p = Path::builder();
                p.begin(point(r.x, cy));
                p.line_to(point(pos.0, cy));
                p.end(false);
                p.build()
            },
            0.02,
            Color { a: 0.8, ..ui.background() },
        );
        ui.stroke_path(
            &{
                let mut p = Path::builder();
                p.begin(point(pos.0, cy));
                p.line_to(point(r.right(), cy));
                p.end(false);
                p.build()
            },
            0.02,
            semi_white(0.8),
        );
        ui.stroke_options = ui.stroke_options.with_line_cap(LineCap::Square);
        // 阶段 7：手柄本体——先铺一层阴影（base 提到 0.7 使小尺寸手柄也能看出立体感），
        // 再叠加白色实心圆。阴影与圆都用归一化长度，因此手柄大小与屏幕宽度成比例。
        rounded_rect_shadow(
            ui,
            Rect::new(pos.0, pos.1, 0., 0.).feather(Self::RADIUS),
            &ShadowConfig {
                radius: Self::RADIUS,
                base: 0.7,
                ..Default::default()
            },
        );
        ui.fill_circle(pos.0, pos.1, Self::RADIUS, WHITE);
    }
}

// 跨帧的控件交互状态：键是控件的稳定字符串 id（如 `input#标签`、`chkbox#文字`、`slider:-`），
// 值是该控件当前捕获的触摸 id。
// 为什么用字符串 id：控件每帧都会被重建（没有持久对象），只能靠调用方给出的稳定标识
// 跨帧记住“哪个手指按住的是我”。
// 为什么用 thread_local：UI 只在渲染线程使用，避免为一次点击加锁；代价是不同线程的
// UI 状态互相独立——这正是期望的行为。
thread_local! {
    static STATE: RefCell<HashMap<String, Option<u64>>> = RefCell::new(HashMap::new());
}

/// `Ui::input` 的可选参数集合。
/// 通过一组 `From` 实现，使这个参数可以简写为 `()`、`InputMode`、`f32`（宽度）
/// 或 `(f32, &mut bool)`（宽度 + 变更标志），兼顾简写与可读性。
pub struct InputParams<'a> {
    /// 文本被提交后置为 `true`，调用方据此决定是否保存/校验。
    pub changed: Option<&'a mut bool>,
    /// 输入模式（文本/密码/数字…），决定宿主弹出哪种输入法。
    pub mode: InputMode,
    /// 输入区宽度（归一化长度），默认 0.3。
    pub length: f32,
}

// 全部取默认值：文本模式、宽度 0.3、不关心变更事件。
impl From<()> for InputParams<'_> {
    /// `()` 简写：只指定默认值。
    fn from(_: ()) -> Self {
        Self {
            changed: None,
            mode: InputMode::Text,
            length: 0.3,
        }
    }
}

// 只指定输入模式，其余取默认。
impl From<InputMode> for InputParams<'_> {
    /// `InputMode` 简写。
    fn from(mode: InputMode) -> Self {
        Self { mode, ..().into() }
    }
}

// 只指定输入区宽度，其余取默认。
impl From<f32> for InputParams<'_> {
    /// `f32` 简写：只指定宽度。
    fn from(length: f32) -> Self {
        Self { length, ..().into() }
    }
}

// 同时指定宽度与“已提交”标志（需要观察用户确认输入的场景）。
impl<'a> From<(f32, &'a mut bool)> for InputParams<'a> {
    /// `(宽度, &mut changed)` 简写。
    fn from((length, changed): (f32, &'a mut bool)) -> Self {
        Self {
            changed: Some(changed),
            mode: InputMode::Text,
            length,
        }
    }
}

/// 一次 UI 绘制的上下文：持有当前变换、裁剪状态、可复用的镶嵌/顶点缓冲与目标文字绘制器。
/// 生命周期 `'a` 借用 `TextPainter`（通常是场景自有的绘制器，或 `core::PGR_FONT` 等）。
///
/// 设计要点：
/// - 所有绘制方法都以**当前 `transform`** 为坐标系，因此通过 `with`/`scope`/`dx` 就能做嵌套布局，
///   而不必给每个控件传递父容器偏移；
/// - 镶嵌器与缓冲区都是字段而非局部变量，目的是在一个 `Ui` 的生存期内复用它们的内部容量；
/// - 本类型不是线程安全的（借用 `TextPainter` 且操作 GL 状态），只能在渲染线程创建。
pub struct Ui<'a> {
    /// 视口高宽比 `height / width`。归一化坐标的 `y` 范围即 `[-top, top]`。
    /// 缓存成字段而不是每帧查询屏幕尺寸，既省开销也保证同一帧内所有换算使用同一组值。
    pub top: f32,
    /// 本 Ui 的绘制目标区域 `(x, y, w, h)`（像素）；同时作为 `camera()` 的 viewport，
    /// 因此可以把同一套 UI 代码渲染到屏幕的某个矩形甚至纹理上。
    pub viewport: (i32, i32, i32, i32),

    /// 文字绘制器，由 `Ui::text` 生成的 builder 使用。
    pub text_painter: &'a mut TextPainter,

    /// 逻辑变换：把当前局部坐标映射到归一化坐标，在 CPU 生成顶点时就已生效
    /// （因此 lyon 的容差计算能感知到它，见 `set_tolerance`）。
    pub transform: Matrix,
    /// GL 模型矩阵：与 `transform` 并存的理由是二者处于管线不同阶段——
    /// `transform` 影响 CPU 端顶点，`gl_transform` 影响顶点着色器。
    /// 命中测试必须同时考虑二者，才能与实际画面一致（见 `RectButton::set`）。
    pub gl_transform: Mat4,
    /// 当前裁剪矩形 `(x, y, w, h)`（像素、左下原点）；嵌套时为各层的交集，`None` 表示不裁剪。
    scissor: Option<(i32, i32, i32, i32)>,
    /// 本帧触摸事件缓存，首次访问时从 `Judge` 一次性取出；`None` 表示尚未取用。
    touches: Option<Vec<Touch>>,

    /// 复用的 lyon 顶点/索引缓冲。注意 `emit_lyon` 用 `mem::take` 交出所有权，
    /// 因此每次提交后容量会归零（见该方法的说明）。
    vertex_buffers: VertexBuffers<Vertex, u16>,
    /// 填充镶嵌器，内部持有工作缓冲，跨帧复用以避免重复分配。
    fill_tess: FillTessellator,
    /// 填充参数；容差随当前缩放每帧变化。
    fill_options: FillOptions,
    /// 描边镶嵌器，同样跨帧复用。
    stroke_tess: StrokeTessellator,
    /// 描边参数。公开字段，允许调用方临时改写线宽/线帽（如 `Slider::render` 需要 Round 端点）。
    pub stroke_options: StrokeOptions,

    /// 当前全局透明度乘子，随 `alpha()` 嵌套相乘，作用于所有顶点色与阴影。
    pub alpha: f32,
}

// 构造与基础查询。
impl<'a> Ui<'a> {
    /// 创建 UI 上下文：开启一个默认渲染 pass（清屏）、确定视口并初始化所有绘制状态。
    ///
    /// 副作用：会立即 `begin_default_pass` 并清除颜色缓冲，因此每个渲染帧只应在最外层
    /// 调用一次 `Ui::new`（嵌套 UI 应通过 `with`/`scope` 复用同一实例）。
    /// `viewport` 为 `None` 时取全屏；`top` 由视口高宽比推导，是归一化坐标的核心常量。
    pub fn new(text_painter: &'a mut TextPainter, viewport: Option<(i32, i32, i32, i32)>) -> Self {
        // SAFETY: UI 上下文只在持有 GL 上下文的主渲染线程创建，此时不存在其它借用 `gl` 的活动引用。
        unsafe { get_internal_gl() }.quad_context.begin_default_pass(PassAction::Clear {
            depth: None,
            stencil: Some(0),
            color: None,
        });
        // 视口缺省为整个屏幕；`top` 取高宽比，使归一化 y 的 ±top 恰好覆盖屏幕高度。
        let viewport = viewport.unwrap_or_else(|| (0, 0, screen_width() as i32, screen_height() as i32));
        Self {
            top: viewport.3 as f32 / viewport.2 as f32,
            viewport,

            text_painter,

            transform: Matrix::identity(),
            gl_transform: Mat4::IDENTITY,
            scissor: None,
            touches: None,

            vertex_buffers: VertexBuffers::new(),
            fill_tess: FillTessellator::new(),
            fill_options: FillOptions::default(),
            stroke_tess: StrokeTessellator::new(),
            stroke_options: StrokeOptions::default(),

            alpha: 1.,
        }
    }

    /// 构造与 UI 坐标系匹配的 2D 相机。
    /// `viewport` 限定到本 Ui 的目标区域；`zoom.y` 取负值是为了让 macroquad 原生的
    /// `draw_*`/`gl_use_material` 系列也采用“y 向下”的同一方向，避免同一帧内两套坐标约定打架。
    pub fn camera(&self) -> Camera2D {
        Camera2D {
            zoom: vec2(1., -self.viewport.2 as f32 / self.viewport.3 as f32),
            viewport: Some(self.viewport),
            ..Default::default()
        }
    }

    /// 惰性获取本帧触摸列表（可能含多指）。
    /// 之所以缓存到字段：一帧内触摸列表是不变的，而 `Judge::get_touches` 需要做过滤/拷贝，
    /// 重复调用纯属浪费；此外多个控件会在同一帧依次查询同一份数据。
    pub fn ensure_touches(&mut self) -> &mut Vec<Touch> {
        if self.touches.is_none() {
            self.touches = Some(Judge::get_touches());
        }
        self.touches.as_mut().unwrap()
    }

    /// 注入外部提供的触摸序列，用于替换从 `Judge` 读取的默认来源
    /// （例如回放/录制场景需要按脚本喂入触摸）。设为 `None` 则下次访问时重新从 `Judge` 拉取。
    pub(crate) fn set_touches(&mut self, touches: Option<Vec<Touch>>) {
        self.touches = touches;
    }

    /// 创建顶点构造器，自动带上当前变换与透明度。
    /// 泛型参数先经 `IntoShading` 归一化（`Color` 会被转换成 `GradientShading`），
    /// 因此调用方直接传 `Color` 或任意 `Shading` 实现都可以。
    pub fn builder<T: IntoShading>(&self, shading: T) -> VertexBuilder<T::Target> {
        VertexBuilder::new(self.transform, shading.into_shading(), self.alpha)
    }

    /// 填充一个轴对齐矩形。
    /// 走手工四顶点路径而不是 lyon：矩形的三角化是平凡的，省掉镶嵌器的固定开销对
    /// 这个最热门的图元入口收益明显。两个三角形共享一条对角线，顶点顺序与 `add` 的
    /// 调用顺序（左上/右上/左下/右下）一一对应。
    pub fn fill_rect(&mut self, rect: Rect, shading: impl IntoShading) {
        let mut b = self.builder(shading);
        b.add(rect.x, rect.y);
        b.add(rect.x + rect.w, rect.y);
        b.add(rect.x, rect.y + rect.h);
        b.add(rect.x + rect.w, rect.y + rect.h);
        b.triangle(0, 1, 2);
        b.triangle(1, 2, 3);
        b.commit();
    }

    /// 按当前缩放动态设置镶嵌容差。
    /// lyon 的 `tolerance` 以**局部坐标**为单位，若固定不变：放大后曲线会出现可见折线，
    /// 缩小后又会产生大量无用三角形。这里以“屏幕上的 0.15 像素误差”为目标反推局部容差
    /// （用当前 x 轴缩放与半屏宽估算一个像素对应的局部长度），在画质与顶点数之间取平衡。
    fn set_tolerance(&mut self) {
        let tol = 0.15 / (self.transform.transform_vector(&Vector::new(1., 0.)).norm() * screen_width() / 2.);
        self.fill_options.tolerance = tol;
        self.stroke_options.tolerance = tol;
    }

    /// lyon 绘制的通用骨架：设置容差 -> 构造顶点转换器 -> 由 `f` 执行镶嵌 -> 提交。
    /// `f` 内部决定用填充还是描边镶嵌器；把纹理在镶嵌**之前**取出（`shaded.1.texture()`），
    /// 是因为 `f` 会借用 `self`，之后无法再读 `shaded`；同时也保证同一批顶点只用一张纹理。
    fn draw_lyon<T: Shading>(&mut self, shading: T, f: impl FnOnce(&mut Self, ShadedConstructor<T>)) {
        self.set_tolerance();
        let shaded = ShadedConstructor(self.transform, shading.into_shading(), self.alpha);
        let tex = shaded.1.texture();
        f(self, shaded);
        self.emit_lyon(tex);
    }

    /// 填充任意路径（`PathEvent` 迭代器，通常是 `Path`）。
    /// 这里 `unwrap()` 镶嵌结果：lyon 对合法输入不会失败，唯一可能的失败是顶点数
    /// 溢出 `u16` 索引（需要几万个顶点），UI 的单条路径远达不到该量级。
    pub fn fill_path(&mut self, path: impl IntoIterator<Item = PathEvent>, shading: impl IntoShading) {
        self.draw_lyon(shading.into_shading(), |this, shaded| {
            this.fill_tess
                .tessellate(path, &this.fill_options, &mut BuffersBuilder::new(&mut this.vertex_buffers, shaded))
                .unwrap();
        });
    }

    /// 填充一个圆。交给 lyon 做三角化（比自行按扇形/Voronoi 切分更省事，且容差自适应）。
    /// 注意：圆被限制为**正圆**（半径用归一化长度，x/y 同尺度），因此非等比缩放的坐标系下会呈椭圆。
    pub fn fill_circle(&mut self, x: f32, y: f32, radius: f32, shading: impl IntoShading) {
        self.draw_lyon(shading.into_shading(), |this, shaded| {
            this.fill_tess
                .tessellate_circle(lm::point(x, y), radius, &this.fill_options, &mut BuffersBuilder::new(&mut this.vertex_buffers, shaded))
                .unwrap();
        });
    }

    /// 描边一个圆环，`width` 为线宽（局部长度单位）。
    /// 线宽被写回 `self.stroke_options` 并**保持到下次被覆盖**：这是有意为之的“粘性”状态，
    /// 使调用方可以只设置一次线帽/线宽，随后连续描边多个图元；代价是要注意跨图元的相互影响。
    pub fn stroke_circle(&mut self, x: f32, y: f32, radius: f32, width: f32, shading: impl IntoShading) {
        self.draw_lyon(shading.into_shading(), |this, shaded| {
            this.stroke_options.line_width = width;
            this.stroke_tess
                .tessellate_circle(lm::point(x, y), radius, &this.stroke_options, &mut BuffersBuilder::new(&mut this.vertex_buffers, shaded))
                .unwrap();
        });
    }

    /// 描边任意路径，`width` 为线宽（同样会写回 `stroke_options`）。
    /// 与 `fill_path` 的区别：描边对开放路径有效（如滑轨线段），依赖 `stroke_options`
    /// 的线帽/接头设置；`Slider::render` 就需要临时切换线帽来实现圆头端点。
    pub fn stroke_path(&mut self, path: &Path, width: f32, shading: impl IntoShading) {
        self.draw_lyon(shading.into_shading(), |this, shaded| {
            this.stroke_options.line_width = width;
            this.stroke_tess
                .tessellate_path(path, &this.stroke_options, &mut BuffersBuilder::new(&mut this.vertex_buffers, shaded))
                .unwrap();
        });
    }

    /// 把 lyon 累积的顶点/索引一次性提交给 `quad_gl`，并清空缓冲。
    /// 用 `mem::take` 而非 `clone`/`drain`：顶点数据所有权直接交给提交批次，无需复制。
    /// 代价是被取走的 `Vec` 容量归零，下一帧需要重新增长（每帧一次分配，量级很小，
    /// 但这是已知的、可接受的取舍）；若将来成为瓶颈，可以改为在提交后换回容量。
    fn emit_lyon(&mut self, texture: Option<Texture2D>) {
        // SAFETY: 与 `VertexBuilder::commit` 同理——UI 绘制只发生在持有 GL 上下文的主渲染线程，
        // 且本方法独占 `&mut self`，不存在与 GL 相关的别名借用。
        let gl = unsafe { get_internal_gl() }.quad_gl;
        gl.texture(texture);
        gl.draw_mode(DrawMode::Triangles);
        gl.geometry(&std::mem::take(&mut self.vertex_buffers.vertices), &std::mem::take(&mut self.vertex_buffers.indices));
    }

    /// 覆盖整个屏幕的归一化矩形：左上角 `(-1, -top)`，尺寸 `(2, 2*top)`。
    /// 语义上等于“在**单位变换**下铺满全屏”，因此常与 `abs_scope`/全屏遮罩配合使用。
    pub fn screen_rect(&self) -> Rect {
        Rect::new(-1., -self.top, 2., self.top * 2.)
    }

    /// 对话框的固定归一化尺寸：宽 0.9、高 0.68。
    /// 写成“半宽/半高”（0.45/0.34）是为了让矩形关于原点对称，配合居中的 UI 布局可省去偏移计算。
    pub fn dialog_rect() -> Rect {
        let hw = 0.45;
        let hh = 0.34;
        Rect::new(-hw, -hh, hw * 2., hh * 2.)
    }

    /// 把局部矩形变换到全局归一化坐标。
    /// 位置按**点**变换、尺寸按**向量**变换（不含平移），这是矩形变换的正确做法。
    /// 局限：返回类型仍是轴对齐的 `Rect`，因此只适用于无旋转/斜切的变换——
    /// 若当前 transform 含旋转，结果只是一个近似包围盒（命中判定请用 `RectButton`）。
    pub fn rect_to_global(&self, rect: Rect) -> Rect {
        let pt = self.to_global((rect.x, rect.y));
        let vec = self.vec_to_global((rect.w, rect.h));
        Rect::new(pt.0, pt.1, vec.0, vec.1)
    }

    /// 变换一个**方向/尺寸**向量（不含平移）。用于把长度从局部换算到全局。
    pub fn vec_to_global(&self, vec: (f32, f32)) -> (f32, f32) {
        let r = self.transform.transform_vector(&Vector::new(vec.0, vec.1));
        (r.x, r.y)
    }

    /// 局部点 -> 全局归一化坐标。
    pub fn to_global(&self, pt: (f32, f32)) -> (f32, f32) {
        let r = self.transform.transform_point(&Point::new(pt.0, pt.1));
        (r.x, r.y)
    }

    /// 全局归一化坐标 -> 局部点（`transform` 的逆变换）。
    /// `try_inverse().unwrap()` 的成立前提是 transform 始终由平移/缩放/旋转组合而成（可逆）；
    /// 若将来引入 0 缩放（如 `with` 传入全零矩阵）会在此处 panic。
    pub fn to_local(&self, pt: (f32, f32)) -> (f32, f32) {
        let r = self.transform.try_inverse().unwrap().transform_point(&Point::new(pt.0, pt.1));
        (r.x, r.y)
    }

    /// 在当前变换右侧追加 x 方向平移，作为“布局游标”推进使用。
    /// 追加在右侧意味着先作用于局部内容，再叠加外层变换，因此与嵌套 `with` 的组合顺序直观一致。
    pub fn dx(&mut self, x: f32) {
        self.transform.append_translation_mut(&Vector::new(x, 0.));
    }

    /// 在当前变换右侧追加 y 方向平移。注意 y 轴朝下，正值表示向下移动。
    pub fn dy(&mut self, y: f32) {
        self.transform.append_translation_mut(&Vector::new(0., y));
    }

    /// 在 `f` 执行期间把全局 alpha 乘以 `alpha`，退出时恢复。
    /// 嵌套调用天然支持“父容器整体淡出”：所有顶点色（阴影、文字都在内）会一起衰减，
    /// 这正是 `alpha` 由 `VertexBuilder`/`ShadedConstructor` 统一施加、而不是各控件各自处理的原因。
    #[inline]
    pub fn alpha<R>(&mut self, alpha: f32, f: impl FnOnce(&mut Self) -> R) -> R {
        let old = self.alpha;
        self.alpha = old * alpha;
        let res = f(self);
        self.alpha = old;
        res
    }

    /// 在**叠加了 `transform`** 的坐标系中执行 `f`，退出时恢复原变换。
    /// 新变换乘在右侧（`old * transform`），即 `transform` 先作用于局部坐标，
    /// 因此嵌套 `with` 的书写顺序与“由内到外”的直观顺序一致。
    /// 恢复是手动而非 RAII：若 `f` panic 会跳过恢复，但 UI 代码中 panic 属致命错误，故不额外兜底。
    #[inline]
    pub fn with<R>(&mut self, transform: Matrix, f: impl FnOnce(&mut Self) -> R) -> R {
        let old = self.transform;
        self.transform = old * transform;
        let res = f(self);
        self.transform = old;
        res
    }

    /// 在一个“作用域”内执行 `f`，退出时把 `transform` 恢复为进入时的值。
    /// 与 `with` 的区别：不预先叠加任何变换，留给 `f` 自行修改（例如 `ui.dx(..)` 后仍能整体回滚）。
    #[inline]
    pub fn scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let old = self.transform;
        let res = f(self);
        self.transform = old;
        res
    }

    /// 在**单位变换**下执行 `f`，退出时恢复外层布局。
    /// 用于必须绝对定位的覆盖层（全屏遮罩、居中加载提示），使它们不受当前布局游标/
    /// 嵌套 `with` 的影响，从而在任何上下文里都能铺满屏幕。
    #[inline]
    pub fn abs_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let old = self.transform;
        self.transform = Matrix::identity();
        let res = f(self);
        self.transform = old;
        res
    }

    /// 在额外的 GL 模型矩阵下执行 `f`。
    /// 实现方式是把矩阵压入 `quad_gl` 的模型矩阵栈（作用于顶点着色器阶段），
    /// 因此可以表达 CPU 端不便计算的变换；退出时先 `flush` 再弹栈，
    /// 避免已提交但尚未执行的批次在弹栈后才被处理、从而画在错误的位置。
    /// 注意：多次进入并**不会**在逻辑层累积——相乘那句已被注释，`self.gl_transform` 只是
    /// 恢复为进入时的值，实际变换完全由 `quad_gl` 的矩阵栈承担。`RectButton::set` 仍会读取
    /// 该字段，以便将来在“整体被 GL 变换”时让命中区域保持一致；按当前实现它恒为单位矩阵。
    #[inline]
    pub fn with_gl<R>(&mut self, transform: Mat4, f: impl FnOnce(&mut Self) -> R) -> R {
        let old = self.gl_transform;
        // self.gl_transform = old * transform;
        // SAFETY: 本方法在 UI 渲染线程执行；`get_internal_gl` 返回的全局状态此处被独占使用，
        // 期间不会把控制权交给可能并发访问 GL 的代码（`f` 在栈压入期间执行，属既有约定）。
        let gl = unsafe { get_internal_gl() }.quad_gl;
        gl.push_model_matrix(transform);
        let res = f(self);
        self.gl_transform = old;
        // SAFETY: 同上；此处只是把待执行的绘制批次刷出，随后即可安全弹栈。
        unsafe { get_internal_gl() }.flush();
        gl.pop_model_matrix();
        res
    }

    /// 把当前 `transform` 压入 GL 模型矩阵栈并执行 `f`。
    /// 与 `with_gl` 的差别：它不改动 `self.transform`，而是把已经算好的逻辑变换直接交给 GPU，
    /// 用于让 macroquad 原生绘制路径（例如 `rounded_rect_shadow` 内部的 `draw_rectangle`）
    /// 也跟随 UI 布局，省去把 `Matrix` 手动换算成 `Mat4` 的麻烦。
    #[inline]
    pub fn apply<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> R {
        // SAFETY: 与 `with_gl` 相同——调用发生在 UI 渲染线程，矩阵栈的入栈/出栈在此成对出现，
        // 不会与其它线程的 GL 访问重叠。
        unsafe { get_internal_gl() }.quad_gl.push_model_matrix(nalgebra_to_glm(&self.transform));
        let res = f(self);
        unsafe { get_internal_gl() }.quad_gl.pop_model_matrix();
        res
    }

    /// 在裁剪矩形 `rect`（当前局部坐标系下）内执行 `f`，退出时恢复上层裁剪。
    ///
    /// 换算过程：先把 `rect` 投到全局归一化坐标，再映射为**像素**矩形 `(l, t, w, h)`。
    /// y 方向需要翻转 `screen_height - (vp.y + vp.h)`：GL 的 scissor 以窗口左下角为原点，
    /// 而 UI 的 y 轴朝下；x 方向则只需按视口比例缩放（`rect.w * vp.2 / 2` 即归一化宽度转像素）。
    ///
    /// 嵌套支持：与上层裁剪取交集（左/上取 max、右/下取 min），因此子区域永远不会越出父区域；
    /// 空交集时宽高变成负数，GL 会视作不可见区域，效果等价于“全部裁掉”，无需额外特判。
    ///
    /// 之所以自己在 `self.scissor` 里维护嵌套深度，是因为 `quad_gl` 的 scissor 只是即时状态，
    /// 需要在这里叠加并保证退出时精确还原。
    pub fn scissor<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Ui) -> R) -> R {
        // SAFETY: UI 渲染线程独占 GL 上下文；本方法只在同一线程内读取全局状态并设置 scissor，
        // 不涉及跨线程共享。
        let igl = unsafe { get_internal_gl() };
        let gl = igl.quad_gl;
        // 阶段 1：把局部矩形化为全局坐标，并换算成以窗口左下角为原点的像素矩形。
        let rect = self.rect_to_global(rect);
        let vp = get_viewport();
        let pt = (
            vp.0 as f32 + (rect.x + 1.) / 2. * vp.2 as f32,
            (screen_height() - (vp.1 + vp.3) as f32) + (rect.y * vp.2 as f32 / vp.3 as f32 + 1.) / 2. * vp.3 as f32,
        );

        let old = self.scissor;
        self.scissor = {
            let mut l = pt.0 as i32;
            let mut t = pt.1 as i32;
            let mut r = (pt.0 + rect.w * vp.2 as f32 / 2.) as i32;
            let mut b = (pt.1 + rect.h * vp.2 as f32 / 2.) as i32;
            // 阶段 2：与上层裁剪求交（左/上取 max、右/下取 min），实现嵌套裁剪。
            if let Some((l0, t0, w0, h0)) = old {
                l = l.max(l0);
                t = t.max(t0);
                r = r.min(l0 + w0);
                b = b.min(t0 + h0);
            }
            Some((l, t, r - l, b - t))
        };

        // 阶段 3：应用裁剪、执行内容，之后把裁剪状态还原成上层（含 `self.scissor` 与 GL 状态）。
        gl.scissor(self.scissor);
        let res = f(self);
        self.scissor = old;
        gl.scissor(old);
        res
    }

    /// 创建文字绘制构建器（`DrawText`）。不立即绘制：需要再调用 `draw()`，
    /// 或先调用 `measure()` 拿到尺寸用于布局——这正是把文字做成 builder 的目的。
    pub fn text<'s, 'ui>(&'ui mut self, text: impl Into<Cow<'s, str>>) -> DrawText<'a, 's, 'ui> {
        DrawText::new(self, text.into())
    }

    /// 通用的“点击一个矩形”实现：在 `entry` 中维护当前捕获的触摸 id，
    /// 抬起时若仍在矩形内则返回 `true`（判定语义与 `RectButton` 相同）。
    ///
    /// 与 `RectButton` 的差别在于它直接操作 `touches`，会把**已消费**的触摸从列表中移除，
    /// 从而保证同一次点击不会同时被下层/后续控件响应——这是工具栏、复选框等
    /// “内联控件”比构造 `RectButton` 更省事的替代方案。
    /// 需要 `&mut self` 是因为它要读取（并可能修改）本帧的触摸缓存。
    fn clicked(&mut self, rect: Rect, entry: &mut Option<u64>) -> bool {
        let rect = self.rect_to_global(rect);
        let mut exists = false;
        let mut any = false;
        let old_entry = *entry;
        let mut res = false;
        // 单次遍历同时完成“触摸分配”与“点击判定”：命中的触摸返回 false 即被移出列表
        // （表示已由本控件消费），未命中的保留给后续控件，避免同一次点击被重复响应。
        self.ensure_touches().retain(|touch| {
            exists = exists || old_entry == Some(touch.id);
            if !rect.contains(touch.position) {
                return true;
            }
            any = true;
            match touch.phase {
                TouchPhase::Started => {
                    *entry = Some(touch.id);
                    false
                }
                TouchPhase::Moved | TouchPhase::Stationary => {
                    if *entry != Some(touch.id) {
                        *entry = None;
                        true
                    } else {
                        false
                    }
                }
                TouchPhase::Cancelled => {
                    *entry = None;
                    true
                }
                TouchPhase::Ended => {
                    if entry.take() == Some(touch.id) {
                        res = true;
                        false
                    } else {
                        true
                    }
                }
            }
        });
        if res {
            return true;
        }
        // 所有触摸都已离开矩形、而此前确实有手指按住本控件：清理这个过期的捕获 id，
        // 否则它会一直“占位”，使同一根手指下次按下时无法被重新捕获。
        if !any && exists {
            *entry = None;
        }
        false
    }

    /// 主题强调色（Material Blue 500），用于高亮当前值、选中项等。
    pub fn accent(&self) -> Color {
        Color::from_hex_rgb(0x2196f3)
    }

    /// 主题背景色（深灰蓝）。按钮底色、滑轨底槽、复选框未选中态都由它派生，
    /// 集中在一处可保证整体色调一致，也方便将来做换肤。
    pub fn background(&self) -> Color {
        Color::from_hex_rgb(0x2a323c)
    }

    /// 绘制一个即时模式按钮，返回本帧是否被点击。
    /// `id` 是**跨帧稳定**的标识，用于在 `STATE` 中保存“哪个手指按住本按钮”，
    /// 因此同一个 `id` 在同一帧内不能用于两个不同位置的按钮。
    /// 按住时底部 alpha 降到 0.5 作为轻量按压反馈（不做缩放，避免与 `DRectButton` 的动画竞争）；
    /// 文字超过按钮宽度时由 `DrawText` 自动做省略号截断。
    pub fn button(&mut self, id: &str, rect: Rect, text: impl Into<String>) -> bool {
        let text = text.into();
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let entry = state.entry(id.to_owned()).or_default();
            self.fill_path(
                &rect.rounded(0.01),
                Color {
                    a: if entry.is_some() { 0.5 } else { 1. },
                    ..self.background()
                },
            );
            let ct = rect.center();
            self.text(text)
                .pos(ct.x, ct.y)
                .anchor(0.5, 0.5)
                .max_width(rect.w)
                .size(0.42)
                .color(WHITE)
                .no_baseline()
                .draw();
            self.clicked(rect, entry)
        })
    }

    /// 绘制一个复选框，并在被点击时原地翻转 `value`；返回“文字 + 方框”的整体矩形。
    /// 返回矩形（而非 `bool`）是因为返回的是**布局信息**：调用方可以继续在其右侧或下方排布，
    /// 或据此绘制高亮边框——点击结果已经写进 `value`，无需再回传。
    /// 方框半边长固定 0.025（即 0.05 见方），文字行高更矮时用 `w` 兜底，
    /// 保证点击区域至少与文字行等高，避免细行文字难以点中。
    pub fn checkbox(&mut self, text: impl Into<String>, value: &mut bool) -> Rect {
        let text = text.into();
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let entry = state.entry(format!("chkbox#{text}")).or_default();
            let w = 0.08;
            let s = 0.025;
            let text = self.text(text).pos(w, 0.).size(0.47).no_baseline().draw();
            let r = Rect::new(w / 2. - s, text.center().y - s, s * 2., s * 2.);
            self.fill_path(
                &r.rounded(0.01),
                Color {
                    a: if entry.is_some() { 0.5 } else { 1. },
                    ..if *value { WHITE } else { self.background() }
                },
            );
            let r = Rect::new(r.x, r.y, text.right() - r.x, (text.bottom() - r.y).max(w));
            if self.clicked(r, entry) {
                *value ^= true;
            }
            r
        })
    }

    /// 绘制“标签 + 可点击输入区”的输入控件，返回标签与输入区合并后的矩形。
    ///
    /// 交互协议（跨场景的异步往返）：点击输入区 -> `request_input` 请求宿主弹出系统输入框；
    /// 宿主返回后本方法用 `take_input` 取回结果。取出时若 id 不匹配，必须用
    /// `return_input` **原样放回**，否则同帧内其它输入框的内容会丢失。
    /// 密码模式用 `*` 遮蔽，但按真实字符数生成，使长度信息仍然可见（仅用于显示，不泄漏内容）。
    /// 标签用右对齐绘制，因此输入区位置与标签长度无关（`params.length` 固定输入区宽度）。
    pub fn input<'b>(&mut self, label: impl Into<String>, value: &mut String, params: impl Into<InputParams<'b>>) -> Rect {
        let label = label.into();
        let params = params.into();
        let id = format!("input#{label}");
        let r = self.text(label).anchor(1., 0.).size(0.47).draw();
        let lf = r.x;
        let r = Rect::new(0.02, r.y - 0.01, params.length, r.h + 0.02);
        if if params.mode == InputMode::Password {
            self.button(&id, r, "*".repeat(value.chars().count()))
        } else {
            self.button(&id, r, value.lines().next().unwrap_or_default())
        } {
            request_input(&id, InputBox::new().default_text(value.as_str()).mode(params.mode));
        }
        if let Some((its_id, text)) = take_input() {
            if its_id == id {
                if let Some(changed) = params.changed {
                    *changed = true;
                }
                *value = text;
            } else {
                return_input(its_id, text);
            }
        }
        Rect::new(lf, r.y, r.right() - lf, r.h)
    }

    /// 绘制「标签 + 滑轨 + ± 按钮」的参数调节控件。
    /// 与 `Slider` 的分工：`Slider` 是保留状态、由调用方显式 `touch` 的控件；
    /// 本方法把状态直接写回 `value` 并把捕获的手指记在 `STATE` 里，一行即可使用。
    /// `length` 为滑轨长度（`None` 用默认 0.3）；数值会被吸附到 `step` 的整数倍。
    /// 返回控件占据的整体矩形，便于调用方继续排布。
    pub fn slider(&mut self, text: impl Into<String>, range: Range<f32>, step: f32, value: &mut f32, length: Option<f32>) -> Rect {
        let text = text.into();
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let entry = state.entry(text.clone()).or_default();

            // 阶段 1：先画标签行（"名称: 数值"），行高决定滑轨的 y 位置——
            // 这样即使字号/翻译文本变化，滑轨也不会与标签重叠。
            let len = length.unwrap_or(0.3);
            let s = 0.002;
            let tr = self.text(format!("{text}: {value:.3}")).size(0.4).draw();
            let cy = tr.h + 0.03;
            // 阶段 2：滑轨底槽（一条很细的白色长条，`s*2` 让其有 2 像素级的可见度）
            // 与表示当前值的强调色圆点。
            let r = Rect::new(0., cy - s, len, s * 2.);
            self.fill_rect(r, WHITE);
            let p = (*value - range.start) / (range.end - range.start);
            let p = p.clamp(0., 1.);
            self.fill_circle(len * p, cy, 0.015, self.accent());
            // 阶段 3：把命中区域向外扩张到指示点的半径（0.015），使点按比视觉细线更宽容；
            // 再投影到全局坐标，因为触摸位置是全局的。
            let r = r.feather(0.015 - s);
            let r = self.rect_to_global(r);
            self.ensure_touches();
            // 阶段 4：已有手指在拖动——把触摸位置换算回局部坐标再除以长度得到比例，
            // 因此该控件可以放在任意嵌套变换（含整体平移/缩放）之下而不失准。
            if let Some(id) = entry {
                if let Some(touch) = self.touches.as_ref().unwrap().iter().rfind(|it| it.id == *id) {
                    let Vec2 { x, y } = touch.position;
                    let (x, _) = self.to_local((x, y));
                    let p = (x / len).clamp(0., 1.);
                    *value = range.start + (range.end - range.start) * p;
                    *value = (*value / step).round() * step;
                    if matches!(touch.phase, TouchPhase::Cancelled | TouchPhase::Ended) {
                        *entry = None;
                    }
                }
            } else if let Some(touch) = self.touches.as_ref().unwrap().iter().find(|it| r.contains(it.position)) {
                if touch.phase == TouchPhase::Started {
                    *entry = Some(touch.id);
                }
            }

            // 阶段 5：右侧两个步进按钮（方块 + 居中符号）。方块边长 `s*2`，
            // 起点在滑轨末尾留 0.02 的空隙，避免与拖动手柄的命中区重叠。
            let s = 0.025;
            let mut x = len + 0.02;
            let r = Rect::new(x, cy - s, s * 2., s * 2.);
            self.fill_path(&r.rounded(0.008), self.background());
            self.text("-")
                .pos(r.center().x, r.center().y)
                .anchor(0.5, 0.5)
                .size(0.4)
                .color(WHITE)
                .no_baseline()
                .draw();
            if self.clicked(r, state.entry(format!("{text}:-")).or_default()) {
                *value = (*value - step).max(range.start);
            }
            // 第二个按钮紧接第一个，中间留 0.01 的间隙。
            x += s * 2. + 0.01;
            let r = Rect::new(x, cy - s, s * 2., s * 2.);
            self.fill_path(&r.rounded(0.008), self.background());
            self.text("+")
                .pos(r.center().x, r.center().y)
                .anchor(0.5, 0.5)
                .size(0.4)
                .color(WHITE)
                .no_baseline()
                .draw();
            if self.clicked(r, state.entry(format!("{text}:+")).or_default()) {
                *value = (*value + step).min(range.end);
            }

            // 返回整个控件的包围盒（宽 × 高），供调用方继续排布。
            Rect::new(0., 0., x + s * 2., cy + s)
        })
    }

    /// 以网格形式排布 `count` 个内容单元：每行 `row_num` 个，单元尺寸 `width x height`。
    ///
    /// 调用方式：`content(self, index)` 里只需在**当前原点**绘制，本方法负责推进布局游标；
    /// 因此调用方不必关心行列计算。返回 `(总宽, 总高)`，便于把整块内容居中。
    /// `row_num` 必须大于 0（它会作为除数与步长）。
    /// 方法结束时会复位游标（`dx`/`dy` 各回退），即**除返回值外不改变布局状态**。
    pub fn hgrids(&mut self, width: f32, height: f32, row_num: u32, count: u32, mut content: impl FnMut(&mut Self, u32)) -> (f32, f32) {
        let mut sh = 0.;
        let w = width / row_num as f32;
        for i in (0..count).step_by(row_num as usize) {
            let mut sw = 0.;
            for j in 0..(count - i).min(row_num) {
                content(self, i + j);
                // 每个单元向右推进一格；`sw` 记录本行实际推进的距离，
                // 使最后一个不满行也能正确回到行首。
                self.dx(w);
                sw += w;
            }
            self.dx(-sw);
            // 一行结束后向下移动一个行高。
            self.dy(height);
            sh += height;
        }
        self.dy(-sh);
        (width, sh)
    }

    /// 绘制圆形头像并返回其矩形。三种状态用 `Result<Option<..>>` 表达，避免额外的枚举：
    /// - `Ok(Some(tex))`：加载完成，用纹理填充圆形；
    /// - `Ok(None)`：仍在加载或用户没有头像，显示环形 loading；
    /// - `Err(icon)`：加载失败，用深色底叠加“居中裁剪”的占位图标。
    /// 所有状态下都会绘制阴影与外描边，使占位框与实际图像占据同一尺寸，
    /// 这样加载完成时不会发生布局跳动。
    pub fn avatar(&mut self, cx: f32, cy: f32, r: f32, t: f32, avatar: Result<Option<SafeTexture>, SafeTexture>) -> Rect {
        rounded_rect_shadow(
            self,
            Rect::new(cx - r, cy - r, r * 2., r * 2.),
            &ShadowConfig {
                radius: r,
                ..Default::default()
            },
        );
        let rect = Rect::new(cx - r, cy - r, r * 2., r * 2.);
        match avatar {
            Ok(Some(avatar)) => {
                self.fill_circle(cx, cy, r, (*avatar, rect));
            }
            Ok(None) => {
                self.loading(
                    cx,
                    cy,
                    t,
                    WHITE,
                    LoadingParams {
                        radius: r * 0.6,
                        width: 0.008,
                        ..Default::default()
                    },
                );
            }
            Err(icon) => {
                self.fill_circle(cx, cy, r, semi_black(0.2));
                self.fill_circle(cx, cy, r, (*icon, rect.feather(-0.025), ScaleType::CropCenter, WHITE));
            }
        }
        self.stroke_circle(cx, cy, r, 0.004, WHITE);
        rect
    }

    /// 生成一段圆弧路径，供 loading 圆环描边使用。
    /// `start` 是起始角（弧度，正值表示逆时针偏转）、`len` 是弧长（弧度）。
    /// 用 `(sin a, cos a)` 而不是 `(cos a, sin a)`：让角度 0 对应屏幕**正下方**，
    /// 符合“从底部开始顺时针转”的常见 loading 观感。
    /// 用 `svg_builder` 的 `arc` 而非手写折线，是为了让 lyon 依据容差自动细分。
    pub fn loading_path(start: f32, len: f32, r: f32) -> Path {
        use lyon::math::{point, vector, Angle};
        let mut path = Path::svg_builder();
        let pt = |a: f32| {
            let (sin, cos) = a.sin_cos();
            point(sin * r, cos * r)
        };
        path.move_to(pt(-start));
        path.arc(point(0., 0.), vector(r, r), Angle::radians(len), Angle::radians(0.));
        path.build()
    }

    /// 每完成一圈“伸缩”循环、起始角额外前进的圈数（<1 使每圈不完全重叠，看起来更自然）。
    const LOADING_SCALE: f32 = 0.74;
    /// 弧长伸缩的角速度（弧度/秒），决定“吸气—吐气”的节奏快慢。
    const LOADING_CHANGE_SPEED: f32 = 3.5;
    /// 整体自转角速度（弧度/秒）。与伸缩频率取不同的值，避免两者同步后动画显得呆板。
    const LOADING_ROTATE_SPEED: f32 = 4.1;

    /// 绘制环形 loading 动画。
    /// 两种模式：给定 `params.progress` 时弧长固定为进度对应的角度（只自转，用于上传/下载百分比）；
    /// 否则使用“弧长伸缩 + 自转”的无限动画（弧形先变长再缩短），比匀速转圈更能传达“正在工作”。
    /// `params.last` 用于跨帧平滑弧长，消除相位切换处的跳变。
    /// `t` 是秒为单位的场景时间，动画由它驱动而非内部计时，因此可以被暂停/加速。
    pub fn loading<'b>(&mut self, cx: f32, cy: f32, t: f32, shading: impl IntoShading, params: impl Into<LoadingParams<'b>>) {
        use std::f32::consts::PI;

        let params = params.into();
        // 两种模式：显式进度 -> 弧长固定、只自转；否则走下面的“伸缩 + 自转”无限动画。
        let (st, mut len) = if let Some(p) = params.progress {
            (t * Self::LOADING_ROTATE_SPEED, p * PI * 2.)
        } else {
            // 把时间量化为“整圈数 round + 圈内相位 t”：圈数决定起始角的基础偏移
            // （每圈额外前进 LOADING_SCALE 圈），相位在超过 π 后用 sin 抬起起始角，
            // 同时用 cos 调制弧长，两者相位错开形成伸缩效果。
            let ct = t * Self::LOADING_CHANGE_SPEED;
            let round = (ct / (PI * 2.)).floor();
            let st = round * Self::LOADING_SCALE + {
                let t = ct - round * PI * 2.;
                if t < PI {
                    0.
                } else {
                    ((t - PI * 3. / 2.).sin() + 1.) * Self::LOADING_SCALE / 2.
                }
            };
            let st = st * PI * 2. + t * Self::LOADING_ROTATE_SPEED;
            let len = (-ct.cos() * Self::LOADING_SCALE / 2. + 0.5) * PI * 2.;
            (st, len)
        };
        // 用上一帧弧长做加权平均（新值权重 5/6）平滑过渡，消除相位切换处的跳变；
        // 状态通过 `&mut f32` 交还调用方保存，UI 自身不持有动画时序状态。
        if let Some(last) = params.last {
            len = (*last * 5. + len) / 6.;
            *last = len;
        }
        // 平移到圆心后描边：路径以圆心为原点构造，因此只需一次平移即可定位。
        self.scope(|ui| {
            ui.dx(cx);
            ui.dy(cy);
            ui.stroke_path(&Self::loading_path(st, len, params.radius), params.width, shading);
        });
    }

    /// 返回键的固定位置：屏幕左上角内缩 0.04，边长 0.08。
    /// 位置硬编码而不跟随布局游标，保证任何页面里“返回”都在同一处，便于用户形成肌肉记忆。
    #[inline]
    pub fn back_rect(&self) -> Rect {
        Rect::new(-0.97, -self.top + 0.04, 0.08, 0.08)
    }

    /// 绘制一列纵向排列的标签页按钮：`(按钮, 文字, 是否选中)`。
    /// 行距 0.125 略大于按钮高度 0.11，留出视觉分离；起点在左上角下方 0.18 处以避开返回键。
    /// 因为每个 `DRectButton` 自带命中状态，调用方通常还会配合 `list_switch` 播放页签切换音。
    #[inline]
    pub fn tab_rects<'b>(&mut self, t: f32, it: impl IntoIterator<Item = (&'b mut DRectButton, Cow<'b, str>, bool)>) {
        let mut r = Rect::new(-0.92, -self.top + 0.18, 0.2, 0.11);
        for (btn, text, chosen) in it {
            btn.render_text(self, r, t, text, 0.5, chosen);
            r.y += 0.125;
        }
    }

    /// 返回标准内容区的矩形：`x` 固定从 -0.7（左侧导航栏右侧）开始，宽 1.67，
    /// 高度为屏幕高减去上下各 0.09 的留白。这是“左侧标签页 + 右侧内容”页面的统一布局基线。
    #[inline]
    pub fn content_rect(&self) -> Rect {
        Rect::new(-0.7, -self.top + 0.15, 1.67, self.top * 2. - 0.18)
    }

    /// 全屏加载遮罩：半透明黑底 + 居中转圈 + 字幕，用于切场景等需要说明原因的等待。
    /// 用 `screen_rect` 覆盖全屏（不受当前布局游标影响）；
    /// 转圈上移 0.03、文字位于中心下方 0.05，使两者视觉重心落在屏幕中心附近。
    pub fn full_loading<'b>(&mut self, text: impl Into<Cow<'b, str>>, t: f32) {
        self.fill_rect(self.screen_rect(), semi_black(0.6));
        self.loading(0., -0.03, t, WHITE, ());
        self.text(text.into()).pos(0., 0.05).anchor(0.5, 0.).size(0.6).draw();
    }

    /// 只有遮罩与转圈、没有文字的简化版；用于文案动态或无需解释的场合（如自动重连）。
    pub fn full_loading_simple(&mut self, t: f32) {
        self.fill_rect(self.screen_rect(), semi_black(0.6));
        self.loading(0., 0., t, WHITE, ());
    }

    /// 生成“主色/次级色”一对颜色，供明暗两种主题使用。
    /// 次级色固定为主色 alpha 的 0.64 倍：保持同一比例可以让两种主题的信息层次（主/次）观感一致，
    /// 也便于在明暗主题之间切换时复用同一套排版。
    pub fn main_sub_colors(use_black: bool, alpha: f32) -> (Color, Color) {
        if use_black {
            (semi_black(alpha), semi_black(alpha * 0.64))
        } else {
            (semi_white(alpha), semi_white(alpha * 0.64))
        }
    }
}

/// `Ui::loading` 的参数集合。
/// 同样借助 `From` 让第三个实参可以简写为 `()`、`f32`（进度）
/// 或 `(Option<f32>, &mut f32)`（进度 + 跨帧平滑状态）。
pub struct LoadingParams<'a> {
    /// 圆环半径（归一化长度）。
    pub radius: f32,
    /// 线宽（归一化长度），相对半径很细才有“细环”观感。
    pub width: f32,
    /// 指定进度（0..1）时弧长固定，否则走无限循环动画。
    pub progress: Option<f32>,
    /// 上一帧的弧长，用于指数平滑。放在参数里由调用方持有，
    /// 是为了让 `Ui` 保持“无状态”，同一帧内可绘制多个互不干扰的 loading。
    pub last: Option<&'a mut f32>,
}
// 默认参数：半径 0.05、线宽 0.012、无限循环模式、不做平滑。
impl Default for LoadingParams<'_> {
    fn default() -> Self {
        Self {
            radius: 0.05,
            width: 0.012,
            progress: None,
            last: None,
        }
    }
}
// `()` 简写：使用全部默认值。
impl From<()> for LoadingParams<'_> {
    /// 使用默认参数（无限循环动画）。
    fn from(_: ()) -> Self {
        Self::default()
    }
}
// `f32` 简写：直接给出进度。
impl From<f32> for LoadingParams<'_> {
    /// 用给定的进度值进入“固定弧长”模式。
    fn from(progress: f32) -> Self {
        Self {
            progress: Some(progress),
            ..Self::default()
        }
    }
}
// 同时给出进度与平滑状态。
impl<'a> From<(Option<f32>, &'a mut f32)> for LoadingParams<'a> {
    /// 进度可为 `None`（无限动画）但要跨帧平滑弧长。
    fn from((progress, last): (Option<f32>, &'a mut f32)) -> Self {
        Self {
            progress,
            last: Some(last),
            ..Self::default()
        }
    }
}
/// 音频管理器的“可空”包装。
/// `Option` 的意义：`Drop`/`cleanup_audio` 会提前把管理器取出并销毁，
/// 之后任何通过 `Deref` 的使用都会 panic，从而把“音频后端已销毁仍被使用”的错误
/// 暴露在调用点，而不是产生未定义行为。字段为 `pub` 是为了让 `cleanup_audio` 能取出内部值。
pub struct SafeAudio(pub Option<AudioManager>);

// 让 `SafeAudio` 可以像 `AudioManager` 一样直接调用（UI 音效代码遍布各处，此糖很值）。
// 失败路径给出明确 panic 信息而非静默失败。
impl Deref for SafeAudio {
    type Target = AudioManager;
    /// 取出内部管理器；已被析构时 panic（见 `Drop` 的说明）。
    fn deref(&self) -> &AudioManager {
        self.0.as_ref().expect("SafeAudio is already dropped")
    }
}

// 可变解引用，语义同上。
impl DerefMut for SafeAudio {
    /// 可变取出内部管理器；已被析构时 panic。
    fn deref_mut(&mut self) -> &mut AudioManager {
        self.0.as_mut().expect("SafeAudio is already dropped")
    }
}

// 析构时在“临时屏蔽 panic 钩子 + catch_unwind”的包裹下销毁后端。
// 背景：cpal/WASAPI 的流析构会 join 音频线程；若线程已随进程退出而终止，
// 该 join 会 panic，而此刻已无法补救。因此这里主动吞掉 panic（并抑制 panic 输出），
// 保证进程正常退出。用 `take()` 保证销毁只发生一次（`cleanup_audio` 已提前销毁时是空操作）。
impl Drop for SafeAudio {
    fn drop(&mut self) {
        if let Some(inner) = self.0.take() {
            let prev_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drop(inner);
            }));
            std::panic::set_hook(prev_hook);
            if result.is_err() {
                // cleanup_audio() was not called before process exit;
                // the audio backend thread may have already terminated.
                // Nothing we can do — just let the OS reclaim resources.
            }
        }
    }
}

/// Drop the UI audio manager explicitly, before thread-local destructors run.
/// Must be called before the process exits to avoid panicking in cpal's WASAPI
/// stream drop, which tries to join an already-terminated audio thread.
///
/// 在 thread-local 析构之前显式销毁 UI 音频管理器：进程退出阶段再析构后端时，
/// cpal/WASAPI 会尝试 join 一个已经终止的音频线程从而 panic，故必须在退出前主动清掉。
/// 与 `SafeAudio::drop` 配合：这里先 `take`，`drop` 时便成为空操作。
pub fn cleanup_audio() {
    UI_AUDIO.with(|it| {
        it.borrow_mut().0.take();
    });
}

/// 按平台选择后端创建 UI 音频管理器，失败时降级为静音的 `DummyBackend` 并提示用户。
/// 设计意图：UI 音效属于锦上添花，音频后端不可用绝不应阻止游戏运行，
/// 因此失败路径只报错不中断，同时用一个“永远静音”的后端占位，使上层调用点无需判空。
/// 后端用块表达式 + `#[cfg]` 在**编译期**选择，避免运行时分支与无用依赖被链接进来。
// This function is used to create UI audio manager.
#[allow(clippy::blocks_in_conditions)]
fn build_audio() -> SafeAudio {
    match {
        // Android：Oboe 后端，指定低功耗性能模式与 Game 用途（UI 音效不要求极低延迟）。
        #[cfg(target_os = "android")]
        {
            use sasa::backend::oboe::*;
            AudioManager::new(OboeBackend::new(OboeSettings {
                performance_mode: PerformanceMode::PowerSaving,
                usage: Usage::Game,
                ..Default::default()
            }))
        }
        // OpenHarmony：显式给出缓冲大小/采样率/声道数，因为该平台的默认值在本项目里不可靠。
        #[cfg(target_env = "ohos")]
        {
            use sasa::backend::ohos::*;
            AudioManager::new(OhosBackend::new(OhosSettings {
                buffer_size: Some(512),
                sample_rate: Some(48000),
                channels: 2,
            }))
        }
        // 其它平台（桌面/移动端以外的目标）：cpal 后端，使用其默认设置即可。
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        {
            use sasa::backend::cpal::*;
            AudioManager::new(CpalBackend::new(CpalSettings::default()))
        }
    } {
        Ok(manager) => SafeAudio(Some(manager)),
        Err(e) => {
            show_error(e.context(ttl!("audio-backend-init-failed")));
            SafeAudio(Some(AudioManager::new(DummyBackend).expect("Failed to create dummy audio backend, this should not happen")))
        }
    }
}

/// 无输出的兜底音频后端：音频初始化失败时使用，使 UI 音效调用全部变成空操作。
/// 之所以需要它而不是让 `SafeAudio` 为 `None`：这样上层代码无需到处判空，
/// 且“静音”与“初始化失败”在调用点没有语义差别。
struct DummyBackend;

// 实现 sasa 的后端接口：全部立刻成功返回，等价于“永远静音、永不消费数据”的后端。
impl sasa::backend::Backend for DummyBackend {
    /// 接受任意后端设置但不做实际初始化（无资源可建）。
    fn setup(&mut self, setup: sasa::backend::BackendSetup) -> anyhow::Result<()> {
        let _ = setup;
        Ok(())
    }
    /// 无需启动任何音频线程。
    fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// 恒为 `false`：本后端不会“破音”，因此无需上层做重采样/重连处理。
    fn consume_broken(&self) -> bool {
        false
    }
}

// UI 音效资源：一个音频管理器 + 三个音效槽位（大按键音、按键音、列表切换音）。
// 音效句柄用 `RefCell<Option<Sfx>>` 惰性填充：由宿主在加载完资源后写入，
// 未加载时播放调用静默跳过（见 `button_hit` 等），因此 UI 代码不依赖资源就绪顺序。
// 全部放在 thread_local：音频管理器与 GL 一样不是线程安全的，而 UI 只在主线程运行。
thread_local! {
    // 音频管理器。首次访问时惰性创建（可能失败并降级为静音后端）；`cleanup_audio` 在退出前把它取空。
    pub static UI_AUDIO: RefCell<SafeAudio> = RefCell::new(build_audio());
    // 大号按键音（主要操作/确认），音量与应用场景由宿主决定。
    pub static UI_BTN_HITSOUND_LARGE: RefCell<Option<Sfx>> = const { RefCell::new(None) };
    // 普通按键音的句柄，绝大多数按钮点击使用。
    pub static UI_BTN_HITSOUND: RefCell<Option<Sfx>> = const { RefCell::new(None) };
    // 列表/页签切换音。
    pub static UI_SWITCH_SOUND: RefCell<Option<Sfx>> = const { RefCell::new(None) };
}

/// 播放普通按键音（按钮点击）。
/// 音量取自 `UI_SFX_VOLUME`（存的是 f32 的位模式，故用 `from_bits`）；
/// `amplifier` 是播放时的增益参数，这里直接以全局音量为准。
/// 音效未加载或播放失败都静默返回——音效失败不应影响交互，也不值得向上报错。
pub fn button_hit() {
    UI_BTN_HITSOUND.with(|it| {
        if let Some(sfx) = it.borrow_mut().as_mut() {
            let _ = sfx.play(PlaySfxParams {
                amplifier: f32::from_bits(UI_SFX_VOLUME.load(Ordering::Relaxed)),
            });
        }
    });
}

/// 播放大号按键音，用于主操作（开始游戏、确认删除等）。音量规则同 `button_hit`。
pub fn button_hit_large() {
    UI_BTN_HITSOUND_LARGE.with(|it| {
        if let Some(sfx) = it.borrow_mut().as_mut() {
            let _ = sfx.play(PlaySfxParams {
                amplifier: f32::from_bits(UI_SFX_VOLUME.load(Ordering::Relaxed)),
            });
        }
    });
}

/// 播放列表/页签切换音，用于与点击音区分开、提示“视图已切换”。音量规则同 `button_hit`。
pub fn list_switch() {
    UI_SWITCH_SOUND.with(|it| {
        if let Some(sfx) = it.borrow_mut().as_mut() {
            let _ = sfx.play(PlaySfxParams {
                amplifier: f32::from_bits(UI_SFX_VOLUME.load(Ordering::Relaxed)),
            });
        }
    });
}
