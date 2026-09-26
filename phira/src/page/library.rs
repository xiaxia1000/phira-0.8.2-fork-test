//! 曲库页（Library）：主场景里的「谱面库」标签页，也是本 crate 中最复杂的页面之一。
//!
//! 它把三类完全不同的数据来源统一成一组标签页（见 [`ChartListType`]）：本地已安装谱面、
//! 云端分页列表（正式/特殊/不稳定/热门）、以及本地列表的「收藏夹过滤」视图。每个标签页持有
//! 自己独立的 [`ChartsView`] 与滚动位置，而筛选、排序、搜索、当前收藏夹是**全页共享**的，
//! 因此任何共享条件变化都会重新装载当前标签页——本地走 [`LibraryPage::sync_local`]（纯内存），
//! 云端走 [`LibraryPage::load_online`]（异步请求，且总是回到第 1 页）。
//!
//! 本模块还承担三件跨模块的职责：
//! - **导入**：只发起系统文件选择（`request_file("_import")`），解压、校验与写入
//!   `data/charts/custom/<uuid>` 全部由 `MainScene` 完成，完成后经 `NEED_UPDATE` 通知本页重扫；
//! - **导出**：把选中的本地谱面目录打成「外层 zip + 每个谱面一个内层 zip + `export.json`」的
//!   批量包。申请文件句柄是异步的，因此用 [`request_export`] / [`take_export`] / [`resolve_export`]
//!   这套握手：先发起请求，拿到句柄后在线程里压缩，写完再回执给平台侧；
//! - **收藏夹与封面**：用 [`FAV_UPDATED`] / [`CHOOSE_COVER`] / `CHOSEN_COVER` 与
//!   [`FavoritesPage`] 双向通信，避免页面之间互相持有引用。
//!
//! 设计要点：所有长耗时操作（云端请求、收藏夹同步、导出）都以任务句柄/线程 + 通道的形式
//! 存在字段里，在 `update` 中轮询；只要句柄非 `None`，页面就进入「忙」状态并吞掉触摸
//! （见 `touch` 开头），保证同一时刻只有一个异步流程在改写列表数据。
prpr_l10n::tl_file!("library");

// 页面协作方：收藏夹页与合集页（后者仅在 `closed` 构建下可达，见 `CollectionPage` 的用法）、
// 页面跳转枚举，以及主场景共享状态。
use super::{CollectionPage, FavoritesPage, NextPage, Page, SharedState};
use crate::{
    charts_view::{ChartDisplayItem, ChartsView, NEED_UPDATE},
    client::{recv_raw, Chart, ChartRef, ChartRefChartInfo, Client, Collection, CollectionUpdate, LocalCollection},
    dir, get_data, get_data_mut,
    icons::Icons,
    page::{favorites::FAV_PAGE_RESULT, ChartItem, ChartType, Illustration},
    popup::Popup,
    rate::RateDialog,
    save_data,
    scene::{check_read_tos_and_policy, compress_folder, confirm_dialog, ChartOrder, JUST_LOADED_TOS},
    tabs::{Tabs, TitleFn},
    tags::TagsDialog,
};
use anyhow::{anyhow, Error, Result};
use chrono::{DateTime, Utc};
use inputbox::InputBox;
#[cfg(target_os = "android")]
use jni::{jni_sig, jni_str, objects::JObject, refs::Global, vm::JavaVM, EnvUnowned};
use macroquad::prelude::*;
use prpr::{
    ext::{poll_future, semi_black, JoinToString, LocalTask, RectExt, SafeTexture, ScaleType},
    scene::{request_file, request_input, return_input, show_error, show_message, take_input, NextScene},
    task::Task,
    ui::{button_hit, DRectButton, Dialog, RectButton, Ui},
};
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    borrow::Cow,
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs::File,
    io::{self, BufWriter, Cursor, Write},
    mem,
    ops::Deref,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc, Mutex,
    },
};
use tap::Tap;
use uuid::Uuid;

// 「收藏夹内容已变更」的一次性通知标志。
//
// 收藏动作发生在别的页面（如 `SongScene` 的收藏按钮）甚至别处的异步任务里，那些地方没有
// 本页的引用，只能置位这个全局标志；本页在 `enter`（重新进入曲库）时 `swap(false)` 取走，
// 顺手清位，从而保证一次变更只触发一次本地列表重排，也不会把状态永久卡在 true。
pub static FAV_UPDATED: AtomicBool = AtomicBool::new(false);

// 「正在为收藏夹挑选封面」的模式标志。
//
// 该模式由 [`FavoritesPage`] 的「设置封面」菜单项开启，随后那个页面被 Pop 掉、玩家回到本页
// 点选谱面；因此这里是一个跨页面的模式开关：置位期间本页的触摸判定要整体让位给列表点击
// （见 `touch`），并且不能弹出排序/筛选等菜单，否则会把「选封面」误当成普通交互。
// 拿到结果或被取消后必须清位，否则页面会一直停留在只能点卡片的「僵尸模式」。
pub static CHOOSE_COVER: AtomicBool = AtomicBool::new(false);

// 「选封面」结果的一次性回传通道（线程本地，无需加锁）。
//
// 写入方是 [`ChartsView::touch`]（选中某张谱面时），读取方是本页的 `update`；
// `Some(Ok(id))` = 选了一张**云端**谱面（用其服务端 id 作封面），
// `Some(Err(path))` = 选了一张**本地**谱面（用其本地目录名作封面，因为本地谱面没有 id），
// `None` = 还没有结果。`update` 取走后立刻把 [`CHOOSE_COVER`] 清位，并把结果作为参数
// 重新构造 [`FavoritesPage`]（即「回到收藏夹页并应用封面」）。
thread_local! {
    pub static CHOSEN_COVER: RefCell<Option<Result<i32, String>>> = const { RefCell::new(None) };
}

// 云端列表每页请求的条目数。服务端只返回总数 `count`，总页数由客户端按
// `(count - 1) / PAGE_NUM + 1` 自行推算（见 `load_online`），因此改这个常量会同时影响
// 请求与翻页按钮的可用范围。
const PAGE_NUM: u64 = 28;

/// 曲库列表的视图种类。每个变体代表一种**数据来源 + 查询语义**完全不同的列表，
/// 各自拥有独立的 [`ChartsView`] 实例与滚动位置（见 [`ChartList`]）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChartListType {
    /// 本地列表：来自磁盘扫描出的已安装谱面（`SharedState::charts_local`）。
    /// 支持拖拽排序、多选导出/删除，不参与分页与云端查询；在 `closed` 构建下还会在
    /// 列表最前面插入一张占位卡用于跳转合集页。
    Local,
    /// 云端正式（Ranked）谱面：查询参数 `type=0`。
    Ranked,
    /// 云端特殊（Special）谱面：查询参数 `type=1`。
    Special,
    /// 云端不稳定（Unstable）谱面：查询参数 `type=2`。
    Unstable,
    /// 云端热门榜：不套用搜索/排序/标签/评分区间等参数，直接请求 `/chart/popular`，
    /// 因而该标签页隐藏筛选、排序与翻页控件。
    Popular,
}

/// 一个曲库标签页的全部状态：视图种类 + 它自己的列表视图。
/// 每个标签页单独持有一份 [`ChartsView`]，因此切换标签页会保留各自的滚动位置，
/// 而筛选/排序/搜索条件则是**全页共享**的（存在 [`LibraryPage`] 上）。
struct ChartList {
    /// 该标签页的数据来源种类，决定查询参数、是否可翻页/筛选/拖拽排序。
    ty: ChartListType,
    /// 列表渲染与交互（虚拟滚动、卡片点击、多选、拖拽排序）全部委托给它。
    /// 滚动条与虚拟滚动的约定（`scroll.size(...)` 声明内容尺寸、`scroll.render(ui, |ui| -> (w, h))`
    /// 返回实际内容尺寸）都封装在 [`ChartsView`] 内部，本页只提供一个外框矩形。
    view: ChartsView,
}
// 构造单个标签页的列表视图：本地列表靠 `NEED_UPDATE` 广播被动刷新，因此关闭下拉刷新
// 以免玩家误以为能手动从服务端拉取新数据；云端列表则允许下拉刷新。
impl ChartList {
    fn new(ty: ChartListType, icons: Arc<Icons>, rank_icons: [SafeTexture; 8]) -> Self {
        let mut view = ChartsView::new(icons, rank_icons);
        view.can_refresh = ty != ChartListType::Local;
        Self { ty, view }
    }
}

/// 「把多选中的谱面新建为一个本地收藏夹」任务的中间产物。
/// 之所以要单独跑一个任务，是因为部分选中的谱面只有 id 而没有详细信息，
/// 需要先批量拉取元信息补全后才能落盘保存。
struct CreateFavorite {
    /// 玩家在输入框里填写的收藏夹名称（已通过文本审核）。
    name: String,
    /// 已补齐 `info` 的谱面引用列表，将被整体写入新的 `LocalCollection`。
    charts: Vec<ChartRef>,
}

/// 「把多选中的谱面移入/移出某个收藏夹」任务的中间产物。
struct ManageFavorite {
    /// 目标收藏夹的本地 uuid（不是服务端 id，云端同步要在后续步骤里自己查）。
    uuid: Uuid,
    /// 待操作的谱面引用；若为「加入」操作，这些引用已补齐云端元信息。
    charts: Vec<ChartRef>,
    /// `true` = 加入收藏夹，`false` = 从收藏夹移除。
    /// 由菜单项当前的勾选状态取反得出，因此重复点击同一项会在两种操作间来回切换。
    add: bool,
}

/// 云端列表请求的返回值：渲染用条目、原始 `Chart`（供后续按 id 反查）、总页数。
type OnlineTaskResult = (Vec<ChartDisplayItem>, Vec<Chart>, u64);
/// 云端列表请求的任务句柄。页面只持有一个，新请求会直接覆盖旧请求（见 `load_online`）。
type OnlineTask = Task<Result<OnlineTaskResult>>;

/// 曲库页。
///
/// 一个页面同时承载「本地 / 云端分页 / 收藏夹」三大类列表：列表数据本身是**每标签页独立**
/// 的（`tabs`），而筛选、排序、搜索、收藏夹选择是**全页共享**的，因此任何一个共享条件
/// 变化都必须刷新当前标签页——本地列表走 [`Self::sync_local`]，云端列表走
/// [`Self::load_online`]，后者总是从第 1 页重新开始。
pub struct LibraryPage {
    /// 五个固定标签页（本地/正式/特殊/不稳定/热门），每个持有独立的 [`ChartList`]。
    /// 切换标签页会重置该页滚动位置并清空所有页的多选状态。
    tabs: Tabs<ChartList>,

    /// 云端列表当前页码（与 `current_page + 1` 一起展示给玩家）。
    /// 仅云端标签页使用；任何筛选/搜索变化都会把它归零。
    current_page: u64,
    /// 服务端返回的谱面总数换算出的总页数，用于限制「下一页」按钮并显示 `x / y`。
    online_total_page: u64,
    /// 「上一页」按钮；到第 1 页时按钮仍可点，但点击后不会请求（见 `touch`）。
    prev_page_btn: DRectButton,
    /// 「下一页」按钮；超过 `total_page()` 时点击后不会请求。
    next_page_btn: DRectButton,

    /// 正在进行的云端列表请求。同一时刻只保留一个：新请求会**直接覆盖**旧句柄，
    /// 相当于丢弃尚未完成的旧请求（其返回值不再有人消费）。
    online_task: Option<OnlineTask>,
    /// Set when `load_online` was blocked by the TOS gate, so the online list
    /// is retried automatically once the player accepts the terms — otherwise
    /// it stays on the loading spinner forever.
    /// 补充：只在协议门禁放行后（`terms_modified` 由 `None` 变为 `Some`）才自动重试一次。
    online_pending_tos: bool,

    /// 图标集，同时用于本页按钮与新建的子页面（[`FavoritesPage`] 等）。
    icons: Arc<Icons>,
    /// 定数/评级徽标贴图，透传给每个 [`ChartsView`] 与子页面。
    rank_icons: [SafeTexture; 8],

    /// 「+」导入按钮。点击只发起系统文件选择，真正的导入由 `MainScene` 完成，
    /// 见 `touch` 中的 `request_file("_import")`。
    import_btn: DRectButton,

    /// 搜索入口按钮（点击后弹出系统输入框，而不是内嵌编辑框）。
    search_btn: DRectButton,
    /// 当前搜索关键字。对本地列表是**即时生效的子串匹配**（`#数字` 前缀则按谱面 id 精确匹配），
    /// 对云端列表则作为查询参数随 `load_online` 下发；本页没有输入框，改动只来自 `take_input`。
    search_str: String,
    /// 搜索框右侧的清除按钮；仅在有内容时显示，命中后清空关键字并立即刷新列表。
    search_clr_btn: RectButton,

    /// 排序入口按钮（图标按钮），点击后先弹出 [`Self::order_meta_menu`]（排序字段 + 升降序）。
    order_btn: DRectButton,
    /// 排序字段选择菜单（由 `order_meta_menu` 的第一项拉起）。
    order_menu: Popup,
    /// 当前可选排序字段。本地列表不含「评分」项，因此该集合在渲染时才按标签页类型填充。
    order_menu_options: Vec<ChartOrder>,
    /// 一帧延迟标志：`render` 里置位后由下一次 `render` 真正弹出菜单，
    /// 保证菜单基于当帧算出的按钮矩形定位。
    need_show_order_menu: bool,
    /// 当前排序字段。`Default` 在云端映射为 `updated`，本地则等价于「保持磁盘/手动顺序」。
    current_order: ChartOrder,
    /// 「排序字段 / 升序降序」两级菜单。
    order_meta_menu: Popup,
    /// 一帧延迟标志，含义同 `need_show_order_menu`。
    need_show_order_meta_menu: bool,

    /// 是否降序。切换到 `Default`/`Rating` 时会被强制置 `true`（见 `update`/`order_menu.changed`），
    /// 因为这两个字段的语义默认就是从新到旧 / 从高到低。
    order_rev: bool,

    /// 筛选入口按钮，仅在云端标签页显示。
    filter_btn: DRectButton,
    /// 标签筛选对话框（云端查询的 `tags`/`division` 参数来源）。
    tags: TagsDialog,
    /// 上一帧标签对话框是否可见，用于检测「刚关闭」并据此重新请求列表。
    tags_last_show: bool,
    /// 评分区间筛选对话框（云端查询的 `rating` 参数来源）。
    rating: RateDialog,
    /// 上一帧评分对话框是否可见，作用同上。
    rating_last_show: bool,
    /// 筛选按钮当前打开的是标签还是评分对话框；两者可通过对话框内的按钮互相切换。
    filter_show_tag: bool,

