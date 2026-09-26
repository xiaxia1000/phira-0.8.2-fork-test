//! Judgement system
//!
//! # 概览
//!
//! 本模块是 Phira 玩法判定的唯一实现，负责把「原始输入」变成「每个 Note 的判定结果」，
//! 整体流水线分为四步：
//!
//! 1. 输入采集：`Handler` 重放 miniquad 在本帧累积的所有输入事件，把触屏、鼠标、滚轮
//!    与键盘事件统一归一化到线程局部的 `TOUCHES` / `WHEEL` 中；
//! 2. 坐标归一化：先把屏幕像素坐标换算到游戏的归一化坐标系（x ∈ [-1, 1]，
//!    y ∈ [-1/aspect_ratio, 1/aspect_ratio]），必要时做水平镜像，再用判定线自身变换的逆矩阵
//!    （`now_transform().try_inverse()`）反变换到**判定线局部坐标**——只有这样旋转 / 平移 /
//!    缩放的判定线才能被正确判定；
//! 3. 时空匹配：在每条判定线上按「时间差 + 横向偏离代价」搜索最合适的 Note，产出 `Judgement`；
//! 4. 统计与结算：由 `JudgeInner` 维护 combo / 各判定计数 / 准确率 / 分数，最终产出 `PlayResult`。
//!
//! # 时间单位约定（重要）
//!
//! 谱面时间（`Note::time`、`res.time` 等）是「谱面秒」；比较前统一除以 `speed`（代码中的
//! `dt / spd`）换算为**现实秒**，所以 `LIMIT_PERFECT` / `LIMIT_GOOD` / `LIMIT_BAD` 等判定窗口
//! 常量的单位是「玩家实际感知到的秒数」，与选手设置的谱面流速无关：调高 speed 时谱面时间
//! 流逝更快，但判定窗口的体感宽度保持不变。
//!
//! # 闭源分支
//!
//! `cfg(closed)` 时 `JudgeInner` 会被替换成 `inner` 模块中的实现（联机 / 服务端专用，
//! 含 HoldPerfect / HoldGood 这类只在协议层存在的区分），本文件其余逻辑不受影响。
//!
//! # 联机协议补充
//!
//! 本地判定只有 4 种结果（见 `Judgement`）；Hold 的起手完美性通过 `Result::Err(bool)`
//! 另行携带，只有在生成联机 / 结算记录时才会被翻译成 HoldPerfect / HoldGood。

use crate::{
    config::Config,
    core::{BadNote, Chart, NoteKind, Point, Resource, Vector, NOTE_WIDTH_RATIO_BASE},
    ext::{get_viewport, NotNanExt},
};
use macroquad::prelude::{
    utils::{register_input_subscriber, repeat_all_miniquad_input},
    *,
};
use miniquad::{EventHandler, MouseButton};
use once_cell::sync::Lazy;
use sasa::{PlaySfxParams, Sfx};
use serde::Serialize;
use std::{cell::RefCell, collections::HashMap, mem, num::FpCategory};
use tracing::debug;

/// Flick 手势的基础速度阈值，单位「归一化坐标 / 秒」。
///
/// 实际使用的阈值还要乘以 `dpi / 386.`（见 `FlickTracker::new`），此处 0.8 是基准密度下的
/// 经验值：约为「0.8 个归一化横坐标单位 / 秒」。数值取得偏大的原因是 Flick 是唯一要求
/// 主动甩动的 note 类型，阈值过低会让普通按住 / 滑动误触发。
pub const FLICK_SPEED_THRESHOLD: f32 = 0.8;
/// Perfect 判定窗口半径，单位：现实秒（已除以 speed 后的时间差绝对值）。
/// 0.08s 约为 80ms，是 Phigros 系手感里「明显偏早 / 偏晚但仍算完美」的临界值；
/// 它同时被复用为若干派生阈值（Click 的窗口收紧、Hold 起手 perfect 判定）。
pub const LIMIT_PERFECT: f64 = 0.08;
/// Good 判定窗口半径，单位：现实秒。
/// 恰为 `LIMIT_PERFECT * 2`：Good 覆盖 Perfect 之外到 ±0.16s 的范围；Flick / Drag
/// 这类「触碰即算」的 note 只放宽到该窗口（见 `update` 中的候选筛选）。
pub const LIMIT_GOOD: f64 = 0.16;
/// Bad 判定窗口半径，单位：现实秒，同时也是 **Miss 的判定阈值**。
/// 超出 ±0.22s 仍未命中即判 Miss；Click 类 note 在 Good 与 Bad 之间还会按横向偏离
/// 进一步收紧（越偏越难判到好成绩）。
pub const LIMIT_BAD: f64 = 0.22;
/// Hold 的抬手容忍时间，单位：现实秒。
/// 手指离开判定区后，只有在 0.05s 之内没有回到判定区才会断连；这个宽限是为了兼容
/// 触屏采样抖动、以及玩家按住时手指轻微滑动导致的瞬时空隙。
pub const UP_TOLERANCE: f64 = 0.05;
/// 横向偏离折算成时间代价的系数（单位：秒 / 个音符宽度）。
/// 多个候选 Note 竞争同一次点击时，用 `key = 时间差 + 横向超出代价` 选最优，
/// 每偏离一个音符宽度折算 0.2s 的时间代价——即「横向偏得越远，需要时间上越准」。
pub const DIST_FACTOR: f64 = 0.2;

/// 早期命中（提前按下）的偏移补偿，单位：现实秒，仅在本模块内使用。
///
/// 玩家普遍会「提前预判」打击（视觉判定线与音频不同步、或单纯习惯性抢拍），
/// 因此对 `dt < 0` 的时间差统一加上 0.07s 再取值：相当于把负向（偏早）窗口整体后移
/// 0.07s，使得提前 0.07s 内视为 Perfect、更早的提前量才按 Good/Bad 计算，
/// 而偏晚方向不做补偿（偏晚更容易被玩家感知，不需要额外宽容）。
const EARLY_OFFSET: f64 = 0.07;

/// 命中音效的来源。
///
/// 优先级约定：谱面（RPE 等格式）可以为单个 Note 指定自定义音效文件，解析时这些 clip 会被
/// 加载进 `Resource::extra_sfxs`（见 `GameScene::new`），此时使用 `Custom`；未指定时由
/// `default_from_kind` 按 Note 类型回退到资源包内置的三种音效。
#[derive(Debug, Clone)]
pub enum HitSound {
    /// 静音：既没有自定义音效，也不使用内置音效的 Note（例如被谱面显式关闭打击音）。
    None,
    /// 内置点击音，对应资源包 `sfx_click`（Click / Hold 命中时使用）。
    Click,
    /// 内置滑动音，对应资源包 `sfx_flick`（划动 note 被甩中时使用）。
    Flick,
    /// 内置拖拽音，对应资源包 `sfx_drag`（Drag note 判定时使用）。
    Drag,
    /// 谱面自定义音效，字符串是 `Resource::extra_sfxs` 中的文件名（不含扩展名）；
    /// 若加载失败或键不存在，播放时静默跳过而不是报错（判定流程不应因音效失败而中断）。
    Custom(String),
}

// 命中音效的播放实现与「按 Note 类型取默认音效」的映射规则。
impl HitSound {
    /// 播放该命中音效。
    ///
    /// `Custom` 变体在 `extra_sfxs` 中找不到对应项时直接静默返回：音效属于表现层，
    /// 缺失不应影响判定结果（这也是这里不返回 `Result` 的原因）。
    pub fn play(&self, res: &mut Resource) {
        match self {
            HitSound::None => {}
            HitSound::Click => play_sfx(&mut res.sfx_click, &res.config),
            HitSound::Flick => play_sfx(&mut res.sfx_flick, &res.config),
            HitSound::Drag => play_sfx(&mut res.sfx_drag, &res.config),
            HitSound::Custom(s) => {
                if let Some(sfx) = res.extra_sfxs.get_mut(s) {
                    play_sfx(sfx, &res.config);
                }
            }
        }
    }

    /// 按 Note 类型给出默认命中音效，用于谱面未指定自定义音效的场景。
    ///
    /// Hold 使用 Click 音（只在按住的那一刻播放一次，尾判不发声），这是 Phigros 的既定表现。
    pub fn default_from_kind(kind: &NoteKind) -> Self {
        match kind {
            NoteKind::Click => HitSound::Click,
            NoteKind::Flick => HitSound::Flick,
            NoteKind::Drag => HitSound::Drag,
            NoteKind::Hold { .. } => HitSound::Click,
        }
    }
}

/// 播放一个音效，并统一套用配置里的音效音量。
///
/// 音量小于等于 1e-2 时直接跳过：一是玩家用 0 音量表示静音，二是极小的 amplifier 在某些
/// 音频后端上等价于静音却仍要走一遍混音路径，这里用阈值直接省掉开销。
/// 播放失败（后端已满 / 已关闭）时返回值被有意忽略——判定流程不能因为播放失败而中断。
pub fn play_sfx(sfx: &mut Sfx, config: &Config) {
    if config.volume_sfx <= 1e-2 {
        return;
    }
    let _ = sfx.play(PlaySfxParams {
        amplifier: config.volume_sfx,
    });
}

// 单调时钟读取：把触摸事件携带的原始时间戳换算成「距当前时刻过去了多久」。
// 之所以按平台拆成三份实现，是因为各平台事件时间戳的基准不同（POSIX 单调时钟 / iOS 系统
// 运行时长 / Windows 由 miniquad 封装的计数器），但都必须满足「只增不减、不受用户改系统
// 时间影响」这一前提，否则触摸延迟换算会出现负数或跳变。
/// 读取单调时间（秒），POSIX 平台实现。
///
/// 使用 `CLOCK_MONOTONIC` 而非墙钟时间：玩家调整系统时间不应影响触摸时间戳的换算结果。
/// `clock_gettime` 一旦失败（理论上仅在极端内核异常时发生）直接断言失败 panic——
/// 此时触摸延迟会完全失真，静默继续会造成难以排查的误判。
#[cfg(all(not(target_os = "windows"), not(target_os = "ios")))]
fn get_uptime() -> f64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `time` 是本函数栈上初始化的合法 `timespec`，其指针在调用期间始终有效；
    // `CLOCK_MONOTONIC` 是合法的时钟 id。该调用只写入 `time`，不持有任何跨调用状态。
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    assert!(ret == 0);
    time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9
}

/// 读取单调时间（秒），iOS 实现。
///
/// iOS 上不便直接使用 POSIX 时钟，`systemUptime` 语义等价（系统启动以来的秒数，单调递增）。
#[cfg(target_os = "ios")]
fn get_uptime() -> f64 {
    objc2_foundation::NSProcessInfo::processInfo().systemUptime()
}

/// 读取单调时间（秒），Windows 实现。
///
/// 直接复用 miniquad 的 Windows 封装（内部为高精度性能计数器），避免自写 FFI。
#[cfg(target_os = "windows")]
fn get_uptime() -> f64 {
    miniquad::native::windows::get_uptime()
}

