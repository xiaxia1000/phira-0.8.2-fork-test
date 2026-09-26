//! # Phira 应用层入口与全局状态
//!
//! 本 crate 是 Phira（Phigros 社区复刻）的应用层：它持有整个进程唯一的全局可变状态
//! （`DATA`、`MESSAGES_TX`、`DATA_PATH` 等），负责各平台初始化、启动 `prpr` 核心引擎的
//! `Main`，并把宿主（桌面窗口 / Android JNI / iOS / OpenHarmony NAPI）回调桥接到主循环。
//!
//! ## 线程与时序
//! 真正的游戏循环只跑在宿主线程：`quad_main` 在该线程里调用 `macroquad::Window::from_config`，
//! 由它驱动 `the_main` 中的 `'app: loop`，因此 `Main` 的 `update`/`render` 始终在宿主线程执行。
//! 其余线程（Android 的 JNI 回调线程、ohos 的 NAPI 线程）不得直接触碰 `Data` 或场景状态，
//! 只能通过 `MESSAGES_TX` 投递暂停/恢复消息，或经 `prpr` 侧的输入通道（`CHOSEN_FILE`、
//! `INPUT_TEXT`）把用户操作送回主线程，避免跨线程数据竞争。
//!
//! ## 平台差异
//! `DATA_PATH` / `CACHE_DIR` 在窗口创建后才被填充：ohos 在 `the_main` 开头硬编码沙箱路径，
//! iOS 在 `init_assets()` 之后按沙箱规则查询 Library/Caches，Android 由 JNI 主动回填，
//! 桌面端保持为空即退化为相对当前工作目录的 `.`。所有目录访问都经 `dir::*` 统一拼接。

prpr_l10n::tl_file!("common" ttl crate::);

// 闭源构建（官方发行版的 `cfg(closed)`）专属模块：资源解密、违禁词词库等不便公开的实现。
#[rustfmt::skip]
#[cfg(closed)]
mod inner;

// 界面过渡/缓动动画基元。
mod anim;
// 违禁词（敏感词）检测：本地离线自动机，`aa` feature 下可同步校验。
mod censor;
// 「谱面」页：本地与云端谱面的列表、搜索、排序。
mod charts_view;
// HTTP 客户端与网络实体（`Client`、`UserManager`、`Chart`、`User` 等）。
mod client;
// 本地持久化根 `Data`（见 `data.rs`）。
mod data;
// 深链处理：`phira://` 协议与从外部拉起谱面/链接。
pub mod deeplink;
// 图标字形与图标字体封装。
mod icons;
// 图片资源加载与缓存（本地图片、用户头像等）。
mod images;
// 登录流程（账号密码登录与 HYKB 好游快爆 SDK 桥接）。
mod login;
// 多人联机（房间、同步）。
mod mp;
// 页面容器与转场框架。
mod page;
// 弹窗/提示（Toast、警告、确认框等）。
mod popup;
// 成绩评级（分数与 rating）展示。
mod rate;
// 资源包（respack）管理。
mod resource;
// 场景聚合与场景公共工具（见 `scene.rs`）；`Scene` trait 本身位于 `prpr::scene`。
mod scene;
// 「我的」页面各标签页。
mod tabs;
// 谱面标签/筛选分类。
mod tags;
// 3D 展示（谱面预览等）。
mod threed;
// 自定义 UI 标记语言（UML）的解析与渲染，用于可热更新的活动类界面。
mod uml;

use anyhow::Result;
use data::Data;
use macroquad::prelude::*;
use prpr::{
    build_conf,
    core::{init_assets, PGR_FONT},
    ext::SafeTexture,
    log,
    scene::show_error,
    time::TimeManager,
    ui::{cleanup_audio, FontArc, TextPainter},
    Main,
};
use prpr_l10n::set_prefered_locale;
#[cfg(not(feature = "hykb"))]
use prpr_l10n::{GLOBAL, LANGS};
use scene::MainScene;
use std::{
    collections::VecDeque,
    sync::{mpsc, Mutex},
};
use tracing::{error, info};

// Android 专用：JNI 类型（方法签名与字符串参数均来自宿主 Java 层的 QuadNative/HYKB 桥接）。
#[cfg(target_os = "android")]
use jni::{
    objects::{JClass, JString},
    sys::jint,
    EnvUnowned,
};

// 暂停/恢复信号通道的发送端。由 `on_pause_resume`（宿主渲染上下文回调）与 Android JNI 回调
// 从**非主线程**写入；`the_main` 的主循环持有接收端，在场景暂停时用阻塞 `recv` 挂起本线程。
// 之所以绕一圈走 channel 而不直接调用 `main.pause()`，是因为回调线程不持有 `Main`，
// 且 `Main` 并非线程安全——通道把「跨线程通知」与「主线程执行」解耦。
static MESSAGES_TX: Mutex<Option<mpsc::Sender<bool>>> = Mutex::new(None);
// 可写数据根目录，由宿主在窗口创建后回填：Android 经 `setDataPath` 传入应用私有目录，
// iOS 在 `the_main` 内查询沙箱 Library 目录，ohos 直接写死沙箱路径。`None` 表示用 `.`（桌面）。
static DATA_PATH: Mutex<Option<String>> = Mutex::new(None);
// 缓存目录（临时文件、图片缓存）。Android 走 `setTempDir` 并同步 `TMPDIR` 环境变量；
// ohos 用沙箱 cache 子目录；iOS 用相对 `DATA_PATH` 的 Caches；桌面端回退到 `data/../cache`。
static CACHE_DIR: Mutex<Option<String>> = Mutex::new(None);
// 全局唯一的持久化数据根（`data.json`）。`unsafe static mut` 是因为它需要在 `the_main` 里
// 先读取配置再整体替换，而 `Main` 及所有场景都要求 `&'static` 访问；读写一律通过下方
// `get_data` / `get_data_mut` / `set_data` 收口，且只在主线程发生。
pub static mut DATA: Option<Data> = None;

