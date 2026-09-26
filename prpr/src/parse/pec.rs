//! PEC 解析器：旧版 Phigros / PE 的**纯文本**谱面格式。
//!
//! PEC 没有 JSON 结构，靠“一行一条命令 + 空白分词”描述谱面，解析器本质上是一个
//! 逐行状态机。行命令语法（改造时最容易踩坑的地方）：
//!
//! - 首行不是命令，只有一个数字：谱面偏移（毫秒），按 `ms / 1000 - 0.15` 换算成秒；
//! - `bp <拍数> <bpm>`：新增 BPM 变化点，且必须在任何时间相关命令之前；
//! - `n1/n2/n3/n4 <线号> <时间> [<结束时间>] <x> <上方标记> <假音符标记>`：新增音符，
//!   1=Click、2=Hold（多一个结束时间）、3=Flick、4=Drag；时间单位为拍；
//! - `cv/cp/cd/ca <线号> <时间> …`：判定线的**瞬时**事件（速度 / 位移 / 旋转 / 不透明度）；
//! - `cm/cr/cf <线号> <时间> <结束时间> …`：判定线的**区间**事件（位移 / 旋转 / 不透明度），
//!   带缓动编号（`cm`/`cr` 显式给出，`cf` 固定为线性）；
//! - `#` 与 `&`：可单独成行，也可紧跟在音符行之后，用于改写**最近一条音符**的速度与宽度。
//!   这正是解析器必须记住 `last_line` 状态（而不是纯函数式解析）的原因。
//!
//! 归一化换算依据（PEC 的原始量纲与项目不同，全部在 [`parse_judge_line`] 里换算）：
//! - 判定线位移：x 除以 2048（画布宽）、y 除以 1400（画布高），再线性映射到 [-1, 1]；
//! - 音符横坐标：除以 1024（画布半宽）后直接作为归一化偏移；
//! - 不透明度：除以 255；**负值不参与除法**，因为 PE 用负 alpha 表示特殊渲染指令；
//! - 判定线速度：除以 5.85 换算到项目的速度量纲。
use anyhow::{bail, Context, Result};
use macroquad::color::WHITE;
use std::{cell::RefCell, collections::HashMap};
use tracing::warn;

use super::{process_lines, L10N_LOCAL, RPE_TWEEN_MAP};
use crate::{
    core::{
        Anim, AnimFloat, AnimVector, BpmList, Chart, ChartExtra, ChartSettings, JudgeLine, JudgeLineCache, JudgeLineKind, Keyframe, Note, NoteKind,
        Object, TweenId, EPS,
    },
    ext::NotNanExt,
    judge::{HitSound, JudgeStatus},
};

/// PEC 行内分词读取器：把空白分隔的 token 逐个解析成有类型的值。
///
/// PEC 每行的参数个数由命令决定、没有自描述结构，所以解析器采用“按需取下一个 token”
/// 的方式消费迭代器。每个方法在 token 缺失时统一报“行提前结束”，
/// 在类型解析失败时用 `with_context` 附加“期望的类型”；具体行号由调用方补上。
trait Take {
    /// 取下一个 token 并解析为 f32。
    fn take_f32(&mut self) -> Result<f32>;
    /// 取下一个 token 并解析为 usize（判定线编号、00/01 布尔标记等）。
    fn take_usize(&mut self) -> Result<usize>;
    /// 取下一个 token 并按 RPE 缓动编号翻译成 [`TweenId`]。
    fn take_tween(&mut self) -> Result<TweenId>;
    /// 取下一个 token 作为时间点（浮点拍数）并换算为秒。
    fn take_time(&mut self, r: &mut BpmList) -> Result<f64>;
}

// 为所有字符串迭代器实现 Take。注意 PEC 的时间字段是“浮点拍数”，不像 JSON 格式
// 那样用 `Triple` 精确表示分数拍，因此只能走 time_beats 的线性插值换算。
impl<'a, T: Iterator<Item = &'a str>> Take for T {
    /// 实现：取下一个 token 并解析 f32；token 缺失与解析失败使用不同的上下文文案。
    fn take_f32(&mut self) -> Result<f32> {
        self.next()
            .ok_or_else(|| ptl!(err "unexpected-eol"))
            .and_then(|it| -> Result<f32> { Ok(it.parse()?) })
            .with_context(|| ptl!("expected-f32"))
    }

