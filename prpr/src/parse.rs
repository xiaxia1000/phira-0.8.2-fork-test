//! Chart parsers
//!
//! 本模块是四种谱面格式解析器的公共入口与共享设施。Phira 支持四种谱面来源：
//!
//! - **PEC**（`parse_pec`）：旧版 Phigros/PE 的纯文本格式；
//! - **PGR**（`parse_phigros`）：Phigros 官方导出的 JSON；
//! - **RPE**（`parse_rpe`）：社区编辑器 Re:PhiEdit 导出的 JSON；
//! - **extra.json**（`parse_extra`）：Phira 私有的谱面扩展（特效/视频），前三者都没有。
//!
//! 各解析器只负责把源格式翻译成与格式无关的 [`crate::core::Chart`]；
//! 音符排序、多押标记、缓动编号映射等**四种格式共有**的收尾工作集中放在本模块，
//! 避免四个解析器各写一份导致行为漂移。
prpr_l10n::tl_file!("parser" ptl);

mod extra;
pub use extra::parse_extra;

mod pec;
pub use pec::parse_pec;

mod pgr;
pub use pgr::parse_phigros;

mod rpe;
pub use rpe::{lint, parse_rpe, RPE_HEIGHT, RPE_WIDTH};

/// 解析前预扫描（`lint`）得到的格式特性报告，用于在正式解析之前提示用户。
///
/// 正式解析 RPE 需要加载贴图/音频，代价高；而是否需要提示用户往往只取决于
/// 少数几个字段，所以先跑一遍轻量的 [`crate::parse::lint`] 把结论传出来。
#[derive(Debug, Default)]
pub struct ParseWarnings {
    /// 谱面是否含有 `easingType > 1` 的速度事件（RPE 的新式速度缓动）。
    /// 这类事件在 RPE 1.7.0 前后语义不同，需要按版本或用户开关选择积分方式。
    pub has_new_speed_events: bool,
    /// 谱面是否有判定线使用了 `attachUI`（把 UI 元素挂到判定线上）。
    /// 该特性依赖较新的渲染路径，需要提示用户部分环境可能显示异常。
    pub has_attach_ui: bool,
}

/// 对全部判定线做两件四种格式共通的收尾工作：**音符按时间升序排列** + **标记多押提示**。
///
/// 为什么要在这里统一做：四种格式给出的原始音符顺序都不保证有序——
/// PEC 把音符按“出现顺序”追加到所属判定线，PGR 拼接 `notesAbove`/`notesBelow` 后
/// 同拍也可能乱序，RPE 的事件层/音符数组更是任意排列；而后续的判定、渲染
/// （判定线缓存的二分查找、Hold 的绘制、音符前后关系）都假定 `notes` 按 `time`
/// 单调不减。因此统一在这里排一次，而不是让每个解析器各排一次。
///
/// `multiple_hint` 的规则：某条判定线上若有多个音符落在**完全相同的时间**，
/// 这些音符被标记为多押提示。判定只在单条判定线内部、按时间严格相等进行，
/// 不做跨判定线的多押推断；渲染层据此把多押音符画得更醒目（提示玩家一起按下）。
///
/// # Arguments
/// * `v` - 判定线切片，就地排序并就地写入 `multiple_hint`。
///
/// 内部按三步执行（见函数体内的分段注释）：先求每线的排序下标，再汇总全局多押
/// 时刻集合，最后回填标记。
pub(crate) fn process_lines(v: &mut [crate::core::JudgeLine]) {
    use crate::ext::NotNanExt;
    // 阶段 1：为每条判定线算出“按 time 升序排列”的下标序列。之所以存下标而不是
    // 直接排序，是因为后续每一步都要按同一顺序访问音符，且需要保留原地址来写字段。
    let mut times = Vec::new();
    // TODO optimize using k-merge sort
    let sorts = v
        .iter()
        .map(|line| {
            let mut idx: Vec<usize> = (0..line.notes.len()).collect();
            idx.sort_by_key(|id| line.notes[*id].time.not_nan());
            idx
        })
        .collect::<Vec<_>>();
    for (line, idx) in v.iter_mut().zip(sorts.iter()) {
        let v = &mut line.notes;
        let mut i = 0;
        // 以“时间完全相同的一段”为单位推进：j 指向下一个时间不同的音符。
        // 一段里有 2 个及以上音符时，把该时间额外 push 一次，
        // 于是这一轮里“多押时刻”在 times 中出现两次，非多押时刻只出现一次。
        while i < v.len() {
            times.push(v[idx[i]].time.not_nan());
            let mut j = i + 1;
            while j < v.len() && v[idx[j]].time == v[idx[i]].time {
                j += 1;
            }
            if j != i + 1 {
                times.push(v[idx[i]].time.not_nan());
            }
            i = j;
        }
    }
    times.sort();
    // 阶段 2：排序后多押时刻会形成相邻相等的“对”，用前一个元素不相等来去重，
    // 收集成全局多押时刻表 mt（升序），供下一步 O(n) 归并匹配。
    let mut mt = Vec::new();
    if !times.is_empty() {
        for i in 0..(times.len() - 1) {
            // since times are generated in the same way, theoretically we can compare them directly
            if times[i] == times[i + 1] && (i == 0 || times[i - 1] != times[i]) {
                mt.push(*times[i]);
            }
        }
    }
    // 阶段 3：按时间归并回填。`i` 只随音符时间单调前进，因此整批回填是 O(n)。
    for (line, idx) in v.iter_mut().zip(sorts.iter()) {
        let mut i = 0;
        for id in idx {
            let note = &mut line.notes[*id];
            let time = note.time;
            while i < mt.len() && mt[i] < time {
                i += 1;
            }
            if i < mt.len() && mt[i] == time {
                note.multiple_hint = true;
            }
        }
    }
}

