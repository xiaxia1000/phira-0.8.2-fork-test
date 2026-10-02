#![allow(unused)]

//! 游戏场景（游玩界面）——Phira 玩法的编排中枢。
//!
//! 本文件把「谱面 + 判定 + 渲染 + 音频 + 输入」组织成一局完整的游玩，职责划分如下：
//!
//! - **状态机**：`State` 的 `Starting → BeforeMusic → Playing → Ending` 四个相位，
//!   时间轴由 `BEFORE_TIME`（开场淡入）、`WAIT_TIME`（曲末停顿）、`AFTER_TIME`（结算淡出）
//!   共同定义，推进逻辑集中在 `Scene::update` 开头；
//! - **加载**：`load_chart` 完成 extra.json → 谱面字节 → 解析 → 纹理的链路，
//!   `infer_chart_format` 在谱面未声明格式时按内容猜测格式；
//! - **判定**：每帧在**谱面视口**下调用 `Judge::update`（触摸坐标换算依赖视口），
//!   随后更新判定线颜色、处理即死模式与暂停；
//! - **渲染**：`render` 是一条分段管线——准备离屏 target → 背景 → 切到离屏 FBO 画谱面 →
//!   切回输出 target 叠加 Bad 提示 / 粒子 / HUD / 暂停面板 → 场景级后处理 → 贴回屏幕；
//! - **HUD**：`ui` 通过 `Chart::with_element` 把分数、连击、曲名、难度、进度条与暂停键
//!   绑定到谱面判定线的变换上，使谱面（或资源包）能够控制 HUD 的位置与动效。
//!
//! 多个特殊模式共享同一套流程，仅在入口与结算处分支，详见 `GameMode`。
//! 场景切换采用「请求 → 消费」两段式：`update` / `overlay_ui` 只写 `self.next_scene`
//! 或 `should_exit`，由 `Scene::next_scene` 在帧末统一返回给上层。

prpr_l10n::tl_file!("game");

use super::{
    draw_background,
    ending::RecordUpdateState,
    loading::{BasicPlayer, SaveFn, UpdateFn, UploadFn},
    request_input, return_input, show_message, take_input, EndingScene, NextScene, Scene,
};
use crate::{
    bin::BinaryReader,
    config::{Config, Mods},
    core::{copy_fbo, BadNote, Chart, ChartExtra, Effect, Point, Resource, UIElement, Vector, PGR_FONT},
    ext::{parse_time, screen_aspect, semi_white, RectExt, SafeTexture, ScaleType},
    fs::FileSystem,
    info::{ChartFormat, ChartInfo},
    judge::Judge,
    parse::{parse_extra, parse_pec, parse_phigros, parse_rpe},
    task::Task,
    time::TimeManager,
    ui::{OffsetAnalysisPanel, OffsetPanelAction, OffsetPanelLabels, RectButton, TextPainter, Ui},
};
use anyhow::{bail, Context, Result};
use concat_string::concat_string;
use inputbox::InputBox;
use macroquad::{prelude::*, window::InternalGlContext};
use sasa::{Music, MusicParams};
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    cell::RefCell,
    fs::File,
    io::{Cursor, ErrorKind},
    ops::{Deref, DerefMut, Range},
    path::PathBuf,
    process::{Command, Stdio},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use std::sync::atomic::Ordering;
use tracing::{debug, warn};

/// 暂停按钮的双击保护间隔（秒）。
///
/// 值为 0.7s：开启 `double_click_to_pause` 时，首次点击只是「准备好暂停」并在按钮处画出半透明
/// 提示圆点，只有在上一次点击后 0.7s 内再次点击才真正暂停。0.7s 既大于普通连点的间隔，
/// 又短于玩家「误触后想再来一次」的犹豫时间，能有效避免激烈操作时误暂停。
const PAUSE_CLICK_INTERVAL: f32 = 0.7;

// 闭源构建专有：`inner` 模块（不随源码分发）中的实现会接管本场景的部分逻辑，
// 这里同样用 `use inner::*;` 把其中的同名项引入本模块，使调用点无需区分构建类型。
#[rustfmt::skip]
#[cfg(closed)]
mod inner;
#[cfg(closed)]
use inner::*;
use crate::config::{reset_ws, ws, REDUCE_WORLD_SIZE_SECS, REDUCE_WORLD_SIZE_TARGET, WORLD_SCALE};
use crate::core::Matrix;

/// 曲目播放结束后、进入结算前的等待时间（秒）。
///
/// 期间谱面仍在推进（`update` 仍以 `res.track_length` 作为时间），给最后一屏的 note 与判定
/// 动画留出收尾时间；0.5s 是「足够看到最后一个判定，又不会让玩家等待」的经验值。
const WAIT_TIME: f64 = 0.5;
/// 结算淡出的时长（秒）。
///
/// 在 `WAIT_TIME` 之后开始，把 `res.alpha` 由 1 平滑降到 0（二次曲线），
/// 随后才真正切换到结算场景，避免画面突变。
const AFTER_TIME: f64 = 0.7;

/// 单个谱面的最佳成绩记录（可序列化，用于本地存档与上传）。
///
/// 之所以把三个指标分开存而不是只存分数，是因为「提高分数」与「提高准确率 / 拿到 Full Combo」
/// 可能分别发生在不同的一局里：`update` 会逐项取更优值，最终得到的是各项最佳的组合成绩。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleRecord {
    /// 最高总分。用 `i32` 而非 `u32` 是为了兼容历史存档格式。
    pub score: i32,
    /// 最高准确率（0.0 ~ 1.0）。
    pub accuracy: f32,
    /// 是否达成过 Full Combo（一旦达成就永久为 true，见 `update`）。
    pub full_combo: bool,
}

// 最佳成绩的合并逻辑：逐项取更优，而不是「整局更优」。
impl SimpleRecord {
    /// 用另一份记录逐项更新自身，返回是否发生了任何变化。
    ///
    /// 返回值用于判断「是否需要保存 / 上传」：没有变化就不必写盘或发请求。
    /// 三项独立比较，因此可能出现「分数来自这一局、准确率来自上一局」的组合结果。
    /// `full_combo` 使用 `other & !self`：只有「对方是 FC 而自己不是」时才需要更新，
    /// 已达成过 FC 的记录不会被降级。
    ///
    /// # Arguments
    /// * `other` — 待合并的新记录（通常是本局成绩）。
    ///
    /// # Returns
    /// 任意一项被更新则返回 `true`。
    pub fn update(&mut self, other: &SimpleRecord) -> bool {
        let mut changed = false;
        if other.score > self.score {
            self.score = other.score;
            changed = true;
        }
        if other.accuracy > self.accuracy {
            self.accuracy = other.accuracy;
            changed = true;
        }
        if other.full_combo & !self.full_combo {
            self.full_combo = other.full_combo;
            changed = true;
        }
        changed
    }
}

/// 把「秒」格式化成练习模式使用的时间文本 `[-]HH:MM:SS.ss`。
///
/// 细节约定：
/// * 负数（练习模式允许把时间拖到 0 之前，用于对齐前置留白）在最前面加 `-`，绝对值参与格式化；
/// * 秒保留两位小数且宽度固定为 5（`SS.ss`），保证时间文本宽度稳定、拖动时不会抖动；
/// * 小时对 100 取模，因为练习模式的曲目时长不可能超过 100 小时。
fn fmt_time(t: f32) -> String {
    let f = t < 0.;
    let t = t.abs();
    let secs = t % 60.;
    let mut t = (t / 60.) as u64;
    let mins = t % 60;
    t /= 60;
    let hrs = t % 100;
    format!("{}{hrs:02}:{mins:02}:{secs:05.2}", if f { "-" } else { "" })
}

// 仅在 Web 构建下存在的宿主回调：由网页侧的 JS 提供（`#[wasm_bindgen]` 生成绑定），
// 进入游戏场景时通知宿主「已开始游玩」（网页据此隐藏加载界面 / 记录埋点）。
/// Web（wasm32）平台由宿主 JS 提供的回调声明。
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    /// 通知宿主页面「已进入游戏场景」。非 wasm 平台不存在该符号，调用点也被 cfg 保护。
    fn on_game_start();
}

/// 游玩模式：决定入口处的配置改动、判定是否启用、以及结算的去向。
///
/// 各模式的差异集中在三处：`new`（是否强制 autoplay）、`update` / `render`（是否判定、
/// 是否画练习面板）、`next_scene`（返回什么结果）。`View` 之外的模式都会正常结算成绩。
#[derive(PartialEq, Eq)]
pub enum GameMode {
    /// 正常游玩：结束（或中途退出）后进入结算界面，并把本局最佳成绩回传给上层。
    Normal,
    /// 延迟调整：强制打开 autoplay（不会因打不中而分心），通关后把调整好的延迟作为
    /// `PopWithResult(Some(offset))` 返回，由调用方写回配置。
    TweakOffset,
    /// 段落练习：进入时会**移除** autoplay（必须手动打），可拖动设置练习区间，
    /// 播放超出区间会自动回到起点并暂停；不产生成绩。
    Exercise,
    /// 禁止重试：暂停面板中禁用「重试」按钮，用于比赛 / 挑战场景。
    NoRetry,
    /// 纯观赏：完全不执行判定（`update` 中跳过 `Judge::update`），只播放谱面演出，不记录成绩。
    View,
}

/// 游玩场景的相位状态机（`Starting → BeforeMusic → Playing → Ending`）。
///
/// 相位只描述「这一帧该做什么」，具体的时间判断集中在 `Scene::update` 开头；
/// 用 `Clone` 是因为练习模式越界重开时需要保存 / 恢复当前相位。
#[derive(Clone)]
enum State {
    /// 开场：画面淡入（`res.alpha` 由 0 到 1，三次曲线），尚未开始播放音乐。
    Starting,
    /// 音乐即将开始：已定位到起始时间，等待时间轴越过 0 后正式播放（`Playing`）。
    BeforeMusic,
    /// 正常游玩中：音乐在播、判定生效、HUD 正常显示。
    Playing,
    /// 收尾：曲目已播完，等待 `WAIT_TIME + AFTER_TIME` 的淡出后进入结算。
    Ending,
}

/// 游玩场景的全部状态。
///
/// 字段大致分为五类：**去留**（`should_exit` / `next_scene` / `dead`）、
/// **资源与谱面**（`res` / `chart` / `chart_bytes` / `chart_format` / `music` / `effects`）、
/// **判定**（`judge` / `bad_notes` / `mode` / 偏移量）、
/// **练习与调延迟**（`exercise_*` / `offset_analysis` / `info_offset`）、
/// **外部回调与统计**（`*_fn` / `best_record` / `fps_*` / `touch_points`）。
pub struct GameScene {
    /// 玩家请求退出本场景（Q 键或暂停面板的退出按钮）。由 `Scene::next_scene` 消费并清零逻辑。
    should_exit: bool,
    /// 本帧请求切换到的目标场景。结算界面用 `Overlay`（不销毁本场景），
    /// 调延迟 / 练习退出用 `PopWithResult`（把结果回传给上一层）。
    next_scene: Option<NextScene>,

    /// 游玩模式，决定入口配置改动、是否判定与结算去向。
    pub mode: GameMode,
    /// 运行期资源：配置、纹理、音频管理器、相机、曲目长度等。
    pub res: Resource,
    /// 谱面数据：判定线、note、特效、设置。
    pub chart: Chart,
    /// 判定器（触摸采集 + 判定状态机）。
    pub judge: Judge,
    /// macroquad 的内部 GL 上下文，用于直接切换渲染通道 / 视口——这些底层能力
    /// macroquad 的公开 API 不提供，但本场景的离屏渲染管线必须用到。
    pub gl: InternalGlContext<'static>,
    /// 当前玩家信息（头像、rks、历史最佳）；游客 / 离线状态下为 `None`。
    player: Option<BasicPlayer>,
    /// 谱面原始字节。保留它是为了在需要时重新解析（记录上传、结果校验等），
    /// 避免重新走一遍文件系统。
    chart_bytes: Vec<u8>,
    /// 谱面格式（由 `ChartInfo::format` 指定或 `infer_chart_format` 推断）。
    chart_format: ChartFormat,
    /// 谱面附加的延迟偏移（来自 `ChartInfo`）。总偏移 = `chart.offset + config.offset + info_offset`，
    /// 调整延迟模式修改的正是这一项。
    info_offset: f32,
    /// 场景级后处理 effect：从 `chart.extra.global_effects` 取出，由本场景（而非谱面）
    /// 在渲染管线的最后统一执行，作用范围是整个屏幕（例如 FXAA、彩虹滤镜）。
    effects: Vec<Effect>,
    /// 调整延迟模式的偏移分析面板（采样、计算建议延迟、保存）。
    offset_analysis: OffsetAnalysisPanel,

