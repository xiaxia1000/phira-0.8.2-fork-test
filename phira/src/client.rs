//! Http client for Phira API.

//! Phira 官方服务端（`https://phira.5wyxi.com`）的 HTTP 客户端封装。
//!
//! 本模块是应用访问服务端的唯一出口：注册/登录、谱面与作者信息、收藏夹、成绩、
//! 文件上传下载、服务条款拉取都经由这里。有几条贯穿全文件的约定：
//!
//! - **鉴权走 client 的默认请求头**，而不是每次调用传参：access token 被写进
//!   `reqwest::Client` 的 `Authorization: Bearer` 头中，因此换 token 必须重建整个
//!   client，再用 `ArcSwap` 原子替换。token 同时镜像一份到 `CLIENT_TOKEN`，供
//!   `model::File` 这类绕过 `CLIENT` 的裸请求复用。
//! - **统一错误转换**：凡是要解析 JSON 响应体的调用都必须先过 `recv_raw`，由它把
//!   HTTP 状态码与响应体里的 `code` 字段翻译成 [`ErrorCode`]。
//! - **泛型对象缓存**：`load`/`fetch` 借助 [`Object`] trait 按类型分流到各自的 LRU
//!   缓存，调用方不需要区分"来自网络"还是"来自缓存"。
//!
//! 本文件的中文注释仅为文档化补充，不改变任何原有行为。

mod model;
pub use model::*;

use std::{borrow::Cow, collections::HashMap, fmt, marker::PhantomData, sync::Arc};

use crate::{get_data, get_data_mut, save_data};
use anyhow::{anyhow, bail, Context, Result};
use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use prpr::scene::SimpleRecord;
use prpr_l10n::LANG_IDENTS;
use reqwest::{header, ClientBuilder, Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::debug;

/// 当前 access token 的无锁快照，供**不走**全局 `CLIENT` 的裸请求复用。
///
/// 存在的意义：`model::File` 下载资源时会用 `basic_client_builder` 另建一个
/// 临时 client（因为它要直连任意 URL，甚至还要改写成 P2P 网关地址），拿不到
/// `CLIENT` 里的默认头，只能从这里同步读取 token。
///
/// 用 `ArcSwap` 而非 `RwLock` 是为了让请求侧读取永远无阻塞且无中毒风险；
/// 使用方必须与 `CLIENT` 保持同步更新，否则会出现"client 已换 token 但文件下载
/// 仍带旧 token"的不一致。
pub static CLIENT_TOKEN: Lazy<ArcSwap<Option<String>>> = Lazy::new(|| ArcSwap::from_pointee(None));

/// 全局共享、**已带默认头**的 reqwest client。
///
/// 之所以是可原子替换的 `ArcSwap<reqwest::Client>` 而不是 `OnceCell`：token 变化
/// 需要整体换 client，而并发请求可能在任意时刻读取它；`ArcSwap` 保证读到的要么是
/// 旧 client、要么是新的，绝不会是半更新状态。内部 `reqwest::Client` 自身是
/// `Arc` 包裹的连接池，克隆/替换的成本很低。
static CLIENT: Lazy<ArcSwap<reqwest::Client>> = Lazy::new(|| ArcSwap::from_pointee(basic_client_builder().build().unwrap()));

/// 服务端 API 的入口（零大小类型 ZST）。
///
/// 它只作为命名空间承载关联函数，所有真实状态都在 [`CLIENT`] / [`CLIENT_TOKEN`]
/// 两个静态变量里。这样设计使调用点统一写成 `Client::get(..)`、
/// `Client::load::<Chart>(id)`，无需在 UI 各处传递 client 句柄，也天然是全局单例。
///
/// 注意：因为它无实例状态，所有方法都是不可变语义——"修改"配置（如换 token）是通过
/// 替换静态变量实现的。
pub struct Client;

// pub const API_URL: &str = "http://localhost:2924";
/// Phira 官方服务端根地址。所有 `Client::get/post/...` 的 path 都会拼在它后面，
/// 因此 path 必须以 `/` 开头。
///
/// 上一行被注释掉的 `localhost:2924` 是本地联调地址：自建服务端时改这里即可，
/// 但要注意 `model.rs` 中 `File::load_thumbnail` 对图片域名做了硬编码判断，
/// 换域名会导致缩略图走原图下载路径。
pub const API_URL: &str = "https://phira.5wyxi.com";

/// 构造一个**不带鉴权头**的基础 reqwest builder，是所有 client 的唯一来源。
///
/// 为什么不直接用 `reqwest::ClientBuilder::new()` 的默认行为：
/// - **重定向策略**：`anys://` 是 Phira 的 P2P 内容寻址协议，不是 http scheme，
///   reqwest 跟随它必然失败。这里遇到 `anys://` 就 `stop()`，把 3xx 响应原样交回
///   上层（由 `model::File::fetch` 解析 `location` 并改写到网关地址），其余 3xx 照常跟随。
/// - **证书校验**：`accept_invalid_cert` 是给自建/内网部署用的开发开关，默认关闭。
///
/// 其余网络参数（超时、TLS 后端、代理、压缩）完全沿用 reqwest 的 feature 默认值，
/// 本函数不再覆盖——因此若要在全局加超时，应改这里而不是各调用点。
pub fn basic_client_builder() -> ClientBuilder {
    // 重定向策略见上：仅拦截 `anys://`，其余交给默认跟随逻辑。
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if let Some(_cid) = attempt.url().as_str().strip_prefix("anys://") {
            attempt.stop()
        } else {
            attempt.follow()
        }
    });
    let mut builder = reqwest::ClientBuilder::new().redirect(policy);
    if get_data().accept_invalid_cert {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder
}

/// 返回用于 `Accept-Language` 请求头的语言标识。
///
/// 服务端据此返回本地化的错误文案与条款文本；未设置时回退到 `LANG_IDENTS[0]`，
/// 保证该头永远非空（某些反代会拒绝缺失该头的请求）。
fn client_locale() -> String {
    get_data().language.clone().unwrap_or(LANG_IDENTS[0].to_string())
}