// OpenHarmony 的 NAPI 导出宏（`#[napi]`），把 Rust 函数暴露给 ArkTS 侧调用。
#[cfg(target_env = "ohos")]
use napi_derive_ohos::napi;

/// 读取并解密内置资源（仅 `cfg(closed)` 的闭源发行版存在）。
///
/// 桌面上内置资源是明文文件、移动端是打包后的密文，闭源构建通过 `inner::resolve_data`
/// 抹平这一差异，调用方无需关心运行平台。
///
/// # Panics
/// 文件不存在时直接 `unwrap`  panic——内置资源缺失属于打包错误，应当立即暴露。
#[cfg(closed)]
pub async fn load_res(name: &str) -> Vec<u8> {
    let bytes = load_file(name).await.unwrap();
    inner::resolve_data(bytes)
}

/// 读取内置资源并解码为纹理。
///
/// 开源构建（`not(closed)`）没有加密资源，一律返回占位黑图 `BLACK_TEXTURE`，
/// 以便同一份上层代码在两种构建下都能编译、并在缺失资源时安全降级。
#[allow(unused)]
pub async fn load_res_tex(name: &str) -> SafeTexture {
    #[cfg(closed)]
    {
        let bytes = load_res(name).await;
        let image = image::load_from_memory(&bytes).unwrap();
        image.into()
    }
    #[cfg(not(closed))]
    prpr::ext::BLACK_TEXTURE.clone()
}

/// 把内存中的 [`DATA`] 同步给依赖全局单例的子系统，**不落盘**。
///
/// 具体做两件事：确定并应用界面语言（未设置时按 `hykb` 分支决定默认值，非 HYKB 构建取
/// `GLOBAL.order` 的首个可用语言），以及把登录 token 灌进网络客户端。
/// 与 [`save_data`] 的分工是：`sync_data` 管「运行时生效」，`save_data` 管「持久化到磁盘」；
/// 修改 token / 语言后应当两者都调用（参见 [`force_logout`]）。
pub fn sync_data() {
    if get_data().language.is_none() {
        #[cfg(feature = "hykb")]
        let default_lang = "zh-CN".to_owned();
        #[cfg(not(feature = "hykb"))]
        let default_lang = LANGS[GLOBAL.order.lock().unwrap()[0]].to_owned();
        get_data_mut().language = Some(default_lang);
    }
    set_prefered_locale(get_data().language.as_ref().and_then(|it| it.parse().ok()));
    let _ = client::set_access_token_sync(get_data().tokens.as_ref().map(|it| &*it.0));
}

/// 用一份完整的 `Data` 替换全局状态，仅应在 `the_main` 启动流程中调用一次。
pub fn set_data(data: Data) {
    unsafe {
        DATA = Some(data);
    }
}

/// 取得全局 `Data` 的共享引用。
///
/// 所有场景的渲染都依赖它，因此返回 `'static` 引用；仅在主线程调用。
///
/// # Panics
/// 在 `the_main` 调用 [`set_data`] 之前调用会 panic——启动早期不应有场景存在。
#[allow(static_mut_refs)]
pub fn get_data() -> &'static Data {
    unsafe { DATA.as_ref().unwrap() }
}

/// 取得全局 `Data` 的可变引用（用于修改用户设置、收藏、本地谱面索引等）。
///
/// 只允许主线程在**不持有其他 `&Data` 的同一语句**中使用，避免别名可变引用。
///
/// # Panics
/// 同 [`get_data`]，`set_data` 之前调用会 panic。
#[allow(static_mut_refs)]
pub fn get_data_mut() -> &'static mut Data {
    unsafe { DATA.as_mut().unwrap() }
}

/// 把全局 `Data` 序列化写入 `<root>/data.json`。
///
/// 写入是**整体覆盖**而非增量，因此调用点须确保内存中的 `Data` 已是最新；
/// 目录由 `dir::root()` 保证存在（`ensure` 中会创建），故此处不再单独建目录。
///
/// # Errors
/// 序列化失败或磁盘写入失败时返回错误，调用方通常以 `let _ =` 忽略并保留内存态。
pub fn save_data() -> Result<()> {
    std::fs::write(format!("{}/data.json", dir::root()?), serde_json::to_string(get_data())?)?;
    Ok(())
}

// 全部磁盘路径的唯一出口：把相对子路径拼到 `DATA_PATH`（桌面端为空则退化为 `.`）之下，
// 并保证目录存在。除 `bold_font_path` 外都返回**相对根目录的字符串**而非 `PathBuf`，
// 因为它们要原样写进 `data.json` 的 `local_path` 字段，从而在换设备/换路径后仍能解析。
mod dir {
    use anyhow::Result;

    use crate::{CACHE_DIR, DATA_PATH};

