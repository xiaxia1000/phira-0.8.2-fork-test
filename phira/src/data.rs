//! # 本地持久化数据根
//!
//! 本模块定义 `data.json` 的**唯一持久化根** [`Data`] 及其相关结构。`data.json` 记录了
//! 玩家的一切本地状态：登录账号与凭据、用户设置、本地谱面索引、收藏夹、资源包等。
//! 内存中的单例由 `crate::DATA` 持有（见 `lib.rs` 的 `get_data` / `save_data`），
//! 本模块只负责「结构定义 + 启动时的迁移与扫描逻辑」。
//!
//! 两条重要约束：
//! - **兼容性优先**：结构里的每个字段都可能来自任意历史版本，因此新字段一律加
//!   `#[serde(default)]`、旧字段保留兼容处理，不能直接删除或重命名。
//! - **轻量化**：本地索引使用 [`LocalChart`]/[`BriefChartInfo`] 而非网络层的
//!   `Chart`/`ChartInfo`，避免每次启动都要解析网络对象、也避免把网络字段写进本地文件。

use crate::{
    client::{Character, Chart, LocalCollection, Ptr, User},
    dir,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use prpr::{
    config::{Config, Mods},
    info::ChartInfo,
    scene::SimpleRecord,
    ui::PREFER_REDUCED_MOTION,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    ops::DerefMut,
    path::Path,
    sync::{atomic::Ordering, Arc},
};
use tracing::{debug, warn};
use uuid::Uuid;

// 启动扫描时，单个谱面条目允许的最大导入重试次数。
// 设为 1~2 这种很小的值，是因为失败通常由「谱面文件本身损坏」导致，
// 重试再多也无效，反而每次启动都会拖慢速度；超过上限即删除该条目录。
const MAX_IMPORT_RETRIES: u8 = 2;

/// 谱面的「简介级」信息，本地索引与列表展示都使用它。
///
/// 之所以不直接用引擎的 `ChartInfo`：本地索引会被整体序列化进 `data.json`，
/// 必须只保留展示与匹配所需的最小字段集（名称、难度、谱师等），
/// 且不能随引擎字段演进而反复破坏旧存档的兼容性。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefChartInfo {
    /// 服务端谱面 id；手动导入的本地谱面没有 id，故为 `None`。
    /// 该字段同时也是「本地谱面 vs 网络谱面」的判据之一（`custom/` 无 id，`download/` 有 id）。
    pub id: Option<i32>,
    /// 上传者。用 [`Ptr`] 惰性持有：启动时无需拉取用户资料，真正展示时再解析。
    pub uploader: Option<Ptr<User>>,
    /// 曲名。
    pub name: String,
    /// 难度标签（如 `IN`、`AT`），是展示用的短名。
    pub level: String,
    /// 难度定数（数值），用于排序与筛选。
    pub difficulty: f32,
    /// 谱面简介。`alias = "description"` 用于读取旧版本写下的字段名。
    #[serde(alias = "description")]
    pub intro: String,
    /// 谱师。
    pub charter: String,
    /// 曲师。
    pub composer: String,
    /// 曲绘作者。
    pub illustrator: String,
    /// 创建时间。本地谱面没有该信息（旧存档也没有），故整体为 `Option`。
    pub created: Option<DateTime<Utc>>,
    /// 最近更新时间，含义同上。
    pub updated: Option<DateTime<Utc>>,
    /// 谱面（而非元信息）的最近更新时间，用于提示「谱面已更新」。
    pub chart_updated: Option<DateTime<Utc>>,
    /// 该谱面是否存在「解锁视频」。用 `default` 兼容旧存档，
    /// 因为该信息只有引擎的 `ChartInfo` 才有（见下方 `From<ChartInfo>`）。
    #[serde(default)]
    pub has_unlock: bool,
}

// 从网络层的 `Chart` 构造本地简介。网络对象字段齐全，故 id/uploader/时间都有值；
// `has_unlock` 固定为 `false`——网络列表不返回该信息，只有进入详情拉取 `ChartInfo` 才知道。
impl BriefChartInfo {
    /// 由网络谱面对象生成简介信息。
    pub fn from_chart(chart: &Chart) -> Self {
        Self {
            id: Some(chart.id),
            uploader: Some(chart.uploader.clone()),
            name: chart.name.clone(),
            level: chart.level.clone(),
            difficulty: chart.difficulty,
            intro: chart.description.clone().unwrap_or_default(),
            charter: chart.charter.clone(),
            composer: chart.composer.clone(),
            illustrator: chart.illustrator.clone(),
            created: Some(chart.created),
            updated: Some(chart.updated),
            chart_updated: Some(chart.chart_updated),
            has_unlock: false,
        }
    }
}

