//! 应用层根场景模块：`MainScene` 是引擎场景栈的常驻栈底。
//!
//! 引擎只认识 `MainScene` 这一个场景，所有引擎回调都由它再转发给自己维护的
//! `pages` 页面栈的栈顶元素（见 `crate::page::Page`）。页面通过 `next_page()`
//! 返回 `NextPage::Overlay`/`NextPage::Pop` 请求入栈/出栈，转场动画由
//! `SharedState` 内的 `Fader` 统一驱动，因此页面层无需感知引擎的 `NextScene` 机制。
//!
//! 本模块同时是「导入」与「深链」的总入口：拖拽入窗/文件选择/批量导入包/深链下载
//! 得到的文件都在这里被接管并交给 `import_chart`，资源包导入结果则通过
//! `MainScene::take_imported_respack()` 交回给页面层消费。
use super::{import_chart, L10N_LOCAL};
use crate::{
    charts_view::NEED_UPDATE,
    data::LocalChart,
    deeplink::{self, DeepLink, DeepLinkChartOpening, DeepLinkDownload, DeepLinkTarget},
    dir, get_data, get_data_mut,
    icons::Icons,
    mp::MPPanel,
    page::{ChartItem, ExportInfo, HomePage, NextPage, Page, ResPackItem, SharedState},
    save_data,
    scene::{confirm_dialog, import_chart_to, parse_warnings_to_string, SongScene, TEX_BACKGROUND, TEX_ICON_BACK},
};
use anyhow::{anyhow, Context, Result};
use macroquad::prelude::*;
use once_cell::sync::Lazy;
use prpr::{
    core::ResPackInfo,
    ext::{unzip_into, RectExt, SafeTexture, ScaleType},
    info::ChartInfo,
    parse::ParseWarnings,
    scene::{return_file, show_error, show_message, take_file, NextScene, Scene, DIALOG},
    task::Task,
    time::TimeManager,
    ui::{button_hit, Dialog, FontArc, RectButton, Ui, UI_AUDIO},
};
use sasa::{AudioClip, Music};
use std::{
    any::Any,
    cell::RefCell,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    mem,
    path::{Component, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread_local,
    time::{Duration, Instant},
};
use tempfile::tempfile;
use uuid::Uuid;

// 页面栈压入子页面时给 BGM 施加的低通滤波强度。取 0.95（而非 1.0，即完全滤掉）
// 是为了让背景音乐在子页面里依然隐约可闻，返回栈底时再通过 `set_low_pass(0.)` 恢复。
const LOW_PASS: f32 = 0.95;

/// 设置页修改 BGM 音量后置位的一次性标志，由 `MainScene::update` 在下一帧消费
/// （`fetch_and(false, ...)`）并把新音量热更新到正在播放的 `Music` 上。
/// 用一次性标志而不是直接传递句柄，是为了让设置页不必持有 `MainScene` 的字段。
pub static BGM_VOLUME_UPDATED: AtomicBool = AtomicBool::new(false);

// 主线程局部状态：
// - `RESPACK_ITEM`：刚导入成功的资源包条目，等待 `take_imported_respack()` 取走，
//   导入结果无法直接返回给发起方（导入是引擎回调异步触发的），故用槽位暂存；
// - `MP_PANEL`：全局唯一的多人联机面板，在 `new_inner` 中创建后一直复用，
//   这样即使根场景被重新进入，已有的联机连接与消息历史也不会丢失。
thread_local! {
    static RESPACK_ITEM: RefCell<Option<ResPackItem>> = RefCell::default();
    pub static MP_PANEL: RefCell<Option<MPPanel>> = RefCell::default();
}

/// 多人联机悬浮按钮拖拽位置的持久化文件路径（`<root>/mp-pos`，内容为 `"x,y"`）。
/// 该位置属于 UI 偏好而非配置项，故单独存一个文件；读取/解析失败由调用方回退默认值。
#[inline]
fn position_file() -> Result<String> {
    Ok(format!("{}/mp-pos", dir::root()?))
}

/// 引擎场景栈的栈底常驻场景，同时是 phira 自身页面栈（`pages`）的宿主。
///
/// 两层栈的分工：
/// - 引擎栈只负责把 `Scene` 回调（`touch`/`update`/`render`/`enter`…）交给本场景；
/// - 本场景再按优先级把它们分发给 `pages` 栈顶页面，并额外承担页面间转场、
///   标题栏与返回键、以及各种全屏覆盖层（导入进度、深链、联机面板）的绘制与输入拦截。
pub struct MainScene {
    /// 页面栈与各页面共享的全局状态（转场 fader、图标集、本地谱面缓存等）。
    state: SharedState,

    /// 背景音乐。仅在 `closed` feature（正式发行版）下真正加载，其余构建恒为 `None`，
    /// 所有对 `bgm` 的操作都退化成空操作。
    bgm: Option<Music>,

    /// 斜向条纹背景纹理，配合 `STRIPE_MATERIAL` 动画材质铺满全屏。
    background: SafeTexture,
    /// 左上角返回按钮的命中区域：每帧在 `render` 中用 `ui.back_rect()` 刷新，
    /// `touch` 里直接用上一帧的矩形做命中测试（命中与显示保持一致）。
    btn_back: RectButton,
    /// 返回键图标纹理（`TEX_ICON_BACK` 的本地副本，避免每次绘制都访问线程局部）。
    icon_back: SafeTexture,

    /// phira 的页面栈：`pages[0]` 恒为 `HomePage`（栈底），栈顶元素接收输入、
    /// 决定 `next_page()` 并参与渲染；栈长 >= 2 时才绘制返回键。
    pages: Vec<Box<dyn Page>>,

    /// 单谱面导入任务（文件选择或深链下载完成后启动）。存在期间会拦截所有触摸
    /// 并显示全屏 loading，避免用户在导入过程中触发页面跳转导致数据竞争。
    import_task: Option<Task<Result<(LocalChart, ParseWarnings)>>>,

    // deeplink import
    /// 已解析但尚未经用户确认的深链目标；确认对话框按钮回调置位 `deeplink_confirm` 后才真正开始下载。
    deeplink_pending: Option<DeepLinkTarget>,
    /// 确认对话框回调（跨线程/闭包）与 `update` 之间的信号：用户点击「下载」时置 true。
    deeplink_confirm: Arc<AtomicBool>,
    /// 正在进行的深链下载覆盖层。取消时只需把它置为 `None`：`Drop` 会中止底层传输。
    deeplink_dl: Option<DeepLinkDownload>,

    // deeplink chart (open the details page of a chart by id)
    /// 正在拉取谱面详情（`/chart/{id}`）的覆盖层状态。
    deeplink_chart: Option<DeepLinkChartOpening>,
    /// 详情拉取成功后待提交给引擎的入栈场景，下一帧由 `next_scene` 返回。
    deeplink_scene: Option<NextScene>,

    /// 共享图标集（传给 `HomePage`/`SongScene` 等页面使用）。
    icons: Arc<Icons>,

    /// 联机悬浮按钮的命中区域（每帧按 `mp_btn_pos` 更新）。
    mp_btn: RectButton,
    /// 联机按钮的图标纹理。
    mp_icon: SafeTexture,
    /// 联机按钮当前的逻辑坐标；渲染时会 clamp 到屏幕范围内，拖拽越界不会丢失按钮。
    mp_btn_pos: Vec2,
    /// 进行中的拖拽手势：`(触摸 id, 按下点坐标, 按下时的按钮位置)`。
    /// 只跟踪一根手指，从而在拖动按钮时不会误触发其他交互。
    mp_move: Option<(u64, Vec2, Vec2)>,
    /// 本次拖拽是否已越过位移阈值；用来区分「点击按钮」与「拖动按钮」。
    mp_moved: bool,
    /// 拖拽位置待落盘的时刻：拖拽停止 1 秒后才写一次文件，避免高频磁盘写入。
    mp_save_pos_at: Option<Instant>,

    // batch import
    /// 批量导入确认对话框的信号，`update` 消费后启动后台导入任务。
    batch_import_confirm: Arc<AtomicBool>,
    /// 待导入的打包文件路径与其 `export.json` 元信息。
    batch_import: Option<(String, ExportInfo)>,
    /// 批量导入后台任务；它结束代表本轮导入全部处理完毕（成功或失败）。
    batch_import_task: Option<Task<Result<()>>>,
    /// 后台逐谱面导入时回传结果/进度的通道；通道断开说明后台线程 panic。
    batch_import_rx: Option<mpsc::Receiver<ImportChart>>,
    /// 已导入、等待任务结束后统一入库的谱面列表。
    batch_imported_charts: Vec<ImportChart>,
    /// 打包文件内待导入谱面总数，仅用于渲染 `current/total` 进度。
    batch_import_total: usize,
}

/// 批量导入过程中，后台线程通过通道回传给主线程的单个谱面处理结果。
/// 之所以不在后台直接入库，是因为 `get_data_mut`/`save_data` 只能在主线程调用。
enum ImportChart {
    /// 导入成功：谱面数据 + 解析警告（警告会汇总后一次性展示给用户）。
    Imported(Box<LocalChart>, ParseWarnings),
    /// 跳过：该谱面本地已存在（带的字符串是谱面名，用于汇总提示「已跳过」列表）。
    Skipped(String),
}

// `MainScene` 的构造与页面栈操作。构造分三步：
// `init()` 写入全局一次性资源（音效、背景/返回纹理）→ 组织 BGM（`closed` 分支）
// → `new_inner()` 装配全部字段，最后才压入 `HomePage` 作为栈底，
// 保证栈底页面进入时它依赖的纹理/音效等资源已经就绪。
impl MainScene {
    // shall be call exactly once
    /// 创建根场景，并压入 `HomePage` 作为页面栈栈底。
    ///
    /// 全进程只应调用一次：其中会初始化全局音效与线程局部纹理，重复调用会重复加载资源。
    ///
    /// # Arguments
    /// * `fallback` - `SharedState` 使用的兜底字体（UI 缺字时使用）。
    ///
    /// # Errors
    /// 全局资源（音效、纹理、图标）加载失败，或 `HomePage` 初始化失败时返回错误。
    pub async fn new(fallback: FontArc) -> Result<Self> {
        // 阶段 1：全局一次性资源（音效、背景/返回纹理）初始化。
        Self::init().await?;

        // 阶段 2：背景音乐。仅正式发行版（`closed`）打包了 `res/bgm`，其余构建
        // 保持 `None`，于是后续所有 BGM 操作都退化为空操作，无需到处判 cfg。
        #[cfg(closed)]
        let bgm = {
            // BGM 以自定义交叉淡化时长循环（`loop_mix_time` 需与音源实际无缝点对齐）。
            // 初始音量取用户配置，之后由 `BGM_VOLUME_UPDATED` 驱动热更新。
            let bgm_clip = AudioClip::new(crate::load_res("res/bgm").await)?;
            Some(UI_AUDIO.with(|it| {
                it.borrow_mut().create_music(
                    bgm_clip,
                    sasa::MusicParams {
                        amplifier: get_data().config.volume_bgm,
                        loop_mix_time: 5.46,
                        command_buffer_size: 64,
                        ..Default::default()
                    },
                )
            })?)
        };
        // 非发行版：不加载任何 BGM。
        #[cfg(not(closed))]
        let bgm = None;

        // 阶段 3：装配场景字段，并把主页压栈作为页面栈栈底（唯一不显示返回键的页面）。
        let mut sf = Self::new_inner(bgm, fallback).await?;
        // 主页需要 `Icons`，因此必须在 `new_inner`（内部构造 icons）之后创建。
        sf.pages.push(Box::new(HomePage::new(Arc::clone(&sf.icons)).await?));
        Ok(sf)
    }

    /// 全局一次性初始化（只由 `MainScene::new` 调用）。
    ///
    /// 先把用户配置的音效音量同步给 prpr 的全局音量（`UI_SFX_VOLUME` 用原子位模式存 `f32`），
    /// 再加载三个按钮音效并注册到 prpr ui 的线程局部槽位，最后把背景/返回图写入
    /// `TEX_BACKGROUND`/`TEX_ICON_BACK`——这两张纹理除了本场景渲染背景与返回键之外，
    /// 还会被后续压入的页面（如 `HomePage`）通过线程局部直接取用。
    ///
    /// # Errors
    /// 任一音频或纹理加载失败时返回错误（此时无法构造可用的根场景）。
    async fn init() -> Result<()> {
        // 音效总音量与 BGM 音量分开配置，故在此单独写入 UI 音效音量。
        prpr::ui::UI_SFX_VOLUME.store(get_data().config.volume_sfx.to_bits(), Ordering::Relaxed);
        // init button hitsound
        // 局部宏：加载 `$path` 为音效并写入 prpr ui 模块对应的线程局部槽位 `$name`。
        // 之所以定义为宏，只是为了让三处「加载 + 注册」样板保持一行一处。
        macro_rules! load_sfx {
            ($name:ident, $path:literal) => {{
                let clip = AudioClip::new(load_file($path).await?)?;
                let sound = UI_AUDIO.with(|it| it.borrow_mut().create_sfx(clip, None))?;
                prpr::ui::$name.with(|it| *it.borrow_mut() = Some(sound));
            }};
        }
        load_sfx!(UI_BTN_HITSOUND_LARGE, "button_large.ogg");
        load_sfx!(UI_BTN_HITSOUND, "button.ogg");
        load_sfx!(UI_SWITCH_SOUND, "switch.ogg");

        // 背景与返回图标在启动时加载一次即常驻；写入线程局部是为了让 `new_inner`
        // 以及后续页面无需持有 `MainScene` 也能取到这两张纹理。
        let background: SafeTexture = load_texture("background.jpg").await?.into();
        let icon_back: SafeTexture = load_texture("back.png").await?.into();

        TEX_BACKGROUND.with(|it| *it.borrow_mut() = Some(background));
        TEX_ICON_BACK.with(|it| *it.borrow_mut() = Some(icon_back));

        Ok(())
    }

    /// `MainScene::new` 的字段装配阶段：构造共享状态、创建全局唯一的联机面板，
    /// 并初始化所有字段（含从磁盘读取联机按钮位置等 IO）。
    ///
    /// 单独抽出的原因是它不涉及 `closed`/BGM 的 cfg 分支，便于在调试构建中复用。
    async fn new_inner(bgm: Option<Music>, fallback: FontArc) -> Result<Self> {
        // `SharedState` 会加载图标集与本地谱面数据，并保留 `fallback` 字体作为兜底。
        let state = SharedState::new(fallback).await?;
        // 联机面板全局唯一：在此创建并注册到线程局部，之后由 `MainScene` 每帧轮询驱动，
        // 面板的开关/关闭都不会销毁它，从而保留连接与消息历史。
        let icon_user = load_texture("user.png").await?;
        MP_PANEL.with(|it| *it.borrow_mut() = Some(MPPanel::new(icon_user.into())));
        Ok(Self {
            state,

            bgm,

            // 两张纹理取 `init()` 写入线程局部的副本；`new` 保证在此之前已写入，故 unwrap 安全。
            background: TEX_BACKGROUND.with(|it| it.borrow().clone().unwrap()),
            btn_back: RectButton::new(),
            icon_back: TEX_ICON_BACK.with(|it| it.borrow().clone().unwrap()),

            // 页面栈暂为空，`MainScene::new` 最后才压入 `HomePage` 作为栈底。
            pages: Vec::new(),

            import_task: None,

            // 深链与批量导入状态一律以「空闲」起步，等引擎回调或用户操作再驱动。
            deeplink_pending: None,
            deeplink_confirm: Arc::new(AtomicBool::new(false)),
            deeplink_dl: None,

            deeplink_chart: None,
            deeplink_scene: None,

            icons: Arc::new(Icons::new().await?),

            mp_btn: RectButton::new(),
            mp_icon: SafeTexture::from(load_texture("multiplayer.png").await?).with_mipmap(),
            // 联机按钮位置持久化在 `mp-pos`；解析失败（首次启动/文件损坏）时回退默认位置，
            // 所以这里用 `unwrap_or_default()` 而不是把错误上抛。
            mp_btn_pos: (|| -> Result<Vec2> {
                let s = std::fs::read_to_string(position_file()?)?;
                let (x, y) = s.split_once(',').ok_or_else(|| anyhow!("invalid"))?;
                Ok(vec2(x.parse()?, y.parse()?))
            })()
            .unwrap_or_default(),
            mp_move: None,
            mp_moved: false,
            mp_save_pos_at: None,

            // 批量导入相关状态：确认信号先置 false，任务与通道只在用户确认后建立。
            batch_import_confirm: Arc::default(),
            batch_import: None,
            batch_import_task: None,
            batch_import_rx: None,
            batch_imported_charts: Vec::new(),
            batch_import_total: 0,
        })
    }

    /// 弹出栈顶页面并启动返回转场。
    ///
    /// 若被弹出的页面禁止播放 BGM、而新的栈顶页面允许播放，则让 BGM 淡入
    /// （典型场景：从禁止 BGM 的谱面详情返回主页时恢复背景音乐）。
    /// 真正的出栈并不在此发生，而是在转场结束（`update` 中 `fader.done`）时。
    fn pop(&mut self) {
        if !self.pages.last().unwrap().can_play_bgm() && self.pages[self.pages.len() - 2].can_play_bgm() {
            if let Some(bgm) = &mut self.bgm {
                let _ = bgm.fade_in(0.5);
            }
        }
        self.state.fader.back(self.state.t);
    }

    /// 取走并清空「刚导入成功的资源包」槽位，供设置页等页面轮询消费。
    ///
    /// 返回 `None` 表示当前没有新的资源包导入结果；由于编辑器/拖拽导入的结果无法
    /// 同步返回给调用方，这里用「取走即清空」的语义保证同一结果只被消费一次。
    pub fn take_imported_respack() -> Option<ResPackItem> {
        RESPACK_ITEM.with(|it| it.borrow_mut().take())
    }
}

// `MainScene` 作为引擎侧唯一常驻场景，把引擎回调黏合到页面栈：
// - `on_result`：把引擎回传的结果原样转交栈顶页面（页面栈内部不跨页转发）；
// - `enter`/`resume`/`pause`：同步 BGM 状态与 `SharedState`，再通知联机面板与栈顶页面；
// - `touch`：按固定优先级分发输入（转场 → 导入 → 深链 → 联机面板 → 栈顶页面 → 返回键）；
// - `update`：驱动转场状态机与页面栈增减，并处理导入/深链/联机等常驻任务；
// - `render`：背景条纹 → 页面（转场含旧页面）→ 标题 → 返回键 → 页面顶层 → 联机面板 → 覆盖层；
// - `next_scene`：把页面或联机面板请求的引擎级场景切换冒泡给引擎。
impl Scene for MainScene {
    /// 引擎通过 `NextScene::Result` 回传的数据直接下发给栈顶页面处理。
    /// 转场过程中引擎不会回传结果，因此这里无需像 `touch` 那样同时通知下一层页面。
    fn on_result(&mut self, _tm: &mut TimeManager, result: Box<dyn Any>) -> Result<()> {
        self.pages.last_mut().unwrap().on_result(result, &mut self.state)
    }

    /// 场景（重新）进入引擎前台：BGM 淡入、刷新共享状态，然后通知栈顶页面与联机面板。
    /// 面板的 `enter()` 用于标记「已随根场景进入过」，是结算上报的前置条件之一。
    fn enter(&mut self, tm: &mut TimeManager, _target: Option<RenderTarget>) -> Result<()> {
        // 1.3s 淡入与引擎的场景切换动画时长匹配，避免音乐先于画面出现。
        if let Some(bgm) = &mut self.bgm {
            let _ = bgm.fade_in(1.3);
        }
        self.state.update(tm);
        self.pages.last_mut().unwrap().enter(&mut self.state)?;
        MP_PANEL.with(|it| {
            if let Some(panel) = it.borrow_mut().as_mut() {
                panel.enter();
            }
        });
        Ok(())
    }

    /// 从后台返回（或对话框关闭）时恢复：先恢复时间管理器与 BGM，
    /// 再把唤醒事件转发给栈顶页面（页面据此重启被暂停的动画/音频）。
    fn resume(&mut self, tm: &mut TimeManager) -> Result<()> {
        tm.resume();
        if let Some(bgm) = &mut self.bgm {
            bgm.play()?;
        }
        self.state.update(tm);
        self.pages.last_mut().unwrap().resume()?;
        Ok(())
    }

    /// 进入后台或弹出对话框时暂停：先暂停时间管理器与 BGM，再转发给栈顶页面，
    /// 避免页面内部动画在挂起期间继续推进（时间管理器暂停后页面拿到的时间戳会冻结）。
    fn pause(&mut self, tm: &mut TimeManager) -> Result<()> {
        tm.pause();
        if let Some(bgm) = &mut self.bgm {
            bgm.pause()?;
        }
        self.state.update(tm);
        self.pages.last_mut().unwrap().pause()?;
        Ok(())
    }

    /// 输入分发总入口，按固定优先级从高到低依次尝试：
    /// 1. 转场中：直接返回 `false`（不消费），让引擎继续把事件交给别的接收者，
    ///    同时阻止用户在转场期间触发页面交互；
    /// 2. 导入任务进行中：吞掉输入（进度由 loading 覆盖层展示）；
    /// 3. 深链下载 / 深链打开谱面覆盖层：独占输入，取消即置 `None` 触发 `Drop` 中止；
    /// 4. 联机面板（`mp_enabled` 打开时）：先给面板自身，再处理联机悬浮按钮的点击/拖拽；
    /// 5. 栈顶页面；
    /// 6. 返回键（`pages.len() > 1` 时）：先问页面能否拦截，否则出栈。
    ///
    /// 覆盖层排在页面之前，是因为它们绘制在页面之上、必须独占交互（否则会「穿透」到下层页面）；
    /// 返回键排在最后，则是为了让页面能优先消费落在左上角的手势。
    fn touch(&mut self, tm: &mut TimeManager, touch: &Touch) -> Result<bool> {
        // 1. 转场中：整帧不接受任何交互，避免新旧页面同时响应导致状态错乱。
        if self.state.fader.transiting() {
            return Ok(false);
        }
        // 2. 导入任务进行中：吞掉输入，防止用户切页后导入结果无处落地。
        if self.import_task.is_some() {
            return Ok(true);
        }
        // 3. 深链下载覆盖层：独占输入并随时可取消。
        if self.deeplink_dl.is_some() {
            let t = tm.real_time() as f32;
            let cancelled = self.deeplink_dl.as_mut().is_some_and(|dl| dl.touch(touch, t));
            if cancelled {
                // dropping the overlay aborts the transfer
                // 覆盖层的 `Drop` 实现会中止底层传输，因此取消只需丢弃它。
                self.deeplink_dl = None;
            }
            return Ok(true);
        }
        // 3. 深链谱面详情覆盖层：同样独占输入（该覆盖层只是展示拉取进度/结果）。
        if self.deeplink_chart.is_some() {
            let t = tm.real_time() as f32;
            let cancelled = self.deeplink_chart.as_mut().is_some_and(|it| it.touch(touch, t));
            if cancelled {
                // dropping the overlay discards the fetch
                // 丢弃覆盖层即放弃本次详情拉取结果。
                self.deeplink_chart = None;
            }
            return Ok(true);
        }

        // 4. 联机面板与联机悬浮按钮。`mp_enabled` 为编译/配置开关：关闭时
        // 既不绘制也不消费触摸，因此整个联机功能对用户完全不可见。
        if get_data().config.mp_enabled {
            // 4.1 面板自身优先消费触摸（面板绘制在所有页面之上，必须独占输入）。
            if MP_PANEL.with(|it| it.borrow_mut().as_mut().is_some_and(|it| it.touch(tm, touch))) {
                return Ok(true);
            }
            // 4.2 悬浮按钮点击：若本次手势没有产生拖动（`mp_moved == false`），
            // 则视为「打开面板」，并清空拖拽状态。
            if self.mp_btn.touch(touch) && !self.mp_moved {
                MP_PANEL.with(|it| {
                    if let Some(panel) = it.borrow_mut().as_mut() {
                        panel.show(tm.real_time() as _);
                    }
                });
                self.mp_move = None;
                self.mp_moved = false;
                return Ok(true);
            }
            // 4.3 拖拽手势跟踪：同一根手指按住按钮后移动超过阈值才判定为拖动，
            // 从而避免手指轻微抖动被当成拖动、导致按钮「点不动」。
            if let Some((id, pos, btn_pos)) = self.mp_move {
                if touch.id == id {
                    if matches!(touch.phase, TouchPhase::Cancelled | TouchPhase::Ended) {
                        // 手势结束：复位拖拽状态；位置落盘交给 `update` 的延时逻辑。
                        self.mp_move = None;
                        self.mp_moved = false;
                        return Ok(true);
                    }
                    let new_pos = touch.position;
                    if !self.mp_moved && (new_pos - pos).length() > 0.03 {
                        self.mp_moved = true;
                    }
                    if self.mp_moved {
                        self.mp_btn_pos = new_pos - pos + btn_pos;
                        self.mp_save_pos_at = Some(Instant::now() + Duration::from_secs(1));
                    }
                }
                return Ok(true);
            } else if self.mp_btn.touching() {
                // 按钮刚被按下：记录起始点与按钮初始位置，进入拖拽跟踪。
                self.mp_move = Some((touch.id, touch.position, self.mp_btn_pos));
                return Ok(true);
            }
        }

        // 5. 栈顶页面：先刷新共享状态，页面拿到的 `s.t` 等字段必须是最新的。
        let s = &mut self.state;
        s.update(tm);
        if self.pages.last_mut().unwrap().touch(touch, s)? {
            return Ok(true);
        }
        // 6. 返回键：仅当栈内不止一页时可用；页面可先通过 `on_back_pressed`
        // 拦截（例如谱面详情页用它来关闭自己的弹窗），返回 `false` 才真正出栈。
        if self.btn_back.touch(touch) && self.pages.len() > 1 {
            button_hit();
            if !self.pages.last_mut().unwrap().on_back_pressed(&mut self.state) {
                if self.pages.len() == 2 {
                    // 即将回到栈底（只剩主页）：解除低通滤波，让 BGM 恢复正常音色。
                    if let Some(bgm) = &mut self.bgm {
                        bgm.set_low_pass(0.)?;
                    }
                }
                self.pop();
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// 每帧驱动本场景：页面栈转场状态机、BGM，以及所有常驻异步任务
    /// （单谱面导入、深链、批量导入、联机按钮位置持久化）。
    ///
    /// 处理顺序不可随意调整：先推进 `SharedState` 与页面，再消费各种任务结果，
    /// 这样页面每帧看到的状态都是自洽的，且本帧新产生的结果会在下一帧生效。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        // 音频上下文失效（切后台、切换音频设备等）时惰性重建，避免整局无音效。
        UI_AUDIO.with(|it| it.borrow_mut().recover_if_needed())?;
        // 联机面板：`mp_enabled` 关闭时完全不轮询，因此不会建连、不产生流量。
        if get_data().config.mp_enabled {
            MP_PANEL.with(|it| {
                if let Some(panel) = it.borrow_mut().as_mut() {
                    panel.update(tm)
                } else {
                    Ok(())
                }
            })?;
        }
        // 页面栈驱动：转场中前后两层页面都要 update —— 旧页面需要继续播完退出动画
        // （否则会突然冻结），新页面需要提前推进自己的入场状态。
        let s = &mut self.state;
        s.update(tm);
        if s.fader.transiting() {
            let pos = self.pages.len() - 2;
            self.pages[pos].update(s)?;
        }
        self.pages.last_mut().unwrap().update(s)?;
        // 只在非转场时接受页面发起的导航请求，保证转场动画不会被打断。
        if !s.fader.transiting() {
            match self.pages.last_mut().unwrap().next_page() {
                NextPage::Overlay(mut sub) => {
                    // 从栈底（主页）进入第一个子页面时压低 BGM（低通滤波），
                    // 形成「背景音乐被推远」的层次感。
                    if self.pages.len() == 1 {
                        if let Some(bgm) = &mut self.bgm {
                            bgm.set_low_pass(LOW_PASS)?;
                        }
                    }
                    // 新页面先 enter（此时它还没进栈，拿到的仍是旧栈顶的状态），
                    // 若它不需要 BGM（如对局场景自理音频）则把 BGM 淡出。
                    sub.enter(s)?;
                    if !sub.can_play_bgm() {
                        if let Some(bgm) = &mut self.bgm {
                            let _ = bgm.fade_out(0.5);
                        }
                    }
                    self.pages.push(sub);
                    s.fader.sub(s.t);
                }
                NextPage::Pop => {
                    // 出栈同样先播转场，实际弹出在转场结束时。
                    self.pop();
                }
                NextPage::None => {}
            }
        } else if let Some(true) = s.fader.done(s.t) {
            // 转场结束：旧栈顶此刻才真正 exit（释放资源/停止它的任务），
            // 新栈顶 enter 一次以获得转场完成后的正确上下文。
            self.pages.pop().unwrap().exit()?;
            self.pages.last_mut().unwrap().enter(s)?;
        }
        // 设置页改过音量：一次性标志消费后热更新正在播放的 Music 音量。
        if let Some(bgm) = &mut self.bgm {
            if BGM_VOLUME_UPDATED.fetch_and(false, Ordering::Relaxed) {
                bgm.set_amplifier(get_data().config.volume_bgm)?;
            }
        }
        // 单谱面导入任务完成：成功则入库 + 落盘 + 通知曲库刷新，并展示解析警告；
        // 失败用错误对话框给出原因（常见于压缩包内缺 info.yml）。
        if let Some(task) = &mut self.import_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(itl!("import-failed")));
                    }
                    Ok((chart, warnings)) => {
                        if let Some(warn) = parse_warnings_to_string(&warnings) {
                            Dialog::plain(itl!("warning"), warn).show();
                        }
                        show_message(itl!("import-success")).ok();
                        get_data_mut().charts.push(chart);
                        save_data()?;
                        self.state.reload_local_charts();
                        NEED_UPDATE.store(true, Ordering::Relaxed);
                    }
                }
                self.import_task = None;
            }
        }
        // 处理引擎送来的拖拽/文件选择结果。`id` 是引擎约定的「导入类型」标识，
        // 由本模块注册的回调写入；`_import_auto` 表示类型未定，需要按下文规则探测。
        if let Some((id, file)) = take_file() {
            match id.as_str() {
                "_import_auto" => {
                    // 自动判别：zip 内含 `click.png` 视为音符资源包，否则按谱面包处理；
                    // 打不开（非 zip 等）也按谱面包交给后续流程报错。
                    let new_id = match File::open(&file).map(BufReader::new).map(zip::ZipArchive::new) {
                        Ok(Ok(zip)) => {
                            if zip.file_names().any(|name| name.ends_with("click.png")) {
                                "_import_respack"
                            } else {
                                "_import"
                            }
                        }
                        _ => "_import",
                    };
                    return_file(new_id.to_owned(), file);
                }
                "_import" => {
                    // 探测是否为「批量导入」打包文件：存在 `export.json` 即视为导出包，
                    // 同时统计包内 .zip 数量用于显示进度。
                    let export_info = (|| -> Result<Option<(ExportInfo, usize)>> {
                        let file = File::open(&file)?;
                        let mut archive = zip::ZipArchive::new(file)?;
                        let export_info = match archive.by_name("export.json") {
                            Err(zip::result::ZipError::FileNotFound) => {
                                return Ok(None);
                            }
                            Err(err) => {
                                return Err(err.into());
                            }
                            Ok(file) => serde_json::from_reader(file)?,
                        };
                        let mut count = 0;
                        for i in 0..archive.len() {
                            let file = archive.by_index(i)?;
                            if file.enclosed_name().is_some_and(|it| it.extension().is_some_and(|ext| ext == "zip")) {
                                count += 1;
                            }
                        }
                        Ok(Some((export_info, count)))
                    })();
                    match export_info {
                        // 探测阶段就失败（zip 损坏等）：直接报错，避免把坏文件当谱面继续解析。
                        Err(err) => {
                            show_error(err.context(itl!("import-failed")));
                        }
                        // 无 export.json：普通单谱面导入。
                        Ok(None) => {
                            self.import_task = Some(Task::new(async move {
                                let file = File::open(&file).context("cannot open file")?;
                                import_chart(file).await
                            }));
                        }
                        // 有 export.json：暂存文件与元信息，等用户确认后再开始批量导入。
                        Ok(Some((info, count))) => {
                            self.batch_import = Some((file, info));
                            self.batch_import_total = count;
                            confirm_dialog(itl!("batch-import"), itl!("batch-import-confirm", "count" => count), self.batch_import_confirm.clone());
                        }
                    };
                }
                // 资源包导入：先在内存中完成全部校验（info.yml + 必需贴图/音频），
                // 全部通过后才写磁盘，避免校验失败时留下半成品目录。
                "_import_respack" => {
                    let root = dir::respacks()?;
                    let dir = prpr::dir::Dir::new(&root)?;
                    let mut dir_id: Option<String> = None;
                    let item: Result<ResPackItem> = (|| {
                        // 只读地校验资源包：解析 info.yml 并校验字段合法性，
                        // 确认所有必选贴图与至少一种格式的音频都能被解码。
                        let config = {
                            let mut zip = zip::ZipArchive::new(BufReader::new(File::open(&file)?))?;
                            let config: ResPackInfo =
                                serde_yaml::from_reader(zip.by_name("info.yml").context("missing info.yml")?).context("invalid info.yml")?;
                            config.verify()?;
                            // 逐个解码必选贴图：任一缺失或损坏都会让整个资源包导入失败。
                            let mut buffer = Vec::new();
                            for file in [
                                "click.png",
                                "click_mh.png",
                                "drag.png",
                                "drag_mh.png",
                                "flick.png",
                                "flick_mh.png",
                                "hold.png",
                                "hold_mh.png",
                                "hit_fx.png",
                            ] {
                                let mut entry = zip.by_name(file).with_context(|| format!("missing file: {file}"))?;
                                buffer.clear();
                                entry.read_to_end(&mut buffer)?;
                                image::load_from_memory(&buffer).with_context(|| format!("failed to load image: {file}"))?;
                            }

                            // 每种音效只需存在一种可用格式（ogg/wav/mp3 之一）即可，
                            // 因此命中一个立即 `break`；四类音效全部缺失才算校验失败。
                            for audio in ["click", "drag", "flick", "ending"] {
                                for ext in [".ogg", ".wav", ".mp3"] {
                                    let mut entry = match zip.by_name(format!("{audio}{ext}").as_str()) {
                                        Err(zip::result::ZipError::FileNotFound) => continue,
                                        Err(err) => return Err(err.into()),
                                        Ok(file) => file,
                                    };
                                    buffer.clear();
                                    entry.read_to_end(&mut buffer)?;
                                    AudioClip::new(mem::take(&mut buffer)).with_context(|| format!("failed to load audio: {audio}"))?;
                                    break;
                                }
                            }
                            config
                        };

                        // 校验全部通过后才写磁盘：目录名用随机 UUID（重名则重掷），
                        // 使资源包目录名与展示名称解耦，避免重名冲突与非法字符。
                        let mut uuid = Uuid::new_v4();
                        while dir.exists(uuid.to_string())? {
                            uuid = Uuid::new_v4();
                        }
                        let id = uuid.to_string();
                        dir.create_dir_all(&id)?;
                        let dir = dir.open_dir(&id)?;
                        // 记下目录 id：一旦后续解压/登记失败，用它整体回滚。
                        dir_id = Some(id.clone());
                        unzip_into(BufReader::new(File::open(file)?), &dir, false).context("failed to unzip")?;
                        get_data_mut().respacks.push(id.clone());
                        save_data()?;
                        Ok(ResPackItem::new(Some(format!("{root}/{id}").into()), config.name))
                    })();
                    match item {
                        Err(err) => {
                            show_error(err.context(itl!("import-respack-failed")));
                            // 回滚：删掉已创建的资源包目录，保持目录状态与配置一致。
                            if let Some(id) = &dir_id {
                                dir.remove_dir_all(id)?;
                            }
                        }
                        Ok(item) => {
                            // 导入结果通过线程局部槽位交给页面层（`take_imported_respack` 取走），
                            // 让设置页能立刻刷新资源包列表。
                            RESPACK_ITEM.with(|it| *it.borrow_mut() = Some(item));
                            show_message(itl!("import-respack-success"));
                        }
                    }
                }
                // 未识别的导入类型：原样退回引擎，交给其他注册方（如谱面编辑器）处理。
                _ => return_file(id, file),
            }
        }
        // 深链统一在此处被取出：必须等到「没有对话框、没有导入或深链任务在进行」，
        // 因为 `Dialog::show` 会替换当前对话框（会覆盖别处的提示），
        // 而积压的深链可以安全地等到下一帧甚至更久再处理。
        // Wait until any dialog is gone and no import is running: `Dialog::show`
        // replaces the current dialog, and a pending deeplink can simply wait.
        if self.deeplink_dl.is_none()
            && self.import_task.is_none()
            && self.deeplink_chart.is_none()
            && self.deeplink_scene.is_none()
            && DIALOG.with(|it| it.borrow().is_none())
        {
            if let Some(input) = deeplink::take_deeplink() {
                // 一次深链只处理一次（`take_deeplink` 取走即清空），解析失败直接提示 URL 非法。
                match deeplink::parse_deeplink(&input) {
                    Err(err) => {
                        show_error(err.context(itl!("deeplink-bad-url")));
                    }
                    Ok(DeepLink::Chart(id)) => {
                        // 谱面深链只是打开详情页（与点击消息里的谱面等价、无副作用），
                        // 所以不需要用户确认。
                        // Viewing a chart's details is safe (same as tapping a
                        // chart in a message), so no confirmation is needed.
                        self.deeplink_chart = Some(deeplink::start_chart_opening(id));
                    }
                    Ok(DeepLink::Import(target)) => {
                        // 导入深链会下载并安装谱面，必须经用户确认；
                        // 官方域名只提示一次确认，第三方域名额外展示官方域名供用户核对。
                        let message = if target.official {
                            format!("{}\n{}", itl!("deeplink-confirm"), target.url)
                        } else {
                            format!(
                                "{}\n\n{}\n{}",
                                itl!("deeplink-unofficial", "host" => deeplink::official_host()),
                                itl!("deeplink-confirm"),
                                target.url
                            )
                        };
                        // 对话框回调受闭包生命周期限制，不能直接修改 `self`，
                        // 因此用共享原子置位信号，由下一帧的 `update` 真正启动下载。
                        // 约定：按钮 id `-1` 表示关闭对话框（不下载），`1` 表示「下载」。
                        Dialog::plain(itl!("deeplink-title"), message)
                            .buttons(vec![ttl!("cancel").into_owned(), itl!("deeplink-download").into_owned()])
                            .listener({
                                let res = self.deeplink_confirm.clone();
                                move |_dialog, id| {
                                    if id == -1 {
                                        return true;
                                    }
                                    if id == 1 {
                                        res.store(true, Ordering::SeqCst);
                                    }
                                    false
                                }
                            })
                            .show();
                        // 暂存目标：只有 `deeplink_confirm` 被置位后才会真正开始下载。
                        self.deeplink_pending = Some(target);
                    }
                }
            }
        }
        // 深链谱面详情拉取完成：若本地已有同 id 谱面，则带上本地路径与 mods，
        // 使详情页可直接开始游戏；否则按纯远程谱面处理（需先下载）。
        if let Some(res) = self.deeplink_chart.as_mut().and_then(|it| it.take_result()) {
            match res {
                Err(err) => show_error(err.context(itl!("deeplink-open-failed"))),
                Ok(chart) => {
                    // 按远端返回的谱面 id 在本地归档中查找同名谱面（用于复用本地成绩/文件）。
                    let (local_path, mods) = {
                        let data = get_data();
                        data.charts
                            .iter()
                            .find(|it| it.info.id == Some(chart.id))
                            .map(|it| (Some(it.local_path.clone()), it.mods))
                            .unwrap_or_default()
                    };
                    // 详情页以 Overlay 形式入栈，下一帧由 `next_scene` 交给引擎。
                    self.deeplink_scene = Some(NextScene::Overlay(Box::new(SongScene::new(
                        ChartItem::from_remote(chart.as_ref()),
                        local_path,
                        Arc::clone(&self.icons),
                        self.state.icons.clone(),
                        mods,
                    ))));
                }
            }
            // 结果已消费，丢弃覆盖层（同时释放其持有的网络资源）。
            self.deeplink_chart = None;
        }
        // 用户在确认框点了「下载」：消费一次性标志并启动下载（重复置位也只生效一次）。
        if self.deeplink_confirm.load(Ordering::Relaxed) && self.deeplink_dl.is_none() && self.import_task.is_none() {
            self.deeplink_confirm.store(false, Ordering::Relaxed);
            if let Some(target) = self.deeplink_pending.take() {
                self.deeplink_dl = Some(deeplink::start_deeplink_download(target)?);
            }
        }
        // 深链下载完成的产物是一个临时文件，直接交给单谱面导入流程；
        // 无论成功失败，覆盖层的使命都已结束。
        let dl_result = self.deeplink_dl.as_mut().and_then(|dl| dl.take_result());
        if let Some(res) = dl_result {
            match res {
                Ok(file) => self.import_task = Some(Task::new(import_chart(file))),
                Err(err) => show_error(err.context(itl!("deeplink-dl-failed"))),
            }
            self.deeplink_dl = None;
        }
        // 用户确认批量导入：启动后台任务逐条解包导入，主线程只负责收集结果。
        if self.batch_import_confirm.swap(false, Ordering::Relaxed) {
            if let Some((file, _info)) = self.batch_import.take() {
                // 建立进度通道：后台仅「产出」结果，入库与落盘统一在主线程完成。
                let (tx, rx) = mpsc::channel();
                self.batch_import_rx = Some(rx);
                self.batch_imported_charts.clear();
                self.batch_import_task = Some(Task::new(async move {
                    let mut archive = zip::ZipArchive::new(BufReader::new(File::open(&file)?))?;
                    let charts_dir = dir::charts()?;
                    for i in 0..archive.len() {
                        let mut file = archive.by_index(i)?;
                        // 只接受 `<目录>/<名称>.zip` 这种固定两层的内嵌谱面包，
                        // 其余条目（纯目录项、更深层级、非 zip）一律跳过，防止路径穿越。
                        let Some(name) = file.enclosed_name() else {
                            continue;
                        };
                        if name.extension().is_none_or(|it| it != "zip") {
                            continue;
                        }
                        let [Component::Normal(dir), Component::Normal(name)] = name.components().collect::<Vec<_>>()[..] else {
                            continue;
                        };
                        // zip 解析需要可随机访问的输入，因此先完整拷贝到临时文件。
                        let mut to_tempfile = || -> std::io::Result<_> {
                            let mut tf = tempfile()?;
                            std::io::copy(&mut file, &mut tf)?;
                            tf.seek(SeekFrom::Start(0))?;
                            Ok(tf)
                        };
                        // 内嵌目录决定谱面归属：`custom` 为用户自建，`download` 来自服务器。
                        match dir.to_str() {
                            Some("custom") => {
                                // 自建谱面：走标准导入流程，生成新的本地目录。
                                let tf = to_tempfile()?;
                                let (chart, warnings) = import_chart(tf)
                                    .await
                                    .with_context(|| itl!("batch-import-failed-chart", "chart" => name.display().to_string()))?;
                                let _ = tx.send(ImportChart::Imported(Box::new(chart), warnings)).ok();
                            }
                            Some("download") => {
                                // 下载谱面：目录名即谱面 id（`download/<id>`），
                                // 已存在同 id 目录说明此前导入过，跳过以避免覆盖用户已有成绩。
                                let Some(id) = name.to_str().and_then(|it| it.strip_suffix(".zip")).and_then(|it| it.parse::<i32>().ok()) else {
                                    warn!("invalid batch import download id: {:?}", name);
                                    continue;
                                };
                                let local_path = format!("download/{id}");
                                let path = PathBuf::from(format!("{charts_dir}/{local_path}"));
                                if std::fs::exists(&path)? {
                                    let info: ChartInfo = serde_yaml::from_reader(File::open(path.join("info.yml"))?)?;
                                    let _ = tx.send(ImportChart::Skipped(info.name));
                                    continue;
                                }
                                std::fs::create_dir(&path)?;
                                let tf = to_tempfile()?;
                                let (chart, warnings) = import_chart_to(&path, local_path, tf)
                                    .await
                                    .with_context(|| itl!("batch-import-failed-chart", "chart" => name.display().to_string()))?;
                                let _ = tx.send(ImportChart::Imported(Box::new(chart), warnings)).ok();
                            }
                            // 未知目录：只告警并跳过，避免把无关文件装进曲库。
                            _ => {
                                warn!("invalid batch import dir: {:?}", dir);
                            }
                        }
                    }
                    Ok(())
                }));
            }
        }

        // 后台每帧最多回传一条结果。这里只做收集，入库统一在任务结束时进行，
        // 因此单条导入失败不会让数据停留在「已部分入库」的状态。
        if let Some(rx) = &mut self.batch_import_rx {
            match rx.try_recv() {
                Ok(chart) => {
                    self.batch_imported_charts.push(chart);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    // 通道断开说明后台线程 panic，收集到的数据不完整，放弃本轮。
                    warn!("import thread panicked");
                    self.batch_import_rx = None;
                }
            }
        }

        // 批量导入任务结束：整体失败则回滚本批次已落盘的谱面目录（保证目录与配置一致）；
        // 成功则一次性入库、落盘、通知曲库刷新，并汇总「成功 / 跳过 / 警告」信息展示给用户。
        if let Some(task) = &mut self.batch_import_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        // 回滚：删除本批次已导入的目录，避免留下无记录可查的孤儿谱面目录。
                        let charts = dir::charts()?;
                        for chart in self.batch_imported_charts.drain(..) {
                            if let ImportChart::Imported(chart, _) = chart {
                                let path = format!("{charts}{}", chart.local_path);
                                let _ = std::fs::remove_dir_all(path);
                            }
                        }
                        show_error(err.context(itl!("batch-import-failed")));
                    }
                    Ok(()) => {
                        // 提交：逐条入库，同时把解析警告按谱面名汇总、把跳过的谱面名拼成一行。
                        let mut warning_messages = vec![];
                        let data = get_data_mut();
                        let mut count = 0;
                        let mut skipped = String::new();
                        for chart in self.batch_imported_charts.drain(..) {
                            match chart {
                                ImportChart::Imported(chart, warnings) => {
                                    if let Some(warn) = parse_warnings_to_string(&warnings) {
                                        warning_messages.push(format!("{}\n{warn}", chart.info.name));
                                    }
                                    data.charts.push(*chart);
                                    count += 1;
                                }
                                ImportChart::Skipped(name) => {
                                    if !skipped.is_empty() {
                                        skipped.push_str(", ");
                                    }
                                    skipped.push_str(&name);
                                }
                            }
                        }
                        save_data()?;
                        self.state.reload_local_charts();
                        NEED_UPDATE.store(true, Ordering::Relaxed);

                        let mut message = itl!("batch-import-success", "count" => count);
                        if !skipped.is_empty() {
                            message.push('\n');
                            message += &itl!("batch-import-downloaded-skipped", "charts" => skipped);
                        }

                        if !warning_messages.is_empty() {
                            message += "\n\n";
                            message += &warning_messages.join("\n\n");
                        }
                        Dialog::simple(message).show();
                    }
                }
                // 无论成败都释放任务与通道；下一轮导入会重新建立它们。
                self.batch_import_task = None;
                self.batch_import_rx = None;
            }
        }

        // 联机按钮位置落盘：由拖拽结束时设置的延时触发（拖拽停止 1 秒后），写完即清空标志。
        if self.mp_save_pos_at.is_some_and(|it| it < Instant::now()) {
            std::fs::write(position_file()?, format!("{},{}", self.mp_btn_pos.x, self.mp_btn_pos.y))?;
            self.mp_save_pos_at = None;
        }

        Ok(())
    }

    /// 绘制整帧，顺序即层级顺序：背景条纹 → 页面（转场时含旧页面）→ 标题 →
    /// 返回键 → 页面顶层元素 → 联机悬浮按钮与面板 → 各种全屏覆盖层。
    ///
    /// 转场中前后两层页面都要渲染（见 `update` 的说明）；`fader` 的 `sub`/`distance`
    /// 会在渲染前后被临时改写，让两层页面分别按「旧栈偏移」和「新栈偏移」绘制。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        set_camera(&ui.camera());

        // 背景条纹：用材质 uniform 推进条纹流动的相位（0.025 的系数即流动速度），
        // 画完立刻切回默认材质，避免影响后续所有绘制。
        STRIPE_MATERIAL.set_uniform("time", ((tm.real_time() * 0.025) % (std::f64::consts::PI * 2.)) as f32);
        gl_use_material(*STRIPE_MATERIAL);
        ui.fill_rect(ui.screen_rect(), (*self.background, ui.screen_rect()));
        gl_use_default_material();

        let s = &mut self.state;
        s.update(tm);

        // 1. page
        // 转场中先画旧栈顶页面：把 fader 的位移取反并衰减为 -0.6 倍，
        // 使新旧页面朝相反方向移动，形成「擦肩而过」的层次感，画完立刻还原。
        if s.fader.transiting() {
            let pos = self.pages.len() - 2;
            let old = s.fader.distance;
            s.fader.distance *= -0.6;
            self.pages[pos].render(ui, s)?;
            s.fader.distance = old;
        }
        // 画新栈顶页面：先把 fader 标记为子层级并复位，页面内部据此按「新页面入场进度」绘制。
        s.fader.sub = true;
        s.fader.reset();
        self.pages.last_mut().unwrap().render(ui, s)?;
        s.fader.sub = false;

        // 2. title
        // 标题取自栈顶页面的 `label()`；转场时对新旧标题做交叉淡入淡出，
        // 因此旧页面的标题要单独渲染一次。
        if s.fader.transiting() {
            let pos = self.pages.len() - 2;
            s.fader.reset();
            s.fader.render_title(ui, s.t, &self.pages[pos].label());
        }
        s.fader.for_sub(|f| f.render_title(ui, s.t, &self.pages.last().unwrap().label()));

        // 3. back
        // 仅有子页面（栈长 >= 2）时才显示返回键；其纵向偏移由子转场进度驱动，
        // 使返回键在入场时从上边缘滑下（`dy` 为裁剪量，配合 rect 高度实现）。
        if self.pages.len() >= 2 {
            let r = ui.back_rect();
            self.btn_back.set(ui, r);
            // `1 =>` 分支在 `len() >= 2` 的守卫下实际不可达，仅为匹配分支齐全而保留。
            let dy = (match self.pages.len() {
                1 => 1.,
                2 => s.fader.for_sub(|f| f.progress(s.t)),
                _ => 0.,
            } * r.h)
                .clamp(0., r.h);
            let ir = Rect::new(r.x, r.y + dy, r.w, r.h);
            ui.fill_rect(Rect::new(r.x, r.y + dy, r.w, r.h - dy), (*self.icon_back, ir, ScaleType::Fit));
        }

        // 页面顶层元素（悬浮按钮、弹窗等）绘制在返回键之上，保证它们不被返回键遮挡。
        self.pages.last_mut().unwrap().render_top(ui, s)?;

        // 联机悬浮按钮与面板：位置每帧钳制在屏幕内（`ui.top` 为纵向半高，逻辑坐标下屏幕为 x∈[-1,1]、y∈[-top,top]），
        // 命中区域随位置更新，因此拖动后仍能正确点击。
        if get_data().config.mp_enabled {
            let r = 0.06;
            self.mp_btn_pos.y = self.mp_btn_pos.y.clamp(-ui.top, ui.top);
            self.mp_btn_pos.x = self.mp_btn_pos.x.clamp(-1., 1.);
            ui.fill_circle(self.mp_btn_pos.x, self.mp_btn_pos.y, r, ui.background());
            let r = Rect::new(self.mp_btn_pos.x, self.mp_btn_pos.y, 0., 0.).feather(r);
            self.mp_btn.set(ui, r);
            let r = r.feather(-0.02);
            ui.fill_rect(r, (*self.mp_icon, r));

            // 面板自身绘制（含遮罩、面板体、消息区与各种覆盖层）。
            MP_PANEL.with(|it| {
                if let Some(panel) = it.borrow_mut().as_mut() {
                    panel.render(tm, ui);
                }
            });
        }

        // 全屏覆盖层最后绘制，保证压在所有内容之上；
        // 同时它们也是 `touch` 中优先消费输入的那批对象（绘制顺序与输入优先级一致）。
        if self.import_task.is_some() {
            ui.full_loading(itl!("importing"), s.t);
        }
        if let Some(dl) = &mut self.deeplink_dl {
            dl.render(ui, s.t);
        }
        if let Some(it) = &mut self.deeplink_chart {
            it.render(ui, s.t);
        }
        if self.batch_import_task.is_some() {
            // 批量导入的进度用「已收集条数 / 总条数」近似表示（后台逐条回传）。
            let current = self.batch_imported_charts.len();
            let total = self.batch_import_total;
            ui.full_loading(itl!("batch-importing", "current" => current, "total" => total), s.t);
        }

        Ok(())
    }

    /// 引擎每帧询问是否有场景切换请求，优先级为：
    /// 1. 深链谱面详情（一次性的，且已提前淡出 BGM）；
    /// 2. 联机面板（进入对局，或对局结束后返回）；
    /// 3. 栈顶页面自身的 `next_scene` 请求（如进入设置/查询页）。
    ///
    /// 只要返回非 `None` 就先淡出 BGM，避免引擎切换场景的瞬间音乐被硬切断。
    fn next_scene(&mut self, _tm: &mut TimeManager) -> NextScene {
        if let Some(next) = self.deeplink_scene.take() {
            if let Some(bgm) = &mut self.bgm {
                let _ = bgm.fade_out(0.5);
            }
            return next;
        }
        // 联机面板优先于页面：面板是覆盖层，其请求（进入对局）应最先响应；
        // 面板没有请求时再问栈顶页面。
        let res = MP_PANEL
            .with(|it| it.borrow_mut().as_mut().and_then(|it| it.next_scene()))
            .unwrap_or(self.pages.last_mut().unwrap().next_scene(&mut self.state));
        if !matches!(res, NextScene::None) {
            if let Some(bgm) = &mut self.bgm {
                let _ = bgm.fade_out(0.5);
            }
        }
        res
    }
}