    // 收藏夹 || Favorites
    /// 「收藏夹」入口按钮：打开收藏夹页（Overlay），并把当前过滤的收藏夹传过去。
    fav_btn: DRectButton,
    // None = 显示全部 || show all,      Some(folder_name) = 过滤指定收藏夹 || filter by folder
    /// 本地列表当前过滤到的收藏夹下标（对应 `get_data().collection_uuids()`）。
    /// `None` 表示不过滤、直接展示所有已安装谱面。
    current_fav_index: Option<usize>,
    /// 收藏夹与云端的同步任务。
    /// `Ok(Some(col))` 表示拉到了云端最新版本（随后与本地 `merge`）；
    /// `Ok(None)` 表示服务端判定本地版本已过期、需要玩家确认强制覆盖上传。
    sync_fav_task: Option<Task<Result<Option<Collection>>>>,
    /// 玩家在「本地已过期」确认框里点了确认：置位后由 `update` 触发一次强制上传。
    /// 用共享标志而非回调返回值传递，因为对话框跨越多个帧。
    force_sync_to_cloud: Arc<AtomicBool>,

    /// 多选模式下的「…」操作按钮（导出/新建收藏夹/移动收藏夹/删除）。
    multi_operation_btn: DRectButton,
    /// 多选操作菜单。
    multi_operation_menu: Popup,
    /// 与菜单项一一对应的**本地化 key**，避免依赖本地化后的显示文本做分支判断。
    multi_operation_options: Vec<&'static str>,
    /// 一帧延迟标志，含义同 `need_show_order_menu`。
    need_show_multi_operation_menu: bool,

    /// 「移动到收藏夹」菜单（多选操作菜单的第二级）。
    /// 关闭了自动消失，因为勾选后需要原地刷新选项（打勾状态变化）继续操作。
    manage_fav_menu: Popup,
    /// 每项为 `(收藏夹 uuid, 选中谱面是否已全部在该收藏夹内)`；后者用于显示勾选标记
    /// 并推导本次是「加入」还是「移除」。
    manage_fav_menu_options: Vec<(Uuid, bool)>,
    /// 一帧延迟标志，含义同 `need_show_order_menu`。
    need_show_manage_fav_menu: bool,
    /// 收藏夹变更的云端提交任务，返回 `(服务端返回的合集, 本次是否为加入)`。
    manage_fav_task: Option<Task<Result<(Collection, bool)>>>,
    /// 提交前的准备任务：为尚无元信息的谱面批量拉取 `Chart`（仅「加入」操作需要）。
    manage_fav_pre_task: Option<Task<Result<ManageFavorite>>>,
    /// 「刷新纯本地收藏夹」任务：本地收藏夹没有服务端 id，无法整体同步，
    /// 只能按其中的谱面 id 批量反查最新元信息，回填后落盘。返回 `(收藏夹 uuid, 新引用列表)`。
    #[allow(clippy::type_complexity)]
    refresh_local_fav_task: Option<Task<Result<(Uuid, Vec<ChartRef>)>>>,

    /// 多选的「全选/反选」按钮。
    multi_select_btn: DRectButton,
    /// 全选/反选菜单，选项固定为两项。
    multi_select_menu: Popup,
    /// 一帧延迟标志，含义同 `need_show_order_menu`。
    need_show_multi_select_menu: bool,

    /// 退出多选模式的按钮。
    multi_select_cancel_btn: DRectButton,
    /// 「确认删除」对话框的回执标志：对话框跨帧，确认后由 `update` 消费并执行删除。
    delete_multi: Arc<AtomicBool>,
    /// 「新建收藏夹」任务：先在后台补齐云端元信息，成功后才在本地建夹并落盘。
    multi_create_fav_task: Option<Task<Result<CreateFavorite>>>,

    /// 本页要切换到的下一个页面（收藏夹页 Overlay / 合集页），由 `next_page()` 取走。
    next_page: Option<NextPage>,
    /// 异步构造下一页面的任务（合集页需要先请求数据），与 `next_page` 二选一。
    next_page_task: LocalTask<Result<NextPage>>,

    /// 待导出的谱面目录名列表。由「多选导出」写入，是一次**待处理请求**，
    /// 只有拿到系统返回的文件句柄（`take_export`）后才会真正开始打包。
    export_paths: Option<Vec<String>>,
    /// 导出线程的完成回执通道。导出在独立线程里做（zip 压缩耗时），
    /// `Disconnected` 意味着线程 panic，需要提示错误并停止等待。
    export_task: Option<mpsc::Receiver<Result<()>>>,
    /// 已完成的谱面数量，供 UI 显示 `current / total` 进度。
    export_progress: Arc<AtomicU32>,
    /// 本次导出包含的谱面总数（`export_paths.len()`），与 `export_progress` 配对显示。
    export_total: usize,
}

// 构造：只搭建「默认状态」，不发起任何网络请求。云端列表的首次加载由页面进入后的
// `update` 触发的标签页切换分支完成，因此构造过程本身几乎不可能失败
// （保留 `Result` 只是为了与其它页面的构造约定保持一致）。
impl LibraryPage {
    /// 创建曲库页。
    ///
    /// 会先把 [`NEED_UPDATE`] 置位，使本页第一帧就重新扫描本地谱面——调用方（主场景）可能
    /// 是在别处导入或删除过谱面之后才切回曲库的，不能假设内存中的列表仍然有效。
    ///
    /// # Errors
    /// 当前实现不会失败；签名中的 `Result` 与其它页面保持一致的构造约定。
    pub fn new(icons: Arc<Icons>, rank_icons: [SafeTexture; 8]) -> Result<Self> {
        NEED_UPDATE.store(true, Ordering::Relaxed);
        let icon_star = icons.star.clone();
        let new_list = |ty| ChartList::new(ty, Arc::clone(&icons), rank_icons.clone());
        Ok(Self {
            // 标签页按界面从左到右排列：本地 / 正式 / 特殊 / 不稳定 / 热门。
            // 标题用惰性函数传入，以便语言切换后无需重建页面即可生效。
            tabs: Tabs::new([
                (new_list(ChartListType::Local), || tl!("local")),
                (new_list(ChartListType::Ranked), || ttl!("chart-ranked")),
                (new_list(ChartListType::Special), || ttl!("chart-special")),
                (new_list(ChartListType::Unstable), || ttl!("chart-unstable")),
                (new_list(ChartListType::Popular), || tl!("popular")),
            ] as [(ChartList, TitleFn); 5]),

            // 分页从第 0 页（界面显示为第 1 页）开始；总页数在首次请求返回前保持 0。
            current_page: 0,
            online_total_page: 0,
            // 其余按钮/菜单/对话框一律初始化为「空闲」状态：位置与可见性都由 `render` 每帧推导，
            // 触摸判定则要求先在渲染阶段设置好命中区域，因此这里只需构造空对象。
            prev_page_btn: DRectButton::new(),
            next_page_btn: DRectButton::new(),

            online_task: None,
            online_pending_tos: false,

            icons,
            rank_icons,

            import_btn: DRectButton::new(),

            search_btn: DRectButton::new(),
            search_str: String::new(),
            search_clr_btn: RectButton::new(),

            order_btn: DRectButton::new(),
            order_menu: Popup::new().with_size(0.5),
            order_menu_options: Vec::new(),
            need_show_order_menu: false,
            current_order: ChartOrder::Default,
            order_meta_menu: Popup::new().with_size(0.5),
            need_show_order_meta_menu: false,

            order_rev: true,

            // 标签筛选对话框的权限取自当前账号：未登录/无权限时，面板中与审核相关的选项
            // （「未审核」「请求稳定」等）会据此隐藏，防止玩家提交注定被服务端忽略的查询。
            filter_btn: DRectButton::new(),
            tags: TagsDialog::new(true).tap_mut(|it| it.perms = get_data().me.as_ref().map(|it| it.perms()).unwrap_or_default()),
            tags_last_show: false,
            // 评分区间默认取 3~10（下发时会除以 10 归一化到 0.3~1.0），
            // 即「只想看中高难度/高评分」的常见诉求，而不是从 0 开始的空区间。
            rating: RateDialog::new(icon_star, true).tap_mut(|it| {
                it.rate.score = 3;
                it.rate_upper.as_mut().unwrap().score = 10;
            }),
            rating_last_show: false,
            filter_show_tag: true,

            // 收藏夹相关状态：初始不过滤（显示全部），且没有任何同步任务在跑。
            fav_btn: DRectButton::new(),
            current_fav_index: None,
            sync_fav_task: None,
            force_sync_to_cloud: Arc::default(),

            multi_operation_btn: DRectButton::new(),
            multi_operation_menu: Popup::new().with_size(0.5),
            multi_operation_options: Vec::new(),
            need_show_multi_operation_menu: false,

            // 关闭自动消失：勾选某一项后菜单要留在原地，并把新的勾选状态刷新出来，
            // 便于玩家连续把同一批谱面加入/移出多个收藏夹。
            manage_fav_menu: Popup::new().with_size(0.5).tap_mut(|it| it.set_auto_dismiss(false)),
            manage_fav_menu_options: Vec::new(),
            need_show_manage_fav_menu: false,
            manage_fav_task: None,
            manage_fav_pre_task: None,
            refresh_local_fav_task: None,

            // 选项固定为「全选」「反选」两项，顺序与 `update` 中按 `selected()` 的 0/1 分支
            // 一一对应，改文案时不要调整顺序。
            multi_select_btn: DRectButton::new(),
            multi_select_menu: Popup::new()
                .with_size(0.5)
                .with_options(vec![tl!("multi-select-all").into_owned(), tl!("multi-select-invert").into_owned()]),
            need_show_multi_select_menu: false,

            multi_select_cancel_btn: DRectButton::new(),
            delete_multi: Arc::default(),
            multi_create_fav_task: None,

            // 所有跨帧状态（页面跳转、导出请求与进度）都从「空」开始：
            // 页面进入后由 `update` 自行发起首次加载，导出要等玩家主动多选导出才有内容。
            next_page: None,
            next_page_task: None,

            export_paths: None,
            export_task: None,
            export_progress: Arc::default(),
            export_total: 0,
        })
    }
}

// 列表装载与本地数据重排。
//
// 这一组方法都不直接绘制，只负责把「当前筛选/排序/搜索/收藏夹选择」换算成列表内容：
// - 云端标签页：构造一次分页请求（[`Self::load_online`]），结果由 `update` 轮询落库；
// - 本地标签页：纯内存重排 + 过滤（[`Self::sync_local`]），无网络、无异步。
// 两者的切换点集中在 [`Self::on_order_update`]，避免各处重复判断标签页类型。
impl LibraryPage {
    /// 当前标签页可翻页的最大页数。本地列表不分页，恒为 0，因此翻页控件只在云端标签页出现。
    fn total_page(&self) -> u64 {
        if self.tabs.selected().ty == ChartListType::Local {
            0
        } else {
            self.online_total_page
        }
    }

    /// 按当前的页码、排序、标签、评分区间、搜索词重新请求云端列表。
    ///
    /// 调用方（筛选/排序/搜索/翻页）只负责改状态与把 `current_page` 归零，实际的参数拼装
    /// 都在这里完成。本方法**不阻塞**：仅构造一个 [`OnlineTask`] 存入 `online_task`，
    /// 由 `update` 轮询结果；若此前已有未完成的请求，其句柄会被直接覆盖丢弃。
    ///
    /// 三种情况会提前返回且不发起请求：离线模式、未登录、协议未同意。其中协议门禁会置位
    /// `online_pending_tos`，以便玩家同意后自动重试一次，而不是永远停在加载动画上。
    pub fn load_online(&mut self) {
        if get_data().config.offline_mode {
            show_message(tl!("offline-mode")).error();
            return;
        }
        if get_data().me.is_none() {
            show_error(anyhow!(tl!("must-login")));
            return;
        }
        if !check_read_tos_and_policy(false, false) {
            // Blocked on the TOS gate; retry automatically once accepted.
            self.online_pending_tos = true;
            return;
        }
        self.online_pending_tos = false;
        // 先清空视图并回到顶部：让玩家立刻看到加载态，同时避免上一页/上次筛选的旧卡片
        // 继续可见而被误认为「已经加载完成」。
        self.tabs.selected_mut().view.reset_scroll();
        self.tabs.selected_mut().view.clear();
        // 以下是查询参数的一次性快照：在发起请求前把界面状态全部克隆出来，
        // 这样请求在途期间玩家继续改筛选也不会影响这一次结果，避免 UI 与结果不一致。
        let page = self.current_page;
        let search = self.search_str.clone();
        // 排序字段映射为服务端可识别的字符串；降序用前缀 `-` 表达（服务端约定）。
        // `Default` 落在 `updated`（按更新时间），与本地列表「保持手动顺序」的语义并不一致，
        // 因此下面的 `sync_local` 对它做了单独处理。
        let order = {
            let order = match self.current_order {
                ChartOrder::Default => "updated",
                ChartOrder::Name => "name",
                ChartOrder::Rating => "rating",
                ChartOrder::Difficulty => "difficulty",
            };
            if self.order_rev {
                format!("-{order}")
            } else {
                order.to_owned()
            }
        };
        // 标签同样用 `-` 前缀表示「排除」：正向标签与排除标签合成一个逗号分隔的 `tags` 参数；
        // `division`（分区）是独立的查询参数，不参与这个列表。
        // 注意 `unwanted` 在 `TagsDialog` 构造时即为 `Some`，此处 `unwrap` 依赖该不变量。
        let tags = self
            .tags
            .tags
            .tags()
            .iter()
            .cloned()
            .chain(self.tags.unwanted.as_ref().unwrap().tags().iter().map(|it| format!("-{it}")))
            .join(",");
        let division = self.tags.division;
        // 评分区间以 0.1 为单位保存（1~10），下发前除以 10 归一化到 0~1。
        let rating_range = format!("{},{}", self.rating.rate.score as f32 / 10., self.rating.rate_upper.as_ref().unwrap().score as f32 / 10.);
        let chosen = self.tabs.selected().ty;
        // 热门榜走独立端点，且**不使用**上面的搜索/排序/标签/评分参数：
        // 因此它既没有筛选入口，也不受这些条件影响。
        let popular = chosen == ChartListType::Popular;
        // 服务端的谱面分类枚举：0 正式、1 特殊、2 不稳定；其余标签页（含热门）传 -1 表示不限制。
        let typ = match chosen {
            ChartListType::Ranked => 0,
            ChartListType::Special => 1,
            ChartListType::Unstable => 2,
            _ => -1,
        };
        // 「只显示我上传的」需要当前账号 id；未登录时保持 `None`（不会走到这里，前面已拦截）。
        let by_me = if self.tags.show_me {
            get_data().me.as_ref().map(|it| it.id)
        } else {
            None
        };
        // 审核状态过滤：勾了「请求稳定」就只看待稳定谱面；否则勾「未审核」才过滤未过审的，
        // 两者互斥（`else if`），避免同时下发互相矛盾的 query。
        let show_unreviewed = self.tags.show_unreviewed;
        let show_stabilize = self.tags.show_stabilize;
        // 请求在后台任务中执行；页面只保留句柄，由 `update` 轮询结果与错误。
        self.online_task = Some(Task::new(async move {
            let mut q = Client::query::<Chart>();
            // 热门榜与普通列表是两条不同的查询路径：热门只带 `type`/`division`/分页参数。
            if popular {
                q = q.suffix("/popular");
            } else {
                q = q.search(search).order(order).tags(tags).query("rating", rating_range);
            }
            if let Some(me) = by_me {
                q = q.query("uploader", me.to_string());
            }
            if show_stabilize {
                q = q.query("stableRequest", "true");
            } else if show_unreviewed {
                q = q.query("reviewed", "false").query("stableRequest", "false");
            }
            let (remote_charts, count) = q
                .query("type", typ.to_string())
                .query("division", division)
                .page(page)
                .page_num(PAGE_NUM)
                .send()
                .await?;
            // 服务端返回的是总条数而非总页数，这里自行换算；`count == 0` 必须单独处理，
            // 否则 `(0 - 1)` 会在无符号数上回绕出一个巨大的页数。
            let total_page = if count == 0 { 0 } else { (count - 1) / PAGE_NUM + 1 };
            // 同时保留渲染用的条目与原始 `Chart`：后者供后续按 id 反查元信息（如加入收藏夹）。
            let charts: Vec<_> = remote_charts.iter().map(ChartDisplayItem::from_remote).collect();
            Ok((charts, remote_charts, total_page))
        }));
    }

