//! 多人联机面板：`phira-mp` 客户端的 UI 宿主与房间状态机。
//!
//! 面板由 `MainScene` 全局持有一个（`MP_PANEL` 线程局部），通过
//! `phira_mp_client::Client` 连接 `phira-mp` 服务器：地址取自 `Config::mp_address`
//! （默认 `mp2.phira.cn:12345`），协议为纯 TCP + 自研二进制，消息类型定义在
//! `phira_mp_common`（`ClientCommand`/`ServerCommand`/`Message`/`RoomState`）。
//!
//! 职责边界：面板只处理「房间级」交互（连接鉴权、建房/进房、选曲、准备、聊天、开局）；
//! 一旦进入对局，`Client` 会被交给 `SongScene`，由后者在结束时调用 `played`/`abort` 上报。
use crate::{
    client::{Chart, Ptr, UserManager},
    dir, get_data,
    mp::L10N_LOCAL,
    scene::{Downloading, SongScene, RECORD_ID},
};
use anyhow::{anyhow, Context, Result};
use inputbox::InputBox;
use macroquad::prelude::*;
use phira_mp_client::Client;
use phira_mp_common::{RoomId, RoomState};
use prpr::{
    config::Mods,
    core::{Smooth, Tweenable},
    ext::{poll_future, semi_black, semi_white, LocalTask, RectExt, SafeTexture},
    info::ChartInfo,
    scene::{request_input, return_input, show_error, show_message, take_input, GameMode, NextScene},
    task::Task,
    time::TimeManager,
    ui::{DRectButton, DrawText},
    ui::{Scroll, Ui},
};
use smallvec::SmallVec;
use std::{
    fs::File,
    path::Path,
    sync::{atomic::Ordering, Arc},
};
use tracing::warn;

/// 面板滑入/滑出的转场时长（秒），同时用作整体遮罩动画时长与「动画期间吞输入」的判定依据。
const ENTER_TRANSIT: f32 = 0.5;
/// 用户列表覆盖层的淡入淡出时长（秒）。
const USER_LIST_TRANSIT: f32 = 0.4;
/// 面板宽度（逻辑坐标下屏幕可视高度为 2.0，因此 1.6 已占去绝大部分屏宽）。
const WIDTH: f32 = 1.6;

/// 聊天功能开关，由 `chat` feature 决定。关闭时聊天输入/发送按钮不渲染，
/// 也不为它们预留高度（见 `render_main` 中对 `CHAT_ENABLED` 的两处使用）。
const CHAT_ENABLED: bool = cfg!(feature = "chat");

/// 当前窗口的物理尺寸，用于检测窗口缩放后是否需要重排消息布局。
fn screen_size() -> (u32, u32) {
    (screen_width() as u32, screen_height() as u32)
}

/// 消息列表中的一条消息及其排版缓存（聊天与系统提示共用）。
struct Message {
    /// 消息文本：聊天为「昵称：内容」，系统消息为已本地化的模板文本。
    content: String,
    /// 本条消息顶部的 y 偏移（相对消息区顶部），由 `render_main` 逐条累加计算并缓存。
    y: f32,
    /// 本条消息底部的 y 偏移，用于计算滚动总高度与可见性裁剪。
    bottom: f32,
    /// 文本颜色：聊天为纯白，系统消息为半透明白（弱化显示）。
    color: Color,
}

// 消息文本的绘制辅助：统一字号、颜色与自动换行（`mw` 为可用最大宽度）。
impl Message {
    /// 构造用于绘制的文本对象；调用方按需继续链式设置位置/锚点，
    /// 因此这里只负责字号、颜色、换行与最大宽度这些共同属性。
    pub fn text<'a, 's, 'ui>(&'s self, ui: &'ui mut Ui<'a>, mw: f32) -> DrawText<'a, 's, 'ui> {
        ui.text(&self.content)
            .pos(0., self.y)
            .size(0.4)
            .color(self.color)
            .max_width(mw)
            .multiline()
    }
}

/// 多人联机面板：连接管理 + 房间操作 + 状态同步 + 进入对局的完整状态机。
///
/// 生命周期与普通 UI 不同：`new` 之后实例常驻于 `MainScene` 的 `MP_PANEL`
/// 线程局部，`show`/关闭只切换 `side_enter_time`（面板可见性），
/// 因此关闭面板不会断开连接、也不会丢失消息历史。
///
/// 所有网络操作都遵循同一约定：`touch` 中只做前置校验并发起 `Task`，
/// 结果统一在 `update` 里消费，避免在网络回调中直接改 UI 状态。
pub struct MPPanel {
    /// 联机客户端；`None` 表示「未连接/已断开」，此时面板只显示连接按钮。
    /// 为 `Some` 时内部已持有鉴权后的连接与后台收发线程。
    pub client: Option<Arc<Client>>,

    /// 面板入场/出场动画计时：正值为正在滑入，负值为正在滑出，
    /// `INFINITY` 表示面板完全关闭（既不渲染也不接收输入）。
    side_enter_time: f32,

    /// 消息区滚动容器与消息缓存。
    msg_scroll: Scroll,
    msgs: Vec<Message>,
    /// 从该下标起的消息需要重算 `y`/`bottom`：追加新消息时取 `msgs.len()`，
    /// 窗口尺寸变化时置 0 触发整体重排（避免每帧全量重排长列表）。
    msgs_dirty_from: usize,
    /// 上一帧的窗口尺寸，用于检测缩放。
    last_screen_size: (u32, u32),

    /// 连接按钮（未连接时显示）。
    connect_btn: DRectButton,
    /// 连接 + 鉴权任务；完成前面板处于 loading 态。
    connect_task: Option<Task<Result<Client>>>,

