//! 用户资料页（[`ProfileScene`]）。
//!
//! 展示某个用户（可能本人，也可能他人）的公开资料与成绩记录；当查看的是自己时，
//! 额外提供账号操作入口：登出、删除账号、上传头像，以及（`hykb` feature 下的）
//! 好游快爆渠道绑定 / 解绑 / 迁移。
//!
//! 「是否本人」由 `id` 与全局登录态 `get_data().me` 比较得出（见 `touch`、`render`），
//! 这是本页几乎所有写操作的前提：他人的资料页是**只读**的，不渲染登出/删除按钮，
//! 头像也不可点击。
//!
//! 数据获取一律是「构造时派发异步任务 + 每帧轮询」：`new` 里就把用户信息
//! （`Client::load`）与成绩列表（`GET /record?player=<id>`）的请求发出去，
//! 之后每次 `update` 用 `Task::take` 检查是否完成，完成后把句柄置回 `None`
//! 以免重复处理。成绩列表是**全量**返回，没有分页参数。

prpr_l10n::tl_file!("profile");

#[cfg(feature = "hykb")]
use super::confirm_dialog;
use super::{TEX_BACKGROUND, TEX_ICON_BACK};
use crate::{
    client::{recv_raw, Client, Record, User, UserManager},
    get_data, get_data_mut, hykb_logout,
    page::{Fader, Illustration, SFader},
    save_data, sync_data,
};
use anyhow::Result;
use chrono::Local;
#[cfg(feature = "hykb")]
use inputbox::InputBox;
use macroquad::prelude::*;
#[cfg(feature = "hykb")]
use prpr::scene::{request_input, return_input, take_input};
use prpr::{
    ext::{open_url, semi_black, semi_white, RectExt, SafeTexture, ScaleType, BLACK_TEXTURE},
    judge::icon_index,
    scene::{request_file, return_file, show_error, show_message, take_file, NextScene, Scene},
    task::Task,
    time::TimeManager,
    ui::{button_hit, rounded_rect_shadow, DRectButton, Dialog, RectButton, Scroll, ShadowConfig, Ui},
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Notify;

/// 成绩列表中的一行。
///
/// 服务端返回的记录里只带惰性引用（`Chart`/`User`），因此曲名与曲绘都要再异步取一次：
/// 这里把两件事拆成 `name`（文本）与 `illu`（图片）两个独立任务，各自的就绪状态互不影响，
/// 渲染时分别判断，能先出字的就先出字。
struct RecordItem {
    /// 服务端返回的完整成绩记录（分数、FC 标记、谱面引用等）。
    record: Record,
    /// 惰性获取谱面名称的任务。`Task::get` 在未完成时返回 `None`，
    /// 因此渲染处用 `if let Some(Ok(name))` 才画标题。
    name: Task<Result<String>>,
    /// 整行的点击热区。当前只用于「点中即停止滚动惯性」，不做页面跳转。
    btn: DRectButton,
    /// 谱面曲绘缩略图，走 `Illustration` 的惰性加载：只有被 `notify()` 放行后
    /// （即该行进入可视区域）才会真正读盘解码。
    illu: Illustration,
}

/// 用户资料页场景。
///
/// 一个场景同时承担两种形态：查看他人（只读）与查看自己（可写）。二者共用同一套
/// 资料与成绩渲染，差别只在下方按钮组与头像可点性上。
pub struct ProfileScene {
    /// 被查看用户的 id。它既是请求参数，也是「是否本人」判据的一侧。
    id: i32,
    /// 用户信息；`None` 表示仍在加载（此时页面只显示一个加载圈）。
    user: Option<Arc<User>>,
    /// 已本地化好的徽章展示名列表。原始标识在 `update` 里被翻译一次后缓存，
    /// 避免每帧重新查表。
    user_badges: Vec<String>,

    /// 左栏资料区的滚动容器（签名过长时可用）。
    pf_scroll: Scroll,

    /// 全局背景纹理（来自上层共享的 `TEX_BACKGROUND`）。
    background: SafeTexture,

    /// 返回按钮图标。
    icon_back: SafeTexture,
    /// 默认头像占位纹理；用户没有头像（或头像尚未加载完）时用它兜底。
    icon_user: SafeTexture,

    /// 左上角返回按钮。
    btn_back: RectButton,
    /// 用户名文字的热区：点一下把昵称复制到剪贴板。
    btn_name: RectButton,
    /// 「在网页中打开」按钮，跳转到 phira.moe 的用户主页。
    btn_open_web: DRectButton,
    /// 登出按钮（仅在查看自己时渲染）。
    btn_logout: DRectButton,
    /// 删除账号按钮（仅在查看自己时渲染，点击后需二次确认）。
    btn_delete: DRectButton,
    // 以下四个字段与两个任务只有启用 `hykb` feature 时存在——好游快爆渠道是可选接入，
    // 不含该 feature 的构建里整组字段被编译掉，相应地也就没有绑定/迁移入口。
    #[cfg(feature = "hykb")]
    /// 「绑定 / 解绑好游快爆」按钮。
    btn_hykb: DRectButton,
    #[cfg(feature = "hykb")]
    /// 进行中的好游快爆操作（绑定/解绑）。非 `None` 时按钮被禁用并显示全屏加载。
    hykb_task: Option<Task<Result<()>>>,
    #[cfg(feature = "hykb")]
    /// 解绑确认对话框写回的标志位。按钮回调与 `update` 之间不能直接共享可变状态，
    /// 故用原子布尔做一次性信号（`fetch_and(false)` 消费）。
    should_unbind_hykb: Arc<AtomicBool>,
    #[cfg(feature = "hykb")]
    /// 「迁移绑定」按钮（仅纯 HYKB 账号即「已绑定且无邮箱」时渲染）。
    btn_transfer: DRectButton,
    #[cfg(feature = "hykb")]
    /// 进行中的迁移请求。
    transfer_task: Option<Task<Result<()>>>,

    /// 用户信息的加载任务（`Client::load`）。完成后取出结果并置回 `None`。
    load_task: Option<Task<Result<Arc<User>>>>,

    /// 头像区域的热区（仅本人可点，点击后弹文件选择框）。
    avatar_btn: RectButton,
    /// 头像上传任务：读文件 → 上传 → 提交到 `/edit/avatar`。
    avatar_task: Option<Task<Result<()>>>,

    /// 删除账号的二次确认标志位，语义同 `should_unbind_hykb`。
    should_delete: Arc<AtomicBool>,
    /// 删除账号的请求任务。
    delete_task: Option<Task<Result<()>>>,

    /// 右栏成绩列表的滚动容器。
    scroll: Scroll,
    /// 成绩列表的加载任务：一次请求拉回该玩家全部成绩并组装成 [`RecordItem`]。
    record_task: Option<Task<Result<Vec<RecordItem>>>>,
    /// 已就绪的成绩行；`None` 表示仍在加载（右栏显示加载圈）。
    record_items: Option<Vec<RecordItem>>,

    /// 本场景的入场/出场淡入淡出。
    sf: SFader,
    /// 成绩卡片「逐层错峰淡入」的动画器：卡片就绪后按行依次淡入，而不是整屏同时出现。
    fader: Fader,

    /// 判定等级图标（用于成绩行的评分图标），由上层共享。
    rank_icons: [SafeTexture; 8],
}

// 构造逻辑。本页的构造不阻塞：它只做「登记要发的请求」和「取现成的全局纹理」，
// 真正的用户信息与成绩数据都在随后的 `update` 里逐帧结算，因此从曲目页点进资料页
// 立刻就能看到界面（先是加载圈，再逐步填充）。写操作（头像/删除/渠道）所需的状态
// 也在这里初始化：任务句柄一律为 None 表示「没有进行中的操作」。
impl ProfileScene {
    /// 创建资料页并派发数据请求。
    ///
    /// # Arguments
    /// - `id`：被查看用户的 id，可能等于当前登录用户（决定页面是否可写）；
    /// - `icon_user`：默认头像占位纹理，在真实头像就绪前顶替；
    /// - `rank_icons`：判定等级图标，由上层共享，避免重复解码。
    pub fn new(id: i32, icon_user: SafeTexture, rank_icons: [SafeTexture; 8]) -> Self {
        // 阶段一：预取用户信息。这里发两条互不冲突的请求——
        // `UserManager::request` 维护一份轻量的「昵称 + 颜色 + 头像」缓存，供页面在正文
        // 就绪前先渲染出名字与头像；`load_task` 则通过对象缓存拿完整的 `User`。
        // 两者都带缓存，重复进入同一资料页通常不会真的产生网络往返。
        UserManager::request(id);
        let load_task = Some(Task::new(Client::load(id)));
        Self {
            id,
            user: None,
            user_badges: Vec::new(),

            pf_scroll: Scroll::new(),

            // 阶段二：取全局共享纹理。二者都是 thread_local，由上层场景在启动时初始化，
            // 因此这里 `unwrap` 是安全的（未初始化即属于程序装配错误）。
            background: TEX_BACKGROUND.with(|it| it.borrow().clone().unwrap()),

            icon_back: TEX_ICON_BACK.with(|it| it.borrow().clone().unwrap()),
            icon_user,

            // 各类按钮/热区先建空壳，矩形在每帧 `render` 时回填。
            btn_back: RectButton::new(),
            btn_name: RectButton::new(),
            btn_open_web: DRectButton::new(),
            btn_logout: DRectButton::new(),
            btn_delete: DRectButton::new(),
            #[cfg(feature = "hykb")]
            btn_hykb: DRectButton::new(),
            #[cfg(feature = "hykb")]
            hykb_task: None,
            #[cfg(feature = "hykb")]
            should_unbind_hykb: Arc::default(),
            #[cfg(feature = "hykb")]
            btn_transfer: DRectButton::new(),
            #[cfg(feature = "hykb")]
            transfer_task: None,

            load_task,

            avatar_btn: RectButton::new(),
            avatar_task: None,

            should_delete: Arc::default(),
            delete_task: None,

            scroll: Scroll::new(),
            // 阶段三：成绩列表任务。一次性拉回该玩家的**全部**成绩（接口无分页），
            // 随后在 `map` 里为每一行挂上两个惰性任务，使列表能边显示边补全细节。
            record_task: Some(Task::new(async move {
                let records: Vec<Record> = recv_raw(Client::get(format!("/record?player={id}"))).await?.json().await?;
                Ok(records
                    .into_iter()
                    .map(|it| {
                        // 曲绘用「闸门 + 结算」两段式：这里只登记一个等待 `notify` 的任务
                        // 并以黑图占位；等该行进入可视区域、渲染时调用 `notify()` 放行，
                        // 才真正去拉取谱面数据并解码缩略图。这样长列表不会一次性下载全部封面。
                        let illu = {
                            let chart = it.chart.clone();
                            let notify = Arc::new(Notify::new());
                            Illustration {
                                texture: (BLACK_TEXTURE.clone(), BLACK_TEXTURE.clone()),
                                notify: Arc::clone(&notify),
                                task: Some(Task::new({
                                    async move {
                                        notify.notified().await;
                                        let illu = &chart.fetch().await?.illustration;
                                        Ok((illu.load_thumbnail().await?, None))
                                    }
                                })),
                                loaded: Arc::default(),
                                load_time: f32::NAN,
                            }
                        };
                        // 曲名单独一个任务，与曲绘无关：名字先到就先显示，
                        // 不必等封面解码完。
                        let chart = it.chart.clone();
                        RecordItem {
                            record: it,
                            name: Task::new(async move { Ok(chart.fetch().await?.name.clone()) }),
                            btn: DRectButton::new(),
                            illu,
                        }
                    })
                    .collect())
            })),
            record_items: None,

            // 阶段四：动画器。`sf` 负责本页进出的整屏淡入淡出；
            // `fader` 负责成绩卡片逐行错峰淡入，位移取 0.12（比页面栈转场更克制，
            // 因为这里是同一页内的内容出现，而非页面切换）。
            sf: SFader::new(),
            fader: Fader::new().with_distance(0.12),

            rank_icons,
        }
    }
}

// 本场景在场景栈中的行为约定：
// - `enter`：启动整屏淡入（交给 `SFader`），不改时间轴——本页的动画都以本场景时间为基准；
// - `pause`/`resume`/`touch` 之外的钩子：未实现，本页没有音视频，也不需要被系统暂停；
// - `update`：全部异步任务的结算中心，顺序为「滚动 → 用户信息 → 头像上传 → 删除账号 →
//   渠道绑定 → 迁移输入 → 成绩列表 → 二次确认标志位 → 曲绘结算」。任务用 `Task::take`
//   取结果，取到后立即把句柄置回 `None`，因此每项操作只会被处理一次；
// - `touch`：见方法注释。整页在淡入淡出或头像上传期间会吞掉所有输入；
// - `render`：左栏资料卡（头像/昵称/等级/签名/徽章/按钮组）+ 右栏成绩网格；
// - `next_scene`：完全由 `SFader` 决定，本页不接受其它场景的回传结果（没有 `on_result`）。
impl Scene for ProfileScene {
    /// 启动整屏淡入。
    ///
    /// `_target` 未使用——本页不渲染到离屏目标。
    fn enter(&mut self, tm: &mut TimeManager, _target: Option<RenderTarget>) -> Result<()> {
        self.sf.enter(tm.now() as _);
        Ok(())
    }

    /// 每帧推进入场动画、两个滚动容器，并结算所有进行中的异步任务。
    ///
    /// # Errors
    /// 头像上传成功后清理用户缓存、好游快爆绑定后写入本地数据（`save_data`）等环节
    /// 会返回错误；这些错误直接上抛，通常表现为一条由上层处理的操作失败提示。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        let t = tm.now() as f32;

        self.pf_scroll.update(t);
        self.scroll.update(t);

        // 用户信息就绪：把徽章标识翻译成展示名并缓存起来。
        // admin/sponsor 这两个内置身份有专属文案；其余走服务端下发的
        // `badge_names` 映射，映射里查不到就原样显示标识，避免整条徽章丢失。
        if let Some(task) = &mut self.load_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err.context(tl!("load-user-failed"))),
                    Ok(res) => {
                        self.user_badges.clear();
                        for badge in &res.badges {
                            match badge.as_str() {
                                "admin" => self.user_badges.push(tl!("badge-admin").into_owned()),
                                "sponsor" => self.user_badges.push(tl!("badge-sponsor").into_owned()),
                                _ => self
                                    .user_badges
                                    .push(res.badge_names.get(badge).cloned().unwrap_or_else(|| badge.clone())),
                            }
                        }
                        self.user = Some(res);
                    }
                }
                self.load_task = None;
            }
        }
        // 文件选择回调。`id` 用于区分请求方：本页只认领 `"avatar"`，
        // 其它 id（可能是别的场景发起的导入/导出）必须原样 `return_file` 还回去，
        // 否则会把不属于本页的回调吞掉。
        if let Some((id, file)) = take_file() {
            if id == "avatar" {
                // 头像上传三步：读本地文件 → 上传取得文件 id → 提交到 `/edit/avatar`
                // 使其成为该用户的头像。
                self.avatar_task = Some(Task::new(async move {
                    let id = Client::upload_file("avatar", std::fs::read(file)?).await?;
                    recv_raw(Client::post("/edit/avatar", &json!({ "file": id }))).await?;
                    Ok(())
                }));
            } else {
                return_file(id, file);
            }
        }
        // 头像上传结算。成功时必须清掉**两层**缓存再重新请求：对象缓存（`Client`）与
        // 展示缓存（`UserManager`，持有昵称/颜色/头像纹理），只清一层会出现
        // 「资料页更新了但列表里仍是旧头像」的不一致。
        if let Some(task) = &mut self.avatar_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("edit-avatar-failed")));
                    }
                    Ok(_) => {
                        show_message(tl!("edit-avatar-success")).ok();
                        let id = get_data().me.as_ref().unwrap().id;
                        Client::clear_cache::<User>(id)?;
                        UserManager::clear_cache(id)?;
                        UserManager::request(id);
                    }
                }
                self.avatar_task = None;
            }
        }

        // 账号删除结算。这里只提示「请求已提交」：删除是服务端异步流程，
        // 本页不会立刻清掉本地登录态（否则用户会以为已经删完）。
        if let Some(task) = &mut self.delete_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("delete-failed")));
                    }
                    Ok(_) => {
                        show_message(tl!("delete-req-sent")).ok();
                    }
                }
                self.delete_task = None;
            }
        }

        // 好游快爆绑定/解绑结算（`hykb` feature）。
        // 关键不变式：`bound` 在此刻读取，而绑定/解绑任务内部已经把 `get_data_mut().me`
        // 刷成了**动作之后**的状态，因此这里的 `bound` 判的是结果而非请求前的状态——
        // 这也是成功提示能区分「绑定成功 / 解绑成功」的依据。
        #[cfg(feature = "hykb")]
        if let Some(task) = &mut self.hykb_task {
            if let Some(res) = task.take() {
                let bound = get_data().me.as_ref().and_then(|it| it.hykb_uid).is_some();
                match res {
                    Err(err) => show_error(err.context(tl!("hykb-action-failed"))),
                    // `bound` now reflects the post-action state (me was refreshed).
                    Ok(_) => {
                        show_message(if bound { tl!("hykb-bind-success") } else { tl!("hykb-unbind-success") }).ok();
                        Client::clear_cache::<User>(self.id)?;
                        UserManager::clear_cache(self.id)?;
                        UserManager::request(self.id);
                    }
                }
                self.hykb_task = None;
            }
        }

        // 文本输入回调。本页只认领 `transfer-email`（迁移绑定要填的目标邮箱）；
        // 空串直接丢弃而不发请求，避免误触提交一个空邮箱；其它 id 原样还回去。
        #[cfg(feature = "hykb")]
        if let Some((id, text)) = take_input() {
            if id == "transfer-email" {
                let email = text.trim().to_owned();
                if !email.is_empty() {
                    self.transfer_task = Some(Task::new(async move {
                        Client::transfer_request(&email).await?;
                        Ok(())
                    }));
                }
            } else {
                return_input(id, text);
            }
        }

        // 迁移申请结算。成功不直接改本地绑定状态，而是提示「邮件已发送」：
        // 迁移需要用户去目标邮箱确认，确认前绑定关系不变。
        #[cfg(feature = "hykb")]
        if let Some(task) = &mut self.transfer_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err.context(tl!("transfer-failed"))),
                    Ok(_) => {
                        Dialog::plain(tl!("hykb-transfer"), tl!("transfer-email-sent")).show();
                    }
                }
                self.transfer_task = None;
            }
        }

        // 成绩列表就绪：一次性替换整个列表，并从当前时刻启动逐行错峰淡入。
        // 列表是全量的，因此这里没有「追加一页」的分支。
        if let Some(task) = &mut self.record_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err.context(tl!("load-record-failed"))),
                    Ok(val) => {
                        self.record_items = Some(val);
                        self.fader.sub(t);
                    }
                }
                self.record_task = None;
            }
        }

        // 删除账号的确认结果。对话框回调无法直接改场景状态，只能置位这个原子布尔；
        // `fetch_and(false)` 既读取又清空，天然保证「一次确认只发一次请求」。
        // 请求只校验 HTTP 状态（`error_for_status`），响应体无意义。
        if self.should_delete.fetch_and(false, Ordering::Relaxed) {
            self.delete_task = Some(Task::new(async move {
                Client::post("/delete-account", &()).send().await?.error_for_status()?;
                Ok(())
            }));
        }

        // 解绑好游快爆的确认结果。除了同样的「一次性消费」语义，还额外要求当前没有
        // 进行中的渠道任务（`hykb_task.is_none()`），否则会把正跑着的任务句柄覆盖掉。
        // 解绑后必须重新拉一次 `me` 并持久化：本地登录态里的 `hykb_uid` 是按钮文案与
        // 是否显示「迁移」入口的依据。
        #[cfg(feature = "hykb")]
        if self.hykb_task.is_none() && self.should_unbind_hykb.fetch_and(false, Ordering::Relaxed) {
            self.hykb_task = Some(Task::new(async move {
                Client::unbind_hykb().await?;
                let me = Client::get_me().await?;
                get_data_mut().me = Some(me);
                save_data()?;
                Ok(())
            }));
        }

        // 推进每一行的曲绘加载：结算已完成的任务、更新淡入进度。
        // 注意这里并不判断可视性——「是否需要加载」由渲染阶段在卡片真正可见时才
        // 通过 `notify()` 放行。
        if let Some(items) = &mut self.record_items {
            for item in items {
                item.illu.settle(t);
            }
        }

        Ok(())
    }

    /// 处理一次触摸，按「返回 → 昵称 → 外部网页 → 登出 → 删除 → 渠道操作 → 头像 →
    /// 滚动/成绩行」的顺序分发。
    ///
    /// 两道前置短路：整页正在淡入淡出、或头像正在上传时，一律吞掉输入（返回 `true`）。
    /// 前者避免过渡途中触发第二次页面切换，后者避免上传期间玩家重复触发其它写操作。
    ///
    /// 所有写操作按钮（登出/删除/渠道/头像）只在查看自己时才会被 `render` 出矩形，
    /// 因此这里的命中判断天然只对本人生效——不过头像上传在命中之外还额外判了一次
    /// `me.id == self.id`，属于双重保险。
    ///
    /// # Errors
    /// 「在网页中打开」可能因平台不支持而失败，错误直接上抛。
    fn touch(&mut self, tm: &mut TimeManager, touch: &Touch) -> Result<bool> {
        // 过渡动画期间不接受输入。
        if self.sf.transiting() {
            return Ok(true);
        }
        // 头像上传期间不接受输入，避免并发触发其它账号操作。
        if self.avatar_task.is_some() {
            return Ok(true);
        }
        let t = tm.now() as f32;
        if self.pf_scroll.touch(touch, t) {
            return Ok(true);
        }
        // 返回上一页（资料页通常是从首页/曲库推动进来的）。
        if self.btn_back.touch(touch) {
            button_hit();
            self.sf.next(t, NextScene::Pop);
            return Ok(true);
        }
        // 点昵称即复制到剪贴板（方便分享/搜索），用户信息未就绪时不做事但仍消费事件。
        if self.btn_name.touch(touch) {
            if let Some(user) = &self.user {
                // SAFETY: `get_internal_gl` 是绕过 Rust 借用检查、直接访问全局 GL/窗口
                // 上下文的逃生舱，只能在持有该上下文的线程（主线程）中调用；本函数由主线程
                // 每帧调用，且这里仅向剪贴板写字符串，不触碰任何 GL 资源或跨线程共享状态。
                unsafe { get_internal_gl() }.quad_context.clipboard_set(&user.name);
                show_message(tl!("name-copied")).ok();
            }
            return Ok(true);
        }
        // 外部浏览器打开网页版主页（服务端 id 与本地 id 同源）。
        if self.btn_open_web.touch(touch, t) {
            open_url(&format!("https://phira.moe/user/{}", self.id))?;
            return Ok(true);
        }
        // 登出：清掉 HYKB 原生会话（非 HYKB/非 Android 构建为空实现）、内存中的
        // 用户对象与 token，再落盘 + 同步给网络客户端，最后退出本页回到未登录态。
        // 注意 `save_data` 用 `let _ =` 忽略失败：即便如此也要完成登出流程，
        // 不能因为写盘失败把玩家留在已登录界面。
        if self.btn_logout.touch(touch, t) {
            hykb_logout();
            get_data_mut().me = None;
            get_data_mut().tokens = None;
            let _ = save_data();
            sync_data();
            show_message(tl!("logged-out")).ok();
            self.sf.next(t, NextScene::Pop);
            return Ok(true);
        }
        // 删除账号：先弹二次确认（带 5 秒倒计时，防止误触），确认结果通过共享原子
        // 布尔回传，真正的请求在 `update` 里发出。回调返回 `false` 表示两个按钮
        // 点击后都关闭对话框。
        if self.btn_delete.touch(touch, t) {
            let res = self.should_delete.clone();
            Dialog::plain(ttl!("del-confirm").into_owned(), tl!("delete-confirm").into_owned())
                .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
                .countdown(5)
                .listener(move |_dialog, id| {
                    if id == 1 {
                        res.store(true, Ordering::SeqCst);
                    }
                    false
                })
                .show();
            return Ok(true);
        }
        // 渠道按钮（`hykb` feature）：同一个按钮承担「绑定」与「解绑」两种语义，
        // 以当前登录态里有没有 `hykb_uid` 区分。
        // - 已绑定：走二次确认（结果写入 `should_unbind_hykb`），避免一键解绑；
        // - 未绑定：立即发起绑定，先向宿主渠道 SDK 取凭证（这一步可能弹渠道登录/选择器，
        //   失败即中止），再交给服务端绑定，最后重新拉 `me` 并落盘。
        // 前置的 `hykb_task.is_none()` 保证操作进行中不会重复触发。
        #[cfg(feature = "hykb")]
        if self.hykb_task.is_none() && self.btn_hykb.touch(touch, t) {
            let bound = get_data().me.as_ref().and_then(|it| it.hykb_uid).is_some();
            if bound {
                confirm_dialog(tl!("hykb-unbind").into_owned(), tl!("hykb-unbind-confirm").into_owned(), Arc::clone(&self.should_unbind_hykb));
            } else {
                self.hykb_task = Some(Task::new(async move {
                    let cred = crate::obtain_hykb_credential().await?.ok_or_err()?;
                    Client::bind_hykb(cred.uid, &cred.access_token).await?;
                    let me = Client::get_me().await?;
                    get_data_mut().me = Some(me);
                    save_data()?;
                    Ok(())
                }));
            }
            return Ok(true);
        }
        // 迁移入口：只是拉起输入框（异步、跨帧），填好的邮箱在 `update` 的 `take_input`
        // 里被取出并发起迁移申请。仅纯 HYKB 账号（已绑定且无邮箱）会渲染这个按钮。
        #[cfg(feature = "hykb")]
        if self.transfer_task.is_none() && self.btn_transfer.touch(touch, t) {
            request_input("transfer-email", InputBox::new().title(tl!("hykb-transfer")).prompt(tl!("transfer-prompt")));
            return Ok(true);
        }
        // 头像上传入口：仅当查看的是自己时才生效，点击后拉起系统文件选择框
        // （异步、跨帧），选中的文件在 `update` 的 `take_file` 里处理。
        if get_data().me.as_ref().is_some_and(|it| it.id == self.id) && self.avatar_btn.touch(touch) {
            request_file("avatar");
            return Ok(true);
        }

        if self.scroll.touch(touch, t) {
            return Ok(true);
        }
        // 成绩行命中：目前只做「停止滚动惯性」（点一下就定住列表，便于查看），
        // 不跳转到成绩详情。
        if let Some(items) = &mut self.record_items {
            for item in items {
                if item.btn.touch(touch, t) {
                    self.scroll.y_scroller.halt();

                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    /// 绘制整页。
    ///
    /// 层级顺序：全屏背景 → 左栏资料卡（底板 + 头像/昵称/等级/签名/徽章 → 操作按钮组）
    /// → 右栏成绩网格 → 场景过渡遮罩 → 操作中提示。
    /// 用户信息未就绪时，左右两栏各自显示一个加载圈，而不是留白。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        set_camera(&ui.camera());
        let t = tm.now() as f32;

        // 阶段一：全屏背景与返回按钮。
        let r = ui.screen_rect();
        ui.fill_rect(r, (*self.background, r));
        let r = ui.back_rect();
        ui.fill_rect(r, (*self.icon_back, r));
        self.btn_back.set(ui, r);

        // 阶段二：左栏资料卡底板。固定宽度 0.6、高度给到 2（远超屏幕），靠滚动裁剪
        // 表现出「内容多时可就地滚动」；底色取 UI 主题背景色，外加深阴影使其浮起。
        let r = Rect::new(-0.85, -ui.top + 0.1, 0.6, 2.);
        let radius = 0.02;
        rounded_rect_shadow(
            ui,
            r,
            &ShadowConfig {
                radius,
                elevation: 0.01,
                ..Default::default()
            },
        );
        ui.fill_path(&r.rounded(radius), ui.background());

        if let Some(user) = &self.user {
            ui.scope(|ui| {
                // 外层平移把滚动容器的原点挪到面板左上角（裁剪范围即面板本身），
                // 内层再由 `Scroll` 把原点还原回整屏坐标，因此下面的绘制仍用绝对坐标。
                ui.dx(r.x);
                ui.dy(r.y);
                self.pf_scroll.size((r.w, ui.top - r.y));
                self.pf_scroll.render(ui, |ui| {
                    ui.dx(-r.x);
                    ui.dy(-r.y);
                    let ow = r.w;
                    let oy = r.y;
                    let pad = 0.02;
                    let mw = r.w - pad * 2.;
                    let cx = r.center().x;
                    // 头像：`opt_avatar` 用 Result 通道区分三态——`Ok(Some)` 真头像、
                    // `Ok(None)` 该用户没有头像、`Err(占位纹理)` 尚未加载完。
                    // `ui.avatar` 直接吃这个 Result，因此无需在此写分支：
                    // 两种「没有真图」的情况都会由它用 `icon_user` 或缓存占位绘制。
                    let radius = 0.12;
                    let r = ui.avatar(cx, r.y + radius + 0.05, radius, t, UserManager::opt_avatar(self.id, &self.icon_user));
                    self.avatar_btn.set(ui, r);
                    // 昵称：颜色由 `name_color` 依用户身份（徽章/权限）决定，
                    // 因此不同玩家看到的名字颜色可能不同。
                    let r = ui
                        .text(&user.name)
                        .size(0.74)
                        .pos(cx, r.bottom() + 0.03)
                        .anchor(0.5, 0.)
                        .max_width(mw)
                        .color(user.name_color())
                        .draw();
                    self.btn_name.set(ui, r);
                    // 以下各行都以上一行的底部为锚点依次下移，形成一条居中的信息列：
                    // 用户 id（服务端主键，加 `#` 前缀）→ RKS → 个性签名 → 徽章 → 最后登录时间。
                    let r = ui
                        .text(format!("#{}", self.id))
                        .size(0.35)
                        .pos(cx, r.bottom() + 0.01)
                        .anchor(0.5, 0.)
                        .color(semi_white(0.5))
                        .draw();
                    let r = ui
                        .text(format!("RKS {:.2}", user.rks))
                        .size(0.5)
                        .pos(cx, r.bottom() + 0.01)
                        .anchor(0.5, 0.)
                        .draw();
                    // 个性签名：允许为空（此时画空串，仅占一行高度），多行自动换行并限宽。
                    // 这里用 `mut r` 是为了下面徽章行可能覆盖它。
                    let mut r = ui
                        .text(user.bio.as_deref().unwrap_or(""))
                        .pos(cx, r.bottom() + 0.01)
                        .anchor(0.5, 0.)
                        .multiline()
                        .max_width(mw)
                        .size(0.4)
                        .draw();
                    // 徽章：把已本地化的展示名用空格拼成一行。无徽章时整行跳过，
                    // `r` 仍是签名那一行的矩形，后续排版不会多留空隙。
                    if !self.user_badges.is_empty() {
                        r = ui
                            .text(self.user_badges.join(" "))
                            .pos(cx, r.bottom() + 0.01)
                            .anchor(0.5, 0.)
                            .size(0.5)
                            .draw();
                    }
                    // 最后登录时间：服务端存的是 UTC，展示前转成设备本地时区再格式化。
                    let r = ui
                        .text(tl!("last-login", "time" => user.last_login.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()))
                        .pos(cx, r.bottom() + 0.01)
                        .anchor(0.5, 0.)
                        .size(0.4)
                        .color(semi_white(0.6))
                        .draw();
                    // 阶段三：操作按钮组。第一颗「在网页中打开」对所有访客可见，
                    // 之后的按钮仅本人可见，逐行下排（`r.y += r.h + 0.02` 是手写排版）。
                    let hw = 0.2;
                    let mut r = Rect::new(r.center().x - hw, r.bottom() + 0.02, hw * 2., 0.1);
                    self.btn_open_web.render_text(ui, r, t, ttl!("open-in-web"), 0.6, true);
                    r.y += r.h + 0.02;
                    if get_data().me.as_ref().is_some_and(|it| it.id == self.id) {
                        self.btn_logout.render_text(ui, r, t, tl!("logout"), 0.6, true);
                        r.y += r.h + 0.02;
                        // 删除是不可逆操作，文字用红色以示危险。
                        self.btn_delete.render_text_color(ui, r, t, tl!("delete"), 0.6, true, RED);
                        #[cfg(feature = "hykb")]
                        {
                            let me = get_data().me.as_ref();
                            let bound = me.and_then(|it| it.hykb_uid).is_some();
                            let has_email = me.is_some_and(|it| it.email.is_some());
                            // Bind/unbind only makes sense for accounts that keep
                            // an email login; a pure-HYKB account must migrate.
                            if has_email {
                                r.y += r.h + 0.02;
                                let label = if bound { tl!("hykb-unbind") } else { tl!("hykb-bind") };
                                self.btn_hykb.render_text(ui, r, t, label, 0.6, true);
                            }
                            // Pure-HYKB players (bound, no email) can migrate
                            // their binding onto an existing email account.
                            if bound && !has_email {
                                r.y += r.h + 0.02;
                                self.btn_transfer.render_text(ui, r, t, tl!("hykb-transfer"), 0.6, true);
                            }
                        }
                    }
                    // 返回滚动内容尺寸：宽取面板宽，高取内容底部相对面板顶部的距离加余量，
                    // `Scroll` 依此决定能否继续下拉。
                    (ow, r.bottom() - oy + 0.04)
                });
            });
        } else {
            // 用户信息还没到：在面板的可见区域内画一个加载圈。
            // `r.bottom().min(ui.top)` 是为了让加载圈居中于「面板与屏幕的重叠部分」，
            // 因为面板高度（2）远超屏幕，直接用 `r.center()` 会把它画到屏幕外。
            ui.loading(r.center().x, (r.y + r.bottom().min(ui.top)) / 2., t, WHITE, ());
        }

        // 阶段四：右栏成绩网格。区域从面板右侧留 0.05 开始，
        // 高度写 1.5 只是给 `Scroll` 一个参考，实际可滚动区按 `ui.top * 2.` 给足。
        let r = Rect::new(r.right() + 0.05, r.y, 0.9 - r.right(), 1.5);
        if let Some(items) = &mut self.record_items {
            // `Fader` 的层计数在每帧渲染时累加，因此必须在每帧渲染前重置，
            // 否则错峰延迟会越积越大。
            self.fader.reset();
            self.fader.for_sub(|f| {
                ui.scope(|ui| {
                    ui.dx(r.x);
                    ui.dy(-ui.top);
                    // `o` 是当前滚动偏移，用于下面的可见性剔除。
                    let o = self.scroll.y_scroller.offset;
                    self.scroll.size((r.w, ui.top * 2.));
                    self.scroll.render(ui, |ui| {
                        // 两列网格：`i` 是行、`j` 是列。每格宽为区域一半，高固定 0.2。
                        // 行数用 `div_ceil(2)` 向上取整，末行不足两列时内层取 `(n - i*2).min(2)`
                        // 限制列数，避免越界；因此「格子总数恰好等于 n」，
                        // 下面的 `unreachable!()` 是对该不变量的断言。
                        let n = items.len();
                        let h = 0.2;
                        let pad = 0.02;
                        let mut iter = items.iter_mut();
                        for i in 0..n.div_ceil(2) {
                            for j in 0..(n - i * 2).min(2) {
                                let Some(item) = iter.next() else { unreachable!() };
                                f.render(ui, t, |ui| {
                                    let r = Rect::new(j as f32 * r.w / 2. + pad, r.y + ui.top + i as f32 * h, r.w / 2. - pad * 2., h - pad * 2.);
                                    // 可见性剔除：完全在滚动视口之外的卡片直接跳过绘制，
                                    // **同时也跳过了 `notify()`**，因此曲绘只会在真正被看到时才加载
                                    // ——这是长成绩列表不一次性下载全部封面的关键。
                                    if r.y - o > ui.top * 2. || r.bottom() - o < 0. {
                                        return;
                                    }
                                    // 放行该行的曲绘加载（幂等，未就绪时才真正开始）。
                                    item.illu.notify();
                                    item.btn.render_shadow(ui, r, t, |ui, path| {
                                        // `.0` 是列表用的缩略图（`.1` 为原图，此处用不到）。
                                        ui.fill_path(&path, (*item.illu.texture.0, r));
                                        ui.fill_path(&path, semi_black(0.6));
                                    });

                                    // 左侧判定图标：由分数与 FC 共同决定档位，
                                    // 因此同分但断连的记录图标不同。
                                    let icon = icon_index(item.record.score as _, item.record.full_combo);
                                    let s = r.h - pad * 2.;
                                    let ir = Rect::new(r.x + pad, r.y + pad, s, s);
                                    ui.fill_rect(ir, (*self.rank_icons[icon], ir, ScaleType::Fit));

                                    let lf = ir.right() + 0.02;

                                    // 曲名是异步取的，尚未就绪时跳过标题（只显示分数），
                                    // 而不是显示空白占位或等待。
                                    if let Some(Ok(name)) = item.name.get().as_ref() {
                                        ui.text(name).pos(lf, ir.y).max_width(r.right() - lf - 0.03).size(0.56).draw();
                                    }

                                    // 下行是分数（补零到 7 位，便于纵向对齐）与 FC 标记。
                                    ui.text(format!("{:07} {}", item.record.score, if item.record.full_combo { "[FC]" } else { "" }))
                                        .pos(lf, ir.bottom() - 0.02)
                                        .anchor(0., 1.)
                                        .size(0.6)
                                        .color(semi_white(0.6))
                                        .draw();
                                });
                            }
                        }
                        // 内容总尺寸：宽为区域宽，高为「区域顶部 + 顶部偏移补偿 + 全部行高 + 余量」，
                        // 供 `Scroll` 计算可滚动范围。
                        (r.w, r.y + ui.top + h * n.div_ceil(2) as f32 + 0.04)
                    })
                });
            });
        } else {
            // 成绩列表还没到：右栏居中画加载圈。
            let ct = r.center();
            ui.loading(ct.x, ct.y, t, WHITE, ());
        }

        // 阶段五：场景过渡遮罩（进场淡入 / 离开淡出）。
        self.sf.render(ui, t);

        // 阶段六：操作进行中的全屏提示。三者互斥地覆盖在最上层，
        // 同时也是「当前有写操作在跑」的视觉反馈（`touch` 里也据此屏蔽了头像上传期间的输入）。
        if self.avatar_task.is_some() {
            ui.full_loading(tl!("uploading-avatar"), t);
        }
        #[cfg(feature = "hykb")]
        if self.hykb_task.is_some() {
            ui.full_loading_simple(t);
        }
        #[cfg(feature = "hykb")]
        if self.transfer_task.is_some() {
            ui.full_loading(tl!("transfer-requesting"), t);
        }
        Ok(())
    }

    /// 场景切换完全由 `SFader` 决定：`enter` 中启动淡入时它返回空，
    /// 返回上一页时（`touch` 里调 `sf.next`）它在淡出结束后交出 `Pop`。
    /// 本页不接收其它场景的结果，因此没有 `on_result`。
    fn next_scene(&mut self, tm: &mut TimeManager) -> NextScene {
        self.sf.next_scene(tm.now() as f32).unwrap_or_default()
    }
}