    /// 重新计算**本地**标签页要显示的列表（纯内存操作，不发起网络请求）。
    ///
    /// 有两种模式：普通本地列表展示全部已安装谱面；当 `current_fav_index` 为 `Some` 时改为
    /// 只展示该收藏夹内的谱面——收藏夹里允许存在「有云端 id 但本机未安装」的引用，这类条目
    /// 不会被丢弃，而是以 `ChartType::Downloaded` + 云端元信息的形式呈现，点击即可下载。
    ///
    /// 过滤条件来自 `search_str`：`#数字` 形式按谱面 id 精确匹配，否则按名称子串匹配。
    /// 结果排序后写入视图，并触发一次淡入过渡（`ChartsView::set`）。
    fn sync_local(&mut self, s: &SharedState) {
        // 第一步：克隆一份本地谱面引用并按当前排序字段排序。`ChartOrder::Default` 不改变顺序
        // （即保留玩家手动拖拽/磁盘扫描得到的原始顺序），这正是它与云端 `updated` 的差异所在。
        let mut charts_local = s.charts_local.iter().collect::<Vec<_>>();
        self.current_order.apply(&mut charts_local, |it| it);
        if self.order_rev {
            charts_local.reverse();
        }

        // 搜索的两种语义：`#id` 走精确匹配（便于从分享的 id 直接定位），其余按名称匹配。
        // 名称匹配用 `to_ascii_lowercase`，只对 ASCII 大小写不敏感，中文名等价于原样比较。
        let search_by_id = if let Some(id_str) = self.search_str.strip_prefix('#') {
            id_str.trim().parse::<i32>().ok()
        } else {
            None
        };
        // 统一的本地匹配器：只有通过搜索条件的谱面才会进入列表。
        let local_matcher = |chart: &ChartItem| {
            if let Some(search_id) = search_by_id {
                chart.info.id == Some(search_id)
            } else {
                chart.info.name.to_ascii_lowercase().contains(&self.search_str.to_ascii_lowercase())
            }
        };

        // 第二步：按标签页类型组装最终列表。云端标签页在这里不做任何事——
        // 它们的内容由 `load_online` 的请求结果通过 `update` 写入。
        let list = self.tabs.selected_mut();
        if list.ty == ChartListType::Local {
            let mut charts = Vec::new();
            if let Some(fav_index) = self.current_fav_index {
                // 收藏夹模式：先建「本地目录名 -> 已安装谱面」的索引，供下面按路径反查，
                // 避免对每个引用都做一次线性扫描。
                let local_chart_map: HashMap<&str, &ChartItem> = charts_local.iter().map(|it| (it.local_path.as_deref().unwrap(), *it)).collect();
                charts.extend(get_data().collection_by_index(fav_index).charts.iter().filter_map(|it| {
                    // 分支一（最常见）：引用指向的谱面已安装在本机，直接复用本地条目，
                    // 这样插画缩略图等资源都是现成的。
                    if let Some(item) = local_chart_map.get(&*it.path) {
                        local_matcher(item).then(|| ChartDisplayItem::new(Some((*item).clone()), None))
                    // 分支二：本机没装，但引用里缓存了服务端元信息 —— 造一个「仅云端」条目，
                    // `local_path` 置 `None`、类型标为 `Downloaded`，于是点击会走下载而不是直接开局；
                    // 后续的多选导出/删除也会正确地视其为「无本地文件」。
                    } else if let Some(chart) = it.info.as_ref() {
                        search_by_id
                            .map_or_else(
                                || chart.info.name.to_ascii_lowercase().contains(&self.search_str.to_ascii_lowercase()),
                                |search_id| chart.info.id == Some(search_id),
                            )
                            .then(|| {
                                ChartDisplayItem::new(
                                    Some(ChartItem {
                                        info: chart.info.clone(),
                                        illu: Illustration::from_file_thumbnail(chart.illustration.clone()),
                                        local_path: None,
                                        chart_type: ChartType::Downloaded,
                                    }),
                                    None,
                                )
                            })
                    // 分支三：没有缓存的元信息，只能按云端 id 反查本地目录名（老数据常见）。
                    // 这里两个 `unwrap` 都有前提：`find_local_path` 会因 I/O 错误 panic；
                    // 内层查表假定「反查出来的路径必定来自 `charts_local`」——`charts_local`
                    // 与 `data.charts` 一致时才成立，属于本页依赖的隐含不变量。
                    } else if let Some(local_path) = it.find_local_path().unwrap() {
                        let item = local_chart_map.get(&*local_path).unwrap();
                        local_matcher(item).then(|| ChartDisplayItem::new(Some((*item).clone()), None))
                    // 分支四：既未安装也没有任何元信息，属于坏数据。只记录警告并跳过，
                    // 不让单条脏数据把整个列表的渲染拖垮。
                    } else {
                        warn!("No info found for chart ref {it:?}");
                        None
                    }
                }));
                // 收藏夹模式单独排序一次：收藏夹内的顺序是玩家自定义的，
                // 但与普通列表保持一致地受排序菜单与升/降序开关影响。
                self.current_order.apply(&mut charts, |it| it.chart.as_ref().unwrap());
                if self.order_rev {
                    charts.reverse();
                }
            } else {
                // 普通本地列表：`closed` 构建会额外提供「合集」入口——
                // 一张不带任何谱面数据的占位卡（`None`），点击后由 `update` 跳转合集页。
                if cfg!(closed) {
                    charts.push(ChartDisplayItem::new(None, None));
                }
                // 其余条目先过搜索条件，再逐个转成卡片；这里克隆 `ChartItem`（插画句柄是共享的）
                // 是刻意的——列表需要一份独立于 `SharedState` 的快照，避免下一帧重新扫描时抖动。
                charts.extend(
                    charts_local
                        .iter()
                        .filter(|it| local_matcher(it))
                        .map(|it| ChartDisplayItem::new(Some((*it).clone()), None)),
                )
            }
            // 一次性替换视图内容并触发淡入；以 `s.t` 作为动画时间基准。
            list.view.set(s.t, charts);
        }
    }

    /// 排序（字段或方向）变更后统一的分流入口。
    ///
    /// 本地列表只能就地重排；云端列表无法在客户端排序，必须回到第 1 页重新请求，
    /// 否则会出现「当前页数据属于旧排序」的错位。
    fn on_order_update(&mut self, s: &mut SharedState) {
        let list = self.tabs.selected_mut();
        if list.ty == ChartListType::Local {
            self.sync_local(s);
        } else {
            self.current_page = 0;
            self.load_online();
        }
    }

    /// 消费收藏夹页回传的「当前选中的收藏夹」结果（线程本地的 [`FAV_PAGE_RESULT`]）。
    ///
    /// 玩家在收藏夹页切换或退出收藏夹都会写这个通道，本页据此更新 `current_fav_index`
    /// 并立即重算本地列表。`take()` 保证只消费一次；`None` 表示收藏夹页没有变更。
    fn check_fav_page(&mut self, s: &mut SharedState) {
        if let Some(result) = FAV_PAGE_RESULT.with(|it| it.borrow_mut().take()) {
            self.current_fav_index = result;
            self.sync_local(s);
        }
    }

    /// 刷新第一级排序菜单的文案：第一项是「按 XX 排序」（XX 随当前字段变化），
    /// 第二项是升降序开关（文案随 `order_rev` 切换）。选项序号与 `update` 中的分支一一对应。
    fn update_order_meta_menu_options(&mut self) {
        self.order_meta_menu.set_options(vec![
            tl!("order-by", "order" => self.current_order.label()),
            if self.order_rev { tl!("order-desc") } else { tl!("order-asc") }.into(),
        ]);
    }

    /// 为「移动到收藏夹」菜单生成选项文案，并把 `(uuid, 是否已全部包含)` 记入
    /// `manage_fav_menu_options` 供后续点击时使用（因此本方法有副作用，不只是查询）。
    ///
    /// 返回 `None` 表示没有多选或选中为空——调用方据此静默不弹菜单。
    /// 只列出当前玩家**可写**的收藏夹（`is_owned`，即本地收藏夹或自己创建的云端收藏夹），
    /// 别人的收藏夹跳过，避免产生注定失败的上报请求。勾选标记由「选中谱面是否已全部在该夹内」
    /// 决定，这也正是后续把操作判定为「加入」还是「移除」的依据。
    fn get_move_fav_menu_options(&mut self) -> Option<Vec<String>> {
        let sel = self.tabs.selected().view.multi_select.as_ref()?;
        if sel.is_empty() {
            return None;
        }
        let data = get_data();
        let mut options = Vec::new();
        self.manage_fav_menu_options.clear();
        for uuid in data.collection_uuids() {
            let col = data.collection_info(uuid);
            if !col.is_owned() {
                continue;
            }
            let paths = col.charts.iter().collect::<HashSet<_>>();
            let all_in = sel.iter().all(|chart_ref| paths.contains(chart_ref));
            self.manage_fav_menu_options.push((*uuid, all_in));
            options.push(format!("{} {}", if all_in { '\u{2713}' } else { ' ' }, col.name));
        }
        Some(options)
    }
}

/// 导出流程的「已就绪」结果：目标文件句柄 + 失败时的清理回调。
///
/// 之所以要带上 `deleter`，是因为各平台的文件可能是**先创建再写入**的（iOS 临时文件、
/// Android 通过 SAF 创建的文档），一旦打包失败就必须把半成品删掉，否则会留下一个空文件或
/// 失效的文档条目；而不同平台的删除方式完全不同，只能由创建方提供闭包。
pub struct ExportConfig {
    /// 已打开、可直接写入的导出目标文件。
    pub file: File,
    /// 失败时的清理动作。要求 `Send` 是因为它要在导出线程里被调用（见 `update` 中的导出分支）。
    pub deleter: Box<dyn FnOnce() -> io::Result<()> + Send>,
}

/// 写在导出包根部的 `export.json` 元信息，用于让导入端识别「这是一份批量导出包」。
///
/// 导入端（`MainScene`）正是靠这个文件区分「单个谱面 zip」与「批量导出 zip」：
/// 存在即认为是批量包，会先统计内层 zip 数量并请玩家确认后再逐个导入。
#[derive(Serialize, Deserialize)]
pub struct ExportInfo {
    /// 导出发生的时间（UTC），仅作记录，不参与兼容性判断。
    pub exported_at: DateTime<Utc>,
    /// 导出时的客户端版本（`CARGO_PKG_VERSION`），用于在旧包导入时给出线索。
    pub version: String,
}

// 导出流程的跨平台握手通道：`request_export` 写入「系统返回的文件句柄」（或错误），
// 页面在 `update` 里用 [`take_export`] 取走。
//
// 用全局 `Mutex` 而不是返回值，是因为申请文件句柄是**异步**的：Android 要等 Java 侧回调
// （`process_export_fd`）、iOS 要等玩家在系统面板里操作，跨越多帧，调用点无法同步拿到结果。
static EXPORT_CONFIG: Mutex<Option<io::Result<ExportConfig>>> = Mutex::new(None);
// iOS 专用：导出写入的临时文件路径。因为 iOS 不能由 Rust 侧弹出「保存到文件」，
// 只能先把内容写到沙盒临时目录，再等文件写完后由 `resolve_export` 拉起系统面板交给用户。
// 与上面的 `EXPORT_CONFIG` 分开存放，是为了让「文件句柄」和「待分享路径」的生命周期解耦：
// 清理回调可以在删除文件的同时把这里置回 `None`，避免面板去分享一个已删除的文件。
#[cfg(target_os = "ios")]
static EXPORT_PICKER_PATH: Mutex<Option<String>> = Mutex::new(None);

