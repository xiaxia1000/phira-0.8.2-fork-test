//! RPE 解析器：社区编辑器 **Re:PhiEdit** 导出的 JSON 谱面。
//!
//! RPE 是社区最主流的编辑器格式，结构上是“判定线树 + 事件层”：
//! 顶层有 `META`（含 `RPEVersion` 与 `offset`）、`BPMList`、`judgeLineList`；
//! 每条判定线用 `eventLayers` 数组承载 alpha / 位移 / 旋转 / 速度事件，
//! 用 `extended` 承载颜色 / 文本 / 缩放 / 倾斜 / 绘制 / GIF 等扩展事件。
//!
//! 与官方 PGR 的字段差异（改造时最易踩坑的地方）：
//! - RPE 的事件统一写成 `start`/`end` 两个值 + `startTime`/`endTime` 两个时间，
//!   而不是 PGR 那种一维打包 / `start2` 二维写法；
//! - 事件数组位于判定线内、且按“层”嵌套，需要先摊平再合并；
//! - 大量事件数组是 `Option`，`None`（键缺失）与空数组都表示“无此动画”；
//! - `META.offset` 的单位是**毫秒**，必须除以 1000 才得到秒。
//!
//! 坐标系：RPE 编辑器画布固定为 [`RPE_WIDTH`] × [`RPE_HEIGHT`]，
//! 所有事件坐标都以此为基准，归一化时按这两个常量换算。
//!
//! 版本相关的关键差异见 [`SpeedEasingMode`] 与 [`parse_rpe`]。
use anyhow::{Context, Result};
use image::{codecs::gif, AnimationDecoder, DynamicImage, ImageError};
use macroquad::prelude::{Color, WHITE};
use sasa::AudioClip;
use serde::{Deserialize, Deserializer};
use std::{any::Any, cell::RefCell, collections::HashMap, future::IntoFuture, io::Cursor, rc::Rc, str::FromStr, time::Duration};
use tracing::debug;

use super::{process_lines, L10N_LOCAL, RPE_TWEEN_MAP};
use crate::{
    core::{
        Anim, AnimFloat, AnimVector, BezierTween, BpmList, Chart, ChartExtra, ChartSettings, ClampedTween, CtrlObject, GeneralIntTween, GifFrames,
        HitSoundMap, IntClampedTween, IntStaticTween, JudgeLine, JudgeLineCache, JudgeLineKind, Keyframe, Note, NoteKind, Object, StaticTween,
        Triple, TweenFunction, Tweenable, UIElement, EPS, HEIGHT_RATIO,
    },
    ext::{NotNanExt, SafeTexture},
    fs::FileSystem,
    judge::{HitSound, JudgeStatus},
    parse::ParseWarnings,
};

/// RPE 编辑器画布的固定宽度（像素）。
///
/// Re:PhiEdit 所有事件坐标都以该宽度的中心为原点、以该宽度为满量程，
/// 与运行分辨率无关；解析时用它把事件值换算到项目的归一化坐标 [-1, 1]
/// （横向系数为 `2 / RPE_WIDTH`）。
pub const RPE_WIDTH: f32 = 1350.;
/// RPE 编辑器画布的固定高度（像素），与 [`RPE_WIDTH`] 一起构成 RPE 的固定坐标系。
/// 另外 `line.png` 普通线体的默认缩放也直接取该常量（见 `parse_judge_line`）。
pub const RPE_HEIGHT: f32 = 900.;
/// RPE 速度事件值 → 项目 `height` 的比例系数。
///
/// RPE 速度事件的单位是编辑器自定义的“每拍位移”，与 PGR 的官方速率不同源：
/// `10 / 45` 是 RPE 的速度基准换算，再除以 [`HEIGHT_RATIO`] 才能得到项目的归一化高度。
/// 该系数一旦改动，所有 RPE 谱面的判定线纵向位移都会整体缩放。
const SPEED_RATIO: f64 = 10. / 45. / HEIGHT_RATIO;

/// RPE 的 BPM 变化点（`BPMList` 数组元素）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPEBpmItem {
    /// 从 `start_time` 起生效的 BPM 值，JSON 键 `bpm`。
    bpm: f64,
    /// BPM 生效起点，JSON 键 `startTime`；用 [`Triple`] 精确表示分数拍。
    start_time: Triple,
}

// serde is weird...
/// `#[serde(default = "...")]` 只接受函数路径，故提供 f32 的 0 默认值函数。
fn f32_zero() -> f32 {
    0.
}

/// 同上，提供 f32 的 1 默认值；`easingRight` 缺省 1 表示“整条缓动曲线不裁剪”。
fn f32_one() -> f32 {
    1.
}

/// 提供 i32 的 1 默认值；`easingType` 缺省 1（RPE 的线性编号）。
fn i32_one() -> i32 {
    1
}

/// `RPEVersion` 缺失时的兜底版本号。
///
/// 取 160（即 1.6.0）：这是 RPE 在事件层/扩展事件趋于稳定、但速度缓动仍是
/// Legacy 语义的版本。相比“当作最新版”，退回 160 更安全——老谱面不该被按
/// 1.7.0 的新速度语义解析（见 [`parse_rpe`] 对 [`SpeedEasingMode`] 的选择）。
fn rpe_version_default() -> i32 {
    160
}

/// 宽容地反序列化 `RPEVersion`。
///
/// RPE 历史上把版本号写成数字（`170`）或“数字形态的字符串”（`"170"`）两种，
/// 且老谱面可能完全没有该键。这里接受 null / 数字 / 可解析为 i32 的字符串，
/// 其余情况一律退回 [`rpe_version_default`]，避免因为一个版本字段让整份谱面解析失败。
fn deserialize_rpe_version<'de, D>(deserializer: D) -> std::result::Result<i32, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    let parsed = match value {
        Some(serde_json::Value::Number(v)) => v.as_i64().map(|it| it as i32),
        Some(serde_json::Value::String(s)) => s.parse::<i32>().ok(),
        _ => None,
    };
    Ok(parsed.unwrap_or(rpe_version_default()))
}

/// RPE 的通用双端事件，是 RPE 里最基本的动画单位。
///
/// 泛型 `T` 表示事件值类型：普通事件是 f32，颜色事件是 [`RGBColor`]，文本事件是
/// [`String`]，因此同一个结构体被复用于所有事件数组。
/// RPE 会省略它认为“默认”的字段，因此缺省语义需要靠 `#[serde(default)]` 补齐：
/// 无自定义贝塞尔、缓动编号为线性（1）、缓动区间为整段 [0, 1]。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPEEvent<T = f32> {
    /// 缓动区间左端，JSON 键 `easingLeft`，缺省 0；
    /// 与 `easing_right` 一起把缓动曲线裁剪到子区间（见 [`RPEEvent::tween`]）。
    #[serde(default = "f32_zero")]
    easing_left: f32,
    /// 缓动区间右端，JSON 键 `easingRight`，缺省 1（不裁剪）。
    #[serde(default = "f32_one")]
    easing_right: f32,
    /// 自定义贝塞尔缓动的标识，JSON 键 `bezier`，缺省 0 表示不使用。
    /// 非 0 时忽略 `easing_type`，改用 [`BezierMap`] 里按控制点查到的曲线。
    #[serde(default)]
    bezier: u8,
    /// 贝塞尔控制点，JSON 键 `bezierPoints`，形如 `[p1x, p1y, p2x, p2y]`；
    /// 与 `bezier` 一起构成缓存键（见 [`RPEEvent::bezier_key`]）。
    #[serde(default)]
    bezier_points: [f32; 4],
    /// 缓动编号，JSON 键 `easingType`，缺省 1；经 [`RPE_TWEEN_MAP`] 翻译成项目 [`TweenId`]。
    #[serde(default = "i32_one")]
    easing_type: i32,
    /// 区间起点值，JSON 键 `start`。
    start: T,
    /// 区间终点值，JSON 键 `end`。
    end: T,
    /// 起点拍号，JSON 键 `startTime`。
    start_time: Triple,
    /// 终点拍号，JSON 键 `endTime`。
    end_time: Triple,
}

// 缓动解析：把 RPE 的“编号 + 裁剪区间 + 可选贝塞尔”统一折算成一个可直接喂给
// 关键帧的 TweenFunction，并尽量复用静态实例、避免重复分配。
impl<T> RPEEvent<T> {
    /// 计算该事件的贝塞尔曲线在 [`BezierMap`] 中的查表键。
    ///
    /// `bezierPoints` 是浮点数，直接当 HashMap 键会因精度不一致而查不到，
    /// 因此把前两个分量放大 100 倍取整，拼成一个整数三元组
    /// （p1x、p1y 压进一个 u16，p2x、p2y 各占一个 i16）。
    /// 与 `add_bezier` 使用**同一算法**，二者必须保持一致，否则查表会落空。
    fn bezier_key(&self) -> (u16, i16, i16) {
        let p = &self.bezier_points;
        let int = |p: f32| (p * 100.).round() as i16;
        ((int(p[0]) * 100 + int(p[1])) as u16, int(p[2]), int(p[3]))
    }

