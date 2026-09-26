//! 服务端对象模型与本地缓存层。
//!
//! 这里定义的是**服务端 API 返回的资源对象的 Rust 镜像**（谱面、收藏夹、活动、
//! 站内消息、成绩、用户），以及围绕它们的三层本地设施：
//!
//! 1. **对象缓存**：`Object` trait 把"对象的查询路径"与类型绑定，`Client::load`
//!    据此为每个类型维护独立的 LRU 表；`Ptr<T>` 则是其上的惰性引用——序列化/反序列化
//!    时只存一个 id，真正需要数据时才 `load`。
//! 2. **文件缓存**：[`File`] 通过 cacache 做磁盘缓存，并处理 `anys://` P2P 重定向。
//! 3. **用户信息缓存**：[`UserManager`] 用后台任务把"每帧都要用的用户名/头像"预取到
//!    内存，避免 UI 帧内发起网络请求。
//!
//! 各子模块只做"网络对象定义 + 少量转换"，导出集中在此，故外部使用
//! `client::Chart`、`client::Collection` 等路径。

mod chart;
pub use chart::*;

mod collection;
pub use collection::*;

mod event;
pub use event::*;

mod message;
pub use message::*;

mod record;
pub use record::*;

mod user;
pub use user::*;

use super::{basic_client_builder, Client, API_URL, CLIENT_TOKEN};
use crate::{
    dir, get_data,
    images::{THUMBNAIL_HEIGHT, THUMBNAIL_WIDTH},
};
use anyhow::{bail, Result};
use bytes::Bytes;
use image::DynamicImage;
use lru::LruCache;
use once_cell::sync::Lazy;
use reqwest::Response;
use serde::{de::DeserializeOwned, Deserialize, Serialize, Serializer};
use std::{
    any::Any,
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, Mutex},
};
use tracing::debug;

/// 单一类型的对象缓存表：`id → Arc<T>` 的固定容量 LRU。
///
/// 存 `Arc<T>` 意味着缓存持有的是**强引用**——被缓存的对象在淘汰之前不会被释放，
/// 因此 `Client::load` 返回的 `Arc` 与缓存里的是同一份数据（可长期持有）。
/// 容量是 64（见 `obtain_map_cache`），超出后按最近最少使用淘汰；缓存**没有过期时间**，
/// 数据更新后必须由调用方用 `Client::clear_cache` 主动失效。
pub(crate) type ObjectMap<T> = LruCache<i32, Arc<T>>;

/// 全局类型缓存注册表：`QUERY_PATH → 该类型的 ObjectMap`。
///
/// 因为不同对象的缓存表类型不同（`ObjectMap<Chart>` / `ObjectMap<User>` …），
/// 这里用 `Box<dyn Any>` 做**类型擦除**把它们塞进同一张 `HashMap`，取出时再
/// `downcast_mut::<ObjectMap<T>>()` 还原。键故意选 `&'static str`（`T::QUERY_PATH`）
/// 而不是 `TypeId`：这样路径本身即是缓存键，调试时可读，且天然是 `'static`。
///
/// 隐含不变量：**不同对象类型不能声明相同的 `QUERY_PATH`**，否则二者会共用一张表，
/// 取出时 `downcast` 失败并触发 `unreachable!()` panic。
type CacheMap = HashMap<&'static str, Arc<Mutex<Box<dyn Any + Send + Sync>>>>;

/// 全局缓存注册表本身，用 `std::sync::Mutex` + `Lazy` 惰性初始化。
///
/// 这里用**同步**锁是刻意的：这些表只在很短的临界区内被访问（查/插一条记录），
/// 且访问点都在 await 之前；代价是调用方必须保证不跨 await 持锁（见 `Ptr::load`
/// 里"sync locks can not be held accross await point"的注释）。
/// 若在持锁过程中 panic，锁会中毒，后续 `lock().unwrap()` 会连带 panic。
static CACHES: Lazy<Mutex<CacheMap>> = Lazy::new(Mutex::default);