    /// 实现：取下一个 token 并解析 usize，用于判定线编号等无符号整数参数。
    fn take_usize(&mut self) -> Result<usize> {
        self.next()
            .ok_or_else(|| ptl!(err "unexpected-eol"))
            .and_then(|it| -> Result<usize> { Ok(it.parse()?) })
            .with_context(|| ptl!("expected-usize"))
    }

    /// 实现：取 token 解析为 u8，再按 RPE 缓动编号查 [`RPE_TWEEN_MAP`]（越界退回线性）。
    fn take_tween(&mut self) -> Result<TweenId> {
        self.next()
            .ok_or_else(|| ptl!(err "unexpected-eol"))
            .and_then(|it| -> Result<u8> {
                let t = it.parse::<u8>()?;
                Ok(RPE_TWEEN_MAP.get(t as usize).copied().unwrap_or(RPE_TWEEN_MAP[0]))
            })
            .with_context(|| ptl!("expected-tween"))
    }

    /// 实现：读一个浮点拍数并按 BPM 表换算为秒。
    fn take_time(&mut self, r: &mut BpmList) -> Result<f64> {
        self.take_f32().map(|it| r.time_beats(it as f64))
    }
}

/// PEC 判定线事件的解析期中间表示（尚未排序、尚未归一化）。
///
/// 之所以先收集再统一处理，是因为 PEC 不保证事件有序、甚至允许区间重叠，
/// 需要在 [`sanitize_events`] 里统一排序与裁剪。瞬时事件（`cp/cd/ca`）用
/// [`PECEvent::single`] 构造，`start_time == end_time`。
struct PECEvent {
    /// 事件起点（秒），由命令里的时间字段经 BPM 表换算而来。
    start_time: f64,
    /// 事件终点（秒）；瞬时事件等于 `start_time`。
    end_time: f64,
    /// 事件终点处的值；区间事件在 `start_time` 处的值取上一个关键帧的值（见 [`parse_events`]）。
    end: f32,
    /// 区间事件使用的缓动（已由 RPE 编号翻译成 [`TweenId`]）；瞬时事件固定为 0。
    easing: TweenId,
}

// PECEvent 的两种构造方式：区间事件用 new，瞬时事件用 single。
impl PECEvent {
    /// 构造区间事件（`cm` / `cr` / `cf`）。
    pub fn new(start_time: f64, end_time: f64, end: f32, tween: TweenId) -> Self {
        Self {
            start_time,
            end_time,
            end,
            easing: tween,
        }
    }

    /// 构造瞬时事件（`cp` / `cd` / `ca`）：起止时间相同、缓动无意义故置 0。
    pub fn single(time: f64, value: f32) -> Self {
        Self::new(time, time, value, 0)
    }
}

/// 一条 PEC 判定线的解析期中间表示。
///
/// 与 [`JudgeLine`] 的差别：这里保存**原始事件与原始音符**，坐标/不透明度都还没归一化，
/// 也没有把事件转成关键帧动画；待整份谱面读完后由 [`parse_judge_line`] 统一换算。
#[derive(Default)]
struct PECJudgeLine {
    /// 速度事件 `(时间秒, 速度值)`，来自 `cv`；后续被积分成判定线高度。
    speed_events: Vec<(f64, f32)>,
    /// 不透明度事件，来自 `ca`（瞬时）与 `cf`（区间）；单位 0..=255，负值有特殊含义。
    alpha_events: Vec<PECEvent>,
    /// 移动事件按轴拆成 (x 事件, y 事件)，来自 `cp`/`cm`；原始单位 0..=2048 / 0..=1400。
    move_events: (Vec<PECEvent>, Vec<PECEvent>),
    /// 旋转事件，来自 `cd`/`cr`；原始单位为角度（PEC 的旋转正方向与项目相反，解析时取负）。
    rotate_events: Vec<PECEvent>,
    /// 该线上的音符；PEC 允许音符行先于任何 `c*` 事件出现，所以判定线按行号惰性扩容即可。
    notes: Vec<Note>,
}

