//! # Phira 场景聚合与场景公共工具
//!
//! 注意：**`Scene` trait 不在这里**，它定义在引擎 crate 的 `prpr::scene`；本模块是 phira
//! 自己的场景子模块目录（各具体场景）加上一组被多个场景复用的公共工具：
//! 内置曲目的虚拟文件系统、TOS/隐私政策流程、谱面导入、排行榜通用渲染、下拉刷新提示等。
//! 阅读「某个界面」应当去对应的子模块（`main` / `song` / `chapter` / `profile`…），
//! 而公共状态（各类全局槽与一次性标志）集中在本文件顶部。
//!
//! 本文件里的全局量大多是为了**桥接异步/原生回调**：文件选择、文本输入、政策拉取都由
//! 外部在「另一帧」完成，场景无法同步等待，于是统一写进静态槽再由场景轮询。

prpr_l10n::tl_file!("import" itl);

// 歌曲排序/分组规则（按更新时间、难度等），供章节与歌单页共用。
mod chart_order;
pub use chart_order::ChartOrder;

// 章节（Chapter）选择页。
mod chapter;
pub use chapter::ChapterScene;

// 活动（Event）页：当前置于 crate 可见性，但场景本身需要被外部引用，故单独 re-export。
pub(crate) mod event;
pub use event::EventScene;

// 应用主场景：承载底部标签栏与各分页，是整个应用的根场景。
mod main;
pub use main::{MainScene, BGM_VOLUME_UPDATED, MP_PANEL};

// 单曲页：曲目详情、难度选择、开始游戏与成绩展示。
mod song;
pub use song::{compress_folder, Downloading, SongScene, RECORD_ID};
// 「解锁视频」场景只在启用 `video` feature 的构建里存在（不支持播放视频的平台会裁剪掉）。
#[cfg(feature = "video")]
mod unlock;
#[cfg(feature = "video")]
pub use unlock::UnlockScene;

// 用户资料页。
mod profile;
pub use profile::ProfileScene;