/// 取出（必要时创建）`T` 对应的对象缓存表。
///
/// 返回的是表本身的 `Arc` 克隆，因此本函数末尾就释放了 `CACHES` 的全局锁——后续
/// 对表的加锁只竞争该类型自己的 `Mutex`，不同类型之间互不阻塞。新表的容量固定为 64。
///
/// 之所以返回 `Arc<Mutex<Box<dyn Any>>>` 而不是泛型化的 `Arc<Mutex<ObjectMap<T>>>`：
/// `Client::clear_cache` 等调用点需要先拿到粗糙句柄，再在同一临界区内 downcast，
/// 由返回值类型统一表达"某类型的一张表"。
pub(crate) fn obtain_map_cache<T: Object + 'static>() -> Arc<Mutex<Box<dyn Any + Send + Sync>>> {
    let mut caches = CACHES.lock().unwrap();
    Arc::clone(
        caches
            .entry(T::QUERY_PATH)
            .or_insert_with(|| Arc::new(Mutex::new(Box::new(ObjectMap::<T>::new(64.try_into().unwrap()))))),
    )
}

/// 可由 `Client::load` 泛型加载/缓存的服务端对象。
///
/// 设计意图是**用类型系统把"请求路径"与"对象类型"绑定**：调用方只写
/// `Client::load::<Chart>(id)`，路径与反序列化目标都由 `Self` 推导出来，编译器保证
/// 二者不会配错；缓存分层也因此得以实现（见 `ObjectMap`）。
///
/// 约束的由来：`Clone` 供各模型自身拷贝；`DeserializeOwned` 用于 `resp.json()`
/// （因此不能借用外部数据）；`Send + Sync` 因为对象会被 `Arc` 化后跨线程/跨 await 共享。
pub trait Object: Clone + DeserializeOwned + Send + Sync {
    /// 该对象在服务端 API 中的资源路径片段（如 `"chart"`），用于拼接 `/{QUERY_PATH}/{id}`
    /// 以及作为缓存表的键。
    const QUERY_PATH: &'static str;

    /// 返回对象主键，用作缓存键与路径参数。
    fn id(&self) -> i32;
}

/// 谱面音乐中的时间点（离线缓存的"上次播放位置"之类的场景）。
///
/// 服务端用 `"HH:MM:SS"` 字符串承载它，而本地只用总秒数计算，因此这里通过
/// `try_from`/`into` 把字符串与秒数互转，外部使用者只见 `seconds`。
///
/// 边界与兼容性：解析时 `splitn(3, ':')` 只切三段，因此两段式（`"MM:SS"`）的输入
/// 会在第三段解析失败并整体报错（返回 `"illegal position"`），**不会**退化成默认值——
/// 即格式错误会导致包含它的整个对象反序列化失败。序列化固定输出 `"00:00:SS"`，
/// 对于超过一小时的位置会输出非标准但可自洽往返的形态（秒段直接写总秒数）。
/// 该错误类型是 `&'static str` 而非 `anyhow::Error`，因为 `TryFrom` 要在 serde 的
/// `try_from` 属性中充当错误来源，越简单越不容易引入依赖。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String")]
#[serde(into = "String")]
pub struct MusicPosition {
    /// 从音乐起始到该位置的总秒数（已把小时/分钟折算进来）。
    pub seconds: u32,
}
// 反序列化方向：`"HH:MM:SS"` → 总秒数。这是服务端下发格式的唯一入口。
impl TryFrom<String> for MusicPosition {
    /// 解析失败时的错误信息（`&'static str`，见类型注释）。
    type Error = &'static str;

    /// 逐段累加：`hours * 60 + minutes` 再 `* 60 + seconds`，任一段缺失或非数字即整体失败。
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let seconds = || -> Option<u32> {
            let mut it = value.splitn(3, ':');
            let mut res = it.next()?.parse::<u32>().ok()?;
            res = res * 60 + it.next()?.parse::<u32>().ok()?;
            res = res * 60 + it.next()?.parse::<u32>().ok()?;
            Some(res)
        }()
        .ok_or("illegal position")?;
        Ok(MusicPosition { seconds })
    }
}
// 序列化方向：总秒数 → 字符串。小时位恒为 00，秒位直接写总秒数（可自洽往返，见类型注释）。
impl From<MusicPosition> for String {
    fn from(value: MusicPosition) -> Self {
        format!("00:00:{:02}", value.seconds)
    }
}

