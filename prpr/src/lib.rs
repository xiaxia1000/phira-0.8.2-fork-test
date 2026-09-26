//! prpr 核心库：Phira 的谱面数据结构、解析、渲染与播放环境运行时。
//!
//! 本 crate 把桌面 / Android / wasm / OpenHarmony 等平台差异封装在内部，
//! 对外提供统一的谱面模型、资源加载、音画同步时间轴与绘制工具，
//! 游戏主程序只需依赖本 crate 的公开 API（见下方各个 `pub use` 与模块）。

/// 自研二进制谱面格式（`.pbc`）的读写实现，见 [`bin::BinaryData`]。
pub mod bin;
/// 播放环境配置：玩家名、音量、速度、autoplay 等用户可见设置项。
pub mod config;
/// 谱面核心数据结构（判定线、音符、动画、BPM 表）以及全局几何常量。
pub mod core;
/// 带路径穿越防护的目录封装，用于所有需要写文件的场景。
pub mod dir;
/// 绘制 / 纹理 / 线程等杂项工具函数。
pub mod ext;
/// 谱面资源来源抽象（桌面目录、zip 包、Android assets、内存补丁）。
pub mod fs;
/// 谱面元数据（与 `info.yml` 对应）。
pub mod info;
/// 判定逻辑：命中检测与判定状态流转。
pub mod judge;
/// 谱面解析：把 RPE / PEC / PGR / PBC 等外部格式转换为内部结构。
pub mod parse;
/// 粒子系统。
pub mod particle;
/// 场景抽象与游戏主循环骨架。
pub mod scene;
/// 异步任务（耗时加载、下载等）的轮询封装。
pub mod task;
/// 音乐时间与真实时间的同步管理器。
pub mod time;
/// UI 组件与文本绘制。
pub mod ui;

// 日志后端依赖 tracing 生态且体积不小，仅在启用 `log` feature 时编译，
// 让无日志需求的精简发行版不必带入这部分依赖。
#[cfg(feature = "log")]
pub mod log;

// inner 模块只在以 `--cfg closed` 编译的闭源构建中存在（用于接入不便开源的资源/服务实现）。
// 开源构建下该模块整体缺失，因此必须用 cfg 门控，否则会编译失败；
// 同时跳过 rustfmt 以免其内部特殊布局被重排。
#[rustfmt::skip]
#[cfg(closed)]
pub mod inner;

/// 游戏主入口场景，各平台共用的场景实现（实现了 [`scene`] 中的场景约定）。
pub use scene::Main;

/// 构造 macroquad 的窗口配置。
///
/// 默认窗口 973x608 是 Phigros 视觉比例在桌面窗口下的取整结果，
/// 使窗口内绘制区域的宽高比接近游戏内基准比例，避免首帧出现明显拉伸；
/// 其余字段（含抗锯齿、采样数、窗口图标等）一律沿用 macroquad 默认值。
pub fn build_conf() -> macroquad::window::Conf {
    macroquad::window::Conf {
        window_title: "Phira".to_string(),
        window_width: 973,
        window_height: 608,
        ..Default::default()
    }
}
