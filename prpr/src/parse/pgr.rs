//! PGR 解析器：**Phigros 官方导出**的 JSON 谱面格式。
//!
//! 与 PEC 的纯文本不同，PGR 是官方格式：字段名、单位与语义由官方定义，
//! 顶层 `formatVersion` 决定部分字段的解释方式。本模块区分两个版本：
//!
//! - `formatVersion == 1`：位移事件把 x、y **打包进一个数值**
//!   （整数部分为一维、小数部分为另一维），需要拆解后换算，见 `parse_move_events_fv1`；
//! - `formatVersion == 3`：位移事件改用 `start`/`end` 表示一维、`start2`/`end2` 表示另一维；
//! - 其它版本直接报错，避免用错误语义把新谱面解析成看似正常的错谱。
//!
//! 官方格式缺失的信息需要从外部补：
//! - **hitsound**：官方只有“按音符类型的默认打击音”，因此一律用
//!   [`HitSound::default_from_kind`]，`HitSoundMap` 传空表；
//! - **extra**：官方格式不含特效/视频，`extra` 由调用方从 `extra.json` 单独解析后传入。
//!
//! 时间单位：官方谱面的所有时间字段都以“拍”为单位（且每条判定线自带 `bpm`），
//! 因此本模块不走顶层 BPM 表，而是按判定线自己的 bpm 直接换算，见 `parse_judge_line`。
use anyhow::{Context, Result};
use macroquad::color::WHITE;
use serde::Deserialize;
use std::{cell::RefCell, collections::HashMap};
use tracing::warn;

use super::{process_lines, L10N_LOCAL};
use crate::{
    core::{
        Anim, AnimFloat, AnimVector, BpmList, Chart, ChartExtra, ChartSettings, JudgeLine, JudgeLineCache, JudgeLineKind, Keyframe, Note, NoteKind,
        Object, HEIGHT_RATIO,
    },
    ext::NotNanExt,
    judge::{HitSound, JudgeStatus},
};

/// PGR 的通用“双端事件”，被 alpha / 旋转 / 位移三类事件共用。
///
/// 字段名与官方 JSON 一致（camelCase，如 `startTime`/`endTime`/`start`/`end`）。
/// `start2`/`end2` 之所以给默认值，是因为 formatVersion 1 的位移事件只有一个打包数值，
/// JSON 里不存在第二维。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PgrEvent {
    /// 事件起点时间，单位**拍**，JSON 键 `startTime`。
    pub start_time: f64,
    /// 事件终点时间，单位**拍**，JSON 键 `endTime`。
    pub end_time: f64,
    /// 起点的第一维值（含义随事件类型：alpha / 旋转角 / 位移，见调用处），JSON 键 `start`。
    pub start: f32,
    /// 终点的第一维值，JSON 键 `end`。
    pub end: f32,
    /// 起点的第二维值（仅位移事件使用），JSON 键 `start2`，缺省 0。
    #[serde(default)]
    pub start2: f32,
    /// 终点的第二维值，JSON 键 `end2`，缺省 0。
    #[serde(default)]
    pub end2: f32,
}

/// PGR 的速度事件（`speedEvents`）。
///
/// 与其它事件不同，它只有一个 `value`（速率），没有 start/end 两个值：
/// 官方语义是“在 [startTime, endTime) 内以 value 速率匀速移动”，
/// 因此必须被积分成判定线 `height` 才能使用（见 `parse_speed_events`）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PgrSpeedEvent {
    /// 速率生效起点，单位**拍**，JSON 键 `startTime`。
    pub start_time: f64,
    /// 速率生效终点，单位**拍**，JSON 键 `endTime`。
    pub end_time: f64,
    /// 该区间内的移动速率，JSON 键 `value`；量纲是官方自定义的速度系数，不是像素。
    pub value: f32,
}

/// PGR 的单个音符（`notesAbove` / `notesBelow` 数组的元素）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgrNote {
    /// 音符类型，JSON 键 `type`：1=Click、2=Drag、3=Hold、4=Flick。
    /// **注意**：与 PEC 的 `n1..n4` 编号不同（PEC 里 2 是 Hold、3 是 Flick），
    /// 跨格式改造时不能直接复用编号。
    #[serde(rename = "type")]
    kind: u8,
    /// 命中判定时间，单位**拍**，JSON 键 `time`。
    time: f64,
    /// 横向位置，JSON 键 `positionX`；官方量纲，需乘 `2 * 9 / 160` 归一化（见 `parse_notes`）。
    position_x: f32,
    /// Hold 的持续**拍数**，JSON 键 `holdTime`；非 Hold 音符忽略。
    hold_time: f64,
    /// 音符相对判定线的移动速度倍率，JSON 键 `speed`。
    speed: f32,
    /// 音符所在“楼层”位置，JSON 键 `floorPosition`；官方用于纵向排版，
    /// 本项目改用判定线 height 积分推导纵向位置，故该字段未使用。
    #[allow(unused)]
    floor_position: f32,
}