    /// 是否首次进入本场景。练习模式首帧会自动暂停，让玩家先设置区间再开始。
    first_in: bool,
    /// 练习区间 `[start, end)`，单位：谱面时间（秒）。播放越过 `end` 会自动回到 `start`。
    exercise_range: Range<f64>,
    /// 正在被拖动的练习控制点 `(类型, 触摸 id)`：`-1` 起点、`0` 播放进度、`1` 终点。
    /// `None` 表示当前没有拖动。
    exercise_press: Option<(i8, u64)>,
    /// 练习面板中「起点」「终点」两个可点击的数值按钮（点击后弹出输入框精确设置时间）。
    exercise_btns: (RectButton, RectButton),

    /// 音乐播放器。播放速率与 `config.speed` 绑定；改流速后需要重建（见 `overlay_ui`）。
    pub music: Music,

    /// 当前相位。
    state: State,
    /// 上一次 `render` 的真实时间（秒），用于计算粒子 / 动画推进的帧间隔 `dt`。
    pub last_update_time: f64,
    /// 「继续游戏」的 3 秒倒计时回退目标时间。`Some` 表示正处于倒计时阶段——
    /// 此时既不做判定也不推进音乐，只在 `overlay_ui` 里显示倒计时。
    pause_rewind: Option<f64>,
    /// 上一次点击暂停按钮的时间，用于双击保护（见 `PAUSE_CLICK_INTERVAL`）。
    /// `NEG_INFINITY` 表示当前不处于「等待第二次点击」的状态。
    pause_first_time: f32,

    /// 被判 Bad 的 note 快照，由判定模块填充、本场景逐帧绘制成 Bad 提示。
    pub bad_notes: Vec<BadNote>,

    /// 上传成绩的回调（闭源构建下才会被赋值为非空）。
    upload_fn: Option<UploadFn>,
    /// 每帧回调，把当前时间与判定结果同步给外部（联机 / 回放 / 练习统计）。
    update_fn: Option<UpdateFn>,
    /// 保存成绩的回调。
    save_fn: Option<SaveFn>,

    /// 本谱面的最佳成绩（进入时加载 + 本局结束后合并），用于结算界面对比与退出时回传。
    best_record: Option<SimpleRecord>,

    /// 需要额外绘制的触摸点（蓝色圆点），用于回放 / 调试 / 外部注入的可视化。
    pub touch_points: Vec<(f32, f32)>,
    /// 平均帧率统计的累计帧数（仅在 `Playing` 且未暂停时累加）。
    fps_frame_count: u32,
    /// 平均帧率统计的累计时间（秒）。
    fps_total_time: f64,
    /// 上一帧的真实时间，用于求帧间隔。
    fps_last_frame_time: f64,

    /// 即死模式（`INSTANT_DEATH_AP` / `INSTANT_DEATH_FC`）是否已触发。
    /// 触发后禁用暂停面板的「继续」按钮——本局已经失败，只能重试或退出。
    dead: bool,
}

/// 把场景重置到「刚进入」的状态，供重试、段落跳转与首次开始复用。
///
/// 必须同时重置三处状态，否则会出现「画面重开了但判定还在继续」这类不一致：
/// 1. 表现层：清空 `bad_notes`，并把判定线颜色恢复为资源包指定的 Perfect 色；
/// 2. 判定层：`judge.reset()` 清游标 / 统计 / 手势跟踪器，`chart.reset()` 清每个 note 的判定状态；
/// 3. 时间层：音乐归零并暂停、时间轴按当前 `speed` 重建并归零，同时重置 FPS 统计与 `dead` 标记。
///
/// 用法：`reset!(self, res, tm)`。宏体内用 `?` 传播音乐操作的错误，
/// 因此只能用于返回 `Result` 的函数；`tm` 为调用方作用域中的时间管理器变量名。
macro_rules! reset {
    ($self:ident, $res:expr, $tm:ident) => {{
        $self.bad_notes.clear();
        $self.judge.reset();
        $self.chart.reset();
        $res.judge_line_color = $res.res_pack.info.color_perfect();
        $self.music.pause()?;
        $self.music.seek_to(0.)?;
        $tm.speed = $res.config.speed as _;
        $tm.reset();
        $self.last_update_time = $tm.now();
        $self.state = State::Starting;
        $self.fps_frame_count = 0;
        $self.fps_total_time = 0.0;
        $self.fps_last_frame_time = $tm.real_time();
        $self.dead = false;
    }};
}

// 游玩场景的主体实现：谱面加载、相位时间常量、HUD / 暂停 / 练习面板绘制，
// 以及供 `Scene` 实现调用的若干辅助方法。
impl GameScene {
    /// 开场淡入时长（秒）。
    ///
    /// 期间不推进判定，`res.alpha` 按三次曲线由 0 升到 1（见 `update` 的 `State::Starting`）。
    /// 它同时是相机、粒子等视觉状态的初始化窗口，因此不设为 0。
    pub const BEFORE_TIME: f64 = 0.7;
    /// 从曲末开始计时、到可以离开本场景的时刻（秒）。
    ///
    /// 等于 `WAIT_TIME + AFTER_TIME + 0.3`：比淡出真正结束再晚 0.3s，
    /// 确保切到结算场景时画面已完全黑掉，不会闪回游玩画面。
    pub const FADEOUT_TIME: f64 = WAIT_TIME + AFTER_TIME + 0.3;

    /// 读取谱面原始字节，附带 `.pec` → `.json` 的回退。
    ///
    /// 回退的原因：历史上有谱面把 JSON 内容存成 `.pec` 后缀，导致按声明的路径读到的文件
    /// 其实无法解析；因此当路径以 `.pec` 结尾时，再尝试同名的 `.json`。
    ///
    /// # Errors
    /// 两条路径都读取失败时返回 `Cannot find chart file`。
    pub async fn load_chart_bytes(fs: &mut dyn FileSystem, info: &ChartInfo) -> Result<Vec<u8>> {
        if let Ok(bytes) = fs.load_file(&info.chart).await {
            return Ok(bytes);
        }
        if let Some(name) = info.chart.strip_suffix(".pec") {
            if let Ok(bytes) = fs.load_file(&concat_string!(name, ".json")).await {
                return Ok(bytes);
            }
        }
        bail!("Cannot find chart file")
    }

