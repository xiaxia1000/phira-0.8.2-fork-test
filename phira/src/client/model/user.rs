//! 用户（User）网络对象、权限模型与全局用户信息缓存。
//!
//! 本文件承担两件事，边界值得留意：
//! 1. **权限模型**：服务端只下发 `User::roles`（角色位掩码），客户端用 `Roles::perms`
//!    换算成具体权限 `Permissions`。也就是说**角色是服务端事实，权限是客户端推导**，
//!    服务端仍会独立校验，客户端权限只用于界面显隐。
//! 2. **用户信息缓存**：[`UserManager`] 为 UI 提供"同步取名字/颜色/头像"的能力，
//!    后台异步预取，避免在主循环里发网络请求。
//!
//! 字段名部分依赖服务端 JSON 的驼峰形式（如 `badgeNames`）。

use super::{File, Object};
use crate::client::Client;
use anyhow::Result;
use bitflags::bitflags;
use chrono::{DateTime, Utc};
use image::DynamicImage;
use macroquad::prelude::Color;
use once_cell::sync::Lazy;
use prpr::{ext::SafeTexture, task::Task};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tracing::warn;

// 权限位掩码。位值由服务端与客户端**共同约定**，不可随意改动；每个位代表一项
// 具体的后台/审核能力，客户端用它来控制界面元素的显隐（服务端仍会二次校验）。
bitflags! {
    #[derive(Default, Debug, Clone, Copy)]
    pub struct Permissions: i64 {
        // 上传谱面（默认登录用户即拥有，被封禁时会被移除）。
        const UPLOAD_CHART      = 0x00000001;
        // 查看未审核谱面。
        const SEE_UNREVIEWED    = 0x00000002;
        // 删除未上架（unstable）谱面。
        const DELETE_UNSTABLE   = 0x00000004;
        // 审核谱面（通过/打回）。
        const REVIEW            = 0x00000008;
        // 查看上架申请列表。
        const SEE_STABLE_REQ    = 0x00000010;
        // 将谱面置为稳定/上架。
        const STABILIZE_CHART   = 0x00000020;
        // 编辑谱面标签。
        const EDIT_TAGS         = 0x00000040;
        // 稳定判定（对"上架判定"的复核）。
        const STABILIZE_JUDGE   = 0x00000080;
        // 删除已上架谱面。
        const DELETE_STABLE     = 0x00000100;
        // 查看全部活动（含未开始/已结束）。
        const SEE_ALL_EVENTS    = 0x00000200;
        // 封禁用户。
        const BAN_USER          = 0x00000400;
        // 设置谱面的"上榜"（ranked）状态。
        const SET_RANKED        = 0x00000800;
        // 设置任意角色（最高权限之一）。
        const SET_ALL_ROLE      = 0x00001000;
        // 任命/撤销审核员。
        const SET_REVIEWER      = 0x00002000;
        // 任命/撤销管理员（supervisor）。
        const SET_SUPERVISOR    = 0x00004000;
        // 封禁头像（内容违规处置）。
        const BAN_AVATAR        = 0x00008000;
        // 审核 PecJam（特殊活动/赛道）相关投稿。
        const REVIEW_PECJAM     = 0x00010000;
    }
}

// 角色位掩码。角色由服务端下发，是 `Permissions` 的来源；位值同样是与服务端
// 约定的协议常量，`HEAD_*` 表示该职能的负责人（权限是普通成员的超集）。
bitflags! {
    #[derive(Default, Debug, Clone, Copy)]
    pub struct Roles: i32 {
        // 管理员：拥有全部权限（见 `perms`）。
        const ADMIN             = 0x0001;
        // 审核员：负责谱面审核。
        const REVIEWER          = 0x0002;
        // 管理员（supervisor）：负责上架与治理。
        const SUPERVISOR        = 0x0004;
        // 管理员负责人：在 SUPERVISOR 之上追加高权限操作。
        const HEAD_SUPERVISOR   = 0x0008;
        // 审核负责人：在 REVIEWER 之上追加封禁与任命权限。
        const HEAD_REVIEWER     = 0x0010;
        // PecJam 专项审核员。
        const PECJAM_REVIEWER   = 0x0020;
        // 版主/社区管理（当前未映射到任何 `Permissions`，保留位）。
        const MODERATOR         = 0x0040;
    }
}

