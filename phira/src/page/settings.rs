//! 设置页面。
//!
//! 用左侧竖排选项卡 [`Tabs`] 在五个分栏间切换：通用 / 音频 / 谱面 / 调试 / 关于；
//! 每个分栏是一个独立结构体（[`GeneralList`] 等），各自持有控件并直接读写
//! 全局 `Data` / `Config` 里的字段。
//!
//! 持久化策略是「延时合并写」：任一设置变化后把 `save_time` 记为该帧时间，
//! 静默 [`SettingsPage::SAVE_TIME`] 秒后才真正 `save_data()`，
//! 从而避免拖动滑块时每帧写盘；离开页面（[`Page::exit`]）时若仍有未落盘的改动则补写。

prpr_l10n::tl_file!("settings");

use super::{NextPage, OffsetPage, Page, SharedState};
use crate::{
    dir, get_data, get_data_mut,
    popup::ChooseButton,
    save_data,
    scene::BGM_VOLUME_UPDATED,
    sync_data,
    tabs::{Tabs, TitleFn},
};
use anyhow::Result;
use bytesize::ByteSize;
use inputbox::InputBox;
use macroquad::prelude::*;
use once_cell::sync::Lazy;
use prpr::{
    core::BOLD_FONT,
    ext::{open_url, poll_future, semi_white, LocalTask, RectExt, SafeTexture},
    scene::{request_input, return_input, show_error, show_message, take_input},
    task::Task,
    ui::{DRectButton, Scroll, Slider, Ui, PREFER_REDUCED_MOTION, UI_SFX_VOLUME},
};
use prpr_l10n::{LanguageIdentifier, LANG_IDENTS, LANG_NAMES};
use reqwest::Url;
use serde::Deserialize;
use std::{borrow::Cow, fs, io, path::PathBuf, sync::atomic::Ordering};

// 设置项的行高（归一化）：各分栏列表以它为栅格基准，`item!` 宏据此推进纵向偏移。
const ITEM_HEIGHT: f32 = 0.15;
// 右侧交互控件（开关/按钮/滑块）的基准宽度。
const INTERACT_WIDTH: f32 = 0.26;
// 服务器状态查询页；点击「检查状态」时用系统浏览器打开。
const STATUS_PAGE: &str = "https://status.phira.cn";

/// 把 YAML 里的「姓名数组」折叠成逗号分隔的单个字符串。
///
/// 用于展示制作人员名单——`staff.yml` 中每位成员的字段可能写成列表。
struct NameList(String);
// 自定义反序列化：源数据是字符串数组，这里在解析阶段直接 join 成展示用文本。
impl<'de> Deserialize<'de> for NameList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = Vec::<String>::deserialize(deserializer)?;
        Ok(Self(s.join(", ")))
    }
}

/// 本地化贡献者名单的原始结构：按语种分开，键名是 locale 标识符。
///
/// 每个字段都是 [`NameList`]，展示时会被 join 成一行。
#[derive(Deserialize)]
struct LocalizationListRaw {
    /// 英语（en-US）贡献者。
    #[serde(rename = "en-US")]
    en_us: NameList,
    /// 法语（fr-FR）贡献者。
    #[serde(rename = "fr-FR")]
    fr_fr: NameList,
    /// 德语（de-DE）贡献者。
    #[serde(rename = "de-DE")]
    de_de: NameList,
    /// 印尼语（id-ID）贡献者。
    #[serde(rename = "id-ID")]
    id_id: NameList,
    /// 日语（ja-JP）贡献者。
    #[serde(rename = "ja-JP")]
    ja_jp: NameList,
    /// 韩语（ko-KR）贡献者。
    #[serde(rename = "ko-KR")]
    ko_kr: NameList,
    /// 波兰语（pl-PL）贡献者。
    #[serde(rename = "pl-PL")]
    pl_pl: NameList,
    /// 葡萄牙语（pt-BR）贡献者。
    #[serde(rename = "pt-BR")]
    pt_br: NameList,
    /// 俄语（ru-RU）贡献者。
    #[serde(rename = "ru-RU")]
    ru_ru: NameList,
    /// 泰语（th-TH）贡献者。
    #[serde(rename = "th-TH")]
    th_th: NameList,
    /// 繁体中文（zh-TW）贡献者。
    #[serde(rename = "zh-TW")]
    zh_tw: NameList,
    /// 土耳其语（tr-TR）贡献者。
    #[serde(rename = "tr-TR")]
    tr_tr: NameList,
    /// 越南语（vi-VN）贡献者。
    #[serde(rename = "vi-VN")]
    vi_vn: NameList,
}

/// 把 [`LocalizationListRaw`] 的各语种字段格式化为多行展示文本。
struct LocalizationList(String);
// 自定义反序列化：先解析原始结构，再拼成「语言名\n贡献者」逐行排列的文本。
impl<'de> Deserialize<'de> for LocalizationList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = LocalizationListRaw::deserialize(deserializer)?;
        Ok(Self(format!(
            "\
English (en-US)\n{}\n
French (fr-FR)\n{}\n
German (de-DE)\n{}\n
Indonesian (id-ID)\n{}\n
Japanese (ja-JP)\n{}\n
Korean (ko-KR)\n{}\n
Polish (pl-PL)\n{}\n
Portuguese (pt-BR)\n{}\n
Russian (ru-RU)\n{}\n
Thai (th-TH)\n{}\n
Traditional Chinese (zh-TW)\n{}\n
Turkish (tr-TR)\n{}\n
Vietnamese (vi-VN)\n{}",
            raw.en_us.0,
            raw.fr_fr.0,
            raw.de_de.0,
            raw.id_id.0,
            raw.ja_jp.0,
            raw.ko_kr.0,
            raw.pl_pl.0,
            raw.pt_br.0,
            raw.ru_ru.0,
            raw.th_th.0,
            raw.zh_tw.0,
            raw.tr_tr.0,
            raw.vi_vn.0
        )))
    }
}

/// 制作人员名单，来自编译期嵌入的 `staff.yml`。
#[derive(Deserialize)]
struct StaffList {
    /// 程序开发。
    development: NameList,
    /// 运营。
    operations: NameList,
    /// 文档。
    documentation: NameList,
    /// 美术。
    art: NameList,
    /// 曲目。
    music: NameList,
    /// 音频。
    audio: NameList,
    /// 社区。
    community: NameList,
    /// 本地化（多语种）。
    localization: LocalizationList,
}

// 编译期把 `staff.yml` 嵌入二进制，首次访问时解析；解析失败属于打包错误，直接 panic 更利于暴露。
static STAFF_LIST: Lazy<StaffList> = Lazy::new(|| {
    let data = include_str!("../../staff.yml");
    serde_yaml::from_str(data).unwrap()
});