/// 排序并消除同一判定线事件的**时间区间重叠**。
///
/// PEC 不保证事件区间互不重叠（可以上一段还没结束就写下一段）。本项目的关键帧模型
/// 要求时间单调，重叠会导致端点交错、插值错乱，因此这里按 `(end_time, start_time)`
/// 排序后，把与前一段重叠的事件起点夹到上一段终点，即裁剪为 `[last_end, end_time)`，
/// 并打一条 warning 保留现场，方便定位是谱面哪一段写得有问题。
///
/// # Arguments
/// * `events` - 就地排序并裁剪；
/// * `id` - 判定线编号，仅用于日志；
/// * `desc` - 事件类型的可读名（如 "move X"），仅用于日志。
fn sanitize_events(events: &mut [PECEvent], id: usize, desc: &str) {
    events.sort_by_key(|e| (e.end_time.not_nan(), e.start_time.not_nan()));
    let mut last_start = 0.0;
    let mut last_end = f64::NEG_INFINITY;
    for e in events.iter_mut() {
        if e.start_time < last_end {
            warn!(
                judge_line = id,
                "Overlap detected in {desc} events: [{last_start}, {last_end}) and [{}, {}). Clipping the last one to [{last_end}, {})",
                e.start_time,
                e.end_time,
                e.end_time
            );
            e.start_time = last_end;
        }
        last_start = e.start_time;
        last_end = e.end_time;
    }
}

/// 把一组事件的原始值序列转成项目的 [`AnimFloat`] 关键帧动画。
///
/// 映射规则：
/// - 瞬时事件（`start_time == end_time`）：只在起点写一个关键帧，缓动 0（无插值意义）；
/// - 区间事件：写两个关键帧——起点沿用“上一个关键帧的值”（即从当前值平滑过渡到新值），
///   终点写入本事件的值与缓动编号。
///
/// # Errors
/// 若第一个事件就是区间事件，此时没有可继承的起始值、无法确定起点，返回错误，
/// 而不是猜一个默认值（PEC 的事件顺序是谱面作者意图的一部分）。
///
/// # Panics
/// 若 `events` 为空，最终会调用 [`AnimFloat::new`] 且关键帧列表为空，
/// 触发其内部断言而 panic。既有实现未做空输入保护：只要某个解析器把“一条事件都没有”
/// 的判定线交给本函数就会命中。
fn parse_events(mut events: Vec<PECEvent>, id: usize, desc: &str) -> Result<AnimFloat> {
    sanitize_events(&mut events, id, desc);
    let mut kfs = Vec::new();
    for e in events {
        if e.start_time == e.end_time {
            kfs.push(Keyframe::new(e.start_time, e.end, 0));
        } else {
            if kfs.is_empty() {
                bail!("failed to parse {desc} events: interpolating event found before a concrete value appears");
            }
            assert!(!kfs.is_empty());
            kfs.push(Keyframe::new(e.start_time, kfs.last().unwrap().value, e.easing));
            kfs.push(Keyframe::new(e.end_time, e.end, 0));
        }
    }
    Ok(AnimFloat::new(kfs))
}

