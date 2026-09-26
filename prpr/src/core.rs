//! Core module for prpr, submodules:
//!   - [crate::core::anim]
//!   - [crate::core::chart]
//!   - [crate::core::effect]
//!   - [crate::core::line]
//!   - [crate::core::note]
//!   - [crate::core::object]
//!   - [crate::core::render]
//!   - [crate::core::resource]
//!   - [crate::core::smooth]
//!   - [crate::core::tween]
//!
//! 本模块同时定义全局几何约定与常量：谱面坐标一律以“归一化坐标系”描述
//! （见 `NOTE_WIDTH_RATIO_BASE`、`HEIGHT_RATIO`），渲染时再乘屏幕尺寸，
//! 从而保证同一份谱面在不同分辨率下观感一致。

/// 直接复用 macroquad 的颜色类型，避免 prpr 与渲染后端之间来回换算。
pub use macroquad::color::Color;

/// 音符贴图宽度相对判定线宽度的基准比例。
///
/// Phigros 官方资源包按此比例绘制音符，谱面里音符宽度 = 判定线宽度 *
/// 本常量 * note 缩放系数；调整它会让所有谱面的音符大小整体失真。
pub const NOTE_WIDTH_RATIO_BASE: f64 = 0.13175016;
/// 判定线高度相对屏幕高度的基准比例。
///
/// 与 `NOTE_WIDTH_RATIO_BASE` 共同构成游戏内元素的缩放基准：
/// 判定线的基准长度 = 屏幕高度 * 本常量，横向取 16:9 时恰好铺满判定区域。
pub const HEIGHT_RATIO: f64 = 0.83175;

/// 浮点比较与几何判定的容差。
///
/// 谱面时间、坐标都经过浮点运算，用 1e-5 作为“可视为相等”的阈值，
/// 它同时也是几何退化判断（如判断两线是否重合）的精度上限。
pub const EPS: f64 = 1e-5;

/// 二维点，表示判定线局部坐标系中的一个位置。
pub type Point = nalgebra::Point2<f32>;
/// 二维向量，用于方向、位移等与“位置”语义相区分的量。
pub type Vector = nalgebra::Vector2<f32>;
/// 3x3 矩阵，承载二维仿射变换（旋转 / 缩放 / 平移的复合）。
pub type Matrix = nalgebra::Matrix3<f32>;

// 动画与关键帧插值：把“随时间变化的标量/向量”统一成可插值的曲线。
mod anim;
pub use anim::{Anim, AnimFloat, AnimVector, Keyframe};

// 谱面顶层结构：判定线集合、BPM 信息与谱面级设置。
mod chart;
pub use chart::{Chart, ChartExtra, ChartSettings, HitSoundMap};

// 判定线特效（着色器参数）定义。
mod effect;
pub use effect::{Effect, Uniform};

// 判定线及其缓存、UI 元素挂载点。
mod line;
pub use line::{GifFrames, JudgeLine, JudgeLineCache, JudgeLineKind, UIElement};

mod note;
// 该导入服务于 [`init_assets`]：把 assets 目录注册给 macroquad 的资源加载器。
use macroquad::prelude::set_pc_assets_folder;
pub use note::{BadNote, HitSound, Note, NoteKind, RenderConfig};

// 控制对象：判定线的 alpha / 尺寸 / 位置等由手势驱动的属性。
mod object;
pub use object::{CtrlObject, Object};

// 渲染辅助（离屏渲染目标、内部 id 分配等）。
mod render;
pub use render::{copy_fbo, internal_id, MSRenderTarget};

// 资源包：贴图、打击音、粒子发射器等的加载与持有。
mod resource;
pub use resource::{NoteStyle, ParticleEmitter, ResPackInfo, Resource, ResourcePack, BUFFER_SIZE, DPI_VALUE};

// 平滑工具（手势 / 镜头的阻尼跟随）。
mod smooth;
pub use smooth::Smooth;

