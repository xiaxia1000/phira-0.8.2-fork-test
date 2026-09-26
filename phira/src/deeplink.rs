//! Phira 深链（`phira://` 等自定义 URL scheme / `phira.moe` 跳转链接）解析与资源下载。
//!
//! 用途：让用户从外部浏览器、QQ/微信等应用点一个链接就能打开 App 内的目标，例如
//! 「查看某个谱面详情」或「下载并导入某个谱面包」。链接进入 App 的具体通道由平台决定
//! （桌面端是启动时的命令行参数，移动端是 intent / URL scheme 回调），本模块只负责：
//! 1. 线程安全地暂存原始链接（[`set_deeplink`] / [`take_deeplink`]），因为链接到达与
//!    被消费的时机不同：进程启动时先写入，真正的 `MainScene` 运行期才取出处理；
//! 2. 把链接解析为 [`DeepLink`]（[`parse_deeplink`]），非法输入一律报错而不做猜测；
//! 3. 提供带进度显示与用户取消的覆盖层（[`DeepLinkDownload`] / [`DeepLinkChartOpening`]）。
prpr_l10n::tl_file!("import" itl);

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use macroquad::prelude::*;
use prpr::{
    ext::{semi_black, RectExt},
    task::Task,
    ui::{DRectButton, Ui},
};
use std::{
    io::{Seek, SeekFrom, Write},
    sync::{Arc, LazyLock, Mutex},
};
use tempfile::tempfile;
use url::Url;

use crate::client::{basic_client_builder, Chart, Ptr, API_URL};

/// Upper bound for deeplink downloads from untrusted sources.
/// 中文说明：深链来自不受信任的外部来源（任意网页/聊天消息），必须限制单次下载体积，
/// 否则恶意链接可用超大文件耗尽移动端流量与磁盘。取 100 MiB 作为谱面包的宽松上限。
pub const MAX_DEEPLINK_DOWNLOAD: u64 = 100 << 20;

/// 待处理深链的全局槽位。写入与消费不在同一时刻（启动期写入、运行期消费），
/// 且可能跨线程，故用 `Mutex` 保护；只保留最新一条链接，旧链接被覆盖。
static PENDING_DEEPLINK: Mutex<Option<String>> = Mutex::new(None);

/// 写入一条待处理深链（如进程启动时把命令行参数登记进来），覆盖此前未消费的链接。
pub fn set_deeplink(input: impl Into<String>) {
    *PENDING_DEEPLINK.lock().unwrap() = Some(input.into());
}

/// 取出并清空待处理深链；没有待处理链接时返回 `None`。
/// 采用「取出即清空」语义，保证同一条链接只被处理一次（例如避免每帧重复弹下载）。
pub fn take_deeplink() -> Option<String> {
    PENDING_DEEPLINK.lock().unwrap().take()
}

/// 官方服务器主机名，用于判定深链目标是否为官方源（决定信任级别）。
/// 从 `API_URL` 中提取 host；解析失败时退化为对 URL 字符串做前后缀裁剪，保证在
/// 任何构建配置下都有一个可用的主机名，而不是 panic。
static OFFICIAL_HOST: LazyLock<String> = LazyLock::new(|| {
    Url::parse(API_URL)
        .ok()
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| API_URL.trim_start_matches("https://").trim_end_matches('/').to_owned())
});

/// 返回官方主机名，供其他模块判断资源来源是否可信。
pub fn official_host() -> &'static str {
    &OFFICIAL_HOST
}

/// 官方跳转域（短链包装域名），形如 `https://phira.moe/dlink/<action>`。
const DLINK_HOST: &str = "phira.moe";
/// 跳转域上承载动作的前缀路径，其后紧跟 `<action>` 段。
const DLINK_PATH: &str = "/dlink/";

/// 一次深链下载的目标：最终下载地址及其是否来自官方源。
#[derive(Clone)]
pub struct DeepLinkTarget {
    /// 最终要请求的下载地址（已解开可能的跳转包装）。
    pub url: Url,
    /// 是否为官方源。用于决定提示语气与信任级别；判定基于**解包后的最终 URL**，
    /// 防止用官方跳转域包装一个第三方地址来伪装成官方。
    pub official: bool,
}