/// 设置页的五个分栏。
///
/// 作为 [`Tabs`] 的泛型参数：选项卡在左侧竖排，选中值决定渲染哪个 `xxxList`。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingListType {
    /// 通用：语言、离线、联机、缓存、无障碍与网络等全局选项。
    General,
    /// 音频：音量、判定偏移校准、采样率等。
    Audio,
    /// 谱面：游玩相关的显示与操作偏好。
    Chart,
    /// 调试：面向开发者的谱面/触控调试开关。
    Debug,
    /// 关于：版本号与制作人员名单。
    About,
}

/// 设置页状态。
///
/// 四个分栏各持有一个子列表对象（状态 + 控件）；`About` 无状态，直接由函数渲染。
pub struct SettingsPage {
    /// 通用分栏。
    list_general: GeneralList,
    /// 音频分栏。
    list_audio: AudioList,
    /// 谱面分栏。
    list_chart: ChartList,
    /// 调试分栏。
    list_debug: DebugList,

    /// 左侧竖排选项卡，选中项即当前分栏。
    tabs: Tabs<SettingListType>,

    /// 右侧内容区的滚动容器（设置项可能超出一屏）。
    scroll: Scroll,
    /// 上次设置变化的时间戳；用于延时合并写盘（见 [`SettingsPage::SAVE_TIME`]）。
    save_time: f32,

    /// 「关于」页展示的应用图标。
    icon: SafeTexture,
}

// 设置页的构造与常量。
impl SettingsPage {
    /// 设置变化后延迟写盘的合并窗口（秒）。拖动滑块时避免每帧落盘。
    const SAVE_TIME: f32 = 0.5;

    /// 构造设置页。
    ///
    /// # Arguments
    /// * `icon` - 「关于」页展示的应用图标。
    /// * `icon_lang` - 语言设置项旁展示的图标。
    pub fn new(icon: SafeTexture, icon_lang: SafeTexture) -> Self {
        Self {
            list_general: GeneralList::new(icon_lang),
            list_audio: AudioList::new(),
            list_chart: ChartList::new(),
            list_debug: DebugList::new(),

            tabs: Tabs::new([
                (SettingListType::General, || tl!("general")),
                (SettingListType::Audio, || tl!("audio")),
                (SettingListType::Chart, || tl!("chart")),
                (SettingListType::Debug, || tl!("debug")),
                (SettingListType::About, || tl!("about")),
            ] as [(SettingListType, TitleFn); 5]),

            scroll: Scroll::new(),
            save_time: f32::INFINITY,

            icon,
        }
    }
}

// 设置页的页面钩子约定：
// - `touch`：按「吸顶控件 → 选项卡 → 滚动区 → 当前分栏」的优先级分发；
// - `update`：推进各分栏控件，并在静默 `SAVE_TIME` 后统一写盘；
// - `render`：只渲染当前选中的分栏内容；
// - `next_page`：透传音频分栏产生的子页面（校准向导）。
impl Page for SettingsPage {
    /// 页面标识。
    fn label(&self) -> Cow<'static, str> {
        tl!("label")
    }

    /// 离开设置页：通知主场景重新读取 BGM 音量，并补写尚未落盘的设置。
    fn exit(&mut self) -> Result<()> {
        // BGM 音量改动是惰性生效的，退出时统一通知主场景刷新音量。
        BGM_VOLUME_UPDATED.store(true, Ordering::Relaxed);
        // `is_finite` 说明还有未到期的写盘计划，立即补写以免丢失。
        if self.save_time.is_finite() {
            save_data()?;
        }
        Ok(())
    }

    /// 触摸分派：吸顶控件 > 选项卡 > 滚动区 > 当前分栏。
    ///
    /// # Returns
    /// `Ok(true)` = 已被本页消费；`Ok(false)` = 未命中任何控件。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        let t = s.t;
        // 阶段 1：先让当前分栏处理「吸顶」控件（如语言选择弹层）——它们浮在内容之上，需优先响应。
        if match self.tabs.selected() {
            SettingListType::General => self.list_general.top_touch(touch, t),
            SettingListType::Audio => self.list_audio.top_touch(touch, t),
            SettingListType::Chart => self.list_chart.top_touch(touch, t),
            SettingListType::Debug => self.list_debug.top_touch(touch, t),
            SettingListType::About => false,
        } {
            return Ok(true);
        }

        // 阶段 2：选项卡切换（动画用真实时间 `rt` 驱动）与滚动区拖动。
        if self.tabs.touch(touch, s.rt) {
            return Ok(true);
        }

        if self.scroll.touch(touch, t) {
            return Ok(true);
        }
        // 阶段 3：分栏控件的触摸。三态返回值 `Some(true)`=值已变（安排延时写盘），
        // `Some(false)`=吞掉触摸但值未变，`None`=不相关。
        if let Some(p) = match self.tabs.selected() {
            SettingListType::General => self.list_general.touch(touch, t)?,
            SettingListType::Audio => self.list_audio.touch(touch, t)?,
            SettingListType::Chart => self.list_chart.touch(touch, t)?,
            SettingListType::Debug => self.list_debug.touch(touch, t)?,
            SettingListType::About => None,
        } {
            if p {
                self.save_time = t;
            }
            // 控件交互时停止滚动惯性，避免拖动滑块时页面继续滑动。
            self.scroll.y_scroller.halt();
            return Ok(true);
        }
        Ok(false)
    }

    /// 推进当前分栏与滚动区的状态，并在静默后写盘。
    fn update(&mut self, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        // 各分栏返回 `true` 表示本帧有设置项被修改。
        let changed = match self.tabs.selected() {
            SettingListType::General => self.list_general.update(t)?,
            SettingListType::Audio => self.list_audio.update(t)?,
            SettingListType::Chart => self.list_chart.update(t)?,
            SettingListType::Debug => self.list_debug.update(t)?,
            SettingListType::About => false,
        };
        self.scroll.update(t);
        if changed {
            self.save_time = t;
        }
        // 合并写：距上次变化超过 SAVE_TIME 才真正落盘，随后把计时器重置为无穷（表示无写盘计划）。
        if t > self.save_time + Self::SAVE_TIME {
            save_data()?;
            self.save_time = f32::INFINITY;
        }
        Ok(())
    }

    /// 渲染当前分栏：内容区套在滚动容器里，整体再套一层页面淡入。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        let rt = s.rt;

        // 阶段 1：页面淡入包裹层。
        s.fader.render(ui, s.t, |ui| {
            let r = ui.content_rect();
            // 阶段 2：左侧选项卡 + 右侧内容；`item` 是当前选中的分栏枚举。
            self.tabs.render(ui, rt, r, |ui, item| {
                let r = r.feather(-0.01);
                self.scroll.size((r.w, r.h));
                // 阶段 3：把坐标系原点移到内容区左上角，使滚动只影响内容、不影响选项卡。
                ui.scope(|ui| {
                    ui.dx(r.x);
                    ui.dy(r.y);
                    self.scroll.render(ui, |ui| match item {
                        SettingListType::General => self.list_general.render(ui, r, t),
                        SettingListType::Audio => self.list_audio.render(ui, r, t),
                        SettingListType::Chart => self.list_chart.render(ui, r, t),
                        SettingListType::Debug => self.list_debug.render(ui, r, t),
                        SettingListType::About => render_about(ui, r, &self.icon),
                    });
                });

                Ok(())
            })
        })?;

        Ok(())
    }

    /// 透传音频分栏产生的子页面（如判定偏移校准向导）。
    fn next_page(&mut self) -> NextPage {
        if matches!(self.tabs.selected(), SettingListType::Audio) {
            return self.list_audio.next_page().unwrap_or_default();
        }
        NextPage::None
    }
}

