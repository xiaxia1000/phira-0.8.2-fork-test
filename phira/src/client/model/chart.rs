//! 谱面（Chart）网络对象。
//!
//! 这是服务端 `/chart/{id}` 返回的资源：既包含展示用的元数据（曲名、谱师、插画），
//! 也包含三份文件引用（插画、试听、谱面本体）。它与内核使用的
//! [`BriefChartInfo`] 的分工是：本类型是**网络模型**（字段随服务端演进、含文件 URL），
//! `BriefChartInfo` 是**本地/内核模型**（可离线序列化、含本地解锁状态），
//! 二者通过 `to_info()` 单向转换。

use super::{File, Object, Ptr, User};
use crate::data::BriefChartInfo;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 一张谱面在服务端的完整表示。
///
/// 字段名按 `camelCase` 反序列化（对应服务端 JSON 的同名驼峰字段）。
/// 同时派生 `Serialize`：下载谱面后会被写进用户的本地谱面列表缓存，因此需要能回写。
///
/// 状态位（`ranked`/`reviewed`/`stable`/`stable_request`）是**服务端裁决的审核状态**，
/// 客户端只读不判：本地展示与可玩性判断都应以此为准，不要自行推导。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chart {
    /// 谱面唯一 id（主键，也是 `Ptr<Chart>` 与缓存键使用的值）。
    pub id: i32,
    /// 曲名。
    pub name: String,
    /// 难度**名称**字符串（如 `"IN"`/`"AT"`），由谱师填写，用于展示。
    pub level: String,
    /// 数值难度（定数，浮点，可含小数位），用于排序与 RKS 计算。
    pub difficulty: f32,
    /// 谱师（charter）署名。
    pub charter: String,
    /// 曲师（composer）署名。
    pub composer: String,
    /// 插画师（illustrator）署名。
    pub illustrator: String,
    /// 谱面简介；服务端允许为空，故为 `Option`。
    pub description: Option<String>,
    /// 是否已"上榜"（计入排行榜/RKS）。由审核流程决定。
    pub ranked: bool,
    /// 是否已通过审核（未审核谱面仅本人与有权限者可见）。
    pub reviewed: bool,
    /// 是否已稳定/上架。
    pub stable: bool,
    /// 是否处于"申请上架"流程中；与 `stable` 互斥地表达生命周期阶段。
    pub stable_request: bool,

    /// 插画文件（缩略图走 `File::load_thumbnail`，可省流量）。
    pub illustration: File,
    /// 试听音频文件。
    pub preview: File,
    /// 谱面本体文件（下载后解包到本地谱面目录即可游玩）。
    pub file: File,

    /// 上传者引用：只带 id，需要展示用户名/头像时再 `load`，避免列表页 N+1 请求。
    pub uploader: Ptr<User>,

    /// 创建时间（服务端时间戳，UTC）。
    pub created: DateTime<Utc>,
    /// 记录更新时间（元数据或状态的最近变更）。
    pub updated: DateTime<Utc>,
    /// 谱面内容（本体文件）最近更新时间，与 `updated` 分开：改名/改简介不改它。
    pub chart_updated: DateTime<Utc>,
    /// 标签列表；老数据可能缺该字段，故 `#[serde(default)]` 保证兼容。
    #[serde(default)]
    pub tags: Vec<String>,

    /// 服务端给出的评级/评分；未评级时为 `None`。
    pub rating: Option<f32>,
}
// 把谱面接入泛型对象机制：路径 `chart` 使其可被 `Client::load::<Chart>(id)` 加载，
// 并与其他对象共享同一套 LRU 缓存与惰性引用设施。
impl Object for Chart {
    /// 资源路径片段，最终请求 `GET /chart/{id}`，同时充当缓存表键。
    const QUERY_PATH: &'static str = "chart";

    /// 主键即 `id` 字段。
    fn id(&self) -> i32 {
        self.id
    }
}

// 网络模型 → 本地模型的转换。只做字段重命名与缺省填充，不做任何网络/磁盘访问。
impl Chart {
    /// 转换成内核与本地列表使用的 `BriefChartInfo`。
    ///
    /// 几个有意的取舍：
    /// - `description` 为 `None` 时填**空串**（本地模型没有 `Option`，用空串表达"无简介"）；
    /// - 三个时间戳都用 `Some(..)` 原样带过去，本地可据此判断是否需要封面/缓存更新；
    /// - `has_unlock` 恒为 `false`——解锁状态是**纯本地**信息，由存档单独维护，
    ///   网络对象无从得知，因此这里直接给默认值，调用方需自行与本地存档合并。
    ///
    /// # Returns
    /// 可直接写入本地谱面列表（`Data::charts`）的信息结构。
    pub fn to_info(&self) -> BriefChartInfo {
        BriefChartInfo {
            id: Some(self.id),
            uploader: Some(self.uploader.clone()),
            name: self.name.clone(),
            level: self.level.clone(),
            difficulty: self.difficulty,
            intro: self.description.clone().unwrap_or_default(),
            charter: self.charter.clone(),
            composer: self.composer.clone(),
            illustrator: self.illustrator.clone(),
            created: Some(self.created),
            updated: Some(self.updated),
            chart_updated: Some(self.chart_updated),
            has_unlock: false,
        }
    }
}