/// 判断 URL 是否指向官方主机且使用默认端口。
/// 要求端口为 `None` 是为了避免 `phira.moe:8080` 之类的伪装绕过来源判定。
fn is_official(url: &Url) -> bool {
    url.host_str() == Some(&OFFICIAL_HOST) && url.port().is_none()
}

/// The `<action>` segment of an `https://phira.moe/dlink/<action>` wrapper.
/// 中文说明：仅当主机为官方跳转域且路径以 `/dlink/` 开头时返回动作名（如 `import`、
/// `chart`）；否则返回 `None`，表示这不是一条跳转包装链接。
fn dlink_action(url: &Url) -> Option<&str> {
    if url.host_str() != Some(DLINK_HOST) {
        return None;
    }
    url.path().strip_prefix(DLINK_PATH)
}

/// 读取查询参数中第一个匹配 `key` 的值（重复参数取首个）。
fn query_param(url: &Url, key: &str) -> Option<String> {
    url.query_pairs().into_owned().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// 从 `import` 链接的 `src` 参数中取出真正的下载地址。
/// 显式拒绝「跳转套跳转」：嵌套包装会让来源判定失去意义，也可能是构造出的解析绕过，
/// 因此遇到 `src` 指向又一个 `phira.moe/dlink` 包装时直接报错。
fn src_target(url: &Url) -> Result<Url> {
    let src = query_param(url, "src").context("missing src")?;
    let target = Url::parse(&src).context("invalid src")?;
    if dlink_action(&target).is_some() {
        bail!("nested wrapper");
    }
    Ok(target)
}

/// 从 `chart` 链接的 `id` 参数中解析谱面 id；缺失或非数字均报错。
fn chart_id(url: &Url) -> Result<i32> {
    query_param(url, "id").context("missing id")?.parse().context("invalid id")
}

/// What a deeplink asks the app to do.
/// 中文说明：深链语义收敛为两种动作，分别对应 `import` 与 `chart` 两个 `<action>`。
#[derive(Clone)]
pub enum DeepLink {
    /// Download a chart file and import it. `official` is judged on the final
    /// download URL, after unwrapping any wrapper.
    /// 中文说明：下载一个谱面包并在下载完成后走导入流程（进入 `ImportScene`）。
    Import(DeepLinkTarget),
    /// Open the details page of the chart with this id.
    /// 中文说明：仅需打开该 id 对应谱面的详情页，不涉及文件下载。
    Chart(i32),
}

/// Accepted forms:
/// - `phira://chart?id=<id>` or `https://phira.moe/dlink/chart?id=<id>`
/// - `phira://import?src=<url>` or `https://phira.moe/dlink/import?src=<url>`
/// 中文说明：解析分两步——先确定「动作名」，再按动作解析参数。
/// 动作来源有两条互斥路径：自定义 scheme `phira://<action>?...`（`<action>` 位于 host
/// 段），或官方跳转域 `https://phira.moe/dlink/<action>?...`。
/// 其他 scheme 或未知动作一律报错，不做兜底猜测（避免把无关链接误当指令执行）。
/// # Errors
/// URL 无法解析、scheme/host 不受支持、动作未知，或所需参数缺失/非法时返回错误。
pub fn parse_deeplink(input: &str) -> Result<DeepLink> {
    // 阶段一：解析原始链接并确定动作名。
    let url = Url::parse(input.trim())?;
    let action = if url.scheme() == "phira" {
        url.host_str().context("missing action")?
    } else if let Some(action) = dlink_action(&url) {
        action
    } else {
        bail!("unsupported scheme");
    };
    // 阶段二：按动作解析各自参数并构造结果。
    match action {
        "import" => Ok(DeepLink::Import(download_target(src_target(&url)?)?)),
        "chart" => Ok(DeepLink::Chart(chart_id(&url)?)),
        _ => bail!("unknown action"),
    }
}

/// 由下载地址构造下载目标，并先行判定来源是否为官方。
/// 只接受 `http`/`https`：拒绝 `file:` 等本地协议，防止链接诱导 App 读取本机文件。
fn download_target(url: Url) -> Result<DeepLinkTarget> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("unsupported scheme");
    }
    Ok(DeepLinkTarget {
        official: is_official(&url),
        url,
    })
}