    /// 建房/进房/退房按钮与任务。
    create_room_btn: DRectButton,
    create_room_task: Option<Task<Result<()>>>,
    join_room_btn: DRectButton,
    /// 进房任务：成功后返回进房那一刻的房间状态，用于同步本地选曲缓存。
    join_room_task: Option<Task<Result<RoomState>>>,
    leave_room_btn: DRectButton,

    /// 主动断开连接（丢弃 `client`，回到未连接态）。
    disconnect_btn: DRectButton,

    /// 房主专属按钮：开始游戏 / 锁定房间 / 循环房间。
    request_start_btn: DRectButton,
    lock_room_btn: DRectButton,
    cycle_room_btn: DRectButton,

    /// 准备与取消准备按钮：按当前准备状态二选一显示，因此不会同时可点。
    ready_btn: DRectButton,
    cancel_ready_btn: DRectButton,

    /// 聊天输入内容、输入框按钮、发送按钮与发送任务（仅 `chat` feature 使用）。
    chat_text: String,
    chat_btn: DRectButton,
    chat_send_btn: DRectButton,
    chat_task: Option<Task<Result<()>>>,

    /// 开局前的谱面元信息拉取任务，以及随后的下载进度覆盖层。
    download_task: Option<Task<Result<Arc<Chart>>>>,
    downloading: Option<Downloading>,
    // true for request_start, false for ready
    /// 下载结束后的动作：`true` 走房主的 `request_start`，`false` 走普通玩家的 `ready`。
    download_next: bool,

    /// 当前房间选中的谱面 id 缓存（来自 `SelectChart` 状态或进房时的状态快照）。
    /// 开局与下载都依赖它，因此必须紧跟 `ChangedState` 同步。
    chart_id: Option<i32>,
    /// 本次 `Playing` 状态是否已被消费，避免该状态持续存在时重复进入对局；
    /// 一旦离开 `Playing` 就复位，使下一局能再次触发。
    game_start_consumed: bool,
    /// 本局结束后是否需要上报结果（`played` 或 `abort`）。
    need_upload: bool,
    /// 对局场景是否真正进入过。结算上报必须等到从 `SongScene` 返回本场景之后才做，
    /// 该标志即用于这个时机判断。
    entered: bool,

    /// 面板请求的引擎级场景切换（进入对局），由 `next_scene` 一次性取走。
    next_scene: Option<NextScene>,

    /// 通用一次性任务：选曲、锁房、循环、准备、取消准备、退房、聊天外的上报等，
    /// 错误统一弹提示（不复用别的任务槽，避免互相覆盖结果）。
    task: Option<Task<Result<()>>>,

    /// 进入对局的场景构造任务（`SongScene::global_launch` 的结果），
    /// 就绪后写入 `next_scene`。
    scene_task: LocalTask<Result<NextScene>>,

    /// 用户列表覆盖层：展开按钮、展开进度、滚动容器与缺省头像。
    user_list_btn: DRectButton,
    user_list_p: Smooth<f32>,
    user_list_scroll: Scroll,
    icon_user: SafeTexture,
}

// `MPPanel` 的构造与「网络操作发起」部分：这里的方法只做前置状态校验并起 `Task`，
// 真正的结果处理都在 `update` 中，以保持状态变更只发生在主线程的固定时机。
impl MPPanel {
    /// 创建面板。
    ///
    /// `icon_user` 是用户列表中的缺省头像（真实头像由 `UserManager` 异步加载后替换）。
    /// 实例创建后会被 `MainScene` 放进 `MP_PANEL` 线程局部常驻，不随面板开关销毁，
    /// 因此初始状态必须是「完全关闭 + 未连接」。
    pub fn new(icon_user: SafeTexture) -> Self {
        Self {
            client: None,

            // INFINITY 即「面板完全关闭」：既不渲染也不接收输入。
            side_enter_time: f32::INFINITY,

            msg_scroll: Scroll::new(),
            msgs: Vec::new(),
            msgs_dirty_from: 0,
            last_screen_size: screen_size(),

            connect_btn: DRectButton::new(),
            connect_task: None,

            create_room_btn: DRectButton::new(),
            create_room_task: None,
            join_room_btn: DRectButton::new(),
            join_room_task: None,
            leave_room_btn: DRectButton::new(),

            disconnect_btn: DRectButton::new(),

            request_start_btn: DRectButton::new(),
            lock_room_btn: DRectButton::new(),
            cycle_room_btn: DRectButton::new(),

            ready_btn: DRectButton::new(),
            cancel_ready_btn: DRectButton::new(),

            chat_text: String::new(),
            chat_btn: DRectButton::new().with_delta(-0.002),
            chat_send_btn: DRectButton::new(),
            chat_task: None,

            download_task: None,
            downloading: None,
            download_next: false,

            // 对局相关状态以「未开局」起步：未选曲、未消费开局事件、无需上报。
            chart_id: None,
            game_start_consumed: false,
            need_upload: false,
            entered: false,

            next_scene: None,

            task: None,

            scene_task: None,

            user_list_btn: DRectButton::new(),
            user_list_p: Smooth::default(),
            user_list_scroll: Scroll::new(),
            icon_user,
        }
    }

    /// 取得客户端的引用计数值。
    ///
    /// # Panics
    /// 调用前必须已连接（`client` 为 `Some`）；仅在确认连接后的分支里调用。
    /// 之所以取 `Arc` 而非借用，是为了把连接安全地移动进异步任务。
    fn clone_client(&self) -> Arc<Client> {
        Arc::clone(self.client.as_ref().unwrap())
    }