/// 按给定 token 构造带默认头的 client，并把 token 同步到 `CLIENT_TOKEN`。
///
/// 这是全文件**唯一**同时更新两份 token 副本的地方：`CLIENT` 用于常规 API 请求，
/// `CLIENT_TOKEN` 用于 `File` 的裸下载请求，二者必须一致，所以任何改变登录态的路径
/// 最终都要落到这里。
///
/// `set_sensitive(true)` 让 reqwest 在 Debug/日志中把 `Authorization` 打印为
/// `Sensitive`，避免 token 泄漏到日志。
///
/// # Errors
/// 语言标识或 token 含有非法 header 字符（如换行）时返回错误——这也是防 header
/// 注入的兜底。
fn build_client(access_token: Option<&str>) -> Result<Arc<reqwest::Client>> {
    CLIENT_TOKEN.store(access_token.map(str::to_owned).into());
    let mut headers = header::HeaderMap::new();
    headers.append(header::ACCEPT_LANGUAGE, header::HeaderValue::from_str(&client_locale())?);
    if let Some(token) = access_token {
        let mut auth_value = header::HeaderValue::from_str(&format!("Bearer {token}"))?;
        auth_value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_value);
    }
    Ok(basic_client_builder().default_headers(headers).build()?.into())
}

/// **同步**替换当前 access token（传 `None` 即登出）。
///
/// 名字里的 `_sync` 是刻意的，原因在于鉴权信息的存放位置：token 必须在构造请求头时
/// 被**同步**读取，而 `RequestBuilder` 的构造过程无法 await，所以这里不能用
/// `tokio::sync::Mutex` 之类的异步锁保护 token，只能采用"重建整个 client + 原子
/// 替换"的方式，由 `ArcSwap` 保证读方始终看到一致的 client。
///
/// 时序上它属于登录流程的收尾步骤（`login` → `store_login` → 本函数），也是
/// 应用启动时用本地缓存 token 恢复会话的入口；因为不带 async，启动早期即可调用。
///
/// 代价：换 token 会丢弃旧连接池，故只应在登录/登出/续期这类低频时机调用。
///
/// # Errors
/// 见 `build_client`。
pub fn set_access_token_sync(access_token: Option<&str>) -> Result<()> {
    CLIENT.store(build_client(access_token)?);
    Ok(())
}

/// `set_access_token_sync` 的异步包装，仅为让 async 调用点保持 `?` 链式风格。
async fn set_access_token(access_token: &str) -> Result<()> {
    CLIENT.store(build_client(Some(access_token))?);
    Ok(())
}

/// 服务端业务错误码。
///
/// 它不是由 HTTP 状态码直接映射而来的枚举，而是一个**字符串包装类型**：同一个 HTTP
/// 状态码下服务端会用响应体的 `code` 字段区分具体原因（例如 401 既可能是
/// `UNAUTHENTICATED`（未登录/token 无效）也可能是 `EXPIRED`（token 过期，可续期）），
/// 保留原字符串才能不失真，也能在服务端新增码时不由客户端崩溃。
///
/// `Cow<'static, str>` 让编译期常量走 `Borrowed`（零分配），运行时码走 `Owned`。
/// 上层通过 `err.downcast_ref::<ErrorCode>()` 取回它，据此决定是提示重登、弹出
/// 限流退避提示，还是仅记录日志，见 `recv_raw`。
///
/// 它是 `Display + Error` 而非 `Clone` 之外的派生仅保留了比较/哈希能力，便于
/// 在测试或匹配逻辑中直接比对常量。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ErrorCode(Cow<'static, str>);

/// 批量声明 [`ErrorCode`] 常量：**标识符名字本身就是错误码字符串**。
///
/// `=> $status` 右侧的 HTTP 状态码只起到"人肉对照表"的作用——宏体内并未使用它，
/// 因此不存在自动的 `StatusCode → ErrorCode` 转换（HTTP 状态与业务码是一对多关系，
/// 无法反推）。新增错误码只需在此宏里加一行，并且要与服务端保持同名。
macro_rules! error_code {
    ($($name:ident => $status:expr),* $(,)?) => {
        $(
            pub const $name: ErrorCode = ErrorCode(Cow::Borrowed(stringify!($name)));
        )*
    };
}