/// 渲染「关于」页：应用图标、版本号（含 git hash）与制作人员名单。
///
/// 用局部矩形（从 0,0 起）自行排版，避免受传入矩形位置影响；
/// 返回 `(内容宽, 内容高)` 供滚动容器定位。
fn render_about(ui: &mut Ui, mut r: Rect, icon: &SafeTexture) -> (f32, f32) {
    r.x = 0.;
    r.y = 0.;
    let ow = r.w;
    let r = r.feather(-0.02);

    // 图标：以内容中心为水平基准，绘制边长 0.1 的圆角方块。
    let ct = r.center();
    let s = 0.1;
    let ir = Rect::new(ct.x - s, r.y + 0.05, s * 2., s * 2.);
    ui.fill_path(&ir.rounded(0.02), (**icon, ir));

    // 名单来自编译期嵌入的 staff.yml；版本串附带构建时的 git hash。
    let staff = &*STAFF_LIST;
    let text = tl!(
        "about-content",
        "version" => format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("GIT_HASH")),

        "development" => &staff.development.0,
        "operations" => &staff.operations.0,
        "documentation" => &staff.documentation.0,
        "art" => &staff.art.0,
        "music" => &staff.music.0,
        "audio" => &staff.audio.0,
        "community" => &staff.community.0,
        "localization" => &staff.localization.0
    );
    // 首行固定为版本信息，用加粗字体单独绘制；其余为多行名单。
    let (first, text) = text.split_once('\n').unwrap();
    let tr = ui
        .text(first)
        .pos(ct.x, ir.bottom() + 0.03)
        .anchor(0.5, 0.)
        .size(0.6)
        .draw_using(&BOLD_FONT);

    let r = ui
        .text(text.trim())
        .pos(r.x, tr.bottom() + 0.06)
        .size(0.55)
        .multiline()
        .max_width(r.w)
        .h_center()
        .draw();

    (ow, r.bottom() + 0.03)
}

/// 渲染一行设置项的标题（可带副标题），返回标题右边界的 x 坐标。
///
/// 带副标题时标题与副标题在一行高度内上下居中堆叠；副标题用较淡颜色且限宽换行。
/// 返回值供调用方在其右侧摆放交互控件。
fn render_title<'a>(ui: &mut Ui, title: impl Into<Cow<'a, str>>, subtitle: Option<Cow<'a, str>>) -> f32 {
    // 字号/左边距/行间距/副标题最大宽度：整页排版只需在此集中调整。
    const TITLE_SIZE: f32 = 0.6;
    const SUBTITLE_SIZE: f32 = 0.35;
    const LEFT: f32 = 0.06;
    const PAD: f32 = 0.01;
    const SUB_MAX_WIDTH: f32 = 1.4;
    if let Some(subtitle) = subtitle {
        let title = title.into();
        let r1 = ui.text(Cow::clone(&title)).size(TITLE_SIZE).measure();
        let r2 = ui
            .text(Cow::clone(&subtitle))
            .size(SUBTITLE_SIZE)
            .max_width(SUB_MAX_WIDTH)
            .no_baseline()
            .measure();
        let h = r1.h + PAD + r2.h;
        let r1 = ui
            .text(subtitle)
            .pos(LEFT, (ITEM_HEIGHT + h) / 2.)
            .anchor(0., 1.)
            .size(SUBTITLE_SIZE)
            .max_width(SUB_MAX_WIDTH)
            .color(semi_white(0.6))
            .draw()
            .right();
        let r2 = ui
            .text(title)
            .pos(LEFT, (ITEM_HEIGHT - h) / 2.)
            .no_baseline()
            .size(TITLE_SIZE)
            .draw()
            .right();
        r1.max(r2)
    } else {
        ui.text(title.into())
            .pos(LEFT, ITEM_HEIGHT / 2.)
            .anchor(0., 0.5)
            .no_baseline()
            .size(TITLE_SIZE)
            .draw()
            .right()
    }
}

/// 绘制一个开关控件：按布尔值显示「开/关」文案，并负责命中反馈与点击音效。
///
/// `t` 为动画时间，`on` 同时决定文案与选中态配色。
#[inline]
fn render_switch(ui: &mut Ui, r: Rect, t: f32, btn: &mut DRectButton, on: bool) {
    btn.render_text(ui, r, t, if on { ttl!("switch-on") } else { ttl!("switch-off") }, 0.5, on);
}

/// 计算设置项右侧交互控件的矩形：宽度为 [`INTERACT_WIDTH`]，高度取行高的 2/3 并垂直居中。
#[inline]
fn right_rect(w: f32) -> Rect {
    let rh = ITEM_HEIGHT * 2. / 3.;
    Rect::new(w - 0.3, (ITEM_HEIGHT - rh) / 2., INTERACT_WIDTH, rh)
}

/// 通用分栏的状态与控件。
struct GeneralList {
    /// 语言设置项旁展示的图标。
    icon_lang: SafeTexture,

    /// 语言选择按钮，点击弹出语言列表。选中项映射到 `Data::language`。
    lang_btn: ChooseButton,

    /// 全屏开关（对应 `Config::fullscreen_mode`）。仅 Windows/Linux 桌面端提供，移动端由系统管理。
    #[cfg(all(any(target_os = "windows", target_os = "linux"), not(target_env = "ohos")))]
    fullscreen_btn: DRectButton,