/// 背景斜向条纹的动画材质，惰性初始化一次后全局复用。
///
/// `time` uniform 每帧由 `render` 推进以驱动条纹流动；shader 源码是编译进二进制的
/// 常量，加载失败只可能是没有可用的 OpenGL 上下文，此时 `unwrap` panic 是合理的。
static STRIPE_MATERIAL: Lazy<Material> = Lazy::new(|| {
    load_material(
        shader::VERTEX,
        shader::FRAGMENT,
        MaterialParams {
            uniforms: vec![("time".to_owned(), UniformType::Float1)],
            ..Default::default()
        },
    )
    .unwrap()
});

/// 背景条纹效果的 GLSL 源码（ES 1.00），与 macroquad 的默认着色器风格保持一致。
/// 单独成模块便于把源码与使用它的材质定义分开放置。
mod shader {
    /// 顶点着色器：常规 MVP 变换，额外把屏幕坐标与纹理坐标传给片元着色器。
    pub const VERTEX: &str = r#"#version 100
attribute vec3 position;
attribute vec2 texcoord;
attribute vec4 color0;

varying lowp vec4 color;
varying lowp vec2 pos0;
varying lowp vec2 uv;

uniform mat4 Model;
uniform mat4 Projection;

void main() {
    gl_Position = Projection * Model * vec4(position, 1);
    color = color0 / 255.0;
    pos0 = position.xy;
    uv = texcoord;
}"#;

    /// 片元着色器：沿 0.66 弧度的方向计算条纹相位（`w`），相位对 0.02 取模后
    /// 落在 (0, 0.012] 的区间按 0.07 的比例混入白色，于是得到一组宽度固定、
    /// 间距固定的斜向亮条纹；`time` 控制条纹整体滚动，方向由 `pos0` 决定。
    pub const FRAGMENT: &str = r#"#version 100
precision highp float;

varying lowp vec4 color;
varying lowp vec2 pos0;
varying lowp vec2 uv;

uniform sampler2D Texture;
uniform float time;

void main() {
    float angle = 0.66;
    float w = sin(angle) * pos0.y + cos(angle) * pos0.x - time;
    float t = mod(w, 0.02);
    float p = step(t, 0.012) * 0.07;
    gl_FragColor = texture2D(Texture, uv);
    gl_FragColor += (vec4(1.0) - gl_FragColor) * p;
}"#;
}
