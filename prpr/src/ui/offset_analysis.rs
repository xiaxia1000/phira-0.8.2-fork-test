//! 自动 offset（音频偏移）分析面板。
//!
//! 面板让玩家一键估算「谱面音符时间点」与「音乐中真实打击点」之间的整体时间差，
//! 并把估算结果画成一条相关系数曲线供人工判断，最后可以把推荐值写回 `info.offset`。
//!
//! 三块职责：
//! 1. **状态机** [`OffsetAnalysisState`]：空闲 → 计算中 → 完成，界面按状态切换提示与图表；
//! 2. **后台计算**：对齐算法来自 `prpr-auto-offset` 包
//!    （[`SuperFlux`] 提取音频的 onset 包络 + [`WeightedGaussianNote`] 把音符时间点
//!    抹成连续信号 + [`estimate_with`] 做互相关），计算量很大，因此丢进
//!    `std::thread::spawn` 的独立线程跑，避免卡住渲染主循环；
//! 3. **交互回传**：面板本身不改游戏状态，而是通过 [`OffsetPanelAction`]
//!    把「取消 / 重置 / 保存」的意图交回调用方（`GameScene`）决定如何生效。
//!
//! 平台差异：`std::thread` 在 wasm 上不可用，因此 [`OffsetAnalysisPanel::start_analysis`]
//! 在网页端是**空实现**——点击「自动偏移」不会有任何反应（既不计算也不报错），
//! 但状态机不会被切到「计算中」，界面不会卡在转圈状态。

use super::{Scroll, Ui};
use crate::{
    core::{Chart, NoteKind, Resource},
    ext::RectExt,
};
use lyon::math::point;
use macroquad::prelude::*;
use prpr_auto_offset::{estimate_with, AlignConfig, AlignResult, AutoOffsetNoteKind, NoteEvent, SuperFlux, WeightedGaussianNote};
use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

/// Ratio of graph content width to viewport width.
/// The visible viewport shows `o_range / GRAPH_CONTENT_RATIO` seconds of offset data.
/// 取 2.0 意味着内容宽度是可视区的两倍、一次只能看到一半的搜索区间——
/// 这样横向滚动才有意义，同时曲线细节不会被压得太扁。
const GRAPH_CONTENT_RATIO: f32 = 2.0;
/// 纵轴（相关系数得分）的下限刻度。
///
/// 若直接把纵轴上限定为本次得分，低分结果会被竖向拉满、把噪声放大成「很明显的峰」，
/// 误导用户；用一个 0.4 的固定下限，低分曲线看起来就是平的，如实反映可信度低。
const MIN_SCORE_TOP: f32 = 0.4;

// In a random sample of 2000 charts, only 12 charts scored below 0.35,
// about 6 per thousand. The 0.6 and 0.75 lines cover the range where most
// charts land.
/// 参考线：`(相关系数, 显示文本)`。
///
/// 数值来自对 2000 张谱面的随机抽样统计——低于 0.35 的只有 12 张（约千分之六），
/// 因此 0.35 是「几乎不可信」的下限，而 0.6 / 0.75 覆盖了绝大多数谱面的落点区间，
/// 给用户一个「我这次的分数算好还是差」的直观标尺。
const SCORE_REFERENCE_LINES: [(f32, &str); 3] = [(0.35, "0.35"), (0.6, "0.6"), (0.75, "0.75")];

/// 面板的状态机。
///
/// 必须区分「计算中」与「完成」：后台线程的结果不是立刻可用的，
/// 没有这个状态就只能用「结果是否为空」来猜，无法区分「还没算完」与「算出来是空的」。
#[derive(Clone)]
enum OffsetAnalysisState {
    /// 尚未开始，或已被重置。界面显示「点击自动偏移开始分析」的提示。
    Idle,
    /// 后台线程正在计算。界面显示等待提示，并禁用「自动偏移」按钮防止重复提交。
    Computing,
    /// 计算完成，携带完整结果。界面显示相关系数曲线与推荐值。
    Done(AlignResult),
}

/// 面板向调用方（`GameScene`）回传的用户意图。
///
/// 面板刻意不直接改 `info.offset` 或关闭自己，而是把意图交出去：
/// 「保存到哪个字段」「关闭后回到哪个界面」都属于场景层的决策，
/// 放在面板里会把 UI 组件和游戏流程耦合起来。
pub enum OffsetPanelAction {
    /// 用户点了取消：放弃本次调整并关闭面板。
    Cancel,
    /// 用户点了重置：把偏移恢复为打开面板前的值。
    Reset,
    /// 用户点了保存：采用当前偏移值（单位秒）并关闭面板。
    Save(f32),
}