    /// 是否有关键异步任务在进行中（连接/建房/聊天/下载/通用操作/场景加载）。
    ///
    /// 为真时面板显示全屏 loading 并吞掉输入：这些操作大多会改变协议状态，
    /// 并发发起容易让本地缓存与服务端状态不一致。
    fn has_task(&self) -> bool {
        self.connect_task.is_some()
            || self.create_room_task.is_some()
            || self.chat_task.is_some()
            || self.download_task.is_some()
            || self.task.is_some()
            || self.scene_task.is_some()
    }

    /// 发起连接：先取登录 token，再连接 `Config::mp_address` 并完成鉴权。
    ///
    /// 未登录时直接提示并返回（联机要求账号身份）。成功/失败的结果都由 `update`
    /// 统一处理；本函数同时也是掉线自动重连的入口（见 `touch` 中的 ping 失败检测）。
    fn connect(&mut self) {
        let Some(token) = get_data().tokens.as_ref().map(|it| it.0.clone()) else {
            show_message(mtl!("connect-must-login")).error();
            return;
        };
        let addr = get_data().config.mp_address.clone();
        self.connect_task = Some(Task::new(async move {
            let client = Client::from_address(&addr).await?;
            client
                .authenticate(token)
                .await
                .with_context(|| anyhow!(mtl!("connect-authenticate-failed")))?;
            Ok(client)
        }));
    }

    /// 创建房间；`id` 由用户在输入框中填写，重名/非法由服务端返回错误。
    fn create_room(&mut self, id: RoomId) {
        let client = self.clone_client();
        self.create_room_task = Some(Task::new(async move {
            client.create_room(id).await?;
            Ok(())
        }));
    }

    /// 房主在 `SelectChart` 状态下选择谱面；由外部页面（曲库）在用户确认选曲后调用。
    ///
    /// 非房主或房间不处于选曲状态时直接提示并放弃请求。服务端也会做同样的校验，
    /// 客户端提前拦截只是为了让用户立刻得到反馈。
    pub fn select_chart(&mut self, id: i32) {
        let client = self.clone_client();
        if !client.blocking_is_host().unwrap() {
            show_message(mtl!("select-chart-host-only")).error();
            return;
        }
        if !matches!(client.blocking_room_state(), Some(RoomState::SelectChart(_))) {
            show_message(mtl!("select-chart-not-now")).error();
            return;
        }
        self.task = Some(Task::new(async move {
            client.select_chart(id).await.with_context(|| mtl!("select-chart-failed"))?;
            Ok(())
        }));
    }

    /// 房主开始游戏：先确认已有选曲（`SelectChart(None)` 表示房主尚未选曲），
    /// 再进入谱面检查流程（必要时先下载，下载完成后再真正发起 `request_start`）。
    fn request_start(&mut self) {
        if matches!(self.client.as_ref().unwrap().blocking_room_state().unwrap(), RoomState::SelectChart(None)) {
            show_message(mtl!("request-start-no-chart")).error();
            return;
        }
        self.check_download(true);
    }

    /// 开局前的谱面检查：拉取谱面元信息，用来判断本地是否已有该谱面、
    /// 以及是否需要按更新时间重新下载。`next` 记录下载完成后要执行的动作。
    fn check_download(&mut self, next: bool) {
        let id = self.chart_id.unwrap();
        self.download_next = next;
        self.download_task = Some(Task::new(async move { Ptr::new(id).fetch().await }));
    }

    /// 谱面检查/下载完成后的收尾：按 `download_next` 决定发送 `request_start`（房主）
    /// 还是 `ready`（普通玩家）。这是「准备/开始」真正到达服务端的唯一出口。
    fn post_download(&mut self) {
        let client = self.clone_client();
        if self.download_next {
            self.task = Some(Task::new(async move {
                client.request_start().await.with_context(|| mtl!("request-start-failed"))?;
                Ok(())
            }));
        } else {
            self.task = Some(Task::new(async move {
                client.ready().await.with_context(|| mtl!("ready-failed"))?;
                Ok(())
            }));
        }
    }
}

// `MPPanel` 的对外接口与每帧驱动：`in_room`/`select_chart` 供曲库等页面调用，
// `show`/`touch`/`update`/`render`/`next_scene` 由 `MainScene` 在相应时机调用
// （`update`/`render` 每帧，`show` 由联机悬浮按钮的点击触发）。
impl MPPanel {
    /// 当前是否已在某个房间内（服务端已分配 `RoomId`）。
    /// 曲库页面据此决定「选曲」操作是发给房间还是走单机流程。
    #[inline]
    pub fn in_room(&self) -> bool {
        self.client.as_ref().is_some_and(|it| it.blocking_room_id().is_some())
    }

    /// 打开面板：记录当前实时时间作为滑入动画的起点。
    ///
    /// 用 `real_time` 而非游戏内时间，是因为面板动画不应受暂停/变速影响；
    /// 重复调用只会重播入场动画，不会重置面板内容。
    #[inline]
    pub fn show(&mut self, rt: f32) {
        self.side_enter_time = rt;
    }

    /// 标记「已随根场景进入过」。
    ///
    /// 与 `need_upload` 配合决定结算上报时机：只有真正进过对局场景
    /// （即从 `SongScene` 返回后本方法被再次调用）才会发送 `played`/`abort`。
    pub fn enter(&mut self) {
        self.entered = true;
    }

