//! 收藏夹（Collection）的云端模型与本地模型。
//!
//! 这里有两套平行的表示，理解它们的差异是读懂本文件的前提：
//! - [`Collection`]：**服务端**的对象，谱面以完整的 `Chart` 内联返回，字段只读；
//! - [`LocalCollection`]：**本地**的表示，谱面以 [`ChartRef`]（路径 + 可选缓存信息）
//!   表示，可以引用本地谱面、可以离线修改，并按 UUID 存进 `Data::collections`。
//!
//! 二者通过 `LocalCollection::merge`（云端覆盖本地）与 `LocalCollection::update`
//! （本地改动 + 可选同步到云端）互通。`Data` 中另有一张 `collection_uuids` 映射表
//! 负责把本地 UUID 与服务端数字 id 对应起来，因此本地对象用 `Uuid` 定位、云端对象用
//! `i32` 定位，混用会导致找不到收藏夹。

use std::{
    borrow::Cow,
    collections::HashSet,
    fmt::Debug,
    hash::{Hash, Hasher},
};

use crate::{
    client::{recv_raw, Client, File},
    data::BriefChartInfo,
    dir, get_data,
    page::{local_illustration, Illustration},
};

use super::{Chart, Object, Ptr, User};
use anyhow::Result;
use chrono::{DateTime, Utc};
use prpr::{ext::BLACK_TEXTURE, info::ChartInfo, task::Task, ui::Dialog};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 云端收藏夹（`GET /collection/{id}` 的响应）。
///
/// 与本地模型的区别：谱面是**内联的完整 `Chart`**（服务端一次给全，避免客户端
/// 逐条拉取）；`cover` 可以直接为 `None` 表示未设置封面。
/// 只实现 `Deserialize`——云端对象由服务端权威维护，客户端改动用 PATCH（见
/// [`CollectionPatch`]），不需要整体回传。`#[allow(dead_code)]` 表明部分字段
/// （如 `created`）当前界面未使用。
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Collection {
    /// 收藏夹 id（主键；本地通过 `Data::collection_uuids` 与 UUID 对应）。
    pub id: i32,
    /// 封面文件；`None` 表示未设置（界面需回退到第一张谱面的插画，见 `LocalCollection::cover`）。
    pub cover: Option<File>,
    /// 创建者引用（惰性）。`LocalCollection::is_owned` 依赖它与当前用户比对。
    pub owner: Ptr<User>,
    /// 收藏夹名称。
    pub name: String,
    /// 描述文字（服务端约定为空串表示无描述，而非 `None`）。
    pub description: String,
    /// 创建时间，UTC。
    pub created: DateTime<Utc>,
    /// 最近更新时间；本地用它与 `LocalCollection::remote_updated` 比较来判断是否需合并。
    pub updated: DateTime<Utc>,
    /// 内联的完整谱面列表（云端权威顺序）。
    pub charts: Vec<Chart>,
    /// 是否公开可见。
    pub public: bool,
}
// 接入泛型对象机制：`QUERY_PATH = "collection"` → `GET /collection/{id}`，
// 并拥有独立的 LRU 缓存表。
impl Object for Collection {
    /// 资源路径片段，同时作为缓存表键。
    const QUERY_PATH: &'static str = "collection";

    /// 主键即 `id` 字段。
    fn id(&self) -> i32 {
        self.id
    }
}

/// 谱面引用所携带的**已解析信息**：本地缓存的信息 + 插画文件。
///
/// 之所以存在这一层：`ChartRef` 只是一条路径（可能指向本地、也可能指向云端），
/// 展示列表需要曲名/难度/插画时若每次都去请求网络，就无法离线工作。于是把
/// `BriefChartInfo`（可离线序列化）与插画 URL 一起缓存进收藏夹文件里，作为显示用的
/// 快照；它**可能与云端最新值不同步**，权威数据仍以 `Chart` 为准。
#[derive(Clone, Serialize, Deserialize)]
pub struct ChartRefChartInfo {
    /// 本地谱面信息；`flatten` 使它与 `illustration` 平铺在同一个 JSON 对象里，
    /// 从而复用 `BriefChartInfo` 自身的字段布局（无需嵌套一层）。
    #[serde(flatten)]
    pub info: BriefChartInfo,
    /// 插画文件引用，用于离线展示封面。
    pub illustration: File,
}