    /// 拼出 `<DATA_PATH>/<s>` 并确保该目录存在，返回拼接后的字符串。
    ///
    /// 这是所有目录函数的基础：`DATA_PATH` 未设置（桌面端）时前缀为 `.`，
    /// 即相对进程当前工作目录——而 `init_assets()` 会改变工作目录，故桌面端路径
    /// 必须在 `init_assets()` 之后再解析。
    fn ensure(s: &str) -> Result<String> {
        let s = format!("{}/{}", DATA_PATH.lock().unwrap().as_ref().map(|it| it.as_str()).unwrap_or("."), s);
        let path = std::path::Path::new(&s);
        if !path.exists() {
            std::fs::create_dir_all(path)?;
        }
        Ok(s)
    }

    /// 缓存根目录：宿主显式指定过 [`CACHE_DIR`] 就用它，否则在数据根下建 `cache`。
    pub fn cache() -> Result<String> {
        if let Some(cache) = &*CACHE_DIR.lock().unwrap() {
            ensure(cache)
        } else {
            ensure("cache")
        }
    }

    /// 粗体字体文件的路径。注意这里**不创建目录**，因为字体随资源分发、无需自建。
    pub fn bold_font_path() -> Result<String> {
        Ok(format!("{}/bold.ttf", root()?))
    }

    /// 图片缓存目录（下载的头像等），位于 `cache/` 之下。
    pub fn cache_image_local() -> Result<String> {
        ensure(&format!("{}/image", cache()?))
    }

    /// 数据根目录，`data.json` 与本地谱面、收藏数据都位于其下。
    pub fn root() -> Result<String> {
        ensure("data")
    }

    /// 本地谱面根目录，其下分 `custom/` 与 `download/` 两类。
    pub fn charts() -> Result<String> {
        ensure("data/charts")
    }

    /// 收藏夹（collection）信息目录，每个收藏夹一个 `<uuid>.json`。
    pub fn collections() -> Result<String> {
        ensure("data/collections")
    }

    /// 玩家手动导入的谱面目录，每个谱面一个随机 UUID 子目录。
    pub fn custom_charts() -> Result<String> {
        ensure("data/charts/custom")
    }

    /// 从服务器下载的谱面目录，子目录名即谱面的网络 id。
    pub fn downloaded_charts() -> Result<String> {
        ensure("data/charts/download")
    }

    /// 资源包（respack）安装目录。
    pub fn respacks() -> Result<String> {
        ensure("data/respack")
    }
}