/// 难度等级类型，取值由服务端定义并以**整数**下发（而非字符串）。
///
/// 数字到变体的映射是线上协议的一部分，因此这里用 `#[repr(u8)]` 固定判别值，
/// 并只实现 `Deserialize`（网络 → 本地单向，客户端不需要回传难度类型）。
///
/// 兼容性风险：`try_from` 会拒绝未知数字，所以**服务端一旦新增难度，旧客户端在解析
/// 含该难度的谱面时会直接失败**（而不是降级展示）。`#[allow(dead_code)]` 表明部分
/// 变体在当前客户端只用于匹配、不会在本地构造。
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "u8")]
#[repr(u8)]
#[allow(dead_code)]
pub enum LevelType {
    /// Easy（0）：最低难度。
    EZ = 0,
    /// Hard（1）。
    HD,
    /// Insane（2）：Phigros 传统中的高难度档。
    IN,
    /// Another（3）：比 IN 更难，常作为"另一谱面"。
    AT,
    /// Special（4）：特殊/活动难度，用于非常规谱面。
    SP,
}
// 数字 → 枚举。这是一一对应的显式映射表，不用 `as` 转换是为了让越界值有明确的报错。
impl TryFrom<u8> for LevelType {
    /// 使用 `String` 而非 `&'static str`，因为错误信息需要带上越界的具体数值。
    type Error = String;

    /// 仅映射 0..=4；其余一律失败（见类型注释中的兼容性说明）。
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        use LevelType::*;
        Ok(match value {
            0 => EZ,
            1 => HD,
            2 => IN,
            3 => AT,
            4 => SP,
            x => {
                return Err(format!("illegal level type: {x}"));
            }
        })
    }
}

/// 指向某个服务端对象的**惰性引用**：只持有 id，需要数据时才去请求。
///
/// 与 `Arc<T>` 的区别在于"是否已经拿到数据"：
/// - `Arc<T>` 是**已解析**的共享所有权句柄，拿到即能用；
/// - `Ptr<T>` 是**未解析**的弱承诺，序列化时只是一个整数，因此可以零成本地嵌进
///   `User`/`Chart` 等对象里（例如谱面的 `uploader` 字段），避免为展示一个名字就
///   递归拉取整棵对象图（N+1 请求问题）。
///
/// 典型用法：`ptr.load().await?`（先查缓存再回源）、`ptr.fetch().await?`（强制回源）。
/// 注意它**不是** `Arc`/`Weak` 那种内存级引用，只是协议层的"外键"。
#[derive(Debug)]
pub struct Ptr<T> {
    /// 目标对象的主键，也是 `/{QUERY_PATH}/{id}` 的路径参数。
    pub id: i32,
    /// 编译期标记目标类型，使 `Ptr<Chart>` 与 `Ptr<User>` 成为不同类型（避免串用），
    /// 且无需实际持有 `T`。
    _marker: PhantomData<T>,
}
// 手写 Clone 而非 derive：derive 会给 `T` 也加上 `Clone` 约束，而这里只需要拷贝 id，
// 与 `T` 是否可克隆无关。
impl<T: Object + 'static> Clone for Ptr<T> {
    fn clone(&self) -> Self {
        Self::new(self.id)
    }
}
// 允许把裸 id 直接 `into()` 成 `Ptr`，方便从服务端返回的整数字段构造引用。
impl<T: Object + 'static> From<i32> for Ptr<T> {
    fn from(value: i32) -> Self {
        Self::new(value)
    }
}