/// PGR 的一条判定线。
///
/// 键名不完全符合 camelCase 转换规律：`bpm` 是纯小写，而事件字段带
/// `judgeLine` 前缀（`judgeLineDisappearEvents` 等），因此对这些键逐个显式 `rename`。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PgrJudgeLine {
    /// 该判定线自身的 BPM，JSON 键 `bpm`；用于把拍换算成秒（见 `parse_judge_line`）。
    /// 官方把 BPM 放在判定线上而不是谱面级，因此顶层不需要 BPM 表。
    bpm: f64,
    /// 不透明度事件，JSON 键 `judgeLineDisappearEvents`。
    #[serde(rename = "judgeLineDisappearEvents")]
    alpha_events: Vec<PgrEvent>,
    /// 旋转事件，JSON 键 `judgeLineRotateEvents`。
    #[serde(rename = "judgeLineRotateEvents")]
    rotate_events: Vec<PgrEvent>,
    /// 位移事件，JSON 键 `judgeLineMoveEvents`。
    #[serde(rename = "judgeLineMoveEvents")]
    move_events: Vec<PgrEvent>,
    /// 速度事件（决定判定线 height），JSON 键 `speedEvents`。
    speed_events: Vec<PgrSpeedEvent>,

    /// 判定线上方的音符，JSON 键 `notesAbove`。
    notes_above: Vec<PgrNote>,
    /// 判定线下方的音符，JSON 键 `notesBelow`。
    notes_below: Vec<PgrNote>,
}

/// PGR 的顶层结构（官方谱面 JSON）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PgrChart {
    /// 格式版本，JSON 键 `formatVersion`；本解析器只支持 1 与 3（见 `parse_judge_line`）。
    format_version: u32,
    /// 全局谱面偏移，单位**秒**，JSON 键 `offset`；直接作为 [`Chart::offset`]（无需换算）。
    offset: f32,
    /// 判定线列表，JSON 键 `judgeLineList`。
    judge_line_list: Vec<PgrJudgeLine>,
}

/// 过滤掉时间区间非法的 PGR 事件（`start_time > end_time`）。
///
/// 官方谱面偶尔出现这种脏数据；若原样保留会让关键帧时间倒序，进而使
/// [`crate::core::Anim`] 的游标查找失效。这里直接丢弃并告警，让解析继续，
/// 而不是因为个别事件把整份谱面判为失败。
macro_rules! validate_events {
    ($pgr:expr) => {
        $pgr.retain(|it| {
            if it.start_time > it.end_time {
                warn!("invalid time range, ignoring");
                false
            } else {
                true
            }
        });
    };
}

/// 解析 PGR 速度事件，返回 `(速度曲线, height 曲线)`。
///
/// 返回两个动画：前者是原始速度值（缓动写 0，供音符显示速度/调试参考），
/// 后者是**把速度对时间积分**得到的累计位移 `height`——官方谱面里判定线的纵向位置
/// 正是由速度事件累加出来的，所以必须积分后才能与 [`crate::core::JudgeLine::height`]
/// 的语义衔接。积分按逐段“速率 × 时长”累加；最后除以 [`HEIGHT_RATIO`]
/// 把官方高度单位换算成项目的归一化高度。
///
/// # Arguments
/// * `r` - 拍→秒的换算系数（该判定线的 `60 / 32 / bpm`）；
/// * `pgr` - 速度事件列表；
/// * `max_time` - 谱面结束时间（秒），用于补末尾关键帧，保证结束后高度不再外推。
///
/// 细节：若首个事件起点不为 0，会把它夹到 0，避免时间轴开头没有速度定义。
fn parse_speed_events(r: f64, mut pgr: Vec<PgrSpeedEvent>, max_time: f64) -> Result<(AnimFloat, AnimFloat)> {
    validate_events!(pgr);
    //assert_eq!(pgr[0].start_time, 0.0);
    if pgr[0].start_time != 0. {
        pgr[0].start_time = 0.
    }
    let mut kfs = Vec::new();
    let mut pos = 0.;
    kfs.extend(pgr[..pgr.len().saturating_sub(1)].iter().map(|it| {
        let from_pos = pos;
        pos += ((it.end_time - it.start_time) * r) as f32 * it.value;
        Keyframe::new(it.start_time * r, from_pos, 2)
    }));
    let last = pgr.last().unwrap();
    kfs.push(Keyframe::new(last.start_time * r, pos, 2));
    kfs.push(Keyframe::new(max_time, pos + (max_time - last.start_time * r) as f32 * last.value, 0));
    for kf in &mut kfs {
        kf.value /= HEIGHT_RATIO as f32;
    }
    Ok((AnimFloat::new(pgr.iter().map(|it| Keyframe::new(it.start_time * r, it.value, 0)).collect()), AnimFloat::new(kfs)))
}