// iOS 专用：把已经写好的文件交给系统文档面板（导出/拷贝到「文件」App）。
//
// 这里没有返回值也没有错误通道：结果只通过系统回调体现（成功时提示「已导出」），
// 因此调用方（`resolve_export`）只管拉起面板即可。delegate 必须用线程本地变量保活，
// 否则 Objective-C 侧只持有弱引用、对象会被立刻回收导致回调丢失。
#[cfg(target_os = "ios")]
fn present_export_picker(path: String) {
    use objc2::{available, define_class, rc::Retained, runtime::ProtocolObject, MainThreadMarker, MainThreadOnly};
    use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSString, NSURL};
    use objc2_ui_kit::{UIDocumentPickerDelegate, UIDocumentPickerViewController};

    // 保活 delegate：Objective-C 侧只弱引用 delegate，若不在这里持有，系统回调
    // （提示「已导出」）永远不会被触发，面板也会立刻失效。
    thread_local! {
        static DELEGATE: RefCell<Option<Retained<PickerDelegate>>> = const { RefCell::new(None) };
    }

    define_class! {
        // SAFETY:
        // - The superclass NSObject does not have any subclassing requirements.
        // - `PickerDelegate` does not implement `Drop`.
        #[unsafe(super = NSObject)]
        #[thread_kind = MainThreadOnly]
        struct PickerDelegate;

        // SAFETY: `NSObjectProtocol` has no safety requirements.
        unsafe impl NSObjectProtocol for PickerDelegate {}

        // SAFETY: `UIDocumentPickerDelegate` has no safety requirements.
        unsafe impl UIDocumentPickerDelegate for PickerDelegate {
            // SAFETY: The signature is correct.
            #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
            fn did_pick_documents_at_urls(&self, _controller: &UIDocumentPickerViewController, _urls: &NSArray<NSURL>) {
                show_message(tl!("exported")).ok();
            }
        }
    }

    impl PickerDelegate {
        // 手工构造：alloc + set_ivars + init，等价于 `[[PickerDelegate alloc] init]`；
        // 之所以不 derive 自动构造，是因为需要在主线程标记下手动声明 ivars。
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(());
            // Safety: `super(this)` 传的是尚未 `init` 的实例，`init` 是 NSObject 的标准初始化方法，
            // 调用后返回已初始化的对象，符合 objc2 对 `msg_send!` 的使用约定。
            unsafe { objc2::msg_send![super(this), init] }
        }
    }

    let mtm = MainThreadMarker::new().unwrap();

    let url = NSURL::fileURLWithPath(&NSString::from_str(&path));
    let urls = NSArray::from_retained_slice(&[url]);
    let picker = UIDocumentPickerViewController::alloc(mtm);
    // iOS 14 起才有「导出为副本」的初始化方法；老系统只能回退到已废弃的 `ExportToService`
    // 模式，因此那一段需要 `allow(deprecated)` 才能编译通过。
    let picker = if available!(ios = 14.0.0) {
        UIDocumentPickerViewController::initForExportingURLs_asCopy(picker, &urls, true)
    } else {
        #[allow(deprecated)]
        {
            use objc2_ui_kit::UIDocumentPickerMode;
            UIDocumentPickerViewController::initWithURLs_inMode(picker, &urls, UIDocumentPickerMode::ExportToService)
        }
    };
    let dlg_obj = PickerDelegate::new(mtm);
    picker.setDelegate(Some(ProtocolObject::from_ref(&*dlg_obj)));
    DELEGATE.with(|it| *it.borrow_mut() = Some(dlg_obj));

    // 必须从当前顶层 view controller 弹出：若此时已有别的模态在展示（拿不到顶层控制器），
    // 只能放弃并提示，否则面板会静默不出现，玩家会以为导出失败了。
    if let Some(controller) = inputbox::backend::IOS::get_top_view_controller(mtm) {
        controller.presentViewController_animated_completion(&picker, true, None);
    } else {
        show_error(Error::msg("Failed to present export dialog"));
    }
}

/// 向系统发起一次「另存为」请求，并把结果异步交回 [`take_export`]。
///
/// 这是一段**跨平台分支**：桌面端用 `rfd` 同步弹出保存对话框并直接拿到路径；Android/HarmonyOS
/// 交给原生层（Java / miniquad 回调）处理，文件句柄稍后经 `process_export_fd` 回填；iOS 则先把
/// 文件写进沙盒临时目录，等打包完成后由 [`resolve_export`] 拉起系统面板交给用户。
///
/// 调用方只需给出建议文件名；无论成功失败都会在 `EXPORT_CONFIG` 里留下 `Some(...)`，
/// 由页面下一帧取走，因此本函数不返回任何结果、也不阻塞。
pub fn request_export(suggested_name: String) {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "android")] {
            // Android：请求 Java 层（QuadNative.showExportDialog）弹出系统「另存为」。
            // 这里只发出请求：真正的文件句柄由 Java 侧回调 process_export_fd 回填。
            // Safety: 通过 miniquad 附加到当前 JVM 线程后再调用 JNI；方法名/签名与 Java 侧
            // 常量保持一致，`NewStringUTF` 的入参在本调用期间保持有效，符合 JNI 约定。
            unsafe {
                let env = miniquad::native::attach_jni_env();
                let ctx = ndk_context::android_context().context();
                let class = (**env).GetObjectClass.unwrap()(env, ctx);
                let method =
                    (**env).GetMethodID.unwrap()(env, class, c"showExportDialog".as_ptr() as _, c"(Ljava/lang/String;)V".as_ptr() as _);
                let url = std::ffi::CString::new(suggested_name).unwrap();
                (**env).CallVoidMethod.unwrap()(
                    env,
                    ctx,
                    method,
                    (**env).NewStringUTF.unwrap()(env, url.as_ptr()),
                );
            }
        } else if #[cfg(target_os = "ios")] {
            use objc2_foundation::NSTemporaryDirectory;

            // iOS：先在沙盒临时目录里创建文件，写入过程由导出线程完成；
            // 清理回调顺手清空 `EXPORT_PICKER_PATH`，防止系统面板去分享一个已被删掉的文件。
            let dir = NSTemporaryDirectory();
            let output_path = std::path::PathBuf::from(dir.to_string()).join(&suggested_name);
            let output_path_str = output_path.to_string_lossy().to_string();
            let config = File::create(&output_path).map(|file| {
                let delete_path = output_path.clone();
                ExportConfig {
                    file,
                    deleter: Box::new(move || {
                        *EXPORT_PICKER_PATH.lock().unwrap() = None;
                        std::fs::remove_file(delete_path)
                    }),
                }
            });
            // 只有文件创建成功才登记待分享路径，否则会让面板去分享一个不存在的文件。
            if config.is_ok() {
                EXPORT_PICKER_PATH.lock().unwrap().replace(output_path_str);
            }
            EXPORT_CONFIG.lock().unwrap().replace(config);
        } else if #[cfg(target_env = "ohos")] {
            // HarmonyOS：把请求转交给原生侧，文件描述符同样经回调（`process_export_fd_ohos`）回填。
            miniquad::native::call_request_callback(format!("{{\"action\":\"request_export\",\"filename\":\"{}\"}}", suggested_name));
        } else {
            // 桌面端：同步弹出系统保存对话框，玩家取消时什么也不做——
            // 于是 `EXPORT_CONFIG` 保持为空，页面下一帧取不到结果，导出流程自然终止。
            if let Some(output_path) = rfd::FileDialog::new().set_title(tl!("multi-export-title")).set_file_name(&suggested_name).save_file() {
                let config = File::create(&output_path).map(|file| ExportConfig {
                    file,
                    deleter: Box::new(move || std::fs::remove_file(output_path)),
                });
                EXPORT_CONFIG.lock().unwrap().replace(config);
            }
        }
    }
}

// 导出握手的第二步与第三步：`take_*` 是「取出待处理的结果」（消费一次），
// `resolve_*` 是「回执」（告诉平台侧文件已经写好，可以交给用户了）。
// 两者分开是因为中间隔着一次可能耗时数秒的压缩过程：先 take 拿到句柄开始写，
// 写完再 resolve 通知系统；若打包失败则既不 resolve，还会调用 `deleter` 清理半成品。
//
/// 取出系统返回的导出目标（`None` = 玩家还没选好文件，或已取消）。
/// 返回 `Some(Err(_))` 表示系统侧申请文件失败，调用方应直接把错误提示给玩家。
pub fn take_export() -> Option<io::Result<ExportConfig>> {
    EXPORT_CONFIG.lock().unwrap().take()
}

/// 打包成功后的回执：在 iOS 上拉起系统文档面板让玩家取走文件，其它平台只提示「已导出」。
pub fn resolve_export() {
    // iOS：待分享路径还在（清理回调尚未执行）说明文件写好且已登记，拉起系统文档面板；
    // 否则退化为普通提示，避免面板去分享一个不存在的路径。
    #[cfg(target_os = "ios")]
    {
        if let Some(path) = EXPORT_PICKER_PATH.lock().unwrap().clone() {
            present_export_picker(path);
        } else {
            show_message(tl!("exported")).ok();
        }
    }
    // 其余平台：文件已经直接写到玩家选定的位置（或已交给原生侧），只需提示完成。
    #[cfg(not(target_os = "ios"))]
    show_message(tl!("exported")).ok();
}

// Android 专用：删除由 SAF（系统文件框架）创建的文档。
//
// Rust 侧拿到的是 `Uri` 而不是普通路径，无法用 `std::fs` 删除，必须回调 Java 侧的
// `deleteUri`。仅在导出失败时（通过 `ExportConfig::deleter`）调用，用于回收半成品文件。
#[cfg(target_os = "android")]
fn delete_uri(uri: Global<JObject<'static>>) {
    // 这里的 `unwrap` 依赖「Android 上 JVM 在进程启动阶段就已初始化」这一前提；
    // 失败会 panic，而这是一条清理路径，调用方（导出失败分支）本就在尽力回收资源，
    // 因此不再向上层层上报错误。
    JavaVM::singleton()
        .unwrap()
        .attach_current_thread(|env| -> jni::errors::Result<()> {
            let ctx = ndk_context::android_context().context();
            // Safety: `ndk_context` 持有的 Activity 全局引用在进程存活期间始终有效，
            // 这里只借用它调用方法，不接管所有权，因此不会产生悬垂引用或双重释放。
            let ctx = unsafe { JObject::from_raw(env, ctx as _) };
            env.call_method(ctx, jni_str!("deleteUri"), jni_sig!("(Landroid/net/Uri;)V"), &[uri.as_ref().into()])?;
            Ok(())
        })
        .unwrap();
}