/// 应用真正的启动流程与主循环，由 [`quad_main`] 交给 macroquad 在宿主线程上驱动。
///
/// 返回后 `Window::from_config` 才会返回，因此本函数结束即意味着进程即将退出；
/// 中途任何致命错误都由调用方（`quad_main` 的闭包）记录日志，而不是 panic。
///
/// # Errors
/// 资源加载、`Data` 初始化或 `Main` 构建失败时向上抛出，交由调用方统一 `error!` 处理。
async fn the_main() -> Result<()> {
    // 阶段 1：注册全局日志订阅者，之后所有 `tracing` 宏才有效。
    log::register();
    // 阶段 2（平台）：ohos 的沙箱路径无法由 ArkTS 侧传入，只能硬编码；
    // DPI 也在此写死一个经验值，避免首帧按错误的缩放布局。
    #[cfg(target_env = "ohos")]
    {
        *DATA_PATH.lock().unwrap() = Some("/data/storage/el2/base".to_owned());
        *CACHE_DIR.lock().unwrap() = Some("/data/storage/el2/base/cache".to_owned());
        prpr::core::DPI_VALUE.store(250, std::sync::atomic::Ordering::Relaxed);
    };

    // 阶段 3：初始化引擎资源。**注意此调用会改变进程的工作目录**，因此
    // 之后所有相对路径（`dir::*`、`load_file`）都以资源目录为基准。
    init_assets();

    // 阶段 4：起一个 4 线程 tokio runtime，并用 `enter()` 让后续同步代码也能
    // `spawn`/使用 reactor；`_guard` 必须活到函数结束，否则 runtime 提前析构。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let _guard = rt.enter();

    // 阶段 5（平台）：iOS 不能写绝对路径，只能按沙箱规则查询 Library 目录；
    // 缓存则用相对 DATA_PATH 的 `Caches` 子目录（iOS 约定缓存可被系统清理）。
    #[cfg(target_os = "ios")]
    {
        use objc2_foundation::{NSSearchPathDirectory, NSSearchPathDomainMask, NSSearchPathForDirectoriesInDomains};

        let directories = NSSearchPathForDirectoriesInDomains(NSSearchPathDirectory::LibraryDirectory, NSSearchPathDomainMask::UserDomainMask, true);
        let path = directories.firstObject().unwrap().to_string();
        *DATA_PATH.lock().unwrap() = Some(path);
        *CACHE_DIR.lock().unwrap() = Some("Caches".to_owned());
    }

    // 阶段 6：读 `data.json`。文件缺失或解析失败一律降级为 `Data::default()`，
    // 使首次启动、升级后字段缺失、文件损坏三种情况都能继续运行而不是崩溃。
    let dir = dir::root()?;
    let mut data: Data = std::fs::read_to_string(format!("{dir}/data.json"))
        .map_err(anyhow::Error::new)
        .and_then(|s| Ok(serde_json::from_str(&s)?))
        .unwrap_or_default();
    // 阶段 7：执行迁移与扫描（旧收藏结构、本地谱面目录、资源包等），见 `Data::init`。
    data.init().await?;
    // 阶段 8：安装全局状态，并把内存态同步给语言/网络子系统后立即回写磁盘，
    // 以固化 `init()` 产生的迁移结果（否则迁移会每次启动重复执行）。
    set_data(data);
    sync_data();
    save_data()?;

    // Warm up the offline banned-word automaton so local edits can check
    // synchronously. No-op without the `aa` feature.
    // 中文补充：非闭源构建下 `censor::preload` 是空实现，故该 `spawn` 无副作用；
    // 预热放在后台任务里，避免阻塞首帧。
    tokio::spawn(censor::preload());

    // 阶段 9：建立暂停/恢复消息通道。接收端留在本函数（主线程），发送端存入全局
    // `MESSAGES_TX` 供宿主回调线程投递——这是跨线程通知唯一被允许的入口。
    let rx = {
        let (tx, rx) = mpsc::channel();
        *MESSAGES_TX.lock().unwrap() = Some(tx);
        rx
    };

    // 阶段 10：向 macroquad 的渲染上下文注册生命周期回调（Android 切前后台、
    // 桌面最小化等），回调只会 `send` 消息，真正的暂停/恢复在主循环里执行。
    unsafe { get_internal_gl() }
        .quad_context
        .display_mut()
        .set_pause_resume_listener(on_pause_resume);

    // 阶段 11：加载两套字体。`phigros.ttf` 专供游戏内数字/谱面风格的文本（PGR_FONT），
    // 是 thread_local 的 painter，需显式写入；`font.ttf` 作为界面默认字体创建 painter。
    let pgr_font = FontArc::try_from_vec(load_file("phigros.ttf").await?)?;
    PGR_FONT.with(move |it| *it.borrow_mut() = Some(TextPainter::new(pgr_font, None)));

    let font = FontArc::try_from_vec(load_file("font.ttf").await?)?;
    let mut painter = TextPainter::new(font.clone(), None);

    // 阶段 12：构建引擎驱动器并压入应用的根场景 `MainScene`。
    // 第三个参数是渲染目标选择器，`None` 表示直接绘制到窗口（不做画中画/离屏合成）。
    let mut main = Main::new(Box::new(MainScene::new(font).await?), TimeManager::default(), None).await?;

    // 阶段 13：主循环的准备。宿主只负责「开帧」，场景逻辑全部在 `prpr::Main` 内，
    // 因此这里只维护帧计时与 FPS 统计。
    let tm = TimeManager::default();
    // 上一次打印 FPS 的整秒时间戳，-1 表示还没打印过。
    let mut fps_time = -1;

    // FPS 采样窗口长度（帧）：用 60 帧滑动平均，避免瞬时抖动误报。
    const FPS_BUF_SIZE: usize = 60;
    // 最近 FPS_BUF_SIZE 帧的耗时环形缓冲；配合 `fps_time_sum` 维持 O(1) 求平均。
    let mut fps_times = VecDeque::<f32>::with_capacity(FPS_BUF_SIZE);
    // 上一帧开始的真实时间；首帧无基准，用 NaN 显式表示「未初始化」。
    let mut last_frame_start = f32::NAN;
    // 窗口内耗时之和，与 `fps_times` 保持同步增删。
    let mut fps_time_sum = 0.;

    'app: loop {
        // 阶段 a：若宿主已要求暂停（切后台），阻塞等待恢复消息。
        // 这里刻意用阻塞 `recv` 把整个线程挂起，避免后台空转耗电；
        // 通道关闭（发送端被丢弃）说明宿主即将退出，直接跳出循环。
        if main.paused() {
            match rx.recv() {
                Ok(false) => {
                    main.resume()?;
                }
                Ok(true) => {}
                Err(_) => break 'app,
            }
        }

        // 阶段 b：更新 FPS 滑动窗口——先移除过期样本，再登记本帧耗时。
        let frame_start = tm.real_time();
        if !last_frame_start.is_nan() {
            if fps_times.len() == FPS_BUF_SIZE {
                fps_time_sum -= fps_times.pop_front().unwrap();
            }
            let frame_time = frame_start as f32 - last_frame_start;
            fps_times.push_back(frame_time);
            fps_time_sum += frame_time;
        }
        last_frame_start = frame_start as f32;
        // 阶段 c：推进并绘制一帧。整段包在立即调用闭包里，把 `?` 的错误收敛到
        // 统一出口——渲染/更新出错只弹错误提示，不让主循环 panic 退出。
        // 帧内的暂停/恢复用 `try_recv` 非阻塞取一次，保证响应延迟不超过一帧。
        // 收尾调用 `flush_pending_texture_deletions`，在本帧渲染结束后真正释放
        // 被延迟销毁的纹理（删除必须等 GPU 不再引用它）。
        let res = || -> Result<()> {
            main.update()?;
            main.render(&mut painter)?;
            if let Ok(paused) = rx.try_recv() {
                if paused {
                    main.pause()?;
                } else {
                    main.resume()?;
                }
            }
            prpr::ext::flush_pending_texture_deletions();
            Ok(())
        }();
        if let Err(err) = res {
            error!("uncaught error: {err:?}");
            show_error(err);
        }
        // 阶段 d：场景栈已请求退出（`NextScene::Exit` 一路冒泡到根），结束循环。
        if main.should_exit() {
            break 'app;
        }

        // 阶段 e：FPS 统计。只在跨过整秒时打印一次，避免日志刷屏；
        // `current_fps` 是本帧瞬时值，`actual_fps` 是窗口平均值（受垂直同步封顶）。
        let t = tm.real_time();

        let fps_now = t as i32;
        if fps_now != fps_time {
            fps_time = fps_now;
            if fps_times.len() == FPS_BUF_SIZE {
                let actual_fps = 1. / (fps_time_sum / FPS_BUF_SIZE as f32);
                let current_fps = 1. / (t - frame_start);
                info!("FPS {} (capped at {})", current_fps as u32, actual_fps as u32);
            }
        }

        // While backgrounded the scene is paused; the blocking `recv_timeout`
        // above already parks this thread, so nothing extra is needed here.
        // 中文补充：`next_frame` 是 macroquad 的帧同步点，交出控制权等待宿主下一帧；
        // 由于暂停时本线程已阻塞在 `rx.recv()`，这里无需再额外降帧。
        next_frame().await;
    }
    Ok(())
}