    /// 清除缓存按钮（删除缓存目录并重新统计）。
    cache_btn: DRectButton,
    /// 离线模式开关（对应 `Config::offline_mode`）。
    offline_btn: DRectButton,
    /// 跳转服务器状态页的按钮。
    server_status_btn: DRectButton,
    /// 联机功能开关（对应 `Config::mp_enabled`）。
    mp_btn: DRectButton,
    /// 联机服务器地址编辑按钮（对应 `Config::mp_address`）。
    mp_addr_btn: DRectButton,
    /// 低画质开关（在 `Config::sample_count` 的 1/2 间切换）；OpenHarmony 强制关闭抗锯齿故不显示。
    #[cfg(not(target_env = "ohos"))]
    lowq_btn: DRectButton,
    /// 减少动效（无障碍）开关（对应 `Data::prefer_reduced_motion`）。
    prefer_reduced_motion_btn: DRectButton,
    /// 接受无效 HTTPS 证书开关（对应 `Data::accept_invalid_cert`）。
    insecure_btn: DRectButton,
    /// 自定义网关（anys）开关（对应 `Data::enable_anys`）。
    enable_anys_btn: DRectButton,
    /// 自定义网关地址编辑按钮（对应 `Data::anys_gateway`）。
    anys_gateway_btn: DRectButton,

    /// 缓存目录当前大小（字节）；`None` 表示尚未算出或在计算中。
    cache_size: Option<u64>,
    /// 缓存大小统计任务。
    cache_task: Option<Task<Result<u64>>>,
}

// 通用分栏：构造、目录统计与交互。
impl GeneralList {
    /// 构造通用分栏；语言选择器的初始选中项由已保存的语言标识反查得出。
    ///
    /// # Arguments
    /// * `icon_lang` - 语言项旁展示的图标。
    pub fn new(icon_lang: SafeTexture) -> Self {
        let mut this = Self {
            icon_lang,

            // 语言列表与 `LANG_IDENTS` 一一对应；用已保存的语言标识反查下标作为初始选中项，
            // 若未设置或解析失败则退回第 0 项（默认语言）。
            lang_btn: ChooseButton::new()
                .with_options(LANG_NAMES.iter().map(|s| s.to_string()).collect())
                .with_selected(
                    get_data()
                        .language
                        .as_ref()
                        .and_then(|it| it.parse::<LanguageIdentifier>().ok())
                        .and_then(|ident| LANG_IDENTS.iter().position(|it| *it == ident))
                        .unwrap_or_default(),
                ),

            #[cfg(all(any(target_os = "windows", target_os = "linux"), not(target_env = "ohos")))]
            fullscreen_btn: DRectButton::new(),

            cache_btn: DRectButton::new(),
            offline_btn: DRectButton::new(),
            server_status_btn: DRectButton::new(),
            mp_btn: DRectButton::new(),
            mp_addr_btn: DRectButton::new(),
            #[cfg(not(target_env = "ohos"))]
            lowq_btn: DRectButton::new(),
            prefer_reduced_motion_btn: DRectButton::new(),
            insecure_btn: DRectButton::new(),
            enable_anys_btn: DRectButton::new(),
            anys_gateway_btn: DRectButton::new(),

            cache_size: None,
            cache_task: None,
        };
        // 构造后异步统计一次缓存目录大小（不阻塞构造过程）。
        let _ = this.update_cache_size();
        this
    }

    /// 处理「吸顶」控件的触摸（目前只有语言选择弹层）。
    pub fn top_touch(&mut self, touch: &Touch, t: f32) -> bool {
        if self.lang_btn.top_touch(touch, t) {
            return true;
        }
        false
    }

    /// 递归统计目录占用的总字节数。
    ///
    /// # Errors
    /// 遍历目录或读取元数据失败时返回 IO 错误。
    fn dir_size(path: impl Into<PathBuf>) -> io::Result<u64> {
        fn inner(mut dir: fs::ReadDir) -> io::Result<u64> {
            dir.try_fold(0, |acc, file| {
                let file = file?;
                // 遇目录递归进入累加，遇普通文件累加其字节长度。
                let size = match file.metadata()? {
                    data if data.is_dir() => inner(fs::read_dir(file.path())?)?,
                    data => data.len(),
                };
                Ok(acc + size)
            })
        }

        inner(fs::read_dir(path.into())?)
    }

    /// 触发一次缓存目录大小统计（先清空旧值，结果由 [`GeneralList::update`] 收尾）。
    ///
    /// # Errors
    /// 解析缓存目录路径失败时返回错误。
    fn update_cache_size(&mut self) -> Result<()> {
        self.cache_size = None;

        // 目录遍历放到后台任务里跑，避免大缓存目录卡住 UI 线程。
        let cache_dir = dir::cache()?;
        self.cache_task = Some(Task::new(async { Ok(Self::dir_size(cache_dir)?) }));
        Ok(())
    }

    /// 处理通用分栏的触摸。
    ///
    /// # Returns
    /// `Some(true)` = 设置值已改变（需要保存）；`Some(false)` = 已消费但无需保存
    /// （如打开页面、清缓存）；`None` = 未命中任何控件。
    ///
    /// # Errors
    /// 清除缓存等文件操作失败时上抛。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> Result<Option<bool>> {
        let data = get_data_mut();
        let config = &mut data.config;
        // 语言按钮只负责弹出选择层，真正的值更新在 `update` 里通过 `changed()` 检测。
        if self.lang_btn.touch(touch, t) {
            return Ok(Some(false));
        }

        // 全屏开关：立即应用到窗口，无需重启。
        #[cfg(all(any(target_os = "windows", target_os = "linux"), not(target_env = "ohos")))]
        if self.fullscreen_btn.touch(touch, t) {
            config.fullscreen_mode ^= true;

            macroquad::window::set_fullscreen(config.fullscreen_mode);

            return Ok(Some(true));
        }

