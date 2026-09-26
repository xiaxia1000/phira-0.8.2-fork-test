//! 收藏 / 合集页。
//!
//! # 三个必须先分清的「合集」概念
//! - **本地收藏夹**（[`LocalCollection`]）：纯客户端实体，存于 `data/collections/<uuid>.json`。
//!   每个收藏夹有一个**本地生成**的 `Uuid`（UUID 形态），展示顺序由 `Data::collection_uuids`
//!   的排列唯一决定（内容文件里不存顺序）。`id` 为 `None` 就表示它只存在于本地。
//! - **云端合集**（[`Collection`]）：服务端对象，带**服务端分配的数字 `id`**，以及封面、作者、
//!   公开性、创建/修改时间戳。客户端永远无法自行生成这个数字 id。
//! - **收藏 / 精选这类系统合集**：本质上仍是云端 `Collection`，与用户自建合集的差别只在归属——
//!   用 [`LocalCollection::is_owned`] 区分，决定编辑菜单里能否出现重命名、删除等写操作。
//!
//! 三者通过 `LocalCollection::id` 串起来：`id == None` 时是纯本地收藏夹；一旦上传成功或从云端
//! 导入，就把服务端的数字 id 与 `remote_updated` 时间戳写回本地记录，之后才能做增量同步。
//! 因此本页大量分支都围绕「有没有数字 id」展开。
//!
//! # 同步与冲突
//! - 全量同步：`PUT /collection/{id}`（[`PutCollection`]），可带 `updated` 时间戳做乐观并发；
//! - 局部修改：`PATCH /collection/{id}`（[`CollectionPatch`] 的 `Set` / `Public` / `Cover` 补丁）；
//! - 服务端返回 412 表示远端已被他人改动，界面会弹确认框，用户可选择强制覆盖（丢弃时间戳）；
//! - 任何写回都经 [`LocalCollection::merge`] 合并，确保本地缓存的 `remote_updated` 与远端一致。
//!
//! # 与其它页面的协作
//! 「显示全部谱面」或选中某个收藏夹的结果通过线程局部变量 `FAV_PAGE_RESULT` 回传给 `SongScene`
//! / `LibraryPage`；选封面流程则借由 `FAV_PAGE_RESULT` 与 `CHOOSE_COVER` 两个信号协同跳转。

prpr_l10n::tl_file!("favorites");