/// 构造窗口配置：以 `prpr::build_conf()`（引擎侧的默认分辨率、采样数等）为基底，
/// 覆盖应用专属的标题与图标。
///
/// Windows 上额外从 `data.json` 里预读全屏偏好：窗口在 `the_main` 之前就创建，
/// 此时全局 [`DATA`] 尚未安装，无法通过 `get_data()` 拿到配置，只能直接读盘；
/// 其他平台的全屏由用户运行时切换，无需在建窗前决定。
fn build_global_window_conf() -> Conf {
    let mut conf = build_conf();
    conf.window_title = "Phira".to_owned();
    conf.icon = Some(miniquad::conf::Icon {
        small: *include_bytes!("../icon/small"),
        medium: *include_bytes!("../icon/medium"),
        big: *include_bytes!("../icon/big"),
    });

    // 仅 Windows 需要（其他平台的窗口全屏状态由系统/宿主保存）：
    // 读盘失败、文件不存在或解析失败都退化为 false（窗口模式），保证能正常启动。
    #[cfg(target_os = "windows")]
    {
        conf.fullscreen = dir::root()
            .ok()
            .and_then(|r| std::fs::read_to_string(std::path::Path::new(&r).join("data.json")).ok())
            .and_then(|s| serde_json::from_str::<Data>(&s).ok())
            .is_some_and(|d| d.config.fullscreen_mode);
    }

    conf
}

/// 桌面端与移动端的统一入口，由 `phira-main`（桌面可执行文件）和移动端原生层调用。
///
/// 它是 `extern "C"` 导出，宿主只需按 C ABI 调用一次；函数内部阻塞直到游戏退出。
///
/// # Safety
/// - 必须在具备图形/事件循环的**主线程**上调用，且整个进程只调用一次：
///   macroquad 的窗口与全局渲染上下文都是进程级单例。
/// - 调用期间不得再触碰任何 Rust 全局状态（会被 `the_main` 接管）。
#[no_mangle]
pub extern "C" fn quad_main() {
    macroquad::Window::from_config(build_global_window_conf(), async {
        if let Err(err) = the_main().await {
            error!(?err, "global error");
        }
    });
    // `Window::from_config` 返回即窗口/音频上下文即将销毁，显式清理音频设备，
    // 否则 Android 上残留的 AAudio 会话会拖住进程退出。
    cleanup_audio();
}

/// 宿主渲染上下文的暂停/恢复回调，只把事件转发进 [`MESSAGES_TX`]。
///
/// 该回调可能来自非主线程，且此时持有 `Main` 的主循环正阻塞在 `rx.recv()` 上，
/// 因此这里仅做一次 `send`（失败说明主循环已退出，忽略即可）。
fn on_pause_resume(pause: bool) {
    if let Some(tx) = MESSAGES_TX.lock().unwrap().as_mut() {
        let _ = tx.send(pause);
    }
}

/// JNI 生命周期最初的钩子：把 JVM 句柄交给输入法后端，使后续能弹出原生软键盘。
///
/// 函数名遵循 JNI 约定：`Java_<包名下划线转义>_<类名>_<方法名>`，`_1` 代表下划线，
/// 故本函数对应 `quad_native.QuadNative.initializeEnvironment()`。
///
/// # Safety
/// - 由 Android 侧 `QuadNative.initializeEnvironment()` 在 **JNI 线程**调用，
///   且必须在任何输入框交互之前调用一次。
/// - `env` 由 JNI 运行时传入，仅在本调用期间有效；这里只取原始指针缓存，不做长期持有。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_initializeEnvironment(env: EnvUnowned, _class: JClass) {
    unsafe {
        inputbox::backend::Android::initialize_raw(env.as_raw()).unwrap();
    }
}

/// Android 宿主 `Activity.onPause`（切后台）回调：投递「暂停」。
///
/// # Safety
/// 由 JNI 线程调用；本函数只操作全局互斥量，不访问 `Main` 或渲染上下文，
/// 因此与主线程并发是安全的。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_prprActivityOnPause(_env: EnvUnowned, _class: JClass) {
    if let Some(tx) = MESSAGES_TX.lock().unwrap().as_mut() {
        let _ = tx.send(true);
    }
}

/// Android 宿主 `Activity.onResume`（回前台）回调：投递「恢复」。
///
/// 必须与 [`Java_quad_1native_QuadNative_prprActivityOnPause`] 成对，否则主循环
/// 会一直阻塞在 `rx.recv()` 上无法继续渲染。
///
/// # Safety
/// 同 `prprActivityOnPause`，仅在 JNI 线程上操作全局通道。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_prprActivityOnResume(_env: EnvUnowned, _class: JClass) {
    if let Some(tx) = MESSAGES_TX.lock().unwrap().as_mut() {
        let _ = tx.send(false);
    }
}