    /// 推断谱面格式：优先采用 `info.format` 中显式声明的格式，否则按**内容**猜测。
    ///
    /// 猜测规则（按判定顺序）：
    /// 1. 能按 UTF-8 解码为文本吗？不能 → `Pbc`（二进制格式）；
    /// 2. 文本以 `{` 开头吗？是 → JSON 系；不是 → `Pec`（自定义文本格式）；
    /// 3. JSON 中含 `"META"` 字段吗？含 → `Rpe`；不含 → `Pgr`（Phigros 官方导出格式无 META）。
    ///
    /// 需要注意的约定：**这里完全不检查文件后缀**。`info.chart` 上的 `.pbc` / `.pec` 等扩展名
    /// 只是命名习惯，格式判定一律以内容为准；因此同一份 JSON 无论改成什么后缀都能被识别。
    /// 代价是这类启发式有边界情况——例如带 BOM 或前导空白的 JSON 不以 `{` 开头，
    /// 会被判成 `Pec`；判断依据是字面量前缀匹配，不做 trim。
    ///
    /// # Arguments
    /// * `info` — 谱面元信息；其中 `format` 已指定时直接返回它。
    /// * `bytes` — 谱面原始字节。
    ///
    /// # Returns
    /// 推断出的谱面格式，供 `load_chart` 选择解析器。
    pub fn infer_chart_format(info: &ChartInfo, bytes: &[u8]) -> ChartFormat {
        info.format.clone().unwrap_or_else(|| {
            if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                if text.starts_with('{') {
                    if text.contains("\"META\"") {
                        ChartFormat::Rpe
                    } else {
                        ChartFormat::Pgr
                    }
                } else {
                    ChartFormat::Pec
                }
            } else {
                ChartFormat::Pbc
            }
        })
    }

    /// 加载并解析谱面，返回 `(谱面, 原始字节, 格式)`。
    ///
    /// 完整链路：
    /// 1. 读取同目录下的 `extra.json`（谱面自定义的音效 / 特效 / 视频等额外资源声明）。
    ///    **文件不存在是合法的**（用默认值继续），但文件存在却无法按 UTF-8 解析会直接报错——
    ///    这类错误通常是资源打包出错，静默忽略只会让问题更难定位；
    /// 2. 读取谱面字节并按 `infer_chart_format` 的结论选择解析器（RPE / Phigros / PEC / 二进制）；
    /// 3. 加载谱面引用的纹理；
    /// 4. 用 `ChartInfo` 中的设置覆盖谱面自带的设置（目前只有 `hold_partial_cover`，
    ///    即 Hold 是否用进度覆盖式渲染，属于难度 / 谱面作者意图的一部分）。
    ///
    /// # Errors
    /// extra.json 解析失败、谱面文件缺失、谱面解析失败或纹理加载失败都会向上传播错误。
    pub async fn load_chart(fs: &mut dyn FileSystem, info: &ChartInfo) -> Result<(Chart, Vec<u8>, ChartFormat)> {
        // extra.json 可选：读不到就当作没有额外资源。
        let extra = fs.load_file("extra.json").await.ok().map(String::from_utf8).transpose()?;
        let extra = if let Some(extra) = extra {
            parse_extra(&extra, fs).await.context("Failed to parse extra")?
        } else {
            ChartExtra::default()
        };
        let bytes = Self::load_chart_bytes(fs, info).await.context("Failed to load chart")?;
        let format = Self::infer_chart_format(info, &bytes);
        // 四种格式各有独立的解析器；Pbc 走二进制读取器（无文本解析开销）。
        let mut chart = match format {
            ChartFormat::Rpe => parse_rpe(&String::from_utf8_lossy(&bytes), fs, extra, info.use_rpe_170_speed.unwrap_or_default()).await,
            ChartFormat::Pgr => parse_phigros(&String::from_utf8_lossy(&bytes), extra),
            ChartFormat::Pec => parse_pec(&String::from_utf8_lossy(&bytes), extra),
            ChartFormat::Pbc => {
                let mut r = BinaryReader::new(Cursor::new(&bytes));
                r.read()
            }
        }?;
        // 加载判定线 / note 引用的纹理（RPE 等格式允许谱面自带图片）。
        chart.load_textures(fs).await?;
        // 用 ChartInfo 的设置覆盖谱面自带设置：hold_partial_cover 决定 Hold 的外观表现，
        // 由谱面元信息控制，避免同一份谱面在不同难度下表现不一致。
        chart.settings.hold_partial_cover = info.hold_partial_cover;
        Ok((chart, bytes, format))
    }

    /// 创建游玩场景：按模式改写配置副本、加载谱面与资源、初始化判定器与音乐。
    ///
    /// # 与模式相关的入口改动
    /// * `TweakOffset`：**强制打开 autoplay**。调整延迟时玩家需要专注观察判定线与音乐的时间
    ///   关系，自动演示可保证判定线颜色不受操作影响；
    /// * `Exercise`：**移除 autoplay**。练习必须手动打，否则无法评估自己在该段落的水平。
    ///
    /// # 加载顺序的隐含约定
    /// 谱面必须先于 `Resource::new` 加载：后者需要知道「谱面 / 配置中是否存在任何 effect」
    /// 才能确定 `no_effect` 标志（没有任何后处理时可直接渲染到屏幕，省掉离屏 FBO 与一次拷贝）。
    /// 同时 `chart.extra.global_effects` 会被 `mem::take` 提前取出交给场景自己驱动，
    /// 因此判断 `no_effect` 用的是「已取走的场景 effects」与「谱面剩余 effects」两个条件。
    ///
    /// # Arguments
    /// * `mode` — 游玩模式。
    /// * `info` — 谱面元信息（id / 名称 / 难度 / 偏移 / 宽高比等）。
    /// * `config` — 全局配置的副本，本函数会在副本上按模式与 mod 做修改。
    /// * `fs` — 文件系统抽象（本地目录 / 压缩包 / 网络）。
    /// * `player` — 玩家信息（头像、历史最佳）；离线 / 游客时为 `None`。
    /// * `background` / `illustration` — 背景与曲绘纹理。
    /// * `upload_fn` / `update_fn` / `save_fn` — 上层注入的成绩上传 / 每帧同步 / 保存回调。
    ///
    /// # Errors
    /// 谱面加载、资源加载或音乐创建失败时返回错误（附带上文信息）。
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        mode: GameMode,
        info: ChartInfo,
        mut config: Config,
        mut fs: Box<dyn FileSystem>,
        player: Option<BasicPlayer>,
        background: SafeTexture,
        illustration: SafeTexture,
        upload_fn: Option<UploadFn>,
        update_fn: Option<UpdateFn>,
        save_fn: Option<SaveFn>,
    ) -> Result<Self> {
        // ---- 按模式固定 autoplay ----
        // 调延迟必须自动演示（否则判定线颜色会随玩家操作闪烁，干扰观察）；
        // 练习必须手动（见文档注释）。改的是 config 副本，不会污染全局配置。
        match mode {
            GameMode::TweakOffset => {
                config.mods.insert(Mods::AUTOPLAY);
            }
            GameMode::Exercise => {
                config.mods.remove(Mods::AUTOPLAY);
            }
            _ => {}
        }

        // ---- 加载谱面 ----
        // 必须最先完成：Resource::new 需要据此决定 no_effect。
        let (mut chart, chart_bytes, chart_format) = Self::load_chart(fs.deref_mut(), &info).await?;
        // NO_SHADER：连谱面自带的 effect 一并清空（低端设备 / 兼容性选项）。
        if config.mods.contains(Mods::NO_SHADER) {
            chart.extra.effects.clear();
            chart.extra.global_effects.clear();
        }
        // ---- 取出场景级 effect ----
        // 谱面级 effect 在离屏 FBO 内生效（随谱面内容一起被后处理），
        // 场景级 effect 作用于整屏，执行时机不同，因此这里取出来交给 GameScene 自己驱动。
        let effects = std::mem::take(&mut chart.extra.global_effects);
        // FXAA 作为全屏后处理追加；一旦存在 effect，渲染就必须走离屏路径。
        if config.fxaa {
            chart
                .extra
                .effects
                .push(Effect::new(0.0..f64::INFINITY, include_str!("fxaa.glsl"), Vec::new(), false).unwrap());
        }

        // NIGHTCORE：流速乘 1.5（同时作用于音乐播放速率与谱面时间），属于不计成绩的 mod。
        if config.has_mod(Mods::NIGHTCORE) {
            config.speed *= 1.5;
        }

        // RAINBOW：追加彩虹滤镜（同为场景级 effect）。
        if config.has_mod(Mods::RAINBOW) {
            chart
                .extra
                .effects
                .push(Effect::new(0.0..f64::INFINITY, include_str!("rainbow.glsl"), Vec::new(), false).unwrap());
        }

        // 记录谱面自带的延迟偏移：调延迟模式修改的正是它，退出时回传给上层写入配置。
        let info_offset = info.offset;
        // 最后一个参数是 has_no_effect：谱面 effect 与场景 effect 都为空时才为 true，
        // 此时 Resource::no_effect 置位，渲染管线可以跳过离屏 FBO。
        let mut res = Resource::new(
            config,
            info,
            fs,
            player.as_ref().and_then(|it| it.avatar.clone()),
            background,
            illustration,
            chart.extra.effects.is_empty() && effects.is_empty(),
        )
        .await
        .context("Failed to load resources")?;

        // Prepare extra sfx from chart.hitsounds
        // 谱面自定义音效在这里统一转成运行时音频 clip 并登记到 extra_sfxs，
        // 供 HitSound::Custom 按名字取用；加载失败则跳过（该音效退化为无声，不影响判定）。
        chart.hitsounds.drain().for_each(|(name, clip)| {
            if let Ok(clip) = res.create_sfx(clip) {
                res.extra_sfxs.insert(name, clip);
            }
        });

        // 练习区间默认值：从「谱面真正开始」的时间（谱面偏移 + 信息偏移 + 配置偏移）到曲末。
        // 起点不取 0 是为了跳过前置留白，让练习一开始就有 note 可打。
        let exercise_range = (chart.offset + info_offset + res.config.offset) as f64..res.track_length;

        // 判定器要在谱面解析并覆盖设置之后创建：它按 note 时间建立索引，并统计
        // 需要判定的 note 总数（作为准确率分母）。
        let judge = Judge::new(&chart);

        // 音乐播放器按当前流速创建；改流速后需重建（见 overlay_ui）。
        let music = Self::new_music(&mut res)?;
        Ok(Self {
            should_exit: false,
            next_scene: None,

            mode,
            res,
            chart,
            judge,
            // SAFETY: `get_internal_gl` 返回进程级单例 GL 上下文的引用，并要求调用方保证
            // 「只在拥有该上下文的线程上使用」。本场景是主线程独占的渲染场景，
            // 且 macroquad 保证该上下文在应用整个生命周期内有效，故按 'static 持有是安全的。
            gl: unsafe { get_internal_gl() },
            player,
            chart_bytes,
            chart_format,
            effects,
            info_offset,

            offset_analysis: OffsetAnalysisPanel::new(),

            first_in: false,
            exercise_range,
            exercise_press: None,
            exercise_btns: (RectButton::new(), RectButton::new()),

            music,

            state: State::Starting,
            last_update_time: 0.,
            pause_rewind: None,
            pause_first_time: f32::NEG_INFINITY,

            bad_notes: Vec::new(),

            upload_fn,
            update_fn,
            save_fn,

            best_record: None,

            touch_points: Vec::new(),

            fps_frame_count: 0,
            fps_total_time: 0.0,
            fps_last_frame_time: 0.0,

            dead: false,
        })
    }

    /// 按配置的音量与流速创建音乐播放器。
    ///
    /// `playback_rate` 直接取 `config.speed`：音乐变速与谱面流速必须一致，
    /// 否则音频与判定会逐渐错位（谱面时间本身就是以「流速倍率」为基准推进的）。
    /// 每次改流速都需要重建播放器（音频后端不支持运行时改速率），见 `overlay_ui`。
    ///
    /// # Errors
    /// 音频后端创建播放器失败时返回错误。
    fn new_music(res: &mut Resource) -> Result<Music> {
        res.audio.create_music(
            res.music.clone(),
            MusicParams {
                amplifier: res.config.volume_music as _,
                playback_rate: res.config.speed as _,
                ..Default::default()
            },
        )
    }

    /// UI 坐标 → 谱面 / 判定坐标的换算系数。
    ///
    /// UI 以整块视口为基准归一化（x ∈ [-1, 1]），而谱面坐标的纵向范围由宽高比决定
    /// （y ∈ [-1/aspect_ratio, 1/aspect_ratio]）。当屏幕宽高比与谱面宽高比不一致、
    /// 谱面视口被居中裁剪时，练习面板里直接使用 UI 触摸坐标会整体偏移，
    /// 因此需要乘这个系数换算回谱面坐标系；两者宽高比一致时系数为 1。
    fn touch_scale(&self) -> f32 {
        (screen_width() / screen_height()) / self.res.aspect_ratio
    }

    /// 绘制游玩 HUD：分数 / 实时准确率、暂停键、连击、曲名、难度与进度条。
    ///
    /// # 两个关键设计
    ///
    /// 1. **HUD 绑定到判定线**：所有元素都通过 `Chart::with_element(ui, res, UIElement::Xxx, ..)`
    ///    绘制。若谱面把某个 `UIElement` 绑定到了某条判定线（`Chart::attach_ui`），
    ///    该元素就会跟随这条线的变换（平移 / 旋转 / 缩放 / 变色 / 透明度），从而支持官方谱面
    ///    那类「HUD 随判定线移动」的演出；未绑定时退化为屏幕固定坐标（回调直接以 WHITE 绘制）。
    /// 2. **新旧两套布局**：`legacy_aui`（即 `!info.use_attach_ui_fix`）是兼容旧谱面的布局。
    ///    旧布局用「文本测量 + 手工偏移」计算位置（见 `unit_h` 与各处 `scale_point`），
    ///    以复刻旧版本 Phira 的像素级观感；新布局直接用设计好的锚点与尺寸。
    ///    不统一的原因是一旦改布局，大量老谱面的 HUD 视觉位置都会发生变化。
    ///
    /// `p` 是整体淡入淡出系数：开始时由 0 升到 1，结束时由 1 降到 0；
    /// 它在纵向叠加上 `(1 - p) * 0.4` 的位移，让 HUD 随画面一起「淡出并移位」。
    fn ui(&mut self, ui: &mut Ui, tm: &mut TimeManager) -> Result<()> {
        // ---- 计算淡入淡出系数 p ----
        // Starting：三次曲线淡入（先快后慢）；Ending：二次曲线淡出。
        // BeforeMusic / Playing 恒为 1。
        let time = tm.now();
        let p = match self.state {
            State::Starting => {
                if time <= Self::BEFORE_TIME {
                    1. - (1. - time / Self::BEFORE_TIME).powi(3)
                } else {
                    1.
                }
            }
            State::BeforeMusic => 1.,
            State::Playing => 1.,
            State::Ending => {
                let t = time - self.res.track_length - WAIT_TIME;
                1. - (t / (AFTER_TIME + 0.3)).min(1.).powi(2)
            }
        } as f32;
        // ---- HUD 的布局基准量 ----
        // eps：视觉留白，按宽高比缩放（0.02 是 16:9 下的基准值），保证不同宽高比下观感一致；
        // top：屏幕顶边对应的 y 值（-1/aspect_ratio），后续纵向位置都以它为基准，`-top` 即底边；
        // pause_*：暂停键（两条竖条）的尺寸与中心点，中心点会随淡出系数 p 下移。
        let res = &mut self.res;
        let eps = 2e-2 / res.aspect_ratio;
        let top = -1. / res.aspect_ratio;
        let pause_w = 0.015;
        let pause_h = pause_w * 3.2;
        let pause_center = Point::new(pause_w * 4.0 - 1., top + eps * 3.5 - (1. - p) * 0.4 + pause_h / 2.);
        // ---- 暂停键命中测试 ----
        // 只在「可交互 + 未暂停 + 不在 3 秒倒计时中」时响应，否则点暂停键会与倒计时逻辑打架。
        if res.config.interactive
            && !tm.paused()
            && self.pause_rewind.is_none()
            && Judge::get_touches().iter().any(|touch| {
                touch.phase == TouchPhase::Started && {
                    let p = touch.position;
                    let p = Point::new(p.x, p.y);
                    (pause_center - p).norm() < 0.05
                }
            })
        {
            // ---- 双击保护 ----
            // 距上次点击超过 PAUSE_CLICK_INTERVAL（或未开启双击保护）时，本次只算「第一次点击」：
            // 记录时间并（在下面的绘制里）给出半透明提示点；否则才真正暂停。
            let t = tm.now() as f32;
            if t - self.pause_first_time > PAUSE_CLICK_INTERVAL && res.config.double_click_to_pause {
                self.pause_first_time = t;
            } else {
                self.pause_first_time = f32::NEG_INFINITY;
                if !self.music.paused() {
                    self.music.pause()?;
                }
                tm.pause();
                // HarmonyOS 专有：暂停后关闭系统手势拦截，让玩家能正常使用系统手势。
                // 对应地，游玩中与「继续游戏」时会重新开启拦截（见 enter / overlay_ui），
                // 以免激烈操作误触发系统返回手势。
                #[cfg(target_env = "ohos")]
                miniquad::native::set_interceptor_state(false);
            }
        }
        // HUD 整体套用 res.alpha（开场淡入 / 结算淡出都作用于此），
        // 保证 HUD 与谱面同步淡出，而不是突然消失。
        ui.alpha(res.alpha, |ui| {
            // 历史遗留的「魔法修复」：先画一段完全透明、内容无意义的文本，
            // 让文本渲染路径（字体图集 / 绘制批次）先初始化一次，避免紧随其后的 HUD 文本
            // 在首帧丢失或错位。保留它是为了不改变既有观感。
            ui.text("MAGIC BUGFIX TEXT").color(Color::new(0., 0., 0., 0.)).draw();
            // 双击保护的视觉反馈：处于「等待第二次点击」状态时，在暂停键位置画半透明提示点。
            if tm.now() as f32 - self.pause_first_time <= PAUSE_CLICK_INTERVAL {
                ui.fill_circle(pause_center.x, pause_center.y, 0.05, Color::new(1., 1., 1., 0.5));
            }

            // 屏幕边距（0.03 是归一化 x 单位下的固定留白）。
            let margin = 0.03;

            // 布局开关：`use_attach_ui_fix` 为真表示使用新的 UI 绑定布局；
            // 旧布局还需要知道一行文字的高度（unit_h）来做「以文本底边为基准」的手工堆叠。
            let legacy_aui = !res.info.use_attach_ui_fix.unwrap_or_default();
            let unit_h = if legacy_aui { ui.text("0").measure_using(&PGR_FONT).h } else { 0. };

            // ---- 分数与实时准确率（右上角）----
            // score
            let h = 0.07;
            let score_top = top + eps * 2.2 - (1. - p) * 0.4;
            let score_right = 1. - margin;
            let score = format!("{:07}", self.judge.score());
            // 旧布局需要额外算一个「缩放中心点」：测量分数的实际文本中心，
            // 把绘制基准从右上角锚点平移到文本中心。新布局直接传 None（用锚点定位）。
            let scale_point = legacy_aui.then(|| {
                let ct = ui.text(&score).size(0.8).measure_using(&PGR_FONT).center();
                (score_right - ct.x, score_top + ct.y)
            });
            self.chart
                .with_element(ui, res, UIElement::Score, scale_point, (score_right, score_top), |ui, c| {
                    ui.text(&score)
                        .pos(score_right, score_top)
                        .anchor(1., 0.)
                        .size(0.8)
                        .color(c)
                        .draw_using(&PGR_FONT);
                    if res.config.show_acc {
                        ui.text(format!("{:05.2}%", self.judge.real_time_accuracy() * 100.))
                            .pos(1. - margin, score_top + h)
                            .anchor(1., 0.)
                            .size(0.4)
                            .color(Color { a: c.a * 0.7, ..c })
                            .draw_using(&PGR_FONT);
                    }
                });

            // 暂停键作为 UIElement::Pause 绘制（两条竖条）。
            // 与其它 HUD 元素一样，谱面可以把它绑定到判定线上，从而跟随判定线移动 / 旋转。
            self.chart.with_element(
                ui,
                res,
                UIElement::Pause,
                legacy_aui.then(|| (pause_center.x, pause_center.y)),
                (pause_center.x - pause_w * 1.5, pause_center.y - pause_h / 2.),
                |ui, c| {
                    let mut r = Rect::new(pause_center.x - pause_w * 1.5, pause_center.y - pause_h / 2., pause_w, pause_h);
                    ui.fill_rect(r, c);
                    r.x += pause_w * 2.;
                    ui.fill_rect(r, c);
                },
            );
            // ---- 连击数（3 连以上才显示，沿用 Phigros 的表现习惯）----
            // 两种布局的差别在于「COMBO 标签贴到数字下方」的方式：
            // 旧布局用 unit_h 手工堆叠（用数字文本 bottom() 求出底边），
            // 新布局用 measure().center() 直接算出中心，从而得到更稳定的间距。
            if self.judge.combo() >= 3 {
                if legacy_aui {
                    let combo_top = top + eps * 2. - (1. - p) * 0.4;
                    let btm = self
                        .chart
                        .with_element(ui, res, UIElement::ComboNumber, None, (0., combo_top + unit_h / 2.), |ui, c| {
                            ui.text(self.judge.combo().to_string())
                                .pos(0., combo_top)
                                .anchor(0.5, 0.)
                                .color(c)
                                .draw_using(&PGR_FONT)
                                .bottom()
                        });
                    let combo_top = btm + 0.01;
                    self.chart
                        .with_element(ui, res, UIElement::Combo, None, (0., combo_top + unit_h * 0.2), |ui, c| {
                            ui.text(if res.config.autoplay() { "AUTOPLAY" } else { "COMBO" })
                                .pos(0., combo_top)
                                .anchor(0.5, 0.)
                                .size(0.4)
                                .color(c)
                                .draw_using(&PGR_FONT);
                        });
                } else {
                    let combo = self.judge.combo().to_string();
                    let ct = ui.text(&combo).size(1.0).measure().center();
                    let combo_y = top + eps * 2. - (1. - p) * 0.4 + ct.y;
                    let btm = self.chart.with_element(ui, res, UIElement::ComboNumber, None, (0., combo_y), |ui, c| {
                        ui.text(&combo)
                            .pos(0., combo_y)
                            .anchor(0.5, 0.5)
                            .size(1.0)
                            .color(c)
                            .draw_using(&PGR_FONT)
                            .bottom()
                    });
                    let ct = ui.text("COMBO").size(0.4).measure().center();
                    let combo_top = btm + 0.01 + ct.y;
                    self.chart.with_element(ui, res, UIElement::Combo, None, (0., combo_top), |ui, c| {
                        ui.text(if res.config.autoplay() { "AUTOPLAY" } else { "COMBO" })
                            .pos(0., combo_top)
                            .anchor(0.5, 0.5)
                            .size(0.4)
                            .color(c)
                            .draw_using(&PGR_FONT);
                    });
                }
            }
            // ---- 曲名与难度（左下角与右下角）----
            // 布局基准：lf 为左边距处的 x，bt 为底边往上留出的 y（随淡出系数 p 反向位移）。
            // magic to make score visible, refer to phira/src/rate.rs#L219
            // 与上面那句一样属于历史「魔法」：先画一段不指定字体的空文本，再画曲名，
            // 才能让分数文本正确显示（原因参见注释指向的 rate.rs，此处保留原注释便于追溯）。
            ui.text("").draw_using(&PGR_FONT);
            let lf = -1. + margin;
            let bt = -top - eps * 2.8 + (1. - p) * 0.4;
            let scale_point = legacy_aui.then(|| {
                let ct = ui.text(&res.info.name).size(0.5).measure().center();
                (lf + ct.x, bt - ct.y)
            });
            self.chart.with_element(ui, res, UIElement::Name, scale_point, (lf, bt), |ui, c| {
                ui.text(&res.info.name)
                    .pos(lf, bt)
                    .anchor(0., 1.)
                    .size(0.5)
                    .color(c)
                    .max_width(0.8)
                    .draw();
            });

            let scale_point = legacy_aui.then(|| {
                let ct = ui.text(&res.info.level).size(0.5).measure().center();
                (-lf - ct.x, bt - ct.y)
            });
            self.chart.with_element(ui, res, UIElement::Level, scale_point, (-lf, bt), |ui, c| {
                ui.text(&res.info.level).pos(-lf, bt).anchor(1., 1.).size(0.5).color(c).draw();
            });

            // ---- 进度条（顶边，UIElement::Bar）----
            // 进度以「x 轴全长 2」为单位换算：已完成宽度 = 2 * 当前时间 / 曲目长度，
            // 并 clamp 到 [0, 2]——防止时间越过曲末或因负偏移为负时把矩形画到屏幕外。
            // 已播放部分用 60% 白色填充，并在当前位置叠一根两倍宽度的纯白游标。
            let hw = 0.003;
            let height = eps * 1.0;
            let dest = (2. * res.time / res.track_length).clamp(0., 2.) as f32;
            self.chart
                .with_element(ui, res, UIElement::Bar, Some((-1., top + height / 2.)), (-1., top + height / 2.), |ui, color| {
                    ui.fill_rect(Rect::new(-1., top, dest, height), semi_white(0.6));
                    ui.fill_rect(Rect::new(-1. + dest - hw, top, hw * 2., height), WHITE);
                });
        });
        Ok(())
    }

    /// 绘制叠加层 UI：暂停面板、练习面板、继续游戏的倒计时与触摸可视化。
    ///
    /// 暂停面板只在 `tm.paused()` 时出现；练习面板、倒计时与触摸点与暂停状态无关
    /// （练习面板本身也只在暂停时显示，但它的拖动状态跨帧保存在 `exercise_press` 中）。
    ///
    /// # 暂停面板的三个按钮
    /// 从左到右为「退出」（`-1`）、「重试」（`0`）、「继续」（`1`），共用同一套命中测试。
    /// 两个屏蔽条件：`NoRetry` 模式禁用重试；即死模式已触发（`dead`）时禁用继续——
    /// 本局已经失败，只允许重试或退出。
    ///
    /// # Errors
    /// 音乐播放器的暂停 / 定位 / 重建失败时向上传播错误。
    fn overlay_ui(&mut self, ui: &mut Ui, tm: &mut TimeManager) -> Result<()> {
        // 叠加层元素统一乘 res.alpha，与场景淡入淡出保持同步。
        let c = semi_white(self.res.alpha);
        let res = &mut self.res;
        // ---- 暂停面板 ----
        if tm.paused() {
            let h = 1. / res.aspect_ratio;
            // 半透明黑遮罩压暗玩法画面，突出按钮；
            // 练习模式下整个面板上移 0.3（o），给下方的时间轴腾出空间。
            draw_rectangle(-1., -h, 2., h * 2., Color::new(0., 0., 0., 0.6));
            let o = if self.mode == GameMode::Exercise { -0.3 } else { 0. };
            let s = 0.06;
            let w = 0.05;
            let no_retry = self.mode == GameMode::NoRetry;
            draw_texture_ex(
                *res.icon_back,
                -s * 3. - w,
                -s + o,
                c,
                DrawTextureParams {
                    dest_size: Some(vec2(s * 2., s * 2.)),
                    ..Default::default()
                },
            );
            // 重试按钮：以 (0, o) 为中心，用 `feather` 把一个零尺寸矩形向外扩张成边长 2s 的
            // 方形命中区（略大于图标本身，触摸判定更宽容）。
            let r = Rect::new(0., o, 0., 0.).feather(s);
            // 禁用态：40% 透明度，用于 NoRetry 模式的重试按钮与已失败局面的继续按钮。
            let disabled_color = semi_white(res.alpha * 0.4);
            ui.fill_rect(r, (*res.icon_retry, r.feather(0.02), ScaleType::Fit, if no_retry { disabled_color } else { c }));
            draw_texture_ex(
                *res.icon_resume,
                s + w,
                -s + o,
                if self.dead { disabled_color } else { c },
                DrawTextureParams {
                    dest_size: Some(vec2(s * 2., s * 2.)),
                    ..Default::default()
                },
            );
            // ---- 按钮命中测试 ----
            // 只统计「本帧刚按下」的触摸；三个按钮的中心等距分布
            //（间距 = 图标尺寸 2s + 间隙 w），落在任一按钮的方形范围内即视为点击它。
            if res.config.interactive {
                let mut clicked = None;
                for touch in Judge::get_touches() {
                    if touch.phase != TouchPhase::Started {
                        continue;
                    }
                    let p = touch.position;
                    let p = Point::new(p.x, p.y);
                    for i in -1..=1 {
                        let ct = Point::new((s * 2. + w) * i as f32, o);
                        let d = p - ct;
                        if d.x.abs() <= s && d.y.abs() <= s {
                            clicked = Some(i);
                            break;
                        }
                    }
                }
                // 禁用判定：NoRetry 下点重试无效；已失败时点继续无效。
                if no_retry && clicked == Some(0) || self.dead && clicked == Some(1) {
                    clicked = None;
                }
                // 「继续」的起点：练习模式用时间轴当前时间（可能已被拖动），
                // 其它模式用音乐播放器的真实位置。
                let mut pos = self.music.position();
                if self.mode == GameMode::Exercise {
                    pos = tm.now();
                }
                // 流速被改动过（练习面板滑块 / 配置热改）时需要重建播放器：
                // 音频后端不支持运行时改速率，不重建会导致音乐与谱面错位。
                if clicked.is_some_and(|it| it != -1) && (tm.speed - res.config.speed as f64).abs() > 0.01 {
                    debug!("recreating music");
                    self.music = res.audio.create_music(
                        res.music.clone(),
                        MusicParams {
                            amplifier: res.config.volume_music as _,
                            playback_rate: res.config.speed as _,
                            ..Default::default()
                        },
                    )?;
                }
                // ---- 按钮动作 ----
                match clicked {
                    // 退出：只置标志，真正的场景切换由 `Scene::next_scene` 在帧末处理；
                    // ohos 上同时放开系统手势拦截，让玩家能正常离开游戏。
                    Some(-1) => {
                        self.should_exit = true;
                        #[cfg(target_env = "ohos")]
                        miniquad::native::set_interceptor_state(false);
                    }
                    // 重试：整体复位（见 reset! 宏）；练习模式额外把判定游标推进到区间起点，
                    // 让起点之前的 note 直接算已判定，不会在随后被逐个判成 Miss。
                    Some(0) => {
                        reset!(self, res, tm);
                        if self.mode == GameMode::Exercise {
                            self.judge.advance_to(&mut self.chart, self.exercise_range.start);
                        }
                        // 回到游玩状态：重新开启系统手势拦截。
                        #[cfg(target_env = "ohos")]
                        miniquad::native::set_interceptor_state(true);
                    }
                    // 继续游戏：统一回退 3 秒作为缓冲。暂停多半是「被打断」，原地恢复会让玩家
                    // 一睁眼就撞上密集 note；回退 3 秒既能重新进入状态，又不会损失太多进度。
                    Some(1) => {
                        // 练习模式下若已跑出区间（超过终点或退到起点之前），先把播放位置挪回起点。
                        if self.mode == GameMode::Exercise && (tm.now() > self.exercise_range.end || tm.now() < self.exercise_range.start) {
                            tm.seek_to(self.exercise_range.start);
                            self.music.seek_to(self.exercise_range.start)?;
                            pos = self.exercise_range.start;
                        }
                        self.music.play()?;
                        // res.time（驱动判定）与音乐位置（驱动音频）都要回退，二者必须保持一致。
                        res.time -= 3.;
                        // 回退目标为负说明还没进入正式演奏段：此时直接回到 BeforeMusic 相位，
                        // 等时间轴从 0 开始，避免用负时间播放音乐。
                        let dst = pos - 3.;
                        if dst < 0. {
                            self.music.pause()?;
                            self.state = State::BeforeMusic;
                        } else {
                            self.music.seek_to(dst)?;
                        }
                        // 时间轴同步回退 3 秒，并按当前配置刷新流速（可能刚被练习面板的滑块改过）。
                        let now = tm.now();
                        tm.speed = res.config.speed as _;
                        tm.resume();
                        tm.seek_to(now - 3.);
                        // 触发 3 秒倒计时：倒计时期间不执行判定（见 update 中的 pause_rewind 判断）。
                        // 目标时刻再往前 0.2s，使倒计时立即从「3」开始显示并恰好持续 3 秒。
                        self.pause_rewind = Some(tm.now() - 0.2);
                        #[cfg(target_env = "ohos")]
                        miniquad::native::set_interceptor_state(true);
                    }
                    _ => {}
                }
            }
            // ---- 练习面板：流速滑块 + 段落时间轴 ----
            // 位于 `if tm.paused()` 内：拖动区间时必须是暂停状态，否则音乐会在拖动过程中继续跑。
            if self.mode == GameMode::Exercise {
                // 把 UI 坐标系的触摸换算到谱面坐标系（滑块与时间轴都按谱面坐标计算）。
                // 这里就地改写 Ui 缓存的触摸列表，绘制结束后再除回来。
                let asp = self.touch_scale();
                for touch in ui.ensure_touches() {
                    touch.position *= asp;
                }
                // 流速滑块：范围 0.5 ~ 2.0（低于 0.5 会让谱面糊成一团，高于 2.0 超过人类可读极限），
                // 步长 0.05，最后一个参数是双击复位的目标值。
                ui.scope(|ui| {
                    ui.dx(0.3);
                    ui.dy(-0.3);
                    ui.slider(tl!("speed"), 0.5..2.0, 0.05, &mut self.res.config.speed, Some(0.5));
                });
                ui.dy(0.06);
                // 时间轴尺寸：hw 半宽、h 半高、eh 为控制点竖条的臂长、rad 为圆形手柄半径。
                let hw = 0.7;
                let h = 0.06;
                let eh = 0.12;
                let rad = 0.03;
                // 时间轴原点偏移：只取负的总偏移，把「谱面开始之前的留白」也纳入时间轴，
                // 这样拖到最左端对应的是真正的音频 0 点，而不是第一个 note 的位置。
                let sp = self.offset().min(0.) as f64;
                // 灰色底轨即整段时间轴。
                ui.fill_rect(Rect::new(-hw, -h, hw * 2., h * 2.), GRAY);
                // st / en / cur：练习起点、终点、当前播放位置在时间轴上的 x 坐标。
                let st = -hw + ((self.exercise_range.start - sp) / (self.res.track_length - sp)) as f32 * hw * 2.;
                let en = -hw + ((self.exercise_range.end - sp) / (self.res.track_length - sp)) as f32 * hw * 2.;
                let t = tm.now();
                let cur = -hw + ((t - sp) / (self.res.track_length - sp)) as f32 * hw * 2.;
                // 白色区间表示当前选中的练习段落。
                ui.fill_rect(Rect::new(st, -h, en - st, h * 2.), WHITE);
                // 蓝色 = 起点手柄（竖条向上伸出，圆形手柄为拖动区）。
                // 命中测试只在「当前没有正在拖动的控制点」时进行，且必须先 rect_to_global
                // 把矩形从当前局部变换换算到全局归一化坐标，才能与触摸坐标直接比较。
                ui.fill_rect(Rect::new(st, -eh, 0., eh + h).feather(0.005), BLUE);
                ui.fill_circle(st, -eh, rad, BLUE);
                if self.exercise_press.is_none() {
                    let r = ui.rect_to_global(Rect::new(st, -eh, 0., 0.).feather(rad));
                    self.exercise_press = Judge::get_touches()
                        .iter()
                        .find(|it| it.phase == TouchPhase::Started && r.contains(it.position))
                        .map(|it| (-1, it.id));
                }
                // 红色 = 终点手柄（竖条向下伸出）。命中后记录的 ctrl 值为 1。
                ui.fill_rect(Rect::new(en, -h, 0., eh + h).feather(0.005), RED);
                ui.fill_circle(en, eh, rad, RED);
                if self.exercise_press.is_none() {
                    let r = ui.rect_to_global(Rect::new(en, eh, 0., 0.).feather(rad));
                    self.exercise_press = Judge::get_touches()
                        .iter()
                        .find(|it| it.phase == TouchPhase::Started && r.contains(it.position))
                        .map(|it| (1, it.id));
                }
                // 绿色 = 当前播放进度手柄（竖直贯穿时间轴）。命中后 ctrl 为 0，
                // 拖动它会自由跳转播放位置并重置判定状态（相当于一次局部重开）。
                ui.fill_rect(Rect::new(cur, -h, 0., h * 2.).feather(0.005), GREEN);
                ui.fill_circle(cur, 0., rad, GREEN);
                if self.exercise_press.is_none() {
                    let r = ui.rect_to_global(Rect::new(cur, 0., 0., 0.).feather(rad));
                    self.exercise_press = Judge::get_touches()
                        .iter()
                        .find(|it| it.phase == TouchPhase::Started && r.contains(it.position))
                        .map(|it| (0, it.id));
                }
                // 时间轴下方的当前时间文本（居中；负值表示落在谱面开始之前的留白内）。
                ui.text(fmt_time(t as f32)).pos(0., -0.23).anchor(0.5, 0.).size(0.8).draw();
                // ---- 拖动处理 ----
                // 用 rfind 取该触摸 id 的**最后一条**事件：同一帧内可能收到多条移动事件，
                // 取最后一条才是手指当前的真实位置。
                if let Some((ctrl, id)) = &self.exercise_press {
                    if let Some(touch) = Judge::get_touches().iter().rfind(|it| it.id == *id) {
                        let x = touch.position.x;
                        // 把时间轴上的 x 反解回时间：[-hw, hw] 线性映射到 [sp, track_length]。
                        let p = (x + hw) as f64 / (hw * 2.) as f64 * (self.res.track_length - sp) + sp;
                        // 取值范围约束：
                        // - 曲目可用时长不足 3 秒（短曲 / 大量前置留白）时不施加额外限制；
                        // - 拖动播放进度（ctrl == 0）也只限制在曲目范围内；
                        // - 拖动起点 / 终点时要求区间至少保留 3 秒，避免出现无法练习的极短区间。
                        let p = if self.res.track_length - sp <= 3. || *ctrl == 0 {
                            p.clamp(sp, self.res.track_length)
                        } else {
                            p.clamp(
                                if *ctrl == -1 { sp } else { self.exercise_range.start + 3. },
                                if *ctrl == -1 {
                                    self.exercise_range.end - 3.
                                } else {
                                    self.res.track_length
                                },
                            )
                        };
                        if *ctrl == 0 {
                            // 拖动播放进度：时间轴、音乐一起跳转，并清空判定状态与 Bad 提示
                            //（相当于在新区间起点局部重开），判定线颜色也复位为 Perfect 色。
                            tm.seek_to(p);
                            self.music.seek_to(p)?;
                            self.bad_notes.clear();
                            self.judge.reset();
                            self.chart.reset();
                            self.res.judge_line_color = self.res.res_pack.info.color_perfect();
                        } else {
                            // 拖动起点 / 终点：只更新区间端点，不改变当前播放位置。
                            *(if *ctrl == -1 {
                                &mut self.exercise_range.start
                            } else {
                                &mut self.exercise_range.end
                            }) = p;
                        }
                        // 抬手（或被系统取消）才结束拖动；覆盖 Cancelled 是为了避免
                        // 来电 / 手势打断时手柄卡在「一直被拖动」的状态。
                        if matches!(touch.phase, TouchPhase::Cancelled | TouchPhase::Ended) {
                            self.exercise_press = None;
                        }
                    }
                }
                ui.dy(0.2);
                // 区间两端的时间数值按钮：点击后弹出输入框精确输入（见 `touch`），
                // 中间的 "to" 是纯文本分隔符。
                let r = ui.text(tl!("to")).size(0.8).anchor(0.5, 0.).draw();
                let mut tx = ui
                    .text(fmt_time(self.exercise_range.start as f32))
                    .pos(r.x - 0.02, 0.)
                    .anchor(1., 0.)
                    .size(0.8)
                    .color(BLACK);
                // 按钮外观：白底黑字，`touching()`（手指正按在上面）时背景变半透明作为按下反馈。
                // 注册到 exercise_btns 后，由 `Scene::touch` 负责把点击转成输入框请求。
                let re = tx.measure();
                self.exercise_btns.0.set(tx.ui, re);
                tx.ui
                    .fill_rect(re.feather(0.01), Color::new(1., 1., 1., if self.exercise_btns.0.touching() { 0.5 } else { 1. }));
                tx.draw();

                let mut tx = ui
                    .text(fmt_time(self.exercise_range.end as f32))
                    .pos(r.right() + 0.02, 0.)
                    .size(0.8)
                    .color(BLACK);
                let re = tx.measure();
                self.exercise_btns.1.set(tx.ui, re);
                tx.ui
                    .fill_rect(re.feather(0.01), Color::new(1., 1., 1., if self.exercise_btns.1.touching() { 0.5 } else { 1. }));
                tx.draw();
                // 还原触摸坐标：本次面板之外的元素（以及 `Scene::touch` 里的按钮命中测试）
                // 看到的仍应是 UI 坐标系的触摸。
                for touch in ui.ensure_touches() {
                    touch.position /= asp;
                }
            }
        }
        // ---- 「继续游戏」的 3 秒倒计时 ----
        // 由 pause_rewind 单独驱动，与暂停面板无关；期间不执行判定（见 update）。
        // 剩余秒数直接由时间差取整得到，倒计时结束时清掉标记以恢复判定。
        if let Some(time) = self.pause_rewind {
            let dt = tm.now() - time;
            let t = 3 - dt.floor() as i32;
            if t <= 0 {
                self.pause_rewind = None;
            } else {
                // 遮罩不透明度从 1 线性降到 0：画面随着倒计时逐渐显现，给玩家「即将开始」的预告。
                let a = (1. - dt as f32 / 3.) * 1.;
                let h = 1. / self.res.aspect_ratio;
                draw_rectangle(-1., -h, 2., h * 2., Color::new(0., 0., 0., a));
                ui.text(t.to_string()).anchor(0.5, 0.5).size(1.).color(c).draw();
            }
        }
        // ---- 触摸可视化（调试）----
        // 开启 touch_debug 时把当前所有触摸画成半透明红点，便于真机上排查判定与坐标问题。
        if self.res.config.touch_debug {
            for touch in Judge::get_touches() {
                ui.fill_circle(touch.position.x, touch.position.y, 0.04, Color { a: 0.4, ..RED });
            }
        }
        // 外部注入的触摸点（回放 / 脚本）用蓝色区分，便于与真实输入对照。
        for pos in &self.touch_points {
            ui.fill_circle(pos.0, pos.1, 0.04, Color { a: 0.4, ..BLUE });
        }
        Ok(())
    }

    /// 判断当前是否应当响应玩法输入（键盘 / 触摸操作与暂停按钮）。
    ///
    /// 除配置开关外还要求处于 `Playing` 相位：开场淡入（`Starting`）与曲末淡出（`Ending`）
    /// 都不接受输入，避免玩家在还没看到判定线时误打、或在结算阶段触发暂停。
    fn interactive(res: &Resource, state: &State) -> bool {
        res.config.interactive && matches!(state, State::Playing)
    }

    /// 当前生效的总延迟偏移（秒）。
    ///
    /// 三项相加：谱面自带的偏移 + 玩家配置的偏移 + `ChartInfo` 的偏移；
    /// 调延迟模式修改并保存的正是最后一项（`info_offset`）。
    fn offset(&self) -> f32 {
        self.chart.offset + self.res.config.offset + self.info_offset
    }

    /// 绘制「调整延迟」面板并把用户操作翻译成场景动作。
    ///
    /// 三个出口：取消 → 以 `None` 结果退出（不改配置）；重置 → 把 `info_offset` 归零；
    /// 保存 → 以 `Some(offset)` 结果退出，由调用方写回谱面信息。
    /// `ita` 表示是否处于「交互」状态（由当前相位决定），非交互时面板只展示、不响应点击。
    fn tweak_offset(&mut self, ui: &mut Ui, ita: bool) {
        let labels = OffsetPanelLabels {
            adjust_offset: tl!("adjust-offset"),
            auto_offset: tl!("auto-offset-btn"),
            analysis_prompt: tl!("analysis-prompt"),
            analysis_computing: tl!("analysis-computing"),
            cancel: tl!("offset-cancel"),
            reset: tl!("offset-reset"),
            save: tl!("offset-save"),
        };
        match self.offset_analysis.render(ui, &self.chart, &mut self.info_offset, ita, &labels) {
            Some(OffsetPanelAction::Cancel) => self.next_scene = Some(NextScene::PopWithResult(Box::new(None::<f32>))),
            Some(OffsetPanelAction::Reset) => self.info_offset = 0.,
            Some(OffsetPanelAction::Save(offset)) => self.next_scene = Some(NextScene::PopWithResult(Box::new(Some(offset)))),
            None => {}
        }
    }
    /// 平均帧率（FPS），用于结算页展示性能。
    ///
    /// 只在 `Playing` 且未暂停的帧上采样（见 `render`），因此不会把暂停时的高帧率算进去。
    /// 样本为空时返回 `None` 而不是 0 或 NaN，由调用方决定是否展示。
    pub fn get_avg_fps(&self) -> Option<f32> {
        if self.fps_frame_count > 0 && self.fps_total_time > 0.0 {
            Some(self.fps_frame_count as f32 / self.fps_total_time as f32)
        } else {
            None
        }
    }
}