// 全部错误码常量集中在此处声明。只有 `recv_raw` 会构造它们，业务侧负责解释：
// 每个名字必须与服务端返回的 `code` 字段逐字一致，否则客户端会把它当成未知码
// （不会报错，只是丧失针对性处理能力）。
#[allow(dead_code)]
impl ErrorCode {
    // 下列每一项为运行时即被识别的业务码，括号中是它对应的 HTTP 语义与典型处理策略。
    error_code! {
        // 400：请求参数不合法（如邮箱格式、密码强度），属于用户可修正的错误，直接展示。
        INVALID_INPUT => StatusCode::BAD_REQUEST,
        // 401：未认证——从未登录，或 token 已被服务端作废；应清空本地 token 并跳登录页。
        UNAUTHENTICATED => StatusCode::UNAUTHORIZED,
        // 401：token 过期；与 UNAUTHENTICATED 区分开，可尝试用 refresh token 静默续期。
        EXPIRED => StatusCode::UNAUTHORIZED,
        // 403：已登录但无权执行该操作（如非审核员提交审核动作），不应重试。
        PERMISSION_DENIED => StatusCode::FORBIDDEN,
        // 403：账号被封禁；属于终态，需向用户展示封禁说明而非反复重试。
        USER_BANNED => StatusCode::FORBIDDEN,
        // 403：账号处于"待删除"冷静期；用 `cancel_delete_request` 重新登录可撤销。
        PENDING_DELETE_REQUEST => StatusCode::FORBIDDEN,
        // 429：触发限流；应退避后重试，而不是立即重发（避免雪崩）。
        RATE_LIMITED => StatusCode::TOO_MANY_REQUESTS,
        // 404：资源不存在；在 `fetch_opt` 路径中被当作正常的"无此对象"处理。
        NOT_FOUND => StatusCode::NOT_FOUND,
        // 409：并发冲突（如注册时用户名/邮箱已被占用、重复绑定），提示用户改输入。
        CONFLICT => StatusCode::CONFLICT,
        // 304：条件请求命中缓存（如条款、文件未变化），属成功分支而非错误。
        NOT_MODIFIED => StatusCode::NOT_MODIFIED,
        // 501：服务端未实现该接口，通常意味着客户端版本过新或服务端灰度中。
        NOT_IMPLEMENTED => StatusCode::NOT_IMPLEMENTED,
        // 503：存储后端不可用（对象存储/数据库故障），可稍后重试。
        STORAGE_UNAVAILABLE => StatusCode::SERVICE_UNAVAILABLE,
        // 500：服务端内部错误，可重试并上报日志。
        INTERNAL_SERVER_ERROR => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// 让错误码能被格式化输出（`ErrorCode(EXPIRED)`）并作为 `std::error::Error` 装箱进
// `anyhow::Error` 链；后者是上层 `downcast_ref::<ErrorCode>()` 能取回它的前提。
impl fmt::Display for ErrorCode {
    /// 仅用于日志/调试展示；面向用户的文案由上层根据码自行本地化。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ErrorCode({})", self.0)
    }
}

// 空实现：本类型只是给错误附带的机器可读标记，没有 source，也不需要自定义展示。
impl std::error::Error for ErrorCode {}

/// 发送请求并做**统一的响应校验与错误转换**（全文件错误处理的中心）。
///
/// 凡是要解析 JSON body 的调用都必须先经过它，因为它承担了"HTTP 层 → 业务错误"的
/// 唯一翻译职责，从而避免各调用点各写一份状态码判断。它的错误结果里会附带
/// [`ErrorCode`]（通过 `anyhow` 的 context 链），上层可据此决定提示/登出/重试。
///
/// # Errors
/// 非 2xx 响应、响应体读取失败，均返回 `Err`；错误消息中会带上 HTTP 状态码与
/// 服务端 `error` 字段的详情，非 JSON 响应体则原样截入消息。
///
/// # Returns
/// 仅在 2xx 时返回 **body 尚未被消费** 的 `Response`，由调用方自行 `.json()`。
pub async fn recv_raw(request: RequestBuilder) -> Result<Response> {
    // 阶段一：真正发包。DNS、连接、超时等传输层错误在此直接冒泡，不附带业务上下文。
    let response = request.send().await?;
    // 阶段二：状态码判定。只有失败响应才需要读 body——注意必须读完，否则连接无法复用。
    if !response.status().is_success() {
        let status = response.status().as_str().to_owned();
        let text = response.text().await.context("failed to receive text")?;
        // 阶段三：尝试把失败体当作结构化错误（`{"error": ..., "code": ...}`）解析。
        if let Ok(what) = serde_json::from_str::<serde_json::Value>(&text) {
            // `error` 缺失时给一个占位串，保证消息可读（这是最常见的字段）。
            let detail = what.get("error").and_then(|it| it.as_str()).unwrap_or("unknown error");
            let mut err = anyhow!("request failed (HTTP {status}): {detail}");
            // `code` 是可选字段；这里用 `context` 而非替换，保证原始消息与错误码都保留。
            if let Some(code) = what.get("code").and_then(|it| it.as_str()) {
                err = err.context(ErrorCode(Cow::Owned(code.to_owned())));
            }
            return Err(err);
        }
        // 阶段四：非 JSON 响应（典型如反代/网关返回的 HTML 错误页），降级为纯文本报错。
        bail!("request failed ({status}): {text}");
    }
    Ok(response)
}

/// `/login` 的请求体：两种互斥登录方式共用同一表示。
///
/// 用 `untagged` 是为了让 JSON 里不出现变体名，正好匹配服务端"按字段推断登录方式"
/// 的约定——带 `email`/`password` 即密码登录，带 `refreshToken` 即续期。因此
/// **新增变体时必须保证字段集合与另一变体不重叠**，否则序列化结果会产生歧义。
/// 仅有 `Serialize`（客户端只发不解析），故不需要考虑反序列化的宽容度。
#[derive(Serialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
pub enum LoginParams<'a> {
    /// 邮箱 + 密码登录，随后服务端下发 token 对。
    Password {
        /// 登录邮箱，大小写不敏感由服务端负责。
        email: &'a str,
        /// 明文密码（依赖 HTTPS 传输；客户端不做本地摘要）。
        password: &'a str,
        /// 若账号处于"申请删除"冷静期，本次登录是否同时撤销该申请。
        cancel_delete_request: bool,
    },
    /// 用 refresh token 静默续期，避免让用户重输密码。
    RefreshToken {
        /// 上次登录时一并下发的 refresh token；字段名需显式改为 `refreshToken`
        /// 以匹配服务端（变体名本身不会被序列化）。
        #[serde(rename = "refreshToken")]
        token: &'a str,
        /// 同 `Password` 变体：续期登录同样可以撤销待删除申请。
        cancel_delete_request: bool,
    },
}

/// A freshly minted token pair returned by every login endpoint.
/// 登录接口统一返回的 token 对（此处仅 HYKB 分支使用；密码登录那条路径在其函数内
/// 单独声明了等价结构，因为本结构被 `hykb` feature 门控）。
/// 字段名走 camelCase 反序列化，故 `refresh_token` 对应服务端 `refreshToken`。
#[cfg(feature = "hykb")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginResp {
    /// 账号 id；`store_login` 保留了该参数但目前不再使用（见其注释）。
    id: i32,
    /// 短期 access token，用于后续请求的 `Authorization` 头。
    token: String,
    /// 长期 refresh token，access token 过期后用它静默续期。
    refresh_token: String,
}