        // 清空缓存目录并重新统计大小；仅弹一条提示，不需要保存设置。
        if self.cache_btn.touch(touch, t) {
            fs::remove_dir_all(dir::cache()?)?;
            self.update_cache_size()?;
            show_message(tl!("item-cache-cleared")).ok();
            return Ok(Some(false));
        }
        // 离线模式：开关立即生效，网络请求处读取该字段后跳过。
        if self.offline_btn.touch(touch, t) {
            config.offline_mode ^= true;
            return Ok(Some(true));
        }
        // 服务器状态页为纯跳转，不改变设置，但仍返回 `Some(true)` 以阻止触摸穿透。
        if self.server_status_btn.touch(touch, t) {
            let _ = open_url(STATUS_PAGE);
            return Ok(Some(true));
        }
        // 联机总开关与服务器地址。地址经输入框异步录入，提交结果在 `update` 中按 id 解析校验。
        if self.mp_btn.touch(touch, t) {
            config.mp_enabled ^= true;
            return Ok(Some(true));
        }
        if self.mp_addr_btn.touch(touch, t) {
            request_input("mp_addr", InputBox::new().default_text(&config.mp_address));
            return Ok(Some(true));
        }
        // 低画质开关在 MSAA 采样数 1/2 间切换（移动 GPU 上 2 已是常见上限）。
        #[cfg(not(target_env = "ohos"))]
        if self.lowq_btn.touch(touch, t) {
            config.sample_count = if config.sample_count == 1 { 2 } else { 1 };
            return Ok(Some(true));
        }
        // 减少动效：即时写入全局原子量，引擎当帧即跳过过渡动画（无需重建界面）。
        if self.prefer_reduced_motion_btn.touch(touch, t) {
            data.prefer_reduced_motion ^= true;
            PREFER_REDUCED_MOTION.store(data.prefer_reduced_motion, Ordering::Relaxed);
            return Ok(Some(true));
        }
        // 接受无效证书：仅供自签服务器/抓包调试，下一次发起请求时生效。
        if self.insecure_btn.touch(touch, t) {
            data.accept_invalid_cert ^= true;
            return Ok(Some(true));
        }
        // 自定义网关开关与地址；地址同样走输入框，在 `update` 中以 URL 解析校验。
        if self.enable_anys_btn.touch(touch, t) {
            data.enable_anys ^= true;
            return Ok(Some(true));
        }
        if self.anys_gateway_btn.touch(touch, t) {
            request_input("anys_gateway", InputBox::new().default_text(&data.anys_gateway));
            return Ok(Some(true));
        }
        Ok(None)
    }

    /// 推进通用分栏状态：检测语言切换、消费输入框提交、收尾缓存统计。
    ///
    /// # Returns
    /// `Ok(true)` = 有设置被修改（需要安排写盘）。
    pub fn update(&mut self, t: f32) -> Result<bool> {
        self.lang_btn.update(t);
        let data = get_data_mut();
        // 语言变更会即时 `sync_data` 重载文案，无需重启。
        if self.lang_btn.changed() {
            data.language = Some(LANG_IDENTS[self.lang_btn.selected()].to_string());
            sync_data();
            return Ok(true);
        }
        if let Some((id, text)) = take_input() {
            // 联机地址要求是合法的 `host:port`（Authority），非法则报错并保留旧值。
            if id == "mp_addr" {
                if let Err(err) = text.parse::<http::uri::Authority>() {
                    show_error(anyhow::Error::new(err).context(tl!("item-mp-addr-invalid")));
                    return Ok(false);
                } else {
                    data.config.mp_address = text;
                    return Ok(true);
                }
            } else if id == "anys_gateway" {
                // 网关地址需为合法 URL；统一去掉尾部 `/`，避免拼接路径时出现双斜杠。
                if let Err(err) = Url::parse(&text) {
                    show_error(anyhow::Error::new(err).context(tl!("item-anys-gateway-invalid")));
                    return Ok(false);
                } else {
                    data.anys_gateway = text.trim_end_matches('/').to_string();
                    return Ok(true);
                }
            } else {
                // 非本分栏负责的输入框：原样退回，交给其它页面处理。
                return_input(id, text);
            }
        }
        // 缓存统计任务收尾；失败时把大小显示为未知（`None`）。
        if let Some(task) = &mut self.cache_task {
            if let Some(size) = task.take() {
                self.cache_size = size.ok();
                self.cache_task = None;
            }
        }
        Ok(false)
    }

    /// 渲染通用分栏的列表，返回 `(内容宽, 内容高)`。
    ///
    /// 用局部 `item!` 宏逐行推进纵向偏移；每行 = 左侧标题（可带副标题）+ 右侧交互控件。
    pub fn render(&mut self, ui: &mut Ui, r: Rect, t: f32) -> (f32, f32) {
        let w = r.w;
        let mut h = 0.;
        // 逐行宏：执行一段渲染后按 ITEM_HEIGHT 下移光标并累加内容高度。
        macro_rules! item {
            ($($b:tt)*) => {{
                $($b)*
                ui.dy(ITEM_HEIGHT);
                h += ITEM_HEIGHT;
            }}
        }
        let rr = right_rect(w);

        let data = get_data();
        let config = &data.config;
        // 语言：标题后的小图标仅作装饰，真实交互由右侧 ChooseButton 弹层完成。
        item! {
            let rt = render_title(ui, tl!("item-lang"), None);
            let w = 0.06;
            let r = Rect::new(rt + 0.01, (ITEM_HEIGHT - w) / 2., w, w);
            ui.fill_rect(r, (*self.icon_lang, r));
            self.lang_btn.render(ui, rr, t);
        }

        // 全屏（仅桌面端显示）。
        #[cfg(all(any(target_os = "windows", target_os = "linux"), not(target_env = "ohos")))]
        item! {
            render_title(ui, tl!("item-fullscreen"), None);
            render_switch(ui, rr, t, &mut self.fullscreen_btn, config.fullscreen_mode);
        }

        // 离线模式：标题带副标题说明影响范围。
        item! {
            render_title(ui, tl!("item-offline"), Some(tl!("item-offline-sub")));
            render_switch(ui, rr, t, &mut self.offline_btn, config.offline_mode);
        }
        // 服务器状态：按钮文案固定为「检查状态」，点击后由系统浏览器打开。
        item! {
            render_title(ui, tl!("item-server-status"), Some(tl!("item-server-status-sub")));
            self.server_status_btn.render_text(ui, rr, t, tl!("check-status"), 0.5, true);
        }
        // 联机开关与地址（地址文本直接作为按钮文案，字号更小以免溢出）。
        item! {
            render_title(ui, tl!("item-mp"), Some(tl!("item-mp-sub")));
            render_switch(ui, rr, t, &mut self.mp_btn, config.mp_enabled);
        }
        item! {
            render_title(ui, tl!("item-mp-addr"), Some(tl!("item-mp-addr-sub")));
            self.mp_addr_btn.render_text(ui, rr, t, &config.mp_address, 0.4, false);
        }
        // 减少动效：无障碍选项，对应 `data.prefer_reduced_motion`。
        item! {
            render_title(ui, tl!("item-prefer-reduced-motion"), Some(tl!("item-prefer-reduced-motion-sub")));
            render_switch(ui, rr, t, &mut self.prefer_reduced_motion_btn, data.prefer_reduced_motion);
        }
        // 低画质：勾选态即「采样数 == 1」。
        #[cfg(not(target_env = "ohos"))]
        item! {
            render_title(ui, tl!("item-lowq"), Some(tl!("item-lowq-sub")));
            render_switch(ui, rr, t, &mut self.lowq_btn, config.sample_count == 1);
        }
        // 清除缓存：副标题实时显示当前缓存大小（未算出时显示「计算中」）。
        item! {
            let cache_size = if let Some(size) = self.cache_size {
                Cow::Owned(tl!("item-cache-size", "size" => ByteSize(size).to_string()))
            } else {
                tl!("item-cache-size-loading")
            };
            render_title(ui, tl!("item-clear-cache"), Some(cache_size));
            self.cache_btn.render_text(ui, rr, t, tl!("item-clear-cache-btn"), 0.5, true);
        }
        // 插入 0.04 的空行，把「网络/安全」等进阶项与常规项在视觉上分组。
        ui.dy(0.04);
        h += 0.04;
        // 进阶网络项：接受无效证书 / 启用自定义网关 / 网关地址。
        item! {
            render_title(ui, tl!("item-insecure"), Some(tl!("item-insecure-sub")));
            render_switch(ui, rr, t, &mut self.insecure_btn, data.accept_invalid_cert);
        }
        item! {
            render_title(ui, tl!("item-enable-anys"), Some(tl!("item-enable-anys-sub")));
            render_switch(ui, rr, t, &mut self.enable_anys_btn, data.enable_anys);
        }
        item! {
            render_title(ui, tl!("item-anys-gateway"), Some(tl!("item-anys-gateway-sub")));
            self.anys_gateway_btn.render_text(ui, rr, t, &data.anys_gateway, 0.4, false);
        }
        // 语言弹层是「吸顶」覆盖层，必须最后绘制以浮在列表之上。
        self.lang_btn.render_top(ui, t, 1.);
        (w, h)
    }
}