// `Ptr` 的构造与解析入口。三个解析方法分别对应"强制回源"、"允许不存在"与"读优先"。
impl<T: Object + 'static> Ptr<T> {
    /// 用 id 构造一个未解析的引用（不发起任何请求）。
    pub fn new(id: i32) -> Self {
        Self { id, _marker: PhantomData }
    }

    /// 强制走网络解析（命中缓存也会重新请求并覆盖缓存）。用于必须拿最新数据的场景。
    ///
    /// # Errors
    /// 对象不存在或请求失败时返回错误。
    #[inline]
    pub async fn fetch(&self) -> Result<Arc<T>> {
        Client::fetch(self.id).await
    }

    /// 允许对象不存在地解析：不存在时返回 `Ok(None)`。
    ///
    /// # Errors
    /// 网络或解析失败时返回错误。
    #[inline]
    pub async fn fetch_opt(&self) -> Result<Option<Arc<T>>> {
        Client::fetch_opt(self.id).await
    }

    /// 读优先解析：先查类型对应的 LRU 缓存，未命中才回源。日常 UI 展示都应走它。
    ///
    /// # Errors
    /// 缓存未命中且网络/解析失败时返回错误。
    pub async fn load(&self) -> Result<Arc<T>> {
        // sync locks can not be held accross await point
        // 同步锁临界区：查缓存命中就直接返回；必须在 await 之前显式释放锁与句柄。
        {
            let map = obtain_map_cache::<T>();
            let mut guard = map.lock().unwrap();
            let Some(actual_map) = guard.downcast_mut::<ObjectMap<T>>() else {
                unreachable!()
            };
            if let Some(value) = actual_map.get(&self.id) {
                return Ok(Arc::clone(value));
            }
            drop(guard);
            drop(map);
        }
        self.fetch().await
    }
}
// 序列化/反序列化都只在 JSON 里留一个整数 id——这是"惰性引用"得以零成本嵌入的前提，
// 也是服务端协议约定的形态（对象引用即主键）。这些实现需要 `T: Object + 'static`
// 才能与缓存机制配套。
impl<T: Object + 'static> Serialize for Ptr<T> {
    /// 序列化成裸 id，丢弃类型信息（类型由字段声明处的 `T` 恢复）。
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i32(self.id)
    }
}
impl<'de, T: Object + 'static> Deserialize<'de> for Ptr<T> {
    /// 从裸 id 反序列化，构造出的引用处于未解析状态（不会触发网络请求）。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        i32::deserialize(deserializer).map(Self::new)
    }
}

/// 磁盘缓存目录：`<系统缓存目录>/http-cache`，供 cacache 使用。
///
/// 惰性初始化——只有在真正要读写缓存时才去探测系统缓存目录，避免启动早期失败。
/// 取不到系统缓存目录时回退到当前工作目录（`"."`），保证离线/受限环境下功能不崩，
/// 代价是缓存会落在程序目录里。`File::fetch` 以**完整 URL 字符串**为键读写这里，
/// 因此 URL 里带随机参数（如签名链接）会导致缓存永不命中。
pub static CACHE_DIR: Lazy<String> = Lazy::new(|| format!("{}/http-cache", dir::cache().unwrap_or_else(|_| ".".to_owned())));

/// 一个服务端文件资源（头像、谱面插画、谱面本体等），本质上只是 URL 的包装。
///
/// `transparent` 让它与字符串同构：序列化/反序列化就是那个 URL 字符串本身，
/// 因此它可以直接嵌在 `Chart`/`User` 等对象里而不改变线上格式。
///
/// 与 `Ptr<T>` 的分工：`Ptr` 指向"服务端有 id 的结构化对象"，`File` 指向"服务端的
/// 静态文件"——后者没有对象缓存，但有**磁盘缓存**（见 `fetch`）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct File {
    /// 资源 URL；既可能是 Phira/CDN 的 https 地址，也可能是相对路径或 `anys://` 形式的
    /// P2P 内容地址（后者由 `fetch` 负责改写）。
    pub url: String,
}
// 文件的请求构造与三层缓存策略（内存无缓存 / 磁盘 cacache / 远程回源）。
impl File {
    /// 构造请求：每次都用 `basic_client_builder` 临时建一个 client（不复用全局 `CLIENT`），
    /// 因为文件可能来自任意域名。返回值同样是"未发送"的 RequestBuilder。
    ///
    /// 三个值得注意的点：
    /// - `enable_anys` 开启时，把 `{API_URL}/files/xxx` 改写成 `{API_URL}/anys/xxx`，
    ///   走内容寻址的 P2P 网关，减少源站带宽；
    /// - 鉴权用 `CLIENT_TOKEN` 快照手动补 `Authorization`（全局 client 的默认头在这里用不上）；
    /// - 代码里保留了 `TODO: thread safety?` 的自我提醒：这里每次调用都新建 client，
    ///   没有连接池复用，高频下载会有额外开销。
    fn request(&self) -> reqwest::RequestBuilder {
        let mut req = basic_client_builder().build().unwrap().get(&self.url);
        // TODO: thread safety?
        if get_data().enable_anys {
            if let Some(path) = self.url.strip_prefix(API_URL) {
                if let Some(rest_path) = path.strip_prefix("/files/") {
                    let url = format!("{API_URL}/anys/{rest_path}");
                    req = basic_client_builder().build().unwrap().get(url);
                }
            }
        }
        if let Some(token) = CLIENT_TOKEN.load().as_ref() {
            req.header("Authorization", format!("Bearer {token}"))
        } else {
            req
        }
    }