// 角色 → 权限的换算规则（唯一的推导入口）。之所以放在客户端而不是直接用服务端下发的
// 权限位：减少协议耦合——服务端只需下发角色，权限矩阵的调整可通过客户端发版完成。
// 注意各分支是**累加**的（`|=`），因此 `ADMIN` 之外的组合也能叠加生效；
// `ADMIN` 是短路全量（直接 `Permissions::all()` 覆盖之前的累加结果）。
impl Roles {
    /// 按角色位计算权限集合。
    ///
    /// # Arguments
    /// - `banned`：账号是否被封禁。封禁只影响 `UPLOAD_CHART` 这一项（其余权限保留，
    ///   以免管理员误封自己时失去运维入口）——这是刻意的取舍，不是遗漏。
    ///
    /// # Returns
    /// 该角色组合对应的权限集合。
    pub fn perms(&self, banned: bool) -> Permissions {
        let mut perm = Permissions::empty();
        // 任何未被封禁的账号都可以上传谱面。
        if !banned {
            perm |= Permissions::UPLOAD_CHART;
        }
        // 管理员直接取全集，跳过后续逐项累加。
        if self.contains(Self::ADMIN) {
            perm = Permissions::all();
        }
        // 审核员：看未审核 + 删未上架 + 审核 + 改标签。
        if self.contains(Self::REVIEWER) {
            perm |= Permissions::SEE_UNREVIEWED;
            perm |= Permissions::DELETE_UNSTABLE;
            perm |= Permissions::REVIEW;
            perm |= Permissions::EDIT_TAGS;
        }
        // 审核负责人：追加封人 + 任命审核员。
        if self.contains(Self::HEAD_REVIEWER) {
            perm |= Permissions::BAN_USER;
            perm |= Permissions::SET_REVIEWER;
        }
        // 管理员（supervisor）：上架相关能力。
        if self.contains(Self::SUPERVISOR) {
            perm |= Permissions::SEE_UNREVIEWED;
            perm |= Permissions::SEE_STABLE_REQ;
            perm |= Permissions::STABILIZE_CHART;
            perm |= Permissions::EDIT_TAGS;
        }
        // 管理员负责人：高权限操作（复核、删已上架、设上榜、任命管理员）。
        if self.contains(Self::HEAD_SUPERVISOR) {
            perm |= Permissions::STABILIZE_JUDGE;
            perm |= Permissions::DELETE_STABLE;
            perm |= Permissions::SET_RANKED;
            perm |= Permissions::SET_SUPERVISOR;
        }
        // PecJam 专项审核。
        if self.contains(Self::PECJAM_REVIEWER) {
            perm |= Permissions::REVIEW_PECJAM;
        }
        perm
    }
}