/// 自动偏移分析面板。
pub struct OffsetAnalysisPanel {
    /// 当前状态，决定界面渲染哪一部分。
    state: OffsetAnalysisState,
    /// 「用户请求开始分析」的一次性标志。
    ///
    /// 按钮在 `render` 里被点击，但 `render` 拿不到音频资源（只有 `&Chart`），
    /// 无法就地启动计算；于是先置位，由下一帧的 `update`（它能拿到 `&Resource`）
    /// 消费并真正启动。这是「渲染只负责表达意图、更新负责执行」的典型分工。
    requested: bool,
    /// 与后台线程共享的结果槽位。
    ///
    /// `Arc<Mutex<Option<..>>>` 三个包装各有作用：`Arc` 让主线程与工作线程共同持有；
    /// `Mutex` 保证写入/读取的互斥；外层 `Option` 表示「当前是否有正在等待的线程」——
    /// 取到结果后置回 `None`，于是同一个 `handle` 不会重复消费同一份结果。
    handle: Option<Arc<Mutex<Option<AlignResult>>>>,
    /// 曲线图的横向滚动容器（内容宽度是可视区的 `GRAPH_CONTENT_RATIO` 倍）。
    scroll: Scroll,
    /// 「是否已经做过自动居中」的一次性闩锁。
    /// 结果到达时自动把视图滚到推荐值附近；之后用户手动滚动时不再被强行拽回去。
    scroll_centered: bool,
}

// 实现语义：与 `new()` 一致，仅用于让持有本面板的场景结构体可以 `#[derive(Default)]`。
impl Default for OffsetAnalysisPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl OffsetAnalysisPanel {
    /// 创建一个空闲的面板：没有进行中的计算，曲线图按**水平**方向滚动。
    ///
    /// 水平滚动是必须的：曲线的内容宽度是可视区的 `GRAPH_CONTENT_RATIO` 倍，
    /// 纵向则是固定高度。
    pub fn new() -> Self {
        Self {
            state: OffsetAnalysisState::Idle,
            requested: false,
            handle: None,
            scroll: Scroll::new().horizontal(),
            scroll_centered: false,
        }
    }

    /// 把触摸交给曲线图的滚动容器，返回「是否已被判定为拖动」。
    /// 面板的其它按钮（自动偏移、±、取消/重置/保存）由外层统一分发，
    /// 这里只处理需要滚动的曲线区域。
    pub fn touch(&mut self, touch: &Touch, now: f32) -> bool {
        self.scroll.touch(touch, now)
    }

    /// 每帧推进：消费待启动的请求、轮询后台结果、驱动滚动。
    ///
    /// # Arguments
    /// * `chart` / `res` / `info_offset` — 启动分析所需的输入；
    ///   只有真正发起计算的那一帧才会用到，因此本函数每帧都要拿到它们
    /// * `now` — 当前时间，转发给滚动容器
    ///
    /// 轮询用 `try_lock` 而不是 `lock` 是关键：主线程绝不能为了等后台线程写结果而阻塞，
    /// 锁被占用（说明工作线程正在写入）时直接跳过、下一帧再试即可。
    /// 结果取出后立刻把 `handle` 置为 `None`，避免同一份结果被反复应用。
    pub fn update(&mut self, chart: &Chart, res: &Resource, info_offset: f32, now: f32) {
        if self.requested {
            self.requested = false;
            self.start_analysis(chart, res, info_offset);
        }

        let handle = self.handle.clone();
        if let Some(handle) = handle {
            if let Ok(mut guard) = handle.try_lock() {
                if let Some(result) = guard.take() {
                    self.state = OffsetAnalysisState::Done(result);
                    self.handle = None;
                }
            }
        }

        self.scroll.update(now);
    }