// 从引擎的 `ChartInfo` 转换。这里的 `unlock_video.is_some()` 是 `has_unlock` 的唯一来源：
// 该字段被解析出来即代表存在解锁视频，本地谱面导入后也据此记住，无需下次再解析谱面。
impl From<ChartInfo> for BriefChartInfo {
    fn from(info: ChartInfo) -> Self {
        Self {
            id: info.id,
            uploader: info.uploader.map(Ptr::new),
            name: info.name,
            level: info.level,
            difficulty: info.difficulty,
            intro: info.intro,
            charter: info.charter,
            composer: info.composer,
            illustrator: info.illustrator,
            created: info.created,
            updated: info.updated,
            chart_updated: info.chart_updated,
            has_unlock: info.unlock_video.is_some(),
        }
    }
}

/// 本地谱面索引项：在 [`BriefChartInfo`] 之上补充「怎么找到它、玩过什么状态」。
///
/// 与网络层的 `Chart` 分开，是因为本地谱面必须自包含地知道自己的磁盘位置与上次游玩结果，
/// 而这些概念在网络数据里并不存在。
#[derive(Serialize, Deserialize)]
pub struct LocalChart {
    /// 谱面简介信息，直接平铺（`flatten`）在本结构里，
    /// 因此序列化后与旧版本的字段布局保持一致，不需要额外的嵌套层级。
    #[serde(flatten)]
    pub info: BriefChartInfo,
    /// 相对于 `data/charts` 的路径（`custom/<uuid>` 或 `download/<id>`）。
    /// 存相对路径而非绝对路径，是为了在更换设备/安装目录后仍能正确定位。
    pub local_path: String,
    /// 本地记录的最近一次游玩成绩，用于列表上直接展示分数。
    pub record: Option<SimpleRecord>,
    /// 该谱面记住的 mod 配置（如速度倍率），随谱面一起保存，下次进入该谱面直接沿用。
    #[serde(default)]
    pub mods: Mods,
    /// 是否已经「玩过一次解锁视频」（解锁视频只播一次）。
    #[serde(default)]
    pub played_unlock: bool,
}

/// `anys_gateway` 的默认值：anys 是官方之外的自建网关，用于改善部分地区（如国内）的
/// 连通性。用函数而非常量作为 `serde` 默认值，是因为要生成一个新的 `String`。
fn default_anys_gateway() -> String {
    "https://anys.mivik.moe".to_string()
}