/// Response of `POST /login/hykb`: either an immediate login or a pending choice.
/// 好游快爆登录的两种结果，用响应体里的 `status` 字段区分（`ok` / `needChoice`）：
/// 老用户（渠道账号已绑定）直接拿到 token；新用户只拿到一个短期的 `hykbToken`，
/// 用于随后选择"注册新号"或"认领已有邮箱号"。
/// `flatten` 让成功分支的 token 对平铺在顶层，与失败分支的字段并存于同一层 JSON。
#[cfg(feature = "hykb")]
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
enum HykbLoginResp {
    /// 渠道账号此前已与某个 Phira 账号绑定，直接完成登录。
    Ok {
        /// 平铺进来的 token 对（见 `LoginResp`）。
        #[serde(flatten)]
        login: LoginResp,
    },
    /// 渠道账号首次出现，需要玩家选择注册或认领；此 token 是**一次性、短时效**凭证。
    NeedChoice {
        /// 供 `/login/hykb/register` 与 `/login/hykb/claim` 使用的临时凭证。
        hykb_token: String,
    },
}

/// Outcome of a HYKB login attempt surfaced to the UI.
/// HYKB 登录的对外结果：与内部 `HykbLoginResp` 的区别在于，成功分支已被消费掉并
/// 写入了本地登录态，UI 只需根据变体决定"进主界面"还是"弹选择框"。
/// 同理，`hykb_token` 是敏感的临时凭证，UI 拿到后应尽快用完、不要落盘。
#[cfg(feature = "hykb")]
pub enum HykbLoginOutcome {
    /// The HYKB account was already bound; the user is now logged in.
    /// 渠道账号已绑定：本地 token 已写入并持久化，无需再做任何事。
    LoggedIn,
    /// First time we see this HYKB account; the user must register or claim.
    /// 首次见到该渠道账号：需引导玩家二选一（注册新号 / 认领已有邮箱号）。
    NeedChoice { hykb_token: String },
}

// Client 的全部接口实现。分成三类职责：
// 1) `get/post/delete/request` —— 只负责拼 URL 与选方法，返回未发送的 RequestBuilder
//    （是否经过 `recv_raw` 校验取决于调用方，`fetch_inner`/`fetch_terms` 就自行处理了响应）；
// 2) `load/fetch/fetch_opt/clear_cache` —— 泛型对象加载与缓存读写；
// 3) 业务方法 —— 注册、登录（含 HYKB 渠道）、个人信息、成绩、上传、条款。
impl Client {
    /// 构造一个 GET 请求（**尚未发送**）。
    ///
    /// # Arguments
    /// - `path`：以 `/` 开头、拼接在 [`API_URL`] 之后的相对路径。
    #[inline]
    pub fn get(path: impl AsRef<str>) -> RequestBuilder {
        Self::request(Method::GET, path)
    }

    /// 构造一个以 JSON 为请求体的 POST 请求（**尚未发送**）。
    ///
    /// # Arguments
    /// - `path`：同 `get`；
    /// - `data`：任意可序列化类型，会被 `.json()` 编码并自动补 `Content-Type`。
    #[inline]
    pub fn post<T: Serialize>(path: impl AsRef<str>, data: &T) -> RequestBuilder {
        Self::request(Method::POST, path).json(data)
    }

    /// 构造一个 DELETE 请求（**尚未发送**）。
    #[inline]
    pub fn delete(path: impl AsRef<str>) -> RequestBuilder {
        Self::request(Method::DELETE, path)
    }

    /// 构造任意方法的请求：拼接基址并取当前 client 快照。
    ///
    /// 用 `CLIENT.load()` 而非缓存句柄引用，保证拿到的是最近一次 token 替换后的
    /// client；`API_URL.to_string() + path` 的字符串拼接方式要求 `path` 必须以 `/` 开头，
    /// 否则会拼出错误地址。
    pub fn request(method: Method, path: impl AsRef<str>) -> RequestBuilder {
        CLIENT.load().request(method, API_URL.to_string() + path.as_ref())
    }