// 缓动函数与各类 Tween 实现。
mod tween;
pub use tween::{
    easing_from, BezierTween, ClampedTween, GeneralIntTween, IntClampedTween, IntStaticTween, StaticTween, TweenFunction, TweenId, TweenMajor,
    TweenMinor, Tweenable, TWEEN_FUNCTIONS,
};

// 视频背景支持按 feature 可选：解码依赖较重，部分平台（如无硬解的移动端）不打包。
#[cfg(feature = "video")]
mod video;
#[cfg(feature = "video")]
pub use prpr_avc::demux_audio;
#[cfg(feature = "video")]
pub use video::{Video, VideoAttach};

use crate::ui::TextPainter;
use std::cell::RefCell;

// 字体句柄用 thread_local 保存：macroquad 的 Texture2D 只能在创建它的 GL 线程
// （主线程）上使用，而 UI 各处需要以 `&mut` 借用后重新写回，故用 RefCell 提供
// 内部可变性。注意此处是宏调用，不能在上方使用文档注释。
thread_local! {
    // Phigros 徽标风格字体（PGR），用于标题/带装饰的文本。
    pub static PGR_FONT: RefCell<Option<TextPainter>> = RefCell::default();
    // 粗体字体，用于普通 UI 文本。
    pub static BOLD_FONT: RefCell<Option<TextPainter>> = RefCell::default();
}

/// 初始化资源加载路径与工作目录。
///
/// OpenHarmony 之外的平台：从可执行文件所在目录逐级向上回溯，找到第一个含
/// `assets` 子目录的目录并切换为当前工作目录——这样开发构建（exe 在
/// `target/debug/`）与发布包（exe 与 `assets` 同级）都能正确定位资源。
/// 找到后立即 `break`，避免继续上溯到文件系统根。
/// OpenHarmony 的沙箱结构固定且没有可回溯的 exe 路径，因此直接切到 bundle 内
/// 资源目录；最后把 `assets` 注册为 macroquad 的 pc assets 根目录。
pub fn init_assets() {
    // 非 OpenHarmony 平台才做目录回溯探测（该 API 在 OHOS 沙箱内无意义）。
    #[cfg(not(target_env = "ohos"))]
    if let Ok(mut exe) = std::env::current_exe() {
        while exe.pop() {
            if exe.join("assets").exists() {
                std::env::set_current_dir(exe).unwrap();
                break;
            }
        }
    }
    // OpenHarmony 下资源随应用 bundle 一起发布，路径固定，直接切换。
    #[cfg(target_env = "ohos")]
    let _ = std::env::set_current_dir("/data/storage/el1/bundle/entry/resources/resfile/");
    set_pc_assets_folder("assets");
}

#[derive(serde::Deserialize)]
/// `(i, n, d)`: `i + n / d`
/// 以“整数拍 + 真分数拍”表示的拍号。
///
/// 谱面中大量使用 `1/3`、`1/6` 这类节拍点，若直接存 f64 只能存近似值，
/// 而用整数分子分母可以精确表示，使 [`BpmList`] 的节拍换算不引入累积舍入误差。
pub struct Triple(i32, u32, u32);
// 默认 0 + 0/1 = 0 拍；分母取 1 而非 0 是为了避免 [`Triple::beats`] 除零。
impl Default for Triple {
    fn default() -> Self {
        Self(0, 0, 1)
    }
}

// 拍号到节拍数的换算。
impl Triple {
    /// 把 `i + n/d` 还原为 f64 的节拍数。
    ///
    /// 分母由谱面解析阶段保证非 0（可能为 1），此处直接相除。
    pub fn beats(&self) -> f64 {
        self.0 as f64 + self.1 as f64 / self.2 as f64
    }
}