/// Android 宿主 `Activity.onDestroy` 回调：直接结束进程。
///
/// 这是有意为之的「硬退出」——Android 生命周期中该回调后不会再回到游戏，
/// 走正常退出流程反而可能因等待渲染线程而卡住或留下未落盘的进度。
///
/// # Safety
/// 由 JNI 线程调用即终止进程，因此调用方必须确认当前确为销毁路径
/// （不是仅被系统回收内存的 `onStop`）；进程终止后不会再返回本函数。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_prprActivityOnDestroy(_env: EnvUnowned, _class: JClass) {
    std::process::exit(0);
}

/// 由宿主告知应用私有数据目录（`getFilesDir()` 等），写入全局 [`DATA_PATH`]。
///
/// 必须在 `dir::root()` 第一次被解析之前调用，否则数据会被写到错误的目录。
///
/// # Safety
/// 由 JNI 线程调用；`path` 的生命周期只保证在本次调用内，故立即 `to_string()` 拷贝。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_setDataPath(_env: EnvUnowned, _class: JClass, path: JString) {
    *DATA_PATH.lock().unwrap() = Some(path.to_string());
}

/// 由宿主告知临时/缓存目录，同时设置 `TMPDIR` 环境变量并写入全局 [`CACHE_DIR`]。
///
/// 设置环境变量是必要的：解压、图片编解码等第三方库直接读 `TMPDIR`，
/// 而 Android 的默认值在沙箱内不可写。
///
/// # Safety
/// 由 JNI 线程调用；`set_var` 在 Android 单线程初始化阶段执行，不会与其他线程竞争。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_setTempDir(_env: EnvUnowned, _class: JClass, path: JString) {
    let path = path.to_string();
    std::env::set_var("TMPDIR", path.clone());
    *CACHE_DIR.lock().unwrap() = Some(path);
}

/// 同步屏幕像素密度，供引擎计算 UI 缩放（不与 ohos 的硬编码值共用路径）。
///
/// # Safety
/// 由 JNI 线程调用；只写一个原子量，主线程读取，无数据竞争。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_setDpi(_env: EnvUnowned, _class: JClass, dpi: jint) {
    prpr::core::DPI_VALUE.store(dpi as _, std::sync::atomic::Ordering::SeqCst);
}

/// 回填文件选择结果：把用户选中的本地路径写入 [`prpr::scene::CHOSEN_FILE`] 的「路径」位。
///
/// 采用「写全局槽 + 主逻辑轮询」而非回调，是因为原生选择器在另一线程/另一帧才返回。
///
/// # Safety
/// 由 JNI 线程调用；只操作 `Mutex` 保护的全局槽，`file` 在调用内即被拷贝为 `String`。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_setChosenFile(_env: EnvUnowned, _class: JClass, file: JString) {
    use prpr::scene::CHOSEN_FILE;
    CHOSEN_FILE.lock().unwrap().1 = Some(file.to_string());
}

/// 接收宿主解析出的深链 URL（`phira://...`），交给 `deeplink` 模块排队处理。
///
/// # Safety
/// 由 JNI 线程调用；`deeplink::set_deeplink` 内部以互斥量保护，可与主线程并发。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_setDeepLink(_env: EnvUnowned, _class: JClass, url: JString) {
    deeplink::set_deeplink(url.to_string());
}

/// 标记「本次文件选择的目的是导入谱面」：占位请求 id 为 `_import`。
///
/// 之所以要预先写 id：会话恢复/冷启动时宿主直接给出文件，没有交互式选择过程，
/// 上层据此区分「用户主动选了文件」与「系统自动导入」两条路径。
///
/// # Safety
/// 由 JNI 线程调用；只操作 `Mutex` 保护的全局槽。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_markImport(_env: EnvUnowned, _class: JClass) {
    use prpr::scene::CHOSEN_FILE;

    CHOSEN_FILE.lock().unwrap().0 = Some("_import".to_owned());
}

/// 与 [`Java_quad_1native_QuadNative_markImport`] 同理，但占位 id 为 `_import_respack`，
/// 表示后续写入的文件按**资源包**而非谱面处理。
///
/// # Safety
/// 由 JNI 线程调用；只操作 `Mutex` 保护的全局槽。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_markImportRespack(_env: EnvUnowned, _class: JClass) {
    use prpr::scene::CHOSEN_FILE;

    CHOSEN_FILE.lock().unwrap().0 = Some("_import_respack".to_owned());
}

/// 回填原生输入框的文本结果，写入 [`prpr::scene::INPUT_TEXT`] 的「文本」位。
///
/// 只写文本不写 id，是为了让上层能凭借既有 id 判断这属于哪次输入请求。
///
/// # Safety
/// 由 JNI 线程调用；只操作 `Mutex` 保护的全局槽。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_setInputText(_env: EnvUnowned, _class: JClass, text: JString) {
    use prpr::scene::INPUT_TEXT;
    INPUT_TEXT.lock().unwrap().1 = Some(text.to_string());
}

/// Credentials obtained from the native HYKB (好游快爆) login SDK.
/// 由原生 HYKB（好游快爆）登录 SDK 回传的凭证。
///
/// 该结构只承载数据、不持有任何 JNI 引用，因此可以安全地跨线程经 channel 传递。
pub struct HykbCredential {
    /// SDK result code: 0 on success, otherwise an error / user cancellation.
    /// SDK 结果码：`0` 表示成功，非 0 为各类失败或用户取消（见 `ok_or_err`）。
    pub code: i32,
    /// HYKB 侧的用户 id，用于与 Phira 账号做绑定校验（不一致即视为登录失败）。
    pub uid: i64,
    /// HYKB 昵称，仅用于展示/初始注册，可缺失。
    pub nick: String,
    /// 用于向 Phira 服务器换取本应用会话的凭证；敏感数据，不可写入日志。
    pub access_token: String,
}