    /// 获取文件内容（`Bytes`），带磁盘缓存与 P2P 重定向处理。
    ///
    /// # Errors
    /// - 磁盘缓存读取失败且**不是**"未命中"（如权限/损坏）时直接返回该错误；
    /// - `anys://` 之外的 3xx 重定向被判定为非法（正常重定向应由 reqwest 自动跟随，
    ///   走到这里说明是 P2P 信令之外的异常响应）；
    /// - 非 2xx 响应会把响应体当作错误信息返回。
    ///
    /// # Returns
    /// 文件字节内容；命中磁盘缓存时不会产生任何网络请求。
    pub async fn fetch(&self) -> Result<Bytes> {
        // 内层小工具：发一次请求。用 `basic_client_builder` 建的 client 自带
        // `anys://` 拦截策略，P2P 重定向会以 3xx 形式返回而不是被跟随。
        async fn fetch_raw(f: &File) -> Result<Response> {
            Ok(f.request().send().await?)
        }
        // 阶段一：先查并发安全的磁盘缓存（cacache，以 URL 为键）。
        match cacache::read(&*CACHE_DIR, &self.url).await {
            // 命中：直接返回，零网络开销。
            Ok(data) => Ok(data.into()),
            // 未命中：回源下载。
            Err(cacache::Error::EntryNotFound(..)) => {
                let mut resp = fetch_raw(self).await?;
                // 阶段二：P2P 重定向处理。服务端用 `Location: anys://<cid>` 表示
                // "该内容在 P2P 网络里，用这个 CID 去网关取"，此处把它翻译成
                // `{anys_gateway}/{cid}` 后再请求一次（缓存键仍用**原始 URL**，
                // 保证下次能直接命中）。
                if resp.status().is_redirection() {
                    let p2p_url = resp.headers().get("location").unwrap().to_str().unwrap().to_owned();
                    if let Some(cid) = p2p_url.strip_prefix("anys://") {
                        let cid = cid.to_owned();
                        let data = get_data();
                        let new_url = format!("{}/{}", data.anys_gateway, cid);
                        debug!("p2p redirection: {} -> {}", p2p_url, new_url);
                        resp = fetch_raw(&File { url: new_url }).await?
                    } else {
                        bail!("illegal p2p redirection: {}", p2p_url);
                    }
                }
                // 阶段三：校验状态并把结果写入缓存，供后续命中。
                if !resp.status().is_success() {
                    bail!("{}", resp.text().await?);
                } else {
                    let bytes = resp.error_for_status()?.bytes().await?;
                    cacache::write(&*CACHE_DIR, &self.url, &bytes).await?;
                    Ok(bytes)
                }
            }
            // 其它 cacache 错误（IO/校验失败）直接上抛，不做降级重试。
            Err(err) => Err(err.into()),
        }
    }

    /// 把文件内容解码成图片（头像、插画等）。走 `fetch`，因此同样受磁盘缓存加速。
    ///
    /// # Errors
    /// 下载失败，或内容不是可识别的图片格式（由 `image` crate 判定）。
    pub async fn load_image(&self) -> Result<DynamicImage> {
        Ok(image::load_from_memory(&self.fetch().await?)?)
    }

    /// 加载图片的**缩略图**，按 URL 所属域名选用不同的压缩方案。
    ///
    /// 三种情况（顺序即优先级）：
    /// - `phira.mivik.cn`（旧图床）：用其图片处理服务，URL 追加 `?imageView/0/w/{W}/h/{H}`；
    /// - `files.phira.cn` / `phira.5wyxi.com/files/`：用预生成的 `.thumbnail` 变体
    ///   （服务端在对象存储里并排存放的缩略图），比现算更快；
    /// - 其它域名（含自建服务端）：没有缩略图能力，只能下载原图，UI 侧需自行缩放。
    ///
    /// 注意这几种判断都是**硬编码域名**，因此换服务端地址时会退化为下载原图。
    pub async fn load_thumbnail(&self) -> Result<DynamicImage> {
        if self.url.starts_with("https://phira.mivik.cn/") {
            File {
                url: format!("{}?imageView/0/w/{THUMBNAIL_WIDTH}/h/{THUMBNAIL_HEIGHT}", self.url),
            }
            .load_image()
            .await
        } else if self.url.starts_with("https://files.phira.cn/") || self.url.starts_with("https://phira.5wyxi.com/files/") {
            File {
                url: format!("{}.thumbnail", self.url),
            }
            .load_image()
            .await
        } else {
            self.load_image().await
        }
    }
}