// 从网络对象构造本地缓存信息：只做字段拷贝，不访问网络或磁盘。
impl ChartRefChartInfo {
    /// 由云端 `Chart` 生成本地缓存信息。
    ///
    /// # Arguments
    /// - `chart`：来自服务端的完整谱面对象。
    pub fn from_chart(chart: &Chart) -> Self {
        Self {
            info: BriefChartInfo::from_chart(chart),
            illustration: chart.illustration.clone(),
        }
    }
}

/// 本地收藏夹中的一条谱面引用。
///
/// 核心是 `path` 这一个字符串，它用一个约定同时编码了两种来源：
/// - 以 `download/` 开头 → 指向**云端/已下载**的谱面，前缀后的数字是谱面 id；
/// - 其它 → 指向**本地文件系统**里的相对路径（用户导入的谱面）。
///
/// 约定用字符串而非枚举，是为了让本地收藏夹文件格式简单且向后兼容（见手写的
/// `Deserialize`）。构造时应优先用 `new_bare`/`From<Chart>`，而不是手工拼字符串。
#[derive(Clone, Serialize)]
pub struct ChartRef {
    /// 谱面路径或 `download/{id}`，见类型注释。
    pub path: String,
    /// 可选的已解析信息（展示用快照）。`Box` 是为了让 `ChartRef` 本身足够小——
    /// 它会被大量存放在 `Vec` 与 `HashSet` 中并频繁哈希/克隆。
    pub info: Option<Box<ChartRefChartInfo>>,
}

// 手写 Debug：只打印 `path`。`info` 内容较长且属于缓存，打印出来会淹没日志。
impl Debug for ChartRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChartRef").field("path", &self.path).finish()
    }
}

// 手写反序列化以兼容**三个历史版本的存储格式**（旧版本本地收藏夹文件仍可能存在于
// 用户磁盘上，必须能读）：
// 1. `New`：对象 `{ path, info }`——当前格式；
// 2. `Local`：裸字符串——最早期格式，只存本地路径；
// 3. `Online`：二元组 `[id, info]`——中期格式，用数组承载"在线 id + 信息"。
// untagged 按声明顺序尝试匹配（New 是 map、Local 是 string、Online 是 array，互不冲突），
// 因此顺序在这里也隐式表达了优先级。
impl<'de> Deserialize<'de> for ChartRef {
    /// 依次尝试三种格式，全部不匹配则返回 serde 错误。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // 仅为本次反序列化服务的"融合"枚举，不对外暴露。
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum FuseChartRef {
            // 当前格式：显式路径 + 可选信息。
            New { path: String, info: Option<Box<ChartRefChartInfo>> },
            // 旧格式一：裸本地路径。注意旧值若恰好以 `download/` 开头，会被 `id()`
            // 误判为在线谱面，这是该兼容方案的固有歧义。
            Local(String),
            // 旧格式二：`[id, info]`，id 为在线谱面 id。
            Online(i32, Option<Box<ChartRefChartInfo>>),
        }

        let fuse = FuseChartRef::deserialize(deserializer)?;
        // 统一归一化为 `path` 字符串表示，旧格式在这里被"升级"到新约定。
        Ok(match fuse {
            FuseChartRef::New { path, info } => Self { path, info },
            FuseChartRef::Local(local_path) => Self {
                path: local_path,
                info: None,
            },
            FuseChartRef::Online(id, info) => Self {
                path: format!("download/{id}"),
                info,
            },
        })
    }
}

// 谱面引用的构造与查询。这里只做"路径 ↔ id / 本地文件"的换算，不访问网络。
impl ChartRef {
    /// 用"在线 id"或"本地路径"二者之一构造一个**无缓存信息**的引用。
    ///
    /// `info` 留空意味着这条引用暂时无法离线展示曲名（界面需要先解析一次）。
    ///
    /// # Panics
    /// 两个参数都是 `None` 时 panic：引用必须至少有一个来源，否则是无意义的路径。
    pub fn new_bare(id: Option<i32>, local_path: Option<&str>) -> Self {
        let path = if let Some(id) = id {
            format!("download/{id}")
        } else if let Some(local) = local_path {
            local.to_string()
        } else {
            panic!("chart ref must have either id or local path");
        };
        Self { path, info: None }
    }

