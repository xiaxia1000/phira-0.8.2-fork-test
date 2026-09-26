//! 活动（Event）网络对象：服务端 `/event/{id}` 返回的限时活动条目。

use super::{File, Object, Ptr, User};
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// 一个限时活动。
///
/// 只实现 `Deserialize`（活动由服务端定义、客户端只读展示，不回写），
/// `#[allow(dead_code)]` 表示部分字段目前仅供未启用的界面使用。
/// 活动的**可见性不由起止时间单独决定**：未开始或已结束的活动是否展示仍需服务端
/// 授权（相关权限见 `Permissions::SEE_ALL_EVENTS`），因此调用方不应仅凭时间过滤。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Event {
    /// 活动 id（主键）。
    pub id: i32,
    /// 创建者引用；惰性解析，需要展示时再 `load`。
    pub creator: Ptr<User>,
    /// 活动名称。
    pub name: String,
    /// 活动宣传图文件。
    pub illustration: File,
    /// 开始时间；之前活动不应展示（仍需服务端权限确认）。
    pub time_start: DateTime<Utc>,
    /// 结束时间。
    pub time_end: DateTime<Utc>,
}
// 接入泛型对象机制：可被 `Client::load::<Event>(id)` 加载，请求路径为 `GET /event/{id}`。
impl Object for Event {
    /// 资源路径片段，同时作为缓存表键。
    const QUERY_PATH: &'static str = "event";

    /// 主键即 `id` 字段。
    fn id(&self) -> i32 {
        self.id
    }
}