/// Android 原生侧回调：Java 的 `QuadNative` 在玩家选好保存位置后调用（JNI 导出符号），
/// 把文件描述符交回 Rust。
///
/// 实现要点：把 `fd` 包装成 [`File`]（由 Rust 负责关闭）、持有 `Uri` 的全局引用以便失败时删除，
/// 然后写入 `EXPORT_CONFIG` 等页面下一帧取走。这里必须尽快返回——真正的压缩在主线程之外的
/// 导出线程里做，不能阻塞 Java 的调用栈。
#[cfg(target_os = "android")]
#[export_name = "Java_quad_1native_QuadNative_processExportFd"]
extern "system" fn process_export_fd(mut env: EnvUnowned, _: jni::objects::JClass, uri: jni::objects::JObject, fd: jni::sys::jint) {
    use std::os::fd::FromRawFd;
    env.with_env(|env| -> jni::errors::Result<()> {
        let uri = env.new_global_ref(uri)?;
        // Safety: `fd` 由 Java 侧传入且只在本次调用中有效，`from_raw_fd` 取得其所有权后
        // 由 `File` 负责关闭，保证不会被重复关闭。
        let file = unsafe { File::from_raw_fd(fd as _) };
        EXPORT_CONFIG.lock().unwrap().replace(Ok(ExportConfig {
            file,
            deleter: Box::new(|| {
                delete_uri(uri);
                Ok(())
            }),
        }));
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// HarmonyOS 侧的导出入口：与 Android 的 [`process_export_fd`] 对应，由原生层回调。
/// 清理回调是空实现——文件由系统侧管理，Rust 不做删除。
#[cfg(target_env = "ohos")]
mod ohos_export {
    use super::*;
    use napi_derive_ohos::napi;
    // `#[napi]` 会生成胶水代码把本函数导出给原生侧调用，函数名需与原生侧约定一致。
    #[napi]
    #[allow(dead_code)]
    pub fn process_export_fd_ohos(fd: u32) {
        use std::os::fd::FromRawFd;
        // Safety: `fd` 由原生侧移交所有权，包装成 `File` 后由 Rust 负责关闭。
        let file = unsafe { File::from_raw_fd(fd as _) };
        EXPORT_CONFIG.lock().unwrap().replace(Ok(ExportConfig {
            file,
            deleter: Box::new(|| Ok(())),
        }));
    }
}

// [`Page`] 实现：把上面那套状态机接入主场景的页面调度。
//
// 各钩子的行为约定：
// - `label`：底栏显示的标题；
// - `enter`：仅在「从别的页面回到曲库」时调用，负责消费 [`FAV_UPDATED`] 并重排本地列表；
// - `on_result`：子页面返回时把结果转交列表视图（`bool` 表示该谱面是否已被删除，用于播放退场动画）；
// - `touch`：按「忙 → 弹窗 → 列表 → 标签页 → 分页 → 工具按钮 → 多选 → 排序」的优先级分发；
// - `update`：集中消费所有异步任务、全局标志与输入框/菜单结果（页面唯一的副作用汇聚点）；
// - `render` / `render_top`：绘制列表、工具条与遮罩层；
// - `next_page` / `next_scene`：把跳转请求交给调度器，或由列表视图给出要进入的场景。
//
// 只要还有任何异步任务在跑（见 `touch` 开头的判断），本页就整体进入「忙」状态并吞掉触摸，
// 这是本页最重要的不变量：任何一次触摸都不会在数据被后台改写的中途生效。
impl Page for LibraryPage {
    /// 底栏标题。
    fn label(&self) -> Cow<'static, str> {
        tl!("label")
    }

    /// 每次回到曲库页时调用：若期间有别的页面改动过收藏夹（置位了 [`FAV_UPDATED`]），
    /// 这里取走并清位，然后重排本地列表。`swap` 保证只处理一次，不会重复刷新。
    fn enter(&mut self, s: &mut SharedState) -> Result<()> {
        if FAV_UPDATED.swap(false, Ordering::SeqCst) {
            self.sync_local(s);
        }
        Ok(())
    }

    /// 接收子页面（如歌曲详情页）返回的结果。
    ///
    /// 只识别 `bool` 类型：表示玩家是否在详情页删除了这张谱面；是则由列表视图播放退场动画
    /// （而不是直接消失）。其它类型的结果本页不关心，原样忽略（`_res` 仅用于保持所有权）。
    fn on_result(&mut self, res: Box<dyn Any>, s: &mut SharedState) -> Result<()> {
        let _res = match res.downcast::<bool>() {
            Err(res) => res,
            Ok(delete) => {
                self.tabs.selected_mut().view.on_result(s.t, *delete);
                return Ok(());
            }
        };
        Ok(())
    }

    /// 触摸分发。返回 `true` 表示本次事件已被本页消费，主场景不再向其它页面传递。
    ///
    /// 分发顺序即优先级：忙状态 → 模态弹窗 → 列表 → 标签页 → 分页 → 工具按钮 → 多选工具 →
    /// 排序。任何一步返回 `true` 都会直接结束，因此下层控件只会在上层未命中时收到事件。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        let t = s.t;
        // 第一阶段：忙检查。以下任务都会改写列表/收藏夹内容，期间一律吞掉输入避免竞态。
        // 注意云端列表请求（`online_task`）刻意**不在**其列：它只是整体替换列表内容，
        // 玩家仍可继续翻页或改筛选（新请求会直接覆盖旧的）。
        if self.sync_fav_task.is_some()
            || self.export_task.is_some()
            || self.multi_create_fav_task.is_some()
            || self.manage_fav_pre_task.is_some()
            || self.manage_fav_task.is_some()
            || self.refresh_local_fav_task.is_some()
        {
            return Ok(true);
        }
        // 第二阶段：模态弹窗优先。但「选封面」模式例外——那时弹窗（若有）不应拦截点击，
        // 因为玩家此刻的目标是在列表里选中一张谱面。
        let choose_cover = CHOOSE_COVER.load(Ordering::Relaxed);
        if !choose_cover {
            if self.order_menu.showing() {
                self.order_menu.touch(touch, t);
                return Ok(true);
            }
            if self.order_meta_menu.showing() {
                self.order_meta_menu.touch(touch, t);
                return Ok(true);
            }
            if self.manage_fav_menu.showing() {
                self.manage_fav_menu.touch(touch, t);
                return Ok(true);
            }
            if self.multi_operation_menu.showing() {
                self.multi_operation_menu.touch(touch, t);
                return Ok(true);
            }
            if self.multi_select_menu.showing() {
                self.multi_select_menu.touch(touch, t);
                return Ok(true);
            }
            if self.tags.touch(touch, t) {
                return Ok(true);
            }
            if self.rating.touch(touch, t) {
                return Ok(true);
            }
        }
        // 第三阶段：列表本体。卡片点击、长按菜单、滚动、下拉刷新都在 [`ChartsView`] 内部处理，
        // 它优先于标签页与工具条，保证在列表区域内拖动不会被误判成切换标签页。
        let charts_view = &mut self.tabs.selected_mut().view;
        // 切换页面/进入歌曲的过渡动画期间吞掉输入，避免连点导致跳转两次或状态错乱。
        if charts_view.transiting() {
            return Ok(true);
        }
        if charts_view.touch(touch, t, s.rt)? {
            return Ok(true);
        }
        // 选封面模式到此为止：只允许点中卡片（其结果由 ChartsView 写入 `CHOSEN_COVER`），
        // 不响应标签页、工具条等任何其它控件。
        if choose_cover {
            return Ok(true);
        }
        // 第四阶段：标签页切换。
        if self.tabs.touch(touch, s.rt) {
            return Ok(true);
        }
        // 第五阶段：云端列表的分页。边界判断放在这里（而非禁用按钮），既省一次渲染状态同步，
        // 也保证永远不会请求越界页码。
        if !matches!(self.tabs.selected().ty, ChartListType::Local) {
            if self.prev_page_btn.touch(touch, t) {
                if self.current_page != 0 {
                    self.current_page -= 1;
                    self.load_online();
                }
                return Ok(true);
            }
            if self.next_page_btn.touch(touch, t) {
                if self.current_page + 1 < self.total_page() {
                    self.current_page += 1;
                    self.load_online();
                }
                return Ok(true);
            }
        }

        // 第六阶段：按标签页类型分派工具按钮。热门榜没有任何工具按钮
        // （它既不支持搜索/排序/筛选，也不展示分页）。
        match self.tabs.selected().ty {
            ChartListType::Local => {
                // 多选进行中时隐藏「导入」与「收藏夹」：这两个操作都会把玩家带去别处，
                // 与正在进行的批量操作冲突。
                if self.tabs.selected().view.multi_select.is_none() {
                    if self.import_btn.touch(touch, t) {
                        // 这里只发起系统文件选择：真正的解压、校验、写入
                        // `data/charts/custom/<uuid>` 与生成 `info.yml` 都由 `MainScene` 完成，
                        // 完成后置位 `NEED_UPDATE` 通知本页重扫本地谱面（见 `update` 末段）。
                        request_file("_import");
                        return Ok(true);
                    }
                    // 打开收藏夹页（Overlay），并把当前过滤的收藏夹带过去以便高亮；
                    // 第四参 `None` 表示「不是以选择封面为目的进入的」。
                    if self.fav_btn.touch(touch, t) {
                        self.next_page = Some(NextPage::Overlay(Box::new(FavoritesPage::new(
                            self.icons.clone(),
                            self.rank_icons.clone(),
                            self.current_fav_index,
                            None,
                        ))));
                        return Ok(true);
                    }
                }
                // 清除搜索：本地列表可以立即重算，无需网络。先判非空是因为按钮矩形在有内容时
                // 才会被设置，否则会命中一个过期的区域。
                if !self.search_str.is_empty() && self.search_clr_btn.touch(touch) {
                    button_hit();
                    self.search_str.clear();
                    self.sync_local(s);
                    return Ok(true);
                }
                // 排除清除按钮的区域，否则点「×」会同时触发输入框弹出。
                // 输入走系统输入框，结果由 `take_input` 在 `update` 中取回（见那里的 `"search"` 分支）。
                if !self.search_clr_btn.contains(touch.position) && self.search_btn.touch(touch, t) {
                    request_input("search", InputBox::new().default_text(&self.search_str));
                    return Ok(true);
                }
            }
            ChartListType::Ranked | ChartListType::Special | ChartListType::Unstable => {
                // 云端三类列表共用同一套工具：搜索、筛选。搜索/筛选变化都要先回到第 1 页，
                // 否则会停留在「旧条件对应的页码」上。
                if !self.search_str.is_empty() && self.search_clr_btn.touch(touch) {
                    button_hit();
                    self.search_str.clear();
                    self.current_page = 0;
                    self.load_online();
                    return Ok(true);
                }
                if !self.search_clr_btn.contains(touch.position) && self.search_btn.touch(touch, t) {
                    request_input("search", InputBox::new().default_text(&self.search_str));
                    return Ok(true);
                }
                // 同一个「筛选」按钮负责两个对话框：`filter_show_tag` 记忆上次用的是哪个，
                // 两个对话框内部也可以互相切换（见 `update` 里的 `show_rating` / `show_tags`）。
                if self.filter_btn.touch(touch, t) {
                    if self.filter_show_tag {
                        self.tags.enter(t);
                    } else {
                        self.rating.enter(t);
                    }
                    return Ok(true);
                }
            }
            // 热门榜没有可交互的工具按钮：它既不接受筛选，也不翻页。
            ChartListType::Popular => {}
        }
        // 第七阶段：多选工具条（仅在列表已进入多选模式时存在）。
        if self.tabs.selected_mut().view.multi_select.is_some() {
            if self.multi_operation_btn.touch(touch, t) {
                // 菜单项按「列表当前是否可编辑」动态拼装：只有本地列表、无搜索过滤、
                // 且当前收藏夹可写时才追加「删除」（该标志由 `update` 每帧刷新）。
                let mut options = vec!["multi-export", "multi-create-fav", "multi-manage-fav"];
                if self.tabs.selected_mut().view.allow_edit {
                    options.push("multi-delete");
                }
                self.multi_operation_menu
                    .set_options(options.iter().map(|it| tl!(*it).into_owned()).collect());
                // 记下 i18n key 本身而不是显示文本，`update` 里才能不依赖语言做分支判断。
                self.multi_operation_options = options;
                self.need_show_multi_operation_menu = true;
                return Ok(true);
            }
            if self.multi_select_btn.touch(touch, t) {
                self.need_show_multi_select_menu = true;
                return Ok(true);
            }
            // 退出多选：只清空选择集合，不动列表数据。
            if self.multi_select_cancel_btn.touch(touch, t) {
                self.tabs.selected_mut().view.multi_select = None;
                return Ok(true);
            }
        }
        // 第八阶段：排序按钮，先拉出「字段 + 升降序」两级菜单中的第一级。
        if self.order_btn.touch(touch, t) {
            self.need_show_order_meta_menu = true;
            return Ok(true);
        }
        Ok(false)
    }

    /// 每帧逻辑：本页所有副作用都在这里汇聚。
    ///
    /// 按顺序做四类工作：① 消费全局标志与线程本地通道（选封面结果、收藏夹页回传、输入框结果）；
    /// ② 处理各菜单/对话框的选中项（这些属于「用户刚做的决定」，应在刷新列表前生效）；
    /// ③ 轮询所有在途任务并把结果落盘、刷新视图；④ 推导每帧状态（如列表能否编辑）。
    ///
    /// 顺序是有意为之：先消费外部结果、再改数据、最后刷新列表，可避免同一帧里用旧数据做判断；
    /// 而 `load_online` 之类的请求是异步的，本帧只是把任务挂上，结果要到后续帧才落地。
    ///
    /// # Errors
    /// 收藏夹落盘失败等 I/O 错误会向上抛出，由主场景统一提示。
    fn update(&mut self, s: &mut SharedState) -> Result<()> {
        let t = s.t;

        // ①-A：「选封面」结果。玩家在列表里点中一张谱面后，ChartsView 把它写进
        // `CHOSEN_COVER`；这里取走结果、清掉模式标志，并重新构造收藏夹页让玩家看到封面已应用。
        if let Some(chosen_cover) = CHOSEN_COVER.with(|it| it.borrow_mut().take()) {
            CHOOSE_COVER.store(false, Ordering::Relaxed);
            self.next_page = Some(NextPage::Overlay(Box::new(FavoritesPage::new(
                self.icons.clone(),
                self.rank_icons.clone(),
                self.current_fav_index,
                Some(chosen_cover),
            ))));
        }

        // ①-B：收藏夹页回传的「当前选中收藏夹」。
        self.check_fav_page(s);

        // ② 修正非法组合：本地列表没有评分数据，因此切到本地时若仍停留在「按评分排序」，
        // 就悄悄回退到默认排序（并恢复默认的降序），避免列表看起来毫无变化却排序失效。
        if self.tabs.selected().ty == ChartListType::Local && self.current_order == ChartOrder::Rating {
            self.current_order = ChartOrder::Default;
            self.order_rev = true;
        }

        self.tags.update(t);
        self.rating.update(t);

        // ②-B：标签页切换。需要做三件清理：回到顶部（每个标签页各自的滚动位置都会丢失）、
        // 清空**所有**标签页的多选状态（否则切回来时残留的选中项会指向已经不存在的条目）、
        // 丢弃在途的云端请求（结果属于旧标签页，不能被写进新列表）。
        let is_local = self.tabs.selected().ty == ChartListType::Local;
        if self.tabs.changed() {
            self.tabs.selected_mut().view.reset_scroll();
            self.tabs.iter_mut().for_each(|it| it.view.multi_select = None);
            self.online_task = None;
            if is_local {
                self.sync_local(s);
            } else {
                self.current_page = 0;
                self.load_online();
            }
        }
        // ②-C：`closed` 构建专属的「合集」占位卡被点击。合集页需要先请求数据，
        // 因此用异步任务构造页面，而不是像收藏夹页那样直接构造。
        if cfg!(closed) && self.tabs.selected_mut().view.clicked_special {
            let icons = Arc::clone(&self.icons);
            self.next_page_task = Some(Box::pin(async move { Ok(NextPage::Overlay(Box::new(CollectionPage::new(icons).await?))) }));
            self.tabs.selected_mut().view.clicked_special = false;
        }
        // ②-D：只有当异步构造完成（或失败）后才设置跳转目标，保证 `next_page()` 拿到的要么是
        // 完整页面、要么是默认值，不会出现「跳转过去了但页面还没准备好」。
        if let Some(task) = &mut self.next_page_task {
            if let Some(res) = poll_future(task.as_mut()) {
                self.next_page = Some(res?);
                self.next_page_task = None;
            }
        }

        // ②-E：两个筛选对话框的互相跳转与「关闭即重查」。
        // `show_rating`/`show_tags` 是对话框内部的「切换到另一个对话框」请求；
        // 而 `tags_last_show`/`rating_last_show` 记录上一帧的可见性，用于做**沿检测**：
        // 只有在「上一帧还开着、这一帧关掉了」时才重新请求列表，避免每帧都发一次请求。
        if self.tags.show_rating {
            self.tags.show_rating = false;
            self.filter_show_tag = false;
            self.rating.enter(t);
        } else if self.tags_last_show && !self.tags.showing() {
            self.current_page = 0;
            self.load_online();
        }
        if self.rating.show_tags {
            self.rating.show_tags = false;
            self.filter_show_tag = true;
            self.tags.enter(t);
        } else if self.rating_last_show && !self.rating.showing() {
            self.current_page = 0;
            self.load_online();
        }
        self.tags_last_show = self.tags.showing();
        self.rating_last_show = self.rating.showing();
        // ③-A：云端请求回执。总页数只在成功时更新；失败只提示错误、不改动列表，
        // 于是玩家看到的是空列表 + 错误提示，而不是「上一批数据配新页码」的错位状态。
        if let Some(task) = &mut self.online_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err.context(tl!("failed-to-load-online"))),
                    Ok(res) => {
                        self.online_total_page = res.2;
                        self.tabs.selected_mut().view.set(t, res.0);
                    }
                }
                self.online_task = None;
            }
        }
        // ③-B：推进菜单动画，并让本地谱面的插画缩略图完成异步加载（`settle` 会把刚解码好的
        // 贴图换进正在显示的那个句柄，因此这里必须每帧调用，否则缩略图会永远停在占位图）。
        self.order_menu.update(t);
        self.order_meta_menu.update(t);
        self.multi_operation_menu.update(t);
        self.multi_select_menu.update(t);
        for chart in &mut s.charts_local {
            chart.illu.settle(t);
        }
        // ③-C：下拉刷新的可用性。本地「全部谱面」列表没有服务端数据可拉，因此禁用；
        // 但本地**收藏夹视图**可以刷新——其中的云端引用需要重新同步。云端标签页始终允许。
        self.tabs.selected_mut().view.can_refresh = !is_local || self.current_fav_index.is_some();
        // `view.update` 返回 `true` 表示玩家触发了下拉刷新，需要按标签页类型重新装载数据。
        if self.tabs.selected_mut().view.update(t)? {
            if is_local {
                if let Some(index) = self.current_fav_index {
                    let data = get_data();
                    let uuid = data.collection_uuids()[index];
                    let col = data.collection_by_index(index);
                    // 有服务端 id 的收藏夹：整份拉取云端内容回来 merge，是真正的双向同步。
                    if let Some(col_id) = col.id {
                        if !data.config.offline_mode {
                            self.sync_fav_task = Some(Task::new(async move {
                                let resp: Collection = recv_raw(Client::get(format!("/collection/{col_id}"))).await?.json().await?;
                                Ok(Some(resp))
                            }));
                        }
                    // 纯本地收藏夹（无 id）：无法整体同步，只能把里面的谱面按 id 批量反查元信息，
                    // 用来更新名称/难度/封面临时地址等展示字段。
                    } else {
                        let charts = col.charts.clone();
                        self.refresh_local_fav_task = Some(Task::new(async move {
                            // 拼出逗号分隔的 id 列表（末尾多一个逗号，稍后 pop 掉）；
                            // 若一个 id 都没有（整夹都是纯本地谱面），下面会跳过网络请求，
                            // 直接返回原列表，避免发一次无意义的空查询。
                            let mut ids_str = String::new();
                            for chart in &charts {
                                if let Some(id) = chart.id() {
                                    ids_str.push_str(&id.to_string());
                                    ids_str.push(',');
                                }
                            }
                            let mut updated = charts;
                            if !ids_str.is_empty() {
                                ids_str.pop();
                                let resp: Vec<Chart> = recv_raw(Client::get(format!("/chart/multi-get?ids={ids_str}"))).await?.json().await?;
                                let mut id_to_chart = HashMap::new();
                                for chart in resp {
                                    id_to_chart.insert(chart.id, chart);
                                }
                                for chart in &mut updated {
                                    if let Some(id) = chart.id() {
                                        if let Some(info) = id_to_chart.get(&id) {
                                            chart.info = Some(Box::new(ChartRefChartInfo::from_chart(info)));
                                        }
                                    }
                                }
                            }
                            Ok((uuid, updated))
                        }));
                    }
                }
            } else {
                // 云端标签页：下拉刷新等同于「重新请求当前页」。
                self.load_online();
            }
        }
        // ③-D：`NEED_UPDATE` 是跨模块的一次性广播（`MainScene` 导入谱面完成、歌曲详情页删除
        // 谱面等都会置位）。这里重扫磁盘并重排本地列表；读取时同时消费掉标志，
        // 因此同一个事件不会触发第二次重扫。
        if self.tabs.selected_mut().view.need_update() {
            s.reload_local_charts();
            self.sync_local(s);
        }
        // ①-C：系统输入框的结果。`id` 用于区分「谁请求的输入」：本页自己发起的
        // `"search"` / `"new_fav"` 在这里消费，其余一律 `return_input` 交还上层——
        // 这样嵌套的子页面（如收藏夹页）也能共用同一个全局输入框而不互相抢结果。
        if let Some((id, text)) = take_input() {
            if id == "search" {
                // 输入框是「确认后才回来」的，不需要额外的防抖：本地列表立即重算，
                // 云端列表归零页码后重新请求（否则会停留在旧条件对应的页码上）。
                self.search_str = text;
                if is_local {
                    self.sync_local(s);
                } else {
                    self.current_page = 0;
                    self.load_online();
                }
            } else if id == "new_fav" {
                // 名称校验：空名与不合规文本都在这里拦下并提示，避免把注定失败的请求发到服务端。
                if text.is_empty() {
                    use crate::page::favorites::{tl as ftl, L10N_LOCAL};
                    show_message(ftl!("name-empty")).error();
                } else if let Err(err) = crate::censor::check_text(&text) {
                    show_message(err.to_string()).error();
                } else {
                    let charts_view = &mut self.tabs.selected_mut().view;
                    // 只有确实存在多选时才创建任务：否则连「加载中」遮罩都不会出现。
                    if let Some(mut selected) = charts_view.multi_select.clone() {
                        self.multi_create_fav_task = Some(Task::new(async move {
                            // 多选里的引用可能只带云端 id（例如来自云端标签页），
                            // 必须在写入**本地**收藏夹前批量补全元信息，否则离线时这张卡片
                            // 将无法显示名称与插画。
                            let mut ids_str = String::new();
                            for chart in &selected {
                                if let Some(id) = chart.id() {
                                    ids_str.push_str(&id.to_string());
                                    ids_str.push(',');
                                }
                            }
                            if !ids_str.is_empty() {
                                ids_str.pop();
                                let resp: Vec<Chart> = recv_raw(Client::get(format!("/chart/multi-get?ids={ids_str}"))).await?.json().await?;
                                let mut id_to_chart = HashMap::new();
                                for chart in resp {
                                    id_to_chart.insert(chart.id, Box::new(chart));
                                }
                                for chart in &mut selected {
                                    if let Some(id) = chart.id() {
                                        // 隐含不变量：multi-get 会返回请求里存在的全部 id；
                                        // 若服务端遗漏某个 id，这里会 panic（而不是静默丢数据）。
                                        chart.info = Some(Box::new(ChartRefChartInfo::from_chart(id_to_chart.get(&id).unwrap())));
                                    }
                                }
                            }
                            Ok(CreateFavorite {
                                name: text,
                                charts: selected,
                            })
                        }));
                    }
                }
            } else {
                // 不是本页请求的输入，交还上层（见本分支开头的说明）。
                return_input(id, text);
            }
        }
        // ③-E：「新建收藏夹」完成。成功后才清空多选并切换过去；
        // 失败只提示错误、保持原状，玩家的选择不会丢。
        if let Some(task) = &mut self.multi_create_fav_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err),
                    Ok(result) => {
                        self.tabs.selected_mut().view.multi_select = None;
                        let data = get_data_mut();
                        let mut col = LocalCollection::new(result.name);
                        col.charts = result.charts;
                        data.push_collection(col)?;
                        let _ = save_data();
                        show_message(tl!("fav-created")).ok();
                        // 新收藏夹总是追加在末尾，因此下标可以直接算出来；
                        // 切换过去并重排列表，让玩家立刻看到新建的结果。
                        self.current_fav_index = Some(data.collection_uuids().len() - 1);
                        self.sync_local(s);
                    }
                }
                self.multi_create_fav_task = None;
            }
        }
        // ③-F：多选删除的确认回执（对话框跨帧，用共享标志传结果，`swap` 保证只执行一次）。
        // 执行顺序是「先删磁盘目录、再删数据记录、最后落盘」：目录已不存在（`NotFound`）被视为
        // 成功以保证幂等；中途出错则宁可留下一条指向不存在目录的记录，也不产生
        // 「目录还在但记录已丢」的孤儿数据。
        if self.delete_multi.swap(false, Ordering::Relaxed) {
            // 能走到这里必然处于多选模式（删除项只在多选菜单里出现），因此 `unwrap` 安全。
            let selected = self.tabs.selected_mut().view.multi_select.take().unwrap();
            let selected = selected.into_iter().collect::<HashSet<_>>();
            let data = get_data_mut();
            let mut local_paths = HashSet::new();
            for chart in &selected {
                // 只有真正存在于本机的谱面才有目录可删；仅云端的引用会被跳过。
                if let Some(path) = chart.find_local_path()? {
                    match std::fs::remove_dir_all(format!("{}/{path}", dir::charts()?)) {
                        Ok(_) => {}
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                        Err(err) => return Err(err.into()),
                    }
                    local_paths.insert(path);
                }
            }
            data.charts.retain(|it| !local_paths.contains(it.local_path.as_str()));
            let _ = save_data();
            show_message(tl!("multi-deleted")).ok();
            // 磁盘内容已变，必须重扫并重排（而不是就地删卡片），否则缩略图与磁盘状态会不一致。
            s.reload_local_charts();
            self.sync_local(s);
        }
        // ②-F：排序一级菜单。第 0 项不是「选定」而是「进入下一级」，所以它只置位延时标志，
        // 真正的弹出发生在 `render`（那里才有当帧算出的按钮矩形可用于定位二级菜单）。
        // 第 1 项翻转升降序并立即生效；`set_selected(usize::MAX)` 是 Popup 约定的「无选中项」，
        // 用于清掉高亮，避免下次打开时仍显示上一次的选择。
        if self.order_meta_menu.changed() {
            match self.order_meta_menu.selected() {
                0 => {
                    self.need_show_order_menu = true;
                }
                1 => {
                    self.order_rev = !self.order_rev;
                    self.update_order_meta_menu_options();
                    self.order_meta_menu.set_selected(usize::MAX);
                    self.on_order_update(s);
                }
                _ => {}
            }
        }
        // ②-G：排序字段二级菜单。切换字段时顺带把方向重置成该字段的惯用方向
        // （`Default`=最近更新、`Rating`=评分从高到低，因此都是降序），
        // 否则玩家会看到「按名称降序」这类不太符合直觉的默认结果。
        if self.order_menu.changed() {
            self.current_order = self.order_menu_options[self.order_menu.selected()];
            self.order_rev = matches!(self.current_order, ChartOrder::Default | ChartOrder::Rating);
            self.order_meta_menu.set_selected(usize::MAX);
            self.update_order_meta_menu_options();
            self.on_order_update(s);
        }
        // ②-H：多选操作菜单。分支用的是 i18n key（在 `touch` 里存下的 `multi_operation_options`），
        // 而不是本地化后的显示文本，因此换语言不会走错分支。
        if self.multi_operation_menu.changed() {
            let charts_view = &mut self.tabs.selected_mut().view;
            // 菜单只可能在多选模式下弹出，因此这里可以安全地解引用。
            let selected = charts_view.multi_select.as_mut().unwrap();
            match self.multi_operation_options[self.multi_operation_menu.selected()] {
                "multi-export" => {
                    self.multi_operation_menu.dismiss(t);
                    // 导出前先逐个确认谱面确实存在于本机：只有云端 id 的条目没有目录可打包。
                    let mut paths = Vec::with_capacity(selected.len());
                    let mut non_existent = Vec::new();
                    for chart in selected {
                        match chart.find_local_path()? {
                            Some(path) => paths.push(path.into_owned()),
                            None => {
                                let mut charts = charts_view.charts.as_ref().unwrap().iter().filter_map(|it| it.chart.as_ref());
                                non_existent.push(charts.find(|it| &it.to_bare_ref() == chart).unwrap().info.name.clone());
                            }
                        }
                    }
                    // 只要有任何一个条目没装在本机，就整体放弃本次导出并列出名字——
                    // 而不是静默地只导出能导出的那些，否则玩家会误以为全都导出了。
                    if !non_existent.is_empty() {
                        Dialog::simple(tl!("multi-export-no-file", "charts" => non_existent.join(", "))).show();
                    } else {
                        // 全部就绪：只记下待导出目录并发起系统「另存为」。真正的压缩要等拿到
                        // 文件句柄（`take_export`）之后，可能还要过若干帧。文件名带时间戳以免重名。
                        self.export_paths = Some(paths);
                        request_export(format!("phira-export-{}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S")));
                    }
                }
                // 新建收藏夹要先输入名称：菜单立刻收起，等输入框结果回来再启动任务
                // （结果由 `update` 的 `take_input` 分支消费）。
                "multi-create-fav" => {
                    self.multi_operation_menu.dismiss(t);
                    request_input("new_fav", InputBox::new());
                }
                "multi-manage-fav" => {
                    if let Some(options) = self.get_move_fav_menu_options() {
                        // 同样是一帧延迟：勾选标记要基于选中谱面计算，随后在 `render` 里弹出。
                        self.need_show_manage_fav_menu = true;
                        self.manage_fav_menu.set_options(options);
                        self.multi_operation_menu.set_selected(usize::MAX);
                    } else {
                        // Do nothing
                        // 没有多选或没有可写收藏夹时静默忽略：不弹空菜单。
                    }
                }
                // 删除不可撤销，因此走二次确认对话框，结果经 `delete_multi` 回传。
                "multi-delete" => {
                    self.multi_operation_menu.dismiss(t);
                    confirm_dialog(ttl!("del-confirm"), tl!("multi-delete-confirm", "count" => selected.len()), self.delete_multi.clone());
                }
                _ => {}
            }
        }
        // ②-I：全选 / 反选。占位卡（`chart` 为 `None`，如合集入口）永远不参与选择，
        // 因此两处都跳过它。
        if self.multi_select_menu.changed() {
            let charts_view = &mut self.tabs.selected_mut().view;
            let sel = charts_view.multi_select.as_mut().unwrap();
            let charts = charts_view.charts.as_ref().unwrap();
            match self.multi_select_menu.selected() {
                0 => {
                    sel.clear();
                    sel.extend(charts.iter().filter_map(|it| it.chart.as_ref()).map(ChartItem::to_bare_ref));
                }
                1 => {
                    // 反选：以当前选择为基准，把列表里尚未选中的补进来。
                    // 先 `mem::take` 取走旧集合，避免同时不可变借用 `charts` 与可变借用 `sel`。
                    let old_sel = mem::take(sel).into_iter().collect::<HashSet<_>>();
                    for chart in charts {
                        if let Some(chart) = &chart.chart {
                            let r = chart.to_bare_ref();
                            if !old_sel.contains(&r) {
                                sel.push(r);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // ②-J：移动到收藏夹。所点项的勾选状态取反即本次操作方向（已全在夹内 → 移除，否则加入）。
        // 菜单刻意不关闭也不高亮（`set_selected(usize::MAX)`），方便玩家连续对多个收藏夹操作。
        if self.manage_fav_menu.changed() {
            let (uuid, all_in_fav) = self.manage_fav_menu_options[self.manage_fav_menu.selected()];
            self.manage_fav_menu.set_selected(usize::MAX);
            let add = !all_in_fav;
            let mut charts = self.tabs.selected().view.multi_select.as_ref().unwrap().clone();
            // 先起一个「准备任务」：加入操作需要为仅有 id 的引用补全元信息，
            // 移除操作则不需要网络（省一次往返），直接构造结果。真正的提交在结果分支里做。
            self.manage_fav_pre_task = Some(Task::new(async move {
                if add {
                    // 加入收藏夹后这些谱面要在本地（含离线）也能展示，因此先批量补齐元信息；
                    // 移除方向只需按 id/路径删引用，不必发这次请求。
                    let mut ids_str = String::new();
                    for chart in &charts {
                        if let Some(id) = chart.id() {
                            ids_str.push_str(&id.to_string());
                            ids_str.push(',');
                        }
                    }
                    if !ids_str.is_empty() {
                        ids_str.pop();
                        let resp: Vec<Chart> = recv_raw(Client::get(format!("/chart/multi-get?ids={ids_str}"))).await?.json().await?;
                        let mut id_to_chart = HashMap::new();
                        for chart in resp {
                            id_to_chart.insert(chart.id, chart);
                        }
                        for chart in &mut charts {
                            if let Some(id) = chart.id() {
                                if let Some(info) = id_to_chart.get(&id) {
                                    chart.info = Some(Box::new(ChartRefChartInfo::from_chart(info)));
                                }
                            }
                        }
                    }
                }
                Ok(ManageFavorite { uuid, charts, add })
            }));
        }
        // ①-D：用户协议/隐私政策刚刚从服务端拉取完成（一次性广播）。此时需要重新评估门禁，
        // 因为协议内容可能被更新，需要立刻向玩家弹出新的同意窗口。
        if JUST_LOADED_TOS.fetch_and(false, Ordering::Relaxed) {
            check_read_tos_and_policy(false, false);
        }
        // If loading was blocked on the TOS gate, retry as soon as the player
        // has accepted (terms_modified transitions from None to Some).
        // ①-E：补偿被协议门禁挡下的那次云端加载。判据是「之前被挡过」且「协议已加载完成」，
        // 而不是直接依赖门禁函数的返回值——后者只在这一帧有效。
        if self.online_pending_tos && get_data().terms_modified.is_some() {
            self.load_online();
        }
        let list = self.tabs.selected_mut();
        let view = &mut list.view;
        // ②-K：拖拽排序的结果。手动顺序只在「默认排序 + 未过滤收藏夹」时才有意义——
        // 否则列表是按字段排出来的，插入位置对不上，于是直接拒绝并提示（整体放弃本次拖拽）。
        if let Some((from, to)) = view.take_movement() {
            if self.current_order != ChartOrder::Default && self.current_fav_index.is_none() {
                show_message(tl!("order-update-failed-sort")).error();
                return Ok(());
            }
            let data = get_data_mut();
            if let Some(index) = self.current_fav_index {
                // 收藏夹视图：直接搬动收藏夹内的引用顺序并落盘；
                // 若这是云端合集且非离线，还要立刻把新顺序同步上去，否则下次同步会被云端顺序覆盖。
                let uuid = data.collection_uuids()[index];
                let mut col = data.collection_info(&uuid).as_ref().clone();
                let online = col.id.is_some();
                let chart = col.charts.remove(from);
                col.charts.insert(to, chart);
                data.set_collection_info(&uuid, col)?;
                let _ = save_data();
                if online && !data.config.offline_mode {
                    if let Some(task) = FavoritesPage::sync_to_cloud_task(index, false) {
                        self.sync_fav_task = Some(task);
                    }
                }
            } else {
                // 普通本地列表：`data.charts` 的顺序就是显示顺序，但界面处于降序显示时下标需要
                // 镜像换算，否则会把条目插到相反的一端。注意 `insert` 发生在 `remove` 之后
                // （此时长度已减 1），这正是映射写成 `len - to` 而不是 `len - to - 1` 的原因。
                if self.order_rev {
                    let chart = data.charts.remove(data.charts.len() - from - 1);
                    data.charts.insert(data.charts.len() - to, chart);
                } else {
                    let chart = data.charts.remove(from);
                    data.charts.insert(to, chart);
                }
                let _ = save_data();
                s.reload_local_charts();
            }
            show_message(tl!("order-updated")).ok();
        }
        // ③-L：可编辑（拖拽排序/多选删除）的前提。必须是本地标签页、没有搜索过滤，
        // 且当前收藏夹可写——搜索状态下显示顺序与底层存储顺序不再一一对应，
        // 拖动会产生对不上的结果；而别人的云端收藏夹也不允许本地改写。
        view.allow_edit(
            list.ty == ChartListType::Local
                && self.search_str.is_empty()
                && self.current_fav_index.is_none_or(|it| get_data().collection_by_index(it).is_owned()),
        );

        // ③-G：纯本地收藏夹的元信息刷新完成。按 uuid 写回（而不是按下标），
        // 因为任务执行期间玩家可能已经切换甚至删除了当前收藏夹。
        if let Some(task) = &mut self.refresh_local_fav_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err),
                    Ok((uuid, charts)) => {
                        let data = get_data();
                        let mut col = data.collection_info(&uuid).as_ref().clone();
                        col.charts = charts;
                        data.set_collection_info(&uuid, col)?;
                        let _ = save_data();
                        show_message(tl!("fav-synced")).ok();
                        self.sync_local(s);
                    }
                }
                self.refresh_local_fav_task = None;
            }
        }
        // ③-H：收藏夹云端同步结果。三种走向差异很大：
        // - `Err`：网络/服务端错误，只提示，本地数据保持不动；
        // - `Ok(Some(col))`：拿到云端内容，用 `merge` 合并——它会保留仅存在于本地的字段
        //   （如「是否为默认收藏夹」），再把云端内容写回并落盘；
        // - `Ok(None)`：服务端认为本地版本已过期（无改动可拉），这时**不能**静默覆盖，
        //   必须让玩家确认是否用本地内容强制上传（确认结果经 `force_sync_to_cloud` 回传）。
        if let Some(task) = &mut self.sync_fav_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err.context(tl!("fav-sync-failed"))),
                    Ok(Some(col)) => {
                        // 用当前下标反查 uuid：同步期间页面处于忙态、收藏夹列表不会变，
                        // 因此这里取到的仍是发起请求时的那个收藏夹。
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.current_fav_index.unwrap()];
                        let local = data.collection_info(&uuid);
                        data.set_collection_info(&uuid, local.merge(&col))?;
                        let _ = save_data();
                        show_message(tl!("fav-synced")).ok();
                        self.sync_local(s);
                    }
                    Ok(None) => {
                        use crate::page::favorites::{tl as ftl, L10N_LOCAL};
                        confirm_dialog(ftl!("sync-to-cloud"), ftl!("sync-outdated"), self.force_sync_to_cloud.clone());
                    }
                }
                self.sync_fav_task = None;
            }
        }
        // ③-I：玩家确认了「用本地覆盖云端」。`force = true` 表示绕开版本检查直接上传；
        // 这里复用同一个 `sync_fav_task` 槽位，因此同一时刻只会有一个收藏夹同步在跑。
        if self.force_sync_to_cloud.swap(false, Ordering::SeqCst) {
            if let Some(index) = self.current_fav_index {
                if let Some(task) = FavoritesPage::sync_to_cloud_task(index, true) {
                    self.sync_fav_task = Some(task);
                }
            }
        }
        // ③-J：拿到系统返回的导出目标，开始打包。到这里才真正写文件——
        // 玩家在系统对话框里取消时 `EXPORT_CONFIG` 是空的，这段代码不会执行
        // （`export_paths` 会被下一次导出覆盖，因此无需额外清理）。
        if let Some(config) = take_export() {
            // 打包实现（导出线程内执行）。产物结构：外层一个 zip，内部每个谱面又是一个
            // `{目录名}.zip`（由 `compress_folder` 把 `data/charts/<目录名>` 整目录打进去），
            // 再附一个 `export.json` 作为「这是批量导出包」的标志。导入端据此逐个解包。
            fn export_inner(paths: Vec<String>, output: File, progress: Arc<AtomicU32>) -> Result<()> {
                let charts = dir::charts()?;
                let mut zip = zip::ZipWriter::new(BufWriter::new(output));
                // 内层谱面用 `Stored`（不压缩）：谱面目录里本来就是已压缩的音频/图片，
                // 再压一遍只会白白消耗 CPU 和时间；只有体积小、可读性重要的 export.json 用 Deflate。
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored)
                    .unix_permissions(0o755)
                    .last_modified_time(chrono::Utc::now().naive_utc().try_into().unwrap_or_default());
                for (i, name) in paths.iter().enumerate() {
                    // 先在内存里压出单个谱面的 zip，再整体写进外层——这样可以一次 `write_all`，
                    // 不必让 ZipWriter 处理嵌套写入。
                    zip.start_file(format!("{name}.zip"), options)?;
                    let mut chart_bytes = Vec::new();
                    compress_folder(Path::new(&format!("{charts}/{name}")), &mut Cursor::new(&mut chart_bytes))?;
                    zip.write_all(&chart_bytes)?;
                    // 每完成一个谱面就更新进度（原子量，渲染线程随时可读，无需加锁）。
                    progress.store(i as u32 + 1, Ordering::Relaxed);
                }

                zip.start_file("export.json", options.compression_method(zip::CompressionMethod::Deflated))?;
                let info = ExportInfo {
                    exported_at: Utc::now(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                };
                serde_json::to_writer(&mut zip, &info)?;

                zip.finish()?;
                Ok(())
            }

            match config {
                // 系统侧申请文件失败（如权限/空间问题），直接提示，不改动任何页面状态。
                Err(err) => show_error(err.into()),
                Ok(config) => {
                    // 正常情况下这里是 `Some`（与 `request_export` 成对设置）；取不到时静默结束。
                    if let Some(paths) = self.export_paths.take() {
                        self.export_total = paths.len();
                        // 容量为 1 的同步通道即「结果回执」：导出线程发一次就退出，
                        // 主线程读到 `Disconnected` 即表示线程异常结束。
                        let (tx, rx) = mpsc::sync_channel(1);
                        let progress = self.export_progress.clone();
                        progress.store(0, Ordering::SeqCst);
                        // 压缩是 CPU/IO 密集操作，放在独立线程里做，避免主线程掉帧。
                        std::thread::spawn(move || {
                            let result = export_inner(paths, config.file, progress);
                            // 失败时必须删掉半成品：否则玩家会在目标位置看到一个损坏的 zip。
                            if result.is_err() {
                                if let Err(err) = (config.deleter)() {
                                    warn!("failed to delete export file: {:?}", err);
                                }
                            }
                            let _ = tx.send(result);
                        });
                        self.export_task = Some(rx);
                    }
                }
            }
        }
        // ③-K：轮询导出线程。只要句柄还在，页面就处于忙态（见 `touch` 的忙检查），
        // 并在 `render_top` 里显示进度。
        if let Some(rx) = &mut self.export_task {
            match rx.try_recv() {
                Ok(Err(err)) => {
                    show_error(err);
                    self.export_task = None;
                }
                Ok(Ok(())) => {
                    // 成功：通知平台侧（iOS 拉起系统面板），并退出多选——
                    // 导出完成后玩家通常不再需要保持选中状态。
                    resolve_export();
                    self.tabs.selected_mut().view.multi_select = None;
                    self.export_task = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                // 发送端在没有 send 的情况下被丢弃 = 线程 panic，必须报错并停止等待，
                // 否则页面会永远停在「导出中」的遮罩上。
                Err(mpsc::TryRecvError::Disconnected) => {
                    show_error(Error::msg("Export thread panicked"));
                    self.export_task = None;
                }
            }
        }
        // ③-M：移动到收藏夹的「准备」阶段完成，开始提交。
        // `Collection::update` 负责本地校验与写入，返回 `Unchanged` 表示无需改动
        // （例如目标状态与当前一致），此时连成功提示都不给。
        if let Some(task) = &mut self.manage_fav_pre_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err),
                    Ok(ManageFavorite { uuid, charts, add }) => {
                        let data = get_data();
                        let col = data.collection_info(&uuid).as_ref().clone();
                        match col.update(uuid, &charts, add) {
                            CollectionUpdate::Unchanged => {}
                            // `Updated` 会原样带回 `add`，用于区分「已加入」/「已移除」的提示文案；
                            // 而无改动的两种情况（条目已存在、或云端收藏夹里塞本地谱面被拦下，
                            // 后者由 `update` 内部自行弹框）都落在上面的 `Unchanged`，不提示成功。
                            CollectionUpdate::Updated { sync_task, add } => {
                                // 云端收藏夹会附带一个提交任务，转交给 `manage_fav_task` 继续等待
                                // （页面保持忙态直到云端确认）；纯本地收藏夹没有任务，直接提示成功。
                                if let Some(task) = sync_task {
                                    self.manage_fav_task = Some(task);
                                } else if add {
                                    show_message(tl!("multi-added-to-fav")).duration(1.).ok();
                                } else {
                                    show_message(tl!("multi-removed-from-fav")).duration(1.).ok();
                                }
                            }
                        }
                        // 无论是否需要云端提交，本地改动都要落盘。
                        let _ = save_data();
                        // 操作的正是当前正在浏览的收藏夹：收起两级菜单、退出多选并重排列表，
                        // 让玩家立刻看到结果。否则保持菜单打开并刷新勾选状态，方便继续操作别的夹。
                        if self.current_fav_index.is_some_and(|it| data.collection_uuids()[it] == uuid) {
                            self.manage_fav_menu.dismiss(s.t);
                            self.multi_operation_menu.dismiss(s.t);
                            self.tabs.selected_mut().view.multi_select = None;
                            self.sync_local(s);
                        } else {
                            // 此时必然仍处于多选模式，因此 `unwrap` 安全。
                            let options = self.get_move_fav_menu_options().unwrap();
                            self.manage_fav_menu.set_options(options);
                        }
                    }
                }
                self.manage_fav_pre_task = None;
            }
        }
        // ③-N：云端提交结果。本地 uuid 与服务端 id 是两套标识，因此要用返回的 `col.id`
        // 反查本地收藏夹再 `merge`；查不到说明该收藏夹已被删除，跳过本地合并即可。
        // 注意这里的失败只代表「这次云端提交没成功」，本地改动已经落盘，
        // 玩家再次进入该收藏夹时会由同步流程重新上报。
        if let Some(task) = &mut self.manage_fav_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err);
                    }
                    Ok((col, added)) => {
                        let data = get_data();
                        if let Some(uuid) = data.collection_uuids().iter().find(|it| data.collection_info(it).id == Some(col.id)) {
                            let uuid = *uuid;
                            let local = data.collection_info(&uuid);
                            data.set_collection_info(&uuid, local.merge(&col))?;
                        }
                        if added {
                            show_message(tl!("multi-added-to-fav")).ok();
                        } else {
                            show_message(tl!("multi-removed-from-fav")).ok();
                        }
                    }
                }
                self.manage_fav_task = None;
            }
        }

        Ok(())
    }

    /// 绘制整页。
    ///
    /// 层次由外到内：标签页外壳 → 当前标签页的列表（虚拟滚动）→ 右下角工具条 → 底部分页行
    /// → 各种弹窗/对话框（最后绘制，保证盖在列表之上）。
    ///
    /// 注意这里只负责「画」与「摆放」，所有状态变更仍在 `update`/`touch` 中完成；
    /// 但菜单的弹出时机被刻意放在渲染阶段（见各处 `need_show_*`），因为只有这一帧才知道
    /// 按钮的实际矩形，菜单才能贴着按钮定位。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        // 再消费一次收藏夹页回传：`render` 与 `update` 的帧序不保证固定，
        // 而 `take()` 天生幂等（结果只会被取走一次），因此重复调用是安全的。
        self.check_fav_page(s);

        let t = s.t;
        let rt = s.rt;
        let mut r = ui.content_rect();
        let chosen = self.tabs.selected().ty;
        // 云端标签页底部要放分页行，因此内容区整体上移一点，避免被分页文字压住。
        if chosen != ChartListType::Local {
            r.h -= 0.08;
        }
        // 列表区域交给当前标签页自己的 [`ChartsView`]：它内部维护 [`Scroll`] 与可见区裁剪，
        // 只渲染屏幕内的卡片，并负责缩略图的异步加载与占位。`feather(-0.01)` 是轻微内缩，
        // 让卡片不贴屏幕边缘。
        s.render_fader(ui, |ui| {
            self.tabs.render(ui, rt, r, |ui, list| {
                list.view.render(ui, r.feather(-0.01), t);
                Ok(())
            })
        })?;
        // 右下角工具条：从右往左依次摆放（每次都把 `r.x` 左移），因此下面的先后顺序
        // 就是界面上从左到右的逆序。热门榜没有任何工具按钮，直接跳过这一段。
        if chosen != ChartListType::Popular {
            s.render_fader(ui, |ui| {
                let multi_select = self.tabs.selected().view.multi_select.is_some();
                let mut r = Rect::new(r.right(), -ui.top + 0.04, 0., r.y + ui.top - 0.06);
                r.w = r.h;
                r.x -= r.w;

                // 多选工具簇：最右侧是「…」操作菜单，往左依次是「已选 N 个」按钮（兼全选/反选入口）
                // 与「×」退出多选；三者只在列表进入多选模式后出现。菜单的弹出同样延后一帧，
                // 这里只把菜单摆到按钮下方并对齐屏幕边缘。
                // 多选模式操作按钮
                if let Some(selected) = &mut self.tabs.selected_mut().view.multi_select {
                    self.multi_operation_btn.render_shadow(ui, r, t, |ui, path| {
                        ui.fill_path(&path, WHITE);
                        let cr = r.feather(-0.01);
                        ui.fill_rect(cr, (*self.icons.r#mod, cr, ScaleType::Fit, BLACK));
                    });
                    if self.need_show_multi_operation_menu {
                        self.need_show_multi_operation_menu = false;
                        self.multi_operation_menu
                            .set_auto_adjust(Some(ui.screen_rect().nonuniform_feather(-0.03, -0.05)));
                        self.multi_operation_menu.set_bottom(true);
                        self.multi_operation_menu.set_selected(usize::MAX);
                        self.multi_operation_menu.set_auto_dismiss(false);
                        self.multi_operation_menu.show(ui, t, Rect::new(r.x, r.bottom() + 0.02, 0.35, 0.4));
                    }

                    let text = tl!("multi-select-status", "count" => selected.len());
                    let tw = ui.text(&text).size(0.5).measure().w;
                    let w = tw + 0.1;
                    let sr = Rect::new(r.x - w - 0.02, r.y, w, r.h);
                    self.multi_select_btn.render_shadow(ui, sr, t, |ui, path| {
                        ui.fill_path(&path, WHITE);
                        let ir = Rect::new(sr.x + 0.04, sr.center().y, 0., 0.).feather(0.025);
                        ui.fill_rect(ir, (*self.icons.select, ir, ScaleType::Fit, BLACK));
                        ui.text(text)
                            .pos((ir.right() + sr.right() - 0.01) / 2., sr.center().y)
                            .size(0.5)
                            .anchor(0.5, 0.5)
                            .no_baseline()
                            .color(BLACK)
                            .draw();
                    });
                    if self.need_show_multi_select_menu {
                        self.need_show_multi_select_menu = false;
                        self.multi_select_menu
                            .set_auto_adjust(Some(ui.screen_rect().nonuniform_feather(-0.03, -0.05)));
                        self.multi_select_menu.set_bottom(true);
                        self.multi_select_menu.set_selected(usize::MAX);
                        self.multi_select_menu.show(ui, t, Rect::new(r.x, r.bottom() + 0.02, 0.3, 0.2));
                    }
                    r.x = sr.x - r.w - 0.02;

                    self.multi_select_cancel_btn.render_shadow(ui, r, t, |ui, path| {
                        ui.fill_path(&path, WHITE);
                        let cr = r.feather(-0.01);
                        ui.fill_rect(cr, (*self.icons.close, cr, ScaleType::Fit, BLACK));
                    });
                    r.x -= r.w + 0.02;
                }

                // 导入按钮：只在本地标签页且非多选时出现（多选时隐藏，避免与批量操作冲突，
                // 与 `touch` 中的判定保持一致）。
                if chosen == ChartListType::Local && !multi_select {
                    self.import_btn.render_shadow(ui, r, t, |ui, path| {
                        ui.fill_path(&path, semi_black(0.4));
                        let cr = r.feather(-0.01);
                        ui.fill_rect(cr, (*self.icons.plus, cr, ScaleType::Fit));
                    });
                    r.x -= r.w + 0.02;
                }

                // 「筛选」与「收藏夹」共用同一个位置：云端标签页是筛选，本地标签页（非多选）
                // 是收藏夹入口，且用高亮色 + 实心星标表示「正在按某个收藏夹过滤」。
                if chosen != ChartListType::Local {
                    self.filter_btn.render_shadow(ui, r, t, |ui, path| {
                        ui.fill_path(&path, semi_black(0.4));
                        let cr = r.feather(-0.01);
                        ui.fill_rect(cr, (*self.icons.filter, cr, ScaleType::Fit));
                    });
                    r.x -= r.w + 0.02;
                } else if !multi_select {
                    let active = self.current_fav_index.is_some();
                    self.fav_btn.render_shadow(ui, r, t, |ui, path| {
                        ui.fill_path(&path, if active { WHITE } else { semi_black(0.4) });
                        let cr = r.feather(-0.01);
                        if active {
                            ui.fill_rect(cr, (*self.icons.star, cr, ScaleType::Fit, Color::from_rgba(255, 193, 7, 255)));
                        } else {
                            ui.fill_rect(cr, (*self.icons.star_outline, cr, ScaleType::Fit));
                        }
                    });
                    r.x -= r.w + 0.02;
                }

                // 排序按钮，以及它的两级菜单。可选字段在这里（而不是构造时）确定：
                // 本地列表没有评分数据，因此不含 `Rating`；云端列表四项齐全。
                // `order_menu_options` 同时被 `update` 用来把菜单下标还原成 `ChartOrder`，
                // 因此这份集合必须在菜单弹出前就位。
                self.order_btn.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.4));
                    let cr = r.feather(-0.01);
                    ui.fill_rect(cr, (*self.icons.order, cr, ScaleType::Fit));
                });
                if self.need_show_order_meta_menu {
                    self.need_show_order_meta_menu = false;
                    self.order_meta_menu
                        .set_auto_adjust(Some(ui.screen_rect().nonuniform_feather(-0.03, -0.05)));
                    if self.tabs.selected().ty == ChartListType::Local {
                        self.order_menu_options = vec![ChartOrder::Default, ChartOrder::Name, ChartOrder::Difficulty];
                    } else {
                        self.order_menu_options = vec![ChartOrder::Default, ChartOrder::Rating, ChartOrder::Name, ChartOrder::Difficulty];
                    }
                    self.order_meta_menu.set_bottom(true);
                    self.order_meta_menu.set_auto_dismiss(false);
                    self.update_order_meta_menu_options();
                    self.order_meta_menu.set_selected(usize::MAX);
                    self.order_meta_menu.show(ui, t, Rect::new(r.x, r.bottom() + 0.02, 0.35, 0.2));
                }

                // 搜索框：固定宽度的胶囊，内部左侧是「×」（仅在有内容时）与放大镜图标，
                // 右侧显示关键字。没有内容时把左侧那块宽度收回去，避免留下一个点不到的空位。
                let empty = self.search_str.is_empty();
                r.w = 0.53;
                r.x -= r.w + 0.02;
                if empty {
                    r.x += r.h;
                    r.w -= r.h;
                }
                let rt = r.right();
                self.search_btn.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.4));
                });
                let mut r = r.feather(-0.01);
                r.w = r.h;
                if !empty {
                    // 每帧都要重设清除按钮的命中区域：`touch` 里靠 `search_clr_btn` 判断是否
                    // 点到了「×」，若某帧忘记设置就会命中过期矩形（关键字清空后仍可被点到）。
                    ui.fill_rect(r, (*self.icons.close, r, ScaleType::Fit));
                    self.search_clr_btn.set(ui, r);
                    r.x += r.w;
                }
                ui.fill_rect(r, (*self.icons.search, r, ScaleType::Fit));
                ui.text(&self.search_str)
                    .pos(r.right() + 0.01, r.center().y)
                    .anchor(0., 0.5)
                    .no_baseline()
                    .size(0.6)
                    .max_width(rt - r.right() - 0.02)
                    .draw();
            });
        }
        // 底部分页行（仅云端标签页）：显示 `当前页 / 总页数` 与上一页/下一页按钮。
        // 展示时页码要 +1（内部从 0 开始计数）；总页数为 0 时会显示成 `1 / 0`，
        // 这恰好提示玩家「当前筛选下没有任何谱面」。
        if chosen != ChartListType::Local {
            let total_page = self.total_page();
            s.render_fader(ui, |ui| {
                let cx = r.center().x;
                let r = ui
                    .text(tl!("page", "current" => self.current_page + 1, "total" => total_page))
                    .pos(cx, r.bottom() + 0.034)
                    .anchor(0.5, 0.)
                    .no_baseline()
                    .size(0.5)
                    .draw();
                let dist = 0.3;
                let ft = 0.024;
                let prev_page = tl!("prev-page");
                let r = ui.text(prev_page.deref()).pos(cx - dist, r.y).anchor(0.5, 0.).size(0.5).measure();
                self.prev_page_btn.render_text(ui, r.feather(ft), t, prev_page, 0.5, false);
                let next_page = tl!("next-page");
                let r = ui.text(next_page.deref()).pos(cx + dist, r.y).anchor(0.5, 0.).size(0.5).measure();
                self.next_page_btn.render_text(ui, r.feather(ft), t, next_page, 0.5, false);
            });
        }
        // 弹窗统一放在最后绘制，保证盖住列表与工具条（`1.` 是弹窗动画进度，1 = 完全展开）。
        // 二级菜单的弹出被延后到这里，是因为它需要贴着上一级菜单的实际矩形摆放。
        self.order_menu.render(ui, t, 1.);
        self.order_meta_menu.render(ui, t, 1.);
        // 排序字段菜单（二级）：位置相对一级菜单向左错开（宽度 0.3，留 0.02 间距）；
        // 打开时按当前排序字段设置选中项并根据升降序刷新文案——`usize::MAX` 表示「没有匹配项，不高亮」。
        if self.need_show_order_menu {
            self.need_show_order_menu = false;
            self.order_menu.set_bottom(true);
            self.order_menu.set_selected(
                self.order_menu_options
                    .iter()
                    .position(|&it| it == self.current_order)
                    .unwrap_or(usize::MAX),
            );
            self.order_menu
                .set_options(self.order_menu_options.iter().map(|it| it.label().into_owned()).collect());

            let mut r = self.order_meta_menu.rect();
            r.w = 0.3;
            r.x -= r.w + 0.02;
            r.h = 0.4;
            self.order_menu.show(ui, t, r);
        }
        // 多选相关的两个菜单（全选/反选、多选操作）。
        self.multi_select_menu.render(ui, t, 1.);
        self.multi_operation_menu.render(ui, t, 1.);
        // 「移动到收藏夹」菜单：位于多选操作菜单左侧，每次打开都清空高亮，
        // 让各收藏夹按「是否已全部包含选中谱面」重新显示勾选状态。
        if self.need_show_manage_fav_menu {
            self.need_show_manage_fav_menu = false;
            self.manage_fav_menu.set_selected(usize::MAX);
            let mut r = self.multi_operation_menu.rect();
            r.w = 0.3;
            r.x -= r.w + 0.02;
            r.h = 0.4;
            self.manage_fav_menu.show(ui, t, r);
        }
        self.manage_fav_menu.render(ui, t, 1.);
        self.tags.render(ui, t);
        self.rating.render(ui, t);
        Ok(())
    }

    /// 顶层绘制：列表自身的顶部元素（下拉刷新指示）与各种「加载中」遮罩。
    ///
    /// 遮罩只负责视觉反馈，**不**拦截触摸——拦截由 `touch` 开头的忙检查完成，
    /// 因此每个可能长时间运行的任务都必须同时出现在那两处（否则会出现「能点但没反应」）。
    /// 导出任务额外显示 `已完成 / 总数` 的进度，其余任务只有无限旋转的提示。
    fn render_top(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        self.tabs.selected_mut().view.render_top(ui, t);
        if self.sync_fav_task.is_some() {
            ui.full_loading_simple(t);
        }
        if self.export_task.is_some() {
            // 进度由导出线程写入原子量，这里只读，不参与同步。
            let current = self.export_progress.load(Ordering::Relaxed);
            let total = self.export_total;
            ui.full_loading(tl!("multi-exporting", "current" => current, "total" => total), t);
        }
        if self.multi_create_fav_task.is_some()
            || self.manage_fav_pre_task.is_some()
            || self.manage_fav_task.is_some()
            || self.refresh_local_fav_task.is_some()
        {
            ui.full_loading_simple(t);
        }
        Ok(())
    }

    /// 取走并清空待跳转的页面（收藏夹页 / 合集页）。
    /// `unwrap_or_default()` 表示「没有跳转需求」时返回默认值，调用方无需判空。
    fn next_page(&mut self) -> NextPage {
        self.next_page.take().unwrap_or_default()
    }

    /// 由列表视图决定的场景切换：玩家点击某张谱面时，[`ChartsView`] 会先构造好 `SongScene`
    /// （携带该谱面、其本地路径或待下载路径、图标以及本地 mod 设置），但只在卡片的入场动画
    /// 播放完成后才把它交出来——因此本方法是「动画结束 → 真正进歌」的出口。
    /// 返回默认值表示本帧没有要进入的场景。
    fn next_scene(&mut self, _s: &mut SharedState) -> NextScene {
        self.tabs.selected_mut().view.next_scene().unwrap_or_default()
    }
}