    /// 启动后台对齐计算（非 wasm 平台）。
    ///
    /// 计算流程见 `prpr-auto-offset`：先用 [`SuperFlux`] 从音频算出 onset 包络（频谱通量），
    /// 再用 [`WeightedGaussianNote`] 把离散的音符时间点抹成连续信号，
    /// 最后 [`estimate_with`] 在搜索区间上做互相关，取相关系数峰值处的偏移。
    ///
    /// 参数来源说明：
    /// - `search_range_sec = 0.30`：只搜索 ±0.3s 的窄区间。因为中心点取的是
    ///   「谱面自带偏移 + 当前手动偏移」，正常情况下两者之和已经接近真实值，
    ///   扩大搜索范围只会引入更多伪峰。
    /// - `sampling_interval_sec = 0.005`：5ms 采样步长，与常见音游的判定精度量级相当，
    ///   再细也只是徒增计算量。
    /// - `search_center_sec = chart.offset + info_offset`：搜索中心，**单位秒**。
    ///   这是「当前实际生效的偏移」——`chart.offset` 是谱面作者写在 `info.yml` 里的值，
    ///   `info_offset` 是玩家在面板上临时微调的增量（界面以毫秒显示）。
    ///   注意 [`AlignResult::offset`] 是**绝对时间**，所以真正的修正量要用
    ///   `result.offset - chart.offset` 求得（见 [`OffsetAnalysisPanel::render_graph_area`]）。
    ///
    /// 为什么放进 `std::thread::spawn`：整首歌的频谱通量加上整个搜索区间的互相关是
    /// 秒级的重计算，放在主线程会直接卡住渲染循环（表现为画面冻结）。
    /// 线程句柄被有意丢弃成 `_handle`（分离线程）：结果通过共享槽位回传，
    /// 不需要 `join`，而线程的生命周期天然被歌曲长度与搜索区间限制。
    #[cfg(not(target_arch = "wasm32"))]
    fn start_analysis(&mut self, chart: &Chart, res: &Resource, info_offset: f32) {
        use std::thread;

        let note_events = extract_note_events(chart);
        // 先克隆音频句柄再取数据，这样下面可以把 PCM 的所有权移进线程闭包，
        // 而不必让 `res` 一直存活到计算结束。
        let clip = res.music.clone();
        // 降为单声道：算法只关心「什么时候有能量跳变」，立体声的两个通道是冗余的，
        // 取左右平均即可，同时把数据量减半。
        let pcm: Vec<f32> = clip.frames().iter().map(|f| (f.0 + f.1) / 2.0).collect();
        let sample_rate = clip.sample_rate();
        let config = AlignConfig {
            search_range_sec: 0.30,
            sampling_interval_sec: 0.005,
            search_center_sec: (chart.offset + info_offset) as f64,
        };

        // 结果槽位：主线程持有 `self.handle`，工作线程持有克隆。
        let result_slot: Arc<Mutex<Option<AlignResult>>> = Arc::new(Mutex::new(None));
        self.handle = Some(result_slot.clone());

        let _handle = thread::spawn(move || {
            // 2048 / 1024 是 STFT 的窗长与 hop：1024 的 hop 即 50% 重叠，
            // 是 onset 检测在「频率分辨率够分辨音高」与「时间分辨率够分辨打点」之间的常用折中。
            let superflux = SuperFlux::new(&pcm, sample_rate, 2048, 1024);
            // 高斯 sigma 取 0.02s：与玩家打点误差的量级相当，
            // 既能把手动的离散音符抹成连续信号，又不会把相邻音符糊成一片导致峰位偏移。
            let note = WeightedGaussianNote::new(note_events, 0.02);
            let duration = pcm.len() as f64 / sample_rate as f64;
            let result = estimate_with(&superflux, &note, duration, &config);
            // 写回共享槽位。若锁被主线程占用则丢弃结果——这是刻意的：
            // 说明主线程正在读取，而读取成功后会清空槽位，本次写入本就多余。
            if let Ok(mut guard) = result_slot.lock() {
                *guard = Some(result);
            }
        });

        self.state = OffsetAnalysisState::Computing;
        // 新一轮分析要重新做一次自动居中。
        self.scroll_centered = false;
    }

    /// wasm 平台上的空实现。
    ///
    /// 浏览器环境无法使用 `std::thread`（单线程运行时），而且整首歌的频谱计算会
    /// 长时间阻塞页面，因此这里干脆什么都不做。结果是网页端点击「自动偏移」无声无息——
    /// 状态机仍停在 `Idle`，界面不会卡在「计算中」，这是有意的降级而非遗漏。
    #[cfg(target_arch = "wasm32")]
    fn start_analysis(&mut self, _chart: &Chart, _res: &Resource, _info_offset: f32) {}

