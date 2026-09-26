//! Time manager for music time and real time synchronization.
//!
//! 游戏内一切动画、判定与音乐播放都以 `TimeManager::now` 返回的秒为时间基准。
//! 该时间由“真实时钟”线性推导而来，因此不受掉帧影响（不会像累加 dt 那样漂移），
//! 同时可通过调整时间原点与音乐播放位置缓慢对齐。

use crate::config::Config;

/// 音画同步时间管理器：把真实经过时间换算为游戏内时间，并与音乐播放位置对齐。
pub struct TimeManager {
    /// 是否允许用音乐播放位置校正游戏时间（来自 [`Config::adjust_time`]）。
    pub adjust_time: bool,
    /// 时间原点，即让 `now()` 归零的那个真实时刻（单位：秒，取自 `get_time_fn`）。
    pub start_time: f64,
    /// 暂停发生的真实时刻；`Some` 表示当前处于暂停态，`None` 表示正常走时。
    pause_time: Option<f64>,
    /// 播放速度倍率。`now()` 会按它缩放，因此变速时音乐与谱面同时变速、不会失步。
    pub speed: f64,
    /// 对齐收敛系数：每帧把“音乐位置 - 游戏时间”的误差按此比例折算回 `start_time`。
    pub force: f64,
    /// 禁止自动对齐的截止时刻（真实时间秒）。在它之前不做时间修正，
    /// 取 `f64::NEG_INFINITY` 表示立即允许对齐。
    wait: f64,

    /// 真实时间（秒）的来源，平台相关：桌面用单调时钟，wasm 用 `performance.now()`。
    get_time_fn: Box<dyn Fn() -> f64>,
}

// 默认 1 倍速、不开启对齐，且时间原点在构造时确定；
// 适用于不播放音乐的界面（菜单、设置页）中需要一条稳定时间轴的情形。
impl Default for TimeManager {
    fn default() -> Self {
        Self::new(1.0, false)
    }
}

// 时间轴的构造、读时与暂停 / 跳转控制。
impl TimeManager {
    /// 依据用户配置构造。当前只有 `adjust_time` 会影响本结构，
    /// 速度倍率由游玩流程在进入谱面时另行设置。
    pub fn from_config(config: &Config) -> Self {
        Self::new(1., config.adjust_time)
    }

    /// 用调用方提供的时钟函数构造，并以该函数为准（不启用对齐）。
    ///
    /// 适用于“时间权威来源在外部”的场景，例如直接以音频播放位置作为游戏时间。
    /// 此时 `force` / `wait` 仍被填入常规初值，便于调用方之后自行打开
    /// `adjust_time` 进入对齐模式。
    pub fn manual(get_time_fn: Box<dyn Fn() -> f64>) -> Self {
        let start_time = get_time_fn();
        Self {
            adjust_time: false,
            start_time,
            pause_time: None,
            speed: 1.0,
            wait: f64::NEG_INFINITY,
            force: 3e-3,

            get_time_fn,
        }
    }

    /// 构造时间管理器。
    ///
    /// # Arguments
    /// * `speed` - 初始速度倍率
    /// * `adjust_time` - 是否启用音乐播放位置对齐
    ///
    /// 真实时钟的选型有平台差异：wasm 下改用 `performance.now()`，
    /// 因为 Web Audio 的 `currentTime` 更新粒度粗且受音频线程调度影响会抖动，
    /// 直接当作画面时间基准会造成明显的延迟和卡顿感；其他平台使用单调的
    /// [`std::time::Instant`]，不会受系统时间被修改的影响。
    pub fn new(speed: f64, adjust_time: bool) -> Self {
        // we use performance.now() on web since audioContext.currentTime is not stable
        // and may cause serious latency problem
        #[cfg(target_arch = "wasm32")]
        let get_time_fn = {
            let perf = web_sys::window().unwrap().performance().unwrap();
            move || perf.now() / 1000.
        };
        #[cfg(not(target_arch = "wasm32"))]
        let get_time_fn = {
            let start = std::time::Instant::now();
            move || start.elapsed().as_secs_f64()
        };
        // 以构造时刻作为时间原点，使 `now()` 从 0 开始。
        let t = get_time_fn();
        Self {
            adjust_time,
            start_time: t,
            pause_time: None,
            speed,
            wait: f64::NEG_INFINITY,
            force: 3e-3,

            get_time_fn: Box::new(get_time_fn),
        }
    }