/// 一个 Phira 用户/账号。
///
/// 容器级 `#[serde(default)]` 让**所有字段都可缺失**（缺省走 `Default`），这是有意为之：
/// 该结构既用于网络响应，也被本地缓存持久化，服务端增删字段时旧/新数据都能解析，
/// 避免因一个新增字段导致整份用户数据不可用。
///
/// `roles` 是**原始位掩码**而非枚举，且不做解析（可能含本版本不认识的高位），
/// 权限一律通过 `perms()` 换算——见 `Roles::perms`。
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    /// 用户 id（主键，也是 `Ptr<User>` 与缓存键使用的值）。
    pub id: i32,
    /// 昵称（可由用户修改，因此不要把它当作稳定标识）。
    pub name: String,
    /// 邮箱；仅在查看**自己**的资料时由服务端下发，他人资料为 `None`（隐私考虑）。
    pub email: Option<String>,
    /// 绑定的好游快爆渠道 uid；未绑定时为 `None`，也是"是否绑定渠道"的判据。
    pub hykb_uid: Option<i64>,
    /// 头像文件；未设置头像时为 `None`（界面需回退到默认头像）。
    pub avatar: Option<File>,
    /// 当前展示的单个徽章标识（与 `badges` 的区别：这是"选中"的那一个）。
    pub badge: Option<String>,
    /// 拥有的全部徽章标识列表；`name_color` 依赖它判断身份配色。
    pub badges: Vec<String>,
    /// 徽章标识 → 展示名的映射（服务端字段名为 `badgeNames`）。
    /// 用 `HashMap` 是因为界面只做"按标识查名字"的随机访问。
    #[serde(rename = "badgeNames")]
    pub badge_names: HashMap<String, String>,
    /// 用户偏好的语言标识，用于跨设备同步语言设置。
    pub language: String,
    /// 个性签名；可为空。
    pub bio: Option<String>,
    /// 经验值（整数，可能为负，故用 `i64` 而非无符号）。
    pub exp: i64,
    /// 排名分（RKS）：Phira 的核心实力指标，浮点。
    pub rks: f32,
    /// **原始角色位掩码**，与 `Roles` 的位定义对应；不要直接比较数字，用 `perms()`。
    pub roles: i32,

    /// 注册时间，UTC。
    pub joined: DateTime<Utc>,
    /// 最近一次登录时间，UTC（用于展示"最后在线"）。
    pub last_login: DateTime<Utc>,
}
// 接入泛型对象机制：`QUERY_PATH = "user"` 使 `Client::load::<User>(id)` 可用，
// 请求路径为 `GET /user/{id}`，并享有独立的 LRU 缓存。
impl Object for User {
    /// 资源路径片段，同时作为缓存表键。
    const QUERY_PATH: &'static str = "user";

    /// 主键即 `id` 字段。
    fn id(&self) -> i32 {
        self.id
    }
}
// 权限与展示相关的派生方法。全部为纯函数式读取，不做任何缓存或网络访问。
impl User {
    /// 换算当前用户的权限集合。
    ///
    /// 注意这里**恒传 `banned: false`**：`User` 结构中没有封禁字段（封禁状态体现在
    /// 后续请求被拒上），因此客户端无从判断，只能按未封禁换算；界面若需要体现封禁，
    /// 必须依赖服务端单独下发（如新增字段后改这里）。
    ///
    /// # Returns
    /// 由 `roles` 推导出的权限集合。
    pub fn perms(&self) -> Permissions {
        // 用 `from_bits_retain` 而非 `from_bits`：保留本版本不认识的高位，避免服务端
        // 新增角色时把已识别的位一起丢掉。
        Roles::from_bits_retain(self.roles).perms(false)
    }

    /// 判断是否拥有某项权限（等价于 `self.perms().contains(perm)`）。
    ///
    /// 每次调用都会重新换算权限矩阵，因此**热路径上应缓存结果**（如把 `perms()` 的
    /// 返回值存进界面状态），而不是在每帧循环里反复调用。
    pub fn has_perm(&self, perm: Permissions) -> bool {
        Roles::from_bits_retain(self.roles).perms(false).contains(perm)
    }

    /// 昵称的展示颜色。
    ///
    /// 纯客户端显示策略，与权限无关：拥有 `admin` 徽章为紫色、`sponsor` 徽章为橙色，
    /// 其余为白色。徽章标识是**服务端字符串约定**，改色只需改这里。
    pub fn name_color(&self) -> Color {
        Color::from_hex_rgb(if self.badges.iter().any(|it| it == "admin") {
            0x673ab7
        } else if self.badges.iter().any(|it| it == "sponsor") {
            0xff7043
        } else {
            0xffffff
        })
    }
}