    /// 绘制面板本体，并返回用户这一帧表达的意图（若有）。
    ///
    /// # Arguments
    /// * `info_offset` — 当前偏移（秒），可被 ± 按钮就地修改
    /// * `can_adjust` — 是否允许微调；为假时 ± 按钮点不动（例如谱面不可编辑）
    /// * `labels` — 文案集合，由调用方注入，使本模块不依赖具体的翻译表
    ///
    /// # Returns
    /// `Some(action)` 表示用户本帧点了取消/重置/保存，调用方（`GameScene`）据此
    /// 关闭面板、恢复旧值或写回 `info.offset`；`None` 表示无需处理。
    /// 注意 `Save` 携带的是当前偏移值，因此场景层不需要自己追踪这个数。
    ///
    /// 布局：面板固定在**右上角**（`ui.dx(1. - width - 0.02)` 依赖 UI 原点在屏幕中心、
    /// 横坐标范围为 [-1, 1]），内部再按「标题行 / 曲线区 / 当前值行 / 按钮行」自上而下排。
    /// 标题与「自动偏移」按钮以面板中线为界分成左右两半，正好与曲线图的中央对齐。
    pub fn render(
        &mut self,
        ui: &mut Ui,
        chart: &Chart,
        info_offset: &mut f32,
        can_adjust: bool,
        labels: &OffsetPanelLabels<'_>,
    ) -> Option<OffsetPanelAction> {
        let mut action = None;
        ui.scope(|ui| {
            let width = 0.55;
            let height = 0.4;
            ui.dx(1. - width - 0.02);
            ui.dy(ui.top - height - 0.02);
            ui.fill_rect(Rect::new(0., 0., width, height), GRAY);
            ui.dy(0.02);
            let r = ui
                .text(labels.adjust_offset.as_ref())
                .pos(width / 2. - 0.03, 0.)
                .anchor(1.0, 0.)
                .size(0.7)
                .no_baseline()
                .draw();
            // 计算中时禁用按钮：与 `requested` 标志一起构成「防重复提交」，
            // 否则用户连点会不断启动新线程、白烧 CPU。
            if ui.button("auto-offset", Rect::new(width / 2. + 0.03, r.top(), r.w, r.h), labels.auto_offset.as_ref())
                && !matches!(self.state, OffsetAnalysisState::Computing)
            {
                // 这里只表达意图，真正的启动交给下一帧的 `update`（它才有音频资源）。
                self.requested = true;
            }

            ui.dy(0.04 + r.h / 2.);
            // 曲线区高度随标题行高度反比调整，保证「标题 + 曲线」总高度大致恒定，
            // 这样切换状态时下方的当前值行与按钮行不会上下跳动。
            let graph_rect = Rect::new(0., 0., width, 0.17 - r.h / 2.);
            self.render_graph_area(ui, chart, *info_offset, graph_rect, labels);

            ui.dy(0.02);
            // 当前偏移以**毫秒**显示（`info_offset` 内部是秒），取整避免出现 3.0000001ms
            // 这种读数；± 按钮的水平位置与这一行的垂直中线对齐。
            let r = ui
                .text(format!("{}ms", (*info_offset * 1000.).round() as i32))
                .pos(width / 2., 0.)
                .anchor(0.5, 0.)
                .size(0.6)
                .no_baseline()
                .draw();
            adjust_offset_buttons(ui, info_offset, can_adjust, width, r.center().y);

            ui.dy(0.07);
            // 底部三按钮等分面板宽度，按钮之间与两端各留一个 pad。
            let pad = 0.02;
            let spacing = 0.01;
            let mut r = Rect::new(pad, 0., (width - pad * 2. - spacing * 2.) / 3., 0.06);
            if ui.button("cancel", r, labels.cancel.as_ref()) {
                action = Some(OffsetPanelAction::Cancel);
            }
            r.x += r.w + spacing;
            if ui.button("reset", r, labels.reset.as_ref()) {
                action = Some(OffsetPanelAction::Reset);
            }
            r.x += r.w + spacing;
            if ui.button("save", r, labels.save.as_ref()) {
                action = Some(OffsetPanelAction::Save(*info_offset));
            }
        });
        action
    }

    /// 按状态绘制曲线区域，并在所有分支里**恰好**消耗掉 `graph_rect.h` 的高度。
    ///
    /// 三个分支的竖向占位必须一致：`Idle`/`Computing` 由 [`draw_centered_text`]
    /// 用「先下移半高、画完再下移半高」的方式填满，`Done` 分支则显式 `dy(graph_rect.h)`；
    /// 否则开始分析与出结果之间，面板下方的内容会跳一下。
    ///
    /// `self.state.clone()` 的克隆是为了解耦借用：`Done` 分支内部需要 `&mut self`
    /// （驱动 `scroll` 与自动居中），不克隆就没法同时持有 `self.state` 的不可变借用。
    /// 代价是每帧都会复制一次相关系数曲线，属于可优化点（改动前先确认收益）。
    fn render_graph_area(&mut self, ui: &mut Ui, chart: &Chart, info_offset: f32, graph_rect: Rect, labels: &OffsetPanelLabels<'_>) {
        match self.state.clone() {
            OffsetAnalysisState::Idle => draw_centered_text(ui, graph_rect, labels.analysis_prompt.as_ref()),
            OffsetAnalysisState::Computing => draw_centered_text(ui, graph_rect, labels.analysis_computing.as_ref()),
            OffsetAnalysisState::Done(ref result) => {
                self.scroll.size((graph_rect.w, graph_rect.h));
                let chart_offset = chart.offset;
                self.scroll.render(ui, |ui| {
                    // 内容比可视区宽 [`GRAPH_CONTENT_RATIO`] 倍，横向可滚动。
                    let content_width = graph_rect.w * GRAPH_CONTENT_RATIO;
                    let expanded_rect = Rect::new(0., 0., content_width, graph_rect.h);
                    draw_offset_graph(chart_offset, info_offset, ui, expanded_rect, result);
                    (content_width, graph_rect.h)
                });

                // 右上角读数：推荐**修正量**（毫秒，带符号）。
                // 之所以减 `chart_offset` 而不是直接用 `result.offset`：后者是绝对时间，
                // 而用户需要知道的是「相对谱面自带偏移还要再挪多少」。
                let correction_ms = ((result.offset - chart_offset as f64) * 1000.0).round() as i32;
                let r = ui
                    .text(format!("{correction_ms:+}ms"))
                    .pos(graph_rect.w - 0.01, 0.)
                    .anchor(1.0, 0.0)
                    .size(0.35)
                    .color(Color::new(0.0, 1.0, 0.0, 0.7))
                    .no_baseline()
                    .draw();
                // 第二行是匹配置信度（相关系数峰值）：它才是判断「这个推荐值可不可信」的依据，
                // 因此与推荐值放在一起、用琥珀色与绿色区分。
                ui.text(format!("{:.3}", peak_match_score(result)))
                    .pos(graph_rect.w - 0.01, r.bottom() + 0.003)
                    .anchor(1.0, 0.0)
                    .size(0.3)
                    .color(Color::new(1.0, 0.9, 0.55, 0.72))
                    .no_baseline()
                    .draw();
                draw_threshold_labels(ui, graph_rect, result);
                // 只在结果刚到达时自动定位一次（内部有闩锁），之后尊重用户的滚动位置。
                self.center_on_recommendation(result, graph_rect.w);
                ui.dy(graph_rect.h);
            }
        }
    }