/// 单个触摸点（手指 / 鼠标）的 Flick 手势识别器。
///
/// Phira 的 Flick note 并不依赖引擎原生的「滑动」事件，而是自己逐帧采样触摸位置，
/// 用「方向一致性 + 速度阈值 + 必须先静止」这组启发式条件判断玩家是否做出了甩动动作。
/// 每个触摸 id 在被按下时创建一个实例，抬手时销毁（生命周期见 `Judge::update`）。
///
/// 设计取舍与局限：
/// - 只用「上一次位移方向」与「当前位移」的点积衡量方向一致性，转急弯时点积为负，
///   会被判成减速而非甩动；
/// - `new` 中的 dpi 被硬编码（源码留存 TODO），因此阈值在所有设备上是同一个经验值，
///   与真实屏幕密度无关；
/// - 甩动状态 `flicked` 是「一次性」的，被判定逻辑消费后必须复位，否则一次甩动会连续
///   命中接下来经过判定区的所有 Flick note。
pub struct FlickTracker {
    /// 触发甩动所需的速度阈值，单位「归一化坐标 / 秒」；由硬编码 dpi 与
    /// `FLICK_SPEED_THRESHOLD` 换算而来（≈ 0.57）。
    threshold: f32,
    /// 上一次采样到的位置（屏幕归一化坐标，已含 flip_x）。
    last_point: Point,
    /// 上一次位移的**单位向量**；`None` 表示尚无历史方向可比较（首个采样点）。
    last_delta: Option<Vector>,
    /// 上一次采样的时间（与 `push` 的 time 同一基准），用于求速度。
    last_time: f32,
    /// 本轮按下期间是否已经识别出一次甩动。置 true 后需由判定逻辑显式复位，
    /// 保证「一次甩动只命中一个 Flick note」。
    flicked: bool,
    /// 上一次采样时是否处于「近乎静止」状态。只有「先静止、后快速移动」才算甩动，
    /// 用于排除持续滑动过程中零散的速度波动。
    stopped: bool,
}

// 手势识别器的构造与逐帧采样推进；触发条件与阈值来源见各方法注释。
impl FlickTracker {
    /// 创建跟踪器，初始状态为「静止」。
    ///
    /// # Arguments
    /// * `_dpi` — 调用方传入的真实屏幕 DPI，**当前被忽略**：内部硬编码 275
    ///   （源码中的 `// TODO maybe a better approach?` 即指此处）。副作用是阈值在不同
    ///   DPI 设备上物理含义不同，但好处是各平台甩动手感统一，不会在高分屏上要求玩家
    ///   甩得更快。
    /// * `time` — 起始采样时间，必须与后续 `push` 的时间同基准。
    /// * `point` — 起始位置（屏幕归一化坐标）。
    pub fn new(_dpi: u32, time: f32, point: Point) -> Self {
        // TODO maybe a better approach?
        let dpi = 275;
        Self {
            threshold: FLICK_SPEED_THRESHOLD * dpi as f32 / 386.,
            last_point: point,
            last_delta: None,
            last_time: time,
            flicked: false,
            stopped: true,
        }
    }

    /// 推入一个新的采样点，更新速度判定与甩动状态。
    ///
    /// 判定过程（全部基于归一化坐标与秒）：
    /// 1. `delta = 本次位置 - 上次位置`，即本段位移向量；同时把 `last_point` 前移；
    /// 2. 若存在上次移动方向，则计算 `speed = delta · last_delta / dt`——即本段位移在
    ///    **上次移动方向**上的投影速度。方向一致时为正，反向 / 急转弯时可为负；
    /// 3. `speed < threshold`（≈ 0.57 单位/秒）视为减速，把状态标记回「静止」；
    /// 4. 只有「此前处于静止」且「本轮按下还没触发过甩动」时，才用位移**模长**速度
    ///    `|delta| / dt >= threshold * 2`（≈ 1.14 单位/秒）确认甩动。取 2 倍门槛是留出余量，
    ///    避免擦边速度导致的时灵时不灵；
    /// 5. 记录归一化后的位移方向供下一帧比较。
    ///
    /// 注意 `dt` 由调用方保证为正（同一帧内的多个事件会在 `Judge::update` 中被均匀铺开时间），
    /// 因此这里不做除零保护。
    pub fn push(&mut self, time: f32, position: Point) {
        let delta = position - self.last_point;
        self.last_point = position;
        if let Some(last_delta) = &self.last_delta {
            let dt = time - self.last_time;
            let speed = delta.dot(last_delta) / dt;
            if speed < self.threshold {
                self.stopped = true;
            }
            if self.stopped && !self.flicked {
                self.flicked = delta.magnitude() / dt >= self.threshold * 2.;
            }
            // if speed < self.threshold || self.stopped {
            // self.stopped = delta.magnitude() / dt < self.threshold * 5.;
            // self.flicked = self.threshold <= speed;
            // if self.flicked {
            // warn!("new flick!");
            // }
            // }
        }
        self.last_delta = Some(delta.normalize());
        self.last_time = time;
    }
}

/// 单个 Note 的判定生命周期状态，存放于 `Note::judge`。
///
/// 关键设计：**没有独立的 HoldPerfect / HoldGood 判定结果**。Hold 的「完美性」被编码在
/// `Hold` 变体的第一个 `bool` 里；只有当结果以 `Result::Err(bool)` 提交给 `Judge::judgements`
/// 时（联机 / 成绩记录场景），这个 bool 才会被翻译成协议层的 HoldPerfect / HoldGood 区分。
/// 本地统计（`JudgeInner::commit`）里 Hold 只按 Perfect 或 Good 计数。
///
/// 状态的迁移只允许沿 `NotJudged → (PreJudge) → Judged` 单向推进，
/// 这是「一个 Note 只能被判定一次」这一不变量的实现方式。
#[derive(Debug)]
pub enum JudgeStatus {
    /// 尚未被任何输入触及，也还没超时，可以被本帧的输入匹配。
    NotJudged,
    /// 已被输入「预判」但还没有定论。三种进入方式：
    /// 1. Drag / Flick 被触摸到（注意 Flick 需要后续确认甩动，见 `update` 的 PreJudge 结算）；
    /// 2. Hold 正在被按住（起手已判定，等待尾判）；
    /// 3. Click 已经被判 Bad 但尚未超出窗口——保留该状态是为了**防止同一个 Note 被重复判定**，
    ///    而不是为了继续尝试命中。
    PreJudge,
    /// 判定已完成（命中、Miss、或 Hold 尾判结束），不再参与任何后续匹配。
    Judged,
    /// Hold 专用状态。
    ///
    /// 字段依次为（源码注释：`// perfect, at, diff, pre-judge, up-time`）：
    /// - `bool`：起手是否落在 Perfect 窗口内；
    /// - 第 1 个 `f64`：起手判定的时刻（谱面时间）；
    /// - 第 2 个 `f64`：起手的时间差 `diff`，正负分别表示偏晚 / 偏早；
    /// - 第 2 个 `bool`：是否已进入尾判 / 预结算阶段（尾判窗口内或已抬手超时）；
    /// - 第 3 个 `f64`：手指离开判定区的时间，`f64::INFINITY` 表示当前仍按住；
    ///   配合 `UP_TOLERANCE` 实现「抬手宽容」。
    Hold(bool, f64, f64, bool, f64), // perfect, at, diff, pre-judge, up-time
}

/// 一次判定的结果，本地只有这四种。
///
/// `#[repr(u8)]` + 声明顺序构成了一组**隐式约定的下标**：`Perfect/Good/Bad/Miss` 分别对应
/// 0/1/2/3，被广泛用作 `JudgeInner::counts`、`early_kind`、`late_kind` 等数组的下标
/// （代码里的 `what as usize`）。因此**调整变体顺序会静默破坏统计、分数与结算图标**。
/// 该类型还需要 `Serialize`，用于成绩 / 联机记录的上报。
#[repr(u8)]
#[derive(Debug, Copy, Clone, Serialize)]
pub enum Judgement {
    /// 命中时间差不超过 `LIMIT_PERFECT`，计满分权重 1.0。
    Perfect,
    /// 命中时间差超过 Perfect 但在 Good 窗口内，计权重 0.65（见 `JudgeInner::accuracy`）。
    Good,
    /// 命中但明显偏离（超出 Good 窗口、或 Click 因横向偏离被收紧后落在 Bad 内）。
    /// Phigros 规则下 Bad 会断连但不视为 Miss，本实现按「已判定」处理。
    Bad,
    /// 未命中（超时、Hold 断连等）。`JudgeInner::commit` 中 Miss 以 diff = 0.25 提交，
    /// 因此统计里所有 Miss 都会被计入「偏晚」一侧。
    Miss,
}

// 本地（开源构建）的判定统计容器。closed 构建下由 `inner` 模块中的同名类型取代，
// 以承载联机 / 服务端所需的额外统计（例如 Hold 的 Perfect/Good 细分）。
/// 判定统计：连击、各类计数、准确率与分数。
///
/// 所有计数字段都由 `commit` 单点写入，`reset` 单点清零，
/// 保证「统计只增不减、重开时整体归零」这一不变量的实现集中在一处。
#[cfg(not(closed))]
#[derive(Default)]
pub(crate) struct JudgeInner {
    /// 所有 Good 判定的时间差（正晚负早）。**只记录 Good**：它既是准确率之外的
    /// 手感分布数据，也是结算页展示「偏早 / 偏晚」数量的依据（见 `result`）。
    diffs: Vec<f64>,

    /// 当前连击数。仅 Perfect / Good 递增，其余判定清零。
    combo: u32,
    /// 本局历史最大连击，用于分数中的连击权重与结算页展示。
    max_combo: u32,
    /// 各判定结果的数量，下标语义见 `Judgement`（0=Perfect, 1=Good, 2=Bad, 3=Miss）。
    counts: [u32; 4],
    /// 全谱非 fake note 总数，作为准确率 / 分数的分母；在 `Judge::new` 时一次性统计。
    num_of_notes: u32,
    /// 各判定结果中「偏早」的数量，下标同 `counts`。
    early_kind: [u32; 4],
    /// 各判定结果中「偏晚」的数量，下标同 `counts`。
    late_kind: [u32; 4],
}

// 统计容器的构造、落账、清零与派生指标（准确率 / 分数 / 结算结果）。
// 计分口径：准确率权重刻意不用 1.0，而是把 Good 折成 0.65，以拉开「全 Perfect」与
// 「全 Good」之间的区分度；总分再按 9:1 混合准确率与最大连击。
#[cfg(not(closed))]
impl JudgeInner {
    /// 创建一个全新的统计容器。
    ///
    /// # Arguments
    /// * `num_of_notes` — 全谱非 fake note 总数，用作准确率与分数的分母；
    ///   调用方（`Judge::new`）负责排除 fake note，使分母与玩家实际需要打的 note 数一致。
    pub fn new(num_of_notes: u32) -> Self {
        Self {
            diffs: Vec::new(),

            combo: 0,
            max_combo: 0,
            counts: [0; 4],
            num_of_notes,
            early_kind: [0; 4],
            late_kind: [0; 4],
        }
    }