/// 音频分栏的状态与控件。
struct AudioList {
    /// 音画自动对齐开关（对应 `Config::adjust_time`）。
    adjust_btn: DRectButton,
    /// 谱面音乐音量滑块（对应 `Config::volume_music`），范围 0.0..2.0。
    music_slider: Slider,
    /// 音效音量滑块（对应 `Config::volume_sfx`），范围 0.0..2.0。
    sfx_slider: Slider,
    /// 背景（菜单）音乐音量滑块（对应 `Config::volume_bgm`），范围 0.0..2.0。
    bgm_slider: Slider,
    /// 判定偏移校准入口按钮（展示 `Config::offset`，单位为毫秒）。
    cali_btn: DRectButton,
    /// 期望采样率切换按钮（对应 `Config::preferred_sample_rate`）；Android 平台不提供。
    #[cfg(not(target_os = "android"))]
    preferred_sample_rate_btn: DRectButton,
    /// 音频缓冲区大小切换按钮（对应 `Config::audio_buffer_size`）；仅 OpenHarmony 需要。
    #[cfg(target_env = "ohos")]
    audio_buffer_size_btn: DRectButton,
    /// 校准向导页面的异步加载任务（持有结果，供 `next_page` 取用）。
    cali_task: LocalTask<Result<OffsetPage>>,
    /// 校准向导产生的子页面。
    next_page: Option<NextPage>,
}

// 音频分栏：音量、判定偏移校准与采样率/缓冲设置。
impl AudioList {
    /// 构造音频分栏控件。
    ///
    /// 三个音量滑块范围统一为 `0.0..=2.0`（允许放大到 2 倍）、步长 `0.05`。
    pub fn new() -> Self {
        Self {
            adjust_btn: DRectButton::new(),
            music_slider: Slider::new(0.0..2.0, 0.05),
            sfx_slider: Slider::new(0.0..2.0, 0.05),
            bgm_slider: Slider::new(0.0..2.0, 0.05),
            cali_btn: DRectButton::new(),
            #[cfg(not(target_os = "android"))]
            preferred_sample_rate_btn: DRectButton::new(),
            #[cfg(target_env = "ohos")]
            audio_buffer_size_btn: DRectButton::new(),

            cali_task: None,
            next_page: None,
        }
    }

    /// 音频分栏没有吸顶控件。
    pub fn top_touch(&mut self, _touch: &Touch, _t: f32) -> bool {
        false
    }

