//! Time consuming task management.
//!
//! 提供把异步任务接入“每帧轮询”式游戏主循环的最小封装，
//! 注意它本身不起运行时的作用：任务仍由 tokio 驱动，本模块只解决
//! “异步结果如何被同步的游戏循环安全地观察到”这一问题。
//! 因此调用方必须处于 tokio 上下文中（`Task::new` 会调用 `tokio::spawn`）。

use std::{
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
};

/// 一次性异步任务的轮询句柄。
///
/// 设计意图：游戏主循环是同步且不可阻塞的，而谱面下载、解压、资源解码等
/// 工作必须异步完成。任务在后台由 tokio 运行，结果写入共享的 `Mutex<Option<T>>`；
/// 主线程每帧调用 [`Task::ok`] 即可知道是否完成。`Option` 同时承担
/// “是否完成”的标志与结果存储两个职责，避免额外状态位。
pub struct Task<T: Send + 'static>(Arc<Mutex<Option<T>>>);

// 克隆只共享同一份完成状态（由 Arc 决定），所有副本观察到相同结果，
// 因此可以把句柄同时交给 UI 与逻辑模块而不需要额外同步。
impl<T: Send + 'static> Clone for Task<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

// 任务的创建与结果读取。
impl<T: Send + 'static> Task<T> {
    /// 后台启动 `future` 并返回可轮询的句柄。
    ///
    /// `T: Send` 是硬性要求：结果需要从 tokio 工作线程搬回游戏主线程。
    /// 该函数依赖 tokio 运行时，必须在 tokio 上下文中调用。
    pub fn new(future: impl Future<Output = T> + Send + 'static) -> Self {
        let arc = Arc::new(Mutex::new(None));
        {
            // 在嵌套作用域内 clone，让 spawn 之后外层仍握有原始 Arc 用于构造 Self。
            let arc = Arc::clone(&arc);
            tokio::spawn(async move {
                let result = future.await;
                // 写入结果即完成了 `None -> Some` 的状态跃迁，也就是完成的信号。
                *arc.lock().unwrap() = Some(result);
            });
        }
        Self(arc)
    }

    /// 永不完成的任务，用作占位句柄。
    ///
    /// 典型场景：某个异步流程失败后仍需让上层保持在 loading 状态，
    /// 或需要类型为 `Task<T>` 的“空”值来填充结构体字段。
    pub fn pending() -> Self {
        Self::new(std::future::pending())
    }

    /// 结果是否已就绪。非破坏性检查，可安全地每帧调用。
    pub fn ok(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }

    /// 取走结果。
    ///
    /// 取走后 [`Task::ok`] 会重新变为 `false`，即结果只能被消费一次，
    /// 适合“完成即切换场景 / 跳转”的一次性流程。
    pub fn take(&mut self) -> Option<T> {
        self.0.lock().unwrap().take()
    }

    /// 直接借用结果槽。
    ///
    /// 供需要在持锁期间做较复杂处理（或需要区分“未完成”与“已完成但结果为 None”语义）
    /// 的调用方使用。持锁会阻塞后台任务写入，因此应及时释放，不要跨 `await` 持有。
    pub fn get(&self) -> MutexGuard<'_, Option<T>> {
        self.0.lock().unwrap()
    }
}

// 需要在保留结果的前提下被多处读取时使用；相比 `take` 不破坏完成状态。
impl<T: Send + Clone + 'static> Task<T> {
    /// 复制一份结果而不消费它，便于 UI 与逻辑同时读取同一份数据。
    pub fn clone_result(&self) -> Option<T> {
        self.0.lock().unwrap().clone()
    }
}