    /// 把曲线视图滚动到让推荐值（绿线）落在可视区中央，只执行一次。
    ///
    /// 两个提前返回都是必要的健壮性保护：
    /// - `scroll_centered` 已为真 → 用户可能已经手动滚开，不要把他的视图拽回去；
    /// - 曲线为空 → `min_o`/`max_o` 都退化成 0，算出的位置毫无意义。
    ///
    /// 直接写 `x_scroller.offset` 而不是走 `goto`，是因为这是一次性瞬时定位，
    /// 不需要惯性或动画；clamp 的上界必须是 `content_width - width`（真正的滚动余量），
    /// 用 `width` 会导致靠后的推荐值无法居中。
    fn center_on_recommendation(&mut self, result: &AlignResult, width: f32) {
        if self.scroll_centered || result.correlation_curve.is_empty() {
            return;
        }
        let curve = &result.correlation_curve;
        // 曲线按 offset 升序排列，因此首尾元素就是横轴范围。
        let min_o = curve.first().map(|&(o, _)| o).unwrap_or(0.0);
        let max_o = curve.last().map(|&(o, _)| o).unwrap_or(0.0);
        // `max(1e-6)` 防止区间退化为 0 时除零产生 NaN 坐标。
        let o_range = (max_o - min_o).max(1e-6);
        let content_width = width * GRAPH_CONTENT_RATIO;
        let green_x = ((result.offset - min_o) / o_range) as f32 * content_width;
        self.scroll.x_scroller.offset = (green_x - width / 2.0).clamp(0.0, content_width - width);
        self.scroll_centered = true;
    }
}

/// 面板所需的全部文案。
///
/// 用 `Cow<'a, str>` 而非 `&'a str` 或 `String`，是为了让调用方既能直接借用
/// 翻译表里的静态字符串（不分配），也能在需要时传入临时拼好的字符串；
/// 面板只读地引用它，因此生命周期参数 `'a` 不污染面板自身的类型。
#[derive(Clone)]
pub struct OffsetPanelLabels<'a> {
    /// 「调整偏移」标题。
    pub adjust_offset: Cow<'a, str>,
    /// 「自动偏移」按钮文案。
    pub auto_offset: Cow<'a, str>,
    /// 未开始分析时的曲线区提示。
    pub analysis_prompt: Cow<'a, str>,
    /// 计算进行中的曲线区提示。
    pub analysis_computing: Cow<'a, str>,
    /// 取消按钮。
    pub cancel: Cow<'a, str>,
    /// 重置按钮。
    pub reset: Cow<'a, str>,
    /// 保存按钮。
    pub save: Cow<'a, str>,
}

/// 当前偏移值两侧的 ± 微调按钮：三档步长，从粗到细。
///
/// 关键在于三档的落点各不相同：`d`（距离面板左右边缘的距离）取 0.14 / 0.08 / 0.03，
/// 使三组按钮的命中矩形互不重叠；`feather` 随 `d` 递减（0.026 / 0.022 / 0.017），
/// 让三档的触摸面积尽量相当——越靠近边缘的按钮能向外扩张的余量越小。
///
/// 步长依次是 0.05 / 0.005 / 0.001 **秒**，即界面上显示的 50ms / 5ms / 1ms。
/// 提供三档而不是一个滑块，是因为对齐校正的典型流程是「先粗调再微调」，
/// 逐次点击比拖动滑块更容易精确命中整毫秒值。
///
/// `can_adjust` 只门控**效果**而不是绘制：按钮照常画出（布局稳定、不会闪烁），
/// 只是点了不起作用。
fn adjust_offset_buttons(ui: &mut Ui, info_offset: &mut f32, can_adjust: bool, width: f32, center_y: f32) {
    // 粗调 ±50ms
    let d = 0.14;
    if ui.button("lg_sub", Rect::new(d, center_y, 0., 0.).feather(0.026), "-") && can_adjust {
        *info_offset -= 0.05;
    }
    if ui.button("lg_add", Rect::new(width - d, center_y, 0., 0.).feather(0.026), "+") && can_adjust {
        *info_offset += 0.05;
    }
    // 中调 ±5ms
    let d = 0.08;
    if ui.button("sm_sub", Rect::new(d, center_y, 0., 0.).feather(0.022), "-") && can_adjust {
        *info_offset -= 0.005;
    }
    if ui.button("sm_add", Rect::new(width - d, center_y, 0., 0.).feather(0.022), "+") && can_adjust {
        *info_offset += 0.005;
    }
    // 细调 ±1ms：0.001 秒正是偏移字段（三位小数）能表达的最小单位。
    let d = 0.03;
    if ui.button("ti_sub", Rect::new(d, center_y, 0., 0.).feather(0.017), "-") && can_adjust {
        *info_offset -= 0.001;
    }
    if ui.button("ti_add", Rect::new(width - d, center_y, 0., 0.).feather(0.017), "+") && can_adjust {
        *info_offset += 0.001;
    }
}

