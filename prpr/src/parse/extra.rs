//! `extra.json` 解析器：Phira 对谱面的私有扩展（特效与视频）。
//!
//! PEC / PGR / RPE 三种谱面格式都没有“全局特效”“视频背景”这类概念，
//! 它们是 Phira 在谱面之外附加的能力，因此单独放在与谱面同目录的 `extra.json` 里，
//! 由本模块解析成 [`crate::core::ChartExtra`]，再挂到 [`crate::core::Chart`] 上。
//!
//! 关键设计是**双路径着色器**：`shader` 字段若以 `/` 开头，视为谱面包内的**相对路径**，
//! 从[虚拟文件系统](crate::fs::FileSystem)读入自定义 GLSL；否则视为内置预设名，
//! 查 [`crate::core::Effect::get_preset`]。前缀 `/` 是 Phira 用来区分“用户资源”与
//! “引擎内置”的约定；相对路径的基准是谱面所在目录（即虚拟文件系统的根）。
//!
//! 特效分两类，渲染时机完全不同：`global == true` 的**全局特效**作用于整个屏幕，
//! 在 `GameScene::render` 最后统一绘制；普通特效属于谱面本身，在 `Chart::render`
//! 内部绘制（因此会被 UI/设置界面遮挡）。
use super::RPE_TWEEN_MAP;
use anyhow::{Context, Result};
use macroquad::prelude::{Color, Vec2};
use serde::Deserialize;
use std::{collections::HashMap, rc::Rc};

use super::L10N_LOCAL;
#[cfg(feature = "video")]
use crate::core::Video;
use crate::{
    core::{Anim, BpmList, ChartExtra, ClampedTween, Effect, Keyframe, StaticTween, Triple, Tweenable, Uniform, EPS},
    ext::ScaleType,
    fs::FileSystem,
};

// serde is weird...
/// `#[serde(default = "...")]` 只接受函数路径，不能写字面量，故提供 f32 的 0 默认值函数。
fn f32_zero() -> f32 {
    0.
}

/// 同上，提供 f32 的 1 默认值；`easingRight` 缺省 1 表示“整条缓动曲线不裁剪”。
fn f32_one() -> f32 {
    1.
}

/// extra.json 中的单个关键帧（沿用 RPE 的 `startTime`/`endTime` 双端事件写法）。
///
/// 字段名经 `rename_all = "camelCase"` 映射到 JSON 的驼峰键；同一个结构体被复用于
/// 标量（`T = f32`）、二维向量（`T = (f32, f32)`）与颜色（`T = [u8; 4]`），
/// 具体是哪种由外层 [`Variable`] 的 untagged 反序列化按 JSON 结构决定。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtKeyframe<T> {
    /// 缓动区间左端（0..=1 的子区间），JSON 键 `easingLeft`，缺省 0。
    /// 收窄区间表示只取整条缓动曲线的一段。
    #[serde(default = "f32_zero")]
    easing_left: f32,
    /// 缓动区间右端，JSON 键 `easingRight`，缺省 1（不裁剪）。
    #[serde(default = "f32_one")]
    easing_right: f32,
    /// 缓动编号，JSON 键 `easingType`；extra.json 沿用 RPE 的编号体系，
    /// 由 [`RPE_TWEEN_MAP`] 翻译成 [`crate::core::TweenId`]。
    easing_type: i32,
    /// 区间起点值，JSON 键 `start`。
    start: T,
    /// 区间终点值，JSON 键 `end`。
    end: T,
    /// 起点拍号（[`Triple`]，可精确表示 1/3 拍等分数拍），JSON 键 `startTime`；
    /// 经 [`crate::core::BpmList::time`] 折算为秒。
    start_time: Triple,
    /// 终点拍号，JSON 键 `endTime`。
    end_time: Triple,
}

/// 变量取值的三种形态，用 untagged 反序列化按 JSON 结构自动区分。
///
/// 需要这个枚举是因为 extra.json 允许把同一个 uniform 写成常量、固定值或关键帧数组，
/// 而它们在 JSON 里只是 `0.5` / `[x, y]` / `[{...}, ...]`，没有类型标记，
/// serde 只能按结构逐个尝试匹配，因此变体的**顺序即匹配优先级**。
#[derive(Default, Deserialize)]
#[serde(untagged)]
enum ExtAnim<V> {
    /// 变量未出现（或为 `null`）：保持 [`Anim::default`]（即“无动画”）。
    #[default]
    Default,
    /// 单个常量值：整条时间轴取同一个值。
    Fixed(V),
    /// 关键帧数组，元素为 [`ExtKeyframe`]。
    Keyframes(Vec<ExtKeyframe<V>>),
}