/// 单个用户信息的异步加载任务：成功时给出头像原图，`None` 表示该用户没有头像。
///
/// 注意出口是 `DynamicImage`（CPU 侧图）而非纹理：下载与解码可以放在任意线程做，
/// **纹理上传必须在渲染线程**完成，因此这一步被推迟到 `UserManager::get_avatar` 里。
type UserTask = Task<Result<Option<DynamicImage>>>;
/// 进行中/已完成的任务表，键为用户 id；见 `UserManager::request` 的去重逻辑。
type UserTaskMap = HashMap<i32, UserTask>;
/// 用户展示信息的三元组：`(昵称, 昵称颜色, 头像纹理槽位)`。
///
/// 头像槽位是**双层 Option**，三层状态各有含义：
/// - `None`：还没处理过任务结果（尚未加载或正在加载）；
/// - `Some(None)`：已处理完成，确定该用户没有头像；
/// - `Some(Some(tex))`：头像纹理已就绪，可直接用于绘制。
type UserResult = (String, Color, Option<Option<SafeTexture>>);
/// 已解析结果的表（键为用户 id），供 UI 同步读取。
type UserResultMap = HashMap<i32, UserResult>;

/// 用户信息的后台加载任务表。
///
/// 这里刻意用 `tokio::sync::Mutex`（**异步**锁）而非 `std::sync::Mutex`：同一把锁既要
/// 在任务内部（async 上下文）用 `lock().await` 访问，又要在 UI 线程的同步函数里用
/// `blocking_lock()` 访问。与之相对，`client::model` 中的对象缓存用的是同步锁，
/// 因为那里只在短临界区内同步访问、且不允许跨 await 持锁。
///
/// `blocking_lock()` 在 async 运行时线程上调用会 panic，因此**这些方法只能从非异步
/// 的运行主循环调用**（本项目的 UI 线程满足该条件）。
static TASKS: Lazy<Mutex<UserTaskMap>> = Lazy::new(Mutex::default);
/// 已解析的用户展示信息表（昵称/颜色/头像），UI 每帧只读这里，绝不发请求。
static RESULTS: Lazy<Mutex<UserResultMap>> = Lazy::new(Mutex::default);

/// 全局用户名/头像缓存的对外入口（零大小类型，只有关联函数）。
///
/// 设计目标是让"绘制玩家昵称"这类高频操作变成纯内存读取：
/// - 写入侧 `request(id)` 发一次异步请求，结果落进 `RESULTS`；
/// - 读取侧 `name_and_color(id)` / `get_avatar(id)` 都是同步的、不会阻塞渲染；
/// - 数据过期或用户改了昵称/头像后，用 `clear_cache(id)` 强制下次重新拉取。
///
/// 返回值语义统一为"查不到就返回 `None`"，调用方需要自行提供占位显示。
pub struct UserManager;

// UserManager 的同步读取 + 异步预取实现。所有方法都通过全局 TASKS/RESULTS 协作，
// 加锁顺序固定为 TASKS → RESULTS，修改时需保持该顺序以避免死锁。
impl UserManager {
    /// 丢弃某用户的缓存，使其下次被访问时重新请求。
    ///
    /// 典型调用时机：用户改名/换头像后、或检测到服务端数据已变。两处缓存同时清理，
    /// 否则会出现"名字已更新但头像仍是旧的"这类不一致。
    ///
    /// # Returns
    /// 恒为 `Ok(())`——保留 `Result` 只是为了与其它缓存清理接口保持一致的调用风格。
    pub fn clear_cache(id: i32) -> Result<()> {
        TASKS.blocking_lock().remove(&id);
        RESULTS.blocking_lock().remove(&id);
        Ok(())
    }

    /// 发起（或在已有任务时跳过）某用户信息的异步加载。
    ///
    /// 幂等去重：只要 `TASKS` 里还有该 id 的条目就直接返回。注意任务完成后条目**不会**
    /// 被移除（结果取走后任务输出变为 `None`），因此除非 `clear_cache`，否则不会重复请求——
    /// 这是"一次请求、长期复用"的取舍，代价是数据不会自动刷新。
    ///
    /// 任务内部分两步：
    /// 1. 先 `Client::load(id)` 取用户（走对象缓存），把昵称与颜色写入 `RESULTS`
    ///    （**保留已有的头像槽位**，避免覆盖已经加载好的纹理）；
    /// 2. 再下载头像原图并作为任务结果返回，纹理化留给 `get_avatar`。
    pub fn request(id: i32) {
        let mut tasks = TASKS.blocking_lock();
        if tasks.contains_key(&id) {
            return;
        }
        tasks.insert(
            id,
            Task::new(async move {
                let user: Arc<User> = Client::load(id).await?;
                {
                    // 阶段一：更新昵称与颜色。已存在条目时只改前两项，保持头像槽位不变。
                    let mut guard = RESULTS.lock().await;
                    if let Some((name, color, ..)) = guard.get_mut(&id) {
                        *name = user.name.clone();
                        *color = user.name_color();
                    } else {
                        guard.insert(id, (user.name.clone(), user.name_color(), None));
                    }
                }
                // 阶段二：下载并解码头像；无头像则返回 `Ok(None)`（会被记为 `Some(None)`）。
                if let Some(avatar) = &user.avatar {
                    Ok(Some(image::load_from_memory(&avatar.fetch().await?)?))
                } else {
                    Ok(None)
                }
            }),
        );
    }