    /// 由本事件生成运行期的缓动函数。
    ///
    /// 优先级：`bezier != 0` 时用 [`BezierMap`] 中该控制点对应的贝塞尔曲线；
    /// 否则按 `easing_type`（先 `.max(1)`）查 [`RPE_TWEEN_MAP`] 得到基础曲线。
    /// 只有当缓动区间被真正收窄、且基础曲线不是线性/静止时才包一层 [`ClampedTween`]，
    /// 其余情况复用静态缓动以减少分配。
    /// `left >= right` 这类非法区间同样退回静态曲线，避免下游按零长度区间做除法。
    pub fn tween(&self, bezier_map: &BezierMap) -> Rc<dyn TweenFunction> {
        let tween = RPE_TWEEN_MAP.get(self.easing_type.max(1) as usize).copied().unwrap_or(RPE_TWEEN_MAP[0]);
        let left = self.easing_left.clamp(0., 1.);
        let right = self.easing_right.clamp(0., 1.);
        if self.bezier != 0 {
            Rc::clone(&bezier_map[&self.bezier_key()])
        } else if tween <= 2 || (left.abs() < EPS as f32 && (right - 1.0).abs() < EPS as f32) || left >= right {
            StaticTween::get_rc(tween)
        } else {
            Rc::new(ClampedTween::new(tween, left..right))
        }
    }
}

/// RPE 的控制事件（`posControl` / `sizeControl` / `alphaControl` / `yControl`）。
///
/// 与普通事件不同，控制事件是“时间点 + 一组同名键值”：`#[serde(flatten)]` 把除
/// `easing`/`x` 之外的键收集进 `value`，因此同一条控制事件可以一次性控制多个量
/// （例如同时改 `size` 与 `alpha`）。`x` 是**秒**而非拍，且缓动语义与普通事件相反
/// （见 `parse_ctrl_events`）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPECtrlEvent {
    /// 缓动编号，JSON 键 `easing`；由 [`RPE_TWEEN_MAP`] 翻译。
    easing: u8,
    /// 事件时间，JSON 键 `x`，单位**秒**（不走 BPM 换算）。
    x: f64,
    /// 其余键值对（如 `alpha`、`size`、`pos`、`y`），键名即被控制的量、值为该量在该时刻的取值。
    #[serde(flatten)]
    value: HashMap<String, f32>,
}

/// RPE 的一个事件层：一条判定线可以有多个层，动画按层叠加。
///
/// 每个字段都是 `Option<Vec<...>>`：RPE 只为“有事件的类型”写键，
/// 因此 `None`（键缺失）与 `Some(空数组)` 都要当作“无动画”处理（见 `parse_events`）。
/// 同一类型的事件在多个层之间用 [`Anim::chain`] 合并（同一时刻各层贡献相加）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPEEventLayer {
    /// 不透明度事件，JSON 键 `alphaEvents`；值域 0..=255，换算时才除以 255。
    alpha_events: Option<Vec<RPEEvent>>,
    /// 横向位移事件，JSON 键 `moveXEvents`；单位是 [`RPE_WIDTH`] 像素。
    move_x_events: Option<Vec<RPEEvent>>,
    /// 纵向位移事件，JSON 键 `moveYEvents`；单位是 [`RPE_HEIGHT`] 像素。
    move_y_events: Option<Vec<RPEEvent>>,
    /// 旋转事件，JSON 键 `rotateEvents`；RPE 的旋转正方向与项目相反，解析时整体取负。
    rotate_events: Option<Vec<RPEEvent>>,
    /// 速度事件，JSON 键 `speedEvents`；不直接作为动画，而是被积分成 height
    /// （见 `parse_speed_events`）。
    speed_events: Option<Vec<RPEEvent>>,
}

/// RPE 的颜色事件值：`[r, g, b]` 三个字节。
///
/// 单独定义而不是直接用元组，是为了能实现 [`From`] 转到 macroquad 的 [`Color`]。
/// 保持 0..=255 的字节语义是为了贴近源格式，归一化发生在转换时。
#[derive(Clone, Deserialize)]
struct RGBColor(u8, u8, u8);
// 颜色换算：RPE 只提供 RGB，转成项目颜色时补一个不透明的 alpha = 255。
impl From<RGBColor> for Color {
    /// RGB 三字节 → 项目 `Color`，alpha 固定为不透明。
    fn from(RGBColor(r, g, b): RGBColor) -> Self {
        Self::from_rgba(r, g, b, 255)
    }
}

/// RPE 判定线的 `extended` 扩展事件块，只在新版 RPE 里出现（旧谱面为 `null`/缺失）。
///
/// 与 [`RPEEventLayer`] 的差别：这些事件不属于“动画层”，而是判定线级别的独立属性
/// （颜色、文本、缩放、倾斜、绘制、GIF），且部分事件值类型不是 f32
/// （颜色是 [`RGBColor`]、文本是 [`String`]）。
/// 所有字段都是可选：缺省即“无此扩展动画”。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPEExtendedEvents {
    /// 颜色事件，JSON 键 `colorEvents`；值类型为 RGB。缺省时线体保持白色。
    color_events: Option<Vec<RPEEvent<RGBColor>>>,
    /// 文本事件，JSON 键 `textEvents`；值类型为字符串，用于把判定线渲染成文字。
    text_events: Option<Vec<RPEEvent<String>>>,
    /// 横向缩放事件，JSON 键 `scaleXEvents`。
    scale_x_events: Option<Vec<RPEEvent>>,
    /// 纵向缩放事件，JSON 键 `scaleYEvents`。
    scale_y_events: Option<Vec<RPEEvent>>,
    /// 倾斜事件，JSON 键 `inclineEvents`；单位是角度（渲染时取正弦值参与矩阵变换）。
    incline_events: Option<Vec<RPEEvent>>,
    /// 绘制（着色）事件，JSON 键 `paintEvents`；仅对 `line.png` 线有效（见 `parse_judge_line`）。
    paint_events: Option<Vec<RPEEvent>>,
    /// GIF 帧进度事件，JSON 键 `gifEvents`；仅当线的贴图是 GIF 时有效。
    gif_events: Option<Vec<RPEEvent>>,
}

/// RPE 的单个音符。
///
/// 与官方 PGR 音符相比，RPE 暴露了更多玩家可见细节（大小、透明度、提前显示时长、
/// 染色、判定区宽度），因此字段明显更多；这些字段各有自己的生效条件，
/// 改造时应逐个确认是否被渲染/判定路径使用。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPENote {
    // TODO above == 0? what does that even mean?
    /// 音符类型，JSON 键 `type`：1=Click、2=Hold、3=Flick、4=Drag
    /// （与 PGR 一致，与 PEC 的编号不同）。
    #[serde(rename = "type")]
    kind: u8,
    /// 是否位于判定线上方，JSON 键 `above`：1 = 上方、0 = 下方。
    above: u8,
    /// 命中时间拍号，JSON 键 `startTime`。
    start_time: Triple,
    /// Hold 的结束时间拍号，JSON 键 `endTime`；非 Hold 音符无意义。
    end_time: Triple,
    /// 横向位置，JSON 键 `positionX`；单位是 [`RPE_WIDTH`] 像素。
    position_x: f32,
    /// 相对判定线的纵向偏移，JSON 键 `yOffset`；单位是 [`RPE_HEIGHT`] 像素。
    y_offset: f32,
    /// 音符不透明度，JSON 键 `alpha`；注释保留：实际谱面可能出现 256，
    /// 超出 255 的部分在解析时被夹住（见 `parse_notes`）。
    alpha: u16,               // some alpha has 256...
    /// 自定义打击音效文件名，JSON 键 `hitsound`；`Some` 时按文件名加载或匹配内置音。
    hitsound: Option<String>, // TODO implement this feature
    /// 音符缩放，JSON 键 `size`；同时作用于 x/y 两轴。
    size: f32,
    /// 音符移动速度倍率，JSON 键 `speed`；同时会放大 `yOffset`（见 `parse_notes`）。
    speed: f32,
    /// 是否为假音符（不参与判定），JSON 键 `isFake`：非 0 即假。
    is_fake: u8,
    /// 提前可见的时长（秒），JSON 键 `visibleTime`；大于 0 时音符会提前淡入。
    visible_time: f64,
    /// 音符本体染色，JSON 键 `tint`；缺省为白色。
    #[serde(default)]
    tint: Option<[u8; 3]>,
    /// 判定特效染色，JSON 键 `tintHitEffects`；缺省表示使用默认特效色。
    #[serde(default)]
    tint_hit_effects: Option<[u8; 3]>,
    /// 判定区宽度倍率，JSON 键 `judgeArea`；缺省 1.0（标准宽度）。
    #[serde(default)]
    judge_area: Option<f32>,
}