/// RPE 的缓动**数字编号** → 本项目 [`crate::core::TweenId`] 的映射表（共 30 项）。
///
/// 为什么需要这张表：RPE 的事件只用数字编号（`easingType` / `easing`）表示缓动曲线，
/// 且编号成对出现（一条曲线的 in/out 各占一个编号），而本项目的 `TweenId` 用的是
/// `major * 3 + minor` 的编码（见 [`crate::core::easing_from`]，`major` 占 3 个槽位）。
/// 两套编号的排列顺序不同源，既不能直接相加也不能取模换算，只能显式列出来。
///
/// **约定：数组下标即 RPE 编号**，`RPE_TWEEN_MAP[n]` 就是 RPE 编号 `n` 对应的曲线。
/// 每对里前一项为 Out、后一项为 In（如下标 2/3 是 Sine Out/Sine In）；
/// 下标 0 与 1 都被定义为线性（对应 RPE 用两个编号表示同一条直线），配合调用处
/// 常见的 `.max(1)`，RPE 的 0/1 都退化为线性。
///
/// 所有使用方一律走 `.get(n).copied().unwrap_or(RPE_TWEEN_MAP[0])`，
/// 使越界或未知编号退回线性，保证解析不会因为一个陌生编号而失败。
#[rustfmt::skip]
pub const RPE_TWEEN_MAP: [crate::core::TweenId; 30] = {
    use crate::core::{easing_from as e, TweenMajor::*, TweenMinor::*};
    [
        2, 2, // linear
        e(Sine, Out), e(Sine, In),
        e(Quad, Out), e(Quad, In),
        e(Sine, InOut), e(Quad, InOut),
        e(Cubic, Out), e(Cubic, In),
        e(Quart, Out), e(Quart, In),
        e(Cubic, InOut), e(Quart, InOut),
        e(Quint, Out), e(Quint, In),
        e(Expo, Out), e(Expo, In),
        e(Circ, Out), e(Circ, In),
        e(Back, Out), e(Back, In),
        e(Circ, InOut), e(Back, InOut),
        e(Elastic, Out), e(Elastic, In),
        e(Bounce, Out), e(Bounce, In),
        e(Bounce, InOut), e(Elastic, InOut),
    ]
};