    /// 读取底层真实时钟的当前值（秒）。这是未经速度缩放、未经对齐修正的原始时间，
    /// 主要用于调试与对齐窗口判定，业务逻辑应使用 [`TimeManager::now`]。
    pub fn real_time(&self) -> f64 {
        (self.get_time_fn)()
    }

    /// 把时间原点重置到“现在”，同时清除暂停态与对齐等待窗口。
    /// 相当于让游戏时间立刻归零并重新开始走时（谱面重开用）。
    pub fn reset(&mut self) {
        self.start_time = self.real_time();
        self.pause_time = None;
        self.wait = f64::NEG_INFINITY;
    }

    /// 开启 0.1 秒的对齐等待窗口。
    ///
    /// 0.1 秒的取值是为了给音频后端留出足够时间完成 seek / 起播，
    /// 等 `currentTime` 稳定下来再开始自动对齐，避免刚跳转就被
    /// 尚未更新的音乐位置“纠正”回去而反复抖动。
    pub fn wait(&mut self) {
        self.wait = self.real_time() + 0.1;
    }

    /// 立即取消等待窗口，使下一次 [`TimeManager::update`] 就允许对齐。
    pub fn dont_wait(&mut self) {
        self.wait = f64::NEG_INFINITY;
    }

    /// 当前游戏内时间（秒）。
    ///
    /// 公式：`(基准时刻 - 时间原点) * 速度倍率`，其中基准时刻在暂停时取
    /// `pause_time`（时间冻结）、否则取真实时钟。整个时间轴由真实时钟推导，
    /// 因此帧率波动不会造成时间漂移；`#[must_use]` 提醒调用方该函数无副作用。
    #[must_use]
    pub fn now(&self) -> f64 {
        (self.pause_time.unwrap_or_else(&self.get_time_fn) - self.start_time) * self.speed
    }

    /// 用音乐的实际播放位置 `music_time`（秒）校正游戏时间。
    ///
    /// 生效条件：开启了 [`TimeManager::adjust_time`]、当前不处于暂停态、
    /// 且已越过 `wait` 指定的静默窗口（避免刚 seek 完就被过期的音乐位置带偏）。
    ///
    /// 修正对象是时间原点 `start_time` 而不是当前时间，因此画面表现为平滑收敛
    /// 而非瞬间跳变。`force = 3e-3` 决定收敛速度：每帧只消化约 0.3% 的误差，
    /// 数十毫秒的偏差需要约 1~2 秒拉平——足够快以消除累积漂移，
    /// 又足够慢以让玩家察觉不到画面在“追赶”。
    pub fn update(&mut self, music_time: f64) {
        if self.adjust_time && self.real_time() > self.wait && self.pause_time.is_none() {
            self.start_time -= (music_time - self.now()) * self.force;
        }
    }

    /// 当前是否处于暂停态。暂停时 [`TimeManager::now`] 冻结不变。
    #[must_use]
    pub fn paused(&self) -> bool {
        self.pause_time.is_some()
    }

    /// 暂停：记录发生暂停的真实时刻，此后 `now()` 将以它为基准而不再推进。
    pub fn pause(&mut self) {
        self.pause_time = Some(self.real_time());
    }

    /// 恢复走时。
    ///
    /// 把暂停期间流逝的真实时长补加到 `start_time` 上，使暂停完全不占用游戏时间
    /// （而不是让谱面时间瞬间前跳）。随后开启等待窗口，
    /// 因为音乐恢复播放需要一点时间才回到正确位置。
    pub fn resume(&mut self) {
        self.start_time += self.real_time() - self.pause_time.take().unwrap();
        self.wait();
    }

    /// 把游戏时间跳转到 `pos` 秒。
    ///
    /// 由目标时间反推出时间原点：`start_time = 基准时刻 - pos / speed`，
    /// 其中基准时刻在暂停态取 `pause_time`，否则取真实时钟——这样暂停中 seek
    /// 不会因为时钟前进而产生额外偏移。跳转后开启等待窗口，
    /// 等音乐 seek 到新位置再恢复自动对齐。
    pub fn seek_to(&mut self, pos: f64) {
        self.start_time = self.pause_time.unwrap_or_else(&self.get_time_fn) - pos / self.speed;
        self.wait();
    }
}