    /// 判断引用的谱面文件是否已存在于本地谱面目录。
    ///
    /// 注意这是"路径存在性"判断，**不代表谱面可用**（例如文件损坏或缺少 info.yml）。
    ///
    /// # Panics
    /// 内部对 `dir::charts()` 与 `std::fs::exists` 都做了 `unwrap()`——获取谱面目录
    /// 失败或文件系统报错（而非"不存在"）时会 panic，调用方应在启动期已确保目录可用。
    pub fn exists(&self) -> bool {
        std::fs::exists(format!("{}/{}", dir::charts().unwrap(), self.path)).unwrap()
    }

    /// 找出该引用对应的**本地谱面目录名**（相对谱面根目录）。
    ///
    /// 两级查找策略（先便宜后昂贵）：
    /// 1. 若 `path` 本身就是一个存在的本地路径，直接借用返回（无需扫描）；
    /// 2. 否则若是在线引用（能解析出 id），则在线性扫描 `Data::charts` 找同 id 的本地条目，
    ///    返回其 `local_path`（需要拷贝，故为 `Cow::Owned`）。
    ///
    /// 第 2 步是 O(n) 扫描且带 `TODO: optimize` 标注——收藏夹里条目多时可能成为热点，
    /// 但目前只在加载封面/点击时调用，尚未优化。
    ///
    /// # Returns
    /// `Some(相对路径)` 表示本地已有该谱面；`None` 表示尚未下载/导入。
    ///
    /// # Errors
    /// 获取谱面目录失败或文件系统报错时返回错误。
    pub fn find_local_path<'a>(&'a self) -> Result<Option<Cow<'a, str>>> {
        let charts = dir::charts()?;
        if std::fs::exists(format!("{charts}/{}", self.path))? {
            return Ok(Some(Cow::Borrowed(&self.path)));
        }
        let Some(id) = self.id() else {
            return Ok(None);
        };
        // TODO: optimize
        Ok(get_data().charts.iter().find_map(|it| {
            if it.info.id == Some(id) {
                Some(Cow::Owned(it.local_path.clone()))
            } else {
                None
            }
        }))
    }

    /// 若是在线引用则解析出谱面 id。
    ///
    /// # Returns
    /// 本地引用返回 `None`；`download/` 后的内容能解析成整数时返回 `Some(id)`。
    pub fn id(&self) -> Option<i32> {
        self.path.strip_prefix("download/").and_then(|s| s.parse().ok())
    }
    /// 判断是否为在线（云端）引用——即路径以 `download/` 开头。
    ///
    /// 收藏夹同步前会用它校验"云端收藏夹只能包含在线谱面"这一约束。
    pub fn is_online(&self) -> bool {
        self.path.starts_with("download/")
    }
}

// 从云端谱面对象转换：顺带把展示信息缓存进 `info`，这样加入收藏夹后即可离线展示。
impl From<Chart> for ChartRef {
    /// 生成在线引用 `download/{id}`，并用 `Chart` 填充 `ChartRefChartInfo`。
    fn from(chart: Chart) -> Self {
        ChartRef {
            path: format!("download/{}", chart.id),
            info: Some(Box::new(ChartRefChartInfo::from_chart(&chart))),
        }
    }
}

// 相等/哈希**只看路径**，忽略 `info`：这样"同一条引用在不同时间被解析出不同缓存信息"
// 仍被视为同一项，去重（`HashSet<ChartRef>`）与集合运算才符合预期。
impl PartialEq for ChartRef {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}
impl Eq for ChartRef {}