/// Overlay for an in-progress deeplink download. Dropping it cancels the transfer.
/// 中文说明：这是本模块「取消」机制的核心——取消不靠显式标志位，而是靠**丢弃覆盖层**。
/// `prog` 以弱引用（`Weak`）传入后台任务，只有覆盖层本身持有强引用；一旦覆盖层被丢弃
/// （用户点取消、或跳转离开），后台任务下一次检查 `strong_count` 时即发现只剩自己持有的
/// 弱引用，从而主动中止。这样无需在 UI 与任务间同步一个额外的取消标记。
pub struct DeepLinkDownload {
    /// 下载进度（0..=1）；`None` 表示服务器未返回 `Content-Length`，进度未知。
    /// 用 `Arc<Mutex<..>>` 是因为背景任务与渲染线程会同时读写它。
    prog: Arc<Mutex<Option<f32>>>,
    /// 上一帧的进度值，供 `ui.loading` 做平滑插值，避免进度突变。
    loading_last: f32,
    /// 覆盖层上的「取消」按钮。
    cancel_btn: DRectButton,
    /// 后台下载任务；完成时产出已回绕到文件开头的临时文件，失败时给出错误。
    task: Task<Result<std::fs::File>>,
}

/// 启动一次深链下载，返回可渲染的覆盖层。
/// 下载内容写入匿名临时文件（`tempfile`），不占用用户可见的路径。
/// # Errors
/// 仅当覆盖层构造阶段出错时返回错误；真正的网络/IO 错误在 [`DeepLinkDownload::take_result`] 中体现。
pub fn start_deeplink_download(target: DeepLinkTarget) -> Result<DeepLinkDownload> {
    let prog = Arc::new(Mutex::new(None));
    let prog_wk = Arc::downgrade(&prog);
    Ok(DeepLinkDownload {
        prog,
        loading_last: 0.,
        cancel_btn: DRectButton::new(),
        task: Task::new(async move {
            // 阶段一：建立临时文件；若覆盖层此时已被丢弃（弱引用无法升级），直接取消。
            let mut file = tempfile()?;
            let Some(prog) = prog_wk.upgrade() else {
                bail!("cancelled");
            };
            // 阶段二：发起请求并取回响应，`error_for_status` 让 4xx/5xx 直接成为错误。
            let client = basic_client_builder().build()?;
            let res = client
                .get(target.url)
                .send()
                .await
                .context("failed to send request")?
                .error_for_status()?;
            let size = res.content_length();
            // 阶段三：流式下载，边收边写，同时更新进度、执行体积上限与取消检查。
            let mut stream = res.bytes_stream();
            let mut count: u64 = 0;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("failed to read response body")?;
                count += chunk.len() as u64;
                // 体积上限是硬性防线：服务器即使谎报或不报长度也无法绕过。
                if count > MAX_DEEPLINK_DOWNLOAD {
                    bail!(itl!("deeplink-too-large"));
                }
                file.write_all(&chunk)?;
                if let Some(size) = size {
                    // 以 `min` 兜底：实际字节数可能短暂超过声明的长度，避免进度 >100%。
                    *prog.lock().unwrap() = Some(count.min(size) as f32 / size as f32);
                }
                if prog_wk.strong_count() == 1 {
                    // cancelled by the user
                    // 中文说明：强引用只剩覆盖层自己以外的 0 个，说明覆盖层已被丢弃，
                    // 用户取消或不关心了，立即中止以释放连接与临时文件。
                    bail!("cancelled");
                }
            }
            // 阶段四：把文件指针回绕到开头，交给后续导入流程从头读取。
            file.seek(SeekFrom::Start(0))?;
            Ok(file)
        }),
    })
}

