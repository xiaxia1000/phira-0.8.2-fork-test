//! 谱面列表的「排序字段」定义。
//!
//! 之所以把排序语义抽成这个独立的小枚举，而不是散落在各列表里：曲库页
//! （`page/library.rs`）同时维护**云端**、**本地**、**收藏夹**三份列表，它们
//! 共用同一套排序菜单，但排序发生的位置不同——
//! - 云端列表把变体映射成服务端 `order=` 参数（`Default` → `updated`），由后端排序；
//! - 本地/收藏夹列表无法走服务端，改调用 [`ChartOrder::apply`] 在客户端就地排序。
//!
//! 两者必须给出**可预期的一致性**：同一切换动作在云端与本地看到的顺序语义应当吻合，
//! 因此本文件是「按什么排」的唯一定义点，菜单展示名则统一由 `chart_order` 本地化文件提供。

prpr_l10n::tl_file!("chart_order");

use std::borrow::Cow;

use crate::page::ChartItem;

/// 曲库可选的排序字段。
///
/// 注意 `Rating` 只在云端列表出现：本地谱面没有评分数据，`library.rs` 会在切到本地
/// 标签页时主动把 `Rating` 回退成 `Default`，并在构造菜单选项时剔除该项。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChartOrder {
    /// 默认排序。本地/收藏夹下表示**保持原始顺序**（不被重排，等价于玩家手动拖拽或
    /// 磁盘扫描得到的次序）；云端下被映射为服务端 `updated`（按更新时间）。
    /// 这层「同名不同义」是有意为之：本地没有更新时间可用，退回原序最不易让玩家困惑。
    Default,
    /// 按曲名排序（字符串序，受本地化/大小写影响）。
    Name,
    /// 按难度数值排序（对应 `ChartItem` 的 `difficulty` 浮点值，非定数等级名）。
    Difficulty,
    /// 按评分排序。仅云端列表有效；`apply` 中为空实现，实际排序由服务端完成。
    Rating,
}

impl ChartOrder {
    /// 返回该排序字段用于菜单展示的本地化名称。
    ///
    /// 名称取自 `chart_order` 本地化文件的 `time`/`name`/`difficulty`/`rating` 键；
    /// 返回 `Cow` 以免为静态字符串分配内存。
    pub fn label(&self) -> Cow<'static, str> {
        match self {
            Self::Default => tl!("time"),
            Self::Name => tl!("name"),
            Self::Difficulty => tl!("difficulty"),
            Self::Rating => tl!("rating"),
        }
    }

    /// 就地对切片排序，供**无法走后端排序**的列表使用（本地谱面、收藏夹等）。
    ///
    /// 通过 `f` 把任意元素投影成 `ChartItem` 引用，从而让同一次排序同时服务于
    /// 「元素就是 `ChartItem`」与「元素是包着 `ChartItem` 的包装类型」两种数据形态，
    /// 调用方无需先拷出一份中间集合。
    ///
    /// 比较键与缺失值策略：
    /// - [`ChartOrder::Default`]：不排序，保留调用方传入时的既有顺序；
    /// - [`ChartOrder::Name`]：以 `info.name` 为键做字典序比较；
    /// - [`ChartOrder::Difficulty`]：以 `info.difficulty` 浮点为键；遇到 `NaN` 等不可比
    ///   值时按相等处理（`Ordering::Equal`），避免排序中途 panic，也保证这些元素位置稳定；
    /// - [`ChartOrder::Rating`]：本函数不做任何事——评分只存在于服务端，本地列表根本没有
    ///   可比较的数据，而其菜单项也已在上层被剔除。
    ///
    /// 底层使用 `slice::sort_by`（**稳定排序**），因此比较键相同的元素会保留原有相对次序：
    /// 这正是「按难度排序后曲名仍保持原序」这类细节得以成立的前提，也是本地列表可以放心
    /// 反复切换排序字段而不丢原始顺序的原因。
    ///
    /// 升序/降序不由本函数决定：调用方在 `apply` 之后按需 `reverse()`（见 `library.rs`）。
    pub fn apply<T>(&self, charts: &mut [T], f: impl Fn(&T) -> &ChartItem) {
        match self {
            // 保持原序，不做任何比较。
            Self::Default => {}
            Self::Name => {
                charts.sort_by(|x, y| f(x).info.name.cmp(&f(y).info.name));
            }
            // 浮点难度可能为 NaN，`partial_cmp` 返回 None 时退化为「相等」，
            // 既避免 unwrap panic，也让这类元素在稳定排序中停在原位。
            Self::Difficulty => {
                charts.sort_by(|x, y| {
                    f(x).info
                        .difficulty
                        .partial_cmp(&f(y).info.difficulty)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            // 评分排序由服务端承担；本地没有该字段，故此处是空实现。
            Self::Rating => {}
        }
    }
}
