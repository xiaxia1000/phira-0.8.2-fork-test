//! Configuration module of the playing environment.\
//! e.g. player name, volume, speed, autoplay, etc.
//!
//! 本模块只负责“设置的表示与持久化结构”，不涉及设置界面；
//! 所有字段都以 camelCase 序列化，与前端 / 旧版客户端保持字节级兼容。

use bitflags::bitflags;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

/// 加载界面展示的提示语，`tips.txt` 中的每一行（含空行）对应一个元素。
///
/// 用 `include_str!` 在编译期嵌入文本，配合 [`Lazy`] 延迟到首次访问才切分，
/// 避免启动时为不看提示的场景付出解析开销；全局只读，故可安全跨线程共享。
pub static TIPS: Lazy<Vec<String>> = Lazy::new(|| include_str!("tips.txt").split('\n').map(str::to_owned).collect());

// 玩法修饰符集合：以位标志存储，便于一次性序列化并与旧客户端互换。
// 注意这是宏调用，上方不能用文档注释，故此处用普通注释。
bitflags! {
    #[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Debug)]
    #[serde(transparent)]
    /// 玩法修饰符（mods）集合，决定本局演奏的附加规则与视觉效果。
    ///
    /// 用 `i32` 位域而非结构体，是为了与旧客户端/服务端的整数字段直接兼容；
    /// `serde(transparent)` 保证序列化结果就是一个整数而不是对象。
    pub struct Mods: i32 {
        /// 自动演奏：所有音符自动命中（观感为满 Perfect），用于预览谱面，
        /// 与 [`Mods::UNRATED`] 一起决定成绩不计入排行。
        const AUTOPLAY = 0x0001;
        /// 水平镜像画面（左右翻转）；只影响视觉呈现，不改变判定结果。
        const FLIP_X = 0x0002;
        /// 音符在越过判定线时淡出，而非瞬间消失；与 [`Mods::FADE_IN`] 互斥。
        const FADE_OUT = 0x0004;
        /// 音符在接近判定线前淡入；与 [`Mods::FADE_OUT`] 互斥。
        const FADE_IN = 0x0008;
        /// 夜核模式：以更高速度播放音乐并相应压缩谱面时间轴，考验反应速度。
        const NIGHTCORE = 0x0010;
        /// 彩虹模式：音符颜色随判定线/时间循环变色。
        const RAINBOW = 0x0020;
        /// 禁用自定义着色器，退回到固定功能管线渲染，用于兼容着色器支持不佳的设备。
        const NO_SHADER = 0x0040;
        /// AP 暴毙：本局出现任何非 Perfect 判定都立即结束（挑战 AP 用）。
        const INSTANT_DEATH_AP = 0x0080;
        /// FC 暴毙：本局出现 Miss 立即结束（挑战 FC 用）；与 [`Mods::INSTANT_DEATH_AP`] 互斥。
        const INSTANT_DEATH_FC = 0x0100;

        /// 不计入排行的标志组合：自动演奏 + 无着色器，服务端据此判定成绩无效。
        const UNRATED = Self::AUTOPLAY.bits() | Self::NO_SHADER.bits();
    }
}

// 修饰符的交互语义：切换开关时顺带维护互斥关系。
impl Mods {
    /// 反转修饰符 `flag` 的开关状态。
    ///
    /// 打开前先移除所有与它互斥的修饰符（见 `Mods::conflicts`），
    /// 保证互斥组里至多有一个生效；关闭时只删除自身，不动其它位。
    pub fn toggle_mod(&mut self, flag: Mods) {
        if self.contains(flag) {
            self.remove(flag);
        } else {
            for &conflict in Mods::conflicts(flag) {
                self.remove(conflict);
            }
            self.insert(flag);
        }
    }
    /// 返回与 `flag` 互斥的修饰符列表——即在开启 `flag` 之前必须被清除的项。
    ///
    /// 互斥关系由玩法语义决定：淡入/淡出是同一视觉效果的两种相反表现，
    /// 两种暴毙规则也不可能同时成立；其余修饰符两两兼容，返回空切片。
    fn conflicts(flag: Mods) -> &'static [Mods] {
        match flag {
            Mods::FADE_IN => &[Mods::FADE_OUT],
            Mods::FADE_OUT => &[Mods::FADE_IN],
            Mods::INSTANT_DEATH_AP => &[Mods::INSTANT_DEATH_FC],
            Mods::INSTANT_DEATH_FC => &[Mods::INSTANT_DEATH_AP],
            _ => &[],
        }
    }
}