/// 把 PEC 的速度事件积分成判定线的 `height`（累计位移）。
///
/// 速度事件给的是“判定线当前的移动速率”，而项目内部判定线位置用 `height` 表示，
/// 因此需要按 速度 × 时长 逐段累加求积分。实现要点：
/// - 若首个事件不在 0 时刻，先补一个 `(0, 0)`，保证起点确定；
/// - 在每次速度变化处写入累加高度，缓动固定为 2（线性）——速度段内是匀速直线运动；
/// - 末尾再补一个到 `max_time` 的关键帧，使谱面结束后高度保持恒定而不再外推。
///
/// 这里只处理“判定线自身的运动速度”，与音符的 `Note::speed`（显示速度）无关。
///
/// # Panics
/// 函数体第一件事就是读取 `pec[0]`。若某条判定线只有音符、完全没有 `cv` 速度事件，
/// `speed_events` 为空，这里会越界 panic；既有实现未做空判断。
fn parse_speed_events(mut pec: Vec<(f64, f32)>, max_time: f64) -> AnimFloat {
    if pec[0].0 >= EPS {
        pec.insert(0, (0., 0.));
    }
    let mut kfs = Vec::new();
    let mut height = 0.0;
    let mut last_time = 0.0;
    let mut last_speed = 0f32;
    for (time, speed) in pec {
        height += (time - last_time) * last_speed as f64;
        kfs.push(Keyframe::new(time, height as f32, 2));
        last_time = time;
        last_speed = speed;
    }
    kfs.push(Keyframe::new(max_time, (height + (max_time - last_time) * last_speed as f64) as f32, 0));
    AnimFloat::new(kfs)
}

/// 把一条 PEC 判定线的中间表示转成项目的 [`JudgeLine`]。
///
/// 主要工作是归一化与语义映射：
/// - **位移**：x 除以 2048、y 除以 1400 后再 `* 2 - 1`，映射到项目的 [-1, 1]；
/// - **不透明度**：非负值除以 255；负值原样保留（PE 用它编码特殊渲染指令）；
/// - **旋转**：PEC 的正方向与项目相反，已在 `cd`/`cr` 解析时取负，这里不再处理；
/// - **缩放**：PEC 不提供判定线缩放事件，这里给 x 一个固定系数、y 保持默认。
///   `3.91 / 6.`（约 0.652）是历史硬编码常量，用于对齐旧 PE 的判定线宽度语义；
///   仓库内没有更精确的来源说明，改动它会整体改变所有 PEC 谱面的线宽，需谨慎。
///
/// `height` 由速度事件积分得到；接着用同一个 `height` 动画按音符时间读出各自的高度
/// （Hold 用结束时刻的高度作为 `end_height`），使音符贴合判定线的累计位移——
/// 这一步必须发生在事件归一化之后、构造 [`JudgeLine`] 之前。
///
/// # Arguments
/// * `pec` - 已读完的判定线中间表示；
/// * `id` - 判定线编号，用于错误定位与重叠告警；
/// * `max_time` - 谱面结束时间（秒），用于给速度积分补尾。
fn parse_judge_line(mut pec: PECJudgeLine, id: usize, max_time: f64) -> Result<JudgeLine> {
    let mut height = parse_speed_events(pec.speed_events, max_time);
    let mut process_notes = |notes: &mut Vec<Note>| {
        for note in notes {
            height.set_time(note.time);
            note.height = height.now() as f64;
            if let NoteKind::Hold { end_time, end_height } = &mut note.kind {
                height.set_time(*end_time);
                *end_height = height.now() as f64;
            }
        }
    };
    // 归一化位移：x 以 2048（画布宽）为满量程、y 以 1400（画布高）为满量程，
    // 线性映射到项目的 [-1, 1]（原点在屏幕中心）。
    pec.move_events.0.iter_mut().for_each(|it| it.end = it.end / 2048. * 2. - 1.);
    pec.move_events.1.iter_mut().for_each(|it| it.end = it.end / 1400. * 2. - 1.);
    // 归一化不透明度：只有 >= 0 的值才按 255 满量程换算；
    // 负值必须原样保留——PE 用负 alpha 编码特殊渲染指令（隐藏、仅画上方、提前出现等），
    // 这些语义由 ChartSettings::pe_alpha_extension 在渲染层专门解析。
    pec.alpha_events.iter_mut().for_each(|it| {
        if it.end >= 0.0 {
            it.end /= 255.;
        }
    });
    process_notes(&mut pec.notes);
    let cache = JudgeLineCache::new(&mut pec.notes);
    Ok(JudgeLine {
        object: Object {
            alpha: parse_events(pec.alpha_events, id, "alpha")?,
            translation: AnimVector(parse_events(pec.move_events.0, id, "move X")?, parse_events(pec.move_events.1, id, "move Y")?),
            rotation: parse_events(pec.rotate_events, id, "rotate")?,
            // PEC 无判定线缩放事件：x 取固定系数（见函数文档），y 用 AnimFloat::default()
            // 表示“未定义”，渲染时的缩放回退逻辑会取默认值 1。
            scale: AnimVector(AnimFloat::fixed(3.91 / 6.), AnimFloat::default()),
        },
        ctrl_obj: RefCell::default(),
        kind: JudgeLineKind::Normal,
        height,
        incline: AnimFloat::default(),
        notes: pec.notes,
        color: Anim::default(),
        parent: None,
        rot_with_parent: false,
        z_index: 0,
        show_below: false,
        attach_ui: None,

        cache,
    })
}