/// RPE 的一条判定线。
///
/// 字段名映射注意：`Name`、`Texture`、`father`、`attachUI` 几个键的大小写不符合
/// serde 的 camelCase 规则，因此逐个显式 `rename`。
/// `father` 是父判定线下标（-1 表示无父），用于构建判定线树；父子关系是解析期
/// 最危险的部分，必须做环路检测（见 `parse_rpe` 内的 `has_cycle`）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPEJudgeLine {
    // TODO group
    // TODO bpmfactor
    /// 判定线名字，JSON 键 `Name`（首字母大写）；仅用于报错/调试定位。
    #[serde(rename = "Name")]
    name: String,
    /// 判定线贴图文件名，JSON 键 `Texture`；特殊值 `line.png` 表示普通线体而非图片，
    /// 会走不同的缩放与类型分支（见 `parse_judge_line`）。
    #[serde(rename = "Texture")]
    texture: String,
    /// 父判定线下标，JSON 键 `father`；-1 或缺省表示无父。
    #[serde(rename = "father")]
    parent: Option<isize>,
    /// 是否随父线旋转，JSON 键 `rotateWithFather`；缺省 false（只继承父线位置）。
    rotate_with_father: Option<bool>,
    /// 事件层数组，JSON 键 `eventLayers`；元素可能为 `null`，解析时用 `flatten` 过滤。
    event_layers: Vec<Option<RPEEventLayer>>,
    /// 扩展事件块，JSON 键 `extended`；老版本 RPE 没有。
    extended: Option<RPEExtendedEvents>,
    /// 音符数组，JSON 键 `notes`；缺省表示该线没有音符。
    notes: Option<Vec<RPENote>>,
    /// 是否为“遮挡线”（cover），JSON 键 `isCover`：等于 1 时只画线上方（`show_below` 置 false）。
    is_cover: u8,
    /// 绘制层级，JSON 键 `zOrder`，缺省 0；值越大越靠前。
    #[serde(default)]
    z_order: i32,
    /// 挂在判定线上的 UI 元素，JSON 键 `attachUI`；缺省表示不挂 UI。
    #[serde(rename = "attachUI")]
    attach_ui: Option<UIElement>,

    /// 位置控制事件，JSON 键 `posControl`；缺省为空。
    #[serde(default)]
    pos_control: Vec<RPECtrlEvent>,
    /// 尺寸控制事件，JSON 键 `sizeControl`；缺省为空。
    #[serde(default)]
    size_control: Vec<RPECtrlEvent>,
    /// 不透明度控制事件，JSON 键 `alphaControl`；缺省为空。
    #[serde(default)]
    alpha_control: Vec<RPECtrlEvent>,
    /// 纵向控制事件，JSON 键 `yControl`；缺省为空。
    #[serde(default)]
    y_control: Vec<RPECtrlEvent>,
}

/// RPE 顶层的 `META` 块。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPEMetadata {
    /// 谱面全局偏移，JSON 键 `offset`，单位**毫秒**；解析时除以 1000 转成秒。
    offset: i32,
    /// RPE 格式版本，JSON 键 `RPEVersion`；决定速度缓动语义（>= 170 用新算法，见 `parse_rpe`）。
    #[serde(rename = "RPEVersion", default = "rpe_version_default", deserialize_with = "deserialize_rpe_version")]
    rpe_version: i32,
}

/// RPE 速度事件的缓动积分模式，按 `RPEVersion` 选择。
#[derive(Copy, Clone)]
enum SpeedEasingMode {
    /// 1.7.0 之前的语义：缓动直接作用于“速度-时间”曲线，
    /// 需要用首尾导数反解线性系数再积分（见 `speed_segment_tween`）。
    Legacy,
    /// 1.7.0 起的语义：缓动作用于“缓动曲线的积分”，改用数值积分方式等价实现。
    /// RPE 1.7.0 修改了速度事件的数学定义，同一份谱面用旧公式会算出不同位移，
    /// 因此必须按版本切分而不能只保留一种实现。
    Modern,
}

/// 把“速度缓动”包装成一条可直接求值的位移进度曲线。
///
/// 速度事件的位移是“速度对时间”的积分，而关键帧模型里每个关键帧只能挂一条缓动曲线。
/// 因此这里把速度曲线 `k * f(x) + b`（`f` 为原缓动，`k`/`b` 由首尾速度反解）
/// 先对 x 在 [0, 1] 上积分、再归一化，得到一条“位移进度”曲线，
/// 于是一条普通缓动即可表示积分后的效果。
struct SpeedIntegralTween {
    /// 原始速度缓动曲线 `f`（Legacy 模式是原缓动本身，Modern 模式是缓动的积分形式）。
    tween: Rc<dyn TweenFunction>,
    /// 速度随归一化时间 x 变化的斜率。
    k: f32,
    /// 速度在 x = 0 处的截距。
    b: f32,
    /// [0, 1] 上的积分总量，用于把 partial 归一化到 [0, 1]。
    total: f32,
}

// 构造与部分积分：`try_create` 在积分退化（非有限或过小）时返回 None，
// 让调用方退回线性近似，避免后续除以 0。
impl SpeedIntegralTween {
    /// 尝试构造积分缓动，返回 `(缓动, [0,1] 上的积分总量)`。
    ///
    /// 当总积分为非有限值或其绝对值小于 [`EPS`] 时返回 `None`：
    /// 这种退化情形无法归一化（会得到 NaN/无穷的速度曲线），调用方改用线性近似。
    fn try_create(tween: Rc<dyn TweenFunction>, k: f32, b: f32) -> Option<(Rc<dyn TweenFunction>, f32)> {
        let mut result = Self { tween, k, b, total: 0. };
        let total = result.partial(1.);
        if !total.is_finite() || total.abs() < EPS as f32 {
            return None;
        }
        result.total = total;
        Some((Rc::new(result), total))
    }

    /// 速度曲线在 x 处的**部分积分**（尚未归一化）。
    ///
    /// `tween.y(x)` 是缓动（或其积分形式）在 x 处的值，乘 `k` 是变化分量，
    /// `b * x` 是恒定速度分量对时间的积分。
    fn partial(&self, x: f32) -> f32 {
        self.tween.y(x) * self.k + self.b * x
    }
}

// 对外表现为普通缓动：把部分积分归一化到 [0, 1]，并把定义域端点夹住，
// 保证 y(0) = 0、y(1) = 1；`total` 异常时退回线性，避免输出 NaN。
impl TweenFunction for SpeedIntegralTween {
    /// 归一化后的位移进度：x <= 0 返回 0、x >= 1 返回 1，
    /// 中间按 `partial / total` 归一化；结果非有限时退回线性 `x`。
    fn y(&self, x: f32) -> f32 {
        if x <= 0. {
            return 0.;
        }
        if x >= 1. {
            return 1.;
        }
        let y = self.partial(x) / self.total;
        if y.is_finite() {
            y
        } else {
            x
        }
    }

    /// 向下转型入口，供 `speed_segment_tween` 判断缓动是否已是积分形式（避免二次积分）。
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 在给定首尾速度下，构造“速度线性变化”的等价缓动。
///
/// 速度从 `start_speed` 线性变化到 `end_speed` 时，位移关于时间是二次的，
/// 因此用 quadOut / quadIn 并配合 [0, 1] 的子区间裁剪来精确表示：
/// - 首尾速度相同 → 退化为线性（编号 2）；
/// - 起始速度绝对值更大 → 位移加速变慢，取 quadOut 的 `[0, 1 - end/start)` 段；
/// - 否则 → 取 quadIn 的 `[start/end, 1)` 段。
/// 子区间端点由速度比决定，正是“只取二次曲线的一段”所对应的位置。
fn speed_linear_tween(start_speed: f32, end_speed: f32) -> Rc<dyn TweenFunction> {
    if (start_speed - end_speed).abs() < EPS as f32 {
        StaticTween::get_rc(2)
    } else if start_speed.abs() > end_speed.abs() {
        Rc::new(ClampedTween::new(7 /*quadOut*/, 0.0..(1. - end_speed / start_speed)))
    } else {
        Rc::new(ClampedTween::new(6 /*quadIn*/, (start_speed / end_speed)..1.))
    }
}

/// 把一段速度事件折算成 `(缓动函数, 该段平均速度)`。
///
/// 返回的平均速度供 `parse_speed_events` 累加高度用，缓动用于让高度在段内
/// 按真实积分曲线变化。两种模式对应 RPE 前后两套数学定义：
/// - `Legacy`：假设缓动作用于**速度曲线**。用首尾导数反解线性系数 `k`/`b`
///   （速度被表示为 `k * f'(x) + b`），再对 [0, 1] 积分。
///   若首尾导数差过小（缓动在该段近似恒定），则退回线性速度公式。
/// - `Modern`：1.7.0 起缓动作用于**缓动曲线的积分**，因此先把缓动换成它的积分形式
///   （按静态/裁剪/普通三类分别转成 `IntStaticTween`/`IntClampedTween`/`GeneralIntTween`），
///   再以 `k = end - start`、`b = start` 做积分。
///
/// 任何退化情形（积分无法归一化）都退回 [`speed_linear_tween`]，
/// 并把段内平均速度取首尾均值。
fn speed_segment_tween(mode: SpeedEasingMode, start_speed: f32, end_speed: f32, tween: Rc<dyn TweenFunction>) -> (Rc<dyn TweenFunction>, f32) {
    let (tween, total) = match mode {
        SpeedEasingMode::Legacy => {
            let df0 = tween.derivative(0.);
            let df1 = tween.derivative(1.);
            let denom = df1 - df0;
            if !denom.is_finite() || denom.abs() < 1e-8 {
                return (speed_linear_tween(start_speed, end_speed), (start_speed + end_speed) / 2.);
            }
            let k = (end_speed - start_speed) / denom;
            let b = start_speed - k * df0;
            SpeedIntegralTween::try_create(tween, k, b)
        }
        SpeedEasingMode::Modern => {
            let int_tween: Rc<dyn TweenFunction> = if let Some(s) = tween.as_any().downcast_ref::<StaticTween>() {
                IntStaticTween::get_rc(s.0)
            } else if let Some(s) = tween.as_any().downcast_ref::<ClampedTween>() {
                Rc::new(IntClampedTween::new(s.0, s.1.clone()))
            } else {
                Rc::new(GeneralIntTween::new(tween))
            };
            SpeedIntegralTween::try_create(int_tween, end_speed - start_speed, start_speed)
        }
    }
    .unwrap_or_else(|| (speed_linear_tween(start_speed, end_speed), (start_speed + end_speed) / 2.));
    (tween, total)
}

/// RPE 谱面的顶层结构。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RPEChart {
    /// `META` 块，JSON 键 `META`（全大写，需显式 `rename`）；含偏移量与格式版本。
    #[serde(rename = "META")]
    meta: RPEMetadata,
    /// BPM 变化列表，JSON 键 `BPMList`（大写缩写前缀，需显式 `rename`）。
    #[serde(rename = "BPMList")]
    bpm_list: Vec<RPEBpmItem>,
    /// 判定线列表，JSON 键 `judgeLineList`；顺序即解析/索引顺序，被 `father` 引用。
    judge_line_list: Vec<RPEJudgeLine>,
}

