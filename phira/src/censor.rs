//! Banned-word detection.
//!
//! [`CensorManager`] fetches the word list on first use (or when the cache is
//! older than a week), builds a `daachorse` double-array Aho-Corasick automaton,
//! and caches the *serialized* automaton (zstd-compressed) to disk. Caching the
//! built automaton rather than the raw words turns a ~300ms startup rebuild
//! (55k patterns) into a ~10ms deserialize — important for mobile cold starts.
//!
//! 本模块负责**用户可编辑文本的本地审核（违禁词过滤）**：用户名、合集名、谱面
//! 元信息等在提交或上传前先就地检查，避免明显违规内容进入服务端（合规与内容安全
//! 考量，也减少无意义的服务端往返）。
//!
//! 关键设计约束：
//! - 词库由远端接口下发（base64 编码、`|` 分隔），不内嵌在二进制里，便于运营侧
//!   随时增删词条而无需发版；
//! - 匹配结构为字符级 Aho-Corasick 自动机，构建耗时高，因此**缓存的是构建产物**
//!   而非原始词表；
//! - 检查接口是同步的、不做任何 IO，所以必须依赖 [`preload`] 完成异步预热；
//!   预热完成前检查一律放行（见 [`check_texts`] 的说明）。

#![allow(dead_code)]

// 闭源构建专用子模块：真实词库地址、预热实现等不愿随开源代码公开的部分都放在这里；
// 开源构建下该模块不存在，因此上面的缓存/匹配逻辑必须能在缺少它时独立工作。
#[rustfmt::skip]
#[cfg(closed)]
mod inner;

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use daachorse::{CharwiseDoubleArrayAhoCorasick, CharwiseDoubleArrayAhoCorasickBuilder, MatchKind};
use serde::Deserialize;
use tokio::sync::OnceCell;

/// 词库缓存的最长有效时间：超过一周即视为过期，需要重新拉取并重建自动机。
/// 兼顾移动端冷启动速度（避免频繁重建）与违禁词的时效性（避免长期命中旧词表）。
const REFRESH_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// 缓存序列化时使用的 zstd 压缩级别（取最高档）。自动机体积较大，压缩率直接影响
/// 移动端的磁盘占用与读取耗时，属于一次性成本换来长期收益。
const ZSTD_LEVEL: i32 = 19;
/// 远端词库中词条之间的分隔符。
const WORD_SEPARATOR: char = '|';
/// 缓存文件魔数，用于快速判定文件确实由本模块写出，而非任意二进制文件。
const CACHE_MAGIC: &[u8; 4] = b"CSAC";
/// Bump whenever the daachorse version or our serialization changes, so an
/// incompatible cache triggers a rebuild instead of an unchecked deserialize (UB).
/// 中文说明：daachorse 版本或序列化格式变化时必须递增此版本号。反序列化走的是
/// `deserialize_unchecked`（不做任何校验，格式不符即 UB），所以必须先靠魔数 +
/// 版本号确认缓存与当前二进制兼容；不匹配时宁可丢弃重建，也不能贸然解析。
const CACHE_VERSION: u8 = 1;

/// 实际使用的匹配器类型：字符级双数组 Aho-Corasick，模式负载为 `u32`。
/// 选字符级而非字节级，是为了在按 UTF-8 多字节编码的中文/日文等词条上按字符匹配，
/// 避免把码点从中间截断造成误报或漏报。
type Matcher = CharwiseDoubleArrayAhoCorasick<u32>;

/// 全局单例：构建流程（下载 + 建自动机 + 反序列化）代价高，进程内只应执行一次。
/// 使用 tokio 的 `OnceCell` 以便在异步预热中安全地“只初始化一次”。
static INSTANCE: OnceCell<CensorManager> = OnceCell::const_new();

/// Endpoint response; `data` is base64 of the `|`-separated word list.
/// 中文说明：字段名对应服务端 JSON（`code`/`msg`/`data`），因此不做 rename。
#[derive(Deserialize)]
struct WordsResponse {
    /// 业务状态码，非 200 视为失败（HTTP 200 不代表业务成功）。
    code: i32,
    /// 服务端提示信息；本地不展示给用户，故允许未使用。
    #[allow(dead_code)]
    msg: String,
    /// base64 编码的词表负载，解码后为 `|` 分隔的词条串。
    data: String,
}