    /// 删除某个对象在类型对应 LRU 缓存中的条目，返回它此前是否存在。
    ///
    /// 缓存本身没有超时，只有容量淘汰，因此**服务端数据变化后必须显式调用本函数**
    /// （例如改名、换头像、谱面状态由未审核变为已审核），否则界面会一直看到旧值。
    /// 参数 `T` 决定操作哪张缓存表，`id` 是对象主键。
    ///
    /// 这里的 `unreachable!()` 是一个不变量断言：缓存表以 `QUERY_PATH` 为键，而
    /// `QUERY_PATH` 由类型唯一决定，所以同一个键下拿到的必然是 `ObjectMap<T>`。
    pub fn clear_cache<T: Object + 'static>(id: i32) -> Result<bool> {
        let map = obtain_map_cache::<T>();
        let mut guard = map.lock().unwrap();
        let Some(actual_map) = guard.downcast_mut::<ObjectMap<T>>() else {
            unreachable!()
        };
        Ok(actual_map.pop(&id).is_some())
    }

    /// 读优先的加载：先查缓存，未命中才发起网络请求。
    ///
    /// # Returns
    /// 返回缓存的 `Arc<T>` 克隆——**同一对象在缓存存活期内是同一个 `Arc`**，
    /// 因此调用方可以依赖指针相等做对象标识比较，也可以安全地长期持有。
    ///
    /// # Errors
    /// 未命中缓存且网络/解析失败时返回错误。
    pub async fn load<T: Object + 'static>(id: i32) -> Result<Arc<T>> {
        // 阶段一：查缓存。注意这是一个**同步**锁的临界区，必须在 await 之前显式释放，
        // 否则会跨 await 持有 std::sync 锁（既不 Send 也可能死锁）。
        {
            let map = obtain_map_cache::<T>();
            let mut guard = map.lock().unwrap();
            let Some(actual_map) = guard.downcast_mut::<ObjectMap<T>>() else {
                unreachable!()
            };
            if let Some(value) = actual_map.get(&id) {
                return Ok(Arc::clone(value));
            }
            drop(guard);
            drop(map);
        }
        // 阶段二：缓存未命中，走网络（`fetch` 内部仍会回写缓存）。
        Self::fetch(id).await
    }

    /// 强制走网络的加载：命中缓存也会重新请求并覆盖缓存。
    ///
    /// 适合"必须拿到最新数据"的场景（如进入个人主页前刷新）。
    ///
    /// # Errors
    /// 对象不存在（服务端 404）或请求失败时返回错误。
    pub async fn fetch<T: Object + 'static>(id: i32) -> Result<Arc<T>> {
        Self::fetch_opt(id).await?.ok_or_else(|| anyhow!("entry not found"))
    }

    /// 可失败但**允许对象不存在**的加载：不存在时返回 `Ok(None)` 而非错误。
    ///
    /// 这是缓存回写点：网络取到的对象在这里写入 LRU 表并转成 `Arc` 共享。
    /// 注意它**不做去重**——并发的两次未命中请求会各自下载并各自 `put`，后写者覆盖
    /// 前写者，导致短暂存在两个不同的 `Arc`（因此不要跨并发路径比较指针相等性）。
    ///
    /// # Errors
    /// 网络错误或响应体解析失败时返回错误（404 不算）。
    pub async fn fetch_opt<T: Object + 'static>(id: i32) -> Result<Option<Arc<T>>> {
        // 阶段一：网络获取（可能得到 None）。
        let value = Client::fetch_inner::<T>(id).await?;
        let Some(value) = value else { return Ok(None) };
        // 阶段二：包成 `Arc` 并回写缓存，保证后续 `load` 直接命中。
        let value = Arc::new(value);
        let map = obtain_map_cache::<T>();
        let mut guard = map.lock().unwrap();
        let Some(actual_map) = guard.downcast_mut::<ObjectMap<T>>() else {
            unreachable!()
        };
        actual_map.put(id, Arc::clone(&value));
        Ok(Some(value))
    }

    /// 单对象 GET 的底层实现，路径由对象的 [`Object::QUERY_PATH`] 决定。
    ///
    /// 与 [`recv_raw`] 的差别在于**错误处理更宽松**：因为对象可能合法地不存在，
    /// 404 被翻译成 `Ok(None)`，且错误里不附带 [`ErrorCode`]（只取 `error` 文本）。
    /// 若将来需要按错误码分流，这里应改为复用 `recv_raw`。
    async fn fetch_inner<T: Object>(id: i32) -> Result<Option<T>> {
        // 阶段一：请求 + 判定"不存在"这一正常分支。
        let resp = Self::get(format!("/{}/{id}", T::QUERY_PATH)).send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        // 阶段二：其余非 2xx 一律视为错误，尽力提取服务端的 `error` 文案。
        if !resp.status().is_success() {
            let status = resp.status().as_str().to_owned();
            let text = resp.text().await.context("failed to receive text")?;
            if let Ok(what) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(detail) = what["error"].as_str() {
                    bail!("request failed ({status}): {detail}");
                }
            }
            bail!("request failed ({status}): {text}");
        }
        // 阶段三：反序列化。类型由 `T` 在编译期确定，字段不匹配即为兼容性事故。
        Ok(Some(resp.json().await?))
    }

    /// 构造针对某类对象的查询构建器，起始为空查询、无页码、无路径后缀。
    ///
    /// 返回的 [`QueryBuilder`] 是链式消费式的，最终用 `send()` 取回一页结果。
    pub fn query<T: Object>() -> QueryBuilder<T> {
        QueryBuilder {
            queries: HashMap::new(),
            page: None,
            suffix: "",
            _phantom: PhantomData,
        }
    }

    /// 注册新账号（`POST /register`）。
    ///
    /// 只创建账号、**不会**自动登录：调用方成功后还需要走 `login` 才能拿到 token。
    /// 这里刻意用 [`recv_raw`] 而非 `.json()`，因为该接口成功时无响应体，只需要它统一的
    /// 错误转换（用户名/邮箱重复会返回 `CONFLICT`）。
    ///
    /// # Errors
    /// 邮箱格式非法、用户名或邮箱已被占用、网络失败等。
    pub async fn register(email: &str, username: &str, password: &str) -> Result<()> {
        recv_raw(Self::post(
            "/register",
            // 注意此处请求体字段名是 `name` 而非 `username`，与服务端保持一致。
            &json!({
                "email": email,
                "name": username,
                "password": password,
            }),
        ))
        .await?;
        Ok(())
    }

    /// 登录或续期（`POST /login`），成功后**立即落盘登录态**。
    ///
    /// 这是密码登录与 refresh token 续期的共同入口，具体方式由 [`LoginParams`] 决定。
    ///
    /// # Arguments
    /// - `params`：密码登录或 refresh token 续期参数。
    ///
    /// # Errors
    /// 凭据错误（401）、账号被封禁（403 `USER_BANNED`）、处于待删除状态且未选择撤销
    /// （403 `PENDING_DELETE_REQUEST`）、限流（429）等。
    pub async fn login(params: LoginParams<'_>) -> Result<()> {
        // 在 `LoginParams` 外再包一层以附加客户端版本号：服务端用它做最低版本校验与灰度。
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct FullLoginParams<'a> {
            // 平铺，使最终 JSON 与 `LoginParams` 自身序列化结果处于同一层。
            #[serde(flatten)]
            inner: LoginParams<'a>,
            // 编译期写入的 crate 版本，运行时不可变，故用 `&'static str`。
            #[serde(rename = "clientVersion")]
            client_version: &'static str,
        }

        // 本结构不复用模块级 `LoginResp`，因为后者被 `hykb` feature 门控；
        // 字段与它完全一致（`refreshToken` → `refresh_token`）。
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Resp {
            // 账号 id。
            id: i32,
            // 短期 access token。
            token: String,
            // 长期 refresh token。
            refresh_token: String,
        }
        // 阶段一：请求 + 统一错误校验 + 反序列化 token 对。
        let resp: Resp = recv_raw(Self::post(
            "/login",
            &FullLoginParams {
                inner: params,
                client_version: env!("CARGO_PKG_VERSION"),
            },
        ))
        .await?
        .json()
        .await?;

        // 阶段二：写入运行时 client 与本地持久化（此处失败会导致"已登录但下次启动掉线"）。
        Self::store_login(resp.id, resp.token, resp.refresh_token).await?;
        Ok(())
    }

    /// Persist a freshly minted token pair and wire it into the HTTP client.
    /// Shared by every login entry point (password, refresh, HYKB). `_id` is
    /// kept in the signature so callers can pass the account id even though it
    /// is no longer needed here (the native anti-addiction bridge that used it
    /// is gone).
    /// 持久化新签发的 token 对并接入 HTTP client，所有登录路径（密码、续期、HYKB）
    /// 共用。三步顺序有语义：先换 client（立即可用），再写全局 `Data`（内存态），
    /// 最后 `save_data()` 落盘（下次启动可恢复会话）。若落盘失败，本次登录在本次
    /// 运行内仍有效，但重启后会掉线。
    ///
    /// `_id` 保留在签名里是历史原因：调用方仍可传入账号 id，但自从原生防沉迷桥接
    /// 被移除后这里已不再需要它。
    ///
    /// # Errors
    /// `set_access_token`（token 含非法 header 字符）或本地写文件失败。
    async fn store_login(_id: i32, token: String, refresh_token: String) -> Result<()> {
        set_access_token(&token).await?;
        get_data_mut().tokens = Some((token, refresh_token));
        save_data()?;
        Ok(())
    }

    #[cfg(feature = "hykb")]
    /// Entry point for HYKB (好游快爆) login. The verified `(uid, access_token)`
    /// come from the native SDK. Either logs the user straight in (account already
    /// bound) or returns a short-lived `hykb_token` for the register/claim step.
    /// HYKB（好游快爆）渠道登录入口。`(uid, access_token)` 由**原生 SDK**验证后提供，
    /// 客户端不做校验，只做转发。整个渠道登录是"账号绑定"模型：一个 HYKB 账号最多
    /// 绑定一个 Phira 账号。
    ///
    /// 结果有三种可能（对外收敛为两种）：
    /// - 已绑定 → 直接完成登录，本地登录态已写入；
    /// - 未绑定 → 返回一次性 `hykb_token`，由 UI 引导玩家选择"注册新号"或"认领邮箱号"。
    ///
    /// 该接口的失败通常是 SDK 返回的 token 已失效或账号被封禁。
    pub async fn login_hykb(uid: i64, access_token: &str) -> Result<HykbLoginOutcome> {
        // 请求体字段名 `hykbUid`/`accessToken` 需与服务端严格一致。
        let resp: HykbLoginResp = recv_raw(Self::post(
            "/login/hykb",
            &json!({
                "hykbUid": uid,
                "accessToken": access_token,
            }),
        ))
        .await?
        .json()
        .await?;
        // 分支处理：已绑定则走与密码登录相同的落盘路径，保证会话恢复行为一致。
        match resp {
            HykbLoginResp::Ok { login } => {
                Self::store_login(login.id, login.token, login.refresh_token).await?;
                Ok(HykbLoginOutcome::LoggedIn)
            }
            HykbLoginResp::NeedChoice { hykb_token } => Ok(HykbLoginOutcome::NeedChoice { hykb_token }),
        }
    }

    #[cfg(feature = "hykb")]
    /// New player: create a fresh Phira account bound to the pending HYKB identity,
    /// using the username chosen by the player.
    /// 新玩家路径：用玩家自选的用户名创建一个全新的 Phira 账号，并与待绑定的 HYKB
    /// 身份绑定。成功后与其它登录路径一样直接进入已登录状态，因此 UI 无需再调 `login`。
    ///
    /// `hykb_token` 来自 `login_hykb` 的 `NeedChoice`，**一次性且短时效**，被消费后
    /// 若再次使用会失败（需重新走渠道登录）。
    pub async fn login_hykb_register(hykb_token: &str, username: &str) -> Result<()> {
        // 注意服务端字段名为 `nick`（昵称）而非 `name`，与邮箱注册接口不同。
        let resp: LoginResp = recv_raw(Self::post(
            "/login/hykb/register",
            &json!({
                "hykbToken": hykb_token,
                "nick": username,
            }),
        ))
        .await?
        .json()
        .await?;
        Self::store_login(resp.id, resp.token, resp.refresh_token).await?;
        Ok(())
    }

    #[cfg(feature = "hykb")]
    /// Legacy migration: bind the pending HYKB identity to an existing email account
    /// after verifying its email + password.
    /// 老玩家迁移路径：校验邮箱密码后，把待绑定的 HYKB 身份"认领"到已有邮箱账号上。
    /// 适用于渠道上线前就已注册的玩家，避免他们丢失原有成绩与收藏。
    ///
    /// 语义上等价于"用邮箱密码证明你是该账号主人，然后把渠道身份挂上去"，
    /// 成功后直接用该邮箱账号的登录态，`hykb_token` 同时被消费。
    pub async fn login_hykb_claim(hykb_token: &str, email: &str, password: &str) -> Result<()> {
        let resp: LoginResp = recv_raw(Self::post(
            "/login/hykb/claim",
            &json!({
                "hykbToken": hykb_token,
                "email": email,
                "password": password,
            }),
        ))
        .await?
        .json()
        .await?;
        Self::store_login(resp.id, resp.token, resp.refresh_token).await?;
        Ok(())
    }

    #[cfg(feature = "hykb")]
    /// Bind a HYKB account to the currently logged-in account.
    /// 把 HYKB 账号绑定到**当前已登录**的账号上（与 `login_hykb*` 的区别：后者是
    /// 用渠道身份换取登录态，这里是已登录用户在个人页补绑渠道账号，主要用于满足
    /// 渠道侧实名/防沉迷要求）。一个 HYKB 账号只能绑定一个 Phira 账号，冲突时服务端
    /// 会返回 `CONFLICT`。
    pub async fn bind_hykb(uid: i64, access_token: &str) -> Result<()> {
        recv_raw(Self::post(
            "/me/bind-hykb",
            &json!({
                "hykbUid": uid,
                "accessToken": access_token,
            }),
        ))
        .await?;
        Ok(())
    }

    #[cfg(feature = "hykb")]
    /// Unbind the HYKB account from the current account.
    /// 解绑当前账号上的 HYKB 渠道身份。注意解绑**不会**删除账号，只是切断渠道关联；
    /// 若该账号原本就是渠道首登创建的，解绑后仍可用邮箱密码登录（前提是已设置邮箱）。
    pub async fn unbind_hykb() -> Result<()> {
        recv_raw(Self::post("/me/unbind-hykb", &())).await?;
        Ok(())
    }

    #[cfg(feature = "hykb")]
    /// Request transferring the current HYKB-only account onto an existing email
    /// account. Sends a confirmation email to `email`; the move happens only once
    /// the user clicks the link. Returns Ok even when the email is unregistered
    /// (the server intentionally does not reveal whether it exists).
    /// 请求把当前"仅有渠道身份"的账号迁移到一个已有邮箱账号上。服务端只负责向
    /// `email` 发送确认邮件，**真正迁移发生在用户点击邮件链接之后**，因此本调用返回
    /// `Ok` 并不代表已迁移。
    ///
    /// 安全设计：邮箱未注册时同样返回成功——防止通过该接口枚举"哪些邮箱是 Phira 用户"。
    /// 因此 UI 不能把成功当作"邮箱存在"的信号。
    pub async fn transfer_request(email: &str) -> Result<()> {
        recv_raw(Self::post("/me/transfer-request", &json!({ "email": email }))).await?;
        Ok(())
    }

    /// 拉取当前登录账号的完整资料（`GET /me`），并刷新本地 `Data` 中的用户态。
    ///
    /// # Returns
    /// 服务端下发的 [`User`]，其中 `roles` 为原始位掩码，权限需由客户端换算
    /// （见 [`Permissions`]）。
    ///
    /// # Errors
    /// 未登录或 token 失效（401）、被限流（429）等。
    pub async fn get_me() -> Result<User> {
        // Accounts not bound to a HYKB account are valid: anti-addiction is
        // covered by a native HYKB login performed at sign-in (used for the
        // SDK's enforcement, not bound to the account), and the player may
        // bind HYKB later from the profile page.
        Ok(recv_raw(Self::get("/me")).await?.json().await?)
    }

    /// 查询某张谱面的个人最好成绩（`GET /record/best/{id}`）。
    ///
    /// 返回的是内核侧通用的 `SimpleRecord`，用于成绩页与谱面详情展示。
    ///
    /// # Arguments
    /// - `id`：谱面 id（不是成绩记录 id）。
    ///
    /// # Errors
    /// 未登录、谱面不存在，或**该谱面暂无成绩**（服务端通常返回 404）时出错。
    pub async fn best_record(id: i32) -> Result<SimpleRecord> {
        Ok(recv_raw(Self::get(format!("/record/best/{id}"))).await?.json().await?)
    }

    /// 上传一个二进制文件，返回服务端分配的**文件 id**（`POST /upload/{name}`）。
    ///
    /// 典型用途是上传头像：`name` 是文件名/类别（路径片段），返回的 id 再作为
    /// 头像字段提交。这里用 `Method::POST` + `.body(bytes)` 直接发原始字节，
    /// 不经过 JSON 编码。
    ///
    /// # Returns
    /// 可直接用于构造 `File`/头像 URL 的资源 id 字符串。
    ///
    /// # Errors
    /// 未登录、文件过大或类型不被允许、存储不可用（503）等。
    pub async fn upload_file(name: &str, bytes: Vec<u8>) -> Result<String> {
        // 成功响应体仅含资源 id；本地声明以免为它单独暴露一个公共类型。
        #[derive(Deserialize)]
        struct Resp {
            // 服务端生成的资源标识（字符串形式）。
            id: String,
        }
        let resp: Resp = recv_raw(Self::request(Method::POST, format!("/upload/{name}")).body(bytes))
            .await?
            .json()
            .await?;
        Ok(resp.id)
    }

    /// Returns `Some(modified)` (the `Last-Modified` header) if the terms have
    /// been updated since `modified`, or `None` if unchanged. Uses HEAD so the
    /// ~9 KB body is never downloaded — change detection relies solely on the
    /// `Last-Modified` header.
    /// 检查服务条款是否更新：返回新的 `Last-Modified` 时间戳（`Some`）或表示未变化
    /// （`None`）。用 **HEAD** 而非 GET，因此从不下载那 ~9KB 正文，变更检测只依赖
    /// `Last-Modified` 响应头。
    ///
    /// 协议细节：首次调用（`modified == None`）必然返回 `Some`，供调用方记录基准值；
    /// 之后传回上次的值，命中 `304` 即视为未变。最后的字符串比对是针对对象存储
    /// （七牛）在部分情况下不返回 304 的兜底——此时仍会返回 200，但时间戳相同。
    ///
    /// 条款语言随 `client_locale()` 变化，即换语言会拿到不同文件、时间戳也不同。
    ///
    /// # Errors
    /// 非 2xx、缺少 `Last-Modified` 头、网络失败等。
    pub async fn fetch_terms(modified: Option<&str>) -> Result<Option<String>> {
        // 阶段一：构造条件请求。注意此处直接取 `CLIENT` 快照（与 `Client::request`
        // 等价），并带上 `If-Modified-Since` 让服务端有机会回 304。
        let mut req = CLIENT.load().head(format!("{API_URL}/terms/{}.txt", client_locale()));
        if let Some(modified) = modified {
            req = req.header(header::IF_MODIFIED_SINCE, header::HeaderValue::from_str(modified)?);
        }
        let resp = req.send().await?;
        // 阶段二：标准的条件请求命中，直接判定未变化。
        if resp.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        if !resp.status().is_success() {
            bail!("failed to fetch terms: {:?}", resp.status());
        }
        // 阶段三：提取 `Last-Modified`；缺失说明对方不支持该头，视为异常而非"未变化"。
        let new_modified = resp
            .headers()
            .get(header::LAST_MODIFIED)
            .and_then(|it| it.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("invalid last-modified header"))?;
        debug!("{new_modified} {modified:?}");
        // 阶段四：对象存储未返回 304 时的兜底比对。
        if Some(new_modified.as_str()) == modified {
            // That mother fucker qiniu does not return NOT_MODIFIED
            return Ok(None);
        }
        Ok(Some(new_modified))
    }
}