/// 自定义贝塞尔缓动的缓存表：键为 `RPEEvent::bezier_key` 算出的控制点整数编码。
///
/// RPE 允许成千上万个事件各带 `bezierPoints`，但相同控制点会被反复使用；
/// 预先扫描全谱建表（见 `get_bezier_map`）可让解析时只做一次 HashMap 查询，
/// 避免为每个事件都构造一条贝塞尔曲线——`BezierTween` 会预采样查找表，构造成本不低。
type BezierMap = HashMap<(u16, i16, i16), Rc<dyn TweenFunction>>;

/// 把 RPE 的一组事件转成项目的关键帧动画。
///
/// 与 extra.json 的处理同源：可选地用 `default` 补一个 0 时刻的起始关键帧
/// （当首个事件不在 0 拍时，避免时间轴开头无定义、取值落到默认值）。
/// 每个事件压入两个关键帧：起点（带该事件的缓动）与终点（缓动 0，
/// 因为段内曲线已由起点的缓动描述）。
///
/// # Arguments
/// * `r` - BPM 表，把拍号折算成秒；
/// * `rpe` - 事件数组，**假定已按时间排序**；
/// * `default` - 需要补 0 时刻起始值时的取值；
/// * `bezier_map` - 贝塞尔曲线缓存，供 `RPEEvent::tween` 查表。
///
/// 空数组返回 [`Anim::default()`]（语义为“无动画”），而不是固定 0，
/// 以便上层用 `Anim::is_default` 区分“没写事件”与“写了 0”。
fn parse_events<T: Tweenable, V: Clone + Into<T>>(
    r: &mut BpmList,
    rpe: &[RPEEvent<V>],
    default: Option<T>,
    bezier_map: &BezierMap,
) -> Result<Anim<T>> {
    if rpe.is_empty() {
        return Ok(Anim::default());
    }
    let mut kfs = Vec::new();
    if let Some(default) = default {
        if rpe[0].start_time.beats() != 0.0 {
            kfs.push(Keyframe::new(0.0, default, 0));
        }
    }
    for e in rpe {
        kfs.push(Keyframe {
            time: r.time(&e.start_time),
            value: e.start.clone().into(),
            tween: e.tween(bezier_map),
        });
        kfs.push(Keyframe::new(r.time(&e.end_time), e.end.clone().into(), 0));
    }
    Ok(Anim::new(kfs))
}

/// 解析 RPE 速度事件层并积分成判定线 `height`（**现代积分算法**，对应 RPE >= 1.7.0）。
///
/// RPE 允许多个事件层同时写速度，语义是各层叠加；这里对每层分别积分成一条高度曲线，
/// 最后用 [`AnimFloat::chain`] 把各层相加。
///
/// # Arguments
/// * `r` - BPM 表；
/// * `rpe` - 判定线的全部事件层（`None` 层被忽略）；
/// * `bezier_map` - 贝塞尔缓存；
/// * `max_time` - 谱面结束时间（秒），用于补尾；
/// * `mode` - 速度缓动语义（Legacy/Modern），由 `RPEVersion` 决定。
///
/// 一层内的处理：先按起点时间排序事件；`push_kf` 负责在同一时刻合并关键帧并把
/// 段位移累加进 `height`；随后逐段判断积分方式（匀速、速度过零、带缓动），
/// 最后补齐到 `max_time`。速度过零必须把段拆成两半——否则梯形积分的位移非单调。
fn parse_speed_events(r: &mut BpmList, rpe: &[RPEEventLayer], bezier_map: &BezierMap, max_time: f64, mode: SpeedEasingMode) -> Result<AnimFloat> {
    // 阶段 1：收集所有带速度事件的层；一层都没有即为“无动画”。
    let layers: Vec<_> = rpe.iter().filter_map(|it| it.speed_events.as_ref()).collect();
    if layers.is_empty() {
        return Ok(AnimFloat::default());
    }
    let mut anis = Vec::new();
    // 阶段 2：逐层独立积分，后面再 chain 相加。
    for layer in layers {
        if layer.is_empty() {
            continue;
        }
        let mut events = layer.iter().collect::<Vec<_>>();
        events.sort_by_key(|it| it.start_time.beats().not_nan());

        let mut kfs = vec![Keyframe::new(0.0, 0.0, 2)];
        let mut height = 0f64;
        // 写入 [start_time, end_time] 这一段：起点关键帧复用/追加，随后把
        // “平均速度 × 段长”累加进 height。同一时刻已有末帧时直接改写它，
        // 避免产生零长度区间导致的时间不单调。
        let mut push_kf = |start_time: f64, end_time: f64, tween: Rc<dyn TweenFunction>, factor: f32| {
            if end_time - start_time <= EPS {
                return;
            }
            if let Some(last) = kfs.last_mut() {
                if (last.time - start_time).abs() < EPS {
                    last.value = height as f32;
                    last.tween = tween;
                } else {
                    kfs.push(Keyframe {
                        time: start_time,
                        value: height as f32,
                        tween,
                    });
                }
            }
            height += factor as f64 * (end_time - start_time);
        };

        // 阶段 3：按时间推进游标，把 [cursor, max_time] 切成若干关键帧。
        let mut cursor = 0.0;
        let mut last_speed = 0.0;
        for event in events {
            let start_time = r.time(&event.start_time).max(cursor);
            let end_time = r.time(&event.end_time).max(start_time);
            let start_speed = event.start * SPEED_RATIO as f32;
            let end_speed = event.end * SPEED_RATIO as f32;

            push_kf(cursor, start_time, StaticTween::get_rc(2), last_speed);
            if end_time > start_time + EPS {
                if event.easing_type == 0 {
                    push_kf(start_time, end_time, StaticTween::get_rc(2), start_speed);
                } else if event.easing_type <= 1 {
                    // 线性速度（easing_type 0/1）：速度变号时必须先求出过零时刻、
                    // 把区间拆成两段分别积分，否则梯形积分会在过零点算出非单调位移。
                    if start_speed * end_speed < 0. {
                        let x = start_speed / (start_speed - end_speed);
                        let mid = f64::tween(&start_time, &end_time, x);
                        for (start_time, end_time, start, end) in [(start_time, mid, start_speed, 0.), (mid, end_time, 0., end_speed)] {
                            let factor = start.midpoint(end);
                            let tween = speed_linear_tween(start, end);
                            push_kf(start_time, end_time, tween, factor);
                        }
                    } else {
                        let factor = start_speed.midpoint(end_speed);
                        let tween = speed_linear_tween(start_speed, end_speed);
                        push_kf(start_time, end_time, tween, factor);
                    }
                } else {
                    let (tween, factor) = speed_segment_tween(mode, start_speed, end_speed, event.tween(bezier_map));
                    push_kf(start_time, end_time, tween, factor);
                }
            }
            cursor = end_time;
            last_speed = end_speed;
        }

        // 收尾：最后一段以末端速度匀速延续到谱面结束，并确保末尾存在关键帧
        // （否则 Anim 在 max_time 之后无定义）。
        push_kf(cursor, max_time, StaticTween::get_rc(2), last_speed);
        if let Some(last) = kfs.last() {
            if (last.time - max_time).abs() > EPS {
                kfs.push(Keyframe::new(max_time, height as f32, 0));
            }
        }
        anis.push(AnimFloat::new(kfs));
    }
    if anis.is_empty() {
        return Ok(AnimFloat::default());
    }
    Ok(AnimFloat::chain(anis))
}