/// 解析单值事件（alpha / 旋转），生成线性插值的 [`AnimFloat`]。
///
/// 构造方式是把每个事件的起点值、终点值依次压入关键帧：只有当前值与上一关键帧
/// 不同才补起点帧（去重，避免零长度区间），缓动固定为 2（线性），因为官方事件
/// 隐含“区间内线性过渡”。起点时间会夹到 >= 0（官方偶有负值）。
///
/// 末尾 `pop()` 掉最后一个关键帧：它是最后一个事件的终点，靠后面的收尾逻辑补帧，
/// 留着会在时间轴末尾多出一个孤立点。
fn parse_float_events(r: f64, mut pgr: Vec<PgrEvent>) -> Result<AnimFloat> {
    validate_events!(pgr);
    let mut kfs = Vec::<Keyframe<f32>>::new();
    for e in pgr {
        if !kfs.last().is_some_and(|it| it.value == e.start) {
            kfs.push(Keyframe::new((e.start_time * r).max(0.), e.start, 2));
        }
        kfs.push(Keyframe::new(e.end_time * r, e.end, 2));
    }
    kfs.pop();
    Ok(AnimFloat::new(kfs))
}

/// 解析 formatVersion 3 的位移事件：第一维取 `start`/`end`、第二维取 `start2`/`end2`。
///
/// 结构与 [`parse_float_events`] 同构，但为两维各维护一条关键帧序列。
/// 最后把值从官方量纲映射到项目的 [-1, 1]（`-1 + v * 2`），
/// 使屏幕中心对应 0、官方 0..=1 对应 -1..=1。
fn parse_move_events(r: f64, mut pgr: Vec<PgrEvent>) -> Result<AnimVector> {
    validate_events!(pgr);
    let mut kf1 = Vec::<Keyframe<f32>>::new();
    let mut kf2 = Vec::<Keyframe<f32>>::new();
    for e in pgr {
        let st = (e.start_time * r).max(0.);
        let en = e.end_time * r;
        if !kf1.last().is_some_and(|it| it.value == e.start) {
            kf1.push(Keyframe::new(st, e.start, 2));
        }
        if !kf2.last().is_some_and(|it| it.value == e.start2) {
            kf2.push(Keyframe::new(st, e.start2, 2));
        }
        kf1.push(Keyframe::new(en, e.end, 2));
        kf2.push(Keyframe::new(en, e.end2, 2));
    }
    kf1.pop();
    kf2.pop();
    for kf in &mut kf1 {
        kf.value = -1. + kf.value * 2.;
    }
    for kf in &mut kf2 {
        kf.value = -1. + kf.value * 2.;
    }
    Ok(AnimVector(AnimFloat::new(kf1), AnimFloat::new(kf2)))
}