/// 本地持久化数据的根结构，与 `data.json` 一一对应。
///
/// 整个应用只有一份（`crate::DATA`）。`#[serde(default)]` 作用于结构体级别，
/// 意味着**任意字段缺失都能反序列化成功**——这是升级兼容的核心保障：老存档缺新字段时
/// 走 `Default`，新存档被老版本读取时也只会忽略未知字段，不会导致启动失败。
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Data {
    /// 当前登录用户；`None` = 未登录（离线模式或已登出）。
    /// 与 `tokens` 必须同时存在才构成有效会话。
    pub me: Option<User>,
    /// 本地谱面索引（手动导入 + 已下载）。
    pub charts: Vec<LocalChart>,
    /// 以 `local_path` 为键的本地成绩缓存。与 `charts[i].record` 的区别是：
    /// 这里保留那些**已不在** `charts` 中的路径的记录（如谱面被删除后重现），
    /// 且启动时会清理磁盘上已不存在的条目。
    pub local_records: HashMap<String, Option<SimpleRecord>>,
    /// 用户设置（音量、判定偏移、离线模式等），自身含 `init()` 做字段补全。
    pub config: Config,
    /// 上次检查私信的时间，用于只拉取增量消息。
    pub message_check_time: Option<DateTime<Utc>>,
    /// 界面语言。`None` 表示尚未设置，由 `crate::sync_data` 按系统语言/默认值补齐。
    pub language: Option<String>,
    /// 主题（配色方案）索引。
    pub theme: usize,
    /// 登录凭据 `(access_token, refresh_token)`。
    /// 前者用于所有业务请求的 `Authorization`，后者仅用于换取新的 access token。
    /// 属于敏感数据，任何日志都不应输出其内容。
    pub tokens: Option<(String, String)>,
    /// 已安装资源包的文件名列表（不含路径），实际文件位于 `data/respack/`。
    pub respacks: Vec<String>,
    /// 当前选中的资源包下标；等于 `respacks.len()` 表示「不使用任何资源包」
    /// （因此启动时会做 `min(len)` 夹取，越界一律退化为不使用）。
    pub respack_id: usize,
    /// 是否接受无效的 HTTPS 证书（用于自签证书的自建服务器/抓包调试）。
    pub accept_invalid_cert: bool,
    // for compatibility
    /// 旧版本用来记录「是否已读并同意协议」的布尔值，已被 `terms_modified` 取代。
    /// 保留字段仅为反序列化老存档，启动迁移后会置回 `false`（见 `init`）。
    pub read_tos_and_policy: bool,
    /// 玩家**已同意**的协议版本（服务端的 `Last-Modified` 时间戳字符串）。
    /// 与最新版本不一致时需重新弹窗；`None` 表示尚未同意任何版本。
    pub terms_modified: Option<String>,
    /// 玩家选择「忽略」的版本号，用于不再提示「有新版本」。
    pub ignored_version: Option<semver::Version>,
    /// 玩家选定的角色/看板娘。
    pub character: Option<Character>,

    /// 是否启用自定义网关（anys）。用于绕过官方域名的连通性问题。
    pub enable_anys: bool,
    /// 自定义网关地址；未显式配置时用 [`default_anys_gateway`] 的默认值。
    #[serde(default = "default_anys_gateway")]
    pub anys_gateway: String,

    /// 无障碍选项：偏好「减少动效」。该值在启动时同步到全局
    /// `PREFER_REDUCED_MOTION`，供引擎跳过过渡动画（对晕动症玩家友好）。
    pub prefer_reduced_motion: bool,

    /// 【历史字段】旧版本把收藏夹直接内嵌在 `data.json` 里。
    /// 现改为每个收藏夹一个独立文件（见 `collection_uuids`），此字段仅用于
    /// 读取旧存档并在 `init` 中迁移，迁移后即被清空（`drain`）。
    /// `rename = "collections"` 保持与旧存档的键名一致。
    #[serde(default, rename = "collections")]
    collections_legacy: Vec<LocalCollection>,
    /// 收藏夹的 **uuid 列表**，顺序即界面上展示的顺序。
    /// 内容实体存放在 `data/collections/<uuid>.json`，列表只保存引用，
    /// 这样调整顺序/增删都是轻量操作，也便于与云端同步。
    #[serde(default)]
    collection_uuids: Vec<Uuid>,

    /// Need to know what path caused the problem when restarting the program next time
    /// see: https://github.com/TeamFlos/phira/pull/689/#discussion_r2899026506
    /// 启动导入扫描的重试计数（键为 `local_path`）。
    /// 必须持久化：若某个谱面会让解析崩溃，进程会在扫描阶段挂掉，计数不落盘就会
    /// 每次启动无限重试——持久化后才能在超过 [`MAX_IMPORT_RETRIES`] 时删除该条目并继续启动。
    #[serde(default)]
    pub import_scan_retry: HashMap<String, u8>,

    /// 收藏夹实体的运行期缓存（uuid → 已解析内容），**不写入磁盘**。
    /// 用 `DashMap` 是因为读取路径（渲染每帧取收藏名）需要并发且廉价的访问；
    /// 用 `skip` 是因为它随时可由 `data/collections/*.json` 重建。
    #[serde(skip)]
    collection_cache: DashMap<Uuid, Arc<LocalCollection>>,
}