use super::{Illustration, NextPage, Page, SharedState};
use crate::{
    client::{recv_raw, Chart, Client, Collection, CollectionContent, CollectionCover, CollectionPatch, File, LocalCollection, Ptr, UserManager},
    get_data, get_data_mut,
    icons::Icons,
    page::{SFader, CHOOSE_COVER},
    popup::Popup,
    save_data,
    scene::{confirm_dialog, ProfileScene, TEX_BACKGROUND},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use inputbox::{InputBox, InputMode};
use macroquad::prelude::*;
use prpr::{
    core::Tweenable,
    ext::{open_url, semi_black, semi_white, RectExt, SafeTexture, ScaleType},
    scene::{request_input, show_error, show_message, take_input},
    task::Task,
    ui::{button_hit, DRectButton, Dialog, LoadingParams, RectButton, Scroll, Ui},
};
use regex::Regex;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

// 页面级线程局部状态。
//
// 之所以不放进 `FavoritesPage` 字段：本页会被反复弹出/重建（例如跳到选封面页再返回），
// 而这些值需要在页面实例之外存活，或需要跨越页面栈被其它页面读取。
thread_local! {
    // 本页退出时回传给调用方的结果，是一个「两层 Option」：
    // 外层 `None` = 未产生结果；`Some(None)` = 目标合集已被删除/未选中具体合集（即"显示全部"）；
    // `Some(Some(i))` = 切换到第 i 个合集。调用方据此决定筛选哪个合集的谱面。
    pub static FAV_PAGE_RESULT: RefCell<Option<Option<usize>>> = const { RefCell::new(None) };
    // 点赞状态缓存：键为合集的服务端数字 id，值为（是否已点赞, 写入时刻）。
    // 带时间戳是为了做 3 分钟过期——进出合集很频繁，无缓存会导致每次进入都打一次 /like 请求。
    static LIKE_CACHE: RefCell<HashMap<i32, (bool, DateTime<Utc>)>> = RefCell::default();
}

/// 合集卡片宽度（占屏幕宽度的比例）。
const CARD_WIDTH: f32 = 0.41;
/// 合集卡片高度（占屏幕高度的比例）。
const CARD_HEIGHT: f32 = 0.3;
/// 卡片之间的横/纵向间距，同时也用作网格留白。
const CARD_PAD: f32 = 0.033;

/// 右侧信息栏滑入/滑出动画的时长（秒）。
const INFO_TRANSIT: f32 = 0.32;
/// 右侧信息栏的宽度（占屏幕宽度的比例）；左侧剩余区域即卡片网格的可视宽度。
const INFO_WIDTH: f32 = 0.75;

/// `PUT /collection/{id}` 的请求体。
///
/// 用 `#[serde(flatten)]` 把 [`CollectionContent`] 摊平到顶层，而不是嵌成子对象：
/// 服务端这个接口把 name/description/charts/public 当作正文字段，同时又需要顶层 `updated`
/// 做乐观并发检测。
/// `updated` 为 `None` 表示「强制覆盖」——通常发生在用户已经见过冲突提示并选择覆盖之后。
#[derive(Serialize)]
struct PutCollection {
    /// 合集正文：名称、简介、谱面数字 id 列表、公开性
    #[serde(flatten)]
    content: CollectionContent,
    /// 客户端已知的远端最后修改时间；`None` 表示跳过冲突检测、强制上传
    updated: Option<DateTime<Utc>>,
}

// 收藏夹文件夹项 || Folder item
/// 卡片网格中的一项。
///
/// `index` 为 `None` 时代表「显示全部谱面」这一**虚拟卡片**：它不对应任何本地或云端合集，
/// 存在的意义是提供"不筛选、看全部谱面"的入口，同时让网格第一格永远是它。
struct FolderItem {
    /// 在 `Data::collection_uuids` 中的下标；`None` 表示"显示全部谱面"虚拟项
    index: Option<usize>,
    /// 卡片标题（取自本地收藏夹的 name，虚拟卡片用本地化文案）
    name: String,
    /// 封面缩略图，由 [`Illustration`] 负责异步解码与淡入
    cover: Illustration,
    /// 卡片命中区域，每帧渲染时写入
    btn: RectButton,
}

/// 收藏 / 合集页面。
///
/// 页面承担三块职责：
/// 1. 以网格展示所有本地收藏夹（外加"显示全部谱面"虚拟卡片），提供新建与按链接/id 导入入口；
/// 2. 右侧可滑入的信息栏，展示所选合集的名称/简介/谱面数/云端 id/作者，并提供打开网页版、点赞、
///    进入作者主页等操作；
/// 3. 通过「编辑 / 更多操作 / 云端」三个下拉菜单，组合出重命名、改简介、设封面、设为默认、复制、
///    批量导入、删除，以及上传 / 拉取 / 公开性切换等同步动作。
///
/// 所有网络动作都建模为 `Option<Task<...>>` 字段：`Some` 表示请求在飞，此时 [`FavoritesPage::has_task`]
/// 为真，页面进入全屏 loading 并吞掉一切触摸；`update` 轮询到完成后才消费结果并写回 `Data`。
/// 这样既避免了在触摸/渲染回调里做阻塞 IO，也让「同一时刻不会有两个请求改同一份数据」成为不变量。
pub struct FavoritesPage {
    /// 共享图标集
    icons: Arc<Icons>,
    /// 排位图标，转交给作者主页场景 `ProfileScene` 使用
    rank_icons: [SafeTexture; 8],

    /// 全部卡片；第 0 项恒为"显示全部谱面"虚拟卡片
    folders: Vec<FolderItem>,
    /// 卡片网格的滚动容器
    scroll: Scroll,

    /// 底部"新建"按钮（走输入框流程 `fav_create`）
    create_btn: DRectButton,
    /// 底部"导入"按钮（走输入框流程 `fav_import`）
    import_btn: DRectButton,
    /// "显示全部谱面"卡片的封面，直接用背景图，不需要任何网络请求
    all_illu: Illustration,

    /// 当前选中的合集下标；`None` 表示停留在"显示全部谱面"
    active_folder: Option<usize>,

    /// 顶部工具条的"信息"按钮，点击后滑出右侧信息栏
    info_btn: RectButton,
    /// 信息栏内部的滚动容器
    info_scroll: Scroll,
    /// 信息栏动画进度的时间基准：`f32::INFINITY` 表示已关闭；正值表示展开中，负值（取绝对值）表示收起中
    side_enter_time: f32,
    /// 信息栏中的"在网页中打开"按钮，仅当合集已上传到云端时可见
    open_web_btn: DRectButton,
    /// 作者头像按钮，点击进入作者主页
    owner_btn: RectButton,

    /// 顶部工具条的"云端"菜单按钮
    cloud_btn: RectButton,
    /// 云端操作下拉菜单
    cloud_menu: Popup,
    /// 云端菜单当前可选项（与 `cloud_menu` 的本地化文案一一对应，用于把选中下标映射回动作）
    cloud_options: Vec<&'static str>,
    /// 标记"下一帧才首次展开云端菜单"：`Popup` 需要先由 `render` 写入位置，触摸阶段才能命中
    need_show_cloud_menu: bool,
    /// 删除云端合集的确认标志，由确认对话框回调置位、`update` 轮询消费
    cloud_delete: Arc<AtomicBool>,
    /// "以云端覆盖本地"的确认标志
    sync_from_cloud: Arc<AtomicBool>,
    /// 冲突后"强制上传"的确认标志
    force_sync_to_cloud: Arc<AtomicBool>,
    /// 云端拉取回来的待应用数据：先暂存，等用户确认"以云端为准"后再合并，避免静默丢本地修改
    new_data_from_cloud: Option<Collection>,

    // 编辑状态 || Editing state
    /// 编辑菜单按钮，仅对自己拥有的合集显示
    edit_btn: RectButton,
    /// 编辑菜单（重命名 / 改简介 / 设封面）
    edit_menu: Popup,
    /// 编辑菜单当前可选项
    edit_options: Vec<&'static str>,
    /// 标记需要在下一帧首次展开编辑菜单
    need_show_edit_menu: bool,

    /// "更多操作"菜单按钮
    operations_menu_btn: RectButton,
    /// "更多操作"菜单（设为默认 / 复制 / 批量导入 / 删除）
    operations_menu: Popup,
    /// "更多操作"当前可选项
    operations_options: Vec<&'static str>,
    /// 删除本地收藏夹的确认标志
    operations_delete: Arc<AtomicBool>,
    /// 标记需要在下一帧首次展开"更多操作"菜单
    need_show_operations_menu: bool,

    /// 页面切换的淡入淡出效果
    sf: SFader,
    /// 待执行的页面跳转指令，由 [`Page::next_page`] 取出消费
    next_page: Option<NextPage>,

    /// 选封面流程的回传值：`Ok(id)` 是来自在线谱面的封面，`Err(path)` 是本地谱面路径；
    /// `Some` 表示本次 `update` 需要消费一次选封面结果
    chosen_cover: Option<Result<i32, String>>,

    /// 上传新合集到云端（`POST /collection`）
    upload_task: Option<Task<Result<Collection>>>,
    /// 删除云端合集
    delete_from_cloud_task: Option<Task<Result<()>>>,
    /// 切换云端合集的公开性
    set_public_task: Option<Task<Result<Collection>>>,
    /// 从云端拉取合集
    sync_from_cloud_task: Option<Task<Result<Option<Collection>>>>,
    /// 同步到云端；`Ok(None)` 表示服务端返回 412（本地版本落后，存在冲突）
    sync_to_cloud_task: Option<Task<Result<Option<Collection>>>>,
    /// 通过链接/id 导入他人合集
    import_task: Option<Task<Result<Collection>>>,
    /// 按 id 列表批量导入谱面（一次 `multi-get`）
    batch_import_task: Option<Task<Result<Vec<Chart>>>>,
    /// 设置封面：`Ok(Ok(col))` 表示合集已同步、云端返回了更新后的合集对象；
    /// `Ok(Err(file))` 表示合集尚未同步、只拿到了谱面插画文件，需要本地写入封面
    set_cover_task: Option<Task<Result<Result<Collection, File>>>>,

    /// 当前合集的点赞状态（界面视图，来源为缓存或 `/like` 查询结果）
    liked: bool,
    /// 信息栏中的点赞按钮
    like_btn: RectButton,
    /// 查询点赞状态的任务
    fetch_like_task: Option<Task<Result<bool>>>,
    /// 提交点赞/取消点赞的任务
    like_task: Option<Task<Result<()>>>,
}

// 页面内部辅助方法：卡片重建、信息栏渲染、任务判定、谱面 id 提取、导入解析与云端同步入口。
impl FavoritesPage {
    /// 创建收藏页面。
    ///
    /// 两个参数都是"回传值"而非普通配置：跳去封面选择页或谱面库再返回时，需要把此前的选中项
    /// 与选中的封面带回来，页面才能无缝续上，因此由调用方通过 `FAV_PAGE_RESULT` / `CHOOSE_COVER`
    /// 协议读出后传进来。
    ///
    /// # Arguments
    /// * `icons`、`rank_icons` — 图标集，后者会转交作者主页场景使用
    /// * `active_folder` — 初始选中的合集下标；`None` 表示"显示全部谱面"
    /// * `chosen_cover` — 选封面流程的回传值
    pub fn new(icons: Arc<Icons>, rank_icons: [SafeTexture; 8], active_folder: Option<usize>, chosen_cover: Option<Result<i32, String>>) -> Self {
        let mut page = Self {
            icons,
            rank_icons,

            folders: Vec::new(),
            scroll: Scroll::new(),

            create_btn: DRectButton::new(),
            import_btn: DRectButton::new(),
            all_illu: Illustration::from_done(TEX_BACKGROUND.with(|it| it.borrow().clone().unwrap())),

            active_folder,

            info_btn: RectButton::new(),
            info_scroll: Scroll::new(),
            side_enter_time: f32::INFINITY,
            open_web_btn: DRectButton::new(),
            owner_btn: RectButton::new(),

            cloud_btn: RectButton::new(),
            cloud_menu: Popup::new(),
            cloud_options: Vec::new(),
            need_show_cloud_menu: false,
            cloud_delete: Arc::default(),
            sync_from_cloud: Arc::default(),
            force_sync_to_cloud: Arc::default(),
            new_data_from_cloud: None,

            edit_btn: RectButton::new(),
            edit_menu: Popup::new(),
            edit_options: Vec::new(),
            need_show_edit_menu: false,

            operations_menu_btn: RectButton::new(),
            operations_menu: Popup::new(),
            operations_options: Vec::new(),
            operations_delete: Arc::default(),
            need_show_operations_menu: false,

            sf: SFader::new(),
            next_page: None,

            chosen_cover,

            upload_task: None,
            delete_from_cloud_task: None,
            set_public_task: None,
            sync_from_cloud_task: None,
            sync_to_cloud_task: None,
            import_task: None,
            batch_import_task: None,
            set_cover_task: None,

            liked: false,
            like_btn: RectButton::new(),
            fetch_like_task: None,
            like_task: None,
        };
        // 字段就位后统一重建卡片列表：卡片内容依赖 `Data`，不能塞进上面的结构体字面量里
        page.rebuild_folders();
        page
    }

    // 根据当前收藏夹数据重建文件夹列表 || Rebuild folder list from current data
    /// 按当前本地数据重建卡片列表。
    ///
    /// 任何会改变收藏夹集合或顺序的操作（新建、复制、删除、改名、改封面、上传、拉取、导入）之后
    /// 都必须调用：它既重建卡片的命中区域 `RectButton`，也重新按当前内容取封面。
    fn rebuild_folders(&mut self) {
        let data = get_data();
        let mut folders = Vec::new();

        // "显示全部谱面"卡片 || "Show all charts" card
        folders.push(FolderItem {
            index: None,
            name: tl!("show-all").to_string(),
            cover: self.all_illu.clone(),
            btn: RectButton::new(),
        });

        // 每个本地收藏夹对应一张卡片；index 即它在 collection_uuids（也就是展示顺序）中的下标，
        // 卡片只保存下标而不持有合集数据，保证列表顺序变化后卡片自动指向正确对象。
        for (index, col) in data.collections().enumerate() {
            folders.push(FolderItem {
                index: Some(index),
                name: col.name.clone(),
                cover: col.cover(),
                btn: RectButton::new(),
            });
        }

        self.folders = folders;
    }

    /// 渲染右侧信息栏的内容。
    ///
    /// 依次绘制「在网页中打开」按钮、作者行，以及名称/简介/谱面数/云端 ID 四行文字。
    ///
    /// # Returns
    /// 闭包返回 `(width, h)`，即内容区宽度与内容实际占用高度；`Scroll` 依据这个高度决定是否出现滚动条。
    ///
    /// # Panics
    /// 使用前必须已选中一个真实合集（`active_folder` 为 `Some`），否则 `unwrap` 会 panic。
    fn render_info(&mut self, ui: &mut Ui, rt: f32) {
        let data = get_data();
        let col = data.collection_by_index(self.active_folder.unwrap());
        let pad = 0.03;
        ui.dx(pad);
        ui.dy(0.03);
        let width = INFO_WIDTH - pad;
        self.info_scroll.size((width - pad, ui.top * 2. - 0.06));
        self.info_scroll.render(ui, |ui| {
            let mut h = 0.;
            // `dy!` 同时推进布局并累计总高度；累计值就是滚动内容尺寸，必须把每一处位移都记进来
            macro_rules! dy {
                ($e:expr) => {{
                    let dy = $e;
                    h += dy;
                    ui.dy(dy);
                }};
            }
            let mw = width - pad * 3.;
            // 只有已上传到云端的合集才有网页版详情页
            if col.id.is_some() {
                let r = Rect::new(0.03, 0., mw, 0.12).nonuniform_feather(-0.03, -0.01);
                self.open_web_btn.render_text(ui, r, rt, ttl!("open-in-web"), 0.6, true);
                dy!(r.h + 0.04);
            }
            // 作者行：只有云端合集（上传过或导入的）才带 owner，纯本地收藏夹没有作者
            if let Some(uploader) = &col.owner {
                let c = 0.06;
                let s = 0.05;
                let r = ui.avatar(c, c, s, rt, UserManager::opt_avatar(uploader.id, &self.icons.user));
                self.owner_btn.set(ui, Rect::new(c - s, c - s, s * 2., s * 2.));
                if let Some((name, color)) = UserManager::name_and_color(uploader.id) {
                    ui.text(name)
                        .pos(r.right() + 0.02, r.center().y)
                        .anchor(0., 0.5)
                        .no_baseline()
                        .max_width(width - 0.15)
                        .size(0.6)
                        .color(color)
                        .draw();
                }
                dy!(0.14);
            }
            // 文本行的统一渲染：标题用灰色小字，内容用白色大字并支持换行
            let mut item = |title: Cow<'_, str>, content: Cow<'_, str>| {
                dy!(ui.text(title).size(0.4).color(semi_white(0.7)).draw().h + 0.02);
                dy!(ui.text(content).pos(pad, 0.).size(0.6).multiline().max_width(mw).draw().h + 0.03);
            };
            item(tl!("info-name"), col.name.as_str().into());
            item(tl!("info-description"), col.description.as_str().into());
            item(tl!("info-count"), col.charts.len().to_string().into());
            if let Some(id) = col.id {
                item("ID".into(), id.to_string().into());
            }
            (width, h)
        });
    }

    /// 是否有任何网络任务在飞。
    ///
    /// 这是页面级的全局闸门：为真时 [`Page::touch`] 直接返回 `true` 吞掉所有事件，并且渲染出全屏 loading。
    /// 由此保证同一时刻不会有两个请求并发改写同一份合集数据（也顺带避免用户在请求途中重复点击）。
    fn has_task(&self) -> bool {
        self.upload_task.is_some()
            || self.delete_from_cloud_task.is_some()
            || self.set_public_task.is_some()
            || self.sync_from_cloud_task.is_some()
            || self.sync_to_cloud_task.is_some()
            || self.import_task.is_some()
            || self.batch_import_task.is_some()
            || self.set_cover_task.is_some()
            || self.like_task.is_some()
    }

    /// 从本地合集中提取可上传的谱面数字 id 列表。
    ///
    /// 云端合集只认数字 id，而本地导入的谱面没有 id，因此这类谱面无法同步。
    /// `allow_local` 决定遇到本地谱面时的行为：为 `false`（同步路径）时弹窗列出这些谱面的名字并
    /// 返回 `None`，让调用方中止同步——静默丢弃用户的谱面是不可接受的；
    /// 为 `true`（批量导入去重）时直接跳过它们，只取已有 id。
    ///
    /// # Returns
    /// 全部谱面都有云端 id 时返回 id 列表；否则在 `allow_local == false` 时返回 `None`。
    fn collect_chart_ids(col: &LocalCollection, allow_local: bool) -> Option<Vec<i32>> {
        let data = get_data();
        let mut chart_ids = Vec::with_capacity(col.charts.len());
        let mut local_charts = Vec::new();
        for chart in &col.charts {
            if let Some(id) = chart.id() {
                chart_ids.push(id);
            } else if !allow_local {
                local_charts.push(&*chart.path);
            }
        }
        if !local_charts.is_empty() {
            let mut charts = String::new();
            for path in local_charts {
                if let Some(index) = data.find_chart_by_path(path) {
                    charts.push_str(&data.charts[index].info.name);
                    charts.push_str(", ");
                }
            }
            if !charts.is_empty() {
                charts.truncate(charts.len() - 2);
            }
            Dialog::simple(ttl!("favorites-online-only", "charts" => charts)).show();
            return None;
        }
        Some(chart_ids)
    }

    /// 解析用户输入的文本，尝试导入一个他人分享的云端合集。
    ///
    /// 兼容两种输入：纯数字 id，或形如 `phira.moe/collection/<id>` 的链接。
    ///
    /// # Returns
    /// `true` 表示输入被识别：可能已发起导入请求，也可能因"该合集已导入过"而只弹了提示；
    /// `false` 表示完全无法解析，调用方应提示输入非法。
    fn try_import(&mut self, text: String) -> bool {
        let text = text.trim();

        // 先按纯 id 解析，失败再退化到从链接里用正则抠出 id
        let mut id = text.parse::<i32>().ok();
        if id.is_none() {
            let regex = Regex::new(r"phira\.moe/collection/(\d+)").unwrap();
            if let Some(caps) = regex.captures(text) {
                if let Some(id_str) = caps.get(1) {
                    id = id_str.as_str().parse::<i32>().ok();
                } else {
                    return false;
                }
            }
        }
        // 已经导入过同一个云端合集就不重复添加，避免本地出现两份指向同一 id 的副本
        if let Some(id) = id {
            if get_data().collections().any(|col| col.id == Some(id)) {
                show_message(tl!("already-imported")).error();
            } else {
                self.import_task = Some(Task::new(async move {
                    let resp: Collection = recv_raw(Client::get(format!("/collection/{id}"))).await?.json().await?;
                    Ok(resp)
                }));
            }
            return true;
        }

        false
    }

    /// 构造"同步到云端"的任务：把本地合集内容整体 `PUT` 到服务端。
    ///
    /// 与 `PATCH` 的增量补丁不同，这里是全量覆盖式提交，因此需要先把合集内容折算成
    /// 服务端认识的谱面数字 id 列表（含本地谱面时直接放弃，见 [`FavoritesPage::collect_chart_ids`]）。
    ///
    /// # Arguments
    /// * `index` — 合集在当前展示顺序中的下标
    /// * `force` — 为真时不携带 `updated` 时间戳，跳过服务端的冲突检测、直接覆盖远端
    ///
    /// # Returns
    /// 可挂载的任务；`None` 表示前置条件不满足（合集含本地谱面，已弹窗提示）。
    /// 任务结果为 `Ok(None)` 时代表服务端返回 412：远端已被他人改动，需要用户确认后以 `force = true` 重试。
    pub fn sync_to_cloud_task(index: usize, force: bool) -> Option<Task<Result<Option<Collection>>>> {
        let data = get_data();
        let col = data.collection_by_index(index);
        let chart_ids = Self::collect_chart_ids(&col, false)?;

        let body = PutCollection {
            content: CollectionContent {
                name: col.name.clone(),
                description: col.description.clone(),
                charts: chart_ids,
                public: col.public,
            },
            updated: if force { None } else { col.remote_updated },
        };
        let col_id = col.id.unwrap();
        Some(Task::new(async move {
            let result = recv_raw(Client::request(Method::PUT, format!("/collection/{col_id}")).json(&body)).await;
            match result {
                Ok(resp) => {
                    let resp: Collection = resp.json().await?;
                    Ok(Some(resp))
                }
                Err(err) => {
                    // 412 Precondition Failed 即乐观并发冲突：转成 `Ok(None)` 交给界面弹确认框，
                    // 而不是当作普通错误上报——这是可恢复的、需要用户决策的状态
                    if err.to_string().starts_with("request failed (412)") {
                        Ok(None)
                    } else {
                        Err(err)
                    }
                }
            }
        }))
    }

    /// 发起一次同步，把任务挂到字段上等 `update` 轮询。
    ///
    /// 若前置条件不满足（含本地谱面）则不挂任务，此时不会进入全屏 loading。
    fn sync_to_cloud(&mut self, force: bool) {
        if let Some(task) = Self::sync_to_cloud_task(self.active_folder.unwrap(), force) {
            self.sync_to_cloud_task = Some(task);
        }
    }

    /// 选中项变化（或进入本页）后刷新与当前合集相关的远端信息。
    ///
    /// 目前只做点赞状态：离线模式下直接跳过（不发任何请求）；
    /// 点赞状态走 3 分钟缓存，命中且未过期就直接复用，否则发起一次 `/collection/{id}/like` 查询。
    /// 之所以设缓存：用户在合集间来回切换很频繁，每次都查会白白打满请求。
    fn on_active_update(&mut self) {
        let Some(folder) = self.active_folder else { return };
        let data = get_data();
        // 离线模式：不触碰网络，点赞按钮会保持上一次的本地视图
        if data.config.offline_mode {
            return;
        }
        let col = data.collection_by_index(folder);
        // 纯本地收藏夹没有云端 id，也就没有点赞概念
        let Some(id) = col.id else { return };

        // 命中且未过期就直接复用；过期则顺手删除，等本次请求的结果重新写回缓存
        let cached = LIKE_CACHE.with_borrow_mut(|cache| {
            if let Some((like, updated)) = cache.get_mut(&id) {
                if *updated + chrono::Duration::minutes(3) < Utc::now() {
                    cache.remove(&id);
                } else {
                    self.liked = *like;
                    return true;
                }
            }
            false
        });
        if cached {
            return;
        }

        self.fetch_like_task = Some(Task::new(async move {
            #[derive(Deserialize)]
            struct Resp {
                like: bool,
            }
            let resp: Resp = recv_raw(Client::get(format!("/collection/{id}/like"))).await?.json().await?;
            Ok(resp.like)
        }));
    }
}

// `Page` 钩子行为：
// - `label` 提供侧边栏标题；
// - `enter` 在页面进入时刷新当前合集的点赞状态；
// - `touch` 是全部交互的分发中心：先处理信息栏动画与三个下拉菜单，再做按钮与卡片命中；
//   期间只要 `has_task()` 为真就直接吞掉事件；
// - `update` 消费封面选择、输入框与菜单结果，并轮询所有网络任务把结果写回 `Data`；
// - `render` 绘制工具条、卡片网格、底部按钮、信息侧栏与全屏 loading；
// - `render_top` 在顶层重复绘制信息侧栏，保证它盖在其它页面之上；
// - `next_page` 取出待执行的跳转指令。
impl Page for FavoritesPage {
    /// 侧边栏显示的页面标题。
    fn label(&self) -> Cow<'static, str> {
        ttl!("favorites")
    }

    /// 页面进入时刷新与当前合集相关的远端信息。
    fn enter(&mut self, _s: &mut SharedState) -> Result<()> {
        self.on_active_update();
        Ok(())
    }

    /// 处理触摸事件。
    ///
    /// 分发顺序编码了几条隐含规则：
    /// - 有网络任务在飞时一律返回 `true`（`has_task` 闸门），避免并发修改同一份合集数据；
    /// - 信息栏展开时只有信息栏内的控件可点，点击左侧空白处则触发收起动画；
    /// - 任一下拉菜单展开时事件被该菜单独占，否则"点击菜单外部关闭"会顺手触发底下的按钮。
    ///
    /// # Returns
    /// 事件被消费返回 `true`。
    ///
    /// # Errors
    /// 打开合集网页版链接失败时返回错误。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        let t = s.t;
        let rt = s.rt;

        // 全局闸门：任何网络任务在飞时屏蔽交互并进入 loading
        if self.has_task() {
            return Ok(true);
        }

        // 信息栏状态机：`side_enter_time` 有限即表示动画中或已展开
        if self.side_enter_time.is_finite() {
            if self.side_enter_time > 0. && rt > self.side_enter_time + INFO_TRANSIT {
                // 点在信息栏左侧的空白区域：开始收起动画（负的时间基准表示反向播放）
                if touch.position.x < 1. - INFO_WIDTH && touch.phase == TouchPhase::Started {
                    self.side_enter_time = -rt;
                    return Ok(true);
                }
                if self.info_scroll.touch(touch, t) {
                    return Ok(true);
                }
                // 打开网页版详情页：链接由云端数字 id 拼出，故只有已上传的合集可见此按钮
                if self.open_web_btn.touch(touch, rt) {
                    if let Some(index) = self.active_folder {
                        let col = get_data().collection_by_index(index);
                        open_url(&format!("https://phira.moe/collection/{}", col.id.unwrap()))?;
                    }
                    return Ok(true);
                }
                // 点击作者头像进入作者主页；需要把本页持有的图标集转交给新场景
                if self.owner_btn.touch(touch) {
                    button_hit();
                    let col = get_data().collection_by_index(self.active_folder.unwrap());
                    self.sf
                        .goto(t, ProfileScene::new(col.owner.as_ref().unwrap().id, self.icons.user.clone(), self.rank_icons.clone()));
                    return Ok(true);
                }
            }
            return Ok(false);
        }

        // 展开中的下拉菜单独占事件，必须排在所有普通按钮之前
        if self.edit_menu.showing() {
            self.edit_menu.touch(touch, t);
            return Ok(true);
        }
        if self.operations_menu.showing() {
            self.operations_menu.touch(touch, t);
            return Ok(true);
        }
        if self.cloud_menu.showing() {
            self.cloud_menu.touch(touch, t);
            return Ok(true);
        }

        // 底部功能按钮：新建与导入都只是发起输入框，真正的处理在 `update` 的输入回传分支里
        if self.create_btn.touch(touch, t) {
            request_input("fav_create", InputBox::new());
            return Ok(true);
        }
        if self.import_btn.touch(touch, t) {
            request_input("fav_import", InputBox::new());
            return Ok(true);
        }

        // 卡片网格滚动（用实时时间 rt 而非 t，滚动惯性需要不受页面帧率影响的时钟）
        if self.scroll.touch(touch, rt) {
            return Ok(true);
        }

        // 顶部工具条：三个菜单的可选项随合集状态动态生成——
        // 是否已上传（有无数字 id）、是否自己拥有、是否默认合集，都会改变可用动作集合
        if let Some(index) = self.active_folder {
            let data = get_data();
            let col = data.collection_by_index(index);
            // 编辑类动作只对自己拥有的合集开放（系统合集不可改名/改封面）
            if self.edit_btn.touch(touch) && col.is_owned() {
                button_hit();
                let mut options = Vec::new();
                if col.is_owned() {
                    options.push("rename");
                    options.push("set-description");
                    options.push("set-cover");
                }

                self.edit_menu.set_selected(usize::MAX);
                self.edit_menu.set_options(options.iter().map(|it| tl!(*it).into_owned()).collect());
                self.edit_options = options;
                self.need_show_edit_menu = true;
                return Ok(true);
            }
            // 更多操作：默认合集不可删除；复制对任何合集都可用（生成纯本地副本）；
            // 批量导入会写云端，故仅限自己拥有的合集
            if self.operations_menu_btn.touch(touch) {
                button_hit();
                let is_default = col.is_default;
                let mut options = Vec::new();
                if !is_default && col.is_owned() {
                    options.push("set-as-default");
                }
                options.push("duplicate");
                if col.is_owned() {
                    options.push("batch-import");
                }
                if !is_default {
                    options.push("delete");
                }

                self.operations_menu.set_selected(usize::MAX);
                self.operations_menu.set_options(options.iter().map(|it| tl!(*it).into_owned()).collect());
                self.operations_options = options;
                self.need_show_operations_menu = true;
                return Ok(true);
            }
            // 云端菜单：尚未上传时只能"上传到云端"；已上传后才出现拉取/公开性/删除；
            // 其中的写操作（改公开性、删云端）只对自己拥有的合集开放
            if self.cloud_btn.touch(touch) {
                button_hit();
                let mut options = Vec::new();
                if col.id.is_some() {
                    if col.is_owned() {
                        options.push("sync-to-cloud");
                    }
                    options.push("sync-from-cloud");
                    if col.is_owned() {
                        if col.public {
                            options.push("make-private");
                        } else {
                            options.push("make-public");
                        }
                        options.push("delete-from-cloud");
                    }
                } else {
                    options.push("upload-to-cloud");
                }
                self.cloud_menu.set_selected(usize::MAX);
                self.cloud_menu.set_options(options.iter().map(|it| tl!(*it).into_owned()).collect());
                self.cloud_options = options;
                self.need_show_cloud_menu = true;
                return Ok(true);
            }
            // 展开信息栏：以当前实时时间作为动画起点，`render` 据此插值出滑入位移
            if self.info_btn.touch(touch) {
                button_hit();
                self.side_enter_time = rt;
                return Ok(true);
            }
            // 点赞：仅已上传合集有该接口；正在查询点赞状态时禁用，避免用未知的当前值去取反；
            // 提交的是「取反后的目标状态」，故服务端接口是幂等的状态设置而非 toggle
            if col.id.is_some() && self.fetch_like_task.is_none() && self.like_btn.touch(touch) {
                button_hit();
                let liked = self.liked;
                self.like_task = Some(Task::new(async move {
                    #[derive(Serialize)]
                    struct Req {
                        like: bool,
                    }
                    recv_raw(Client::post(format!("/collection/{}/like", col.id.unwrap()), &Req { like: !liked })).await?;
                    Ok(())
                }));
                return Ok(true);
            }
        }

        // 卡片命中：点到的是当前卡片则回传结果并退出本页（据此筛选谱面）；
        // 点到别的卡片只切换选中项并刷新远端信息，不离开本页
        // 编辑按钮检测 || Edit button detection
        for folder in self.folders.iter_mut() {
            if folder.btn.touch(touch) {
                button_hit();
                if self.active_folder == folder.index {
                    FAV_PAGE_RESULT.with(|it| *it.borrow_mut() = Some(folder.index));
                    self.next_page = Some(NextPage::Pop);
                } else {
                    self.active_folder = folder.index;
                    self.on_active_update();
                }
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// 每帧推进页面状态。
    ///
    /// 这里集中处理三类「回传」入口，再统一收尾：
    /// 1. 别的页面/平台回调带回来的用户输入——封面选择结果 `chosen_cover` 与输入框 `take_input`；
    /// 2. 三个下拉菜单的选中结果（`changed()`）；
    /// 3. 确认对话框置位的 `AtomicBool`（删本地、删云端、云端覆盖、强制上传）。
    /// 最后逐个轮询 `Task`，成功的结果一律经 [`LocalCollection::merge`] 合并回本地。
    ///
    /// # Errors
    /// 写回 `Data`（`set_collection_info` / `push_collection` / `remove_collection`）失败时返回错误。
    fn update(&mut self, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        // 阶段一：推进三个弹出菜单的动画与选中状态
        self.edit_menu.update(t);
        self.operations_menu.update(t);
        self.cloud_menu.update(t);

        self.scroll.update(s.rt);
        self.info_scroll.update(s.rt);

        // 阶段二：驱动卡片缩略图的异步解码与淡入（`settle` 内部会轮询图片任务）
        for folder in &mut self.folders {
            folder.cover.settle(t);
        }

        // 阶段三：消费封面选择页回传的结果
        if let Some(chosen_cover) = self.chosen_cover.take() {
            let data = get_data_mut();
            let col = data.collection_by_index(self.active_folder.unwrap());
            match chosen_cover {
                Ok(chart_id) => {
                    // 选的是在线谱面的插画：合集已上传则直接 PATCH 云端封面（服务端自己会取图），
                    // 未上传则先拉一份插画文件，再作为本地封面写入
                    let col_id = col.id;
                    self.set_cover_task = Some(Task::new(async move {
                        if let Some(col_id) = col_id {
                            // Collection is synced, update cloud directly
                            let resp: Collection =
                                recv_raw(Client::request(Method::PATCH, format!("/collection/{col_id}")).json(&CollectionPatch::Cover(chart_id)))
                                    .await?
                                    .json()
                                    .await?;
                            Ok(Ok(resp))
                        } else {
                            // Collection is not synced, fetch chart
                            // illustration and set as cover locally
                            let chart = Ptr::<Chart>::new(chart_id).fetch().await?;
                            Ok(Err(chart.illustration.clone()))
                        }
                    }));
                }
                Err(local_path) => {
                    // 选的是本地谱面：云端合集无法用本地图片当封面（服务端拿不到文件），
                    // 因此已上传的合集只能提示"仅支持在线谱面"，未上传的才写入本地并落盘
                    if col.id.is_some() {
                        let chart = if let Some(index) = data.find_chart_by_path(&local_path) {
                            data.charts[index].info.name.clone()
                        } else {
                            String::new()
                        };
                        Dialog::simple(ttl!("favorites-online-only", "charts" => chart)).show();
                    } else {
                        let new_col = LocalCollection {
                            cover: CollectionCover::LocalChart(local_path),
                            ..col.as_ref().clone()
                        };
                        data.set_collection_info(&data.collection_uuids()[self.active_folder.unwrap()], new_col)?;
                        show_message(tl!("updated")).ok();
                        self.rebuild_folders();
                    }
                }
            }
        }

        // 处理输入事件 || Handle input events
        // 阶段四：处理输入框回传（新建 / 重命名 / 改简介 / 导入 / 批量导入），id 区分来源
        if let Some((id, text)) = take_input() {
            match id.as_str() {
                "fav_create" => {
                    // 名称必须非空且通过敏感词校验；新建的收藏夹只有本地 uuid，尚无云端 id
                    let name = text.trim().to_string();
                    if name.is_empty() {
                        show_message(tl!("name-empty")).error();
                    } else if let Err(err) = crate::censor::check_text(&name) {
                        show_message(err.to_string()).error();
                    } else {
                        get_data_mut().push_collection(LocalCollection::new(name))?;
                        let _ = save_data();
                        show_message(tl!("created")).ok();
                        self.rebuild_folders();
                    }
                }
                "fav_rename" => {
                    // 重命名：先落本地，若该合集已上传且不在离线模式，再自动同步到云端——
                    // 用户改完名字却发现远端还是旧名字会非常困惑，因此这里做自动上传
                    let new_name = text.trim().to_string();
                    if new_name.is_empty() {
                        show_message(tl!("name-empty")).error();
                    } else if let Err(err) = crate::censor::check_text(&new_name) {
                        show_message(err.to_string()).error();
                    } else if let Some(index) = self.active_folder {
                        let data = get_data();
                        let uuid = data.collection_uuids()[index];
                        let col = data.collection_info(&uuid);
                        let new_col = LocalCollection {
                            name: new_name,
                            ..col.as_ref().clone()
                        };
                        data.set_collection_info(&uuid, new_col)?;
                        let _ = save_data();
                        show_message(tl!("updated")).ok();
                        self.rebuild_folders();
                        if col.id.is_some() && !data.config.offline_mode {
                            self.sync_to_cloud(false);
                        }
                    }
                }
                "fav_description" => {
                    let new_description = text.trim().to_string();
                    if let Err(err) = crate::censor::check_text(&new_description) {
                        show_message(err.to_string()).error();
                    } else if let Some(index) = self.active_folder {
                        let data = get_data();
                        let uuid = data.collection_uuids()[index];
                        let col = data.collection_info(&uuid);
                        let new_col = LocalCollection {
                            description: new_description,
                            ..col.as_ref().clone()
                        };
                        data.set_collection_info(&uuid, new_col)?;
                        let _ = save_data();
                        show_message(tl!("updated")).ok();
                        self.rebuild_folders();
                        if col.id.is_some() && !data.config.offline_mode {
                            self.sync_to_cloud(false);
                        }
                    }
                }
                "fav_import" => {
                    if !self.try_import(text) {
                        show_message(tl!("invalid-import")).error();
                    }
                }
                "fav_batch_import" => {
                    // 批量导入：接受逗号或空格分隔的谱面 id；先算出已有 id 用于去重，
                    // 再一次性 multi-get 拉取，避免逐个请求把服务器打满
                    let data = get_data();
                    let col = data.collection_by_index(self.active_folder.unwrap());
                    let local_chart_ids = Self::collect_chart_ids(&col, true).unwrap().into_iter().collect::<HashSet<_>>();
                    let Ok(mut chart_ids) = text
                        .split([',', ' '])
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|it| it.parse::<i32>())
                        .collect::<Result<Vec<_>, _>>()
                    else {
                        show_message(tl!("invalid-import")).error();
                        return Ok(());
                    };
                    // 去掉合集里已有的谱面，全部重复则直接返回（不发请求、也不提示）
                    chart_ids.retain(|id| !local_chart_ids.contains(id));
                    if chart_ids.is_empty() {
                        return Ok(());
                    }
                    self.batch_import_task = Some(Task::new(async move {
                        let mut ids_str = String::new();
                        for id in &chart_ids {
                            ids_str.push_str(&id.to_string());
                            ids_str.push(',');
                        }
                        ids_str.pop();

                        let resp: Vec<Chart> = recv_raw(Client::get(format!("/chart/multi-get?ids={ids_str}"))).await?.json().await?;
                        Ok(resp)
                    }));
                }
                _ => {}
            }
        }

        // 阶段五：消费三个下拉菜单与确认对话框产生的动作
        if let Some(index) = self.active_folder {
            let data = get_data_mut();
            let col = data.collection_by_index(index);
            // 编辑菜单：重命名 / 改简介都只请求输入框，设封面则要跳到封面选择页
            if self.edit_menu.changed() {
                match self.edit_options[self.edit_menu.selected()] {
                    "rename" => {
                        request_input("fav_rename", InputBox::new().default_text(&col.name));
                    }
                    "set-description" => {
                        request_input("fav_description", InputBox::new().default_text(&col.description).mode(InputMode::Multiline));
                    }
                    "set-cover" => {
                        if col.charts.is_empty() {
                            show_message(tl!("no-charts")).error();
                        } else {
                            FAV_PAGE_RESULT.with(|it| *it.borrow_mut() = Some(Some(index)));
                            CHOOSE_COVER.store(true, Ordering::Relaxed);
                            show_message(tl!("select-cover"));
                            self.next_page = Some(NextPage::Pop);
                        }
                    }
                    _ => {}
                }
            }
            // 更多操作菜单
            if self.operations_menu.changed() {
                match self.operations_options[self.operations_menu.selected()] {
                    "set-as-default" => {
                        // 「默认」在整个收藏夹集合中全局唯一，因此要先遍历全部合集清掉旧标记，
                        // 再只把目标合集置为默认，避免出现两个默认合集
                        let uuid = data.collection_uuids()[index];
                        let uuids = data.collection_uuids().to_vec();
                        for its_uuid in uuids {
                            let col = LocalCollection {
                                is_default: its_uuid == uuid,
                                ..data.collection_info(&its_uuid).as_ref().clone()
                            };
                            data.set_collection_info(&its_uuid, col)?;
                        }
                    }
                    "delete" => {
                        // 删除是破坏性且本地的操作，先弹确认框；真正执行在下面 `operations_delete` 置位后
                        confirm_dialog(tl!("delete"), tl!("delete-confirm"), self.operations_delete.clone());
                    }
                    "duplicate" => {
                        // 复制：内容沿用，但必须清掉云端身份（id/public/remote_updated）与默认标记，
                        // 否则副本会声称自己就是那个云端合集，后续同步将改错对象
                        data.push_collection(LocalCollection {
                            id: None,
                            public: false,
                            remote_updated: None,
                            is_default: false,
                            ..data.collection_by_index(index).as_ref().clone()
                        })?;
                        let _ = save_data();
                        self.rebuild_folders();
                    }
                    "batch-import" => {
                        request_input("fav_batch_import", InputBox::new());
                    }
                    _ => {}
                }
            }
            // 用户确认删除本地收藏夹
            if self.operations_delete.swap(false, Ordering::SeqCst) {
                data.remove_collection(index)?;
                let _ = save_data();
                show_message(tl!("deleted")).ok();
                // 删除后选中项可能越界：没有合集时退回"显示全部"，否则夹到最后一个合集
                self.active_folder = if data.collection_uuids().is_empty() {
                    None
                } else {
                    self.active_folder.map(|it| it.min(data.collection_uuids().len() - 1))
                };
                FAV_PAGE_RESULT.with(|it| *it.borrow_mut() = Some(self.active_folder));
                self.rebuild_folders();
            }
            // 用户确认删除云端合集
            if self.cloud_delete.swap(false, Ordering::SeqCst) {
                let col_id = data.collection_by_index(index).id.unwrap();
                self.delete_from_cloud_task = Some(Task::new(async move {
                    recv_raw(Client::delete(format!("/collection/{col_id}"))).await?;
                    Ok(())
                }));
            }

            // 云端菜单
            if self.cloud_menu.changed() {
                match self.cloud_options[self.cloud_menu.selected()] {
                    "upload-to-cloud" => {
                        // 首次上传：用本地内容创建云端合集。标题/简介沿用本地，
                        // 公开性先取 `false`——默认不公开更保守，需要分享时再手动切换
                        let data = get_data();
                        let col = data.collection_by_index(index);
                        let Some(chart_ids) = Self::collect_chart_ids(&col, false) else {
                            return Ok(());
                        };

                        let body = CollectionContent {
                            name: col.name.clone(),
                            description: col.description.clone(),
                            charts: chart_ids,
                            public: false,
                        };
                        self.upload_task = Some(Task::new(async move {
                            let resp: Collection = recv_raw(Client::post("/collection", &body)).await?.json().await?;
                            Ok(resp)
                        }));
                    }
                    "delete-from-cloud" => {
                        // 只删云端对象；本地合集与其中的谱面保留（在 `delete_from_cloud_task` 完成后仅清空 id）
                        confirm_dialog(tl!("delete-from-cloud"), tl!("delete-from-cloud-confirm"), self.cloud_delete.clone());
                    }
                    "make-public" | "make-private" => {
                        // 公开性是二态开关，菜单只提供"切到相反状态"这一项，
                        // 目标值即当前值的取反（与点击时看到的一致）
                        let data = get_data();
                        let col = data.collection_by_index(index);
                        let col_id = col.id.unwrap();
                        let new_public = !col.public;
                        self.set_public_task = Some(Task::new(async move {
                            let resp =
                                recv_raw(Client::request(Method::PATCH, format!("/collection/{col_id}")).json(&CollectionPatch::Public(new_public)))
                                    .await?
                                    .json()
                                    .await?;
                            Ok(resp)
                        }));
                    }
                    "sync-to-cloud" => {
                        self.sync_to_cloud(false);
                    }
                    "sync-from-cloud" => {
                        let col_id = data.collection_by_index(index).id.unwrap();
                        self.sync_from_cloud_task = Some(Task::new(async move {
                            let resp: Collection = recv_raw(Client::get(format!("/collection/{col_id}"))).await?.json().await?;
                            Ok(Some(resp))
                        }));
                    }
                    _ => {}
                }
            }
            // 用户确认"以云端覆盖本地"：此时才把此前暂存的远端数据合并进来
            if self.sync_from_cloud.swap(false, Ordering::SeqCst) {
                if let Some(col) = self.new_data_from_cloud.take() {
                    let data = get_data();
                    let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                    let local = data.collection_info(&uuid);
                    // `merge` 会连同远端 updated 时间戳一起写回，作为下次冲突检测的基准
                    data.set_collection_info(&uuid, local.merge(&col))?;
                    show_message(tl!("synced")).ok();
                    self.rebuild_folders();
                }
            }
            // 用户确认强制上传：放弃冲突检测，用本地内容覆盖远端
            if self.force_sync_to_cloud.swap(false, Ordering::SeqCst) {
                self.sync_to_cloud(true);
            }
        }

        // 信息栏收起动画播完：重置为"已关闭"，让后续触摸回到正常分发路径
        if self.side_enter_time < 0. && -s.rt + INFO_TRANSIT < self.side_enter_time {
            self.side_enter_time = f32::INFINITY;
        }

        // 阶段六：轮询各网络任务并把结果合并回本地
        if let Some(task) = &mut self.upload_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(col) => {
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                        let mut local = data.collection_info(&uuid).as_ref().clone();
                        // 回填服务端分配的数字 id：本地这次才真正"认识"这个云端合集
                        local.id = Some(col.id);
                        data.set_collection_info(&uuid, local.merge(&col))?;
                        show_message(tl!("uploaded")).ok();
                        self.rebuild_folders();
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.upload_task = None;
            }
        }
        // 云端删除完成：清掉本地记录里的 id，使其退回"纯本地收藏夹"（本地内容与谱面都保留）
        if let Some(task) = &mut self.delete_from_cloud_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(()) => {
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                        let mut local = data.collection_info(&uuid).as_ref().clone();
                        local.id = None;
                        data.set_collection_info(&uuid, local)?;
                        show_message(tl!("deleted")).ok();
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.delete_from_cloud_task = None;
            }
        }
        // 公开性切换完成：以服务端返回的合集为准合并（服务端可能做了额外规范化）
        if let Some(task) = &mut self.set_public_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(col) => {
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                        let local = data.collection_info(&uuid);
                        data.set_collection_info(&uuid, local.merge(&col))?;
                        show_message(tl!("updated")).ok();
                        self.rebuild_folders();
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.set_public_task = None;
            }
        }
        // 同步到云端：`Ok(None)` 表示服务端 412（远端已被改动），弹确认框让用户选择是否强制覆盖
        if let Some(task) = &mut self.sync_to_cloud_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(Some(col)) => {
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                        let local = data.collection_info(&uuid);
                        data.set_collection_info(&uuid, local.merge(&col))?;
                        show_message(tl!("synced")).ok();
                        self.rebuild_folders();
                    }
                    Ok(None) => {
                        confirm_dialog(tl!("sync-to-cloud"), tl!("sync-outdated"), self.force_sync_to_cloud.clone());
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.sync_to_cloud_task = None;
            }
        }
        // 从云端拉取：远端 updated 与本地记录一致时说明内容未变，直接刷新谱面列表并提示"已是最新"；
        // 否则不能静默覆盖本地，先暂存结果再弹确认框
        if let Some(task) = &mut self.sync_from_cloud_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(Some(col)) => {
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                        let local = data.collection_info(&uuid);
                        if local.remote_updated == Some(col.updated) {
                            // 远端时间戳没变：只有谱面列表可能是本地缺失的，直接补上即可，不算冲突
                            data.set_collection_info(
                                &uuid,
                                LocalCollection {
                                    charts: col.charts.into_iter().map(Into::into).collect(),
                                    ..LocalCollection::clone(&local)
                                },
                            )?;
                            show_message(tl!("already-up-to-date")).ok();
                        } else {
                            confirm_dialog(tl!("sync-from-cloud"), tl!("sync-confirm"), self.sync_from_cloud.clone());
                            self.new_data_from_cloud = Some(col);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.sync_from_cloud_task = None;
            }
        }
        // 导入他人合集：新建一个本地收藏夹并立刻回填云端 id——它从诞生起就是"云端合集"
        if let Some(task) = &mut self.import_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(col) => {
                        let data = get_data_mut();
                        let mut local = LocalCollection::new(String::new());
                        local.id = Some(col.id);
                        data.push_collection(local.merge(&col))?;
                        let _ = save_data();
                        show_message(tl!("imported")).ok();
                        self.rebuild_folders();
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.import_task = None;
            }
        }
        // 批量导入完成：把拿到的谱面追加进当前合集；已上传的合集且非离线模式时顺带同步到云端
        if let Some(task) = &mut self.batch_import_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(charts) => {
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                        let mut col = data.collection_info(&uuid).as_ref().clone();
                        let online = col.id.is_some();
                        col.charts.extend(charts.into_iter().map(Into::into));
                        data.set_collection_info(&uuid, col)?;
                        show_message(tl!("imported")).ok();
                        if online && !data.config.offline_mode {
                            self.sync_to_cloud(false);
                        }
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.batch_import_task = None;
            }
        }
        // 设置封面完成：`Ok(Ok(_))` 是云端路径（合并服务端结果即可）；
        // `Ok(Err(_))` 是本地路径（只拿到插画文件，直接改本地 cover 字段并落盘）
        if let Some(task) = &mut self.set_cover_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(Ok(col)) => {
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                        let mut local = data.collection_info(&uuid).as_ref().clone();
                        local.id = Some(col.id);
                        data.set_collection_info(&uuid, local.merge(&col))?;
                        show_message(tl!("updated")).ok();
                        self.rebuild_folders();
                    }
                    Ok(Err(cover)) => {
                        let data = get_data();
                        let uuid = data.collection_uuids()[self.active_folder.unwrap()];
                        let mut col = data.collection_info(&uuid).as_ref().clone();
                        col.cover = CollectionCover::Online(cover);
                        data.set_collection_info(&uuid, col)?;
                        show_message(tl!("updated")).ok();
                        self.rebuild_folders();
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.set_cover_task = None;
            }
        }
        // 点赞状态查询完成：写入 3 分钟缓存（键为云端数字 id），避免频繁进出合集时反复请求
        if let Some(task) = &mut self.fetch_like_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(like) => {
                        if let Some(index) = self.active_folder {
                            let data = get_data();
                            let col = data.collection_by_index(index);
                            if let Some(id) = col.id {
                                LIKE_CACHE.with_borrow_mut(|cache| {
                                    cache.insert(id, (like, Utc::now()));
                                });
                            }
                        }
                        self.liked = like;
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.fetch_like_task = None;
            }
        }
        // 点赞提交完成：翻转本地状态并同步更新缓存——若不更新缓存，下次进入本页用缓存渲染时
        // 会短暂显示成修改前的旧状态
        if let Some(task) = &mut self.like_task {
            if let Some(result) = task.take() {
                match result {
                    Ok(()) => {
                        self.liked = !self.liked;
                        if let Some(index) = self.active_folder {
                            let data = get_data();
                            let col = data.collection_by_index(index);
                            if let Some(id) = col.id {
                                LIKE_CACHE.with_borrow_mut(|cache| {
                                    cache.insert(id, (self.liked, Utc::now()));
                                });
                            }
                        }
                    }
                    Err(err) => {
                        show_error(err);
                    }
                }
                self.like_task = None;
            }
        }

        Ok(())
    }

    /// 渲染整页：顶部工具条、卡片网格、底部按钮、信息侧栏与全屏 loading。
    ///
    /// # Errors
    /// 绘制本身不产生错误；保留 `Result` 仅为满足 `Page` 签名。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        // 先驱动所有卡片缩略图的异步加载，保证本帧画到的是最新一帧的图
        for folder in &self.folders {
            folder.cover.notify();
        }

        s.render_fader(ui, |ui| {
            let top = -ui.top + 0.13;
            let bottom = ui.top;
            let content_h = bottom - top;

            // 顶部工具条：只有选中真实合集（非"显示全部"）时才出现；
            // 自右向左依次是 更多操作 / 编辑 / 云端 / 信息 / 点赞
            if let Some(index) = self.active_folder {
                ui.scope(|ui| {
                    ui.dx(1. - 0.03);
                    ui.dy(-ui.top + 0.03);
                    let s = 0.08;
                    let r = Rect::new(-s, 0., s, s);
                    ui.fill_rect(r, (*self.icons.menu, r, ScaleType::Fit, WHITE));
                    self.operations_menu_btn.set(ui, r);
                    // 菜单须"先由渲染写入位置、再于下一帧展示"：`need_show_*` 标记完成这一拍的延迟
                    if self.need_show_operations_menu {
                        self.need_show_operations_menu = false;
                        self.operations_menu.set_bottom(true);
                        self.operations_menu.set_selected(usize::MAX);
                        let d = 0.28;
                        let h = self.operations_options.len().min(5) as f32 * 0.1;
                        self.operations_menu.show(ui, t, Rect::new(r.x - d, r.bottom() + 0.02, r.w + d, h));
                    }
                    // 编辑按钮仅对自己拥有的合集显示（系统合集的按钮直接不绘制，也就不可能被点到）
                    if get_data().collection_by_index(index).is_owned() {
                        ui.dx(-r.w - 0.03);
                        ui.fill_rect(r, (*self.icons.edit, r, ScaleType::Fit, WHITE));
                        self.edit_btn.set(ui, r);
                        if self.need_show_edit_menu {
                            self.need_show_edit_menu = false;
                            self.edit_menu.set_bottom(true);
                            self.edit_menu.set_selected(usize::MAX);
                            let d = 0.28;
                            let h = self.edit_options.len().min(5) as f32 * 0.1;
                            self.edit_menu.show(ui, t, Rect::new(r.x - d, r.bottom() + 0.02, r.w + d, h));
                        }
                    }
                    // 云端图标直接用"已同步/未同步"两种图案，让用户一眼看出这个合集在云上有无副本
                    ui.dx(-r.w - 0.03);
                    ui.fill_rect(
                        r,
                        (
                            if get_data().collection_by_index(index).id.is_some() {
                                *self.icons.cloud_check
                            } else {
                                *self.icons.cloud_none
                            },
                            r,
                            ScaleType::Fit,
                        ),
                    );
                    self.cloud_btn.set(ui, r);
                    if self.need_show_cloud_menu {
                        self.need_show_cloud_menu = false;
                        self.cloud_menu.set_bottom(true);
                        self.cloud_menu.set_selected(usize::MAX);
                        let d = 0.28;
                        let h = self.cloud_options.len().min(5) as f32 * 0.1;
                        self.cloud_menu.show(ui, t, Rect::new(r.x - d, r.bottom() + 0.02, r.w + d, h));
                    }
                    ui.dx(-r.w - 0.03);
                    ui.fill_rect(r, (*self.icons.info, r, ScaleType::Fit));
                    self.info_btn.set(ui, r);

                    ui.dx(-r.w - 0.03);
                    // 点赞位：查询中显示转圈占位（此时不可点）；已上传的合集显示可点击的心形，
                    // 已点赞用橙色填充加以区分
                    if self.fetch_like_task.is_some() {
                        ui.fill_rect(r, (*self.icons.heart_outline, r, ScaleType::Fit, semi_white(0.4)));
                        let ct = r.center();
                        ui.loading(
                            ct.x,
                            ct.y,
                            t,
                            WHITE,
                            LoadingParams {
                                radius: r.w * 0.35,
                                width: 0.008,
                                ..Default::default()
                            },
                        );
                    } else if get_data().collection_by_index(index).id.is_some() {
                        let icon = if self.liked { &self.icons.heart } else { &self.icons.heart_outline };
                        ui.fill_rect(r, (**icon, r, ScaleType::Fit, if self.liked { ORANGE } else { WHITE }));
                        self.like_btn.set(ui, r);
                    }
                });
            }

            // 卡片网格滚动区域：宽度固定为 2（与布局坐标系一致），高度随内容变化
            self.scroll.size((2., content_h));

            ui.scope(|ui| {
                ui.dx(-1.);
                ui.dy(top);
                self.scroll.render(ui, |ui| {
                    let start_x = 0.12;
                    let mut x = start_x;
                    let mut y = 0.02;
                    let max_x = 2.0 - 0.12;
                    // 按可用宽度算每行列数（至少 1 列），保证窄屏下不会因为算出 0 列而陷入死循环排版
                    let cols = ((max_x - start_x + CARD_PAD) / (CARD_WIDTH + CARD_PAD)).floor() as usize;
                    let cols = cols.max(1);

                    // 逐卡片排版：横向步进；每到整行末尾就回到起始 x 并下移一行
                    for (idx, folder) in self.folders.iter_mut().enumerate() {
                        if idx > 0 && idx % cols == 0 {
                            x = start_x;
                            y += CARD_HEIGHT + CARD_PAD;
                        }

                        let r = Rect::new(x, y, CARD_WIDTH, CARD_HEIGHT);
                        folder.btn.set(ui, r);
                        // 选中态：在卡片外扩一圈白边，比填充色更不易与封面图冲突
                        if self.active_folder == folder.index {
                            ui.fill_rect(r.feather(0.005), WHITE);
                        }

                        let illu_r = r;
                        let alpha = folder.cover.alpha(t);
                        if alpha > 0. {
                            ui.fill_rect(illu_r, folder.cover.shading(illu_r, t));
                        }
                        ui.fill_rect(illu_r, semi_black(0.3));
                        let mut name_max_w = r.w;
                        // 默认合集在右下角打一个橙色角标；同时压缩标题可用宽度，避免文字压到角标上
                        if folder.index.is_some_and(|it| get_data().collection_by_index(it).is_default) {
                            let mut text = ui.text(tl!("default")).size(0.5).color(WHITE);
                            let mut text_r = text.measure();
                            let pad_x = 0.02;
                            let pad_y = 0.01;
                            text_r.x = r.right() - text_r.w - pad_x;
                            text_r.y = r.bottom() - text_r.h - pad_y;
                            text.ui.fill_rect(text_r.nonuniform_feather(pad_x, pad_y), ORANGE);
                            text.pos(text_r.x, text_r.y).draw();
                            name_max_w -= text_r.w + pad_x * 2.;
                        }

                        ui.text(&folder.name)
                            .pos(r.x + 0.03, r.bottom() - 0.03)
                            .anchor(0., 1.)
                            .no_baseline()
                            .size(0.42)
                            .max_width(name_max_w)
                            .color(semi_white(0.9))
                            .draw();

                        // 谱面数量 || Chart count
                        if let Some(index) = folder.index {
                            let count = get_data().collection_by_index(index).charts.len();
                            ui.text(count.to_string())
                                .pos(r.right() - 0.02, r.y + 0.02)
                                .anchor(1., 0.)
                                .size(0.35)
                                .color(semi_white(0.6))
                                .draw();
                        }

                        x += CARD_WIDTH + CARD_PAD;
                    }

                    let total_h = y + CARD_HEIGHT + CARD_PAD + 0.1;
                    (2., total_h)
                });
            });

            // 新建收藏夹按钮 || Create folder button
            // 底部两个按钮固定贴右下角；先画"新建"，再左移一格画"导入"
            let btn_w = 0.28;
            let btn_h = 0.1;
            let mut btn_r = Rect::new(1.0 - btn_w - 0.04, bottom - btn_h - 0.02, btn_w, btn_h);
            let ct = btn_r.center();
            self.create_btn.render_shadow(ui, btn_r, t, |ui, path| {
                ui.fill_path(&path, semi_black(0.5));
            });
            ui.text(tl!("create"))
                .pos(ct.x, ct.y)
                .anchor(0.5, 0.5)
                .no_baseline()
                .size(0.45)
                .color(semi_white(0.9))
                .draw();

            btn_r.x -= btn_w + 0.02;
            let ct = btn_r.center();
            self.import_btn.render_shadow(ui, btn_r, t, |ui, path| {
                ui.fill_path(&path, semi_black(0.5));
            });
            ui.text(tl!("import"))
                .pos(ct.x, ct.y)
                .anchor(0.5, 0.5)
                .no_baseline()
                .size(0.45)
                .color(semi_white(0.9))
                .draw();
        });

        // 信息侧栏：按 `side_enter_time` 的正负与进度插值出滑入/滑出位移（三次缓出），
        // 同时按同一进度给背景压一层半透明黑，做出"聚焦信息栏"的效果
        let rt = s.rt;
        if self.side_enter_time.is_finite() {
            let p = ((rt - self.side_enter_time.abs()) / INFO_TRANSIT).min(1.);
            let p = 1. - (1f32 - p).powi(3);
            let p = if self.side_enter_time < 0. { 1. - p } else { p };
            ui.fill_rect(ui.screen_rect(), semi_black(p * 0.6));
            let w = INFO_WIDTH;
            let lf = f32::tween(&1.04, &(1. - w), p);
            ui.scope(|ui| {
                ui.dx(lf);
                ui.dy(-ui.top);
                let r = Rect::new(-0.2, 0., 0.2 + w, ui.top * 2.);
                ui.fill_rect(r, (Color::default(), (r.x, r.y), Color::new(0., 0., 0., p * 0.7), (r.right(), r.y)));
                self.render_info(ui, rt);
            });
        }

        // 三个下拉菜单在页面内容之上绘制
        self.edit_menu.render(ui, t, 1.);
        self.operations_menu.render(ui, t, 1.);
        self.cloud_menu.render(ui, t, 1.);

        // 有网络任务时用全屏 loading 遮罩：既提示"正在处理"，也用视觉方式解释为何点击无响应
        if self.has_task() {
            ui.full_loading_simple(t);
        }

        Ok(())
    }

    /// 在场景顶层重绘信息侧栏。
    ///
    /// `render` 只负责本页自身的内容；信息栏必须盖在页面栈之上（尤其是从封面选择页返回时），
    /// 所以这里把同一段绘制逻辑再执行一次，用 `render_top` 这个更晚的绘制时机保证层级。
    ///
    /// # Errors
    /// 绘制本身不产生错误；保留 `Result` 仅为满足 `Page` 签名。
    fn render_top(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let rt = s.rt;
        if self.side_enter_time.is_finite() {
            let p = ((rt - self.side_enter_time.abs()) / INFO_TRANSIT).min(1.);
            let p = 1. - (1f32 - p).powi(3);
            let p = if self.side_enter_time < 0. { 1. - p } else { p };
            ui.fill_rect(ui.screen_rect(), semi_black(p * 0.6));
            let w = INFO_WIDTH;
            let lf = f32::tween(&1.04, &(1. - w), p);
            ui.scope(|ui| {
                ui.dx(lf);
                ui.dy(-ui.top);
                let r = Rect::new(-0.2, 0., 0.2 + w, ui.top * 2.);
                ui.fill_rect(r, (Color::default(), (r.x, r.y), Color::new(0., 0., 0., p * 0.7), (r.right(), r.y)));
                self.render_info(ui, rt);
            });
        }

        self.sf.render(ui, s.t);
        Ok(())
    }

    /// 取出并清空待执行的页面跳转指令。
    ///
    /// 用 `take()` 保证一次跳转只被消费一次；没有待处理指令时返回 `NextPage::default()`
    /// （即不跳转），因此调用方无需判断 `Option`。
    fn next_page(&mut self) -> NextPage {
        self.next_page.take().unwrap_or_default()
    }
}