    /// 同步读取某用户的昵称与颜色。
    ///
    /// # Returns
    /// 已加载完成时返回 `(昵称, 颜色)`；数据尚未就绪（或从未请求过）时返回 `None`，
    /// 调用方应显示占位文本并确保此前调用过 `request`。
    pub fn name_and_color(id: i32) -> Option<(String, Color)> {
        let names = RESULTS.blocking_lock();
        if let Some((name, color, ..)) = names.get(&id) {
            Some((name.to_owned(), *color))
        } else {
            None
        }
    }

    /// 同步获取某用户的头像纹理。
    ///
    /// 三态返回：
    /// - `None`：还没就绪（请求进行中或尚未请求）；
    /// - `Some(None)`：确定该用户没有头像；
    /// - `Some(Some(tex))`：纹理可用。
    ///
    /// 本函数也是**任务结果的一次性消费点**：若任务已完成，这里把 `DynamicImage`
    /// 上传为纹理并写入 `RESULTS`；失败则 `warn!` 并**移除任务条目**，使后续
    /// `request` 可以重试（与成功路径的"不重试"形成对比）。
    ///
    /// # Panics
    /// 对 `RESULTS` 条目的 `unwrap()` 依赖"任务成功时必然已写入 RESULTS"这一不变量；
    /// 由于 `clear_cache` 也按 TASKS → RESULTS 的顺序加锁，且本函数在持 TASKS 锁期间
    /// 才访问 RESULTS，二者不会交错破坏该不变量。
    pub fn get_avatar(id: i32) -> Option<Option<SafeTexture>> {
        let mut guard = TASKS.blocking_lock();
        if let Some(task) = guard.get_mut(&id) {
            // `take()` 取出任务输出；已取过（为 None）则说明结果早已处理，无需重复。
            if let Some(result) = task.take() {
                match result {
                    Err(err) => {
                        // 加载失败：清掉任务，让下一次 request 重新尝试。
                        warn!("Failed to fetch user info: {err:?}");
                        guard.remove(&id);
                    }
                    Ok(image) => {
                        // 纹理上传发生在调用线程（渲染线程），并生成 mipmap 以支持缩放采样。
                        RESULTS.blocking_lock().get_mut(&id).unwrap().2 = Some(image.map(|it| SafeTexture::from(it).with_mipmap()));
                    }
                }
            }
        } else {
            // 没有任务条目时显式释放锁再读结果，避免无谓地持有 TASKS 锁。
            drop(guard);
        }
        RESULTS.blocking_lock().get(&id).and_then(|it| it.2.clone())
    }

    /// 取头像，并区分"未就绪"与"确实没有头像"两种情况。
    ///
    /// # Returns
    /// - `Ok(Some(tex))`：头像可用；
    /// - `Ok(None)`：该用户确实没有头像，调用方应显示默认头像；
    /// - `Err(fallback)`：尚未就绪，错误值就是传入的**占位纹理**，可直接用于本帧绘制，
    ///   下一帧再调用即可（这种"用错误通道传递占位资源"的设计让调用方无需额外状态）。
    pub fn opt_avatar(id: i32, tex: &SafeTexture) -> Result<Option<SafeTexture>, SafeTexture> {
        Self::get_avatar(id).map(|it| it.ok_or_else(|| tex.clone())).transpose()
    }
}