// HYKB 凭证的语义化校验：把 SDK 的结果码翻译成业务结论。
impl HykbCredential {
    /// Map the SDK result code to an error, or yield the credential on success.
    /// Centralizes the code → user-facing message translation shared by every
    /// HYKB login/bind entry point.
    ///
    /// # Errors
    /// 非 0 结果码一律返回错误；注意此处会**强制登出**（见 [`force_logout`]），
    /// 因为 HYKB 构建要求会话与 SDK 状态严格一致，任何失败都必须清掉本地登录态。
    #[cfg(feature = "hykb")]
    pub fn ok_or_err(self) -> Result<Self> {
        if self.code == 0 {
            Ok(self)
        } else {
            // A non-zero code is any failure the HYKB SDK reports: 2001 auth
            // failed, 2002 login failed, 2003 cancelled, 2004 exception, 2005
            // developer-requested exit / account logout. A HYKB build mandates a
            // valid, matching HYKB session, so every one of these must tear the
            // in-game session down — otherwise cancelling the HYKB prompt during
            // a silent re-verify would leave the player signed in and bypass the
            // gate entirely.
            force_logout();
            anyhow::bail!("{}", crate::ttl!("hykb-login-cancelled"))
        }
    }
}

/// Slot for the pending HYKB login result. The native callback fulfills it.
/// 待决 HYKB 登录结果的一次性槽位（oneshot），由原生回调负责填充。
///
/// 用 `oneshot` 而非 `mpsc`：一次登录只有一个等待者，且发送后即失效；
/// `ok_or_err` 的 `take()` 语义保证重复回调不会串台到下一次登录。
static HYKB_TX: Mutex<Option<tokio::sync::oneshot::Sender<HykbCredential>>> = Mutex::new(None);

/// Call a no-arg `void` method on the Android host activity (the HYKB shell).
/// 在 Android 宿主 Activity（HYKB 外壳）上调用一个无参 `void` 方法。
///
/// 不走 JNI 导出而是**反向**从 Rust 调 Java：登录、登出、切号都由宿主 Java 侧实现，
/// 这些方法必须在带 Looper 的 UI 线程上执行，故统一在此处 `attach_current_thread`。
///
/// # Panics
/// 未取得 `JavaVM` 单例、附加线程失败或调用失败时 panic——这些都意味着
/// 宿主与 Rust 侧的桥接配置不一致，属于不可恢复的集成错误。
#[cfg(all(target_os = "android", feature = "hykb"))]
fn call_activity_void(method: &'static jni::strings::JNIStr) {
    use jni::{jni_sig, objects::JObject, vm::JavaVM};

    JavaVM::singleton()
        .unwrap()
        .attach_current_thread(|env| -> jni::errors::Result<()> {
            let ctx = unsafe { JObject::from_raw(env, ndk_context::android_context().context() as _) };
            env.call_method(ctx, method, jni_sig!("()V"), &[])?;
            Ok(())
        })
        .unwrap();
}

/// Ask the Android shell to pop the HYKB account picker (`MainActivity.hykbSwitchAccount`).
/// Used by the explicit login / switch-account flow.
/// 请求 Android 外壳弹出 HYKB 账号选择器，用于主动登录/切换账号。
#[cfg(all(target_os = "android", feature = "hykb"))]
fn request_hykb_login() {
    call_activity_void(jni::jni_str!("hykbSwitchAccount"));
}

/// 非 HYKB 构建的空实现，保证上层调用点无需处处 `cfg`。
#[cfg(not(all(target_os = "android", feature = "hykb")))]
fn request_hykb_login() {}

/// Ask the Android shell to sign in using the cached HYKB account without
/// popping the picker (`MainActivity.hykbLogin`). The credentials the SDK
/// reports flow back through `HYKB_TX`, so the caller can verify them against
/// the restored Phira session. Used by the silent startup restore.
/// 使用缓存的 HYKB 账号静默登录（不弹选择器），凭证经 [`HYKB_TX`] 回传，
/// 供启动时的静默校验流程使用。
#[cfg(all(target_os = "android", feature = "hykb"))]
fn request_hykb_login_silent() {
    call_activity_void(jni::jni_str!("hykbLogin"));
}

/// 非 HYKB 构建的空实现。
#[cfg(not(all(target_os = "android", feature = "hykb")))]
fn request_hykb_login_silent() {}

/// Tell the native HYKB SDK to sign out (`MainActivity.hykbLogout`). Called when the
/// player logs out from their profile.
/// 通知原生 HYKB SDK 登出，用于玩家在个人资料页主动退出登录。
#[cfg(all(target_os = "android", feature = "hykb"))]
pub fn hykb_logout() {
    call_activity_void(jni::jni_str!("hykbLogout"));
}

/// 非 HYKB 构建的空实现。
#[cfg(not(all(target_os = "android", feature = "hykb")))]
pub fn hykb_logout() {}

/// Tear down the local session: sign out of the native HYKB SDK, clear the
/// stored account and tokens, then re-sync. Shared by every path that must
/// reject a login — a failed/cancelled HYKB verification, a uid mismatch, or
/// the player logging out from their profile.
///
/// 顺序很重要：先登出原生 SDK（否则下次静默登录仍会拿回同一凭证），
/// 再清内存态，最后落盘 + `sync_data`，确保磁盘与网络客户端都不残留旧 token。
pub fn force_logout() {
    hykb_logout();
    get_data_mut().me = None;
    get_data_mut().tokens = None;
    let _ = save_data();
    sync_data();
}