/// 违禁词审核器：持有一个已构建完成的自动机。
/// 构建完成后为纯只读，内部无锁，可安全跨线程共享（由全局单例持有）。
pub struct CensorManager {
    /// 多模式匹配自动机，采用 LeftmostLongest 匹配策略。
    ac: Matcher,
}

// 审核器的构造与查询：构造负责「取词库 → 建自动机 → 落盘缓存」三步异步流程；
// 查询（`check`）为纯内存同步操作，可在任意线程、任意时刻调用。
impl CensorManager {
    /// 缓存文件路径（应用缓存目录下的 `censor.bin`）。
    /// 中文说明：路径依赖运行时才能确定的缓存目录，故返回 `Result`。
    pub fn cache_path() -> Result<PathBuf> {
        Ok(format!("{}/censor.bin", crate::dir::cache()?).into())
    }

    /// Initialize (or return) the singleton with a specific cache path.
    /// If already initialized, the passed path is ignored (first caller wins).
    /// 中文说明：`url` 只在首个真正执行初始化的调用者处生效（先到先得），
    /// 后续并发调用会等待并复用同一实例，其传入的地址被忽略。
    /// # Errors
    /// 首次初始化失败时返回错误。`get_or_try_init` 在失败时不会写入单元，
    /// 因此失败后可以再次调用进行重试（而不是永久卡死在失败状态）。
    pub async fn init(url: String) -> Result<&'static CensorManager> {
        // get_or_try_init leaves the cell empty on error, so a failed init retries.
        INSTANCE.get_or_try_init(|| async move { Self::new(url).await }).await
    }

    /// Construct with a specific cache file. Uses a fresh cache (<1 week) directly,
    /// otherwise fetches and rebuilds; on fetch failure falls back to a stale cache.
    /// 中文说明：构建流程的唯一入口，具体的新鲜度与回退策略见 [`load_or_fetch`]。
    /// # Errors
    /// 远端拉取失败且本地亦无可用缓存时返回错误。
    pub async fn new(url: String) -> Result<Self> {
        let cache_path = Self::cache_path()?;
        tracing::debug!("initializing censor manager");
        let ac = load_or_fetch(&cache_path, &url).await?;
        tracing::info!("censor manager ready ({} KB)", ac.heap_bytes() / 1024);
        Ok(Self { ac })
    }

    /// Check if the given text contains any banned words. Case-insensitive.
    /// 中文说明：先做 ASCII 小写折叠再匹配，因此词库中的词条也统一按 ASCII 小写
    /// 存储（见 [`parse_words`]），保证大小写变体（如 `FuCk`）同样被命中。
    /// 只关心「是否命中」，不返回命中位置——调用方只需给用户一个笼统的拒绝提示。
    /// # Errors
    /// 文本包含任一违禁词时返回错误（错误信息为本地化的用户提示）。
    pub fn check(&self, text: &str) -> Result<()> {
        let hay = text.to_ascii_lowercase();
        let hit = self.ac.leftmost_find_iter(&hay).next();
        if hit.is_some() {
            bail!("{}", crate::ttl!("contains-banned-words"));
        }
        Ok(())
    }

    /// Approximate heap size of the automaton, in bytes.
    /// 中文说明：仅供日志观测内存占用，不保证统计精确。
    pub fn heap_bytes(&self) -> usize {
        self.ac.heap_bytes()
    }
}

/// Stale if the file is missing or its mtime is older than the refresh interval.
/// 中文说明：文件不存在、或无法读取元数据（权限/被占用）一律视为过期，从而走重建；
/// 仅当读取修改时间成功且确实超过刷新周期才判过期。时钟回拨导致 `duration_since`
/// 失败时保守地按「未过期」处理，避免因系统时间异常而反复重建。
fn is_stale(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age > REFRESH_INTERVAL)
        .unwrap_or(false)
}

/// 判定缓存是否可用：新鲜则直接反序列化复用；缓存缺失/过期、或即便新鲜但解析失败
/// （文件损坏、版本不匹配），都退化为联网重新拉取并重建。
async fn load_or_fetch(path: &Path, url: &str) -> Result<Matcher> {
    if !is_stale(path) {
        tracing::debug!("censor cache is fresh, loading from {}", path.display());
        match load_cache(path) {
            Ok(ac) => return Ok(ac),
            Err(e) => tracing::warn!("failed to load censor cache: {e:#}, rebuilding"),
        }
    } else {
        tracing::debug!("censor cache missing or stale, refetching");
    }
    fetch_build_cache(path, url).await
}