    /// 记录一次判定结果并更新连击。
    ///
    /// # Arguments
    /// * `what` — 判定结果，同时作为各计数数组的下标（依赖 `Judgement` 的判别式顺序）。
    /// * `diff` — 该次判定的时间差（现实秒，正晚负早）。调用方约定的取值：
    ///   Miss 固定传 0.25，Drag / Flick 传 0.0，其余传实际时间差；
    ///   因此「早 / 晚」统计里 Miss 恒记为偏晚、Drag/Flick 不计入任何一侧（`diff == 0.` 时
    ///   两个分支都不命中，这是有意的——无方向信息的命中不该污染早晚分布）。
    ///
    /// 副作用有两类：把 Good 的时间差追加进 `diffs` 供结算分析；以及维护 combo——
    /// **只有 Bad / Miss 会把 combo 清零**，Perfect / Good 都会递增并顺带刷新 `max_combo`。
    pub fn commit(&mut self, what: Judgement, diff: f64) {
        use Judgement::*;
        if matches!(what, Judgement::Good) {
            self.diffs.push(diff);
        }
        if diff < 0. {
            self.early_kind[what as usize] += 1;
        } else if diff > 0. {
            self.late_kind[what as usize] += 1;
        }
        self.counts[what as usize] += 1;
        match what {
            Perfect | Good => {
                self.combo += 1;
                if self.combo > self.max_combo {
                    self.max_combo = self.combo;
                }
            }
            _ => {
                self.combo = 0;
            }
        }
    }

    /// 把本局统计清零，用于重开 / 跳段。
    ///
    /// 注意 `num_of_notes` **不**被清零：它描述的是谱面本身，与游玩进度无关，
    /// 重开同一张谱时它保持不变（这也是分母不会随重开而错乱的原因）。
    pub fn reset(&mut self) {
        self.combo = 0;
        self.max_combo = 0;
        self.counts = [0; 4];
        self.diffs.clear();
        self.early_kind = [0; 4];
        self.late_kind = [0; 4];
    }

    /// 结算用准确率：`(Perfect + 0.65 * Good) / 谱面总 note 数`。
    ///
    /// 分母是全谱 note 数而非已判定数，因此中途退出 / 未打完的进度会被按缺失计入，
    /// 得到偏低的准确率——这正是结算页想要的结果。
    /// Good 的权重 0.65 是本项目的既定口径（不是 0.5，也不是 1.0）：既让 Good 明显低于
    /// Perfect，又不会让差一个 Good 就掉太多准确率。
    ///
    /// 边界：若谱面 note 数为 0（空谱或全 fake），分母为 0，返回 NaN。
    pub fn accuracy(&self) -> f64 {
        (self.counts[0] as f64 + self.counts[1] as f64 * 0.65) / self.num_of_notes as f64
    }

    /// 实时准确率：与 `accuracy` 同口径，但分母换成**已判定数**。
    ///
    /// 供 HUD 实时显示用（玩家希望看到「目前打了的部分有多准」）；还没开始判定时
    /// 已判定数为 0，此时约定返回 1.0 而不是 NaN，避免 HUD 显示 NaN。
    pub fn real_time_accuracy(&self) -> f64 {
        let cnt = self.counts.iter().sum::<u32>();
        if cnt == 0 {
            return 1.;
        }
        (self.counts[0] as f64 + self.counts[1] as f64 * 0.65) / cnt as f64
    }

    /// 计算分数，满分 1000000。
    ///
    /// 规则：
    /// - 全部 Perfect 时**直接返回满分**，避免浮点误差导致 999999 这种「明明全完美却不是
    ///   满分」的结果（同时保证满分与 `icon_index` 的满分分支一致）；
    /// - 否则 `(0.9 * 准确率 + 0.1 * max_combo / 谱面 note 数) * 1000000`，即准确率占九成、
    ///   连击占一成——沿用 Phigros 的计分思路，既奖励精度也奖励稳定性；
    /// - 结果四舍五入到整数。
    ///
    /// 边界：空谱（`num_of_notes == 0`）会命中满分分支直接返回 1000000。
    pub fn score(&self) -> u32 {
        const TOTAL: u32 = 1000000;
        if self.counts[0] == self.num_of_notes {
            TOTAL
        } else {
            let score = (0.9 * self.accuracy() + self.max_combo as f64 / self.num_of_notes as f64 * 0.1) * TOTAL as f64;
            score.round() as u32
        }
    }

    /// 汇总为可供结算页 / 成绩上传使用的结果结构。
    ///
    /// `early` / `late` 全部由 `diffs`（**只有 Good 的时间差**）推导，因此这两个字段描述的是
    /// 「Good 判定里偏早 / 偏晚各有多少」，而不是所有判定的早晚分布（后者请用 `early_kind`
    /// 与 `late_kind`）。`std` 目前恒为 0，属于预留字段。
    pub fn result(&self) -> PlayResult {
        let early = self.diffs.iter().filter(|it| **it < 0.).count() as u32;
        PlayResult {
            score: self.score(),
            accuracy: self.accuracy(),
            max_combo: self.max_combo,
            num_of_notes: self.num_of_notes,
            counts: self.counts,
            early,
            late: self.diffs.len() as u32 - early,
            std: 0.,
            early_kind: self.early_kind,
            late_kind: self.late_kind,
        }
    }

    /// 当前连击数，供 HUD 显示。
    pub fn combo(&self) -> u32 {
        self.combo
    }

    /// 各类判定计数（0=Perfect, 1=Good, 2=Bad, 3=Miss），供 HUD 与断连判定使用。
    pub fn counts(&self) -> [u32; 4] {
        self.counts
    }
}

// 闭源构建专有：判定统计被换成 `inner` 模块的实现。该模块不随源码分发，
// 因此这里用 `mod inner;` 声明并在文件内 `use inner::*;` 引入同名类型，
// 让 `JudgeInner` / `PlayResult` 等名字在两种构建下保持同一套调用点。
#[rustfmt::skip]
#[cfg(closed)]
pub mod inner;
#[cfg(closed)]
use inner::*;

/// 判定事件队列的元素类型：`(判定时刻, 判定线 id, note id, 结果)`。
///
/// 时间戳为判定发生时的谱面时间；`Result` 用于承载「本地四分类之外」的附加信息：
/// - `Ok(Judgement)`：普通判定（Click / Drag / Flick / Miss），四种结果之一；
/// - `Err(bool)`：**Hold 的起手判定**，`bool` 表示起手是否 Perfect。
///   本地统计不区分 Hold 的优劣，因此需要把这份信息单独放进队列，供成绩记录 /
///   联机上报翻译成 HoldPerfect / HoldGood。
type Judgements = Vec<(f64, u32, u32, Result<Judgement, bool>)>;

/// 一个判定线的判定器集合，是玩法系统的状态中枢。
///
/// 这里只保存「判定所需的最小状态」，谱面数据本身仍由 `Chart` 持有（`update` 会同时接收
/// `&mut Chart`）；`Judge` 内的 `notes` 只是每条判定线上**按时间排序的 note 下标列表 + 游标**，
/// 用于把每帧的搜索范围限制在「尚未判定的前缀」上，避免每帧扫描整张谱面。
///
/// 生命周期不变量：`update` 结束时游标只会前进，`last_time` 只会增大；
/// `reset` 与 `advance_to` 是唯一会回退 / 跳跃这两个量的入口。
///
/// 另注：结构体上的 `#[repr(C)]` 并不对应任何 C ABI 交互（内部字段是 `Vec` / `HashMap`
/// 等 Rust 私有布局类型），属于沿用历史写法。
#[repr(C)]
pub struct Judge {
    // notes of each line in order
    // LinkedList::drain_filter is unstable...
    /// 每条判定线一项：`(按时间升序排列的非 fake note 下标, 游标)`。
    ///
    /// 用「下标数组 + 游标」而不是链表，是因为 `LinkedList::drain_filter` 至今不稳定
    /// （源码原注释即此意）；游标之前的 note 一定已经判定完成，不再参与搜索。
    pub notes: Vec<(Vec<u32>, usize)>,
    /// 每个活跃触摸 id 对应的 Flick 手势跟踪器；按下时插入，抬手时移除。
    pub trackers: HashMap<u64, FlickTracker>,
    /// 上一次判定推进到的「已除以 speed 的现实时间」（见 `update` 末尾写入 `t / spd`），
    /// 用于把本帧内多个输入事件的时间均匀铺开，避免同一帧的事件被当成同一时刻。
    pub last_time: f64,

    /// 当前按下的键数（键盘判定用）。由 `key_delta` 加减维护，
    /// 用 `saturating_add_signed` 防止按键事件丢失导致的负数（见 `update`）。
    key_down_count: u32,

    /// 判定统计，闭源构建下为 `inner` 模块中的实现。
    pub(crate) inner: JudgeInner,
    /// 待消费的判定事件队列；用 `RefCell` 是因为提交判定时只能拿到 `&self` 的场景
    /// （例如在遍历谱面的过程中）也需要写队列。
    pub judgements: RefCell<Judgements>,
}

/// 单帧内的原始输入状态，由 `Handler` 填充、`Judge::update` 消费。
///
/// 该结构每帧都会重新构造（`Judge::on_new_frame`），因此其中只保存**本帧新增**的事件：
/// `touches` 是本帧收到的触摸 / 鼠标事件（可能同一 id 多条），`keys_down` 是本帧新按下的
/// 键数，`key_delta` 是本帧按键数的净变化。
#[derive(Default)]
struct TouchStatus {
    /// 本帧收到的原始触摸 / 鼠标事件，坐标是屏幕像素。
    touches: Vec<Touch>,
    /// 本帧按键数的净变化（按下 +1、抬起 -1），用于维护跨帧的 `Judge::key_down_count`。
    key_delta: i32,
    /// 本帧**新按下**的键数（不随抬起减少，因为结构体每帧重建），
    /// 决定键盘判定这次要尝试命中多少个 note。
    keys_down: u32,
}

/// macroquad 输入订阅者的编号。
///
/// macroquad 允许多个订阅者各自消费同一份输入事件队列；这里注册一个专用订阅者，
/// 用 `repeat_all_miniquad_input` 在每帧开始时重放「上次读取之后」累积的事件，
/// 从而拿到带原始时间戳的触摸数据（比 `macroquad::input` 的逐帧布尔查询更精确，
/// 尤其能区分一帧内的多次点击）。
static SUBSCRIBER_ID: Lazy<usize> = Lazy::new(register_input_subscriber);
// 线程局部的输入缓冲：判定逻辑只在渲染 / 主线程运行，用 thread_local 省掉同步开销，
// 也避免与 macroquad 自身的线程局部输入状态竞争。
thread_local! {
    // 本帧的触摸 / 键盘原始状态，由 `Judge::on_new_frame` 写入，`Judge::update` 读取。
    static TOUCHES: RefCell<TouchStatus> = RefCell::default();
    // 本帧累计的滚轮位移，用于谱面预览等 UI；同样每帧重建（`take_wheel` 取走即清零）。
    static WHEEL: RefCell<(f32, f32)> = RefCell::default();
}

/// 取走并清空本帧累计的滚轮位移 `(x, y)`。
///
/// 采用「取走即清零」而不是直接读值，是为了让滚轮事件像触摸事件一样只被消费一次，
/// 避免同一次滚动被多个 UI 组件重复响应。
pub fn take_wheel() -> (f32, f32) {
    WHEEL.with(|it| mem::take(&mut *it.borrow_mut()))
}