/// 解析 PEC 纯文本谱面。
///
/// # Arguments
/// * `source` - PEC 文件的全部文本；
/// * `extra` - 外部的 extra.json 扩展（PEC 格式本身不含特效/视频）。
///
/// # Returns
/// 与其它格式一致的 [`Chart`]。PEC 没有自定义打击音，`hitsounds` 传空表。
///
/// # Errors
/// 未知命令、参数缺失/类型错误、首行偏移缺失、区间事件缺少前置值、
/// 或在插入音符之前遇到 `#`/`&` 等语法/语义错误时返回错误，并附带行号。
///
/// 关于 [`ChartSettings::pe_alpha_extension`]：PE 允许把 alpha 写成负数，
/// 用负值编码“不绘制 / 只画线上方 / 提前出现”等特殊渲染指令（详见 `line.rs` 的渲染分支）。
/// 这类语义只在 PEC 谱面里出现，因此这里显式打开该开关；其它格式保持默认关闭。
pub fn parse_pec(source: &str, extra: ChartExtra) -> Result<Chart> {
    // 解析期的跨行状态：
    // - offset 只从第一行取一次；
    // - r（BPM 表）在首次需要做时间换算时才由 bpm_list 惰性构造（因为 bp 命令可能
    //   出现在真正用到时间的命令之前或之后）；
    // - last_line 记录“最近一条被写入音符的判定线”，供行尾的 #/& 修改该音符。
    let mut offset = None;
    let mut r = None;
    let mut lines = Vec::new();
    let mut bpm_list = Vec::new();
    let mut last_line = None;
    // 按线号取判定线，必要时把 lines 扩容到该下标（PEC 不保证线号连续或升序出现）。
    fn get_line(lines: &mut Vec<PECJudgeLine>, id: usize) -> &mut PECJudgeLine {
        if lines.len() <= id {
            lines.reserve(id - lines.len() + 1);
            for _ in 0..=(id - lines.len()) {
                lines.push(PECJudgeLine::default());
            }
        }
        &mut lines[id]
    }
    // 惰性构造 BPM 表：第一条需要时间换算的命令到达时，把已收集的 bp 列表整体转成 BpmList
    // （std::mem::take 把列表搬空，保证之后不会再被追加——逻辑上也确实不该再有 bp）。
    fn ensure_bpm<'a>(r: &'a mut Option<BpmList>, bpm_list: &mut Vec<(f64, f64)>) -> &'a mut BpmList {
        if r.is_none() {
            *r = Some(BpmList::new(std::mem::take(bpm_list)));
        }
        r.as_mut().unwrap()
    }
    // 取（必要时构造）BPM 表的简写，避免每处都写 ensure_bpm(&mut r, &mut bpm_list)。
    macro_rules! bpm {
        () => {
            ensure_bpm(&mut r, &mut bpm_list)
        };
    }
    // 取“最近一条已插入的音符”，供行尾的 #/& 与独立的 #/& 行使用。
    // 若在插入任何音符之前使用则报错——此时没有可修改的目标。
    macro_rules! last_note {
        () => {{
            let Some(last_line) = last_line else {
                ptl!(bail "no-notes-inserted");
            };
            lines[last_line].notes.last_mut().unwrap()
        }};
    }
    // 单行解析器：按空白分词后依据命令首字符分派；首行的 offset 单独在第一个分支处理。
    let mut inner = |line: &str| -> Result<()> {
        let mut it = line.split_whitespace();
        // 首行只有一个偏移值（毫秒）；再减 0.15 秒是历史固定补偿量（仓库内无来源说明，
        // 改动它会整体平移所有 PEC 谱面的判定时机，需谨慎）。
        if offset.is_none() {
            offset = Some(it.take_f32()? / 1000. - 0.15);
        } else {
            let Some(cmd) = it.next() else {
                return Ok(());
            };
            let cs: Vec<_> = cmd.chars().collect();
            if cs.len() > 2 {
                ptl!(bail "unknown-command", "cmd" => cmd);
            }
            match cs[0] {
                'b' if cmd == "bp" => {
                    if r.is_some() {
                        ptl!(bail "bp-error");
                    }
                    bpm_list.push((it.take_f32()? as f64, it.take_f32()? as f64));
                }
                'n' if cs.len() == 2 && ('1'..='4').contains(&cs[1]) => {
                    let r = bpm!();
                    let line = it.take_usize()?;
                    last_line = Some(line);
                    let line = get_line(&mut lines, line);
                    let time = it.take_time(r)?;
                    let kind = match cs[1] {
                        '1' => NoteKind::Click,
                        '2' => NoteKind::Hold {
                            end_time: it.take_time(r)?,
                            end_height: 0.0,
                        },
                        '3' => NoteKind::Flick,
                        '4' => NoteKind::Drag,
                        _ => unreachable!(),
                    };
                    // 音符横坐标：除以 1024（画布半宽）得到 [-1, 1] 的归一化偏移。
                    let position_x = it.take_f32()? / 1024.;
                    // TODO we don't understand..
                    // 再下一个标记表示音符位于判定线上方（1 = 上方，0 = 下方）。
                    let above = it.take_usize()? == 1;
                    // 最后一个标记是假音符（不参与判定）；PEC 只允许 0/1，其它值视为格式错误。
                    let fake = match it.take_usize()? {
                        0 => false,
                        1 => true,
                        _ => ptl!(bail "expected-01"),
                    };
                    // PEC 不提供自定义打击音，只能按音符类型取默认音效。
                    let hitsound = HitSound::default_from_kind(&kind);
                    line.notes.push(Note {
                        object: Object {
                            translation: AnimVector(AnimFloat::fixed(position_x), AnimFloat::default()),
                            ..Default::default()
                        },
                        kind,
                        hitsound,
                        time,
                        height: 0.0,
                        speed: 1.0,

                        above,
                        multiple_hint: false,
                        fake,
                        judge: JudgeStatus::NotJudged,
                        color: WHITE,
                        fx_color: None,
                        judge_area: 1.,
                    });
                    // 音符行尾可选地再跟 `# <速度>` 与 `& <宽度>`，
                    // 语法与独立成行时相同，但作用于刚插入的这条音符。
                    if it.next() == Some("#") {
                        last_note!().speed = it.take_f32()? as f64;
                    }
                    if it.next() == Some("&") {
                        let note = last_note!();
                        let size = it.take_f32()?;
                        if (size - 1.0).abs() >= EPS as f32 {
                            note.object.scale.0 = AnimFloat::fixed(size);
                        }
                    }
                }
                // 独立成行的 `#`：改写最近一条音符的显示速度（与行尾 `#` 同义）。
                '#' if cs.len() == 1 => {
                    last_note!().speed = it.take_f32()? as f64;
                }
                // 独立成行的 `&`：改写最近一条音符的宽度；1.0 视为默认值故不生成动画。
                '&' if cs.len() == 1 => {
                    let note = last_note!();
                    let size = it.take_f32()?;
                    if (size - 1.0).abs() >= EPS as f32 {
                        note.object.scale.0 = AnimFloat::fixed(size);
                    }
                }
                // c* 命令：第二字符决定事件类型——v=速度(瞬时)、p=位移(瞬时)、d=旋转(瞬时)、
                // a=不透明度(瞬时)、m=位移(区间)、r=旋转(区间)、f=不透明度(区间)。
                // 两个约定：旋转事件统一取负（PEC 的旋转正方向与项目相反）；
                // cv 的速度值还需除以 5.85 换算到项目的速度量纲。
                'c' if cs.len() == 2 => {
                    let r = bpm!();
                    let line = get_line(&mut lines, it.take_usize()?);
                    let time = it.take_time(r)?;
                    match cs[1] {
                        'v' => {
                            line.speed_events.push((time, it.take_f32()? / 5.85));
                        }
                        'p' => {
                            let x = it.take_f32()?;
                            let y = it.take_f32()?;
                            line.move_events.0.push(PECEvent::single(time, x));
                            line.move_events.1.push(PECEvent::single(time, y));
                        }
                        'd' => {
                            line.rotate_events.push(PECEvent::single(time, -it.take_f32()?));
                        }
                        'a' => {
                            line.alpha_events.push(PECEvent::single(time, it.take_f32()?));
                        }
                        'm' => {
                            let end_time = it.take_time(r)?;
                            let x = it.take_f32()?;
                            let y = it.take_f32()?;
                            let t = it.take_tween()?;
                            line.move_events.0.push(PECEvent::new(time, end_time, x, t));
                            line.move_events.1.push(PECEvent::new(time, end_time, y, t));
                        }
                        'r' => {
                            line.rotate_events
                                .push(PECEvent::new(time, it.take_time(r)?, -it.take_f32()?, it.take_tween()?));
                        }
                        'f' => {
                            line.alpha_events.push(PECEvent::new(time, it.take_time(r)?, it.take_f32()?, 2));
                        }
                        _ => ptl!(bail "unknown-command", "cmd" => cmd),
                    }
                }
                _ => ptl!(bail "unknown-command", "cmd" => cmd),
            }
        }
        if let Some(next) = it.next() {
            ptl!(bail "unexpected-extra", "next" => next);
        }
        Ok(())
    };
    // 主循环：逐行喂给 inner；出错时补上从 1 开始的行号，方便直接定位文本行。
    for (id, line) in source.lines().enumerate() {
        inner(line).with_context(|| ptl!("line-location", "lid" => id + 1))?;
    }
    // 计算谱面结束时间：取所有事件终点、速度事件时间与音符时间的最大值，再加 1 秒。
    // 这个 +1 秒是补尾用的余量，保证最后一个事件/音符之后动画仍被关键帧覆盖。
    let max_time = *lines
        .iter()
        .map(|it| {
            it.alpha_events
                .iter()
                .chain(it.rotate_events.iter())
                .chain(it.move_events.0.iter())
                .chain(it.move_events.1.iter())
                .map(|it| it.end_time.not_nan())
                .chain(it.speed_events.iter().map(|it| it.0.not_nan()))
                .chain(it.notes.iter().map(|it| it.time.not_nan()))
                .max()
                .unwrap_or_default()
        })
        .max()
        .unwrap_or_default()
        + 1.;
    // 逐条把中间表示转成最终判定线：归一化坐标/透明度、把事件关键帧化、积分高度。
    let mut lines = lines
        .into_iter()
        .enumerate()
        .map(|(id, line)| parse_judge_line(line, id, max_time).with_context(|| ptl!("judge-line-location", "jlid" => id)))
        .collect::<Result<Vec<_>>>()?;
    // 统一收尾：音符按时间排序 + 标记多押提示（四种格式共用，见 process_lines）。
    process_lines(&mut lines);
    // 兜底：若整份谱面从未触发过时间换算（例如只有 bp/首行、没有任何事件或音符），
    // 惰性构造就不会发生，r 仍是 None；这里统一构造一次，保证 Chart::new 拿到 Some。
    ensure_bpm(&mut r, &mut bpm_list);
    Ok(Chart::new(
        offset.unwrap(),
        lines,
        r.unwrap(),
        ChartSettings {
            pe_alpha_extension: true,
            ..Default::default()
        },
        extra,
        HashMap::new(),
    ))
}