impl Hash for ChartRef {
    /// 与 `PartialEq` 保持一致，仅哈希 `path`。
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

/// 收藏夹封面的三种来源。
///
/// 之所以做成枚举而不是单个 URL：封面可以是"用户手动设置的在线图片"、"某张本地谱面的
/// 插画"，或者"还没设置"。后两者无法用 URL 表达（本地插画要通过
/// `local_illustration` 从磁盘加载），因此必须区分来源。
/// 该枚举会随本地收藏夹一起持久化，**变体顺序/名称影响存档格式**，重命名会导致旧存档
/// 反序列化失败。
#[derive(Clone, Serialize, Deserialize)]
pub enum CollectionCover {
    /// 未设置封面——展示时回退到第一张谱面的插画或黑色占位图。
    Unset,
    /// 在线图片文件（由服务端托管）。
    Online(File),
    /// 某张**本地谱面**的目录名，插画从本地资源里取。
    LocalChart(String),
}

/// 一次收藏夹修改的结果，供 UI 决定后续动作。
///
/// `Updated` 里携带一个 `Task`（可轮询的异步任务）而不是直接 await，是因为调用方多半
/// 在同步的 UI 回调里，需要把"同步到云端"这个副作用延后到主循环中处理。
pub enum CollectionUpdate {
    /// 无需改动：可能是操作无效（如删除不存在的条目），或已被前置校验拦下（如云端收藏夹
    /// 不允许放本地谱面）。调用方不应弹"成功"提示。
    Unchanged,
    /// 有改动。本地一定已落盘；云端同步（若有）挂在 `sync_task` 上。
    Updated {
        /// 云端同步任务：完成为服务端返回的最新 `Collection` 与本次是否为"添加"。
        /// 为 `None` 表示**无需/不应上传**（未同步到云端，或处于离线模式）。
        sync_task: Option<Task<Result<(Collection, bool)>>>,
        /// 本次是添加（`true`）还是移除（`false`），供 UI 做动画或提示文案区分。
        add: bool,
    },
}

/// 本地收藏夹：可离线编辑、可（在联网时）与云端同步的收藏夹表示。
///
/// 与 [`Collection`] 的关键差异：谱面用 [`ChartRef`] 引用（因此可以包含本地谱面），
/// 并且多了"尚未同步到云端"这一状态（`id == None`）。它以 UUID 为键存放在本地
/// `Data` 中，服务端数字 id 通过 `Data::collection_uuids` 关联。
#[derive(Clone, Serialize, Deserialize)]
pub struct LocalCollection {
    /// 服务端 id；`None` 表示这是**纯本地**收藏夹（尚未创建到云端）。
    pub id: Option<i32>,
    /// 拥有者引用；纯本地收藏夹为 `None`（视为本人所有，见 `is_owned`）。
    pub owner: Option<Ptr<User>>,
    /// 封面来源（见 [`CollectionCover`]）。
    pub cover: CollectionCover,
    /// 名称（本地可随意修改，同步时整体上传）。
    pub name: String,
    /// 描述文字（用空串表示"无描述"，与云端 `Collection::description` 一致）。
    pub description: String,
    /// 上次合并时云端 `updated` 的值，用于判断远端是否已被他人改动（需重新合并）。
    pub remote_updated: Option<DateTime<Utc>>,
    /// 谱面引用列表（顺序即展示顺序）。
    pub charts: Vec<ChartRef>,
    /// 是否公开；`#[serde(default)]` 兼容早期没有该字段的存档（缺省为私有）。
    #[serde(default)]
    pub public: bool,
    /// 是否为默认收藏夹（如"我的最爱"）：不可删除，UI 需据此隐藏删除入口。
    pub is_default: bool,
}
// 本地收藏夹的构造、封面推导、归属判断，以及与云端的双向同步。
impl LocalCollection {
    /// 创建一个空的纯本地收藏夹：无 id、无拥有者、封面未设置、私有、非默认。
    ///
    /// # Arguments
    /// - `name`：收藏夹名称（由用户输入，此处不做校验）。
    pub fn new(name: String) -> Self {
        Self {
            id: None,
            owner: None,
            cover: CollectionCover::Unset,
            name,
            description: String::new(),
            remote_updated: None,
            charts: Vec::new(),
            public: false,
            is_default: false,
        }
    }