/// 旧版 RPE（< 1.7.0）的速度事件解析与积分。
///
/// 旧语义下速度事件直接给出“速度值曲线”，位移 = 速度对时间积分。做法是：
/// 先把各层速度曲线合并（`chain` 相加）、把单位换算成项目比例；
/// 再收集所有关键帧时间点与**速度过零点**，排序去重后逐段做梯形积分累加高度。
/// 过零点必须插入——梯形公式 `(speed + end_speed) / 2 * dt` 在变号段会低估位移。
///
/// 注意段内末速度的查询用 `end_time - 1e-4` 而不是 `end_time`：
/// 用右端点会命中下一段的关键帧，把 Hold 型缓动（恒 0）误判为线性缓动。
fn parse_speed_events_legacy(r: &mut BpmList, rpe: &[RPEEventLayer], max_time: f64) -> Result<AnimFloat> {
    let rpe: Vec<_> = rpe.iter().filter_map(|it| it.speed_events.as_ref()).collect();
    if rpe.is_empty() {
        // TODO or is it?
        return Ok(AnimFloat::default());
    };
    let anis: Vec<_> = rpe
        .into_iter()
        .filter_map(|it| {
            if it.is_empty() {
                return None;
            }
            let mut kfs = Vec::new();
            for e in it {
                kfs.push(Keyframe::new(r.time(&e.start_time), e.start, 2));
                kfs.push(Keyframe::new(r.time(&e.end_time), e.end, 0));
            }
            Some(AnimFloat::new(kfs))
        })
        .collect();
    if anis.is_empty() {
        return Ok(AnimFloat::default());
    }
    let mut pts: Vec<_> = anis.iter().flat_map(|it| it.keyframes.iter().map(|it| it.time.not_nan())).collect();
    pts.push(max_time.not_nan());
    pts.sort();
    pts.dedup();
    let mut sani = AnimFloat::chain(anis);
    sani.map_value(|v| v * SPEED_RATIO as f32);
    for i in 0..(pts.len() - 1) {
        let now_time = *pts[i];
        let end_time = *pts[i + 1];
        sani.set_time(now_time);
        let speed = sani.now();
        sani.set_time(end_time - 1e-4);
        let end_speed = sani.now();
        // 速度变号的段要额外插入过零点，保证每段内速度不变号（梯形积分才准确）。
        if speed.signum() * end_speed.signum() < 0. {
            pts.push(f64::tween(&now_time, &end_time, speed / (speed - end_speed)).not_nan());
        }
    }
    pts.sort();
    pts.dedup();
    // 逐段梯形积分：每段用首尾速度的均值 × 段长累加高度，
    // 并按“速度是否单调/是否变号”选择对应的二次缓动来贴合真实位移曲线。
    let mut kfs = Vec::new();
    let mut height = 0f64;
    for i in 0..(pts.len() - 1) {
        let now_time = *pts[i];
        let end_time = *pts[i + 1];
        sani.set_time(now_time);
        let speed = sani.now();
        // this can affect a lot! do not use end_time...
        // using end_time causes Hold tween (x |-> 0) to be recognized as Linear tween (x |-> x)
        sani.set_time(end_time - 1e-4);
        let end_speed = sani.now();
        kfs.push(if (speed - end_speed).abs() < EPS as f32 {
            Keyframe::new(now_time, height as f32, 2)
        } else if speed.abs() > end_speed.abs() {
            Keyframe {
                time: now_time,
                value: height as f32,
                tween: Rc::new(ClampedTween::new(7 /*quadOut*/, 0.0..(1. - end_speed / speed))),
            }
        } else {
            Keyframe {
                time: now_time,
                value: height as f32,
                tween: Rc::new(ClampedTween::new(6 /*quadIn*/, (speed / end_speed)..1.)),
            }
        });
        height += (speed + end_speed) as f64 * (end_time - now_time) / 2.;
    }
    if kfs.is_empty() {
        return Ok(Anim::default());
    }
    kfs.push(Keyframe::new(max_time, height as f32, 0));
    Ok(AnimFloat::new(kfs))
}

/// 为 GIF 类型的判定线解析帧进度事件。
///
/// 输出是一条 [0, 1] 的“播放进度”动画：0 表示帧序列从头开始、1 表示播完一轮。
/// 生成方式是按 GIF 自身总时长（`gif.total_time()`，单位毫秒）反复插入
/// “一轮结束(1) → 下一轮开始(0)”的关键帧对，使静止期间 GIF 也能持续循环；
/// 每个事件则把当前进度对齐到该事件的 `startTime`/`endTime`。
///
/// # Arguments
/// * `r` - BPM 表（事件时间是拍号）；
/// * `rpe` - GIF 帧进度事件数组；
/// * `bezier_map` - 贝塞尔缓存；
/// * `gif` - 已解码的帧序列，用于取总时长。
///
/// 末尾补帧只补到恒定上限（见函数内的 `GIF_MAX_TIME`）：GIF 是循环素材，
/// 谱面事件很少时无限补循环点没有意义。
fn parse_gif_events<V: Clone + Into<f32>>(r: &mut BpmList, rpe: &[RPEEvent<V>], bezier_map: &BezierMap, gif: &GifFrames) -> Result<Anim<f32>> {
    let mut kfs = Vec::new();
    kfs.push(Keyframe::new(0.0, 0.0, 2));
    let mut next_rep_time: u128 = 0;
    for e in rpe {
        while r.time(&e.start_time) > next_rep_time as f64 / 1000. {
            kfs.push(Keyframe::new(next_rep_time as f64 / 1000., 1.0, 0));
            kfs.push(Keyframe::new(next_rep_time as f64 / 1000., 0.0, 2));
            next_rep_time += gif.total_time();
        }
        let stop_prog = 1. - (next_rep_time as f64 - r.time(&e.start_time) * 1000.) / gif.total_time() as f64;
        kfs.push(Keyframe::new(r.time(&e.start_time), stop_prog as f32, 0));
        kfs.push(Keyframe {
            time: r.time(&e.start_time),
            value: e.start.clone().into(),
            tween: e.tween(bezier_map),
        });
        kfs.push(Keyframe::new(r.time(&e.end_time), e.end.clone().into(), 2));
        next_rep_time = (r.time(&e.end_time) * 1000. + gif.total_time() as f64 * (1. - e.end.clone().into()) as f64).round() as u128;
    }

    // TODO maybe a better approach?
    // 补帧上限（毫秒）：只保证在谱面事件稀疏时进度动画仍覆盖前 2 秒，
    // 再往后渲染端已不再关心（GIF 是循环的，后续循环点等价）。
    const GIF_MAX_TIME: f64 = 2000.;
    while GIF_MAX_TIME > next_rep_time as f64 / 1000. {
        kfs.push(Keyframe::new(next_rep_time as f64 / 1000., 1.0, 0));
        kfs.push(Keyframe::new(next_rep_time as f64 / 1000., 0.0, 2));
        next_rep_time += gif.total_time();
    }
    if kfs.is_empty() {
        return Ok(Anim::default());
    }
    Ok(Anim::new(kfs))
}