// `Data` 的运行时行为：启动初始化（迁移 + 扫描 + 清理）与收藏夹的增删改查。
// 所有方法都假定运行在主线程，且 `data.json` 的整体写入由调用方通过 `save_data()` 触发；
// 唯一例外是 `init` 内部的 `persist_retry_state`，原因见其注释。
impl Data {
    /// 启动时初始化：执行历史数据迁移、扫描磁盘补齐本地索引、清理失效条目。
    ///
    /// 之所以集中在启动阶段做（而不是各页面按需懒加载）：这些操作同时依赖 `dir::*`
    /// 与实际磁盘内容，且结果必须尽早写回 `data.json`，才能保证进入任何界面前数据自洽；
    /// 若分散到页面里做，一次失败就会让界面看到半截的索引。
    ///
    /// 执行顺序是有意设计的：先迁移旧的收藏结构（否则后面的收藏逻辑会读到空列表），
    /// 再清理并扫描谱面（产生 `charts`），最后修正资源包下标与兼容字段、同步全局设置。
    ///
    /// # Errors
    /// 目录枚举或文件读写失败时返回错误，由 `the_main` 视作启动失败。
    pub async fn init(&mut self) -> Result<()> {
        // 直接以 `data.json` 为路径写盘，而**不能**用 `crate::save_data()`：
        // 此刻全局 `DATA` 还没安装（`set_data` 在 `init` 返回后才调用），
        // 且扫描过程中随时可能因崩溃中断，必须让重试计数尽早落盘。
        fn persist_retry_state(data: &Data) {
            let res = (|| -> Result<()> {
                let root = dir::root().with_context(|| "failed to get root directory")?;
                let path = format!("{}/data.json", root);
                std::fs::write(&path, serde_json::to_string(data)?).with_context(|| format!("failed to write to {}", path))?;
                Ok(())
            })();
            // 写盘失败只告警不中断：重试计数丢失最坏情况是下次多扫一遍，
            // 不应该因为磁盘问题让整个游戏起不来。
            if let Err(err) = res {
                warn!(?err, "failed to persist import scan retry state");
            }
        }

        // 丢弃一个已达重试上限、无法导入的条目：删文件（或目录）并清掉其计数。
        // 删除失败也继续（只告警），否则坏条目会永久卡住启动流程。
        fn remove_failed_entry(path: &Path, key: &str, retry_map: &mut HashMap<String, u8>) {
            let remove_res = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else if path.exists() {
                std::fs::remove_file(path)
            } else {
                Ok(())
            };
            if let Err(err) = remove_res {
                warn!(?err, "failed to remove exhausted import entry: {}", key);
            }
            retry_map.remove(key);
        }

        // 递增某个条目的重试计数，并以 [`MAX_IMPORT_RETRIES`] 为上限封顶。
        // 封顶而非持续累加，是为了避免计数无意义地增长、也让上限判断恒定成立。
        fn bump_retry(map: &mut HashMap<String, u8>, key: &str) {
            let entry = map.entry(key.to_owned()).or_default();
            *entry = (*entry + 1).min(MAX_IMPORT_RETRIES);
        }

        // 阶段 1：收藏夹从「内嵌在 data.json」迁移到「一个收藏夹一个文件」。
        // 这一步不能省：旧存档的 `collections_legacy` 若不迁移就只能丢弃，
        // 玩家的收藏会凭空消失；迁移后 `drain` 清空旧字段，下次保存即写回新结构。
        let collections = dir::collections()?;
        for col in self.collections_legacy.drain(..) {
            let uuid = Uuid::new_v4();
            self.collection_uuids.push(uuid);
            std::fs::write(format!("{collections}/{uuid}.json"), serde_json::to_string(&col)?)?;
        }
        // 迁移（或早先保存）的 uuid 列表可能已经失效（文件被外部删除/同步冲突），
        // 这里逐个预读并写入缓存，读不出来的直接踢出列表——保证 `collection_info`
        // 后续可以放心 `unwrap` 式访问，不必每处都处理「文件不存在」。
        self.collection_uuids.retain(|uuid| match Self::load_collection_info(uuid) {
            Ok(info) => {
                self.collection_cache.insert(*uuid, Arc::new(info));
                true
            }
            Err(err) => {
                warn!(?err, "failed to load collection info during migration, skipping: {uuid}");
                false
            }
        });

        // 阶段 2：谱面索引与磁盘对齐。先丢掉磁盘上已不存在的条目
        // （玩家在文件管理器里删了谱面，索引不应继续指向它）。
        let charts = dir::charts()?;
        self.charts.retain(|it| Path::new(&format!("{}/{}", charts, it.local_path)).exists());
        // `occurred` 记录索引里已存在的路径，用于判断扫描到的目录是「新导入」还是「已在索引中」，
        // 从而只对前者走导入逻辑。
        let occurred: HashSet<_> = self.charts.iter().map(|it| it.local_path.clone()).collect();
        // 扫描手动导入目录：目录名即 `custom/<uuid>`。
        for entry in std::fs::read_dir(dir::custom_charts()?)? {
            let entry = entry?;
            let filename = entry.file_name();
            let filename = filename.to_str().unwrap();
            let filename = format!("custom/{filename}");
            let path = entry.path();
            // 已在索引中：只要清掉可能残留的重试计数即可（说明它此前导入成功过）。
            if occurred.contains(&filename) {
                self.import_scan_retry.remove(&filename);
                continue;
            }
            // 已达到重试上限：认定该条目无法导入，删除并跳过，避免每次启动都重试。
            if self.import_scan_retry.get(&filename).copied().unwrap_or_default() >= MAX_IMPORT_RETRIES {
                remove_failed_entry(&path, &filename, &mut self.import_scan_retry);
                persist_retry_state(self);
                warn!("skip startup import scan after retry limit reached: {filename}");
                continue;
            }
            // Persist retry count before parsing so crashes during parsing still consume one retry.
            // 计数必须在解析**之前**落盘：解析本身可能让进程崩溃（这正是该机制存在的理由），
            // 若崩溃后才写入计数，则永远停留在 0，重试上限形同虚设。
            bump_retry(&mut self.import_scan_retry, &filename);
            persist_retry_state(self);
            let Ok(mut fs) = prpr::fs::fs_from_file(&path) else {
                continue;
            };
            let result = prpr::fs::load_info(fs.deref_mut()).await;
            match result {
                Ok(info) => {
                    // 导入成功：登记进索引，并清掉重试计数。
                    // `id: None` 表示这是本地导入、没有服务端 id。
                    self.import_scan_retry.remove(&filename);
                    self.charts.push(LocalChart {
                        info: BriefChartInfo { id: None, ..info.into() },
                        local_path: filename,
                        record: None,
                        mods: Mods::default(),
                        played_unlock: false,
                    });
                }
                Err(err) => {
                    // 失败时不删除目录（可能只是暂时读不到），留待下次启动重试，
                    // 直到计数达到上限才由上面的分支清理。
                    warn!(?err, "failed to parse startup custom import candidate: {}", filename);
                }
            }
        }
        // 扫描已下载目录：目录名即服务端谱面 id，因此**必须能解析为整数**，
        // 非数字目录（临时文件、同名残渣）直接跳过——它们不是谱面，也不该计入重试。
        for entry in std::fs::read_dir(dir::downloaded_charts()?)? {
            let entry = entry?;
            let filename = entry.file_name();
            let filename = filename.to_str().unwrap();
            let Ok(id): Result<i32, _> = filename.parse() else { continue };
            let filename = format!("download/{filename}");
            let path = entry.path();
            if occurred.contains(&filename) {
                self.import_scan_retry.remove(&filename);
                continue;
            }
            if self.import_scan_retry.get(&filename).copied().unwrap_or_default() >= MAX_IMPORT_RETRIES {
                remove_failed_entry(&path, &filename, &mut self.import_scan_retry);
                persist_retry_state(self);
                warn!("skip startup import scan after retry limit reached: {filename}");
                continue;
            }
            // Persist retry count before parsing so crashes during parsing still consume one retry.
            // 同样先落盘再解析，理由见手动导入分支。
            bump_retry(&mut self.import_scan_retry, &filename);
            persist_retry_state(self);
            let Ok(mut fs) = prpr::fs::fs_from_file(&path) else {
                warn!("failed to open file system for downloaded chart: {}", filename);
                continue;
            };
            let result = prpr::fs::load_info(fs.deref_mut()).await;
            match result {
                Ok(info) => {
                    // 与手动导入的区别：这里能确定服务端 id，故写入 `Some(id)`，
                    // 使该谱面之后可与云端数据关联（更新检查、上传成绩等）。
                    self.import_scan_retry.remove(&filename);
                    self.charts.push(LocalChart {
                        info: BriefChartInfo { id: Some(id), ..info.into() },
                        local_path: filename,
                        record: None,
                        mods: Mods::default(),
                        played_unlock: false,
                    });
                }
                Err(err) => {
                    warn!(?err, "failed to parse startup downloaded import candidate: {}", filename);
                }
            }
        }
        // 资源包：以磁盘为准补齐列表（玩家可直接把文件拷进 respack 目录），
        // 已记录过的文件名不重复添加，避免同一包出现多项。
        let respacks: HashSet<_> = self.respacks.iter().cloned().collect();
        for entry in std::fs::read_dir(dir::respacks()?)? {
            let entry = entry?;
            let filename = entry.file_name();
            let filename = filename.to_str().unwrap().to_string();
            if respacks.contains(&filename) {
                continue;
            }
            self.respacks.push(filename);
        }
        // 索引夹取：`respack_id == respacks.len()` 是「不使用资源包」的哨兵值，
        // 因此上限是 `len` 而不是 `len - 1`；删除过资源包后下标可能越界，必须收敛。
        self.respack_id = self.respack_id.min(self.respacks.len());
        // 兼容处理：旧存档里这里存的是绝对路径，跨设备后必然失效；
        // 重置为约定名 `chart.zip`（资源包内的固定文件名）后行为才可预期。
        if let Some(res_pack_path) = &mut self.config.res_pack_path {
            if res_pack_path.starts_with('/') {
                // for compatibility
                *res_pack_path = "chart.zip".to_owned();
            }
        }
        // 兼容处理：从「旧版的布尔同意标志」迁移到「版本号」体系。
        // 迁移时固定写入一个历史版本时间戳：既避免强迫老玩家重新同意，
        // 又保证后续协议真的更新时（时间戳不同）仍会正常弹窗。
        if self.read_tos_and_policy {
            debug!("migrating from old version");
            self.terms_modified = Some("Mon, 05 Aug 2024 17:32:41 GMT".to_owned());
            self.read_tos_and_policy = false;
        }
        // 保证至少存在一个「默认收藏夹」：收藏界面假定它恒存在（用于收纳未分类谱面），
        // 缺失时在此补建，并插入列表首位作为展示顺序的第一个。
        if !self.collection_cache.iter().any(|it| it.value().is_default) {
            let uuid = Uuid::new_v4();
            self.set_collection_info(
                &uuid,
                LocalCollection {
                    is_default: true,
                    ..LocalCollection::new(crate::ttl!("default-fav-folder").into_owned())
                },
            )?;
            self.collection_uuids.insert(0, uuid);
        }
        // 与谱面索引同理地清理成绩缓存：文件已删除则对应记录也没有意义，
        // 留着只会在 `data.json` 里无限膨胀。
        let charts = dir::charts()?;
        self.local_records
            .retain(|local_path, _| Path::new(&format!("{charts}/{local_path}")).exists());

        // 最后：让配置自身补全字段（新版本新增的设置项在这里取默认值），
        // 并把无障碍开关同步到引擎侧的全局原子量，使引擎无需反查 `Data`。
        self.config.init();
        PREFER_REDUCED_MOTION.store(self.prefer_reduced_motion, Ordering::Relaxed);
        Ok(())
    }