// 场景框架接口的实现：进入 / 暂停 / 恢复 / 每帧更新 / 触摸 / 渲染 / 场景切换。
// GameScene 只在这里与框架交互（调用顺序、渲染目标、输入分发都由框架负责），
// 因此所有「跨场景」的状态同步点都集中在这几个方法里。
impl Scene for GameScene {
    /// 进入场景（首次进入或从子场景返回）。
    ///
    /// 关键动作：
    /// 1. 重建音乐播放器——流速 / 音量可能在子场景中被改过，重建才能生效；
    /// 2. 把框架传入的 `render_target` 交给相机：录制 / 预览等场景通过它把画面渲染到纹理，
    ///    `None` 表示直接渲染到屏幕；
    /// 3. 同步时间轴参数（流速、是否自动校正）并整体复位（`reset!`）；
    /// 4. 显式设置相机，并置 `first_in = true`——练习模式据此在首帧自动暂停，先让玩家设区间。
    /// 5. 防御性地设置WORLD_SCALE
    ///
    /// # Errors
    /// 音乐创建失败时返回错误。
    fn enter(&mut self, tm: &mut TimeManager, target: Option<RenderTarget>) -> Result<()> {
        #[cfg(target_arch = "wasm32")]
        on_game_start();
        // HarmonyOS 专有：进入游玩即开启系统手势拦截，避免激烈操作误触发系统手势。
        #[cfg(target_env = "ohos")]
        miniquad::native::set_interceptor_state(true);
        self.music = Self::new_music(&mut self.res)?;
        self.res.camera.render_target = target;
        tm.speed = self.res.config.speed as _;
        tm.adjust_time = self.res.config.adjust_time;
        reset!(self, self.res, tm);
        set_camera(&self.res.camera);
        self.first_in = true;
        // 重置世界缩放
        reset_ws();
        Ok(())
    }