// ExtAnim 到 Anim<T> 的转换：把“常量 / 关键帧数组”统一展开成关键帧序列，
// 缓动编号翻译、缓动区间裁剪、首帧补默认值都在这里一次性完成。
impl<V> ExtAnim<V> {
    /// 把扩展动画转换为格式无关的 [`Anim<T>`]。
    ///
    /// # Arguments
    /// * `r` - BPM 表，用于把 `startTime`/`endTime` 的拍号换算为秒；
    /// * `default` - 若首个关键帧不在 0 拍，则用它补一个 0 时刻的起始值。
    ///   不补的话时间轴开头没有定义，[`Anim::now`] 在首帧之前会取到默认值而非真实初值。
    ///
    /// # Returns
    /// 关键帧按事件顺序依次 push，因此**要求事件本身已按时间排好序**。
    ///
    /// 缓动处理：`easing_type` 先 `.max(1)` 再查 [`RPE_TWEEN_MAP`]（越界退回线性）；
    /// 只有缓动区间被真正收窄时（左端 > 0 或右端 < 1）才包一层 [`ClampedTween`]，
    /// 整段曲线直接复用静态缓动，避免每个关键帧都分配一个新对象。
    fn into<T: Tweenable>(self, r: &mut BpmList, default: Option<T>) -> Anim<T>
    where
        V: Into<T>,
    {
        match self {
            ExtAnim::Default => Anim::default(),
            ExtAnim::Fixed(value) => Anim::fixed(value.into()),
            ExtAnim::Keyframes(events) => {
                let mut kfs = Vec::new();
                if let Some(default) = default {
                    if events[0].start_time.beats() != 0.0 {
                        kfs.push(Keyframe::new(0.0, default, 0));
                    }
                }
                for e in events {
                    kfs.push(Keyframe {
                        time: r.time(&e.start_time),
                        value: e.start.into(),
                        tween: {
                            let tween = RPE_TWEEN_MAP.get(e.easing_type.max(1) as usize).copied().unwrap_or(RPE_TWEEN_MAP[0]);
                            if e.easing_left.abs() < EPS as f32 && (e.easing_right - 1.0).abs() < EPS as f32 {
                                StaticTween::get_rc(tween)
                            } else {
                                Rc::new(ClampedTween::new(tween, e.easing_left..e.easing_right))
                            }
                        },
                    });
                    kfs.push(Keyframe::new(r.time(&e.end_time), e.end.into(), 0));
                }
                Anim::new(kfs)
            }
        }
    }
}

/// extra.json 的 BPM 变化点（`bpm` 写成列表形式时的元素）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtBpmItem {
    /// BPM 生效的起点，JSON 键 `time`；这里用拍号表示而非秒。
    time: Triple,
    /// 从 `time` 起生效的 BPM 值，JSON 键 `bpm`。
    bpm: f64,
}

/// `bpm` 字段的两种写法：单个数字，或完整的 BPM 变化列表。
///
/// 用 untagged 反序列化是为了让只有单一 BPM 的谱面可以直接写 `"bpm": 120`，
/// 而不必套一层数组；两种写法最终都会走 [`From`] 统一成 [`BpmList`]。
#[derive(Deserialize)]
#[serde(untagged)]
enum BpmForm {
    /// 恒定 BPM，等价于“从 0 拍起生效”。
    Single(f64),
    /// BPM 变化列表。
    List(Vec<ExtBpmItem>),
}

// 把两种写法统一成 BpmList：单值写法补一个 0 拍起点；
// 列表写法直接把 (拍号秒) 交给 BpmList::new（其内部假定输入按 beats 升序）。
impl From<BpmForm> for BpmList {
    /// 单值写法补 `(0., value)` 作为唯一起点；列表写法逐项取 `Triple::beats()` 还原拍数。
    fn from(value: BpmForm) -> Self {
        match value {
            BpmForm::Single(value) => BpmList::new(vec![(0., value)]),
            BpmForm::List(list) => BpmList::new(list.into_iter().map(|it| (it.time.beats(), it.bpm)).collect()),
        }
    }
}