// 下载覆盖层的 UI 交互：触摸用于判定「取消」，渲染展示进度与提示文案，
// `take_result` 由上层在每帧轮询任务是否结束（与 `Task` 的轮询式设计一致）。
impl DeepLinkDownload {
    /// Returns `true` when the user tapped the cancel button.
    /// 中文说明：返回 `true` 表示本帧的触摸已被取消按钮消费，上层应据此**丢弃整个
    /// 覆盖层**——丢弃即触发后台任务的取消（见结构体文档）。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        self.cancel_btn.touch(touch, t)
    }

    /// 渲染半透明遮罩、环形进度与「正在下载 / 取消」文案。
    /// 进度用互斥锁临时取出后立即释放；`loading_last` 作为平滑插值的状态随帧更新。
    pub fn render(&mut self, ui: &mut Ui, t: f32) {
        ui.fill_rect(ui.screen_rect(), semi_black(0.6));
        ui.loading(0., -0.06, t, WHITE, (*self.prog.lock().unwrap(), &mut self.loading_last));
        ui.text(itl!("deeplink-downloading")).pos(0., 0.02).anchor(0.5, 0.).size(0.6).draw();
        let r = ui.text(ttl!("cancel")).pos(0., 0.12).anchor(0.5, 0.).size(0.7).measure().feather(0.02);
        self.cancel_btn.render_text(ui, r, t, ttl!("cancel"), 0.6, true);
    }

    /// 取出下载结果（尚未完成时返回 `None`）。`Task::take` 的语义是「有结果则取走」，
    /// 因此上层只会在成功或失败时各拿到一次 `Some`，不会重复处理。
    pub fn take_result(&mut self) -> Option<Result<std::fs::File>> {
        self.task.take()
    }
}

/// Overlay while a chart deeplink fetches the chart's details. Dropping it
/// cancels the fetch.
/// 中文说明：`chart` 类深链只需要拿到谱面元信息就能进入谱面页，故任务产出一个
/// `Arc<Chart>`；与下载覆盖层一样，**丢弃覆盖层即取消请求**（`Task` 被丢弃时其
/// 内部 future 一并停止）。
pub struct DeepLinkChartOpening {
    /// 覆盖层上的「取消」按钮。
    cancel_btn: DRectButton,
    /// 拉取谱面详情的后台任务。
    task: Task<Result<Arc<Chart>>>,
}

/// 按谱面 id 启动一次「打开谱面」流程，返回可渲染的覆盖层。
/// 这里只负责取回谱面元信息；上层拿到 `Chart` 后自行决定进入 `SongScene` 的时机。
pub fn start_chart_opening(id: i32) -> DeepLinkChartOpening {
    DeepLinkChartOpening {
        cancel_btn: DRectButton::new(),
        task: Task::new(async move { Ptr::<Chart>::new(id).fetch().await }),
    }
}

// 打开谱面覆盖层的交互：与下载覆盖层同构（触摸取消 + 覆盖层渲染 + 轮询取结果），
// 区别仅在于进度不可知，故用无限循环的不确定进度动画。
impl DeepLinkChartOpening {
    /// Returns `true` when the user tapped the cancel button.
    /// 中文说明：返回 `true` 时应丢弃覆盖层，从而取消尚未完成的详情请求。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        self.cancel_btn.touch(touch, t)
    }

    /// 渲染遮罩、不确定进度动画与「正在打开 / 取消」文案。
    pub fn render(&mut self, ui: &mut Ui, t: f32) {
        ui.fill_rect(ui.screen_rect(), semi_black(0.6));
        ui.loading(0., -0.06, t, WHITE, ());
        ui.text(itl!("deeplink-opening")).pos(0., 0.02).anchor(0.5, 0.).size(0.6).draw();
        let r = ui.text(ttl!("cancel")).pos(0., 0.12).anchor(0.5, 0.).size(0.7).measure().feather(0.02);
        self.cancel_btn.render_text(ui, r, t, ttl!("cancel"), 0.6, true);
    }

    /// 取出谱面详情结果（尚未完成时返回 `None`）。
    pub fn take_result(&mut self) -> Option<Result<Arc<Chart>>> {
        self.task.take()
    }
}