    /// 暂停场景（玩家主动暂停，或被系统 / 上层切换打断）。
    ///
    /// 先清掉可能正在进行的 3 秒倒计时（`pause_rewind`），否则恢复时倒计时状态会残留；
    /// 然后用 `tm.paused()` 做幂等保护，避免重复暂停音乐。ohos 上同时放开手势拦截。
    ///
    /// # Errors
    /// 音乐暂停失败时返回错误。
    fn pause(&mut self, tm: &mut TimeManager) -> Result<()> {
        if !tm.paused() {
            self.pause_rewind = None;
            self.music.pause()?;
            tm.pause();
        }
        #[cfg(target_env = "ohos")]
        miniquad::native::set_interceptor_state(false);
        // 重置世界缩放
        reset_ws();
        Ok(())
    }

    /// 恢复场景。
    ///
    /// 注意这里**只为非 `Playing` 相位恢复时间轴**（例如开场前被暂停）：正式的「继续游戏」
    /// 路径由 `overlay_ui` 处理，它需要额外把音乐与时间轴回退 3 秒并启动倒计时，
    /// 若在此处抢先 `resume()` 会破坏那段逻辑。
    fn resume(&mut self, tm: &mut TimeManager) -> Result<()> {
        if !matches!(self.state, State::Playing) {
            tm.resume();
        }
        // 重置世界缩放
        reset_ws();
        Ok(())
    }