/// 解析 RPE 音符（异步：可能要按 `hitsound` 从虚拟文件系统加载自定义打击音）。
///
/// # Arguments
/// * `r` - BPM 表（音符时间是拍号）；
/// * `rpe` - 音符数组；
/// * `fs` - 虚拟文件系统，用于按文件名加载自定义音效；
/// * `height` - 判定线高度动画，就地取不同时刻的 height 写入音符；
/// * `hitsounds` - 全局音效缓存，避免同一自定义音效被反复解码。
///
/// 关键换算与语义：
/// - `yOffset` 乘 `2 / RPE_HEIGHT * speed`：除归一化外还要乘音符自身 speed，
///   因为 RPE 里音符速度也会放大它与判定线的纵向偏移；
/// - `alpha` 上限 255，但源格式可能写 256，故取 `min(255)`；
/// - `visibleTime`：若“可见时间”晚于命中时间则无需淡入；否则生成一条
///   从 0 渐变到目标 alpha 的两帧动画，实现提前出现的效果；
/// - Hold 的 `end_height` 取结束时刻的 height。
///
/// # Errors
/// 音符类型不在 1..=4、自定义音效加载失败、或自定义音效名无法转成
/// [`HitSound::Custom`] 的标识时返回错误。
async fn parse_notes(
    r: &mut BpmList,
    rpe: Vec<RPENote>,
    fs: &mut dyn FileSystem,
    height: &mut AnimFloat,
    hitsounds: &mut HitSoundMap,
) -> Result<Vec<Note>> {
    // 逐个音符：先取时间与高度，再据类型构造 NoteKind，最后确定打击音与外观。
    let mut notes = Vec::new();
    for note in rpe {
        let time = r.time(&note.start_time);
        height.set_time(time);
        let note_height = height.now();
        // 纵向偏移归一化，并按 RPE 语义再乘音符自身的 speed。
        let y_offset = note.y_offset * 2. / RPE_HEIGHT * note.speed;
        let kind = match note.kind {
            1 => NoteKind::Click,
            2 => {
                let end_time = r.time(&note.end_time);
                height.set_time(end_time);
                NoteKind::Hold {
                    end_time,
                    end_height: height.now() as f64,
                }
            }
            3 => NoteKind::Flick,
            4 => NoteKind::Drag,
            _ => ptl!(bail "unknown-note-type", "type" => note.kind),
        };
        let hitsound = match note.hitsound {
            Some(s) => {
                // TODO: RPE doc needed...
                // RPE 把三个内置音效直接写成固定文件名（flick/tap/drag），
                // 其余名字才视为谱面包内的自定义音效文件，需按缓存/磁盘加载。
                if s == "flick.mp3" {
                    HitSound::Flick
                } else if s == "tap.mp3" {
                    HitSound::Click
                } else if s == "drag.mp3" {
                    HitSound::Drag
                } else {
                    if hitsounds.get(&s).is_none() {
                        let data = fs.load_file(&s).await;
                        if let Ok(data) = data {
                            hitsounds.insert(s.clone(), AudioClip::new(data)?);
                        } else {
                            ptl!(bail "hitsound-missing", "name" => s);
                        }
                    }
                    HitSound::Custom(String::from_str(&s)?)
                }
            }
            None => HitSound::default_from_kind(&kind),
        };
        notes.push(Note {
            object: Object {
                // visibleTime 语义：命中前 visibleTime 秒内应提前淡入。
                // 若可见时间不早于命中时间则视为无需淡入，直接取目标 alpha
                // （等于 255 时用 default 表示“不透明、无动画”）。
                alpha: if note.visible_time >= time {
                    if note.alpha >= 255 {
                        AnimFloat::default()
                    } else {
                        AnimFloat::fixed(note.alpha as f32 / 255.)
                    }
                } else {
                    let alpha = note.alpha.min(255) as f32 / 255.;
                    AnimFloat::new(vec![Keyframe::new(0.0, 0.0, 0), Keyframe::new(time - note.visible_time, alpha, 0)])
                },
                translation: AnimVector(AnimFloat::fixed(note.position_x / (RPE_WIDTH / 2.)), AnimFloat::fixed(y_offset)),
                scale: AnimVector(AnimFloat::fixed(note.size), AnimFloat::fixed(note.size)),
                ..Default::default()
            },
            kind,
            hitsound,
            time,
            height: note_height as f64,
            speed: note.speed as f64,
            color: note.tint.map_or(WHITE, |[r, g, b]| Color::from_rgba(r, g, b, 255)),
            fx_color: note.tint_hit_effects.map(|[r, g, b]| Color::from_rgba(r, g, b, 255)),
            judge_area: note.judge_area.unwrap_or(1.0),

            above: note.above == 1,
            multiple_hint: false,
            fake: note.is_fake != 0,
            judge: JudgeStatus::NotJudged,
        })
    }
    Ok(notes)
}

/// 解析 RPE 的控制事件（`posControl` 等）为动画。
///
/// 两个与普通事件不同的约定：
/// - **缓动语义相反**：普通事件的缓动作用于“从该事件出发的区间”，
///   而控制事件的缓动作用于“到达该事件时间点之前的区间”，
///   所以这里把缓动整体后移一位（第 i 个关键帧取第 i+1 个事件的缓动，末尾补 0）。
///   这不是笔误，改动会让整条控制曲线错位。
/// - **时间单位是秒**：`x` 不再经 BPM 表换算。
///
/// 特殊情形：只有一个“easing = 1 且值为 1”的事件时视为默认值，返回
/// [`AnimFloat::default()`]；空数组同样返回默认（都表示“无控制动画”）。
///
/// # Arguments
/// * `rpe` - 控制事件数组；
/// * `key` - 从 `value` map 中取哪个量（如 `"alpha"`、`"size"`）。
fn parse_ctrl_events(rpe: &[RPECtrlEvent], key: &str) -> AnimFloat {
    let vals: Vec<_> = rpe.iter().map(|it| it.value[key]).collect();
    if rpe.is_empty() || (rpe.len() == 2 && rpe[0].easing == 1 && (vals[0] - 1.).abs() < 1e-4) {
        return AnimFloat::default();
    }
    // In RPE, each control event's easing governs the interval ending at that
    // event's x, not starting from it. The Anim system uses kf[i].tween for
    // the interval [kf[i], kf[i+1]], so we shift the tween assignment: each
    // keyframe gets the tween from the next event.
    let tweens: Vec<Rc<dyn TweenFunction>> = rpe
        .iter()
        .skip(1)
        .map(|it| StaticTween::get_rc(RPE_TWEEN_MAP.get(it.easing.max(1) as usize).copied().unwrap_or(RPE_TWEEN_MAP[0])))
        .chain(std::iter::once(StaticTween::get_rc(0)))
        .collect();
    AnimFloat::new(
        rpe.iter()
            .zip(vals)
            .zip(tweens)
            .map(|((it, val), tween)| Keyframe {
                time: it.x,
                value: val,
                tween,
            })
            .collect(),
    )
}