/// 特效 uniform 变量的取值类型。
///
/// 这里的三层嵌套（Float/Vec2/Color 各自再包一个 [`ExtAnim`]）是为了同时表达
/// “值的类型”与“值随时间的变化方式”。由于是 untagged 反序列化，匹配完全依赖
/// JSON 结构差异：数字 → Float、二元组 → Vec2、四元组 → Color，
/// 因此变体顺序即尝试顺序。
#[derive(Deserialize)]
#[serde(untagged)]
enum Variable {
    /// 标量 uniform（GLSL `float`）。
    Float(ExtAnim<f32>),
    /// 二维向量 uniform（GLSL `vec2`），值形如 `[x, y]`。
    Vec2(ExtAnim<(f32, f32)>),
    /// 颜色 uniform（GLSL `vec4`），源格式为 RGBA 四字节 0..=255，
    /// 经 [`crate::core::Tweenable`] 的 Color 实现转成项目内部的 0..=1 浮点色。
    Color(ExtAnim<[u8; 4]>),
}

/// extra.json 的 `effects` 数组元素：一条特效的完整描述。
#[derive(Deserialize)]
struct ExtEffect {
    /// 特效生效起点拍号，JSON 键 `start`。
    start: Triple,
    /// 特效生效终点拍号，JSON 键 `end`；与 `start` 一起构成 [`Effect::new`] 的 `time_range`，
    /// 区间外该特效不参与渲染。
    end: Triple,
    /// 着色器标识，JSON 键 `shader`。以 `/` 开头表示谱面包内相对路径的自定义 GLSL，
    /// 否则是内置预设名（双路径设计见模块文档）。
    shader: String,
    /// 传给着色器的 uniform 变量表，键为 uniform 名、值为 [`Variable`]，
    /// JSON 键 `vars`，缺省为空。
    #[serde(default)]
    vars: HashMap<String, Variable>,
    /// 是否为全局特效，JSON 键 `global`，缺省 false。
    /// 全局特效作用于整屏、在 `GameScene::render` 末尾统一绘制；
    /// 普通特效隶属谱面、在 `Chart::render` 内绘制。
    #[serde(default)]
    global: bool,
}

/// extra.json 的 `videos` 数组元素：一个视频背景。
///
/// 该结构在未启用 `video` feature 时仍会完成反序列化（除 `attach` 外的字段都保留），
/// 只是不会被构造成 `Video`，因此需要 `allow(dead_code)` 抑制未使用告警。
#[allow(dead_code)]
#[derive(Deserialize)]
struct ExtVideo {
    /// 视频文件在谱面包内的相对路径，JSON 键 `path`。
    path: String,
    /// 视频开始播放的拍号，JSON 键 `time`，缺省 0（谱面开头）。
    #[serde(default)]
    time: Triple,
    /// 画面的缩放 / 裁切方式，JSON 键 `scale`，缺省用 [`ScaleType`] 的默认值。
    #[serde(default)]
    scale: ScaleType,
    /// 视频不透明度动画，JSON 键 `alpha`，缺省按固定 1.0 处理。
    #[serde(default)]
    alpha: ExtAnim<f32>,
    /// 压暗程度（叠加黑幕）动画，JSON 键 `dim`，缺省按固定 0.0 处理。
    #[serde(default)]
    dim: ExtAnim<f32>,
    /// 把视频挂到某条判定线上的信息，JSON 键 `attach`；缺省表示铺满全屏。
    /// 仅启用 `video` feature 时才解析——`VideoAttach` 类型定义在该 feature 下。
    #[cfg(feature = "video")]
    #[serde(default)]
    attach: Option<crate::core::VideoAttach>,
}

/// `extra.json` 的顶层结构。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Extra {
    /// 谱面 BPM，JSON 键 `bpm`；可写成单值或列表，见 [`BpmForm`]。
    /// 注意这是 extra.json 自带的 BPM 表，而不是去读谱面文件的 BPM。
    bpm: BpmForm,
    /// 特效列表，JSON 键 `effects`，缺省为空。
    #[serde(default)]
    effects: Vec<ExtEffect>,
    /// 视频列表，JSON 键 `videos`，缺省为空。
    #[serde(default)]
    videos: Vec<ExtVideo>,
}

