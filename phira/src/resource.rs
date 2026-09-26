//! 本地化资源文案的宏入口。
//!
//! 本文件调用 `prpr_l10n::tl_file!`，在 `crate::resource` 模块路径下生成名为 `rtl!`
//! 的本地化宏，其它模块通过 `use crate::resource::rtl;` 使用它来取当前语言下的文案。
//! 真正的翻译内容不在本文件，而是存放在 `phira/locales/<lang>/resource.ftl`（FTL 格式）；
//! 切换语言时只需替换对应的 `.ftl`，宏调用点无需改动。
prpr_l10n::tl_file!("resource" rtl);