/// 把一条 RPE 判定线解析成项目的 [`JudgeLine`]。
///
/// # Arguments
/// * `r` - BPM 表（把事件拍号换算成秒）；
/// * `rpe` - 判定线描述；
/// * `max_time` - 谱面结束时间，用于速度积分补尾；
/// * `speed_mode` - 速度缓动模式（由 `RPEVersion` 决定）；
/// * `fs` - 虚拟文件系统，用于加载贴图/GIF/音效；
/// * `use_rpe_170_speed` - 用户开关：是否采用 1.7.0 的新速度积分实现；
///   为 false 时无论版本都退回旧算法，用于出错时兼容/对照；
/// * `bezier_map` - 贝塞尔缓存；
/// * `hitsounds` - 音效缓存（跨判定线共用）；
/// * `line_texture_map` - 贴图缓存，同一张图被多条线引用时只解码一次。
///
/// 事件层先摊平成 [`RPEEventLayer`] 列表，再用 `events_with_factor` 统一做
/// “多层合并 + 单位换算”：alpha 除以 255（值域 0..=255）、旋转整体取负
/// （RPE 旋转正方向与项目相反）、位移按 [`RPE_WIDTH`]/[`RPE_HEIGHT`] 归一化。
///
/// `texture` 决定 `JudgeLineKind`：`line.png` 是普通线体（可进一步变成
/// Text / Paint 线），其它文件名走图片/GIF 分支。
///
/// # Errors
/// 各类事件解析失败、贴图/GIF/音效加载失败时返回错误。
#[allow(clippy::too_many_arguments)]
async fn parse_judge_line(
    r: &mut BpmList,
    rpe: RPEJudgeLine,
    max_time: f64,
    speed_mode: SpeedEasingMode,
    fs: &mut dyn FileSystem,
    use_rpe_170_speed: bool,
    bezier_map: &BezierMap,
    hitsounds: &mut HitSoundMap,
    line_texture_map: &mut HashMap<String, SafeTexture>,
) -> Result<JudgeLine> {
    // 摊平事件层：RPE 用 null 表示空层，flatten 后只保留真实存在的层。
    let event_layers: Vec<_> = rpe.event_layers.into_iter().flatten().collect();
    // 内部工具：把某一类事件在所有层里合并成一个动画，并统一乘一个换算系数。
    // 缺省语义：如果合并后仍是“无动画”（各层都没写这类事件），返回固定 0.0，
    // 而不是空动画——调用方需要的是“明确的不变值”。
    fn events_with_factor(
        r: &mut BpmList,
        event_layers: &[RPEEventLayer],
        get: impl Fn(&RPEEventLayer) -> &Option<Vec<RPEEvent>>,
        factor: f32,
        desc: &str,
        bezier_map: &BezierMap,
    ) -> Result<AnimFloat> {
        let anis: Vec<_> = event_layers
            .iter()
            .filter_map(|it| get(it).as_ref().map(|es| parse_events(r, es, None, bezier_map)))
            .collect::<Result<_>>()
            .with_context(|| ptl!("type-events-parse-failed", "type" => desc))?;
        let mut res = AnimFloat::chain(anis);
        if res.is_default() {
            return Ok(AnimFloat::fixed(0.0));
        }
        res.map_value(|v| v * factor);
        Ok(res)
    }
    // 速度事件积分成 height：按用户开关与版本选择现代/旧版两套实现，
    // 二者对同一份谱面可能给出不同的位移曲线（这正是需要开关的原因）。
    let mut height = if use_rpe_170_speed {
        parse_speed_events(r, &event_layers, bezier_map, max_time, speed_mode)?
    } else {
        parse_speed_events_legacy(r, &event_layers, max_time)?
    };
    // 解析音符：可能触发自定义音效的磁盘读取，故为 async。
    let mut notes = parse_notes(r, rpe.notes.unwrap_or_default(), fs, &mut height, hitsounds).await?;
    let cache = JudgeLineCache::new(&mut notes);
    Ok(JudgeLine {
        object: Object {
            alpha: events_with_factor(r, &event_layers, |it| &it.alpha_events, 1. / 255., "alpha", bezier_map)?,
            rotation: events_with_factor(r, &event_layers, |it| &it.rotate_events, -1., "rotate", bezier_map)?,
            translation: AnimVector(
                events_with_factor(r, &event_layers, |it| &it.move_x_events, 2. / RPE_WIDTH, "move X", bezier_map)?,
                events_with_factor(r, &event_layers, |it| &it.move_y_events, 2. / RPE_HEIGHT, "move Y", bezier_map)?,
            ),
            // 缩放：普通线体（line.png）的缩放就是倍率，图片判定线则按 RPE_WIDTH 归一化。
            scale: {
                // 内部工具：解析一条可选缩放事件并乘系数；键缺失或空数组返回固定 default。
                fn parse(r: &mut BpmList, opt: &Option<Vec<RPEEvent>>, factor: f32, default: f32, bezier_map: &BezierMap) -> Result<AnimFloat> {
                    let Some(events) = opt.as_ref().filter(|it| !it.is_empty()) else {
                        return Ok(AnimFloat::fixed(default));
                    };
                    let mut res = parse_events(r, events, None, bezier_map)?;
                    res.map_value(|v| v * factor);
                    Ok(res)
                }
                // line.png 的缩放是纯倍率（1 = 原尺寸）；图片判定线以 RPE_WIDTH 为满宽度，
                // 因此除以半宽换算到归一化坐标下的缩放。
                let factor = if rpe.texture == "line.png" { 1. } else { 2. / RPE_WIDTH };
                let default = if rpe.texture == "line.png" { 1. } else { factor };
                // 有 extended 时按 scaleX/scaleY 事件解析；其中 X 缩放还有一个历史特殊因子：
                // line.png 且既无文本事件、也未挂 UI 时再乘 0.5，改动它会影响所有此类判定线的线宽。
                rpe.extended
                    .as_ref()
                    .map(|e| -> Result<_> {
                        Ok(AnimVector(
                            parse(
                                r,
                                &e.scale_x_events,
                                factor
                                    * if rpe.texture == "line.png"
                                        && rpe
                                            .extended
                                            .as_ref()
                                            .and_then(|it| it.text_events.as_ref())
                                            .is_none_or(|it| it.is_empty())
                                        && rpe.attach_ui.is_none()
                                    {
                                        0.5
                                    } else {
                                        1.
                                    },
                                default,
                                bezier_map,
                            )?,
                            parse(r, &e.scale_y_events, factor, default, bezier_map)?,
                        ))
                    })
                    .transpose()?
                    .unwrap_or_else(|| AnimVector(AnimFloat::fixed(default), AnimFloat::fixed(default)))
            },
        },
        // 控制事件：与时间轴动画不同，它们用“时间点 + 键值对”同时控制多个量，
        // 且缓动语义与普通事件相反（见 parse_ctrl_events）。
        ctrl_obj: RefCell::new(CtrlObject {
            alpha: parse_ctrl_events(&rpe.alpha_control, "alpha"),
            size: parse_ctrl_events(&rpe.size_control, "size"),
            pos: parse_ctrl_events(&rpe.pos_control, "pos"),
            y: parse_ctrl_events(&rpe.y_control, "y"),
        }),
        height,
        incline: if let Some(events) = rpe.extended.as_ref().and_then(|e| e.incline_events.as_ref()) {
            parse_events(r, events, Some(0.), bezier_map).with_context(|| ptl!("incline-events-parse-failed"))?
        } else {
            AnimFloat::default()
        },
        notes,
        // 判定线类型由 texture 决定，并进一步受 extended 影响：
        // - line.png：有 paintEvents 变 Paint 线、有 textEvents 变 Text 线，否则普通线；
        // - 其它文件名：有 gifEvents 走 GIF 分支，否则当图片贴图（同图跨线复用纹理）。
        kind: if rpe.texture == "line.png" {
            if let Some(events) = rpe.extended.as_ref().and_then(|e| e.paint_events.as_ref()) {
                JudgeLineKind::Paint(
                    parse_events(r, events, Some(-1.), bezier_map).with_context(|| ptl!("paint-events-parse-failed"))?,
                    RefCell::default(),
                )
            } else if let Some(extended) = rpe.extended.as_ref() {
                if let Some(events) = extended.text_events.as_ref() {
                    JudgeLineKind::Text(parse_events(r, events, Some(String::new()), bezier_map).with_context(|| ptl!("text-events-parse-failed"))?)
                } else {
                    JudgeLineKind::Normal
                }
            } else {
                JudgeLineKind::Normal
            }
        } else if let Some(extended) = rpe.extended.as_ref() {
            if let Some(events) = extended.gif_events.as_ref() {
                let data = fs
                    .load_file(&rpe.texture)
                    .await
                    .with_context(|| ptl!("gif-load-failed", "path" => rpe.texture.clone()))?;
                let frames = GifFrames::new(
                    tokio::spawn(async move {
                        let decoder = gif::GifDecoder::new(Cursor::new(data))?;
                        debug!("decoding gif");
                        Ok::<std::vec::Vec<_>, ImageError>(decoder.into_frames().collect())
                    })
                    .into_future()
                    .await??
                    .into_iter()
                    .map(|frame| -> (u128, SafeTexture) {
                        let frame = frame.unwrap();
                        let delay: Duration = frame.delay().into();
                        (delay.as_millis(), SafeTexture::from(DynamicImage::ImageRgba8(frame.into_buffer())))
                    })
                    .collect(),
                );
                debug!("gif decoded");
                let events = parse_gif_events(r, events, bezier_map, &frames).with_context(|| ptl!("gif-events-parse-failed"))?;
                JudgeLineKind::TextureGif(events, frames, rpe.texture.clone())
            } else if let Some(texture) = line_texture_map.get(&rpe.texture) {
                debug!("texture {} reused, id: {}", rpe.texture.clone(), texture.clone().into_inner().raw_miniquad_texture_handle().gl_internal_id());
                JudgeLineKind::Texture(texture.clone(), rpe.texture.clone())
            } else {
                let texture = SafeTexture::from(image::load_from_memory(
                    &fs.load_file(&rpe.texture)
                        .await
                        .with_context(|| ptl!("illustration-load-failed", "path" => rpe.texture.clone()))?,
                )?)
                .with_mipmap();
                line_texture_map.insert(rpe.texture.clone(), texture.clone());
                JudgeLineKind::Texture(texture, rpe.texture.clone())
            }
        } else if let Some(texture) = line_texture_map.get(&rpe.texture) {
            debug!("texture {} reused, id: {}", rpe.texture.clone(), texture.clone().into_inner().raw_miniquad_texture_handle().gl_internal_id());
            JudgeLineKind::Texture(texture.clone(), rpe.texture.clone())
        } else {
            let texture = SafeTexture::from(image::load_from_memory(
                &fs.load_file(&rpe.texture)
                    .await
                    .with_context(|| ptl!("illustration-load-failed", "path" => rpe.texture.clone()))?,
            )?)
            .with_mipmap();
            line_texture_map.insert(rpe.texture.clone(), texture.clone());
            JudgeLineKind::Texture(texture, rpe.texture.clone())
        },
        color: if let Some(events) = rpe.extended.as_ref().and_then(|e| e.color_events.as_ref()) {
            parse_events(r, events, Some(WHITE), bezier_map).with_context(|| ptl!("color-events-parse-failed"))?
        } else {
            Anim::default()
        },
        // 父线关系：-1 表示无父；非负下标引用 judge_line_list 的同序号元素，
        // 由 parse_rpe 统一做父子环路检测（否则渲染时递归求父变换会栈溢出）。
        parent: {
            let parent = rpe.parent.unwrap_or(-1);
            if parent == -1 {
                None
            } else {
                Some(parent as usize)
            }
        },
        rot_with_parent: rpe.rotate_with_father.unwrap_or(false),
        z_index: rpe.z_order,
        show_below: rpe.is_cover != 1,
        attach_ui: rpe.attach_ui,

        cache,
    })
}

/// 把一个事件携带的贝塞尔控制点登记进 [`BezierMap`]（已存在则复用）。
///
/// 先用与 [`RPEEvent::bezier_key`] 相同的取整规则算出键，再 `or_insert_with`
/// 惰性构造 [`BezierTween`]——只有真的被引用到才会构造曲线。
/// 控制点取事件的 `bezierPoints` 前四位，解释为 `(p1x, p1y)`、`(p2x, p2y)`。
fn add_bezier<T>(map: &mut BezierMap, event: &RPEEvent<T>) {
    if event.bezier != 0 {
        let p = &event.bezier_points;
        let int = |p: f32| (p * 100.).round() as i16;
        map.entry(((int(p[0]) * 100 + int(p[1])) as u16, int(p[2]), int(p[3])))
            .or_insert_with(|| Rc::new(BezierTween::new((p[0], p[1]), (p[2], p[3]))));
    }
}

/// 对指定的事件字段批量登记贝塞尔曲线（宏用于展开重复的遍历样板）。
///
/// 用法：`process_bezier!(事件来源, &mut map, 字段1, 字段2, ...)`；
/// 每个字段都是 `Option<Vec<RPEEvent<_>>>`，宏内部用 `iter().flatten()` 跳过缺省项。
macro_rules! process_bezier {
    ($event_layer:expr, $map:expr, $($field:ident),*) => {
        $(
            for event in $event_layer.$field.iter().flatten() {
                add_bezier($map, event);
            }
        )*
    };
}