// 判定的对外接口：初始化、重置 / 跳段、手动判定主循环 `update`、自动演示 `auto_play_update`，
// 以及把统计指标转发给 `JudgeInner` 的若干薄封装。
impl Judge {
    /// 依据谱面建立判定器索引。
    ///
    /// 为每条判定线建立一份「非 fake note 的按时间升序下标表」，并把游标初始化为 0。
    /// fake note（视觉装饰 / 仅用于演出）不参与判定，因此从一开始就被排除，
    /// 这也保证了 `JudgeInner::num_of_notes` 与玩家需要打的 note 数一致。
    ///
    /// 排序键上使用 `not_nan()`（把时间包成 `NotNan`）：既然排序依赖 `f64` 比较，
    /// NaN 会让顺序不确定，这里选择在谱面数据非法时直接 panic 而不是给出随机结果。
    ///
    /// # Arguments
    /// * `chart` — 已解析完成、`lines[i].notes` 可直接索引的谱面。
    pub fn new(chart: &Chart) -> Self {
        let notes = chart
            .lines
            .iter()
            .map(|line| {
                let mut idx: Vec<u32> = (0..(line.notes.len() as u32)).filter(|it| !line.notes[*it as usize].fake).collect();
                idx.sort_by_key(|id| line.notes[*id as usize].time.not_nan());
                (idx, 0)
            })
            .collect();
        Self {
            notes,
            trackers: HashMap::new(),
            last_time: 0.,

            key_down_count: 0,

            inner: JudgeInner::new(chart.lines.iter().map(|it| it.notes.iter().filter(|it| !it.fake).count() as u32).sum()),
            judgements: RefCell::new(Vec::new()),
        }
    }

    /// 重置判定器，用于重开。
    ///
    /// 只清空判定侧的进度（游标、手势跟踪器、统计、待消费事件），**不改动谱面中 note 的
    /// `judge` 字段**——那部分由 `Chart::reset` 负责（见 `GameScene` 的 `reset!` 宏，
    /// 两者总是一起调用）。`last_time` 不在此处归零，由 `advance_to` 或下一次 `update` 校正。
    pub fn reset(&mut self) {
        self.notes.iter_mut().for_each(|it| it.1 = 0);
        self.trackers.clear();
        self.inner.reset();
        self.judgements.borrow_mut().clear();
    }

    /// 把游标推进到时间 `t` 之后，并把沿途的 note 直接标记为已判定。
    ///
    /// 用于练习模式跳段：从段落起点开始时，玩家本就不打算打前面的 note；如果只推进游标
    /// 而不改状态，这些 note 会被后续的 Miss 扫描逐个判为 Miss，造成大量无意义的失分与断连。
    /// 因此这里把它们统一置为 `Judged`（不计入 counts / combo，等价于「不存在」）。
    ///
    /// 注意比较用的是谱面时间，而 `Judge::update` 末尾写入的 `last_time` 是 `t / spd`：
    /// 两处量纲不同，调用方传入的 `t` 应与 `res.time` 同源（见 `GameScene::overlay_ui`）。
    ///
    /// # Arguments
    /// * `chart` — 判定线顺序必须与 `self.notes` 一一对应（由 `Judge::new` 建立）。
    /// * `t` — 段落起点（谱面时间）。
    /// Advance note pointers past notes before time `t`, marking them as judged.
    /// Used in exercise mode to skip notes before the exercise range start.
    pub fn advance_to(&mut self, chart: &mut Chart, t: f64) {
        for (line, (idx, st)) in chart.lines.iter_mut().zip(self.notes.iter_mut()) {
            while *st < idx.len() {
                let note = &mut line.notes[idx[*st] as usize];
                if note.time >= t {
                    break;
                }
                note.judge = JudgeStatus::Judged;
                *st += 1;
            }
        }
        self.last_time = t;
    }

    /// 提交一次普通判定（写入待消费事件队列并更新统计）。
    ///
    /// 这里只处理 `Ok(Judgement)`；Hold 起手走另一条路径（直接向 `self.judgements` 压入
    /// `Err(bool)`，因为它还要携带「起手是否 Perfect」这一额外信息，见 `update`）。
    ///
    /// # Arguments
    /// * `t` — 判定发生时刻（谱面时间）。
    /// * `what` — 判定结果。
    /// * `line_id` / `note_id` — 定位被判定的 note，供上层做特效 / 回放。
    /// * `diff` — 时间差（现实秒，正晚负早），仅用于统计早晚分布（`JudgeInner::commit`）。
    pub fn commit(&mut self, t: f64, what: Judgement, line_id: u32, note_id: u32, diff: f64) {
        self.judgements.borrow_mut().push((t, line_id, note_id, Ok(what)));
        self.inner.commit(what, diff);
    }

    /// 结算准确率（口径见 `JudgeInner::accuracy`）。
    #[inline]
    pub fn accuracy(&self) -> f64 {
        self.inner.accuracy()
    }

    /// 实时准确率，供 HUD 显示（口径见 `JudgeInner::real_time_accuracy`）。
    #[inline]
    pub fn real_time_accuracy(&self) -> f64 {
        self.inner.real_time_accuracy()
    }

    /// 当前分数（满分 1000000，口径见 `JudgeInner::score`）。
    #[inline]
    pub fn score(&self) -> u32 {
        self.inner.score()
    }

    /// 每帧开始时采集输入事件，写入线程局部缓冲。
    ///
    /// 流程：
    /// 1. 新建一个空的 `Handler`（它同时是 miniquad 的事件回调接收器）；
    /// 2. `repeat_all_miniquad_input` 把**订阅者上次读取之后**累积的所有原始事件重放给
    ///    `Handler`，因此哪怕一帧内发生了多次点击 / 抬起，也不会被丢弃或合并；
    /// 3. `finalize` 补一条「鼠标仍按住」的事件（鼠标按住时不会再产生事件，
    ///    但对 Hold 来说必须每帧都有触摸点存在）；
    /// 4. 结果整份替换进 `TOUCHES` / `WHEEL`，保证每帧只被消费一次。
    ///
    /// 之所以在每帧开头统一采集而不在事件回调里直接判定，是为了让判定逻辑保持
    /// 「单线程、按帧推进」的确定性模型。
    pub(crate) fn on_new_frame() {
        let mut handler = Handler {
            status: TouchStatus::default(),
            wheel: (0., 0.),
        };
        repeat_all_miniquad_input(&mut handler, *SUBSCRIBER_ID);
        handler.finalize();
        TOUCHES.with(|it| {
            *it.borrow_mut() = handler.status;
        });
        WHEEL.with(|it| {
            *it.borrow_mut() = handler.wheel;
        });
    }

    /// 生成一个把屏幕像素坐标变换到游戏归一化坐标的闭包。
    ///
    /// 变换规则：
    /// * x：视口左边缘 → -1、右边缘 → +1；
    /// * y：先处理坐标系差异——`Touch::position` 的原点在屏幕左上角，而 GL 视口
    ///   `(x, y, w, h)` 的原点在左下角，故用 `screen_height() - (vp.1 + vp.3)` 换算出视口
    ///   顶边在屏幕坐标系中的位置；再线性映射到 [-1, 1]，最后除以宽高比，使 y 落在
    ///   [-1/aspect_ratio, 1/aspect_ratio]，与谱面 / 判定线使用的坐标尺度一致；
    /// * `flip_x`：谱面开启左右镜像时触摸 x 也要取反，否则镜像出的谱面与输入对不上。
    ///
    /// 视口在闭包创建时读取一次（`get_viewport`），所以同一帧内视口不能再变——
    /// 这正是 `GameScene::update` 必须在调用判定前临时切换视口的原因。
    fn touch_transform(flip_x: bool) -> impl Fn(&mut Touch) {
        let vp = get_viewport();
        move |touch| {
            let p = touch.position;
            touch.position = vec2(
                (p.x - vp.0 as f32) / vp.2 as f32 * 2. - 1.,
                ((p.y - (screen_height() - (vp.1 + vp.3) as f32)) / vp.3 as f32 * 2. - 1.) / (vp.2 as f32 / vp.3 as f32),
            );
            if flip_x {
                touch.position.x *= -1.;
            }
        }
    }

    /// 读取本帧的触摸事件（归一化坐标，**不做 flip_x 镜像**）。
    ///
    /// 供 UI 命中测试使用：UI 坐标系与屏幕一致（不镜像），所以这里固定传 `false`；
    /// 判定用的触摸则另经 `touch_transform(res.config.flip_x())` 处理。
    /// 返回克隆值，调用方可以随意修改（例如缩放到 UI 空间）。
    pub fn get_touches() -> Vec<Touch> {
        TOUCHES.with(|it| {
            let guard = it.borrow();
            let tr = Self::touch_transform(false);
            guard
                .touches
                .iter()
                .cloned()
                .map(|mut it| {
                    tr(&mut it);
                    it
                })
                .collect()
        })
    }