/// 在一段高度内垂直居中地画一行文字，并恰好消耗掉 `rect.h` 的高度。
///
/// UI 布局游标只能单向向下推进，因此居中的做法是「先前进到中线，画完再前进到区间末尾」；
/// `0.03` 是 0.5 号字行高的一半，减去它才能让文字视觉上真正居中而不是偏下。
/// 保持总消耗量等于 `rect.h` 是硬性约定，见 [`OffsetAnalysisPanel::render_graph_area`]。
fn draw_centered_text(ui: &mut Ui, rect: Rect, text: &str) {
    ui.dy(rect.h / 2. - 0.03);
    ui.text(text).pos(rect.w / 2., 0.).anchor(0.5, 0.5).size(0.5).no_baseline().draw();
    ui.dy(rect.h / 2. + 0.03);
}

/// 在曲线区左侧画出 [`SCORE_REFERENCE_LINES`] 的刻度文字。
///
/// 两点约定：
/// - `v_pad = 0.08` 必须与 [`draw_offset_graph`] 里的同名值保持一致，
///   否则文字会与参考线错位；
/// - 该函数在滚动容器**之外**调用，因此这些刻度是「坐标轴」而不是「内容」——
///   横向滚动曲线时它们固定不动，这符合读图的直觉。
/// 高于当前纵轴上限（`s_top`）的参考线直接跳过，否则文字会画到图外。
fn draw_threshold_labels(ui: &mut Ui, graph_rect: Rect, result: &AlignResult) {
    let s_top = offset_graph_score_top(result);
    let v_pad = 0.08;
    let inner_y = graph_rect.h * v_pad;
    let inner_h = graph_rect.h * (1.0 - 2.0 * v_pad);
    for (value, label) in SCORE_REFERENCE_LINES {
        if value > s_top {
            continue;
        }
        // 纵轴向上为「分数更高」，因此用 `1.0 - value / s_top` 做一次翻转。
        let y = inner_y + (1.0 - value / s_top) * inner_h;
        // 稍微下移 `inner_h * 0.015`，让文字压在参考线下方而不是被线穿过。
        ui.text(label)
            .pos(0.01, y + inner_h * 0.015)
            .anchor(0.0, 0.0)
            .size(0.3)
            .color(Color::new(1.0, 0.82, 0.32, 0.62))
            .no_baseline()
            .draw();
    }
}

/// 纵轴上限：本次得分与 [`MIN_SCORE_TOP`] 取较大者。
///
/// 这条下限是「诚实的缩放」：低分结果若按自身分数拉满纵轴，图上会出现一个看起来很
/// 明显的峰，用户会误以为算法很有把握；加上下限后低分曲线只是微微起伏，与事实相符。
fn offset_graph_score_top(result: &AlignResult) -> f32 {
    peak_match_score(result).max(MIN_SCORE_TOP)
}

/// 本次估值的最高得分：曲线上所有采样点的最大值，并与 `result.correlation` 取较大者。
///
/// 之所以把 `correlation` 也作为初值：它是算法在**连续搜索空间**上求得的精确峰值，
/// 而曲线只是按采样步长离散化后的结果，未必采到那个峰值点；
/// 以它为初值可以保证纵轴上限不会低于真实峰值，从而不会把曲线画到图外。
fn peak_match_score(result: &AlignResult) -> f32 {
    result.correlation_curve.iter().map(|&(_, s)| s).fold(result.correlation as f32, f32::max)
}