/// 列表类接口的查询构建器（builder 模式，各方法消费并返回 `Self`）。
///
/// 之所以需要它：服务端的分页列表接口（如 `/chart`、`/collection`）把筛选、排序、
/// 搜索与翻页**全部放在 query string** 里，而参数组合较多，直接拼字符串易错且难复用。
/// 本类型把参数收集成 `HashMap`，由最终 `send()` 统一交给 reqwest 的 `.query()`
/// 做百分号编码（因此调用方传入的值不必自己转义）。
///
/// `#[must_use]`：若忘记链上 `send()`，编译器会警告——未发送的构建器毫无作用。
#[must_use]
pub struct QueryBuilder<T> {
    /// 已累积的查询参数（键值对，未编码，编码由 reqwest 负责）。
    /// 用 `Cow<'static, str>` 是为了让字面量常量参数零拷贝。
    queries: HashMap<Cow<'static, str>, Cow<'static, str>>,
    /// 页码，**从 0 开始**；`None` 表示取第 0 页。最终会 +1 变成服务端的 1-based 页码。
    page: Option<u64>,
    /// 附加在 `QUERY_PATH` 之后的路径后缀（如 `/top`），用于复用同一对象的多个列表端点。
    suffix: &'static str,
    /// 标记 `T` 的用途，使构建器在类型上与目标对象绑定（决定返回类型与请求路径）。
    _phantom: PhantomData<T>,
}