    /// 处理音频分栏的触摸。
    ///
    /// # Returns
    /// 三态协议：`Some(true)`=值已变，`Some(false)`=已消费但无需保存，`None`=未命中。
    ///
    /// # Errors
    /// 校准向导页面构造失败时上抛。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> Result<Option<bool>> {
        let data = get_data_mut();
        let config = &mut data.config;
        // `adjust_time` 即时生效：开启后时间管理器会把游戏时间缓慢回对齐音乐实际播放位置。
        if self.adjust_btn.touch(touch, t) {
            config.adjust_time ^= true;
            return Ok(Some(true));
        }
        // 谱面音乐音量：仅写入配置，播放侧读取（无需重建音频后端）。
        if let wt @ Some(_) = self.music_slider.touch(touch, t, &mut config.volume_music) {
            return Ok(wt);
        }
        // 音效音量：除写入配置外，还即时更新全局 `UI_SFX_VOLUME`（存 f32 位模式），
        // 使界面点击音效当帧就采用新音量。
        if let wt @ Some(_) = self.sfx_slider.touch(touch, t, &mut config.volume_sfx) {
            UI_SFX_VOLUME.store(config.volume_sfx.to_bits(), Ordering::Relaxed);
            return Ok(wt);
        }
        // BGM 音量变化超过阈值才置更新标志，避免拖动过程中的微小抖动频繁触发主场景换音量。
        let old = config.volume_bgm;
        if let wt @ Some(_) = self.bgm_slider.touch(touch, t, &mut config.volume_bgm) {
            if (config.volume_bgm - old).abs() > 0.001 {
                BGM_VOLUME_UPDATED.store(true, Ordering::Relaxed);
            }
            return Ok(wt);
        }
        // 打开判定偏移校准向导（异步构造页面）。
        if self.cali_btn.touch(touch, t) {
            self.cali_task = Some(Box::pin(OffsetPage::new()));
            return Ok(Some(false));
        }
        // 期望采样率：在固定候选值列表中循环切换（None = 后端默认）。
        #[cfg(not(target_os = "android"))]
        if self.preferred_sample_rate_btn.touch(touch, t) {
            let options = [None, Some(44100), Some(48000), Some(88200), Some(96000), Some(192000)];
            let current = config.preferred_sample_rate;
            let selected = options.iter().position(|&r| r == current).unwrap_or(0);
            config.preferred_sample_rate = options[(selected + 1) % options.len()];
            return Ok(Some(true));
        }
        // 音频缓冲区大小（帧）：仅 OpenHarmony 暴露，在 128/256/512 间循环。
        #[cfg(target_env = "ohos")]
        if self.audio_buffer_size_btn.touch(touch, t) {
            let options = [128u32, 256u32, 512u32];
            let current = config.audio_buffer_size.unwrap_or(256);
            let selected = options.iter().position(|&r| r == current).unwrap_or(1);
            config.audio_buffer_size = Some(options[(selected + 1) % options.len()]);
            return Ok(Some(true));
        }
        Ok(None)
    }

    /// 轮询校准向导页面的构造结果，就绪后以子页面形式打开。
    pub fn update(&mut self, _t: f32) -> Result<bool> {
        if let Some(task) = &mut self.cali_task {
            if let Some(res) = poll_future(task.as_mut()) {
                match res {
                    Err(err) => show_error(err.context(tl!("load-cali-failed"))),
                    Ok(page) => {
                        self.next_page = Some(NextPage::Overlay(Box::new(page)));
                    }
                }
                self.cali_task = None;
            }
        }
        // 本分栏不通过 `update` 上报设置变更（音量等已在 `touch` 中即时处理），恒返回 false。
        Ok(false)
    }

    /// 渲染音频分栏列表，返回 `(内容宽, 内容高)`。
    pub fn render(&mut self, ui: &mut Ui, r: Rect, t: f32) -> (f32, f32) {
        let w = r.w;
        let mut h = 0.;
        // 逐行宏：执行一段渲染后按 ITEM_HEIGHT 下移光标并累加内容高度。
        macro_rules! item {
            ($($b:tt)*) => {{
                $($b)*
                ui.dy(ITEM_HEIGHT);
                h += ITEM_HEIGHT;
            }}
        }
        let rr = right_rect(w);

        let data = get_data();
        let config = &data.config;
        // 音画自动对齐开关。
        item! {
            render_title(ui, tl!("item-adjust"), Some(tl!("item-adjust-sub")));
            render_switch(ui, rr, t, &mut self.adjust_btn, config.adjust_time);
        }
        // 谱面音乐音量：右侧显示两位小数的当前值。
        item! {
            render_title(ui, tl!("item-music"), None);
            self.music_slider.render(ui, rr, t, config.volume_music, format!("{:.2}", config.volume_music));
        }
        // 音效音量。
        item! {
            render_title(ui, tl!("item-sfx"), None);
            self.sfx_slider.render(ui, rr, t, config.volume_sfx, format!("{:.2}", config.volume_sfx));
        }
        // 背景（菜单）音乐音量。
        item! {
            render_title(ui, tl!("item-bgm"), None);
            self.bgm_slider.render(ui, rr, t, config.volume_bgm, format!("{:.2}", config.volume_bgm));
        }
        // 判定偏移校准：按钮上直接显示当前偏移（`offset` 以秒存储，这里换算为毫秒）。
        item! {
            render_title(ui, tl!("item-cali"), None);
            self.cali_btn.render_text(ui, rr, t, format!("{:.0}ms", config.offset * 1000.), 0.5, true);
        }
        // 期望采样率：未设置时显示「默认」。
        #[cfg(not(target_os = "android"))]
        item! {
            render_title(ui, tl!("item-preferred-sample-rate"), None);
            let text = if let Some(rate) = config.preferred_sample_rate {
                format!("{} Hz", rate)
            } else {
                tl!("preferred-sample-rate-default").to_string()
            };
            self.preferred_sample_rate_btn.render_text(ui, rr, t, text, 0.5, false);
        }
        // 音频缓冲区大小（帧），未设置时按默认 256 显示。
        #[cfg(target_env = "ohos")]
        item! {
            render_title(ui, tl!("item-audio-buffer-size"), None);
            let buf_size = config.audio_buffer_size.unwrap_or(256);
            self.audio_buffer_size_btn.render_text(ui, rr, t, format!("{}", buf_size), 0.5, false);
        }
        (w, h)
    }

    /// 取出待打开的子页面（校准向导）。
    pub fn next_page(&mut self) -> Option<NextPage> {
        self.next_page.take()
    }
}

/// 谱面分栏的状态与控件。
struct ChartList {
    /// 显示实时准确率开关（对应 `Config::show_acc`）。
    show_acc_btn: DRectButton,
    /// AP/FC 实时指示器开关（对应 `Config::ap_fc_indicator`）。
    ap_fc_indicator_btn: DRectButton,
    /// 显示平均帧率开关（对应 `Config::show_avg_fps`）。
    show_avg_fps_btn: DRectButton,
    /// 双击暂停开关（对应 `Config::double_click_to_pause`）。
    dc_pause_btn: DRectButton,
    /// 「双击可暂停」引导提示开关（对应 `Config::double_hint`）。
    dhint_btn: DRectButton,
    /// 激进模式开关（对应 `Config::aggressive`，更早裁剪不可见对象以换帧率）。
    opt_btn: DRectButton,
    /// 键盘模拟触摸开关（对应 `Config::use_keyboard`）。
    use_keyboard_btn: DRectButton,
    /// 谱面流速倍率滑块（对应 `Config::speed`），范围 0.5..2.0。
    speed_slider: Slider,
    /// 音符缩放滑块（对应 `Config::note_scale`），范围 0.8..1.2。
    size_slider: Slider,
}

// 谱面分栏：与游玩显示/操作相关的偏好。
impl ChartList {
    /// 构造谱面分栏控件。
    ///
    /// 流速范围 `0.5..=2.0`（步长 0.05）；音符缩放范围 `0.8..=1.2`（步长 0.005，
    /// 缩放对视觉影响敏感，因此步长更细）。
    pub fn new() -> Self {
        Self {
            show_acc_btn: DRectButton::new(),
            ap_fc_indicator_btn: DRectButton::new(),
            show_avg_fps_btn: DRectButton::new(),
            dc_pause_btn: DRectButton::new(),
            dhint_btn: DRectButton::new(),
            opt_btn: DRectButton::new(),
            use_keyboard_btn: DRectButton::new(),
            speed_slider: Slider::new(0.5..2., 0.05),
            size_slider: Slider::new(0.8..1.2, 0.005),
        }
    }

    /// 谱面分栏没有吸顶控件。
    pub fn top_touch(&mut self, _touch: &Touch, _t: f32) -> bool {
        false
    }