/// 播放环境配置，对应磁盘上的用户配置文件。
///
/// `#[serde(default)]` 保证配置文件缺少新字段时仍可反序列化（用 [`Default`] 补齐），
/// 这是配置结构可以自由增删字段的前提；`rename_all = "camelCase"` 则要求
/// 前端 / 旧客户端沿用同一套字段名，改名会直接导致用户设置丢失。
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(rename = "adjust_time_new")]
    /// 是否启用音画自动对齐。
    ///
    /// 序列化名保留 `adjust_time_new`：该开关经历过一次实现重写，
    /// 沿用旧字段名才能继承老用户的既有设置。开启后 [`crate::time::TimeManager`]
    /// 会以极小的收敛系数把游戏时间缓慢对回音乐实际播放位置。
    pub adjust_time: bool,
    /// 激进模式：更早地裁剪/回收不可见对象以换取帧率，代价是极端谱面可能出现视觉残缺。
    pub aggressive: bool,
    /// 是否在游玩界面显示 AP / FC 实时指示器。
    pub ap_fc_indicator: bool,
    /// 强制画面宽高比；`None` 表示跟随窗口 / 屏幕的真实比例。
    pub aspect_ratio: Option<f32>,
    /// 音频缓冲区大小（单位：帧）。越大越不易爆音但延迟越高；`None` 交给后端决定。
    pub audio_buffer_size: Option<u32>,
    /// 谱面调试模式：显示判定线坐标、命中范围等辅助信息。
    pub chart_debug: bool,
    /// 关闭全部判定线特效（着色器与粒子），用于低端设备提速或排查渲染问题。
    pub disable_effect: bool,
    /// 是否允许双击屏幕暂停（移动端用于避免误触暂停）。
    pub double_click_to_pause: bool,
    /// 是否显示“双击可暂停”的引导提示。
    pub double_hint: bool,
    /// 是否以全屏方式启动。
    pub fullscreen_mode: bool,
    /// 是否开启 FXAA 抗锯齿。
    pub fxaa: bool,
    /// 交互模式：允许玩家点按判定线等可交互元素（谱面预览 / 编辑器场景需要）。
    pub interactive: bool,
    /// 当前启用的玩法修饰符位集合，见 [`Mods`]。
    pub mods: Mods,
    /// 联机服务器地址，格式为 `host:port`。
    pub mp_address: String,
    /// 是否启用联机（多人同房）功能。
    pub mp_enabled: bool,
    /// 音符整体缩放倍率，`1.0` 为原始大小；会与谱面自带的缩放设置相乘。
    pub note_scale: f32,
    /// 离线模式：禁止全部网络访问（成绩上传、资源下载等一律跳过）。
    pub offline_mode: bool,
    /// 判定偏移，单位为秒。正值表示判定整体延后，用于补偿设备音频输出链路的延迟。
    pub offset: f32,
    /// 是否启用粒子特效。
    pub particle: bool,
    /// 玩家昵称，用于成绩展示与联机房间内标识。
    pub player_name: String,
    /// 玩家 rks（Rating 综合实力分），由服务端下发并缓存于此。
    pub player_rks: f32,
    /// 期望的音频采样率（单位：Hz）；`None` 表示使用后端默认采样率。
    pub preferred_sample_rate: Option<u32>,
    /// 自定义资源包路径；`None` 表示使用内置默认资源。
    pub res_pack_path: Option<String>,
    /// MSAA 多重采样数，`1` 表示关闭抗锯齿；部分移动 GPU 会被强制降为 1。
    pub sample_count: u32,
    /// 是否显示实时准确率。
    pub show_acc: bool,
    /// 是否显示平均帧率。
    pub show_avg_fps: bool,
    /// 谱面流速倍率，`1.0` 为标准速度；只影响音符移动速度，不改变判定时刻。
    pub speed: f32,
    /// 触控调试：绘制触点轨迹与判定区域，便于定位触控问题。
    pub touch_debug: bool,
    /// 是否允许用键盘按键模拟触摸点（桌面端无触屏时的开发/试玩手段）。
    pub use_keyboard: bool,
    /// 背景音乐（菜单音乐）音量，取值范围 `0.0..=1.0`。
    pub volume_bgm: f32,
    /// 谱面音乐音量，取值范围 `0.0..=1.0`。
    pub volume_music: f32,
    /// 音效（打击音与界面音）音量，取值范围 `0.0..=1.0`。
    pub volume_sfx: f32,

    // for compatibility
    /// 旧版配置中的 `autoplay` 布尔项。反序列化后由 [`Config::init`] 折叠进
    /// [`Mods::AUTOPLAY`]；此字段保留仅为读取历史配置文件，新配置里始终为 `None`。
    autoplay: Option<bool>,
}