#[derive(Default)] // the default is a dummy
/// 以 beats 为键的 BPM 变化表，负责节拍 <-> 秒的双向换算。
///
/// 谱面只描述“第几拍起 BPM 变为多少”，而渲染与判定全部以秒为单位，
/// 因此两种时间轴之间会被高频转换。为了摊还转换开销，内部维护 `cursor`
/// 缓存上一次命中的区间下标：正常播放时查询时间单调递增，游标可 O(1) 前进；
/// 但 seek、重新开始或倒放会让时间回退，所以查询里有第二个循环把游标往回退，
/// 形成“可双向移动的”近似二分策略，避免每次从头线性扫描。
pub struct BpmList {
    /// (beats, time, bpm)
    /// time in seconds
    /// 每个 BPM 变化点：`beats` 为生效拍号，`time` 为该拍对应的绝对秒数
    /// （从 0 开始按段长累加），`bpm` 为从该拍起生效的 BPM；按 `beats` 升序。
    elements: Vec<(f64, f64, f64)>,
    /// cursor for searching, value is the index of `elements`
    /// 查询游标：最近一次命中的元素下标，同时作为线性扫描的起点。
    cursor: usize,
}

// BPM 表的时间换算：构造时预计算各段起点秒数，查询时维护游标。
impl BpmList {
    /// Create a new BpmList from a list of (beats, bpm) pairs
    ///
    /// Basically just calculate the time for each pair(key frame)
    /// 由关键帧列表构造 BPM 表，并预计算每个关键帧的绝对秒数。
    ///
    /// # Arguments
    /// * `ranges` - `(beats, bpm)` 序列，语义为“从 beats 拍起 BPM 变为 bpm”，
    ///   必须按 beats 升序给出
    ///
    /// 首帧不参与时间累加（`last_bpm` 为 `None` 时跳过），因为它之前没有生效的
    /// BPM，其 time 恒为 0；此后每段时长 = 拍差 * (60 / 当前 BPM)。
    pub fn new(ranges: Vec<(f64, f64)>) -> Self {
        let mut elements = Vec::new();
        let mut time = 0.0;
        let mut last_beats = 0.0;
        let mut last_bpm: Option<f64> = None;
        for (now_beats, bpm) in ranges {
            // 用“上一段的 BPM”把 [last_beats, now_beats) 这段拍数折算成秒；
            // 60 / bpm 即一拍对应的秒数。
            if let Some(bpm) = last_bpm {
                time += (now_beats - last_beats) * (60. / bpm);
            }
            last_beats = now_beats;
            last_bpm = Some(bpm);
            elements.push((now_beats, time, bpm));
        }
        BpmList { elements, cursor: 0 }
    }

    /// Get the time in seconds for a given beats
    /// 把节拍坐标换算为秒。
    ///
    /// 先把游标向后推到最后一个 `beats <= 目标` 的关键帧，再向前回退防止漏段，
    /// 最后在该段内线性插值（段内 BPM 恒定，因此时间关于拍数是线性的）。
    pub fn time_beats(&mut self, beats: f64) -> f64 {
        while let Some(kf) = self.elements.get(self.cursor + 1) {
            if kf.0 > beats {
                break;
            }
            self.cursor += 1;
        }
        while self.cursor != 0 && self.elements[self.cursor].0 > beats {
            self.cursor -= 1;
        }
        let (start_beats, time, bpm) = &self.elements[self.cursor];
        time + (beats - start_beats) * (60. / bpm)
    }

    /// Get the time in seconds for a given `i + n / d`
    /// [`BpmList::time_beats`] 的便利封装，直接接受谱面原生的分数拍号。
    pub fn time(&mut self, triple: &Triple) -> f64 {
        self.time_beats(triple.beats())
    }

    /// Get the beat coordinate for a given time in seconds
    /// [`BpmList::time_beats`] 的逆运算：秒 -> 节拍坐标。
    ///
    /// 与正向换算的唯一区别是游标推进/回退时比较的是 `time` 字段
    /// （`elements[i].1`），它同样随下标单调递增，因此双向扫描策略依然成立。
    pub fn beat(&mut self, time: f64) -> f64 {
        while let Some(kf) = self.elements.get(self.cursor + 1) {
            if kf.1 > time {
                break;
            }
            self.cursor += 1;
        }
        while self.cursor != 0 && self.elements[self.cursor].1 > time {
            self.cursor -= 1;
        }
        let (beats, start_time, bpm) = &self.elements[self.cursor];
        beats + (time - start_time) / (60. / bpm)
    }
}