    /// 处理谱面分栏的触摸（全部为布尔开关或滑块，值即时写入 `Config`）。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> Result<Option<bool>> {
        let data = get_data_mut();
        let config = &mut data.config;
        // 显示实时准确率。
        if self.show_acc_btn.touch(touch, t) {
            config.show_acc ^= true;
            return Ok(Some(true));
        }
        // 显示 AP/FC 指示器。
        if self.ap_fc_indicator_btn.touch(touch, t) {
            config.ap_fc_indicator ^= true;
            return Ok(Some(true));
        }
        // 显示平均帧率（性能排查用）。
        if self.show_avg_fps_btn.touch(touch, t) {
            config.show_avg_fps ^= true;
            return Ok(Some(true));
        }
        // 允许双击暂停。
        if self.dc_pause_btn.touch(touch, t) {
            config.double_click_to_pause ^= true;
            return Ok(Some(true));
        }
        // 双击暂停的引导提示。
        if self.dhint_btn.touch(touch, t) {
            config.double_hint ^= true;
            return Ok(Some(true));
        }
        // 激进模式：更早回收不可见对象，以可能的视觉残缺换取帧率。
        if self.opt_btn.touch(touch, t) {
            config.aggressive ^= true;
            return Ok(Some(true));
        }
        // 键盘模拟触摸（桌面端试玩）。
        if self.use_keyboard_btn.touch(touch, t) {
            config.use_keyboard ^= true;
            return Ok(Some(true));
        }
        // 流速：只影响音符移动速度，不改变判定时刻。
        if let wt @ Some(_) = self.speed_slider.touch(touch, t, &mut config.speed) {
            return Ok(wt);
        }
        // 音符缩放：与谱面自带的缩放设置相乘。
        if let wt @ Some(_) = self.size_slider.touch(touch, t, &mut config.note_scale) {
            return Ok(wt);
        }
        Ok(None)
    }

    /// 谱面分栏无异步任务，恒返回 `false`。
    pub fn update(&mut self, _t: f32) -> Result<bool> {
        Ok(false)
    }

    /// 渲染谱面分栏列表，返回 `(内容宽, 内容高)`。
    pub fn render(&mut self, ui: &mut Ui, r: Rect, t: f32) -> (f32, f32) {
        let w = r.w;
        let mut h = 0.;
        // 逐行宏：执行一段渲染后按 ITEM_HEIGHT 下移光标并累加内容高度。
        macro_rules! item {
            ($($b:tt)*) => {{
                $($b)*
                ui.dy(ITEM_HEIGHT);
                h += ITEM_HEIGHT;
            }}
        }
        let rr = right_rect(w);

        let data = get_data();
        let config = &data.config;
        // 实时准确率。
        item! {
            render_title(ui, tl!("item-show-acc"), None);
            render_switch(ui, rr, t, &mut self.show_acc_btn, config.show_acc);
        }
        // AP/FC 指示器（标题带副标题）。
        item! {
            render_title(ui, tl!("item-ap-fc-indicator"), Some(tl!("item-ap-fc-indicator-sub")));
            render_switch(ui, rr, t, &mut self.ap_fc_indicator_btn, config.ap_fc_indicator);
        }
        // 平均帧率。
        item! {
            render_title(ui, tl!("item-show-avg-fps"), Some(tl!("item-show-avg-fps-sub")));
            render_switch(ui, rr, t, &mut self.show_avg_fps_btn, config.show_avg_fps);
        }
        // 双击暂停。
        item! {
            render_title(ui, tl!("item-dc-pause"), None);
            render_switch(ui, rr, t, &mut self.dc_pause_btn, config.double_click_to_pause);
        }
        // 双击暂停的引导提示。
        item! {
            render_title(ui, tl!("item-dhint"), Some(tl!("item-dhint-sub")));
            render_switch(ui, rr, t, &mut self.dhint_btn, config.double_hint);
        }
        // 激进模式。
        item! {
            render_title(ui, tl!("item-opt"), Some(tl!("item-opt-sub")));
            render_switch(ui, rr, t, &mut self.opt_btn, config.aggressive);
        }
        // 键盘模拟触摸。
        item! {
            render_title(ui, tl!("item-use-keyboard"), Some(tl!("item-use-keyboard-sub")));
            render_switch(ui, rr, t, &mut self.use_keyboard_btn, config.use_keyboard);
        }
        // 流速：右侧显示两位小数。
        item! {
            render_title(ui, tl!("item-speed"), None);
            self.speed_slider.render(ui, rr, t, config.speed, format!("{:.2}", config.speed));
        }
        // 音符缩放：右侧显示三位小数（变化更细微）。
        item! {
            render_title(ui, tl!("item-note-size"), None);
            self.size_slider.render(ui, rr, t, config.note_scale, format!("{:.3}", config.note_scale));
        }
        (w, h)
    }
}

/// 调试分栏的状态与控件。
struct DebugList {
    /// 谱面调试开关（对应 `Config::chart_debug`）。
    chart_debug_btn: DRectButton,
    /// 触控调试开关（对应 `Config::touch_debug`）。
    touch_debug_btn: DRectButton,
}

// 调试分栏：面向开发者的排错开关。
impl DebugList {
    /// 构造调试分栏控件。
    pub fn new() -> Self {
        Self {
            chart_debug_btn: DRectButton::new(),
            touch_debug_btn: DRectButton::new(),
        }
    }

    /// 调试分栏没有吸顶控件。
    pub fn top_touch(&mut self, _touch: &Touch, _t: f32) -> bool {
        false
    }

    /// 处理调试分栏的触摸：分别切换谱面调试与触控调试开关。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> Result<Option<bool>> {
        let data = get_data_mut();
        let config = &mut data.config;
        if self.chart_debug_btn.touch(touch, t) {
            config.chart_debug ^= true;
            return Ok(Some(true));
        }
        if self.touch_debug_btn.touch(touch, t) {
            config.touch_debug ^= true;
            return Ok(Some(true));
        }
        Ok(None)
    }

    /// 调试分栏无异步任务，恒返回 `false`。
    pub fn update(&mut self, _t: f32) -> Result<bool> {
        Ok(false)
    }

    /// 渲染调试分栏列表，返回 `(内容宽, 内容高)`。
    pub fn render(&mut self, ui: &mut Ui, r: Rect, t: f32) -> (f32, f32) {
        let w = r.w;
        let mut h = 0.;
        // 逐行宏：执行一段渲染后按 ITEM_HEIGHT 下移光标并累加内容高度。
        macro_rules! item {
            ($($b:tt)*) => {{
                $($b)*
                ui.dy(ITEM_HEIGHT);
                h += ITEM_HEIGHT;
            }}
        }
        let rr = right_rect(w);

        let data = get_data();
        let config = &data.config;
        // 谱面调试：显示判定线坐标、命中范围等辅助信息。
        item! {
            render_title(ui, tl!("item-chart-debug"), Some(tl!("item-chart-debug-sub")));
            render_switch(ui, rr, t, &mut self.chart_debug_btn, config.chart_debug);
        }
        // 触控调试：绘制触点轨迹与判定区域，便于定位触控问题。
        item! {
            render_title(ui, tl!("item-touch-debug"), Some(tl!("item-touch-debug-sub")));
            render_switch(ui, rr, t, &mut self.touch_debug_btn, config.touch_debug);
        }
        (w, h)
    }
}