/// 把一条 [`ExtEffect`] 解析成运行时的 [`Effect`]。
///
/// 着色器解析是双路径的（见模块文档）：`shader` 以 `/` 开头时从虚拟文件系统读取
/// 自定义 GLSL（先 `strip_prefix('/')` 去掉标记，剩下的路径相对谱面目录解析），
/// 否则按内置预设名查 [`Effect::get_preset`]，查不到即报错。
///
/// # Arguments
/// * `r` - 用 extra.json 的 BPM 表把特效起止拍号换算为秒；
/// * `rpe` - 已反序列化的特效描述；
/// * `fs` - 虚拟文件系统，用于加载自定义着色器源码。
///
/// # Returns
/// 构造好的 [`Effect`]，其 `global` 取自 JSON，决定它挂到全局还是谱面特效列表。
///
/// # Errors
/// 自定义着色器读取失败或内容不是 UTF-8、亦或内置预设名不存在时返回错误。
async fn parse_effect(r: &mut BpmList, rpe: ExtEffect, fs: &mut dyn FileSystem) -> Result<Effect> {
    let range = r.time(&rpe.start)..r.time(&rpe.end);
    let vars = rpe
        .vars
        .into_iter()
        .map(|(name, var)| -> Result<Box<dyn Uniform>> {
            Ok(match var {
                Variable::Float(events) => Box::new((name, events.into::<f32>(r, None))),
                Variable::Vec2(events) => Box::new((name, events.into::<Vec2>(r, None))),
                Variable::Color(events) => Box::new((name, events.into::<Color>(r, None))),
            })
        })
        .collect::<Result<_>>()?;
    // `string` 先声明、后在其中一条分支里赋值：目的是让“从文件读出的 GLSL 源码”
    // 活到 Effect::new 调用结束（预设分支返回的是 'static 内置源码，用不到该变量）。
    let string;
    Effect::new(
        range,
        if let Some(path) = rpe.shader.strip_prefix('/') {
            string = String::from_utf8(fs.load_file(path).await?).with_context(|| ptl!("shader-load-failed", "path" => path))?;
            &string
        } else {
            Effect::get_preset(&rpe.shader).ok_or_else(|| ptl!(err "shader-not-found", "shader" => rpe.shader))?
        },
        vars,
        rpe.global,
    )
}

/// 解析 `extra.json`，得到挂在谱面上的扩展内容（特效与视频）。
///
/// # Arguments
/// * `source` - `extra.json` 的文本内容；
/// * `fs` - 虚拟文件系统，用于加载自定义着色器与视频文件。
///
/// # Returns
/// [`ChartExtra`]：特效按 `global` 分成两组，视频（若启用 feature）附带可选的挂载信息。
///
/// # Errors
/// JSON 语法错误、或特效/视频加载失败时返回错误；错误信息会带上出错下标或文件路径。
///
/// 特效分流规则：`global == true` 进 `global_effects`（整个屏幕、最后由 GameScene 渲染），
/// 否则进 `effects`（谱面内渲染），二者渲染时机不同，见 `ExtEffect` 的 `global` 字段。
pub async fn parse_extra(source: &str, fs: &mut dyn FileSystem) -> Result<ChartExtra> {
    // 阶段 1：反序列化整个 extra.json，并用其中的 bpm 字段构造 BPM 表。
    let ext: Extra = serde_json::from_str(source).with_context(|| ptl!("json-parse-failed"))?;
    let mut r: BpmList = ext.bpm.into();
    let mut effects = Vec::new();
    let mut global_effects = Vec::new();
    // 阶段 2：逐条解析特效，按 global 分流到两组；失败时用下标 id 定位是哪一条。
    for (id, effect) in ext.effects.into_iter().enumerate() {
        (if effect.global { &mut global_effects } else { &mut effects }).push(
            parse_effect(&mut r, effect, fs)
                .await
                .with_context(|| ptl!("effect-location", "id" => id))?,
        );
    }
    // 阶段 3：解析视频。只有启用 `video` feature 才会实际构造 Video 并读取文件，
    // `attach` 字段决定视频挂到哪条判定线上（见 VideoAttach）；
    // 未启用 feature 时忽略 videos，仅在列表非空时给一条告警，避免用户困惑。
    #[cfg(feature = "video")]
    let mut videos = Vec::new();
    #[cfg(feature = "video")]
    for video in ext.videos {
        videos.push((
            Video::new(
                fs.load_file(&video.path)
                    .await
                    .with_context(|| ptl!("video-load-failed", "path" => video.path.clone()))?,
                r.time(&video.time),
                video.scale,
                video.alpha.into(&mut r, Some(1.)),
                video.dim.into(&mut r, Some(0.)),
            )
            .with_context(|| ptl!("video-load-failed", "path" => video.path))?,
            video.attach,
        ));
    }
    #[cfg(not(feature = "video"))]
    if !ext.videos.is_empty() {
        tracing::warn!("video is disabled in this build");
    }
    // 阶段 4：组装 ChartExtra。`videos` 字段本身也受 feature 控制。
    Ok(ChartExtra {
        effects,
        global_effects,
        #[cfg(feature = "video")]
        videos,
    })
}