/// Trigger the native HYKB login and await its credentials.
/// 触发原生 HYKB 登录并等待凭证返回。
///
/// 先装好 `HYKB_TX` 再发起请求，避免凭证先于等待者到达而丢失。
///
/// # Errors
/// 发送端被丢弃（宿主进程/Activity 在回调前销毁）时报「登录已取消」。
#[allow(unused)]
pub async fn obtain_hykb_credential() -> Result<HykbCredential> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    *HYKB_TX.lock().unwrap() = Some(tx);
    request_hykb_login();
    let cred = rx.await.map_err(|_| anyhow::anyhow!("hykb login cancelled"))?;
    Ok(cred)
}

/// Silently restore the HYKB session from the cached account and await its
/// credentials. Unlike [`obtain_hykb_credential`], this does not pop the account
/// picker; used by the blocking startup check to verify the restored session.
/// 从缓存账号静默恢复 HYKB 会话并等待凭证；与 [`obtain_hykb_credential`] 的区别
/// 在于不弹选择器，因此可用于启动时阻塞式校验而不打扰用户。
///
/// # Errors
/// 同 [`obtain_hykb_credential`]，等待者被丢弃时返回错误。
#[allow(unused)]
pub async fn obtain_hykb_credential_silent() -> Result<HykbCredential> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    *HYKB_TX.lock().unwrap() = Some(tx);
    request_hykb_login_silent();
    let cred = rx.await.map_err(|_| anyhow::anyhow!("hykb login cancelled"))?;
    Ok(cred)
}

/// HYKB 登录结果的原生回调入口（`QuadNative.hykbLoginCallback`）。
///
/// 该回调有**两种来源**，因此必须区分「有等待者」与「无等待者」：
/// 主动登录时会命中 [`HYKB_TX`] 并唤醒等待的 Future；而 SDK 的异步通知
/// （防沉迷弹窗等）发生时并没有等待者，只能就地处理。
///
/// # Safety
/// 由 JNI 线程调用；`nick` / `access_token` 允许为 NULL（SDK 在失败路径不回填），
/// 故先做空指针判断再拷贝，拷贝完成后即不再引用 `JString`。
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_quad_1native_QuadNative_hykbLoginCallback(
    _env: EnvUnowned,
    _class: JClass,
    code: jint,
    uid: jni::sys::jlong,
    nick: JString,
    access_token: JString,
) {
    let nick = if nick.is_null() { String::new() } else { nick.to_string() };
    let access_token = if access_token.is_null() {
        String::new()
    } else {
        access_token.to_string()
    };
    if let Some(tx) = HYKB_TX.lock().unwrap().take() {
        let _ = tx.send(HykbCredential {
            code: code as i32,
            uid: uid as i64,
            nick,
            access_token,
        });
    } else if code == 2005 {
        // No login is in flight, so this is the SDK's asynchronous
        // anti-addiction "exit game" action: the player hit a play-time limit
        // and chose to quit from the SDK's own dialog. Honor it by exiting.
        // Other async codes (e.g. 2008 "continue playing") are handled inside
        // the SDK and need no response here. A request-less success (code 0, the
        // SDK switching accounts on its own) is likewise ignored: any signed-in
        // HYKB account is accepted, so a switch no longer tears the session down.
    }
}

// 以下 `#[napi]` 函数是 ohos（OpenHarmony）侧的导出入口，由 ArkTS 通过 NAPI 调用。
// 它们与 Android 的 JNI 版本语义一一对应：写全局输入槽、写文件选择槽、投递暂停/恢复。
// 均运行在 NAPI 线程上，因此只允许操作 `Mutex` 保护的全局槽，不得触碰场景或渲染上下文。

// 回填 ArkTS 文本输入框的结果，写入 INPUT_TEXT 的「文本」位（与 Android setInputText 等价）。
#[cfg(target_env = "ohos")]
#[napi]
pub fn set_input_text(text: String) {
    use prpr::scene::INPUT_TEXT;
    INPUT_TEXT.lock().unwrap().1 = Some(text);
}

// 回填 ArkTS 文件选择器选中的路径，写入 CHOSEN_FILE 的「路径」位。
#[cfg(target_env = "ohos")]
#[napi]
pub fn set_chosen_file(file: String) {
    use prpr::scene::CHOSEN_FILE;
    CHOSEN_FILE.lock().unwrap().1 = Some(file);
}

// 标记「本次文件由系统自动导入（如从文件管理器/分享直接拉起）」，
// 占位 id 为 `_import_auto`；与 Android 的 `_import` 区分，便于上层走不同的用户提示路径。
#[cfg(target_env = "ohos")]
#[napi]
pub fn mark_auto_import() {
    use prpr::scene::CHOSEN_FILE;
    CHOSEN_FILE.lock().unwrap().0 = Some("_import_auto".to_owned());
}

// ohos 从后台回到前台：投递「恢复」（false = 不暂停），唤醒阻塞在 recv 的主循环。
#[cfg(target_env = "ohos")]
#[napi]
pub fn on_foreground() {
    if let Some(tx) = MESSAGES_TX.lock().unwrap().as_mut() {
        let _ = tx.send(false);
    }
}

// ohos 进入后台：投递「暂停」（true），主循环随即停止 update/render 并挂起线程省电。
#[cfg(target_env = "ohos")]
#[napi]
pub fn on_background() {
    if let Some(tx) = MESSAGES_TX.lock().unwrap().as_mut() {
        let _ = tx.send(true);
    }
}