    /// 判定主循环：每帧调用一次，把本帧输入转换成 note 的判定结果。
    ///
    /// # 判定链（按执行顺序）
    ///
    /// 1. **autoplay 短路**：开启自动演示时整个流程交给 `auto_play_update`，不采集真实输入；
    /// 2. **准备**：取谱面流速 `spd` 与单调时钟起点，并确定横向容差 `X_DIFF_MAX`；
    /// 3. **采集触摸**：把触屏事件与 macroquad 的鼠标状态合成同一份触摸表，统一做归一化变换；
    /// 4. **键盘**：读取本帧新按键数 `keys_down` 与净变化 `key_delta`；
    /// 5. **维护 FlickTracker 与触摸表**：把本帧事件在时间轴上均匀铺开，逐个更新手势识别器；
    /// 6. **时刻换算**：用 `uptime` 把触摸的硬件时间戳换算成谱面时间；
    /// 7. **坐标反变换**：对每条判定线求 `now_transform().try_inverse()`，把触摸点映射到
    ///    判定线局部坐标，得到 `pos[line][touch]`；
    /// 8. **Click / Flick 候选搜索与提交**：按「时间差 + 横向偏离代价」取最优候选；
    /// 9. **键盘判定**：每个新按键尝试命中一个最早的 Click / Hold；
    /// 10. **Hold / Drag / Flick 逐帧维护**：处理按住状态、抬手宽容与 Miss；
    /// 11. **PreJudge 结算**：把预判成功的 note 转为最终判定；
    /// 12. **统一提交**：写统计、生成打击特效、播放命中音效；
    /// 13. **游标推进与时间基准更新**。
    ///
    /// # 时序不变量
    ///
    /// - `self.last_time` 单调不减，单位是「除以 speed 后的现实时间」；
    /// - 一个 note 每帧至多被判定一次：离开 `NotJudged` 后本帧后续逻辑不会再命中它；
    /// - 触摸坐标的换算依赖调用瞬间的视口，因此调用方必须在正确的视口下调用本函数
    ///   （见 `GameScene::update`）。
    ///
    /// # Arguments
    /// * `res` — 运行期资源与配置（流速、autoplay、flip_x、音量、DPI 等）。
    /// * `chart` — 谱面；本函数会直接写回 `note.judge`，并读取判定线变换。
    /// * `bad_notes` — 输出参数：被判 Bad 的 note 会被追加进去用于绘制 Bad 提示。
    pub fn update(&mut self, res: &mut Resource, chart: &mut Chart, bad_notes: &mut Vec<BadNote>) {
        // ---- 阶段 1：autoplay 短路 ----
        // 自动演示时完全不读取真实输入，直接走 `auto_play_update`，
        // 因此后续所有触摸采集 / 键盘判定逻辑在 autoplay 下都不会被执行。
        if res.config.autoplay() {
            self.auto_play_update(res, chart);
            return;
        }
        // ---- 阶段 2：准备本帧的时间基准与横向容差 ----
        // X_DIFF_MAX 是「触摸点到 note 中心的横向距离（已按 note.judge_area 归一化）」的上限。
        // 推导：0.21 取自 16:9 基准下屏幕高度的 0.21 倍，而归一化坐标里屏幕高度等于 x 轴上的
        // 2/aspect，故换算到 x 轴为 0.21 / (16/9) * 2 ≈ 0.236。写成常量而非按当前宽高比实时计算，
        // 是为了让判定手感与窗口形状解耦（玩家拉伸窗口不会改变判定宽度）。
        const X_DIFF_MAX: f64 = 0.21 / (16. / 9.) * 2.;
        let spd = res.config.speed as f64;

        // uptime 与触摸事件携带的时间戳同源，用于把「事件发生在何时」换算成「距今多久」。
        let uptime = get_uptime();

        // t 是本帧的谱面时间；涉及时间差的比较都会再除以 spd 换算成现实时间。
        let t = res.time;
        // TODO optimize
        // ---- 阶段 3：合成统一的触摸表（触屏事件 + 鼠标状态） ----
        // macroquad 的鼠标 API 是「逐帧查询式」的（本帧是否按下 / 移动 / 抬起），
        // 这里主动把它合成为 Touch，使鼠标与触屏在后续流程中走完全相同的代码路径。
        let mut touches: HashMap<u64, Touch> = {
            let mut touches = touches();
            let btn = MouseButton::Left;
            // 用 button_to_id 生成与真实触摸 id 不可能冲突的鼠标 id（u64::MAX - n）。
            let id = button_to_id(btn);
            // 合成的鼠标事件没有硬件时间戳，一律用 NEG_INFINITY 表示
            // 「当作当前帧时刻」，后续 `time_of` 会把它替换成 t。
            if is_mouse_button_pressed(btn) {
                let p = mouse_position();
                touches.push(Touch {
                    id,
                    phase: TouchPhase::Started,
                    position: vec2(p.0, p.1),
                    time: f64::NEG_INFINITY,
                });
            } else if is_mouse_button_down(btn) {
                let p = mouse_position();
                touches.push(Touch {
                    id,
                    phase: TouchPhase::Moved,
                    position: vec2(p.0, p.1),
                    time: f64::NEG_INFINITY,
                });
            } else if is_mouse_button_released(btn) {
                let p = mouse_position();
                touches.push(Touch {
                    id,
                    phase: TouchPhase::Ended,
                    position: vec2(p.0, p.1),
                    time: f64::NEG_INFINITY,
                });
            }
            // 统一做「像素 → 归一化坐标」变换（含 flip_x），并改成以 id 为键的表：
            // 同一帧内同一 id 的多个事件会互相覆盖，最终只保留最后一个（最新）状态。
            let tr = Self::touch_transform(res.config.flip_x());
            touches
                .into_iter()
                .map(|mut it| {
                    tr(&mut it);
                    (it.id, it)
                })
                .collect()
        };
        // ---- 阶段 4：读取本帧的键盘事件 ----
        // `use_keyboard` 关闭时把键数一律视为 0，等价于彻底禁用键盘判定；
        // keys_down 是「本帧新按下的键数」，下面会用它逐个尝试命中 note。
        let (events, keys_down, key_delta) = TOUCHES.with(|it| {
            let guard = it.borrow();
            let events = guard.touches.clone();
            if res.config.use_keyboard {
                (events, guard.keys_down, guard.key_delta)
            } else {
                (events, 0, 0)
            }
        });
        // 跨帧维护「当前按住的键数」：用饱和加减避免事件丢失导致的下溢 panic；
        // 该值 != 0 表示「有键按住」，会让 Hold 保持不断、并允许 Flick 被键盘触发。
        self.key_down_count = self.key_down_count.saturating_add_signed(key_delta);
        // ---- 阶段 5：铺开本帧事件的时间轴，并维护 FlickTracker ----
        // 同一帧内到达的多个事件如果共用同一时间戳，速度计算会出现 dt = 0（除零 / 无穷速度），
        // 因此把本帧经过的时间 (t/spd - last_time) 按事件数均分，让每个事件落在不同时刻。
        {
            // 注意：这里的归一化只按整块屏幕换算、**不感知视口**，与 `touch_transform` 不同。
            // 由于同一 id 通常已被上面带视口的变换插入到表中，`or_insert_with` 只在
            // 该 id 缺失时才启用这条退化路径；FlickTracker 只关心位移方向，因此不受影响。
            fn to_local(Vec2 { x, y }: Vec2) -> Point {
                Point::new(x / screen_width() * 2. - 1., y / screen_height() * 2. - 1.)
            }
            let delta = (t / spd - self.last_time) / (events.len() + 1) as f64;
            let mut t = self.last_time;
            for Touch {
                id,
                phase,
                position: p,
                time,
            } in events.into_iter()
            {
                t += delta;
                let t = t as f32;
                let p = to_local(p);
                match phase {
                    // 按下：为该触摸 id 新建手势识别器；同时保证触摸表里存在这个 id，
                    // 并把它的 phase 显式改为 Started（覆盖掉 macroquad 上报的其它阶段）。
                    TouchPhase::Started => {
                        self.trackers.insert(id, FlickTracker::new(res.dpi, t, p));
                        touches
                            .entry(id)
                            .or_insert_with(|| Touch {
                                id,
                                phase: TouchPhase::Started,
                                position: vec2(p.x, p.y),
                                time,
                            })
                            .phase = TouchPhase::Started;
                    }
                    // 移动 / 静止：推进手势识别（静止也要推进，否则速度会一直沿用旧值）。
                    TouchPhase::Moved | TouchPhase::Stationary => {
                        if let Some(tracker) = self.trackers.get_mut(&id) {
                            tracker.push(t, p);
                        }
                    }
                    // 抬手 / 取消：销毁跟踪器，避免 id 被系统复用时沿用旧手势状态。
                    TouchPhase::Ended | TouchPhase::Cancelled => {
                        self.trackers.remove(&id);
                    }
                }
            }
        }
        // ---- 阶段 6：把触摸时间戳换算成谱面时间 ----
        // 无穷时间戳（鼠标合成事件）保持 NEG_INFINITY，留给 `time_of` 当作「当前帧」；
        // 其余事件按「距离当前时刻已经过去多少秒」换算：谱面时间 = t - 延迟 * spd。
        let touches: Vec<Touch> = touches
            .into_values()
            .map(|mut it| {
                it.time = if it.time.is_infinite() {
                    f64::NEG_INFINITY
                } else {
                    t - (uptime - it.time) * spd
                };
                it
            })
            .collect();
        // ---- 阶段 7：把触摸点反变换到每条判定线的局部坐标系 ----
        // 判定线带有旋转 / 平移 / 缩放 / 倾斜，直接用世界坐标比较横向位置会判错，
        // 因此对每条线求其变换的逆矩阵，把触摸点映射进该线自己的坐标系。
        // `try_inverse` 仅在矩阵退化（不可逆）时返回 None，判定线变换理论上总是可逆，故 unwrap。
        // pos[line][touch]
        let mut pos = Vec::<Vec<Option<Point>>>::with_capacity(chart.lines.len());
        for id in 0..chart.lines.len() {
            chart.lines[id].object.set_time(t);
            let inv = chart.lines[id].now_transform(res, &chart.lines).try_inverse().unwrap();
            pos.push(
                touches
                    .iter()
                    .map(|touch| {
                        let p = touch.position;
                        // 触摸坐标的 y 轴朝下，判定线坐标朝上，故取反后再做逆变换。
                        let p = inv.transform_point(&Point::new(p.x, -p.y));
                        // 只接受有限值（零 / 次正规 / 正规），NaN 与 ±inf 一律视为「不在这条线上」，
                        // 避免后续比较出现 NaN 传染导致判定静默失效。
                        fn ok(f: f32) -> bool {
                            matches!(f.classify(), FpCategory::Zero | FpCategory::Subnormal | FpCategory::Normal)
                        }
                        if ok(p.x) && ok(p.y) {
                            Some(p)
                        } else {
                            None
                        }
                    })
                    .collect(),
            );
        }
        // 把触摸时刻归一成「用于比较的谱面时间」：鼠标等合成事件的时间戳是无穷，
        // 统一取当前帧时间 t（鼠标状态本来就只有「本帧」的概念）。
        let time_of = |touch: &Touch| {
            if touch.time.is_infinite() {
                t
            } else {
                touch.time
            }
        };
        // ---- 阶段 8：Click / Flick 的候选搜索与提交 ----
        // 本帧产生的判定先收集到 judgements，最后统一提交，
        // 这样统计、特效与音效可以集中在阶段 12 处理，前面的搜索逻辑可以自由读写 note.judge。
        let mut judgements = Vec::new();
        // clicks & flicks
        for (id, touch) in touches.iter().enumerate() {
            // 只有两类事件能触发判定：
            // - click：本帧刚按下的触摸（一次按下只判定一次）；
            // - flick：正在移动 / 静止且跟踪器已经识别出甩动的手势
            //   （`get_mut` 顺带确认该触摸仍有活跃跟踪器）。
            let click = touch.phase == TouchPhase::Started;
            let flick =
                matches!(touch.phase, TouchPhase::Moved | TouchPhase::Stationary) && self.trackers.get_mut(&touch.id).is_some_and(|it| it.flicked);
            if !(click || flick) {
                continue;
            }
            let t = time_of(touch);
            // 候选变量 `(选中的 (line_id, note_id), 横向偏离, 时间差, 综合代价 key)`。
            // 初值第 4 项是「横向偏离取到上限 X_DIFF_MAX 时可能出现的最大 key」，
            // 用作剪枝上界：只要 key 小于它就有机会入选，从而不会漏掉任何候选。
            let mut closest = (None, X_DIFF_MAX, LIMIT_BAD, LIMIT_BAD + (X_DIFF_MAX / NOTE_WIDTH_RATIO_BASE - 1.).max(0.) * DIST_FACTOR);
            for (line_id, ((line, pos), (idx, st))) in chart.lines.iter_mut().zip(pos.iter()).zip(self.notes.iter_mut()).enumerate() {
                // 该触摸点无法映射到这条判定线（逆变换退化）时整条线跳过。
                let Some(pos) = pos[id] else {
                    continue;
                };
                // 只扫描游标之后的 note：游标之前的必定已判定完成。
                for id in &idx[*st..] {
                    let note = &mut line.notes[*id as usize];
                    // 只有还没定论（NotJudged）或已进入预判（PreJudge）的 note 可以被同一帧内
                    // 后续的输入继续命中；已判定的直接跳过，保证一次判定只落一次账。
                    if !matches!(note.judge, JudgeStatus::NotJudged | JudgeStatus::PreJudge) {
                        continue;
                    }
                    // 非点击事件（即甩动）只允许命中 Drag / Flick：
                    // Flick 不能被单纯点击命中，Click / Hold 也必须由点击触发。
                    if !click && matches!(note.kind, NoteKind::Click | NoteKind::Hold { .. }) {
                        continue;
                    }
                    // dt > 0 表示 note 尚未到达（玩家提前按下）；单位：现实秒。
                    let dt = (note.time - t) / spd;
                    // note 时间升序，一旦与当前候选的综合代价持平或更差就无需继续
                    // （更靠后的 note 时间差只会更大）。
                    if dt >= closest.3 {
                        break;
                    }
                    // 早期命中补偿：偏早时把时间差向 0 收敛 0.07s（见 EARLY_OFFSET），
                    // 偏晚则不做任何补偿。
                    let dt = if dt < 0. { (dt + EARLY_OFFSET).min(0.).abs() } else { dt };
                    // 取该 note 在当前时刻的横向位置，按 note.judge_area 归一化得到横向偏离比例。
                    // judge_area 是 RPE 谱面可定制的横向判定宽度（其它格式固定为 1.0），
                    // 因此 dist 是「相对该 note 自身判定宽度」的比例值。
                    let x = &mut note.object.translation.0;
                    x.set_time(t);
                    let dist = (x.now() - pos.x).abs() as f64 / note.judge_area as f64;
                    // 横向太远直接淘汰（X_DIFF_MAX ≈ 0.236，即约 1.8 个音符宽度）。
                    if dist > X_DIFF_MAX {
                        continue;
                    }
                    // 时间窗口判定：Click 的窗口是 Bad，并附带一个随横向偏离收紧的惩罚项
                    // `LIMIT_PERFECT * (dist - 0.9).max(0.)`；由于 dist 的上限 X_DIFF_MAX ≈ 0.236
                    // 恒小于 0.9，该惩罚项在当前取值范围内恒为 0，等效于固定窗口 LIMIT_BAD。
                    // 其它类型（Drag / Flick / Hold）只放宽到 Good，超出即放弃本次候选。
                    if dt
                        > if matches!(note.kind, NoteKind::Click) {
                            LIMIT_BAD - LIMIT_PERFECT * (dist - 0.9).max(0.)
                        } else {
                            LIMIT_GOOD
                        }
                    {
                        continue;
                    }
                    // Drag / Flick 是「碰到即可」的类型：把有效时间差整体推后一个 Good 窗口，
                    // 使其在比较 key 时不会因为「接触必然发生在 note 时间之前」而系统性输给 Click。
                    let dt = if matches!(note.kind, NoteKind::Flick | NoteKind::Drag) {
                        dt + LIMIT_GOOD
                    } else {
                        dt
                    };
                    // 综合代价 key = （归一化后的）时间差 + 横向超出「一个音符宽度」的惩罚。
                    // 横向与时间不可直接比较，这里用 DIST_FACTOR 把横向偏离折算成时间代价；
                    // 取 key 最小者，等价于「优先时间准，横向偏得远就必须时间更准」。
                    let key = dt + (dist / NOTE_WIDTH_RATIO_BASE - 1.).max(0.) * DIST_FACTOR;
                    if key < closest.3 {
                        closest = (Some((line_id, *id)), dist, dt, key);
                    }
                }
            }
            // 提交本触摸的候选：
            // - Drag 只接受划动命中，被点击「拒绝」，且不消费这次点击（后续 note 仍可参与）；
            // - Flick 必须由甩动触发，单纯点击不会命中（见下面的 continue）。
            if let (Some((line_id, id)), _, dt, _) = closest {
                let line = &mut chart.lines[line_id];
                // Drag 必须被「划过」而不是被「点中」：直接 continue 而不是标记已判定，
                // 保证同一条判定线上紧随其后的其它 note 仍能吃掉这次点击。
                if matches!(line.notes[id as usize].kind, NoteKind::Drag) {
                    debug!("reject by drag");
                    continue;
                }
                if click {
                    // click & hold
                    let note = &mut line.notes[id as usize];
                    // Flick 不能被点击命中：跳过这次点击（让别的线 / 别的 note 有机会），
                    // 甩动命中会走下面的 else 分支。
                    if matches!(note.kind, NoteKind::Flick) {
                        continue; // to next loop
                    }
                    // 判定窗口：Hold 只要落在 Bad 内就算起手成功（起手的完美性另记于 Hold 状态），
                    // Click 则需要落在 Good 内，否则按 Bad 处理。
                    if dt <= LIMIT_GOOD || matches!(note.kind, NoteKind::Hold { .. }) {
                        match note.kind {
                            NoteKind::Click => {
                                note.judge = JudgeStatus::Judged;
                                judgements.push((if dt <= LIMIT_PERFECT { Judgement::Perfect } else { Judgement::Good }, line_id, id, Some(t)));
                            }
                            NoteKind::Hold { .. } => {
                                // Hold 起手成功：命中音效立即播放；判定结果以 Err(bool) 单独入队
                                // （bool 为「起手是否 Perfect」，供联机协议区分 HoldPerfect/HoldGood），
                                // 随后把状态切到 Hold，交给阶段 10 的按住 / 尾判逻辑继续处理。
                                note.hitsound.play(res);
                                self.judgements.borrow_mut().push((t, line_id as _, id, Err(dt <= LIMIT_PERFECT)));
                                note.judge = JudgeStatus::Hold(dt <= LIMIT_PERFECT, t, t, false, f64::INFINITY);
                            }
                            _ => unreachable!(),
                        };
                    } else {
                        // prevent extra judgements
                        // 只有仍是 NotJudged 才落账：PreJudge / Judged 说明该 note 已被处理过，
                        // 重复提交会污染统计与「偏早/偏晚」分布。
                        if matches!(note.judge, JudgeStatus::NotJudged) {
                            // keep the note after bad judgement
                            line.notes[id as usize].judge = JudgeStatus::PreJudge;
                            judgements.push((Judgement::Bad, line_id, id, None));
                        }
                    }
                } else {
                    // flick
                    // 甩动命中：Flick / Drag 不区分 Perfect/Good，这里只置为预判，
                    // 真正的判定在阶段 11 的 PreJudge 结算里统一给出。随后消费掉 tracker 的
                    // flicked 标记，避免一次甩动连续命中接下来经过判定区的多个 Flick note。
                    line.notes[id as usize].judge = JudgeStatus::PreJudge;
                    if let Some(tracker) = self.trackers.get_mut(&touch.id) {
                        tracker.flicked = false;
                    }
                }
            }
        }
        // ---- 阶段 9：键盘判定 ----
        // 键盘被当作「一根可以同时按下的虚拟手指」：本帧新按下几次键，就尝试命中几个 note。
        // 由于 Handler::key_down_event 不区分键码（见其注释），这里无法按 note 颜色 / 键位做分配，
        // 策略退化为：先对每条判定线取「最早的、仍是 NotJudged 的 Click / Hold」，
        // 再在所有线之间取全局时间最早者；注意这里每次循环只判一个 note 且不消耗触摸。
        for _ in 0..keys_down {
            // find the earliest not judged click / hold note
            // 若一次都没找到候选，说明键盘已无可判定的 note，提前结束循环。
            if let Some((line_id, id)) = chart
                .lines
                .iter()
                .zip(self.notes.iter())
                .enumerate()
                .filter_map(|(line_id, (line, (idx, st)))| {
                    idx[*st..]
                        .iter()
                        .cloned()
                        .find(|id| {
                            let note = &line.notes[*id as usize];
                            matches!(note.judge, JudgeStatus::NotJudged) && matches!(note.kind, NoteKind::Click | NoteKind::Hold { .. })
                        })
                        .map(|id| (line_id, id))
                })
                .min_by_key(|(line_id, id)| chart.lines[*line_id].notes[*id as usize].time.not_nan())
            {
                let note = &mut chart.lines[line_id].notes[id as usize];
                // 键盘命中用「绝对时间差」：提前与延后共用同一窗口，且不套用触摸路径的
                // EARLY_OFFSET 偏移补偿（键盘没有手指遮挡判定线的视觉问题，不需要额外宽容）。
                let dt = (t - note.time).abs() / spd;
                // 窗口与触摸路径保持一致：Click 用 Bad，Hold 用 Good（超过窗口则本次按键不命中）。
                if dt <= if matches!(note.kind, NoteKind::Click) { LIMIT_BAD } else { LIMIT_GOOD } {
                    match note.kind {
                        NoteKind::Click => {
                            note.judge = JudgeStatus::Judged;
                            judgements.push((
                                if dt <= LIMIT_PERFECT {
                                    Judgement::Perfect
                                } else if dt <= LIMIT_GOOD {
                                    Judgement::Good
                                } else {
                                    Judgement::Bad
                                },
                                line_id,
                                id,
                                None,
                            ));
                        }
                        NoteKind::Hold { .. } => {
                            note.hitsound.play(res);
                            self.judgements.borrow_mut().push((t, line_id as _, id, Err(dt <= LIMIT_PERFECT)));
                            note.judge = JudgeStatus::Hold(dt <= LIMIT_PERFECT, t, t, false, f64::INFINITY);
                        }
                        _ => unreachable!(),
                    };
                }
            } else {
                break;
            }
        }
        // ---- 阶段 10：Hold / Drag / Flick 的逐帧维护与 Miss 扫描 ----
        // 这些 note 的判定依赖「持续接触」而不是单次点击，必须每帧重新检查：
        // 已进入 Hold 的 note 检查是否还按着；尚未判定的 note 检查是否已超时（Miss）
        // 或是否满足 Drag / Flick 的接触条件（置 PreJudge）。
        for (line_id, ((line, pos), (idx, st))) in chart.lines.iter_mut().zip(pos.iter()).zip(self.notes.iter()).enumerate() {
            line.object.set_time(t);
            for id in &idx[*st..] {
                let note = &mut line.notes[*id as usize];
                // 已经进入 Hold 状态的 note：先看是否已到尾判窗口，
                // 若还没到就检查「是否仍然按着」，否则按 Miss / 抬手宽容处理。
                if let NoteKind::Hold { end_time, .. } = &note.kind {
                    if let JudgeStatus::Hold(.., ref mut pre_judge, ref mut up_time) = note.judge {
                        // 距离 Hold 结束不足一个 Bad 窗口即视为进入尾判阶段，
                        // 交给阶段 11 在 `end_time` 到达时结算（这里不再做按住判定）。
                        if (*end_time - t) / spd <= LIMIT_BAD {
                            *pre_judge = true;
                            continue;
                        }
                        let x = &mut note.object.translation.0;
                        x.set_time(t);
                        let x = x.now();
                        // 「是否还按着」的判定：键盘按住（key_down_count != 0）视为一直按着；
                        // 触屏则要求存在某个触摸点落在该 note 的横向容差内。
                        // 条件整体取反，所以这里一旦为真就表示「松手了 / 手指移出了判定区」。
                        if self.key_down_count == 0
                            && !pos
                                .iter()
                                .any(|it| it.is_some_and(|it| (it.x - x).abs() as f64 / note.judge_area as f64 <= X_DIFF_MAX))
                        {
                            // 松手：在 UP_TOLERANCE 宽限内只记录抬手时刻（up_time），
                            // 超过宽限仍没回来才断连判 Miss——抬手瞬间不立即判 Miss 是为了容忍触摸抖动。
                            if t > *up_time + UP_TOLERANCE {
                                note.judge = JudgeStatus::Judged;
                                judgements.push((Judgement::Miss, line_id, *id, None));
                            } else if up_time.is_infinite() {
                                *up_time = t;
                            }
                        } else {
                            // 手指重新回到判定区：清掉抬手时间，恢复「正在按住」的状态。
                            *up_time = f64::INFINITY;
                        }
                        continue;
                    }
                }
                // 走到这里说明不是 Hold（或 Hold 尚未进入状态）：Judged 直接跳过，
                // PreJudge 留给阶段 11 结算，只有 NotJudged 才需要在下面尝试 Miss / 预判。
                if !matches!(note.judge, JudgeStatus::NotJudged) {
                    continue;
                }
                // process miss
                // dt > 0 表示当前时间已超过 note 时间；超出 Bad 窗口即失去命中机会，判 Miss。
                let dt = (t - note.time) / spd;
                if dt > LIMIT_BAD {
                    note.judge = JudgeStatus::Judged;
                    judgements.push((Judgement::Miss, line_id, *id, None));
                    continue;
                }
                // note 还远未到达（连 Bad 窗口都没进）：该线与 self.notes 都按时间升序，
                // 后续 note 只会更晚，直接结束这条线的扫描（是 break 而非 continue）。
                if -dt > LIMIT_BAD {
                    break;
                }
                // 只有 Drag / Flick 能靠「接触」进入预判：Click / Hold 必须由阶段 8 / 9 命中，
                // 这里直接跳过；而 Flick 在「只有键盘按住」时也不预判（键盘做不出甩动动作）。
                if !matches!(note.kind, NoteKind::Drag) && (self.key_down_count == 0 || !matches!(note.kind, NoteKind::Flick)) {
                    continue;
                }
                // 下面用 |dt| 与横向距离判断「是否接触到了这个 note」。
                // 注意时间窗口里的收紧项 `LIMIT_PERFECT * (dx - 0.9).max(0.)` 与阶段 8 同理，
                // 在 dx <= X_DIFF_MAX ≈ 0.236 的取值范围内恒为 0，实际就是 LIMIT_BAD。
                let dt = dt.abs();
                let x = &mut note.object.translation.0;
                x.set_time(t);
                let x = x.now();
                // 判定条件：键盘按住（视为接触），或存在触摸点既在横向容差内、又在时间窗口内。
                // 命中后只置 PreJudge——Drag / Flick 不区分 Perfect/Good，
                // 统一由阶段 11 的预判结算给出最终结果。
                if self.key_down_count != 0
                    || pos.iter().any(|it| {
                        it.is_some_and(|it| {
                            let dx = (it.x - x).abs() as f64 / note.judge_area as f64;
                            dx <= X_DIFF_MAX && dt <= (LIMIT_BAD - LIMIT_PERFECT * (dx - 0.9).max(0.))
                        })
                    })
                {
                    note.judge = JudgeStatus::PreJudge;
                }
            }
        }
        // ---- 阶段 11：PreJudge 结算 ----
        // 预判生效的 note 在这里转为最终判定：Hold 需要尾判（到达 end_time）才算完成；
        // Drag / Flick 直接结算为 Perfect；被置为 PreJudge 的 Click 则保持不再被判定的状态。
        // process pre-judge
        for (line_id, (line, (idx, st))) in chart.lines.iter_mut().zip(self.notes.iter()).enumerate() {
            line.object.set_time(t);
            for id in &idx[*st..] {
                let note = &mut line.notes[*id as usize];
                // Hold 已进入尾判：到达 end_time 即完成，结果按起手的完美性给 Perfect / Good
                // （本地不产出 HoldPerfect / HoldGood，那只是协议层的区分）。
                if let JudgeStatus::Hold(perfect, .., diff, true, _) = note.judge {
                    if let NoteKind::Hold { end_time, .. } = &note.kind {
                        if *end_time <= t {
                            note.judge = JudgeStatus::Judged;
                            judgements.push((if perfect { Judgement::Perfect } else { Judgement::Good }, line_id, *id, Some(diff)));
                            continue;
                        }
                    }
                }
                // 下面处理 Drag / Flick 的预判结算。
                // ghost_t 给 Click 留出一个 Good 宽度的「幽灵窗口」：被判 Bad 后保留为 PreJudge 的
                // Click 必须等到窗口结束才结算，避免抢在玩家可能的后续输入之前把结果定死。
                // TODO adjust
                let ghost_t = t + LIMIT_GOOD;
                if matches!(note.kind, NoteKind::Click) {
                    if ghost_t < note.time {
                        break;
                    }
                } else if t < note.time {
                    continue;
                }
                // 预判成功且已到结算时机：置为已判定。
                // Hold 的 diff 需要一并带出（它的最终成绩由起手时间差决定）；
                // Click 不在这里产出判定——它早在阶段 8 就完成了，走到这里说明只是被置成了 PreJudge。
                if matches!(note.judge, JudgeStatus::PreJudge) {
                    let diff = if let JudgeStatus::Hold(.., diff, _, _) = note.judge {
                        Some(diff)
                    } else {
                        None
                    };
                    note.judge = JudgeStatus::Judged;
                    if !matches!(note.kind, NoteKind::Click) {
                        judgements.push((Judgement::Perfect, line_id, *id, diff));
                    }
                }
            }
        }
        // ---- 阶段 12：统一提交本帧的所有判定 ----
        // 到这一步才写统计、生成特效、播放音效：前面的搜索逻辑因此可以自由地反复读写
        // `note.judge` 而不会产生副作用（音效与粒子只会表现一次）。
        for (judgement, line_id, id, diff) in judgements {
            let line = &mut chart.lines[line_id];
            let note = &mut line.notes[id as usize];
            // 先把判定线与 note 的动画时间推进到本帧，才能取到正确的变换用于特效定位。
            line.object.set_time(t);
            note.object.set_time(t);
            let line = &chart.lines[line_id];
            let note = &line.notes[id as usize];
            let line_tr = line.now_transform(res, &chart.lines);
            // diff 的口径（只影响「偏早 / 偏晚」统计）：Miss 固定 0.25（恒记为偏晚）、
            // Drag / Flick 记 0（无方向信息）、其余用真实时间差（现实秒）。
            self.commit(
                t,
                judgement,
                line_id as _,
                id,
                if matches!(judgement, Judgement::Miss) {
                    0.25
                } else if matches!(note.kind, NoteKind::Drag | NoteKind::Flick) {
                    0.
                } else {
                    (diff.unwrap_or(t) - note.time) / spd
                },
            );
            // Hold 的打击特效与音效已在起手时给出，这里直接跳过，避免尾判时重复表现。
            if matches!(note.kind, NoteKind::Hold { .. }) {
                continue;
            }
            // 按判定结果生成打击特效，并决定是否播放命中音效：
            // Perfect / Good 立即发声并喷出粒子，Bad 只记录到 `bad_notes`（不发声），Miss 无表现。
            if match judgement {
                Judgement::Perfect => {
                    res.with_model(line_tr * note.object.now(res), |res| {
                        res.emit_at_origin(note.rotation(line), note.fx_color.unwrap_or_else(|| res.res_pack.info.fx_perfect()))
                    });
                    true
                }
                Judgement::Good => {
                    res.with_model(line_tr * note.object.now(res), |res| {
                        res.emit_at_origin(note.rotation(line), note.fx_color.unwrap_or_else(|| res.res_pack.info.fx_good()))
                    });
                    true
                }
                Judgement::Bad => {
                    // Bad 提示：记录一份「脱离判定线」的 note 快照，供 GameScene 逐帧绘制成提示动画。
                    // 矩阵构造 = 判定线变换 →（note 位于判定线下方时）垂直翻转 → note 自身变换
                    // （含高度差按宽高比与速度折算的位移，以及判定线倾斜的影响），
                    // 目的是让提示图形的朝向与位置和 note 的实际表现一致。
                    if !matches!(note.kind, NoteKind::Hold { .. }) {
                        bad_notes.push(BadNote {
                            time: t,
                            kind: note.kind.clone(),
                            matrix: {
                                let mut mat = line_tr;
                                if !note.above {
                                    mat.append_nonuniform_scaling_mut(&Vector::new(1., -1.));
                                }
                                let incline_sin = line.incline.now_opt().map(|it| it.to_radians().sin()).unwrap_or_default();
                                mat *= note.now_transform(
                                    res,
                                    &line.ctrl_obj.borrow_mut(),
                                    ((note.height - line.height.now() as f64) / res.aspect_ratio as f64 * note.speed) as f32,
                                    incline_sin,
                                );
                                mat
                            },
                        });
                    }
                    false
                }
                _ => false,
            } {
                note.hitsound.play(res);
            }
        }
        // ---- 阶段 13：推进游标并更新时间基准 ----
        // 把每条判定线上开头已经判完的 note 移出搜索窗口（游标只前进、不回退）。
        for (line, (idx, st)) in chart.lines.iter().zip(self.notes.iter_mut()) {
            while idx
                .get(*st)
                .is_some_and(|id| matches!(line.notes[*id as usize].judge, JudgeStatus::Judged))
            {
                *st += 1;
            }
        }
        // 记录「已除以 speed 的现实时间」，作为下一帧铺开事件时间的基准。
        self.last_time = t / spd;
    }