/// 画出相关系数曲线图：背景网格 + 参考线 + 曲线本体 + 三条标记线。
///
/// 坐标映射：横轴是**绝对偏移时间**（秒），范围直接取曲线自身的首尾 offset，
/// 因此横轴的缩放完全由算法的搜索区间决定；纵轴是相关系数，按
/// [`offset_graph_score_top`] 归一化后翻转成屏幕 y（分数越高越靠上）。
///
/// # Arguments
/// * `rect` — 落在滚动容器内容里的**扩展**矩形，宽度是可视区的 [`GRAPH_CONTENT_RATIO`] 倍
/// * `chart_offset` — 谱面自带偏移（秒），既是网格零点也是橙色标记的位置
/// * `info_offset` — 玩家当前的临时增量（秒），与 `chart_offset` 之和即蓝色标记（当前生效值）
/// * `result` — 算法结果，`offset` 是绝对时间，画成绿色标记（推荐值）
fn draw_offset_graph(chart_offset: f32, info_offset: f32, ui: &mut Ui, rect: Rect, result: &AlignResult) {
    let curve = &result.correlation_curve;
    // 空曲线（例如音频过短、算法提前返回）时什么都不画，避免后面的
    // `first()/last()` 退化成 0 并算出无意义的坐标。
    if curve.is_empty() {
        return;
    }

    let min_o = curve.first().map(|&(o, _)| o).unwrap_or(0.0);
    let max_o = curve.last().map(|&(o, _)| o).unwrap_or(0.0);
    let o_range = (max_o - min_o).max(1e-6);
    let s_top = offset_graph_score_top(result);

    // 半透明黑底：让灰白曲线与彩色标记在浅色曲绘上也能看清。
    ui.fill_rect(rect, Color::new(0.0, 0.0, 0.0, 0.3));

    // 曲线只画在上下各留 8% 的 `inner` 区域内，给顶部读数与刻度文字让位；
    // 这个 0.08 必须与 `draw_threshold_labels` 一致。
    let v_pad = 0.08;
    let inner = Rect::new(rect.x, rect.y + rect.h * v_pad, rect.w, rect.h * (1.0 - 2.0 * v_pad));
    // 线宽取内容宽度的千分之三：内容宽度是可视区的固定倍数，
    // 因此线宽随可视区尺寸等比缩放，换分辨率时粗细观感一致。
    let line_w = rect.w * 0.003;

    // 网格线贯穿整个 `rect`（而非 `inner`），这样它读起来是「图表的底纹」而不是曲线的一部分。
    draw_offset_time_grid(ui, rect, min_o, o_range, chart_offset as f64, line_w);

    // 参考线：y 坐标算法必须与 `draw_threshold_labels` 完全相同，否则文字与线会错开。
    for (value, _) in SCORE_REFERENCE_LINES {
        if value > s_top {
            continue;
        }
        let y = inner.y + (1.0 - value / s_top) * inner.h;
        let mut mb = lyon::path::Path::builder();
        mb.begin(point(inner.x, y));
        mb.line_to(point(inner.x + inner.w, y));
        mb.end(false);
        ui.stroke_path(&mb.build(), line_w, Color::new(1.0, 0.82, 0.32, 0.26));
    }

    // 曲线本体：抽样到约 70 个点再连线。内容区在屏幕上也就几百像素宽，
    // 画更多点肉眼看不出来，只会白白增加顶点数与三角化开销。
    // `ceil` 保证 `step >= 1`（前面已排除空曲线），`step_by` 因此不会步进为 0。
    let max_pts = 70usize;
    let step = ((curve.len() as f64) / (max_pts as f64)).ceil() as usize;
    let mut path_builder = lyon::path::Path::builder();
    let mut first = true;
    for i in (0..curve.len()).step_by(step) {
        let (o, s) = curve[i];
        let x = inner.x + ((o - min_o) / o_range) as f32 * inner.w;
        // 这里对归一化分数做了 clamp（参考线那一段没做）：曲线点万一超过纵轴上限，
        // 会被压在顶边而不是画到图外——虽然 `s_top` 已保证不低于峰值，属兜底。
        let y = inner.y + (1.0 - (s / s_top).clamp(0.0, 1.0)) * inner.h;
        if first {
            path_builder.begin(point(x, y));
            first = false;
        } else {
            path_builder.line_to(point(x, y));
        }
    }
    path_builder.end(false);
    ui.stroke_path(&path_builder.build(), line_w, Color::new(0.6, 0.6, 0.6, 0.6));

    // 三条标记线（半透明，便于两两重合时仍能分辨）：
    // 橙 = 谱面自带偏移（也是网格零点）、绿 = 算法推荐值、蓝 = 当前生效值。
    // 橙↔绿 的距离就是需要修正的量（对应右上角的绿色读数），
    // 蓝↔绿 的距离则是用户还需要再挪多少——三种颜色的语义在界面文案里并未写明，
    // 全靠这张图的相对位置传达。
    let marker_line_w = line_w * 1.5;
    draw_offset_marker(ui, inner, min_o, o_range, chart_offset as f64, marker_line_w, Color::new(1.0, 0.5, 0.0, 0.5));
    draw_offset_marker(ui, inner, min_o, o_range, result.offset, marker_line_w, Color::new(0.0, 1.0, 0.0, 0.5));
    draw_offset_marker(ui, inner, min_o, o_range, (chart_offset + info_offset) as f64, marker_line_w, Color::new(0.0, 0.5, 1.0, 0.5));
}