// 查询构建器的参数装配与发送。所有参数方法都是"消费式"链式调用，因此不存在
// 半构造状态被复用的问题；`send` 是唯一的终结操作。
impl<T: Object> QueryBuilder<T> {
    /// 追加一个自定义查询参数；同名参数会被**覆盖**而非追加成多值。
    pub fn query(mut self, key: impl Into<Cow<'static, str>>, value: impl Into<Cow<'static, str>>) -> Self {
        self.queries.insert(key.into(), value.into());
        self
    }

    /// 按服务端约定设置排序参数 `order`（取值由服务端定义，如时间/难度）。
    #[inline]
    pub fn order(self, order: impl Into<Cow<'static, str>>) -> Self {
        self.query("order", order)
    }

    /// 按标签筛选（`tags`），多个标签的编码方式由服务端约定。
    #[inline]
    pub fn tags(self, tags: impl Into<Cow<'static, str>>) -> Self {
        self.query("tags", tags)
    }

    /// 关键词搜索（`search`）。
    #[inline]
    pub fn search(self, search: impl Into<Cow<'static, str>>) -> Self {
        self.query("search", search)
    }

    /// 设置 `pageNum` 参数——注意它与 [`QueryBuilder::page`] **不是同一个东西**：
    /// 这是服务端某些端点在页内再分页时使用的辅助参数，语义由服务端定义。
    #[inline]
    pub fn page_num(self, page_num: u64) -> Self {
        self.query("pageNum", page_num.to_string())
    }