/// 角色（看板娘）配置：主界面立绘与名字板的展示参数。
///
/// 多数字段来自服务端下发，但**本类型同时用于本地内置角色**（见 `Default`），
/// 因此部分字段带 `#[serde(default)]` 以便老版本缓存文件缺少新字段时仍能解析。
/// 名字的排版细节（字号、基线、立绘微调）都做成数据驱动的可选参数，避免为不同语言
/// 的角色改代码——这正是 `name_size`/`baseline`/`illu_adjust` 存在的理由。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Character {
    /// 角色标识（如 `"shee"`）；同时是英文名推导的输入（见 `name_en`）。
    pub id: String,
    /// 展示用角色名（可为中文，随语言本地化）。
    pub name: String,
    /// 角色介绍文案。
    pub intro: String,
    /// 立绘资源路径；`"@"` 是特殊约定，表示使用内置默认立绘（见 `Default`）。
    pub illust: String,
    /// 画师署名。
    pub artist: String,
    /// 设计者署名（可能与画师不同）。
    pub designer: String,

    /// 名字板的字号覆盖值（逻辑像素）；`None` 表示沿用主题默认字号。
    /// 服务端可能不返回该字段，故加 `#[serde(default)]`。
    #[serde(default)]
    pub name_size: Option<f32>,

    /// 名字板是否按基线对齐（用于拉丁字母与方块字混排时的视觉微调）。
    #[serde(default)]
    pub baseline: bool,

    /// 立绘的四元组微调量（含义由渲染层解释，通常为位置/缩放偏移）。
    #[serde(default)]
    pub illu_adjust: (f32, f32, f32, f32),

    /// 英文名的**惰性缓存**，由 `id` 推导（见 `name_en()`）。
    /// `#[serde(skip)]`：它是纯本地派生态，既不下发也不上报，避免与服务端字段冲突。
    #[serde(skip)]
    name_en: Option<String>,
}
// 内置默认角色：服务端不可用或未配置角色时兜底，保证主界面一定有内容可渲染。
// 其中名字与介绍走本地化宏 `ttl!`，因此会随当前语言变化；`illust` 用 `"@"` 触发内置立绘。
impl Default for Character {
    fn default() -> Self {
        Self {
            id: "shee".to_owned(),
            name: ttl!("main-character-name").into_owned(),
            intro: ttl!("main-character-intro").into_owned(),
            illust: "@".to_owned(),
            artist: "清水QR".to_owned(),
            designer: "清水QR".to_owned(),

            name_size: None,

            baseline: false,

            illu_adjust: (0., 0., 0., 0.),

            name_en: None,
        }
    }
}
// 角色名的派生与缓存。之所以把英文名做成"按需计算 + 只算一次"，是因为它只在需要
// 展示英文名时才有用，而计算涉及字符串分配。
impl Character {
    /// 返回英文名，首次调用时由 `id` 推导并**写入缓存**（故需要 `&mut self`）。
    ///
    /// 推导规则：按 `_` 分词，每词首字母大写、其余保持原样，用空格连接（如
    /// `"shee"` → `"Shee"`，`"xing_chen"` → `"Xing Chen"`）。
    ///
    /// # Panics
    /// 若 `id` 以下划线开头/结尾或含连续下划线，分词会产生空片段，
    /// 而 `split_at(1)` 对空字符串会 panic——即**该规则要求 id 的每一段都非空**。
    pub fn name_en(&mut self) -> &str {
        if self.name_en.is_none() {
            let words = self.id.split('_');
            let mut name_en = String::new();
            for word in words {
                let (first, rest) = word.split_at(1);
                name_en.push_str(&first.to_uppercase());
                name_en.push_str(rest);
                name_en.push(' ');
            }
            // 逐词追加时每词后都留了空格，这里去掉最后一个。
            if !name_en.is_empty() {
                name_en.pop();
            }
            self.name_en = Some(name_en);
        }
        self.name_en.as_ref().unwrap()
    }
}