    /// 计算用于展示的封面插画。
    ///
    /// 回退链（`Unset` 时逐级降级）：
    /// 1. 未设置封面 → 用第一张谱面：若该谱面**本地存在**则用其本地插画；
    /// 2. 否则若引用里缓存了在线插画信息 → 用在线插画（缩略图）；
    /// 3. 都不行 → 黑色占位纹理。
    ///
    /// 因此本函数可能触发一次文件读取（本地插画）或一次网络请求（在线缩略图），
    /// 不应在每帧调用；返回的 `Illustration` 自身带加载状态缓存。
    pub fn cover(&self) -> Illustration {
        let mut cover = self.cover.clone();
        if matches!(cover, CollectionCover::Unset) {
            if let Some(chart) = self.charts.first() {
                if let Ok(Some(local_path)) = chart.find_local_path() {
                    cover = CollectionCover::LocalChart(local_path.into_owned());
                } else if let Some(info) = &chart.info {
                    cover = CollectionCover::Online(info.illustration.clone());
                }
            }
        }
        match cover {
            CollectionCover::Unset => Illustration::from_done(BLACK_TEXTURE.clone()),
            CollectionCover::Online(file) => Illustration::from_file_thumbnail(file),
            CollectionCover::LocalChart(path) => local_illustration(path, BLACK_TEXTURE.clone(), false),
        }
    }

    /// 判断当前用户是否有权编辑该收藏夹。
    ///
    /// 规则：纯本地收藏夹（`id == None`）恒为本人所有；已同步的则比较拥有者与当前
    /// 登录用户 id。若尚未登录（`Data::me` 为 `None`），已同步的收藏夹会被判为**非本人**，
    /// 因此未登录时界面应统一走只读展示。
    pub fn is_owned(&self) -> bool {
        self.id.is_none()
            || self
                .owner
                .as_ref()
                .is_some_and(|it| get_data().me.as_ref().is_some_and(|me| me.id == it.id))
    }

    /// 用云端数据覆盖本地字段，生成合并后的收藏夹。
    ///
    /// 用于"拉取远端收藏夹后刷新本地"。注意两点：
    /// - `is_default` **保留本地值**——它是纯本地概念，云端不感知；
    /// - 谱面整体替换为云端列表（并借 `From<Chart>` 填充展示信息），因此本地独有的
    ///   未同步改动会被丢弃，调用前应确认没有待上传的改动。
    ///
    /// # Panics
    /// 断言 `self.id == Some(col.id)`：该方法只在"已知本地条目对应这个云端 id"时使用，
    /// 若 id 不匹配说明调用时机有误（例如把云端 A 的数据合并进了本地 B）。
    pub fn merge(&self, col: &Collection) -> Self {
        assert_eq!(self.id, Some(col.id));
        Self {
            id: Some(col.id),
            owner: Some(col.owner.clone()),
            cover: match &col.cover {
                None => CollectionCover::Unset,
                Some(file) => CollectionCover::Online(file.clone()),
            },
            name: col.name.clone(),
            description: col.description.clone(),
            remote_updated: Some(col.updated),
            // 云端谱面 → 本地引用，同时缓存展示信息。
            charts: col.charts.iter().cloned().map(Into::into).collect(),
            public: col.public,
            is_default: self.is_default,
        }
    }