/// 拉取词库 → 构建自动机 → 写回缓存。
/// 这里对两类失败采取不同策略：缓存写入失败只降级（下一次冷启动会慢一点），不影响
/// 本次使用；拉取失败则回退到本地可能已过期的缓存，避免因网络问题让审核功能整体失效。
async fn fetch_build_cache(path: &Path, url: &str) -> Result<Matcher> {
    match fetch_words(url).await {
        Ok(words) => {
            let ac = build_matcher(&words)?;
            // A cache write failure only slows the next cold start; not fatal.
            if let Err(e) = write_cache(path, &ac) {
                tracing::warn!("failed to write censor cache: {e:#}");
            } else {
                tracing::info!("cached censor automaton to {}", path.display());
            }
            Ok(ac)
        }
        Err(e) => {
            if let Ok(ac) = load_cache(path) {
                tracing::warn!("failed to fetch words: {e:#}, falling back to existing cache");
                return Ok(ac);
            }
            Err(e).context("failed to fetch word list and no usable cache available")
        }
    }
}

/// 请求词库接口并逐层解码：HTTP 状态 → 业务码 → base64 → UTF-8 → 分词。
/// 每一层失败都附带上下文返回，便于区分是网络、协议还是编码问题。
/// 空词库会被视为错误：一个空词表意味着审核形同虚设，宁可报错也不要静默放行。
async fn fetch_words(url: &str) -> Result<Vec<String>> {
    let resp: WordsResponse = reqwest::get(url)
        .await
        .context("request to word list endpoint failed")?
        .error_for_status()
        .context("word list endpoint returned error status")?
        .json()
        .await
        .context("failed to decode word list response as JSON")?;

    if resp.code != 200 {
        bail!("word list endpoint returned business code {}", resp.code);
    }

    let decoded = STANDARD
        .decode(resp.data.as_bytes())
        .context("failed to base64-decode word list payload")?;
    let text = String::from_utf8(decoded).context("word list payload is not valid UTF-8")?;
    let words = parse_words(&text);
    anyhow::ensure!(!words.is_empty(), "fetched word list is empty");
    tracing::info!("fetched {} censor words", words.len());
    Ok(words)
}

/// Split on `|` into a trimmed, deduplicated, ASCII-lowercased word list.
/// 中文说明：去首尾空白、丢弃空词条、统一 ASCII 小写（与 [`CensorManager::check`]
/// 的折叠方式一致，二者必须保持同步），并借 `HashSet` 去重——重复模式会让自动机
/// 无谓变大，也可能触发构建器的异常行为。
fn parse_words(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    text.split(WORD_SEPARATOR)
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

/// 用词表构建字符级 Aho-Corasick 自动机。
/// 采用 `LeftmostLongest` 策略：从最左位置起优先匹配最长模式，减少「短词误伤长词」
/// 的情况（例如同时收录了 `ab` 与 `abc` 时优先按 `abc` 处理）。
fn build_matcher<I, P>(words: I) -> Result<Matcher>
where
    I: IntoIterator<Item = P>,
    P: AsRef<str>,
{
    CharwiseDoubleArrayAhoCorasickBuilder::new()
        .match_kind(MatchKind::LeftmostLongest)
        .build(words)
        .map_err(|e| anyhow::anyhow!("failed to build daachorse automaton: {e}"))
}

/// 以 `[魔数][版本][zstd 压缩的序列化自动机]` 的格式落盘。
/// 写入前确保父目录存在：移动端首次运行时缓存目录可能尚未创建。
fn write_cache(path: &Path, ac: &Matcher) -> Result<()> {
    let serialized = ac.serialize();
    let compressed = zstd::encode_all(serialized.as_slice(), ZSTD_LEVEL).context("failed to zstd-compress automaton")?;

    let mut buf = Vec::with_capacity(compressed.len() + CACHE_MAGIC.len() + 1);
    buf.extend_from_slice(CACHE_MAGIC);
    buf.push(CACHE_VERSION);
    buf.extend_from_slice(&compressed);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| format!("failed to create cache dir {}", parent.display()))?;
        }
    }
    std::fs::write(path, &buf).with_context(|| format!("failed to write cache file {}", path.display()))?;
    Ok(())
}

