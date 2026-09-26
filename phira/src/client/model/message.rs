//! 站内消息（Message）网络对象：公告/系统通知及其附带的操作按钮。
//!
//! 这两个类型**不实现 `Object`**——消息是按列表批量拉取（`Client::query`）后由界面
//! 直接消费的，没有"按 id 单独加载"的需求，因此不进对象缓存。字段名与服务端
//! JSON 逐字对应（未启用 `rename_all`，即服务端本身就下发小写/下划线形式）。

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// 消息里附带的一个操作按钮。
///
/// `name` 是给用户看的按钮文案，`action` 是**机器可读的动作标识**（由客户端解释，
/// 通常是跳转链接或内置动作关键字）。二者分离是为了让文案可随语言变化而动作不变。
/// 服务端下发什么动作，客户端就执行什么，因此新增动作需要客户端同步发版。
#[derive(Deserialize)]
pub struct MessageAction {
    /// 按钮展示文案。
    pub name: String,
    /// 动作标识，由客户端负责解释执行。
    pub action: String,
}

/// 一条站内消息（公告 / 系统通知 / 补偿说明等）。
///
/// `content` 为纯文本正文（非富文本）。`time` 是发布时间，用于排序与"未读"判断。
/// `actions` 允许缺失（`#[serde(default)]`）：绝大多数消息没有按钮，服务端不会下发
/// 空数组，缺省即"无操作"。
#[derive(Deserialize)]
#[allow(dead_code)]
pub struct Message {
    /// 消息 id（主键；用于去重与标记已读）。
    pub id: i32,
    /// 标题。
    pub title: String,
    /// 正文（纯文本）。
    pub content: String,
    /// 作者署名——注意它是**纯字符串而非用户引用**，因为多为"官方/运营"这类非账号主体。
    pub author: String,
    /// 发布时间（UTC）。
    pub time: DateTime<Utc>,
    /// 附带的操作按钮；缺省为空（老消息或纯通知类消息不带该字段）。
    #[serde(default)]
    pub actions: Vec<MessageAction>,
}