    /// 按 `local_path` 在本地索引中查找谱面，返回其在 `charts` 中的下标。
    ///
    /// 返回下标而非引用：调用方通常紧接着要 `get_data_mut().charts[idx]` 做修改，
    /// 借用下标可以避开同时持有共享/可变借用的借用检查问题。
    pub fn find_chart_by_path(&self, local_path: &str) -> Option<usize> {
        self.charts.iter().position(|local| local.local_path == local_path)
    }

    /// 收藏夹的 uuid 顺序列表（**顺序即展示顺序**，由 [`Data::move_collection`] 维护）。
    pub fn collection_uuids(&self) -> &[Uuid] {
        &self.collection_uuids
    }
    /// 按展示顺序迭代收藏夹实体（每个已是 `Arc`，可廉价克隆给界面持有）。
    pub fn collections(&self) -> impl Iterator<Item = Arc<LocalCollection>> + '_ {
        self.collection_uuids.iter().map(|uuid| self.collection_info(uuid))
    }
    /// 整体替换收藏顺序：云端同步后调用。
    ///
    /// 必须同时清空缓存——新列表里的 uuid 可能对应尚未下载到本地的收藏夹文件，
    /// 也可能包含内容已变化的项；留着旧缓存会返回过期数据。
    /// 注意只清缓存不删文件：文件由后续同步流程按需拉取或清理。
    pub fn set_collection_uuids(&mut self, uuids: Vec<Uuid>) {
        self.collection_uuids = uuids;
        self.collection_cache.clear();
    }
    /// 按下标取收藏夹实体。
    ///
    /// # Panics
    /// 下标越界时 panic——界面只能用 [`Data::collection_uuids`] 得到的合法下标来调用。
    pub fn collection_by_index(&self, index: usize) -> Arc<LocalCollection> {
        let uuid = &self.collection_uuids[index];
        self.collection_info(uuid)
    }

    /// 写入某个收藏夹的内容：先落盘再更新缓存，保证内存与磁盘不会不一致。
    ///
    /// 接收 `&self` 而非 `&mut self`：缓存是 `DashMap`（内部可变），
    /// 因此可在只有共享借用的场景下修改内容，便于界面在渲染期间顺手写回。
    ///
    /// # Errors
    /// 目录解析或文件写入失败时返回错误；此时缓存不会被更新，磁盘保持旧内容。
    pub fn set_collection_info(&self, uuid: &Uuid, info: LocalCollection) -> Result<()> {
        let path = Self::collection_info_path(uuid)?;
        std::fs::write(path, serde_json::to_string(&info)?)?;
        self.collection_cache.insert(*uuid, Arc::new(info));
        Ok(())
    }
    /// 新建一个收藏夹，追加到展示顺序末尾，返回其 uuid。
    ///
    /// # Errors
    /// 写盘失败时返回错误（此时不会把它加入顺序列表，避免出现指向空文件的条目）。
    pub fn push_collection(&mut self, info: LocalCollection) -> Result<Uuid> {
        let uuid = Uuid::new_v4();
        self.set_collection_info(&uuid, info)?;
        self.collection_uuids.push(uuid);
        Ok(uuid)
    }
    /// 删除指定下标的收藏夹：从顺序列表移除、删除磁盘文件、清掉缓存，返回其 uuid。
    ///
    /// 返回 uuid 是为了让调用方在需要时同步到云端（本地删除要能传播出去）。
    ///
    /// # Errors
    /// 路径解析或文件删除失败时返回错误。
    pub fn remove_collection(&mut self, index: usize) -> Result<Uuid> {
        let uuid = self.collection_uuids.remove(index);
        let path = Self::collection_info_path(&uuid)?;
        std::fs::remove_file(path)?;
        self.collection_cache.remove(&uuid);
        Ok(uuid)
    }
    /// 调整收藏夹顺序：把 `from` 处的项移动到 `to` 位置。
    ///
    /// 顺序是收藏夹唯一的「排序信息」，只存在于 `collection_uuids` 中（内容文件里不含顺序），
    /// 因此拖拽排序只需改这一个列表，无需触碰任何文件。
    pub fn move_collection(&mut self, from: usize, to: usize) {
        let uuid = self.collection_uuids.remove(from);
        self.collection_uuids.insert(to, uuid);
    }

    /// 收藏夹内容文件的路径：`data/collections/<uuid>.json`。
    fn collection_info_path(uuid: &Uuid) -> Result<String> {
        Ok(format!("{}/{}.json", dir::collections()?, uuid))
    }
    /// 从磁盘读取并解析收藏夹内容（不走缓存）。
    fn load_collection_info(uuid: &Uuid) -> Result<LocalCollection> {
        let path = Self::collection_info_path(uuid)?;
        let info: LocalCollection = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        Ok(info)
    }
    /// 取收藏夹内容：优先命中缓存，未命中则从磁盘加载并写入缓存。
    ///
    /// 用 `entry().or_insert_with()` 而非「先查再插」，是为了让并发/重入调用只加载一次文件。
    ///
    /// # Panics
    /// 文件缺失或解析失败时 panic。这是刻意的：`init` 已把不可加载的 uuid 从列表中剔除，
    /// 运行期再出现失败说明数据被外部破坏，属于必须暴露的异常状态。
    pub fn collection_info(&self, uuid: &Uuid) -> Arc<LocalCollection> {
        self.collection_cache
            .entry(*uuid)
            .or_insert_with(|| match Self::load_collection_info(uuid) {
                Ok(info) => Arc::new(info),
                Err(err) => {
                    panic!("failed to load collection info of {uuid}: {err:?}");
                }
            })
            .value()
            .clone()
    }
}