/// 读取并反序列化缓存，是 [`write_cache`] 的逆过程。校验顺序如下：
/// 1. 先校验长度、魔数与版本号，确认这份字节确由当前二进制按当前格式写出；
/// 2. 再 zstd 解压（解压失败说明文件损坏）；
/// 3. 最后执行**未做边界校验**的反序列化（安全性依赖第 1 步的保证，见下方 SAFETY）；
/// 4. 确认没有多余尾字节，防止格式漂移被静默接受。
/// # Errors
/// 文件截断、魔数/版本不匹配、解压失败或残留尾字节时返回错误，调用方据此丢弃重建。
fn load_cache(path: &Path) -> Result<Matcher> {
    let raw = std::fs::read(path).with_context(|| format!("failed to read cache file {}", path.display()))?;

    let header_len = CACHE_MAGIC.len() + 1;
    anyhow::ensure!(raw.len() > header_len, "cache file is truncated");
    anyhow::ensure!(&raw[..CACHE_MAGIC.len()] == CACHE_MAGIC, "cache magic mismatch");
    anyhow::ensure!(raw[CACHE_MAGIC.len()] == CACHE_VERSION, "cache version mismatch (found {}, expected {CACHE_VERSION})", raw[CACHE_MAGIC.len()]);

    let serialized = zstd::decode_all(&raw[header_len..]).context("failed to zstd-decompress cache")?;
    // SAFETY: magic and version were validated above, so these bytes were written
    // by this program under the current format.
    // 中文说明：`deserialize_unchecked` 不校验内部布局，传入构造不当的字节会触发 UB，
    // 因此这里的安全性完全建立在上面「魔数 + 版本号」两道校验之上；一旦序列化格式变化
    // 而忘记提升 `CACHE_VERSION`，就会在此读到不兼容数据。
    let (ac, rest) = unsafe { CharwiseDoubleArrayAhoCorasick::deserialize_unchecked(&serialized) };
    anyhow::ensure!(rest.is_empty(), "cache has trailing bytes after automaton");
    tracing::debug!("loaded censor automaton from cache ({} states)", ac.num_states());
    Ok(ac)
}

/// 单文本检查的便捷入口，等价于 [`check_texts`] 只传一个元素。
pub fn check_text(text: &str) -> Result<()> {
    check_texts([text])
}

/// Synchronously check texts against the banned-word list for *local* edits
/// (e.g. renaming a collection offline), returning `Err` on the first one that
/// contains a banned word. No-op (always `Ok`) unless the `aa` feature is
/// enabled.
///
/// If the automaton hasn't finished loading yet (a brief window right after
/// startup, before [`preload`] completes) the text is allowed through: local
/// data isn't safety-critical on its own, and anything later uploaded is still
/// caught by server-side moderation.
/// 中文说明：这是给本地编辑场景使用的同步接口（例如离线重命名合集），语义是「不阻塞、
/// 只做尽力而为的本地拦截」。两处刻意的放行都是有意的取舍：
/// 1. 未启用相应 feature 时本函数为 no-op——对应构建里根本不带词库与自动机；
/// 2. 预热尚未完成时放行——同步路径不能阻塞在网络上，且本地数据本身不构成风险，
///    后续一旦上传仍会被服务端审核兜底。
/// # Errors
/// 任一文本命中违禁词即返回错误（首个命中者），调用方可在提交前中止操作。
pub fn check_texts<'a>(texts: impl IntoIterator<Item = &'a str>) -> Result<()> {
    if !cfg!(feature = "hykb") {
        return Ok(());
    }
    let Some(manager) = INSTANCE.get() else {
        tracing::warn!("censor manager not ready yet, skipping local check");
        return Ok(());
    };
    for text in texts {
        manager.check(text)?;
    }
    Ok(())
}

/// 闭源构建下的预热入口：真正的实现（含词库地址等闭源数据）位于 `censor::inner`。
#[cfg(closed)]
pub use inner::preload;

/// 开源构建下不内置词库与预热逻辑，故为空实现；对应地 [`check_texts`] 在无词库时
/// 也会直接放行，开源版依赖服务端审核而非本地过滤。
#[cfg(not(closed))]
pub async fn preload() {}