    /// 自动演示（autoplay）的判定更新，与手动判定的差异如下：
    ///
    /// - 不读取任何输入，也不需要触摸 / 键盘状态；
    /// - note 在到达时刻（`note.time <= t`）直接判为 Perfect，不会出现 Good/Bad/Miss，
    ///   因此 autoplay 天然是满分全连（这也让 `Mods::AUTOPLAY` 被归入 `Mods::UNRATED`，
    ///   成绩不予记录）；
    /// - Hold 依旧是「按住 → 到达 end_time 后完成」的两段式，只是起手固定为 Perfect；
    /// - 特效一律使用 Perfect 的配色，不做 Bad 提示。
    ///
    /// 复用同一套「游标 + 按时间升序扫描」的结构，保证 autoplay 与手动判定的推进逻辑一致。
    fn auto_play_update(&mut self, res: &mut Resource, chart: &mut Chart) {
        let t = res.time;
        let spd = res.config.speed as f64;
        // 非 Hold 的判定先收集起来，最后统一提交（提交时还要取 note 变换来定位特效）。
        let mut judgements = Vec::new();
        for (line_id, (line, (idx, st))) in chart.lines.iter_mut().zip(self.notes.iter_mut()).enumerate() {
            for id in &idx[*st..] {
                let note = &mut line.notes[*id as usize];
                // Hold 已完成按住阶段：到达 end_time 即结束并计入 Perfect 特效。
                if let JudgeStatus::Hold(..) = note.judge {
                    if let NoteKind::Hold { end_time, .. } = note.kind {
                        if t >= end_time {
                            note.judge = JudgeStatus::Judged;
                            judgements.push((line_id, *id));
                            continue;
                        }
                    }
                }
                // 已判定的 note 跳过；note 尚未到达则结束本条线的扫描（时间升序）。
                if !matches!(note.judge, JudgeStatus::NotJudged) {
                    continue;
                }
                if note.time > t {
                    break;
                }
                // Hold 与其它 note 的处理不同：Hold 进入「按住」状态并立刻记录一次 Err(true)
                // （起手固定 Perfect），等 end_time 到达后在循环开头结算；
                // 其余 note 直接判为 Perfect 并进入待提交列表。
                note.judge = if matches!(note.kind, NoteKind::Hold { .. }) {
                    note.hitsound.play(res);
                    self.judgements.borrow_mut().push((t, line_id as _, *id, Err(true)));
                    JudgeStatus::Hold(true, t, (t - note.time) / spd, false, f64::INFINITY)
                } else {
                    judgements.push((line_id, *id));
                    JudgeStatus::Judged
                };
            }
            while idx
                .get(*st)
                .is_some_and(|id| matches!(line.notes[*id as usize].judge, JudgeStatus::Judged))
            {
                *st += 1;
            }
        }
        // 逐个提交并生成特效：diff 一律传 0，因为 Perfect 不产生「偏早 / 偏晚」的统计意义。
        for (line_id, id) in judgements.into_iter() {
            self.commit(t, Judgement::Perfect, line_id as _, id, 0.);
            let (note_transform, note_hitsound) = {
                let line = &mut chart.lines[line_id];
                let note = &mut line.notes[id as usize];
                // 普通 note 用自身时间取变换（贴合 note 的实际位置）；
                // Hold 用当前时间，因为此时 note 已停在判定线上。
                let nt = if matches!(note.kind, NoteKind::Hold { .. }) { t } else { note.time };
                line.object.set_time(nt);
                note.object.set_time(nt);
                (note.object.now(res), note.hitsound.clone())
            };
            let line = &chart.lines[line_id];
            res.with_model(line.now_transform(res, &chart.lines) * note_transform, |res| {
                res.emit_at_origin(line.notes[id as usize].rotation(line), res.res_pack.info.fx_perfect())
            });
            if !matches!(chart.lines[line_id].notes[id as usize].kind, NoteKind::Hold { .. }) {
                note_hitsound.play(res);
            }
        }
    }