    /// 每帧更新：推进相位状态机、驱动判定与交互输入、维护各种瞬时状态。
    ///
    /// # 执行顺序（重要）
    /// 1. 延迟分析面板推进（只有调延迟模式会用到）；
    /// 2. 音频后端自愈；`Playing` 相位下用音乐实际位置校正时间轴以抑制漂移；
    /// 3. 练习模式越界检查（超出区间终点则复位回起点并暂停）；
    /// 4. 相位状态机推进，得到本帧应使用的时间；
    /// 5. 减去总偏移得到谱面时间 `res.time`；
    /// 6. **在谱面视口下**执行判定（触摸坐标换算依赖视口）；
    /// 7. 外部同步回调 → 判定线指示色 / 即死模式 → 谱面动画更新；
    /// 8. 交互输入（空格暂停、左右键跳转、Q 退出）与输入框回执。
    ///
    /// # 两种时间基准
    /// `tm.now()` 是「音乐时间轴」（受 `speed` 缩放，可能被 `adjust_time` 自动校正）；
    /// `res.time = tm.now() - offset` 才是谱面时间。判定与谱面动画一律使用后者，
    /// 因此所有偏移量都在这里统一结算，其他位置不得重复扣减。
    ///
    /// # Errors
    /// 音乐播放 / 定位、音频自愈、成绩保存回调失败都会向上传播。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        // ---- 阶段 1：延迟分析面板 ----
        self.offset_analysis
            .update(&self.chart, &self.res, self.info_offset, tm.real_time() as f32);

