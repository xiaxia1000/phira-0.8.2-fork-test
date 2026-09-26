//! Logging utilities.
//!
//! 接入方式：把 `tracing` 生态（[`tracing`] + [`tracing_subscriber`]）作为前端 API，
//! 后端则复用 macroquad/miniquad 提供的日志宏——这样同一份日志既能被
//! `RUST_LOG` 过滤，又能自动落到各平台的原生日志通道（logcat / stderr / 控制台）。

use colored::Colorize;
// 使用 miniquad 的日志宏而非 println：它会按平台把日志分流到 Android logcat、
// OHOS hilog 或标准输出，同时避免在渲染线程直接写标准流导致的阻塞。
use miniquad::{debug, error, info, trace, warn};
use tracing::{field::Visit, Level, Subscriber};
use tracing_subscriber::{prelude::*, EnvFilter, Layer};

/// 自定义的 tracing 层：把事件格式化为带颜色的一行文本后交给底层日志宏。
///
/// 之所以自己实现而不使用 `tracing_subscriber::fmt`，是因为需要
/// 统一两端的行为：既能识别 `log.` 前缀的结构化字段（`tracing-log` 兼容层
/// 转发过来的事件），又能把自定义字段渲染成 `{k=v}` 形式以节省屏幕宽度。
struct CustomLayer;

// 实现 tracing 的 Layer 协议：本层只关心 on_event，其它回调沿用默认空实现。
impl<S> Layer<S> for CustomLayer
where
    S: Subscriber,
{
    /// 事件回调：收集字段、拼装并着色文本行，最后按级别选择输出通道。
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        // 事件字段的收集器。tracing 的字段是动态键值，无法直接反射，
        // 只能通过 `Visit` 访问者模式拿到；`message` 单独存放以便放在行尾，
        // 其余字段按 `{k=v}` 渲染。
        #[derive(Default)]
        struct Visitor {
            message: Option<String>,
            target: Option<String>,
            fields: Vec<(&'static str, String)>,
        }
        // 字符串字段与 Debug 字段分开处理，因为 tracing 对二者调用不同回调。
        impl Visit for Visitor {
            // 处理字符串字段。`log.` 前缀的字段由兼容层注入，
            // 不属于用户数据：其中 `log.target` 是真正的日志目标，单独提取，
            // 其余（如 `log.module_path`）直接丢弃，避免污染输出。
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.message = Some(value.to_string());
                } else if !field.name().starts_with("log.") {
                    self.fields.push((field.name(), value.to_string()));
                } else if field.name() == "log.target" {
                    self.target = Some(value.to_string());
                }
            }

            // 处理非字符串字段（数字、结构体等），统一转成 Debug 字符串。
            // 这里同样过滤 `log.` 前缀字段，且不处理 `log.target`（它必为字符串）。
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let val = format!("{value:?}");
                if field.name() == "message" {
                    self.message = Some(val);
                } else if !field.name().starts_with("log.") {
                    self.fields.push((field.name(), val));
                }
            }
        }

        let mut v = Visitor::default();
        event.record(&mut v);

        let meta = event.metadata();
        // 优先使用事件里显式携带的 target（来自 log 兼容层），否则退回元数据中的 target。
        let target = v.target.as_deref().unwrap_or_else(|| meta.target());
        // JNI 绑定层会为每次调用打印 INFO 级日志，量极大且无排查价值，直接过滤；
        // WARN/ERROR 仍然保留，因为那些通常意味着真实的调用失败。
        if target.starts_with("jni::") && meta.level() >= &Level::INFO {
            return;
        }

        // 非 Android 平台自己拼时间戳与级别：底层日志宏在这些平台上不会附加前缀。
        #[cfg(not(target_os = "android"))]
        let mut msg = format!("{:.6?} ", chrono::Utc::now()).bright_black().to_string()
            + &match *meta.level() {
                Level::TRACE => "TRACE".bright_black(),
                Level::DEBUG => "DEBUG".magenta(),
                Level::INFO => " INFO".green(),
                Level::WARN => " WARN".yellow(),
                Level::ERROR => "ERROR".red(),
            }
            .to_string()
            + " ";

        // Android 下 logcat 会自带时间戳与级别，重复打印只会让日志更难读，故留空。
        #[cfg(target_os = "android")]
        let mut msg = String::new();

        msg += &target.bright_black().to_string();
        // 自定义字段渲染为 `{k=v k2=v2}`；末尾多写了一个空格，因此这里 pop 掉最后一个空格。
        if !v.fields.is_empty() {
            msg += &"{".bold().to_string();
            for (name, val) in &v.fields {
                use std::fmt::Write;
                let _ = write!(msg, "{}={val} ", name.italic());
            }
            if !v.fields.is_empty() {
                msg.pop();
            }
            msg += &"}".bold().to_string();
        }
        // 消息正文放最后，避免长文本把结构化字段挤出视野。
        if let Some(message) = v.message {
            msg += ": ";
            msg += &message;
        }

        // 按级别选择对应的底层日志宏，保证平台侧仍能按级别过滤。
        match *meta.level() {
            Level::TRACE => trace!("{}", msg),
            Level::DEBUG => debug!("{}", msg),
            Level::INFO => info!("{}", msg),
            Level::WARN => warn!("{}", msg),
            Level::ERROR => error!("{}", msg),
        }
    }
}

/// 注册全局日志订阅者。进程启动时调用一次，重复调用会 panic（`init` 只能调用一次）。
///
/// 过滤规则：若外部设置了 `RUST_LOG` 环境变量则完全以它为准（便于现场排障）；
/// 否则使用内置默认值——把依赖库（hyper / rustls）压到 info 以减少噪声，
/// 本项目自身的日志保持在 debug 级别。
pub fn register() {
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::try_new("hyper=info,rustls=info,debug").unwrap()
    };
    tracing_subscriber::registry().with(CustomLayer).with(filter).init();
}