    /// 面板输入分发，按以下优先级处理：
    /// 用户列表覆盖层 → 入场/出场动画 → 关键任务阻塞 → 下载覆盖层 → 边缘滑出 →
    /// 连接/房间/聊天按钮 → 掉线重连检测。
    ///
    /// # Returns
    /// 面板关闭时返回 `false`（`MainScene` 继续把触摸分发给页面）；
    /// 只要面板可见，就一律返回 `true` 独占输入，避免点击穿透到下层页面。
    pub fn touch(&mut self, tm: &mut TimeManager, touch: &Touch) -> bool {
        let t = tm.now() as f32;
        // 面板完全关闭：交还给根场景继续分发（这是面板唯一的「不消费输入」出口）。
        if self.side_enter_time.is_infinite() {
            return false;
        }
        // 用户列表覆盖层：转场动画期间吞输入，展开后交给列表滚动，松手即关闭。
        if self.user_list_p.transiting(t) {
            return true;
        }
        if *self.user_list_p.to() > 0.5 {
            if self.user_list_scroll.touch(touch, t) {
                return true;
            }
            if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.user_list_p.goto(0., t, USER_LIST_TRANSIT);
            }
            return true;
        }
        // 入场/出场动画尚未结束：吞掉输入，避免动画期间按钮位置与手指不一致造成误触。
        if !(self.side_enter_time > 0. && tm.real_time() as f32 > self.side_enter_time + ENTER_TRANSIT) {
            return true;
        }
        // 有关键任务进行中（连接/建房/聊天/下载/场景加载）：吞掉输入，防止并发操作。
        if self.has_task() {
            return true;
        }
        // 下载覆盖层优先消费触摸：点击取消即丢弃覆盖层（由其自身实现中止下载）。
        if let Some(dl) = &mut self.downloading {
            if dl.touch(touch, t) {
                self.downloading = None;
                return true;
            }
        }
        // 点击面板左侧（面板之外的区域）滑出面板；负的入场时间表示正在滑出。
        if touch.position.x + 1. > WIDTH {
            self.side_enter_time = -tm.real_time() as f32;
            return true;
        }
        // 未连接时只有连接按钮可用。
        if self.client.is_none() && self.connect_btn.touch(touch, t) {
            self.connect();
            return true;
        }
        if let Some(client) = &self.client {
            // 消息区滚动优先于下方按钮：否则在消息列表上滑动会被按钮抢走。
            if self.msg_scroll.touch(touch, t) {
                return true;
            }
            if let Some(state) = client.blocking_state() {
                // 聊天：点击输入框弹出系统输入法（结果由 `take_input` 在 `update` 中回收）。
                if self.chat_btn.touch(touch, t) {
                    request_input("chat", InputBox::new().default_text(&self.chat_text));
                    return true;
                }
                // 发送：空文本直接提示；发送成功后才由 `update` 清空输入框，失败保留原文。
                if self.chat_send_btn.touch(touch, t) {
                    if self.chat_text.is_empty() {
                        show_message(mtl!("chat-empty")).error();
                    } else {
                        let client = Arc::clone(client);
                        let text = self.chat_text.clone();
                        self.chat_task = Some(Task::new(async move { client.chat(text).await }));
                    }
                    return true;
                }
                // 房间操作按钮集合取决于「房间状态」与「是否房主」两个维度。
                let is_host = state.is_host;
                match state.state {
                    RoomState::SelectChart(_) => {
                        // 选曲阶段：房主可开始/锁定/循环，所有成员都可退房。
                        if is_host {
                            if self.request_start_btn.touch(touch, t) {
                                self.request_start();
                                return true;
                            }
                            // 锁定/循环发送的是「取反后的目标值」，本地缓存等服务端回包再更新。
                            if self.lock_room_btn.touch(touch, t) {
                                let to = !state.locked;
                                let client = self.clone_client();
                                self.task = Some(Task::new(async move { client.lock_room(to).await.with_context(|| mtl!("lock-room-failed")) }));
                                return true;
                            }
                            if self.cycle_room_btn.touch(touch, t) {
                                let to = !state.cycle;
                                let client = self.clone_client();
                                self.task = Some(Task::new(async move { client.cycle_room(to).await.with_context(|| mtl!("cycle-room-failed")) }));
                                return true;
                            }
                        }
                        if self.leave_room_btn.touch(touch, t) {
                            let client = self.clone_client();
                            self.task = Some(Task::new(async move { client.leave_room().await }));
                            return true;
                        }
                    }
                    RoomState::WaitingForReady => {
                        // 已准备则显示取消准备，否则点击准备要先确认本地已有该谱面（必要时先下载再 ready）。
                        if client.blocking_is_ready().unwrap() {
                            if self.cancel_ready_btn.touch(touch, t) {
                                let client = self.clone_client();
                                self.task = Some(Task::new(async move { client.cancel_ready().await }));
                                return true;
                            }
                        } else if self.ready_btn.touch(touch, t) {
                            self.check_download(false);
                            return true;
                        }
                    }
                    // 其他房间状态（如 Playing）在面板上不提供操作按钮。
                    _ => {}
                }
                // 用户列表：重置滚动位置、播放展开动画，并批量预取昵称/头像。
                if self.user_list_btn.touch(touch, t) {
                    self.user_list_scroll.y_scroller.reset();
                    self.user_list_p.goto(1., t, USER_LIST_TRANSIT);
                    client.blocking_state().unwrap().users.keys().copied().for_each(UserManager::request);
                }
            } else {
                // 已连接但不在任何房间：只能建房/进房/断开连接。
                if self.create_room_btn.touch(touch, t) {
                    request_input("room_id", InputBox::new());
                    return true;
                }
                if self.join_room_btn.touch(touch, t) {
                    request_input("join_room", InputBox::new());
                    return true;
                }
                // 断开连接：清空消息历史，避免重连后与旧会话混在一起。
                if self.disconnect_btn.touch(touch, t) {
                    self.client = None;
                    self.msgs.clear();
                    self.msgs_dirty_from = 0;
                    return true;
                }
            }
            // 掉线检测：心跳累计失败 >= 2 次判定为连接已死，自动重连
            // （已有连接任务时不重复触发，避免重连风暴）。
            if client.ping_fail_count() >= 2 && self.connect_task.is_none() {
                warn!("lost connection, reconnecting…");
                show_message(mtl!("reconnect")).warn();
                self.connect();
            }
        }
        // 面板可见即独占输入。
        true
    }

    /// 每帧驱动面板：动画计时 → 消息拉取与文本化 → 房间状态同步 → 进入对局
    /// → 各类异步任务的完成处理 → 结算上报。
    ///
    /// 所有网络结果都在这里消费：面板其余部分（`touch`/`render`）只读状态不等待网络，
    /// 从而保证同一帧内的 UI 状态是自洽的。
    pub fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        let t = tm.now() as f32;
        // 滑出动画播完：把面板置为「完全关闭」（连接与消息历史都保留）。
        if self.side_enter_time < 0. && -tm.real_time() as f32 + ENTER_TRANSIT < self.side_enter_time {
            self.side_enter_time = f32::INFINITY;
        }
        // 窗口尺寸变化会让消息换行失效，需要整体重排（把排版游标拨回 0）。
        let new_size = screen_size();
        if self.last_screen_size != new_size {
            self.last_screen_size = new_size;
            self.msgs_dirty_from = 0;
        }
        self.msg_scroll.update(t);
        // 用户列表不可见时不更新其滚动，省去无谓计算。
        if self.user_list_p.now(t) > 1e-4 {
            self.user_list_scroll.update(t);
        }
        if let Some(client) = &self.client {
            // 拉取服务端消息并逐条转换为本地化文本。
            // `blocking_take_messages` 具有「取走即清空」语义，因此不会重复追加。
            self.msgs.extend(client.blocking_take_messages().into_iter().map(|msg| {
                use phira_mp_common::Message as M;
                match msg {
                    // 聊天消息单独处理：带发送者昵称，并用白色高亮与系统消息区分。
                    M::Chat { user, content, .. } => Message {
                        content: format!("{}：{content}", client.user_name(user)),
                        y: 0.,
                        bottom: 0.,
                        color: WHITE,
                    },
                    msg => {
                        // 系统消息：每类协议消息对应一条本地化模板文案；
                        // 需要玩家名的消息一律经 `client.user_name` 解析（内部带缓存）。
                        let content = match msg {
                            M::Chat { .. } => unreachable!(),
                            M::CreateRoom { user } => {
                                mtl!("msg-create-room", "user" => client.user_name(user))
                            }
                            M::JoinRoom { name, .. } => {
                                mtl!("msg-join-room", "user" => name)
                            }
                            M::LeaveRoom { name, .. } => {
                                mtl!("msg-leave-room", "user" => name)
                            }
                            M::NewHost { user } => {
                                mtl!("msg-new-host", "user" => client.user_name(user))
                            }
                            M::SelectChart { user, name, id } => {
                                mtl!("msg-select-chart", "user" => client.user_name(user), "chart" => name, "id" => id)
                            }
                            M::GameStart { user } => {
                                mtl!("msg-game-start", "user" => client.user_name(user))
                            }
                            M::Ready { user } => {
                                mtl!("msg-ready", "user" => client.user_name(user))
                            }
                            M::CancelReady { user } => {
                                mtl!("msg-cancel-ready", "user" => client.user_name(user))
                            }
                            M::CancelGame { user } => {
                                mtl!("msg-cancel-game", "user" => client.user_name(user))
                            }
                            M::StartPlaying => mtl!("msg-start-playing").into_owned(),
                            M::Played { user, score, accuracy, full_combo } => {
                                mtl!("msg-played", "user" => client.user_name(user), "score" => format!("{score:07}"), "accuracy" => format!("{:.2}%", accuracy * 100.), "full-combo" => full_combo.to_string())
                            }
                            M::GameEnd => mtl!("msg-game-end").into_owned(),
                            M::Abort { user } => mtl!("msg-abort", "user" => client.user_name(user)),
                            M::LockRoom { lock } => mtl!("msg-room-lock", "lock" => lock.to_string()),
                            M::CycleRoom { cycle } => mtl!("msg-room-cycle", "cycle" => cycle.to_string()),
                        };
                        Message {
                            content,
                            y: 0.,
                            bottom: 0.,
                            color: semi_white(0.7),
                        }
                    }
                }
            }));
            // 房间状态同步：`Playing` 表示服务端已宣布开局，只消费一次。
            let state = client.blocking_room_state();
            if matches!(state, Some(RoomState::Playing)) {
                if !self.game_start_consumed {
                    self.game_start_consumed = true;
                    let id = self.chart_id.unwrap();
                    // 清空本局成绩记录 id（结算时由 SongScene 写回，-1 表示尚未产生成绩）；
                    // 标记需要上报，并把 Client 一并交给 SongScene 以便大厅内共用连接。
                    RECORD_ID.store(-1, Ordering::Relaxed);
                    self.need_upload = true;
                    self.entered = false;
                    self.scene_task = SongScene::global_launch(
                        Some(id),
                        &format!("download/{id}"),
                        Mods::default(),
                        GameMode::NoRetry,
                        self.client.as_ref().map(Arc::clone),
                        None,
                        None,
                        false,
                    )?;
                }
            } else {
                // 离开 `Playing` 后复位，使下一局能再次触发进入对局。
                self.game_start_consumed = false;
            }
            // 缓存选曲（`SelectChart(None)` 表示房主尚未选定谱面）。
            if let Some(RoomState::SelectChart(chart)) = state {
                self.chart_id = chart;
            }
        }
        // 连接完成：把新客户端放入（旧连接随之释放），失败给出可见原因。
        if let Some(task) = &mut self.connect_task {
            if let Some(res) = task.take() {
                match res {
                    Ok(client) => {
                        show_message(mtl!("connect-success")).ok();
                        self.client = Some(client.into());
                    }
                    Err(err) => {
                        show_error(err.context(mtl!("connect-failed")));
                    }
                }
                self.connect_task = None;
            }
        }
        // 建房完成（房间 id 重复等问题在此以错误形式提示）。
        if let Some(task) = &mut self.create_room_task {
            if let Some(res) = task.take() {
                match res {
                    Ok(_) => {
                        show_message(mtl!("create-room-success")).ok();
                    }
                    Err(err) => {
                        show_error(err.context(mtl!("create-room-failed")));
                    }
                }
                self.create_room_task = None;
            }
        }
        // 谱面元信息返回：与本地 info.yml 比较，判断是否需要下载/更新。
        if let Some(task) = &mut self.download_task {
            if let Some(res) = task.take() {
                match res {
                    Ok(entity) => {
                        let path = format!("download/{}", entity.id);
                        let info_path = format!("{}/{path}/info.yml", dir::charts()?);
                        // 本地没有该谱面 → 必须下载；已有则比较 `updated` 时间戳判断是否有更新，
                        // 缺失 `updated` 字段时退化为与 `created` 比较。
                        let should_download = if Path::new(&info_path).exists() {
                            let local_info: ChartInfo = serde_yaml::from_reader(File::open(info_path)?)?;
                            local_info
                                .updated
                                .map_or(entity.updated != entity.created, |local_updated| local_updated != entity.updated)
                        } else {
                            true
                        };
                        if should_download {
                            // 启动下载：若本地已有同 id 目录则复用（更新场景），否则新建。
                            let info = entity.to_info();
                            self.downloading = Some(SongScene::global_start_download(info, Chart::clone(&entity), {
                                if Path::new(&format!("{}/{path}", dir::charts()?)).exists() {
                                    Some(path)
                                } else {
                                    None
                                }
                            })?);
                        } else {
                            // 本地已是最新，直接进入准备/开始流程。
                            self.post_download();
                        }
                    }
                    Err(err) => {
                        show_error(err.context(mtl!("download-failed")));
                    }
                }
                self.download_task = None;
            }
        }
        // 下载进度检查：结束（成功或失败）后一律丢弃覆盖层；成功时才继续准备/开始，
        // 失败时保持房间状态不变，让用户可以重试。
        if let Some(dl) = &mut self.downloading {
            if let Some(res) = dl.check()? {
                if res.is_some() {
                    self.post_download();
                }
                self.downloading = None;
            }
        }
        // 聊天发送完成：成功才清空输入框，失败保留内容方便用户重试。
        if let Some(task) = &mut self.chat_task {
            if let Some(res) = task.take() {
                match res {
                    Ok(_) => {
                        show_message(mtl!("chat-sent")).ok();
                        self.chat_text.clear();
                    }
                    Err(err) => {
                        show_error(err.context(mtl!("chat-send-failed")));
                    }
                }
                self.chat_task = None;
            }
        }
        // 通用任务（选曲/锁房/循环/准备/取消准备/退房/上报结果）：只关心错误，成功无需提示。
        if let Some(task) = &mut self.task {
            if let Some(res) = task.take() {
                if let Err(err) = res {
                    show_error(err);
                }
                self.task = None;
            }
        }
        // 进房完成：从状态快照里同步选曲 id（进房时服务端可能已经在选曲阶段）。
        // 注意：此处清空的是 `task` 而非 `join_room_task`，与其它任务块的写法不一致（未改动原逻辑）。
        if let Some(task) = &mut self.join_room_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(mtl!("join-room-failed")));
                    }
                    Ok(state) => {
                        self.chart_id = match state {
                            RoomState::SelectChart(id) => id,
                            _ => None,
                        };
                    }
                }
                self.task = None;
            }
        }
        // 系统输入框回调：`chat` 只回填文本，`room_id`/`join_room` 立即发起对应请求；
        // 其他 id 原样退回（可能属于同一进程里的其他输入框使用者）。
        if let Some((id, text)) = take_input() {
            match id.as_str() {
                "chat" => {
                    self.chat_text = text;
                }
                "room_id" => {
                    // id 解析失败时直接把错误抛给上层（`update` 会统一提示）。
                    self.create_room(text.try_into().with_context(|| mtl!("create-invalid-id"))?);
                }
                "join_room" => {
                    let client = self.clone_client();
                    // 解析成功才发请求；任务里额外取一次房间状态作为进房结果。
                    if let Ok(id) = text.try_into() {
                        self.join_room_task = Some(Task::new(async move {
                            client.join_room(id, false).await?;
                            client.room_state().await.ok_or_else(|| anyhow!("expected room state"))
                        }));
                    } else {
                        show_message(mtl!("join-room-invalid-id")).error();
                    }
                }
                _ => return_input(id, text),
            }
        }
        // 对局场景构造完成：写入 `next_scene`，由 `MainScene::next_scene` 交给引擎。
        if let Some(task) = &mut self.scene_task {
            if let Some(res) = poll_future(task.as_mut()) {
                match res {
                    Err(err) => {
                        show_error(err);
                    }
                    Ok(scene) => self.next_scene = Some(scene),
                }
                self.scene_task = None;
            }
        }
        // 结算上报：需同时满足「本局需要上报」与「确实进过对局场景」（`entered`）。
        // 这样从 `SongScene` 退出、根场景再次调用 `enter` 之后才发送，避免抢在对局结束前上报。
        if self.need_upload && self.entered {
            let id = RECORD_ID.load(Ordering::Relaxed);
            if id != -1 {
                // 存在有效成绩 → 上报成绩（服务端据此结算名次/得分）。
                let client = self.clone_client();
                self.task = Some(Task::new(async move { client.played(id).await }));
            } else {
                // 无成绩（中途退出/失败）→ 上报中断，让房间继续流程。
                let client = self.clone_client();
                self.task = Some(Task::new(async move { client.abort().await }));
            }
            // 一次性：上报后复位，等待下一局重新置位。
            self.need_upload = false;
        }
        Ok(())
    }

    /// 绘制面板：遮罩 + 面板体 → 主体内容（或连接按钮）→ 下载覆盖层 → 全局 loading。
    ///
    /// 面板完全关闭（`side_enter_time` 为 `INFINITY` 等非有限值）时不做任何绘制，
    /// 但仍保留连接与消息状态，下次打开即可直接继续。
    pub fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) {
        let rt = tm.real_time() as f32;
        let t = tm.now() as f32;
        if self.side_enter_time.is_finite() {
            // 遮罩与面板整体的平移/淡入淡出：p 为缓出进度，负的入场时间表示正在滑出。
            let p = ((rt - self.side_enter_time.abs()) / ENTER_TRANSIT).min(1.);
            let p = 1. - (1. - p).powi(3);
            let p = if self.side_enter_time < 0. { 1. - p } else { p };
            ui.fill_rect(ui.screen_rect(), semi_black(p * 0.6));
            let w = WIDTH;
            let rt = f32::tween(&-1., &(w - 1.), p);
            ui.scope(|ui| {
                ui.dx(rt - w);
                ui.dy(-ui.top);
                let h = ui.top * 2.;
                let r = Rect::new(0., 0., w, h).feather(-0.02);
                ui.fill_path(&r.rounded(0.02), ui.background());
                // 房间号显示在面板右上角（未进房时没有 RoomId，自然不绘制）。
                if let Some(id) = self.client.as_ref().and_then(|it| it.blocking_room_id()) {
                    ui.text(mtl!("room-id", "id" => id.to_string()))
                        .pos(r.right() - 0.02, r.y + 0.02)
                        .anchor(1., 0.)
                        .size(0.44)
                        .color(semi_white(0.4))
                        .draw();
                }
                // 标题下方的剩余区域：未连接只放一个「连接」按钮，已连接则交给 `render_main`。
                let tr = ui.text(mtl!("multiplayer")).pos(0.05, 0.05).draw();
                let r = Rect::new(r.x, tr.bottom(), r.w, r.bottom() - tr.bottom()).feather(-0.02);
                if self.client.is_none() {
                    let ct = r.center();
                    self.connect_btn
                        .render_text(ui, Rect::new(ct.x, ct.y, 0., 0.).nonuniform_feather(0.14, 0.06), t, mtl!("connect"), 0.5, true);
                } else {
                    self.render_main(tm, ui, r);
                }
            });
        }
        // 下载覆盖层与全局 loading 最后绘制，保证压住面板内容。
        if let Some(dl) = &mut self.downloading {
            dl.render(ui, t);
        }
        if self.has_task() {
            ui.full_loading_simple(t);
        }
    }

    /// 绘制面板主体（仅在已连接时调用）：消息区 → 聊天输入（可选）→ 右侧按钮列 → 用户列表覆盖层。
    ///
    /// 右侧按钮的集合由当前的 `RoomState` 与「是否房主」共同决定，这里按状态组装后统一排版，
    /// 保证按钮间距一致、且实际可点的操作与服务端允许的操作严格对应。
    fn render_main(&mut self, tm: &mut TimeManager, ui: &mut Ui, r: Rect) {
        let t = tm.now() as f32;
        let client = self.client.as_ref().unwrap();
        // 消息区占面板宽度的 80%；底部按 `chat` feature 决定是否为聊天框预留高度。
        let mr = Rect::new(r.x, r.y, r.w * 0.8, r.h - if CHAT_ENABLED { 0.11 } else { 0. });
        ui.fill_path(&mr.rounded(0.01), semi_black(0.4));
        ui.scope(|ui| {
            let mut mr = mr.feather(-0.015);
            mr.y -= 0.015;
            mr.h += 0.015;
            ui.dx(mr.x);
            ui.dy(mr.y);
            // 增量排版：从 `msgs_dirty_from` 起重新累加 y，起点取前一条消息的 bottom，
            // 因此正常聊天时每帧只需测量新到达的消息。
            let mut y = if self.msgs_dirty_from == 0 {
                0.
            } else {
                self.msgs.get(self.msgs_dirty_from - 1).map_or(0., |it| it.bottom)
            };
            let old_dirty = self.msgs_dirty_from != self.msgs.len();
            for msg in &mut self.msgs[self.msgs_dirty_from..] {
                msg.y = y + 0.02;
                msg.bottom = msg.text(ui, mr.w).measure().bottom();
                y = msg.bottom;
            }
            // 有新消息且内容已超出视口时，把滚动位置移到底部（自动跟随最新消息）。
            if old_dirty {
                let o = y - mr.h;
                if o >= 0. {
                    self.msg_scroll.y_scroller.goto = Some(o);
                }
            }
            // 排版游标推进到末尾，避免下一帧重复测量。
            self.msgs_dirty_from = self.msgs.len();
            self.msg_scroll.size((mr.w, mr.h));
            let offset = self.msg_scroll.y_scroller.offset;
            self.msg_scroll.render(ui, |ui| {
                // 可见性裁剪：跳过完全滚到视口上方的消息，遇到第一条超出视口底部的消息即可停止。
                for msg in &self.msgs {
                    if msg.bottom < offset {
                        continue;
                    }
                    if msg.y > offset + mr.h {
                        break;
                    }
                    msg.text(ui, mr.w).draw();
                }
                (mr.w, self.msgs.last().map(|it| it.bottom).unwrap_or_default() + 0.03)
            });
        });

        // 聊天区（仅 `chat` feature）：输入框 + 发送按钮，宽度与消息区右边界对齐。
        if CHAT_ENABLED {
            let lw = 0.16;
            let h = 0.09;
            let br = Rect::new(r.x, r.bottom() - h, mr.w - lw - 0.02, h);
            self.chat_btn.render_input(ui, br, t, &self.chat_text, mtl!("chat-placeholder"), 0.5);
            let br = Rect::new(mr.right() - lw, br.y, lw, br.h);
            self.chat_send_btn.render_text(ui, br, t, mtl!("chat-send"), 0.5, true);
        }

        // 右侧按钮列：先按当前状态收集按钮，再统一自上而下排版，
        // 这样按钮集合随状态变化时不必为每种状态各写一套布局。
        let mut br = Rect::new(mr.right() + 0.02, mr.y, r.right() - mr.right() - 0.02, 0.1);
        let mut btns = SmallVec::<[(&mut DRectButton, String); 5]>::new();
        if let Some(state) = client.blocking_state() {
            match state.state {
                RoomState::SelectChart(_) => {
                    // 选曲阶段：房主多出「开始游戏/锁定房间/循环房间」，退房对所有成员可用。
                    if client.blocking_is_host().unwrap() {
                        btns.push((&mut self.request_start_btn, mtl!("request-start").into_owned()));
                        btns.push((&mut self.lock_room_btn, mtl!("lock-room", "current" => state.locked.to_string())));
                        btns.push((&mut self.cycle_room_btn, mtl!("cycle-room", "current" => state.cycle.to_string())));
                    }
                    btns.push((&mut self.leave_room_btn, mtl!("leave-room").into_owned()));
                }
                RoomState::WaitingForReady => {
                    // 准备阶段：按「已准备/未准备」二选一显示，正好与 `touch` 中的判定对应。
                    if client.blocking_is_ready().unwrap() {
                        btns.push((&mut self.cancel_ready_btn, mtl!("cancel-ready").into_owned()));
                    } else {
                        btns.push((&mut self.ready_btn, mtl!("ready").into_owned()));
                    }
                }
                _ => {}
            }
            // 任何房间状态下都可以查看用户列表。
            btns.push((&mut self.user_list_btn, mtl!("user-list").into_owned()));
        } else {
            // 已连接但不在房间：建房 / 进房 / 断开连接。
            btns.push((&mut self.create_room_btn, mtl!("create-room").into_owned()));
            btns.push((&mut self.join_room_btn, mtl!("join-room").into_owned()));
            btns.push((&mut self.disconnect_btn, mtl!("disconnect").into_owned()));
        }
        for (btn, text) in btns {
            btn.render_text(ui, br, t, text, 0.5, true);
            br.y += br.h + 0.02;
        }

        // 用户列表覆盖层：按人数自适应列数（2..=4 列），逐行居中排布卡片。
        let p = self.user_list_p.now(t);
        if p > 1e-4 {
            ui.abs_scope(|ui| {
                ui.alpha(p, |ui| {
                    let users: Vec<_> = client.blocking_state().unwrap().users.values().cloned().collect();
                    let n = users.len();
                    let columns = n.clamp(2, 4);
                    let rn = n.div_ceil(columns);
                    ui.fill_rect(ui.screen_rect(), semi_black(p * 0.4));

                    let mut iter = users.into_iter();
                    let h = 0.14;
                    let w = 0.48;
                    let pad = 0.03;
                    let width = w * columns as f32 + pad * (columns - 1) as f32;
                    let viewport_height = (ui.top * 2. - 0.16).max(h);
                    ui.dx(-width / 2.);
                    ui.dy(-ui.top + 0.08);
                    self.user_list_scroll.size((width, viewport_height));
                    self.user_list_scroll.render(ui, |ui| {
                        for i in 0..rn {
                            // 末行人数可能不足一整行，需要单独计算行宽与居中偏移。
                            let cn = (n - i * columns).min(columns);
                            let row_width = w * cn as f32 + pad * (cn - 1) as f32;
                            let row_offset = (width - row_width) / 2.;
                            for j in 0..cn {
                                let r = Rect::new(row_offset + j as f32 * (w + pad), i as f32 * (h + pad), w, h);
                                let Some(user) = iter.next() else { unreachable!() };
                                // 头像未加载时 `opt_avatar` 会返回 `icon_user` 作为兜底。
                                ui.avatar(r.x + 0.055, r.center().y, 0.04, t, UserManager::opt_avatar(user.id, &self.icon_user));
                                ui.text(user.name)
                                    .pos(r.x + 0.105, r.center().y)
                                    .anchor(0., 0.5)
                                    .no_baseline()
                                    .max_width(0.36)
                                    .size(0.55)
                                    .draw();
                            }
                        }
                        // 返回内容总高度（末行不额外加间距），供滚动容器计算范围。
                        (width, (rn as f32 * (h + pad) - pad).max(0.))
                    });
                });
            });
        }
    }

    /// 取出一次性的场景切换请求（进入对局），由 `MainScene::next_scene` 每帧调用。
    ///
    /// 取出即清空，避免同一个 `NextScene` 被重复提交给引擎。
    #[inline]
    pub fn next_scene(&mut self) -> Option<NextScene> {
        self.next_scene.take()
    }
}