        // ---- 阶段 2：音频自愈与时间轴校正 ----
        // recover_if_needed：音频设备被系统抢占（来电、长时间切后台）后尝试重开；
        // Playing 时用音乐播放器的真实位置校正时间轴，避免长时间播放累积的漂移。
        self.res.audio.recover_if_needed()?;
        if matches!(self.state, State::Playing) {
            tm.update(self.music.position());
        }
        // ---- 阶段 3：练习模式越界处理 ----
        // 播放越过练习区间终点即整体复位并暂停：先保存 / 恢复相位，让玩家停留在原来的阶段。
        // 复位会清空判定状态（也顺带重置本局的 FPS 统计）。
        if self.mode == GameMode::Exercise && tm.now() > self.exercise_range.end && !tm.paused() {
            let state = self.state.clone();
            reset!(self, self.res, tm);
            self.state = state;
            tm.seek_to(self.exercise_range.start);
            tm.pause();
            self.music.pause()?;
            // 已暂停：放开系统手势拦截。
            #[cfg(target_env = "ohos")]
            miniquad::native::set_interceptor_state(false);
        }
        // ---- 阶段 4：相位状态机 ----
        // 每个分支都返回「本帧应当使用的时间」；相位的切换只在这里发生。
        // 同时设置世界缩放
        let offset = self.offset();
        let time = tm.now();
        let time = match self.state {
            State::Starting => {
                // 重置世界缩放
                reset_ws();

                // 淡入结束：置满透明度、进入 BeforeMusic，并把时间轴定位到播放起点——
                // 普通模式定位到 offset（负偏移会被直接跳过），练习模式定位到区间起点。
                if time >= Self::BEFORE_TIME {
                    self.res.alpha = 1.;
                    self.state = State::BeforeMusic;
                    tm.reset();
                    tm.seek_to(if self.mode == GameMode::Exercise {
                        self.exercise_range.start
                    } else {
                        offset.min(0.) as f64
                    });
                    self.last_update_time = tm.real_time();
                    if self.first_in && self.mode == GameMode::Exercise {
                        tm.pause();
                        self.first_in = false;
                    }
                    tm.now()
                } else {
                    // 淡入中：时间轴被固定为起始时间（见该分支的返回值），这里只更新透明度。
                    // Windows 专属 workaround：粒子系统必须先「空发」一次，否则首次真正发射
                    //（通常在开场后的第一批 note）会因为驱动 / GPU 的首次初始化而卡顿或不可见。
                    // 做法是把粒子尺寸临时改为 0 发射一枚不可见粒子：既完成初始化，
                    // 又不会在画面上留下痕迹，随后恢复原配置。
                    #[cfg(target_os = "windows")]
                    {
                        // wtf bro. why must particles exist on Windows?
                        let emitter_config = self.res.emitter.emitter.config.clone();
                        let emitter_square_config = self.res.emitter.emitter_square.config.clone();
                        self.res.emitter.emitter.config.size = 0.0;
                        self.res.emitter.emitter_square.config.size = 0.0;
                        self.res.emitter.emitter.emit(vec2(0.0, 0.0), 1);
                        self.res.emitter.emitter_square.emit(vec2(0.0, 0.0), 1);
                        self.res.emitter.emitter.config = emitter_config;
                        self.res.emitter.emitter_square.config = emitter_square_config;
                    }
                    self.res.alpha = (1. - (1. - time / Self::BEFORE_TIME).powi(3)) as f32;
                    if self.mode == GameMode::Exercise {
                        self.exercise_range.start
                    } else {
                        offset as f64
                    }
                }
            }
            State::BeforeMusic => {
                // 重置世界缩放
                reset_ws();
                // 时间轴越过 0 才开始播放音乐（保证音频从 0 秒起播，
                // 而不是从中途某处开始），随后进入 Playing。
                // 播放前检查暂停状态：练习模式首帧自动暂停时不能直接开播。
                if time >= 0.0 {
                    self.music.seek_to(time)?;
                    if !tm.paused() {
                        self.music.play()?;
                    }
                    self.state = State::Playing;
                }
                time
            }
            State::Playing => {
                // 越过「曲末 + WAIT_TIME」即进入收尾相位；
                // ohos 上同时放开手势拦截（演奏已结束，允许使用系统手势）。
                if time > self.res.track_length + WAIT_TIME {
                    self.state = State::Ending;
                    #[cfg(target_env = "ohos")]
                    miniquad::native::set_interceptor_state(false);
                }
                // 打开缩小谱面配置时的世界缩放
                // 渐入线性插值
                if self.res.config.mods.contains(Mods::REDUCE_WORLD_SIZE) {
                    let t = REDUCE_WORLD_SIZE_SECS.min(time as f32) / REDUCE_WORLD_SIZE_SECS;
                    WORLD_SCALE.store(
                        1.0 + t * (REDUCE_WORLD_SIZE_TARGET - 1.0),
                        Ordering::Relaxed
                    );
                }
                time
            }
            State::Ending => {
                // 重置世界缩放
                reset_ws();
                // t = 曲末之后再经过的时间。
                // 超过 AFTER_TIME + 0.3 说明画面已完全淡出，此时一次性完成结算与场景切换请求。
                let t = time - self.res.track_length - WAIT_TIME;
                if t >= AFTER_TIME + 0.3 {
                    // 上传用的记录数据只在闭源构建下生成，且需同时满足四个条件：
                    // 非离线模式、不含 UNRATED mod、未启用键盘玩法、未降速——
                    // 这些条件都会影响成绩的可比性（源码 TODO：这层保护还需要加强）。
                    let mut record_data = None;
                    // TODO strengthen the protection
                    #[cfg(closed)]
                    if let Some(upload_fn) = &self.upload_fn {
                        if !self.res.config.offline_mode
                            && !self.res.config.mods.intersects(Mods::UNRATED)
                            && !self.res.config.use_keyboard
                            && self.res.config.speed >= 1.0 - 1e-3
                        {
                            if let Some(player) = &self.player {
                                if let Some(chart) = &self.res.info.id {
                                    record_data = Some(encode_record(self, player.id, *chart));
                                }
                            }
                        }
                    }
                    let result = self.judge.result();
                    // 是否计入成绩：UNRATED mod（autoplay / 无 shader）或降速（speed < 1.0）
                    // 一律不记录，避免用「降速 + 自动演示」刷分。
                    // 阈值用 1.0 - 1e-3 而非严格 1.0，是为了容忍浮点误差。
                    let record = if self.res.config.mods.intersects(Mods::UNRATED) || self.res.config.speed < 1.0 - 1e-3 {
                        None
                    } else {
                        Some(SimpleRecord {
                            score: result.score as _,
                            accuracy: result.accuracy as _,
                            full_combo: result.max_combo == result.num_of_notes,
                        })
                    };
                    // 场景切换请求按模式分支（写入 next_scene，由 Scene::next_scene 在帧末返回）：
                    // - Normal / NoRetry / View：合并成绩后以 `Overlay` 弹出结算界面；
                    // - TweakOffset：以 `PopWithResult(None)` 退回上层（不改动偏移，等用户决定）；
                    // - Exercise：不切换，继续留在练习场景，由玩家自己退出。
                    self.next_scene = match self.mode {
                        GameMode::Normal | GameMode::NoRetry | GameMode::View => {
                            let historic_best = self.player.as_ref().map_or(0, |it| it.historic_best);
                            if let Some(new_rec) = &record {
                                // 顺序有意义：先写盘（本局成绩），再合并进场景最佳，最后刷新历史最佳。
                                if let Some(f) = &self.save_fn {
                                    f(new_rec.clone())?;
                                }
                                if let Some(best) = &mut self.best_record {
                                    best.update(new_rec);
                                } else {
                                    self.best_record = record.clone();
                                }
                                if let Some(best) = &self.best_record {
                                    if let Some(player) = &mut self.player {
                                        player.historic_best = player.historic_best.max(best.score as _);
                                    }
                                }
                            }
                            // 构造结算界面所需的全部输入：纹理（背景 / 曲绘 / 头像 / 图标）、
                            // 本局结果、配置、资源包的 ending 配置、上传回调、玩家 rks、
                            // 历史最佳、待上传记录、本场景最佳成绩，以及（可选的）平均帧率。
                            Some(NextScene::Overlay(Box::new(EndingScene::new(
                                self.res.background.clone(),
                                self.res.illustration.clone(),
                                self.res.player.clone(),
                                self.res.icons.clone(),
                                self.res.icon_retry.clone(),
                                self.res.icon_proceed.clone(),
                                self.res.mod_icons.clone(),
                                self.res.info.clone(),
                                self.judge.result(),
                                &self.res.config,
                                self.res.res_pack.ending.clone(),
                                self.upload_fn.as_ref().map(Arc::clone),
                                self.player.as_ref().map(|it| it.rks),
                                historic_best,
                                record_data,
                                self.best_record.clone(),
                                if self.res.config.show_avg_fps { self.get_avg_fps() } else { None },
                            )?)))
                        }
                        GameMode::TweakOffset => Some(NextScene::PopWithResult(Box::new(None::<f32>))),
                        GameMode::Exercise => None,
                    };
                }
                // 结算阶段的画面淡出：alpha 按二次曲线降到 0，
                // 与结算界面的 Overlay 叠加，避免两套画面之间出现突变。
                self.res.alpha = (1. - (t / AFTER_TIME).min(1.).powi(2)) as f32;
                // 收尾阶段时间停在曲末：让谱面停在最后一帧，而不是继续向外推。
                self.res.track_length
            }
        };
        if tm.paused() {
            // 重置世界缩放
            reset_ws();
        }
        // ---- 阶段 5：换算谱面时间 ----
        // 减去总偏移并夹到 0：负偏移（谱面前置留白）会被跳过，保证 res.time 恒非负。
        let time = (time - offset as f64).max(0.);
        self.res.time = time;
        // ---- 阶段 6：执行判定（必须在谱面视口下调用）----
        // Judge::update 内部的触摸坐标换算依赖当前视口，因此这里临时把 GL 视口切到谱面相机
        // 的视口，调用完立即切回 None（全屏），避免影响后续 HUD / 叠加层绘制。
        // 三个跳过条件：时间轴暂停中（含练习模式自动暂停）；正处于「继续游戏」的 3 秒倒计时
        //（倒计时期间不应产生判定）；纯观赏模式（View）根本不判定。
        if !tm.paused() && self.pause_rewind.is_none() && self.mode != GameMode::View {
            self.gl.quad_gl.viewport(self.res.camera.viewport);
            self.judge.update(&mut self.res, &mut self.chart, &mut self.bad_notes);
            self.gl.quad_gl.viewport(None);
        }
        // ---- 阶段 7：外部同步与视觉状态 ----
        // 把本帧时间与判定器交给外部回调（联机 / 回放 / 练习统计）。
        // 位置在判定之后：外部能读到本帧最新的判定结果。
        if let Some(update) = &mut self.update_fn {
            update(self.res.time, &mut self.res, &mut self.judge);
        }
        let counts = self.judge.counts();
        // 判定线指示色：开启 AP/FC 指示且没有 Bad/Miss 时，按「是否已有 Good」显示
        // Perfect 色或 Good 色（即场上是否还有达成 AP 的可能）；否则用白色（资源包默认）。
        self.res.judge_line_color = if counts[2] + counts[3] == 0 && self.res.config.ap_fc_indicator {
            if counts[1] == 0 {
                self.res.res_pack.info.color_perfect()
            } else {
                self.res.res_pack.info.color_good()
            }
        } else {
            WHITE
        };
        // 即死模式：AP 下出现任何非 Perfect（Good/Bad/Miss），FC 下出现 Bad/Miss，立即失败。
        // 触发后暂停音乐与时间轴、标记 dead（禁用继续按钮）并给出提示；`!self.dead` 保证只触发一次。
        if !self.dead
            && matches!(self.state, State::Playing)
            && (self.res.config.mods.contains(Mods::INSTANT_DEATH_AP) && counts[1] + counts[2] + counts[3] > 0
                || self.res.config.mods.contains(Mods::INSTANT_DEATH_FC) && counts[2] + counts[3] > 0)
        {
            if !self.music.paused() {
                self.music.pause()?;
            }
            tm.pause();
            self.dead = true;
            #[cfg(target_env = "ohos")]
            miniquad::native::set_interceptor_state(false);
            show_message(tl!("game-over")).error();
        }
        // 判定线颜色随场景透明度一起淡出（只缩放 alpha，不改色相）。
        self.res.judge_line_color.a *= self.res.alpha;
        // 谱面动画推进：判定线 / note 的位置、旋转、透明度与谱面级特效。
        self.chart.update(&mut self.res);
        // ---- 阶段 8：交互输入 ----
        let res = &mut self.res;
        // 空格：暂停 / 继续的快捷键（暂停面板之外的第二种入口）。
        // 暂停时只在 Playing 相位恢复（避免在开场前误触发播放）；未暂停时在 Playing / BeforeMusic
        // 相位暂停，其它相位（开场淡入、结算）忽略。
        if res.config.interactive && is_key_pressed(KeyCode::Space) {
            if tm.paused() {
                if matches!(self.state, State::Playing) {
                    self.music.play()?;
                    tm.resume();
                }
            } else if matches!(self.state, State::Playing | State::BeforeMusic) {
                if !self.music.paused() {
                    self.music.pause()?;
                }
                tm.pause();
            }
        }
        // 以下快捷键只在「可交互」时生效（见 `interactive`：Playing 相位且未禁用交互）。
        if Self::interactive(res, &self.state) {
            // 左键：回退 1 秒，用于重听刚过去的段落（仅在开启键盘玩法时可用）。
            // 音乐位置与时间轴必须一起 seek，否则音画错位；音乐位置的下界夹到 0。
            if is_key_pressed(KeyCode::Left) && res.config.use_keyboard {
                res.time -= 1.;
                let dst = (self.music.position() - 1.).max(0.);
                self.music.seek_to(dst)?;
                tm.seek_to(dst);
            }
            // 右键：前进 5 秒，用于快速跳过已听过的段落（同样仅在键盘玩法下可用），上界为曲长。
            if is_key_pressed(KeyCode::Right) && res.config.use_keyboard {
                res.time += 5.;
                let dst = (self.music.position() + 5.).min(res.track_length);
                self.music.seek_to(dst)?;
                tm.seek_to(dst);
            }
            // Q：直接退出本场景（不走结算），用于快速切歌 / 放弃本局。
            if is_key_pressed(KeyCode::Q) {
                self.should_exit = true;
            }
        }
        // ---- 阶段 9：场景级 effect 与输入框回执 ----
        // 场景级 effect 需要每帧推进自身的时间状态（例如滤镜动画）。
        for e in &mut self.effects {
            e.update(&self.res);
        }
        // 处理练习区间输入框的回执：校验格式与取值范围，失败时给出本地化提示并保留原值；
        // 不属于本场景的输入原样退回（return_input），交给上层继续分发。
        if let Some((id, text)) = take_input() {
            // 对用户暴露的合法时间范围从「谱面真正开始」算起（总偏移只取负值）。
            let offset = self.offset().min(0.);
            match id.as_str() {
                // 设置练习起点。合法范围 = [谱面起点, min(曲长, 终点 - 3s)]，
                // 即起点最多只能顶到终点前 3 秒，保证练习区间不会短到无法使用。
                // 格式解析失败与数值越界分别给出不同的本地化提示。
                "exercise_start" => {
                    if let Some(t) = parse_time(&text) {
                        if !(offset as f64..self.res.track_length.min(self.exercise_range.end - 3.).max(offset as f64)).contains(&t) {
                            show_message(tl!("ex-time-out-of-range")).error();
                        } else {
                            self.exercise_range.start = t;
                            show_message(tl!("ex-time-set")).ok();
                        }
                    } else {
                        show_message(tl!("ex-invalid-format")).error();
                    }
                }
                // 设置练习终点。合法范围 = [max(起点 + 3s, 谱面起点), 曲长]，
                // 与起点同样保留至少 3 秒区间；越界与格式错误给出不同提示。
                "exercise_end" => {
                    if let Some(t) = parse_time(&text) {
                        if !((self.exercise_range.start + 3.).max(offset as f64).min(self.res.track_length)..self.res.track_length).contains(&t) {
                            show_message(tl!("ex-time-out-of-range")).error();
                        } else {
                            self.exercise_range.end = t;
                            show_message(tl!("ex-time-set")).ok();
                        }
                    } else {
                        show_message(tl!("ex-invalid-format")).error();
                    }
                }
                _ => return_input(id, text),
            }
        }
        Ok(())
    }

    /// 场景级触摸分发（框架在分发触摸时调用）。
    ///
    /// 只处理两类「组件自身不便处理」的输入：
    /// 1. 调延迟模式下的偏移分析面板（需要真实时间戳）；
    /// 2. 练习面板中「起点 / 终点」数值按钮的点击——点击后弹出输入框请求精确时间。
    ///
    /// 练习面板的触摸会被换算到谱面坐标系后再做命中测试（与绘制时的换算保持一致），
    /// 并且只在暂停时响应：拖动 / 修改区间必须在停止播放的状态下进行。
    ///
    /// # Returns
    /// `true` 表示本次触摸已被本场景消费，框架不必再分发给其它组件。
    fn touch(&mut self, tm: &mut TimeManager, touch: &Touch) -> Result<bool> {
        if self.mode == GameMode::TweakOffset {
            self.offset_analysis.touch(touch, tm.real_time() as f32);
        }
        if self.mode == GameMode::Exercise && tm.paused() {
            let touch = Touch {
                position: touch.position * self.touch_scale(),
                ..touch.clone()
            };
            if self.exercise_btns.0.touch(&touch) {
                request_input("exercise_start", InputBox::new().default_text(fmt_time(self.exercise_range.start as f32)));
                return Ok(true);
            }
            if self.exercise_btns.1.touch(&touch) {
                request_input("exercise_end", InputBox::new().default_text(fmt_time(self.exercise_range.end as f32)));
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 渲染一帧：玩法内容 → （按需）离屏 target → 叠加层 → 场景后处理 → 贴回屏幕。
    ///
    /// # 分段管线（顺序不可交换）
    ///
    /// 1. **采样帧率**（仅 `Playing` 且未暂停）；
    /// 2. **尺寸与相机**：`update_size` 在视口变化时重建离屏 target；
    /// 3. **选择目标**：`chart_onto` 指明「谱面内容画到哪里」——存在 `chart_target` 时用它的
    ///    `input()`（MSAA 多重采样 FBO），否则直接画到相机的 `render_target`；
    /// 4. **背景**：临时相机清屏 + 画背景图；
    /// 5. **计算谱面视口**：把相机视口按离屏 FBO 的原点平移；
    /// 6. **切到离屏 pass 与谱面视口**：此后所有绘制落在 FBO 内；
    /// 7. **画谱面**：先铺一层按 `background_dim` 与场景 alpha 混合的遮罩，再执行 `Chart::render`；
    /// 8. **切回输出 pass**（`chart_target.output()` 或相机 pass）：Bad 提示 / 粒子 / HUD / 叠加层
    ///    都画在已解析的谱面画面之上，因此不会被谱面级 effect 影响；
    /// 9. **TweakOffset 面板**：用全屏视口 + 屏幕宽高比的独立相机绘制；
    /// 10. **场景级后处理**；
    /// 11. **贴回屏幕**：`gl.flush()` → 切 viewport / 相机 → `draw_texture_ex`。
    ///
    /// # 为什么必须成对 push / pop 相机状态
    /// `set_camera` 会改写 macroquad 的全局相机，而谱面渲染与 HUD 各自还会再切换相机；
    /// 用 `push_camera_state` / `pop_camera_state` 包裹每次临时改动，才能保证退出该段后
    /// 其它绘制看到的仍是外层期望的相机。
    ///
    /// # 为什么切换 viewport 前必须 flush
    /// 绘制是批量提交的，viewport / render_pass 这类状态在**提交时**才生效。
    /// 若不先 `gl.flush()` 就切换，之前排队但未提交的顶点会按新视口绘制，导致画面错位。
    ///
    /// # Errors
    /// HUD / 叠加层中的音乐操作失败时向上传播。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        // ---- 1. 平均帧率采样 ----
        // 只在 Playing 且未暂停时累加，暂停 / 菜单期间的帧不计入，避免拉高平均值。
        if self.res.config.show_avg_fps {
            let current_time = tm.real_time();
            if matches!(self.state, State::Playing) && !tm.paused() {
                let frame_delta = current_time - self.fps_last_frame_time;
                self.fps_total_time += frame_delta;
                self.fps_frame_count += 1;
            }
            self.fps_last_frame_time = current_time;
        }

        // ---- 2. 视口尺寸与相机 ----
        // asp 是「当前渲染目标」的宽高比（不一定等于谱面宽高比）。
        // update_size 在视口变化时重建离屏 target（并由配置决定是否真的需要它）；
        // View 模式每帧都重设相机，因为该模式的渲染目标可能在帧间被外部切换。
        let res = &mut self.res;
        let asp = ui.viewport.2 as f32 / ui.viewport.3 as f32;
        if res.update_size(ui.viewport) || self.mode == GameMode::View {
            set_camera(&res.camera);
        }

        // ---- 3. 选择离屏 target ----
        // msaa：采样数 > 1 表示开启多重采样抗锯齿。chart_target 仅在「存在 effect」或
        // 「采样数 != 1」时才被创建（见 Resource::update_size），因此它也是 no_effect 的判据。
        // MSAA 开启时谱面先画进多重采样 FBO（input()），谱面渲染结束时再 blit 到 output()；
        // 未开启时直接使用 output()。
        let msaa = res.config.sample_count > 1;

        let chart_onto = res
            .chart_target
            .as_ref()
            .map(|it| if msaa { it.input() } else { it.output() })
            .or(res.camera.render_target);
        // ---- 4. 背景 ----
        // 用一个「不带视口」的临时相机清屏并绘制背景：存在 chart_target 时整块 FBO 都要填底色，
        // 因此 viewport 传 None（否则只会清掉局部区域）。绘制前后成对 push / pop 相机状态。
        push_camera_state();
        // 背景缩放相机（在 clear_background 与 draw_background 之间）
        let ws = ws();
        set_camera(&Camera2D {
            zoom: vec2(ws, -asp * ws),
            viewport: if res.chart_target.is_some() { None } else { Some(ui.viewport) },
            render_target: chart_onto,
            ..Default::default()
        });
        clear_background(BLACK);
        draw_background(*res.background);
        pop_camera_state();

        // ---- 5. 计算谱面视口 ----
        // 离屏 FBO 的原点在自己内部，而相机的 viewport 是屏幕坐标，故有 FBO 时需要按
        // ui.viewport 的 x / y 把视口平移回 FBO 内部坐标系；直接画到屏幕上时原样使用。
        let chart_target_vp = if res.chart_target.is_some() {
            let vp = res.camera.viewport.unwrap();
            Some((vp.0 - ui.viewport.0, vp.1 - ui.viewport.1, vp.2, vp.3))
        } else {
            res.camera.viewport
        };
        // ---- 6. 切换到离屏渲染通道与谱面视口 ----
        // 从这里到下面切回输出通道为止，所有绘制都落在 chart_onto 这块 FBO 内。
        self.gl.quad_gl.render_pass(chart_onto.map(|it| it.render_pass));
        self.gl.quad_gl.viewport(chart_target_vp);

        // 谱面背景遮罩：按谱面元信息给出的 dim 值与场景 alpha 混合，
        // 用来压暗背景以便看清 note（alpha 参与混合是为了随开场 / 收尾一起淡出）。
        let h = 1. / res.aspect_ratio;
        draw_rectangle(-1., -h, 2., h * 2., Color::new(0., 0., 0., res.alpha * res.info.background_dim));

        self.chart.render(ui, res);

        // ---- 7. 切回输出通道 ----
        // 回到 chart_target.output()（或相机的 render_pass），即「已解析、可采样的谱面画面」。
        // 之后的 Bad 提示 / 粒子 / HUD / 叠加层都画在它之上，不受谱面级 effect 影响。
        self.gl.quad_gl.render_pass(
            res.chart_target
                .as_ref()
                .map(|it| it.output().render_pass)
                .or_else(|| res.camera.render_pass()),
        );

        // ---- 8. 叠加层 ----
        // Bad 提示：retain 的闭包返回 false 表示该提示已播放完毕，可以移除。渲染 + 缩放
        let s = Matrix::identity().append_nonuniform_scaling(&Vector::new(ws, ws));
        res.with_model(s, |res| {
            self.bad_notes.retain(|dummy| dummy.render(res))
        });
        // 粒子推进：dt 取两次 render 之间的真实时间（用 mem::replace 顺手把 last_update_time
        // 更新为本次时间），因此粒子动画速度不受逻辑帧率 / 暂停影响。
        let t = tm.real_time();
        let dt = (t - std::mem::replace(&mut self.last_update_time, t)) as f32;
        if res.config.particle {
            // 粒子缩放相机
            push_camera_state();
            set_camera(&Camera2D {
                zoom: vec2(ws, -asp * ws),
                render_target: res.chart_target.as_ref().map(|it| it.output()).or(res.camera.render_target),
                viewport: Some(ui.viewport),
                ..Default::default()
            });
            res.emitter.draw(dt);
            pop_camera_state();
        }
        // HUD 先画，暂停 / 练习面板后画：后画的覆盖关系在上，保证面板压在 HUD 之上。
        ui.with(s, |ui| self.ui(ui, tm))?;
        ui.with(s, |ui| self.overlay_ui(ui, tm))?;

        // ---- 9. TweakOffset 面板 ----
        // 它需要「全屏视口 + 屏幕宽高比」的独立相机（不受谱面视口限制），
        // 因此先把 viewport 设为 None 并换相机，绘制结束后 pop 恢复外层相机。
        if self.mode == GameMode::TweakOffset {
            push_camera_state();
            self.gl.quad_gl.viewport(None);
            set_camera(&Camera2D {
                zoom: vec2(1., -screen_aspect()),
                render_target: self.res.chart_target.as_ref().map(|it| it.output()).or(self.res.camera.render_target),
                ..Default::default()
            });
            // 第二个参数表示面板是否响应交互（非 Playing 相位只读展示）。
            self.tweak_offset(ui, Self::interactive(&self.res, &self.state));
            pop_camera_state();
        }

        // ---- 10. 场景级后处理 ----
        // 谱面没有任何 effect 且特效被配置关闭（no_effect）时整段跳过。
        // 这里用 zoom = (WORLD_SCALE, asp * WORLD_SCALE) 的正向相机：effect 采样的是「已经画好的画面纹理」，
        // 不需要谱面坐标系，用正向 zoom 才能让纹理方向正确。
        if !self.res.no_effect && !self.effects.is_empty() {
            push_camera_state();
            set_camera(&Camera2D {
                // 世界缩放
                zoom: vec2(ws, asp * ws),
                ..Default::default()
            });
            for e in &self.effects {
                e.render(&mut self.res);
            }
            pop_camera_state();
        }
        // ---- 11. 把离屏结果贴回屏幕 ----
        // 只有存在 chart_target 时才需要这一步（否则谱面本来就直接画在屏幕 / 相机 target 上）。
        // 三个动作的先后顺序不能变：
        // 1. `gl.flush()`：先把此前排队的绘制（粒子、场景 effect 等）提交到各自的 target；
        // 2. 切换 viewport 与相机到输出目标；
        // 3. `draw_texture_ex` 把 output 纹理铺满整个可视区域。
        // 若省略 flush，已排队但尚未提交的顶点会使用新视口，导致画面错位。
        if msaa || !self.res.no_effect {
            // render the texture onto screen
            if let Some(target) = &self.res.chart_target {
                self.gl.flush();
                push_camera_state();
                self.gl.quad_gl.viewport(None);
                set_camera(&Camera2D {
                    zoom: vec2(1., asp),
                    render_target: self.res.camera.render_target,
                    viewport: Some(ui.viewport),
                    ..Default::default()
                });
                // 用 output().texture（MSAA 已解析、场景 effect 已应用）铺满视口：
                // 原点取 (-1, -ui.top)，尺寸 2 × (ui.top * 2)，恰好覆盖整个归一化可视区域。
                // 注意这里的 zoom 与谱面相机的 zoom 符号相反（纹理的 y 方向与谱面坐标系相反，
                // 需要在此再翻一次才能正立显示）。
                draw_texture_ex(
                    target.output().texture,
                    -1.,
                    -ui.top,
                    WHITE,
                    DrawTextureParams {
                        dest_size: Some(vec2(2., ui.top * 2.)),
                        ..Default::default()
                    },
                );
                pop_camera_state();
            }
        }
        Ok(())
    }

    /// 帧末的场景切换决策（框架每帧询问一次）。
    ///
    /// 优先级：`should_exit`（玩家主动退出）> `next_scene`（结算 / 调延迟面板写入的请求）> 不切换。
    /// 两条分支都会做同一套「收尾清理」：若时间轴仍处于暂停就恢复、把流速复位为 1.0、
    /// 关闭时间自动校正——这些是场景级设置，必须还原，否则会污染后续场景（例如主菜单）。
    ///
    /// 退出时按模式决定回传内容：`Normal` 回传本场景最佳成绩（供上层刷新分数 / 榜单）；
    /// `TweakOffset` 回传 `None`（等价于「未修改偏移」）；`Exercise` / `NoRetry` / `View` 无结果。
    fn next_scene(&mut self, tm: &mut TimeManager) -> NextScene {
        // 重置世界缩放
        reset_ws();

        if self.should_exit {
            // 收尾清理：先把时间轴从暂停状态恢复，并复位流速与自动校正开关。
            if tm.paused() {
                tm.resume();
            }
            tm.speed = 1.0;
            tm.adjust_time = false;
            match self.mode {
                // return result to update score and refresh
                // 正常游玩：把本场景累计的最佳成绩回传给上层，用于刷新分数与列表。
                GameMode::Normal => {
                    if let Some(rec) = &self.best_record {
                        NextScene::PopWithResult(Box::new(rec.clone()))
                    } else {
                        NextScene::Pop
                    }
                }
                // not sure if they need result. just keep it
                // 练习 / 禁重试 / 观赏都不产生成绩，只做「返回上一层」。
                GameMode::Exercise | GameMode::NoRetry | GameMode::View => NextScene::Pop,
                // 调延迟模式退出且未点保存：回传 None 表示「不修改偏移」。
                GameMode::TweakOffset => NextScene::PopWithResult(Box::new(None::<f32>)),
            }
        } else if let Some(next_scene) = self.next_scene.take() {
            // 结算请求（update 中写入）优先于「什么都不做」；同样先做收尾清理。
            // 仅当真的要切换（非 NextScene::None）时才恢复时间轴，避免无谓地改动暂停状态。
            if !matches!(next_scene, NextScene::None) && tm.paused() {
                tm.resume();
            }
            tm.speed = 1.0;
            tm.adjust_time = false;
            next_scene
        } else {
            NextScene::None
        }
    }
}