    /// 设置路径后缀（拼在 `QUERY_PATH` 之后），例如用它区分同类型的多个列表端点。
    #[inline]
    pub fn suffix(mut self, suffix: &'static str) -> Self {
        self.suffix = suffix;
        self
    }

    /// 设置要请求的页码，**从 0 开始计数**（发送时会自动转换为服务端使用的 1-based）。
    pub fn page(mut self, page: u64) -> Self {
        self.page = Some(page);
        self
    }

    /// 发送请求并解析成"一页结果 + 总数"。
    ///
    /// 分页约定：本方法会**无条件**把 `page` 参数写入 query（默认 0 → 发送 1），
    /// 因此先前通过 `query("page", ..)` 设置的值会被覆盖——要设页码请用 `page()`。
    ///
    /// # Returns
    /// `(results, count)`：当前页的对象列表，以及**不受分页影响的记录总数**
    /// （用于计算总页数、显示总数）。
    ///
    /// # Errors
    /// 参数不合法（400）、未登录、限流，或 `count`/`results` 字段缺失导致反序列化失败。
    pub async fn send(mut self) -> Result<(Vec<T>, u64)> {
        // 分页参数：内部 0-based → 服务端 1-based。
        self.queries.insert("page".into(), (self.page.unwrap_or(0) + 1).to_string().into());
        // 服务端分页响应的统一外壳，仅在函数内使用，故就地声明。
        #[derive(Deserialize)]
        struct PagedResult<T> {
            // 记录总数（跨所有页）。
            count: u64,
            // 当前页数据。
            results: Vec<T>,
        }
        let res: PagedResult<T> = recv_raw(Client::get(format!("/{}{}", T::QUERY_PATH, self.suffix)).query(&self.queries))
            .await?
            .json()
            .await?;
        Ok((res.results, res.count))
    }
}
