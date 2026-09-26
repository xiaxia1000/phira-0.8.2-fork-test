//! 成绩记录（Record）网络对象。
//!
//! 注意与 `Client::best_record` 返回的 `prpr::scene::SimpleRecord` 区分：本类型是
//! **服务端的完整记录**（含玩家/谱面引用、判定细节、mod 与标准化分），用于成绩列表
//! 与详情；`SimpleRecord` 是内核侧的轻量结构，只承载展示所需的最小集合。
//! 字段名与服务端 JSON 逐字对应（未启用 `rename_all`）。

use super::{Chart, Object, Ptr, User};
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// 一条完整成绩记录。
///
/// `player`/`chart` 都是惰性引用，因此一页成绩列表只带来一次请求（不会为每行
/// 额外拉取玩家与谱面信息）。记录一经提交即不可变，故只实现 `Deserialize`。
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct Record {
    /// 记录 id（主键，同时是缓存键）。
    pub id: i32,
    /// 玩家引用（惰性）。
    pub player: Ptr<User>,
    /// 谱面引用（惰性）。
    pub chart: Ptr<Chart>,
    /// 总分（整数，按 Phira 计分规则，越大越好）。
    pub score: i32,
    /// 准确率（0~1 的小数或百分数，具体刻度由服务端统一）。
    pub accuracy: f32,
    /// Perfect 判定数。
    pub perfect: i32,
    /// Good 判定数。
    pub good: i32,
    /// Bad 判定数。
    pub bad: i32,
    /// Miss 判定数。
    pub miss: i32,
    /// 游玩时的谱面速度倍率（玩家设置），影响成绩可比性。
    pub speed: f32,
    /// 最大连击数。
    pub max_combo: i32,
    /// 是否全连（整曲无断连）。
    pub full_combo: bool,
    /// 是否为该玩家在该谱面上的最佳成绩——同一谱面多次游玩会保留多条记录，
    /// 该布尔用于快速筛出最优的一条，避免客户端自行比较。
    pub best: bool,
    /// 修饰（mod）位掩码；**具体位含义由服务端定义**，客户端只做透传与展示。
    pub mods: i32,
    /// 记录产生（提交）时间，UTC。
    pub time: DateTime<Utc>,
    /// 标准化后的难度值；服务端仅对新版记录提供，旧记录/未计算时为 `None`，
    /// 因此**不能**假设它一定存在（老成绩页需回退到 `chart.difficulty`）。
    pub std: Option<f32>,
    /// 标准化后的分数，与 `std` 配套用于跨难度比较；可能为 `None`（见 `std`）。
    pub std_score: Option<f32>,
}
// 接入泛型对象机制。注意 `QUERY_PATH` 用的是复数 `records`，与其它对象的单数路径
// 不同：它必须与服务端路由逐字一致，同时作为缓存表键（键不能与其他类型重复）。
impl Object for Record {
    /// 资源路径片段（复数形式），最终请求 `GET /records/{id}`。
    const QUERY_PATH: &'static str = "records";

    /// 主键即 `id` 字段。
    fn id(&self) -> i32 {
        self.id
    }
}