/// 解析 formatVersion 1 的位移事件：官方把两维打包进一个数值。
///
/// 打包规则是“整数部分 = 一维（以 1000 为进位），余数部分 = 另一维”，
/// 因此用 `(v - v % 1000) / 1000` 取整数部分、`v % 1000` 取余数部分拆开。
/// 归一化与 fv3 不同：两维分别以 880 / 520 为满量程（官方 fv1 的横向/纵向量纲），
/// 例如 `(-880 + v * 2) / 880` 就把该量程线性映射到 [-1, 1]。
///
/// fv1 与 fv3 必须分成两个函数：两者的事件取值语义不兼容，
/// 用错版本会把位移整体算错（因此调用方按 `format_version` 显式分支）。
fn parse_move_events_fv1(r: f64, mut pgr: Vec<PgrEvent>) -> Result<AnimVector> {
    validate_events!(pgr);
    let mut kf1 = Vec::<Keyframe<f32>>::new();
    let mut kf2 = Vec::<Keyframe<f32>>::new();
    for e in pgr {
        let st = (e.start_time * r).max(0.);
        let en = e.end_time * r;
        if !kf1.last().is_some_and(|it| it.value == e.start) {
            let start = (e.start - e.start % 1000.) / 1000.;
            kf1.push(Keyframe::new(st, start, 2));
        }
        if !kf2.last().is_some_and(|it| it.value == e.start2) {
            let start2 = e.start % 1000.;
            kf2.push(Keyframe::new(st, start2, 2));
        }
        let end = (e.end - e.end % 1000.) / 1000.;
        let end2 = e.end % 1000.;
        kf1.push(Keyframe::new(en, end, 2));
        kf2.push(Keyframe::new(en, end2, 2));
    }
    kf1.pop();
    kf2.pop();
    for kf in &mut kf1 {
        kf.value = (-880. + kf.value * 2.) / 880.;
    }
    for kf in &mut kf2 {
        kf.value = (-520. + kf.value * 2.) / 520.;
    }
    Ok(AnimVector(AnimFloat::new(kf1), AnimFloat::new(kf2)))
}

/// 解析一批 PGR 音符，并为每个音符读出所在高度。
///
/// # Arguments
/// * `r` - 拍→秒系数；
/// * `pgr` - 音符数组（上方、下方各调用一次）；
/// * `_speed` - 判定线的速度曲线，当前未使用（保留参数）；
/// * `height` - 判定线 height 曲线，用于取音符所在时间点的高度；
/// * `above` - 该批音符是否在判定线上方，直接写入 `Note::above`。
///
/// 换算与语义：
/// - 横向位置乘 `2 * 9 / 160`（约 0.1125）：官方 `positionX` 的量纲就是按此比例
///   折算到项目的归一化横坐标（等价于把 ±8.89 映射到 ±1）；
/// - Hold 的 `end_height` 取结束时刻的高度，使长条随判定线移动而正确延伸；
/// - Hold 的显示速度被强制为 1.0，因为官方 Hold 的速度由 `holdTime` 决定、
///   `speed` 字段对它无效。
///
/// # Errors
/// 音符类型不在 1..=4 内时返回错误。
fn parse_notes(r: f64, mut pgr: Vec<PgrNote>, _speed: &mut AnimFloat, height: &mut AnimFloat, above: bool) -> Result<Vec<Note>> {
    // is_sorted is unstable...
    if pgr.is_empty() {
        return Ok(Vec::new());
    }
    pgr.sort_by_key(|it| it.time.not_nan());
    pgr.into_iter()
        .map(|pgr| {
            let time = pgr.time * r;
            let kind = match pgr.kind {
                1 => NoteKind::Click,
                2 => NoteKind::Drag,
                3 => {
                    let end_time = (pgr.time + pgr.hold_time) * r;
                    height.set_time(end_time);
                    NoteKind::Hold {
                        end_time,
                        end_height: height.now() as f64,
                    }
                }
                4 => NoteKind::Flick,
                _ => ptl!(bail "unknown-note-type", "type" => pgr.kind),
            };
            let hitsound = HitSound::default_from_kind(&kind);
            Ok(Note {
                object: Object {
                    translation: AnimVector(AnimFloat::fixed(pgr.position_x * (2. * 9. / 160.)), AnimFloat::default()),
                    ..Default::default()
                },
                kind,
                hitsound,
                time,
                speed: if pgr.kind == 3 { 1. } else { pgr.speed as f64 },
                height: {
                    height.set_time(time);
                    height.now() as f64
                },

                above,
                multiple_hint: false,
                fake: false,
                judge: JudgeStatus::NotJudged,
                color: WHITE,
                fx_color: None,
                judge_area: 1.,
            })
        })
        .collect()
}