    /// 汇总本局结果为 `PlayResult`，供结算页与成绩记录使用。
    #[inline]
    pub fn result(&self) -> PlayResult {
        self.inner.result()
    }

    /// 当前连击数，供 HUD 显示。
    #[inline]
    pub fn combo(&self) -> u32 {
        self.inner.combo()
    }

    /// 各类判定计数（0=Perfect, 1=Good, 2=Bad, 3=Miss）。
    /// `GameScene::update` 依赖下标 1/2/3 来实现 AP / FC 指示色与即死模式。
    #[inline]
    pub fn counts(&self) -> [u32; 4] {
        self.inner.counts()
    }
}

/// 原始输入事件收集器，实现 miniquad 的 `EventHandler` 以接收底层事件。
///
/// 它的生命周期只有一帧：`Judge::on_new_frame` 新建、`repeat_all_miniquad_input` 用
/// macroquad 记录的整份事件队列喂进来、`finalize` 补一条鼠标按住事件，最后整份转移进
/// 线程局部的 `TOUCHES`。因此这里的字段只累积「本帧」的数据，不需要考虑跨帧清理。
struct Handler {
    /// 本帧收到的原始事件与键盘计数。
    status: TouchStatus,
    /// 本帧累计的滚轮位移 `(x, y)`。
    wheel: (f32, f32),
}
// 事件收集的收尾处理：把「查询式」的鼠标状态补成事件，补齐触摸事件的语义。
impl Handler {
    /// 收尾：若左键仍处于按下状态，补一条 `Moved` 事件。
    ///
    /// 必要性：鼠标按住不动时不会再产生任何事件，但 Hold note 需要「每帧都摸到判定区」才能
    /// 保持不断；这里补一条当前位置的 Moved 事件，正好让每个后续帧都有触摸点存在。
    /// 时间戳为 NEG_INFINITY，交由 `Judge::update` 解释为「按当前帧时刻处理」。
    fn finalize(&mut self) {
        if is_mouse_button_down(MouseButton::Left) {
            self.status.touches.push(Touch {
                id: button_to_id(MouseButton::Left),
                phase: TouchPhase::Moved,
                position: mouse_position().into(),
                time: f64::NEG_INFINITY,
            });
        }
    }
}

