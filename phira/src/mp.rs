//! 多人联机（multiplayer）模块入口。
//!
//! 本模块只做两件事：加载本模块专用的本地化文案宏（`mtl!`，取自
//! `locales/multiplayer.yml`），并把 `panel` 子模块中的 `MPPanel` 重新导出为
//! `crate::mp::MPPanel`，这样 `scene` 等上层模块可以只依赖 `crate::mp` 而不必
//! 关心里面的文件划分；真正的联机逻辑全部在 `panel.rs` 中实现。
prpr_l10n::tl_file!("multiplayer" mtl);

// 联机面板实现（连接管理、房间操作、消息同步、进入对局）。
mod panel;
// 对外只暴露面板类型；`panel` 其余内部实现细节保持模块私有。
pub use panel::MPPanel;