/// 预扫描整个谱面，收集所有出现过的贝塞尔控制点，构建 [`BezierMap`]。
///
/// 之所以要全量预扫描：解析是流式的，事件在解析时才用到曲线，而贝塞尔曲线的
/// 构造（要预采样查找表）代价较高；集中建表并去重可避免同一控制点被反复构造。
/// 扫描范围覆盖事件层事件（alpha / 位移 X / 位移 Y / 旋转）与扩展事件
/// （绘制 / 缩放 X / 缩放 Y / GIF / 倾斜 / 文本 / 颜色）。
fn get_bezier_map(rpe: &RPEChart) -> BezierMap {
    let mut map = HashMap::new();
    for line in &rpe.judge_line_list {
        for event_layer in line.event_layers.iter().flatten() {
            process_bezier!(event_layer, &mut map, alpha_events, move_x_events, move_y_events, rotate_events);
        }
        if let Some(ext_layer) = &line.extended {
            process_bezier!(ext_layer, &mut map, paint_events, scale_x_events, scale_y_events, gif_events, incline_events, text_events, color_events);
        }
    }
    map
}

/// 解析 Re:PhiEdit 导出的 JSON 谱面。
///
/// # Arguments
/// * `source` - 谱面 JSON 文本；
/// * `fs` - 虚拟文件系统（贴图 / GIF / 音效等资源都在谱面包内）；
/// * `extra` - 外部 extra.json 扩展；
/// * `use_rpe_170_speed` - 用户对“新旧速度算法”的手动选择（见 `parse_judge_line`）。
///
/// # Returns
/// 与其它格式一致的 [`Chart`]。`offset` 取 `META.offset / 1000`——RPE 用**毫秒**记偏移，
/// 而项目统一用秒。`ChartSettings` 取默认（RPE 没有 PE 的负 alpha 特殊语义）；
/// `hitsounds` 是解析过程中收集到的自定义打击音缓存。
///
/// # Errors
/// JSON 语法错误、任一判定线解析失败（附线路名），以及**判定线父子环路**。
///
/// 三个关键点：
/// - `SpeedEasingMode` 由 `META.RPEVersion` 决定：`>= 170` 用 `Modern`，
///   否则 `Legacy`。RPE 1.7.0 改变了速度事件的缓动语义，
///   用错会让判定线纵向位移整体偏斜；
/// - 必须先 `get_bezier_map` 预扫描，再进入流式解析（见该函数）；
/// - 解析完成后**必须**做父子环路检测：RPE 允许把判定线串成树，但编辑器/手改谱面
///   可能出现环（A 的父是 B、B 的父是 A）。不拦截的话，渲染时递归求父线的
///   `fetch_rot`/`fetch_pos` 会无限递归直至栈溢出。这里在全部判定线就绪后逐条沿
///   `parent` 上溯，一旦回到访问过的下标就 `bail!`。
pub async fn parse_rpe(source: &str, fs: &mut dyn FileSystem, extra: ChartExtra, use_rpe_170_speed: bool) -> Result<Chart> {
    // 阶段 1：反序列化并确定速度缓动模式（按 RPEVersion 分界 170）。
    let rpe: RPEChart = serde_json::from_str(source).with_context(|| ptl!("json-parse-failed"))?;
    let speed_mode = if rpe.meta.rpe_version >= 170 {
        SpeedEasingMode::Modern
    } else {
        SpeedEasingMode::Legacy
    };
    // 阶段 2：预扫描贝塞尔控制点，并构造 BPM 表与其他跨线缓存。
    let bezier_map = get_bezier_map(&rpe);
    let mut r = BpmList::new(rpe.bpm_list.into_iter().map(|it| (it.start_time.beats(), it.bpm)).collect());
    // 把 `Option<Vec<T>>` 统一看作可能为空的序列，避免到处写 match。
    fn vec<T>(v: &Option<Vec<T>>) -> impl Iterator<Item = &T> {
        v.iter().flat_map(|it| it.iter())
    }
    let mut hitsounds = HashMap::new();
    // 阶段 3：扫描所有判定线求最大时间（音符结束时间与各事件终点换算成秒取最大），
    // 再加 1 秒作补尾余量，供速度积分与关键帧收尾使用。
    #[rustfmt::skip]
    let max_time = *rpe
        .judge_line_list
        .iter()
        .map(|line| {
            line.notes.as_ref().map(|notes| {
                notes
                    .iter()
                    .map(|note| r.time(&note.end_time).not_nan())
                    .max()
                    .unwrap_or_default()
            }).unwrap_or_default().max(
                line.event_layers.iter().filter_map(|it| it.as_ref().map(|layer| {
                    vec(&layer.alpha_events)
                        .chain(vec(&layer.move_x_events))
                        .chain(vec(&layer.move_y_events))
                        .chain(vec(&layer.rotate_events))
                        .map(|it| r.time(&it.end_time).not_nan())
                        .max().unwrap_or_default()
                })).max().unwrap_or_default()
            ).max(
                line.extended.as_ref().map(|e| {
                    vec(&e.scale_x_events)
                        .chain(vec(&e.scale_y_events))
                        .map(|it| r.time(&it.end_time).not_nan())
                        .max().unwrap_or_default()
                        .max(vec(&e.text_events).map(|it| r.time(&it.end_time).not_nan()).max().unwrap_or_default())
                }).unwrap_or_default()
            )
        })
        .max().unwrap_or_default() + 1.;
    // 阶段 4：逐条解析判定线。这里用顺序 for 循环而不是并发 join_all：除了避免为
    // join_all 引入额外依赖，顺序处理也让 `line_texture_map`/`hitsounds` 的跨判定线
    // 复用天然成立（并发时还要额外处理共享可变状态）。
    // don't want to add a whole crate for a mere join_all...
    let mut lines = Vec::new();
    let mut line_texture_map = HashMap::new();
    for (id, rpe) in rpe.judge_line_list.into_iter().enumerate() {
        let name = rpe.name.clone();
        lines.push(
            parse_judge_line(&mut r, rpe, max_time, speed_mode, fs, use_rpe_170_speed, &bezier_map, &mut hitsounds, &mut line_texture_map)
                .await
                .with_context(move || ptl!("judge-line-location-name", "jlid" => id, "name" => name))?,
        );
    }
    // 阶段 5：父子环路检测（关键安全校验，详见函数文档）。
    // 沿 parent 链一路上溯，把经过的下标记录进 visited；若某步的父线已在 visited 中，
    // 说明存在环，返回该下标；递归到无父线则返回 None（正常终止）。
    fn has_cycle(line: &JudgeLine, lines: &[JudgeLine], visited: &mut Vec<usize>) -> Option<usize> {
        if let Some(parent_index) = line.parent {
            if visited.contains(&parent_index) {
                return Some(parent_index);
            }
            visited.push(parent_index);
            return has_cycle(&lines[parent_index], lines, visited);
        }
        None
    }
    // 每条判定线都从“自己”出发检查一遍：初始把自身下标放进 visited，
    // 这样自己指向自己（parent == 自身下标）的退化情形也能被检出。
    for (i, line) in lines.iter().enumerate() {
        let mut vec = Vec::new();
        vec.push(i);
        if let Some(line) = has_cycle(line, &lines, &mut vec) {
            ptl!(bail "found infinite recursive parent relations", "line" => line)
        }
    }
    // 统一收尾：音符排序 + 多押标记。
    process_lines(&mut lines);
    // offset 单位换算：RPE 用毫秒，项目用秒；ChartSettings 保持默认（RPE 无 PE 的 alpha 扩展）。
    Ok(Chart::new(rpe.meta.offset as f32 / 1000.0, lines, r, ChartSettings::default(), extra, hitsounds))
}

/// 轻量预扫描 RPE 谱面，报告可能影响播放的格式特性，供调用方提示用户或切换兼容路径。
///
/// 只做一次整份 JSON 的反序列化（代价主要在这里）加两次线性扫描，不构造 [`Chart`]：
/// 正式解析要加载贴图/音频，代价高，而这里只需要得出两个布尔结论。
///
/// # Arguments
/// * `source` - 谱面 JSON 文本。
///
/// # Returns
/// [`ParseWarnings`]：是否含 `easingType > 1` 的新式速度事件、是否有判定线挂了 `attachUI`。
///
/// # Errors
/// JSON 反序列化失败时返回错误（与正式解析共用同一套 serde 结构，
/// 因此这里失败通常意味着正式解析也会失败）。
///
/// 注意：函数体内并没有 `await`，`async` 只是为了与 `parse_rpe` 保持一致的调用形态
/// （调用方可以对两者统一 `.await`）。
pub async fn lint(source: &str) -> Result<ParseWarnings> {
    // 反序列化：与正式解析共用同一套结构，因此结构层面的错误在这里就能暴露。
    let rpe: RPEChart = serde_json::from_str(source).with_context(|| ptl!("json-parse-failed"))?;
    // 扫描一：所有事件层里是否存在 easingType > 1 的新式速度事件。
    let has_new_speed_events = rpe
        .judge_line_list
        .iter()
        .flat_map(|line| line.event_layers.iter().flatten())
        .flat_map(|layer| layer.speed_events.iter())
        .flatten()
        .any(|event| event.easing_type > 1);
    // 扫描二：是否有判定线挂了 attachUI（会影响渲染路径选择）。
    let has_attach_ui = rpe.judge_line_list.iter().any(|line| line.attach_ui.is_some());

    Ok(ParseWarnings {
        has_new_speed_events,
        has_attach_ui,
    })
}