/// 把鼠标按键映射成一个「伪触摸 id」。
///
/// 用 `u64::MAX - n` 而不是 0 / 1 / 2 / 3 之类的低位数值，是因为真实触摸 id 由操作系统分配，
/// 低位小块有可能被真实设备占用；取 u64 上界的极端值可以认为绝不会与真实触摸冲突，
/// 从而让鼠标与触屏共存时不会互相顶掉同一条触摸记录。
fn button_to_id(button: MouseButton) -> u64 {
    u64::MAX
        - match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::Unknown => 3,
        }
}

// miniquad 事件回调的实现：把引擎回调「翻译」成本项目的统一输入模型。
// 关键点：触屏与鼠标都产出 `Touch`；鼠标的时间戳用 NEG_INFINITY 表示「没有硬件时间戳」；
// 键盘只关心「按下/抬起的数量」，完全忽略键码。
impl EventHandler for Handler {
    /// 每帧回调，与输入无关，本类型不在此处推进任何状态（判定循环由 `Judge::update` 驱动）。
    fn update(&mut self, _: &mut miniquad::Context) {}
    /// 绘制回调，本类型纯收集输入，不绘制任何内容。
    fn draw(&mut self, _: &mut miniquad::Context) {}
    /// 触屏事件：直接透传 id / 相位 / 坐标 / 时间戳。
    ///
    /// 这是唯一携带硬件时间戳（`time`，与 `get_uptime` 同源）的输入来源，
    /// `Judge::update` 正是靠它把「触摸发生的真实时刻」还原成谱面时间来提升判定精度。
    fn touch_event(&mut self, _: &mut miniquad::Context, phase: miniquad::TouchPhase, id: u64, x: f32, y: f32, time: f64) {
        self.status.touches.push(Touch {
            id,
            phase: phase.into(),
            position: vec2(x, y),
            time,
        });
    }

    /// 滚轮事件：把本帧的滚轮增量累加起来（同一帧多次滚动会被合并）。
    fn mouse_wheel_event(&mut self, _ctx: &mut miniquad::Context, x: f32, y: f32) {
        self.wheel.0 += x;
        self.wheel.1 += y;
    }

    /// 鼠标按下事件：映射为 `Started` 相位与伪触摸 id。
    ///
    /// 时间戳为 NEG_INFINITY（无硬件时间戳），由 `Judge::update` 视作「当前帧时刻」。
    fn mouse_button_down_event(&mut self, _ctx: &mut miniquad::Context, button: MouseButton, x: f32, y: f32) {
        self.status.touches.push(Touch {
            id: button_to_id(button),
            phase: TouchPhase::Started,
            position: vec2(x, y),
            time: f64::NEG_INFINITY,
        });
    }

    /// 鼠标抬起事件：映射为 `Ended` 相位。`Judge::update` 收到后销毁对应的手势跟踪器。
    fn mouse_button_up_event(&mut self, _ctx: &mut miniquad::Context, button: MouseButton, x: f32, y: f32) {
        self.status.touches.push(Touch {
            id: button_to_id(button),
            phase: TouchPhase::Ended,
            position: vec2(x, y),
            time: f64::NEG_INFINITY,
        });
    }

    /// 键盘按下事件。
    ///
    /// **不区分键码**：`_keycode` 被忽略，任何键都等价于「一次点击」。
    /// 这是刻意为之的设计取舍——Phira 的键盘玩法只是触屏的替代方案，不做键位与 note 颜色的映射，
    /// 从而让任何键盘（含外接手柄映射的键盘）都能直接玩；代价是「键盘判定」退化成了
    /// 「任意键命中最早的 Click/Hold」，见 `Judge::update` 阶段 9。
    ///
    /// `repeat` 为操作系统的按键重复标志：只统计首次按下，避免长按被当成连续多次点击。
    fn key_down_event(&mut self, _ctx: &mut miniquad::Context, _keycode: KeyCode, _keymods: miniquad::KeyMods, repeat: bool) {
        if !repeat {
            self.status.key_delta += 1;
            self.status.keys_down += 1;
        }
    }

    /// 键盘抬起事件：只递减净变化量，`keys_down` 不减（它统计的是本帧新按下的次数）。
    fn key_up_event(&mut self, _ctx: &mut miniquad::Context, _keycode: KeyCode, _keymods: miniquad::KeyMods) {
        self.status.key_delta -= 1;
    }
}

// 结算数据：由 JudgeInner::result 产出，供结算页展示、成绩保存与上传。
/// 一局游玩的结算结果。
///
/// 字段分为三类：成绩（`score` / `accuracy` / `max_combo`）、总量（`num_of_notes`）、
/// 分布（`counts` 与四组早晚统计）。所有字段都是快照值，与运行中的 `Judge` 解耦，
/// 因此可以安全地跨越场景传递（例如传给 `EndingScene`）。
#[derive(Default)]
pub struct PlayResult {
    /// 总分，满分 1000000（口径见 `JudgeInner::score`）。
    pub score: u32,
    /// 准确率，0.0 ~ 1.0（口径见 `JudgeInner::accuracy`）。空谱时可能为 NaN。
    pub accuracy: f64,
    /// 本局最大连击。
    pub max_combo: u32,
    /// 谱面中需要判定的 note 总数；`max_combo == num_of_notes` 即表示 Full Combo。
    pub num_of_notes: u32,
    /// 四种判定的数量（0=Perfect, 1=Good, 2=Bad, 3=Miss）。
    pub counts: [u32; 4],
    /// **Good 判定中偏早**的数量（只统计 Good，见 `JudgeInner::result`）。
    pub early: u32,
    /// **Good 判定中偏晚**的数量（= Good 总数 - early，故不包含 Miss）。
    pub late: u32,
    /// 保留字段：目前恒为 0，用于兼容记录格式中的「标准差」字段。
    pub std: f32,
    /// 各类判定中偏早的数量，下标同 `counts`。
    pub early_kind: [u32; 4],
    /// 各类判定中偏晚的数量，下标同 `counts`。
    pub late_kind: [u32; 4],
}

/// 按分数与是否 Full Combo 选择结算图标的索引。
///
/// 索引与 `Resource::icons` 的加载顺序一一对应：
/// `0=F, 1=C, 2=B, 3=A, 4=S, 5=V, 6=FC, 7=phi`。
/// 分数分界沿用 Phigros 的等级阈值（70w / 82w / 88w / 92w / 96w）：
/// 低于 96w 只看分数，96w 以上按是否 Full Combo 区分 V 与 FC，满分单独使用 phi 图标。
///
/// # Arguments
/// * `score` — 本局总分（0 ~ 1000000）。
/// * `full_combo` — 是否全连；调用方应传 `max_combo == num_of_notes`。
///
/// # Returns
/// 图标在 `Resource::icons` 中的下标（0 ~ 7）。
///
/// 注意分支顺序即优先级：满分分支必须排在两个通配分支之前，
/// 否则满分（必然同时满足 Full Combo）会被判成 FC 图标而不是 phi。
pub fn icon_index(score: u32, full_combo: bool) -> usize {
    match (score, full_combo) {
        (x, _) if x < 700000 => 0,
        (x, _) if x < 820000 => 1,
        (x, _) if x < 880000 => 2,
        (x, _) if x < 920000 => 3,
        (x, _) if x < 960000 => 4,
        (1000000, _) => 7,
        (_, false) => 5,
        (_, true) => 6,
    }
}