    /// 添加/移除若干谱面，落盘本地改动，并（在需要时）构造云端同步任务。
    ///
    /// 返回 [`CollectionUpdate`]，`#[must_use]` 提醒调用方必须处理同步任务，否则改动
    /// 只留在本地。
    ///
    /// # Arguments
    /// - `uuid`：该本地收藏夹的 UUID（本地存储键）；
    /// - `charts`：本次要添加或移除的谱面引用；
    /// - `add`：`true` 为添加，`false` 为移除。
    ///
    /// # Returns
    /// `Unchanged`（无需改动/被校验拦下）或 `Updated`（附可选的云端同步任务）。
    #[must_use]
    pub fn update(mut self, uuid: Uuid, charts: &[ChartRef], add: bool) -> CollectionUpdate {
        let data = get_data();
        // 阶段一：前置校验——已同步到云端的收藏夹**只能包含在线谱面**。
        // 本地谱面无法通过服务端 id 表达，因此拒绝操作并弹出提示（把涉及的曲名列出来，
        // 读不到 info.yml 的条目会被静默跳过，不会导致整个操作失败）。
        if self.id.is_some() && charts.iter().any(|it| !it.is_online()) {
            let dir = dir::charts().unwrap();
            let charts: Vec<_> = charts
                .iter()
                .filter(|it| !it.is_online())
                .filter_map(|it| {
                    let path = format!("{dir}/{}/info.yml", it.path);
                    let info = std::fs::read_to_string(path).ok()?;
                    serde_yaml::from_str::<ChartInfo>(&info).ok().map(|info| info.name)
                })
                .collect();
            Dialog::simple(ttl!("favorites-online-only", "charts" => charts.join(", "))).show();
            return CollectionUpdate::Unchanged;
        }

        // 阶段二：判断是否需要上传。纯本地收藏夹或离线模式下只改本地。
        let should_upload = self.id.is_some() && !get_data().config.offline_mode;
        let mut updated = false;
        if add {
            // 添加：用 `HashSet` 做 O(1) 去重，已存在的路径不再重复加入。
            let local_paths: HashSet<String> = self.charts.iter().map(|it| it.path.clone()).collect();
            for chart in charts {
                if !local_paths.contains(&chart.path) {
                    self.charts.push(chart.clone());
                    updated = true;
                }
            }
        } else {
            // 移除：先把待删项收进集合，再一次性 retain（比逐个 remove 更高效）。
            let to_remove: HashSet<ChartRef> = charts.iter().cloned().collect();
            self.charts.retain(|it| {
                if to_remove.contains(it) {
                    updated = true;
                    false
                } else {
                    true
                }
            });
        }
        // 阶段三：没有实际变化就直接返回，避免无谓的磁盘写入与网络请求。
        if !updated {
            return CollectionUpdate::Unchanged;
        }

        // 阶段四：先把本地改动落盘（即使随后上传失败，本地也已生效）。
        let id = self.id;
        let col_ids = self.charts.iter().filter_map(|it| it.id()).collect::<Vec<_>>();
        data.set_collection_info(&uuid, self).unwrap();
        if !should_upload {
            return CollectionUpdate::Updated { sync_task: None, add };
        }

        // 阶段五：构造 PATCH 任务，把完整的谱面 id 列表设为云端内容；服务端返回
        // 合并后的最新 `Collection`，供调用方覆盖本地以保持一致。
        CollectionUpdate::Updated {
            sync_task: Some(Task::new(async move {
                let resp: Collection =
                    recv_raw(Client::request(Method::PATCH, format!("/collection/{}", id.unwrap())).json(&CollectionPatch::Set(col_ids)))
                        .await?
                        .json()
                        .await?;
                Ok((resp, add))
            })),
            add,
        }
    }
}

/// 收藏夹的**增量**修改请求体（`PATCH /collection/{id}`）。
///
/// 用枚举而非"带可选字段的结构体"，是因为服务端按变体语义区分操作类型；
/// `camelCase` 重命名后变体名成为 JSON 里的判别字段（如 `set`/`public`/`cover`），
/// 因此**变体名属于协议的一部分，不可改名**。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CollectionPatch {
    /// 整体设置谱面列表（当前 id 集合的全量覆盖，不是差集）。
    Set(Vec<i32>),
    /// 修改公开性。
    Public(bool),
    /// 修改封面为指定谱面的插画（传谱面 id，由服务端解析出图片地址）。
    Cover(i32),
}

/// 收藏夹**全量**内容请求体（创建/整体更新时使用）。
///
/// 与 [`CollectionPatch`] 的区别：这是"整份内容"语义（含名称与描述），
/// 且谱面以 id 列表给出；字段名保持 snake_case，服务端按原名解析。
#[derive(Serialize)]
pub struct CollectionContent {
    /// 名称。
    pub name: String,
    /// 描述（空串表示无）。
    pub description: String,
    /// 谱面 id 列表（顺序即展示顺序）。
    pub charts: Vec<i32>,
    /// 是否公开。
    pub public: bool,
}