use crate::{
    client::{Client, UserManager},
    data::LocalChart,
    dir, get_data, get_data_mut,
    page::Fader,
    save_data,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use once_cell::sync::{Lazy, OnceCell};
use prpr::{
    config::Mods,
    core::{BOLD_FONT, PGR_FONT},
    ext::{open_url, semi_white, unzip_into, RectExt, SafeTexture},
    fs::{self, FileSystem},
    info::{ChartFormat, ChartInfo},
    parse::ParseWarnings,
    scene::{show_error, show_message, FullLoadingView, GameScene},
    task::Task,
    ui::{Dialog, RectButton, Scroll, Scroller, Ui},
};
use std::{
    any::Any,
    cell::RefCell,
    fs::File,
    io::{BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tracing::{error, info, warn};
use uuid::Uuid;

// 两张全局共享纹理。用 thread_local 而非 static，是因为纹理句柄绑定渲染上下文且非 `Send`，
// 必须在主线程访问；用 `RefCell<Option<..>>` 是因为需「初始化一次、之后多处 `clone` 取用」。
// 生命周期约定：由 MainScene 的初始化（`scene/main.rs` 中 `TEX_*.with(.. = Some(..))`）
// 一次性填充，其余场景（profile、favorites 等）只读并 `unwrap`——因此任何场景都不得早于
// MainScene 构造就访问它们。
thread_local! {
    // 首页/资料页等共用的背景插图。
    pub static TEX_BACKGROUND: RefCell<Option<SafeTexture>> = const { RefCell::new(None) };
    // 各页左上角「返回」图标，供图标集构造时组装按钮。
    pub static TEX_ICON_BACK: RefCell<Option<SafeTexture>> = const { RefCell::new(None) };
}

// 当前展示的内置曲目元信息。之所以放在全局：内置曲目没有磁盘 `info.yml`，
// 其 `ChartInfo` 由章节页在切歌时写入这里（`scene/chapter.rs`），随后被
// `AssetsChartFileSystem` 的 `:info` 伪路径读出来当作谱面信息，从而复用统一的加载流程。
pub static ASSET_CHART_INFO: Lazy<Mutex<Option<ChartInfo>>> = Lazy::new(Mutex::default);
/// External (in-browser) documents shown in the consent dialog.
/// 同意对话框里「用户协议」外链地址（用外部浏览器打开，而非内嵌页面）。
pub const TERMS_URL: &str = "https://phira.moe/terms-of-use";
/// 「隐私政策」外链地址。
pub const PRIVACY_URL: &str = "https://phira.moe/privacy-policy";
// 服务端返回的协议版本标识（通常是一个时间戳字符串）。
// 三态语义：`None` = 尚未拉取；`Some(None)` = 拉取过但内容未变更；`Some(Some(v))` = 有新版本 v。
// 用 `OnceCell` 而非普通静态，是因为一次会话内版本只会确定一次，无需要求可变。
pub static TERMS: OnceCell<Option<String>> = OnceCell::new();
// 政策拉取任务的类型别名（`None` 表示服务端答复「未修改」）。
type LoadTosTask = Task<Result<Option<String>>>;
// 正在进行的政策拉取任务。`Mutex<Option<..>>` 同时充当「是否已在拉取」的互斥标志，
// 防止多个场景每帧都重复发起网络请求；任务完成后由 [`dispatch_tos_task`] 取出并清空。
pub static LOAD_TOS_TASK: Lazy<Mutex<Option<LoadTosTask>>> = Lazy::new(Mutex::default);
// 一次性标志：玩家**刚刚**在本次运行中点了「同意」。
// 由 [`check_read_tos_and_policy`] 置位、登录流程消费（`login.rs` 里 `fetch_and(false)`），
// 用来区分「本次会话新同意」与「历史已同意」——后者不需要触发登录后的引导动作。
pub static JUST_ACCEPTED_TOS: Lazy<AtomicBool> = Lazy::new(AtomicBool::default);
// 一次性标志：政策**刚刚**拉取完成（无论是否有变更）。
// 同样由消费方 `fetch_and(false)` 取用（首页/谱面库刷新列表），避免在回调里直接持有场景引用。
pub static JUST_LOADED_TOS: Lazy<AtomicBool> = Lazy::new(AtomicBool::default);
/// Set once the policy has been verified against the server this session and
/// found unchanged, so a still-accepted policy isn't re-fetched or re-prompted.
/// 本次会话已向服务端校验过且确认「未修改」时置位。
///
/// 它比 [`JUST_LOADED_TOS`] 更「持久」：前者是给 UI 的一次性通知，本标志则让后续
/// 一切 TOS 检查直接短路返回，既不再请求网络也不再弹窗打扰玩家。
pub static TOS_VERIFIED: Lazy<AtomicBool> = Lazy::new(AtomicBool::default);

/// 内置（随包分发）曲目的**虚拟文件系统**：把引擎约定的伪路径映射到打包资源。
///
/// 引擎只认识 `prpr::fs::FileSystem`（文件/压缩包等），而内置曲目既不在磁盘上、
/// 也不在压缩包里，而是以「资源包内路径 + 内置元信息」的形式存在。因此这里实现一个
/// 最小适配器，让内置曲目与本地/网络谱面共用同一套加载代码。
///
/// 两个字段都是 `pub` 元组字段：`.0` 是曲目资源目录名（对应 `res/song/<id>/`），
/// `.1` 是该曲目具体难度对应的谱面文件名（如 `chart.json`），二者由
/// [`fs_from_path`] 从 `:name:diff` 形式的路径里拆出来。
#[derive(Clone)]
#[allow(dead_code)]
pub struct AssetsChartFileSystem(pub String, pub String);

// 实现引擎的 `FileSystem` 契约：只需要「按路径取字节」这一核心能力；
// 其余方法刻意退化（不存在任何常规文件、根目录为空），因为伪路径不是真实目录结构。
#[async_trait]
impl FileSystem for AssetsChartFileSystem {
    /// 按伪路径返回内置资源内容。
    ///
    /// 支持的伪路径只有四条：`:info`（动态生成的元信息）、`:music`（音频）、
    /// `:illu`（封面插图）、`:chart`（谱面本体）。`:info` 不依赖闭源构建，
    /// 因为元信息来自 [`ASSET_CHART_INFO`] 而非打包资源。
    ///
    /// # Errors
    /// 未识别的路径（含开源构建下缺失的 `:music`/`:illu`/`:chart`）一律 `bail!("not found")`，
    /// 让上层按「资源缺失」降级处理。
    async fn load_file(&mut self, path: &str) -> Result<Vec<u8>> {
        if path == ":info" {
            return Ok(serde_yaml::to_string(&ASSET_CHART_INFO.lock().unwrap().clone())?.into_bytes());
        }
        #[cfg(closed)]
        {
            use crate::load_res;
            if path == ":music" {
                return Ok(load_res(&format!("res/song/{}/music", self.0)).await);
            }
            if path == ":illu" {
                return Ok(load_res(&format!("res/song/{}/cover", self.0)).await);
            }
            if path == ":chart" {
                return Ok(load_res(&format!("res/song/{}/{}", self.0, self.1)).await);
            }
        }
        bail!("not found");
    }

    /// 恒为 `false`：内置资源不参与「文件是否存在」的判定分支，
    /// 上层据此走「无 info.yml，需要引擎补全默认值」的路径。
    async fn exists(&mut self, _path: &str) -> Result<bool> {
        Ok(false)
    }

    /// 恒为空：内置资源没有可枚举的目录结构（也无需枚举）。
    fn list_root(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// 需要 `dyn FileSystem` 的装箱克隆能力（`FileSystem` 本身不可 `Clone`）。
    fn clone_box(&self) -> Box<dyn FileSystem> {
        Box::new(self.clone())
    }

    /// 向上转型钩子，供调用方 `downcast_mut` 回具体的文件系统类型做特殊处理。
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// 由「谱面标识」得到可用的文件系统，是内置资源与磁盘谱面的统一入口。
///
/// 路径形态决定实现：
/// - `:name:diff`（以 `:` 开头且含第二个 `:`）：内置曲目，返回 [`AssetsChartFileSystem`]，
///   其中 `name` 是资源目录名、`diff` 是难度对应的文件名；
/// - 其他：视为相对 `data/charts` 的本地路径（`custom/<uuid>` / `download/<id>`），
///   交给引擎从目录或压缩包中打开。
///
/// # Errors
/// 路径不含第二个 `:`（内部约定的格式错误）时 `unwrap` panic；磁盘路径无法打开时返回错误。
///
/// # Panics
/// 伪路径缺少难度段（如只写了 `:name`）会 panic——这属于调用方拼错路径，应当在开发期暴露。
pub fn fs_from_path(path: &str) -> Result<Box<dyn FileSystem + Send + Sync + 'static>> {
    if let Some(name) = path.strip_prefix(':') {
        let (name, diff) = name.split_once(':').unwrap();
        Ok(Box::new(AssetsChartFileSystem(name.to_owned(), diff.to_owned())))
    } else {
        fs::fs_from_file(Path::new(&format!("{}/{path}", dir::charts()?)))
    }
}

/// 弹出一个「取消 / 确认」对话框，用户点「确认」时把 `res` 置为 `true`。
///
/// 用外部 `Arc<AtomicBool>` 作为结果载体，而不是让调用方读取对话框状态：对话框是
/// 异步弹出的（跨多帧），调用点拿到结果时早已返回，只能靠共享标志回传。
/// 两个按钮的回调都返回 `false`（= 关闭对话框）：取消不做事，确认置位后即关闭。
pub fn confirm_dialog(title: impl Into<String>, content: impl Into<String>, res: Arc<AtomicBool>) {
    Dialog::plain(title.into(), content.into())
        .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
        .listener(move |_dialog, id| {
            if id == 1 {
                res.store(true, Ordering::SeqCst);
            }
            false
        })
        .show();
}

/// TOS（用户协议）/隐私政策门禁检查：返回 `true` 表示「当前可以继续」。
///
/// 该函数被各页面**每帧调用**（因此必须廉价、且可重入），语义是「本次检查的结论」而非
/// 「是否已同意」：未拉取完、未同意、正在等待用户点击时都返回 `false`，让调用方把
/// 界面停留在加载态，下一帧继续检查。
///
/// # Arguments
/// * `change_just_accepted` — 若为 `true`，玩家本次点击「同意」时置位 [`JUST_ACCEPTED_TOS`]；
///   登录流程据此区分「本次新同意」与「历史已同意」。
/// * `strict` — 严格模式：忽略本地已存的版本号，强制与服务器核对（用于启动早期的必须确认场景）。
///
/// # Returns
/// `true` 表示门禁通过（已同意当前版本，或已确认服务器未修改协议）；`false` 表示
/// 尚未通过，调用方应继续等待下一帧。
pub fn check_read_tos_and_policy(change_just_accepted: bool, strict: bool) -> bool {
    // 第一优先：若上一帧发起的拉取任务已完成，先消化它的结果（可能直接给出结论）。
    if let Some(value) = dispatch_tos_task() {
        return value;
    }
    // 非严格模式下，本地已有同意记录即放行——避免每次进入页面都打网络请求。
    if get_data().terms_modified.is_some() && !strict {
        return true;
    }
    // Verified against the server this session and still accepted: don't prompt.
    // 严格模式下已确认「服务器未修改」也放行：本会话内无需再次校验或弹窗。
    if get_data().terms_modified.is_some() && TOS_VERIFIED.load(Ordering::Relaxed) {
        return true;
    }
    match TERMS.get() {
        Some(Some(modified)) => {
            // The player already accepted exactly this version — don't re-prompt.
            // 版本号一致说明玩家接受的正是当前版本，直接放行（字符串比较即版本比较）。
            if get_data().terms_modified.as_deref() == Some(modified.as_str()) {
                return true;
            }
            Dialog::plain(ttl!("tos-and-policy"), ttl!("tos-and-policy-desc"))
                .links(vec![
                    (ttl!("tos-link-terms").into_owned(), TERMS_URL.to_owned()),
                    (ttl!("tos-link-privacy").into_owned(), PRIVACY_URL.to_owned()),
                ])
                .on_link(|i| {
                    let url = match i {
                        0 => TERMS_URL,
                        1 => PRIVACY_URL,
                        _ => return,
                    };
                    let _ = open_url(url);
                })
                .buttons(vec![ttl!("tos-deny").into_owned(), ttl!("tos-accept").into_owned()])
                .listener(move |_dialog, pos| match pos {
                    // `-1` = 点击对话框外部、`-2` = 点击正文：返回 `true` 否决关闭，
                    // 因为协议必须由玩家显式做出选择，不能靠点空白处跳过。
                    -2 | -1 => true,
                    // 拒绝：仅提示并关闭对话框，不写入任何同意记录，下一帧会再次弹出。
                    0 => {
                        show_message(ttl!("warn-deny-tos-policy")).warn();
                        false
                    }
                    // 接受：记录本次接受的版本号并落盘，之后 `check_..` 才会放行。
                    1 => {
                        get_data_mut().terms_modified = Some(modified.clone());
                        let _ = save_data();
                        if change_just_accepted {
                            JUST_ACCEPTED_TOS.store(true, Ordering::Relaxed);
                        }
                        false
                    }
                    _ => true,
                })
                .show();
        }
        Some(None) => {
            // 服务端答复「未修改」时 `TERMS` 会停在 `Some(None)`，说明走到这里时
            // 本地却没有同意记录——状态机不应出现该组合。
            error!("unreachable")
        }
        None => {
            // 还没拉取过协议内容：就地发起拉取。非严格模式下这属于「调用时机不当」，
            // 因为会引入一次可见的网络延迟（故打 warn 提醒调用方提前预取）。
            if !strict {
                warn!("loading data to read because `check_..` was called, this would result a delay and shouldn't happen");
            }
            load_tos_and_policy(true);
        }
    }
    false
}

/// 消化已完成的协议拉取任务，返回 `Option<bool>`：`Some(..)` 表示「本次调用就能给出结论」。
///
/// 之所以要「取出任务 + 清空槽位 + 立即释放锁」：任务持有的 Future 可能在 `drop` 时
/// 触发清理逻辑，而它本身又被 `LOAD_TOS_TASK` 的锁保护，先 `drop(tos_task)` 再处理结果
/// 可避免在持锁期间执行用户可见的错误弹窗（`show_error`）造成重入死锁。
///
/// # Returns
/// * `Some(true)` —— 服务器确认协议未修改且本地已有同意记录，门禁直接放行；
/// * `Some(false)` —— 拉取失败（已弹出错误提示），门禁保持关闭；
/// * `None` —— 没有可用的结论（无任务、任务尚未完成，或拉到了新协议需要玩家重新同意）。
pub fn dispatch_tos_task() -> Option<bool> {
    let mut tos_task = LOAD_TOS_TASK.lock().unwrap();
    if let Some(task) = &mut *tos_task {
        if let Some(result) = task.take() {
            *tos_task = None;
            drop(tos_task);
            match result {
                Ok(Some(res)) => {
                    // New or changed policy: cache it and require (re-)acceptance.
                    // 协议有新版本：清掉本地的同意记录（`terms_modified = None`）并落盘，
                    // 这样后续 `check_read_tos_and_policy` 必然弹窗要求重新同意。
                    info!("terms and policy loaded");
                    get_data_mut().terms_modified = None;
                    let _ = save_data();
                    let _ = TERMS.set(Some(res));
                }
                Ok(None) => {
                    // Not modified: whatever the player accepted before is still
                    // current. Mark the session verified so we neither refetch nor
                    // prompt, and resume anything waiting on the TOS gate.
                    // 未修改：仅当本地确实有历史同意记录时才放行；否则仍需等待 TERMS
                    // 被填充（走 `check_..` 的 `None` 分支重新拉取），避免把「没同意过」误判为通过。
                    info!("terms and policy unchanged");
                    if get_data().terms_modified.is_some() {
                        TOS_VERIFIED.store(true, Ordering::Relaxed);
                        JUST_ACCEPTED_TOS.store(true, Ordering::Relaxed);
                        return Some(true);
                    }
                }
                Err(e) => {
                    // 网络失败只提示、不崩溃；返回 `false` 让上层继续等待/重试，
                    // 而不是把玩家永久挡在门外。
                    show_error(e.context(ttl!("fetch-tos-policy-failed")));
                    return Some(false);
                }
            }
            return None;
        }
    }
    drop(tos_task);
    None
}
/// use the return value to add a loading screen
/// 发起一次协议内容拉取（幂等：已在拉取或已拉取过都会直接返回）。
///
/// 幂等由两道判断保证：`TERMS` 已就绪则不再拉取；`LOAD_TOS_TASK` 非空说明上次请求仍在飞行中。
/// 请求会带上本地已接受的版本（`If-Modified-Since` 语义），使未修改时服务端直接返回
/// 「未修改」而省下一次完整正文传输。
///
/// # Arguments
/// * `show_loading` — 是否展示全屏加载遮罩；`true` 时会创建 [`FullLoadingView`]，
///   其 Drop 即结束加载动画，因此它被移进异步任务里，保证遮罩恰好覆盖整个请求周期。
pub fn load_tos_and_policy(show_loading: bool) {
    if TERMS.get().is_some() {
        return;
    }
    let mut guard = LOAD_TOS_TASK.lock().unwrap();
    if guard.is_none() {
        let modified = get_data().terms_modified.clone();
        let loading = show_loading.then(|| FullLoadingView::begin_text(ttl!("loading_tos_policy")));
        *guard = Some(Task::new(async move {
            // Always send If-Modified-Since so an unchanged, already-accepted
            // policy comes back as "not modified" (304) and we can skip the
            // prompt instead of re-fetching the full terms every time.
            let ret = Client::fetch_terms(modified.as_deref()).await.context("failed to fetch terms");
            drop(loading);
            JUST_LOADED_TOS.store(true, Ordering::Relaxed);
            ret
        }));
    }
}

/// 删除前的二次确认对话框：与 [`confirm_dialog`] 同构，但使用删除专用的文案。
#[inline]
pub fn confirm_delete(res: Arc<AtomicBool>) {
    Dialog::plain(ttl!("del-confirm").into_owned(), ttl!("del-confirm-content").into_owned())
        .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
        .listener(move |_dialog, id| {
            if id == 1 {
                res.store(true, Ordering::SeqCst);
            }
            false
        })
        .show();
}

/// 为「手动导入」的谱面分配一个互不冲突的目录 `data/charts/custom/<uuid>` 并创建它。
///
/// 目录名用 UUID 而非谱面名：谱面名可重复、可含非法字符，而 UUID 天然唯一且可用于
/// 跨设备同步的稳定标识（`LocalChart.local_path` 会记录它）。
///
/// # Returns
/// `(目录绝对/相对路径, 生成的 uuid)`——调用方写入文件后把 uuid 对应的
/// `custom/<uuid>` 作为 `local_path` 交给 [`import_chart_to`]。
///
/// # Errors
/// 目录创建失败时返回错误（磁盘满/权限问题）。
pub fn gen_custom_dir() -> Result<(PathBuf, Uuid)> {
    let dir = dir::custom_charts()?;
    let dir = Path::new(&dir);
    let mut id = Uuid::new_v4();
    // UUID v4 虽然碰撞概率极低，但目录已存在时必须重试，否则会覆盖他人谱面。
    while dir.join(id.to_string()).exists() {
        id = Uuid::new_v4();
    }
    let dir = dir.join(id.to_string());
    std::fs::create_dir(&dir)?;

    Ok((dir, id))
}

/// 把解析告警格式化成可直接展示的多行文本。
///
/// 每条告警渲染为 `- <文案>` 的一行，便于放进对话框；没有告警时返回 `None`，
/// 让调用方据此决定「不弹窗」，而不是弹一个空对话框。
pub fn parse_warnings_to_string(w: &ParseWarnings) -> Option<String> {
    let mut warnings = vec![];
    if w.has_new_speed_events {
        warnings.push(format!("- {}", itl!("warning-new-speed-event")));
    }
    if w.has_attach_ui {
        warnings.push(format!("- {}", itl!("warning-attach-ui")));
    }
    if warnings.is_empty() {
        None
    } else {
        Some(warnings.join("\n"))
    }
}

/// 对刚导入的谱面做静态检查（lint），产出可展示的兼容性告警。
///
/// 只对 RPE 格式跑 lint：`prpr::parse::lint` 分析的是 RPE 的扩展事件
/// （新速度事件、attachUI 等），其他格式解析出来没有这些概念，直接返回空告警。
/// 这一步只读不写，失败也不影响导入本身，因此由调用方决定是否容忍其错误。
///
/// # Errors
/// 读取谱面字节失败，或 lint 过程本身失败时返回错误。
async fn lint_chart(fs: &mut dyn FileSystem, info: &ChartInfo) -> Result<ParseWarnings> {
    let bytes = GameScene::load_chart_bytes(fs, info).await.context("Failed to load chart")?;
    let format = GameScene::infer_chart_format(info, &bytes);
    if format != ChartFormat::Rpe {
        return Ok(ParseWarnings::default());
    }
    let source = String::from_utf8_lossy(&bytes);
    prpr::parse::lint(&source).await
}

/// 把已解压/已就位的谱面目录补全元信息，并返回构建好的本地谱面记录。
///
/// 前置条件：`dir` 已经包含谱面文件，且 `local_path` 是**相对于 `data/charts`** 的路径
/// （如 `custom/<uuid>`），二者指向同一份内容——本函数会用 `local_path` 重新打开文件系统，
/// 而不是直接用 `dir`，从而保证写进 `LocalChart.local_path` 的标识与后续读取方式一致。
///
/// 步骤：加载 `info.yml` → 用 `fix_info_with` 补全缺失字段（无 `info.yml` 时补全力度更大）
/// → 必要时跑 lint → 把补全后的 info 写回 `info.yml`（使磁盘状态自洽，避免每次启动重复推断）。
///
/// # Errors
/// 解压失败、缺少可识别的谱面信息、`info.yml` 写入失败时返回错误，调用方负责回滚目录。
pub async fn import_chart_to(dir: &Path, local_path: String, file: File) -> Result<(LocalChart, ParseWarnings)> {
    // 阶段 1：解压。`unzip_into` 的第三个参数表示允许覆盖同名文件（重复导入同一谱面）。
    let dir = prpr::dir::Dir::new(dir)?;
    unzip_into(BufReader::new(file), &dir, true)?;
    // 阶段 2：按 `local_path` 打开文件系统并读取元信息。
    let mut fs = fs_from_path(&local_path)?;
    let mut info = fs::load_info(fs.as_mut()).await.with_context(|| itl!("info-fail"))?;
    // 阶段 3：补全元信息。`has_info_yml` 决定补全策略：没有 info.yml 说明谱面只有一个
    // 裸谱面文件，需要引擎推断出名字/难度等默认值；有 info.yml 则只修已知的坏字段。
    let has_info_yml = fs.exists("info.yml").await?;
    fs::fix_info_with(fs.as_mut(), &mut info, !has_info_yml)
        .await
        .with_context(|| itl!("invalid-chart"))?;
    // 阶段 4：兼容性检查。仅当元信息里缺少「是否使用新速度事件 / attachUI 修复」这类标记时
    // 才需要实际解析谱面（这两个字段为 None 表示来源不是本程序，可能是 RPE 工具导出）。
    // 顺带把 `use_attach_ui_fix` 补为 `true`，使首次游玩的行为与当前版本默认一致。
    let warnings = if info.use_rpe_170_speed.is_none() || info.use_attach_ui_fix.is_none() {
        if info.use_attach_ui_fix.is_none() {
            info.use_attach_ui_fix = Some(true);
        }
        lint_chart(fs.as_mut(), &info).await?
    } else {
        ParseWarnings::default()
    };
    // 阶段 5：把补全后的 info 写回磁盘，保证下次启动读到的是确定值。
    dir.create("info.yml")?.write_all(serde_yaml::to_string(&info)?.as_bytes())?;
    Ok((
        LocalChart {
            info: info.into(),
            local_path,
            record: None,
            mods: Mods::default(),
            played_unlock: false,
        },
        warnings,
    ))
}

/// 导入一个「手动选择」的谱面文件：分配 UUID 目录 → 解压导入 → 失败则整体回滚。
///
/// 回滚（删除刚创建的目录）是必要的：否则失败会留下半个谱面目录，而启动扫描逻辑
/// 会把这种残缺目录当成待导入项反复重试，最终消耗掉重试次数上限。
///
/// # Errors
/// 分配目录失败或 [`import_chart_to`] 失败时返回错误（目录已被清理）。
pub async fn import_chart(file: File) -> Result<(LocalChart, ParseWarnings)> {
    let (dir, id) = gen_custom_dir()?;
    match import_chart_to(&dir, format!("custom/{id}"), file).await {
        Err(err) => {
            std::fs::remove_dir_all(dir)?;
            Err(err)
        }
        Ok(val) => Ok(val),
    }
}

/// 排行榜的一行数据，交给 [`render_ldb`] 统一绘制。
///
/// 排名内容被拆成「可绘制字段」而非直接传网络模型：排行榜的调用方有单曲榜、活动榜等多种，
/// 它们的数据来源不同，但最终都归约成这里的五个字段，从而共用同一套行布局。
pub struct LdbDisplayItem<'a> {
    /// 玩家 id。既用于取头像与用户名颜色，也用于判断「这行是不是我自己」。
    pub player_id: i32,
    /// 名次（从 1 开始），渲染为 `#N`。
    pub rank: u32,
    /// 已格式化好的分数字符串（如 `1,000,000`），避免渲染层关心数值格式。
    pub score: String,
    /// 次级信息（如 ACC、评级），显示在分数左侧；`None` 表示该榜不展示次要指标。
    pub alt: Option<String>,
    /// 头像的命中区域，由本行渲染时写回，调用方在点击时用它跳转到资料页。
    pub btn: &'a mut RectButton,
}

/// 排行榜的通用渲染器：标题 + 可滚动列表（名次/头像/用户名/次要指标/分数）。
///
/// 抽出来的目的是让所有榜单（单曲榜、活动榜、好友榜）保持完全一致的视觉与交互，
/// 包括「高亮自己那一行」和「下拉刷新」这两处细节——它们很容易在各处实现得不一致。
///
/// # Arguments
/// * `ui` — 当前帧的 UI 上下文（本函数会推进其 dy）。
/// * `title` — 榜单标题。
/// * `w` — 列表可用宽度（世界坐标）。
/// * `rt` — 渲染时间戳，用于逐行渐入动画与加载动画。
/// * `scroll` / `fader` — 复用的滚动容器与渐入辅助器（由调用方持有以保留滚动位置）。
/// * `icon_user` — 默认头像，用户头像尚未下载完成时回退使用。
/// * `iter` — `None` 表示数据尚未就绪，此时画加载动画；`Some` 时逐项绘制。
#[allow(clippy::too_many_arguments)]
pub fn render_ldb<'a>(
    ui: &mut Ui,
    title: &str,
    w: f32,
    rt: f32,
    scroll: &mut Scroll,
    fader: &mut Fader,
    icon_user: &SafeTexture,
    iter: Option<impl Iterator<Item = LdbDisplayItem<'a>>>,
) {
    use macroquad::prelude::*;

    // 布局阶段：标题高度决定列表可视区高度；留出上下各一份边距，使列表能贴着
    // 标题下沿开始滚动而不是被裁切。
    let pad = 0.03;
    let width = w - pad;
    ui.dy(0.01);
    let r = ui.text(title).size(0.9).draw_using(&BOLD_FONT);
    ui.dy(r.h + 0.05);
    let sh = ui.top * 2. - r.h - 0.08;
    // 数据未就绪：只画加载动画并提前返回，避免调用方在迭代器为空时看到空列表。
    let Some(iter) = iter else {
        ui.loading(width / 2., sh / 2., rt, WHITE, ());
        return;
    };
    // 记下当前滚动偏移：往下拉时为负值，供「松手刷新」提示计算进度。
    let off = scroll.y_scroller.offset;
    scroll.size((width, sh));
    scroll.render(ui, |ui| {
        render_release_to_refresh(ui, width / 2., off);
        let s = 0.14;
        let mut h = 0.;
        ui.dx(0.02);
        // `fader` 以时间为输入做逐行渐入；`for_sub` 保证每行的动画各自独立推进。
        fader.reset();
        // 提前取出自己的 id：行内每项都要比较，避免重复访问全局数据。
        let me = get_data().me.as_ref().map(|it| it.id);
        fader.for_sub(|f| {
            for item in iter {
                f.render(ui, rt, |ui| {
                    // 高亮自己的记录：整行铺一层背景色，便于快速定位。
                    if me == Some(item.player_id) {
                        ui.fill_path(&Rect::new(-0.02, 0., width, s).feather(-0.01).rounded(0.02), ui.background());
                    }
                    // 左侧名次。用 PGR_FONT（游戏风格数字字体）绘制，圆形底板中心对齐。
                    let r = s / 2. - 0.02;
                    ui.text(format!("#{}", item.rank))
                        .pos((0.18 - r) / 2., s / 2.)
                        .anchor(0.5, 0.5)
                        .no_baseline()
                        .size(0.52)
                        .draw_using(&PGR_FONT);
                    // 头像：尚未下载时用 `icon_user` 占位；同时把命中区写回 item.btn，
                    // 由调用方在触摸处理里判断是否点击了头像（不是每帧都消费）。
                    let ct = (0.18, s / 2.);
                    ui.avatar(ct.0, ct.1, r, rt, UserManager::opt_avatar(item.player_id, icon_user));
                    item.btn.set(ui, Rect::new(ct.0 - r, ct.1 - r, r * 2., r * 2.));
                    // 右侧内容从右往左排版：先放次要指标（半透明白、小字号），
                    // 再放分数（PGR_FONT），两者各自把可用右边界左移，形成紧贴的排列。
                    let mut rt = width - 0.04;
                    if let Some(alt) = item.alt {
                        let r = ui
                            .text(alt)
                            .pos(rt, s / 2.)
                            .anchor(1., 0.5)
                            .no_baseline()
                            .size(0.4)
                            .color(semi_white(0.6))
                            .draw_using(&BOLD_FONT);
                        rt -= r.w + 0.01;
                    } else {
                        rt -= 0.01;
                    }
                    let r = ui
                        .text(item.score)
                        .pos(rt, s / 2.)
                        .anchor(1., 0.5)
                        .no_baseline()
                        .size(0.6)
                        .draw_using(&PGR_FONT);
                    rt -= r.w + 0.03;
                    // 中间的用户名：左对齐在固定位置，用该用户的专属颜色绘制；
                    // 限制 `max_width` 为「分数左边界 - 名字左边界」，保证长名字不会压到分数。
                    let lt = 0.25;
                    if let Some((name, color)) = UserManager::name_and_color(item.player_id) {
                        ui.text(name)
                            .pos(lt, s / 2.)
                            .anchor(0., 0.5)
                            .no_baseline()
                            .max_width(rt - lt - 0.01)
                            .size(0.5)
                            .color(color)
                            .draw();
                    }
                });
                // 每个条目占据固定高度 `s`，`h` 累计出内容总高交给滚动容器。
                ui.dy(s);
                h += s;
            }
        });
        (width, h)
    });
}

/// 绘制「下拉刷新」提示，随下拉距离上移并渐显。
///
/// # Arguments
/// * `cx` — 提示文本的水平中心。
/// * `off` — 滚动容器的纵向偏移；下拉时为负，越负说明拉得越远。
///
/// 进度 `p` 以 `Scroller::EXTEND`（触发刷新所需的下拉距离）为分母并夹取到 `[0, 1]`，
/// 因此提示会在恰好达到阈值时完全不透明，给玩家明确的「可以松手了」反馈。
pub fn render_release_to_refresh(ui: &mut Ui, cx: f32, off: f32) {
    let p = (-off / Scroller::EXTEND).clamp(0., 1.);
    ui.text(ttl!("release-to-refresh"))
        .pos(cx, -0.2 + p * 0.07)
        .anchor(0.5, 0.)
        .size(0.8)
        .color(semi_white(p * 0.8))
        .draw();
}

// 测试模块。当前内容是**故意保留为注释**的手工调试用例：它需要把一个真实的谱面目录
// 放到 workspace 根（而非 crate 内）的 `test/` 下，而该目录被 git 忽略、在 CI 上并不存在，
// 因此不能作为自动化测试运行；保留它是为了开发者需要排查谱面解析问题时能快速启用。
#[cfg(test)]
mod tests {
    // #[tokio::test]
    // #[ignore = "Chart parsing test"]
    // async fn test_parse_chart() -> Result<()> {
    //     // Put the chart in phira(workspace, not crate)/test which is ignored by git
    //     let mut fs = fs_from_path("../../../test")?;
    //     let info = load_info(fs.as_mut()).await?;
    //     let _chart = prpr::scene::GameScene::load_chart(fs.deref_mut(), &info).await?;
    //     Ok(())
    // }
}