/// 把一条官方判定线转成项目的 [`JudgeLine`]。
///
/// `r = 60 / 32 / bpm` 是“拍 → 秒”的换算系数：官方以 1/32 拍为最小时间单位，
/// 即 1 拍 = 32 个官方时间单位，于是 1 个官方时间单位 = `60 / bpm / 32` 秒。
///
/// 位移事件的解释随 `format_version` 分支：
/// - 1 → `parse_move_events_fv1`（打包数值；见该函数）；
/// - 3 → `parse_move_events`（`start2`/`end2` 分开存放）；
/// - 其它 → 报错，绝不猜测。
///
/// 不透明度与旋转两个版本一致，都走 [`parse_float_events`]。
/// 音符由 `notesAbove` / `notesBelow` 分别解析后拼接；`JudgeLineCache` 在拼接后构建。
///
/// # Errors
/// 速度/位移/音符解析失败、或 `format_version` 不属于 {1, 3} 时返回错误。
fn parse_judge_line(pgr: PgrJudgeLine, max_time: f64, format_version: u32) -> Result<JudgeLine> {
    let r = 60. / 32. / pgr.bpm;
    let (mut speed, mut height) = parse_speed_events(r, pgr.speed_events, max_time).context("Failed to parse speed events")?;
    let notes_above = parse_notes(r, pgr.notes_above, &mut speed, &mut height, true).context("Failed to parse notes above")?;
    let mut notes_below = parse_notes(r, pgr.notes_below, &mut speed, &mut height, false).context("Failed to parse notes below")?;
    let mut notes = notes_above;
    notes.append(&mut notes_below);
    let cache = JudgeLineCache::new(&mut notes);
    Ok(JudgeLine {
        object: Object {
            alpha: parse_float_events(r, pgr.alpha_events).with_context(|| ptl!("alpha-events-parse-failed"))?,
            rotation: parse_float_events(r, pgr.rotate_events).with_context(|| ptl!("rotate-events-parse-failed"))?,
            translation: {
                match format_version {
                    1 => parse_move_events_fv1(r, pgr.move_events).with_context(|| ptl!("move-events-parse-failed"))?,
                    3 => parse_move_events(r, pgr.move_events).with_context(|| ptl!("move-events-parse-failed"))?,
                    _ => ptl!(bail "unknown-format-version"),
                }
            },
            ..Default::default()
        },
        ctrl_obj: RefCell::default(),
        kind: JudgeLineKind::Normal,
        height,
        incline: AnimFloat::default(),
        notes,
        color: Anim::default(),
        parent: None,
        rot_with_parent: false,
        z_index: 0,
        show_below: false,
        attach_ui: None,

        cache,
    })
}

/// 解析 Phigros 官方 JSON 谱面。
///
/// # Arguments
/// * `source` - 谱面 JSON 文本；
/// * `extra` - 外部的 extra.json 扩展（官方格式本身不含特效/视频）。
///
/// # Returns
/// 与其它格式一致的 [`Chart`]，其 `offset` 直接取官方的秒值。
///
/// # Errors
/// JSON 反序列化失败、或任一判定线解析失败（附判定线下标）时返回错误。
///
/// 几个“可以不填”的默认值，原因都在官方格式本身的语义：
/// - [`BpmList::default()`]：官方把 BPM 放在**每条判定线**上，本模块用 `r` 直接换算，
///   顶层 BPM 表没有查询入口，留空即可（仅作占位，不参与时间换算）；
/// - [`ChartSettings::default()`]：官方格式没有 PE 的负 alpha 扩展等特殊语义，
///   所有开关保持默认 false；
/// - hitsound 传空 `HitSoundMap`：官方只有类型默认音，没有自定义音效文件引用。
pub fn parse_phigros(source: &str, extra: ChartExtra) -> Result<Chart> {
    // 阶段 1：反序列化整份谱面，取出格式版本。
    let pgr: PgrChart = serde_json::from_str(source).with_context(|| ptl!("json-parse-failed"))?;
    let format_version = pgr.format_version;
    // 阶段 2：扫描所有判定线求最大时间（最后一个音符的拍数换算成秒），再 +1 秒作补尾余量。
    let max_time = *pgr
        .judge_line_list
        .iter()
        .map(|line| {
            (line
                .notes_above
                .iter()
                .chain(line.notes_below.iter())
                .map(|note| note.time.not_nan())
                .max()
                .unwrap_or_default()
                * (60. / line.bpm / 32.))
                .not_nan()
        })
        .max()
        .unwrap_or_default()
        + 1.;
    // 阶段 3：逐条解析判定线（错误信息里带上判定线下标）。
    let mut lines = pgr
        .judge_line_list
        .into_iter()
        .enumerate()
        .map(|(id, pgr)| parse_judge_line(pgr, max_time, format_version).with_context(|| ptl!("judge-line-location", "jlid" => id)))
        .collect::<Result<Vec<_>>>()?;

    // 阶段 4：统一收尾（音符排序 + 多押标记），再组装 Chart。
    process_lines(&mut lines);
    Ok(Chart::new(pgr.offset, lines, BpmList::default(), ChartSettings::default(), extra, HashMap::new()))
}