/// 画竖向时间网格线。
///
/// 刻度以 `zero_offset`（= 谱面自带偏移）为**零点**，而不是以可视区左边界为起点：
/// `min_tick`/`max_tick` 用 `(offset - zero_offset) / step` 的 ceil/floor 求出，
/// 于是线条始终落在绝对时间的整倍数上，横向滚动时网格不会跟着滑移。
///
/// 每两条线之一是「主线」（更亮、更粗），等效于 0.1s 粗网格 + 0.05s 细分，
/// 既能读出量级又能定位到 0.05s。
fn draw_offset_time_grid(ui: &mut Ui, rect: Rect, min_o: f64, o_range: f64, zero_offset: f64, width: f32) {
    let step = 0.05;
    let min_tick = ((min_o - zero_offset) / step).ceil() as i32;
    let max_tick = ((min_o + o_range - zero_offset) / step).floor() as i32;
    for tick in min_tick..=max_tick {
        let offset = zero_offset + tick as f64 * step;
        let x = rect.x + ((offset - min_o) / o_range) as f32 * rect.w;
        // 负数 tick 时 Rust 的 `%` 会给出 -1，但判定「是否等于 0」不受影响，
        // 因此奇偶分类对零点两侧的行为是一致的。
        let is_major = tick % 2 == 0;
        let mut mb = lyon::path::Path::builder();
        mb.begin(point(x, rect.y));
        mb.line_to(point(x, rect.y + rect.h));
        mb.end(false);
        let color = if is_major {
            Color::new(1.0, 1.0, 1.0, 0.13)
        } else {
            Color::new(1.0, 1.0, 1.0, 0.07)
        };
        ui.stroke_path(&mb.build(), width * if is_major { 0.8 } else { 0.55 }, color);
    }
}

/// 在 `offset` 对应位置画一条贯穿 `inner` 高度的竖线。
///
/// 越界时直接返回而不是 clamp 到边缘：标记线落在图外的含义是「这个值不在搜索区间内」，
/// 若把它钉在边缘，看起来就像是落在区间端点上的一个合理值，反而误导。
fn draw_offset_marker(ui: &mut Ui, inner: Rect, min_o: f64, o_range: f64, offset: f64, width: f32, color: Color) {
    let max_o = min_o + o_range;
    if offset < min_o || offset > max_o {
        return;
    }
    let x = inner.x + ((offset - min_o) / o_range) as f32 * inner.w;
    let mut mb = lyon::path::Path::builder();
    mb.begin(point(x, inner.y));
    mb.line_to(point(x, inner.y + inner.h));
    mb.end(false);
    ui.stroke_path(&mb.build(), width, color);
}

/// 把谱面里所有音符拍平成算法需要的 `(时间, 类型)` 事件序列。
///
/// 两类音符被过滤掉：
/// - `fake`（假音符）：只用于演出渲染，没有对应的打击音，留着会污染音符信号；
/// - `time < 0.0`：负数时间来自开场前的对象（如提前出现的装饰音符），
///   而算法的时间基准从 0 开始，负数会被采样成一堆无效的边界点。
///
/// 最后按时间升序排序：算法要在等间隔时间栅格上采样音符信号并做互相关，
/// 有序序列才能线性扫描，并且这与 `prpr-auto-offset` 的输入约定一致。
///
/// # Panics
/// `partial_cmp().unwrap()` 要求所有时间都是有限值；谱面解析阶段已经排除了
/// NaN/Inf，因此实际不会触发。
fn extract_note_events(chart: &Chart) -> Vec<NoteEvent> {
    let mut notes: Vec<NoteEvent> = chart
        .lines
        .iter()
        .flat_map(|line| line.notes.iter())
        .filter(|note| !note.fake && note.time >= 0.0)
        .map(|note| NoteEvent::new(note.time, auto_offset_note_kind(&note.kind)))
        .collect();
    notes.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap());
    notes
}

/// 把 prpr 的音符类型映射到 `prpr-auto-offset` 的类型。
///
/// 没有写成 `From`/`Into` 实现而是显式列表，是为了让两个枚举**故意保持解耦**：
/// 它们分属渲染层与算法层，词汇表未必一一对应（例如将来把 Hold 的头尾拆成两个事件、
/// 或把 Flick 与 Drag 合并处理），映射规则写在这里改起来一目了然，
/// 也不会因为外部枚举新增变体而自动“猜”出一个映射。
fn auto_offset_note_kind(kind: &NoteKind) -> AutoOffsetNoteKind {
    match kind {
        NoteKind::Click => AutoOffsetNoteKind::Tap,
        NoteKind::Hold { .. } => AutoOffsetNoteKind::Hold,
        NoteKind::Flick => AutoOffsetNoteKind::Flick,
        NoteKind::Drag => AutoOffsetNoteKind::Drag,
    }
}