// 出厂默认值。这些默认值都是刻意的产品决策，而非随手填写：
// - `interactive` / `particle` / `double_hint` / `double_click_to_pause` 默认开启，
//   保证首次启动即可获得完整体验；
// - `offset` 取 0，不去偏袒任何设备的音频链路；
// - `mp_address` 指向官方联机服务器；
// - `player_name` / `player_rks` 只是占位，登录后会被真实数据覆盖；
// - `adjust_time` 默认关闭，因为自动对齐会对少数设备的音频时钟产生可见漂移。
impl Default for Config {
    fn default() -> Self {
        Self {
            adjust_time: false,
            aggressive: true,
            ap_fc_indicator: true,
            aspect_ratio: None,
            audio_buffer_size: None,
            chart_debug: false,
            disable_effect: false,
            double_click_to_pause: true,
            double_hint: true,
            fxaa: false,
            interactive: true,
            mods: Mods::default(),
            mp_address: "mp2.phira.cn:12345".to_owned(),
            mp_enabled: false,
            note_scale: 1.0,
            offline_mode: false,
            fullscreen_mode: false,
            offset: 0.,
            particle: true,
            player_name: "Mivik".to_string(),
            player_rks: 15.,
            preferred_sample_rate: None,
            res_pack_path: None,
            sample_count: 1,
            show_acc: false,
            show_avg_fps: false,
            speed: 1.,
            touch_debug: false,
            use_keyboard: false,
            volume_music: 1.,
            volume_sfx: 1.,
            volume_bgm: 1.,

            autoplay: None,
        }
    }
}

// 反序列化后的收尾处理，以及热路径上的修饰符查询快捷方法。
impl Config {
    /// 反序列化完成后必须调用一次：做兼容迁移并施加平台相关修正。
    pub fn init(&mut self) {
        // 把旧版的 `autoplay: Option<bool>` 折叠进 `mods` 位标志。
        // 使用 `set(flag, value)` 而不是 `insert`，这样旧配置里显式的 `false`
        // 也能正确清除该位；字段本身不做清理，保持结构体布局稳定。
        if let Some(flag) = self.autoplay {
            self.mods.set(Mods::AUTOPLAY, flag);
        }
        // OpenHarmony 平台的 Maloon GPU 性能极差，MSAA 会严重拖慢帧率，
        // 因此无论用户配置如何都强制把采样数固定为 1。
        #[cfg(target_env = "ohos")]
        {
            // Due to the fucking poor performance of the Maloon GPU, the sample count must be set to 1.
            self.sample_count = 1;
        }
    }

    /// 是否启用了指定修饰符 `m`。每次渲染都会调用，加上 `#[inline]` 消除函数调用开销。
    #[inline]
    pub fn has_mod(&self, m: Mods) -> bool {
        self.mods.contains(m)
    }

    /// 本局是否处于自动演奏状态（[`Mods::AUTOPLAY`] 的语义化快捷判断）。
    #[inline]
    pub fn autoplay(&self) -> bool {
        self.has_mod(Mods::AUTOPLAY)
    }

    /// 本局是否水平镜像画面（[`Mods::FLIP_X`] 的语义化快捷判断）。
    #[inline]
    pub fn flip_x(&self) -> bool {
        self.has_mod(Mods::FLIP_X)
    }
}
