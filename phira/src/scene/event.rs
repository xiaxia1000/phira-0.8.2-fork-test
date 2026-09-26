//! 活动（event）场景：把服务端下发的 UML 脚本渲染成活动页面。
//!
//! 活动页面的布局/动画/交互都被描述成 UML（由服务端按活动 id 提供，
//! 客户端通过 `GET /event/{id}/uml` 拉取），本场景只负责：
//! - 维护活动业务状态：参与状态（`/event/{id}/status`）、排行榜（`/event/{id}/list15`）；
//! - 为 `Uml` 提供每帧的时间、滚动偏移、是否已参加等上下文，并转发触摸；
//! - 把 UML 内部请求的场景切换（如打开外部链接、进入玩家资料页）冒泡给引擎。
//!
//! 打开 `event_debug` feature 时改为监听并热重载本地 `test.uml`，方便调试活动脚本。
prpr_l10n::tl_file!("event");

use super::{render_ldb, LdbDisplayItem, ProfileScene};
use crate::{
    client::{recv_raw, Client, Event, UserManager},
    icons::Icons,
    page::{EventPage, Fader, Illustration, SFader},
    uml::{parse_uml, Uml},
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use macroquad::prelude::*;
use prpr::{
    core::Tweenable,
    ext::{open_url, semi_black, semi_white, RectExt, SafeTexture, ScaleType},
    scene::{show_error, NextScene, Scene},
    task::Task,
    time::TimeManager,
    ui::{button_hit, DRectButton, Dialog, LoadingParams, RectButton, Scroll, Ui},
};
use reqwest::StatusCode;
use serde::Deserialize;
use std::{any::Any, sync::Arc, time::SystemTime};

/// 活动页调试模式开关，由 `event_debug` feature 决定。
/// 打开后用本地 `test.uml` 的热重载替代服务端拉取（`uml_task` 恒为 `None`），
/// 因此可以在不重启客户端的前提下反复调整活动脚本。
const DEBUG_MODE: bool = cfg!(feature = "event_debug");
/// 侧边排行榜面板宽度占屏宽的比例（宽 0.94 意味着几乎占满，只露出左侧一条边用于收回）。
const LDB_WIDTH: f32 = 0.94;
/// 侧边排行榜滑入/滑出的转场时长（秒），触摸判定与渲染动画共用同一常量以保证一致。
const TRANSIT_TIME: f32 = 0.4;

/// 服务端返回的单条排行榜记录（`/event/{id}/list15`）。
#[derive(Deserialize)]
struct LdbItem {
    /// 玩家 id，用于异步请求昵称与头像。
    player: i32,
    /// 名次。
    rank: i32,
    /// 得分。
    score: i32,
    /// 该条目的点击命中区域：反序列化时跳过并默认构造，每帧由 `render` 刷新。
    #[serde(skip, default)]
    btn: RectButton,
}

/// 当前用户在某活动中的参与状态（`/event/{id}/status`）。
/// 未参加时名次/得分为 `Option::None`；字段当前只读取 `joined`，其余为协议完整性保留。
#[derive(Deserialize)]
#[allow(dead_code)]
struct Status {
    /// 是否已参加该活动；决定参加按钮的显示内容与侧边排行榜是否可用。
    joined: bool,
    /// 已参加时的排名（活动未结束或无有效成绩时可能为空）。
    rank: Option<i32>,
    /// 已参加时的得分。
    score: Option<i32>,
}

/// 活动页面场景：既是服务端 UML 的渲染宿主，也负责活动业务状态（参与状态、排行榜）的加载。
///
/// 由活动列表页以 `NextScene::Overlay` 压入，退出时返回 `NextScene::Pop`；
/// 页面主体完全由 UML 决定，本场景提供的固定元素只有返回键、参加按钮与侧边排行榜。
pub struct EventScene {
    /// 活动元数据（id、名称、起止时间），由调用方从活动列表页传入。
    event: Event,
    /// 活动插图背景（与活动列表页共享的动画背景）。
    illu: Illustration,

    /// 页面主体滚动容器；UML 内容高度由 `Uml::render` 返回后交给它计算滚动范围。
    scroll: Scroll,

    /// 左上角返回按钮命中区域，每帧在 `render` 中刷新。
    btn_back: RectButton,

    /// 参与状态查询任务；为 `Some` 时表示状态未知，参加按钮显示 loading。
    status_task: Option<Task<Result<Status>>>,
    /// 参与状态缓存，决定参加按钮的文案与是否可滑出排行榜。
    status: Option<Status>,

    /// UML 脚本拉取任务；`DEBUG_MODE` 下恒为 `None`，
    /// 同时被 `render` 当作「活动内容尚未就绪」的判据来显示 loading。
    uml_task: Option<Task<Result<String>>>,
    /// 解析后的 UML 实例，负责活动页面的实际绘制与点击命中。
    uml: Uml,
    /// 调试模式下 `test.uml` 上次的修改时间，用于热重载判定。
    last_modified: SystemTime,

    /// 本场景请求的引擎级场景切换（如 `Pop`），由 `next_scene` 取出。
    next_scene: Option<NextScene>,

    /// 「参加活动」按钮（带转场动画的双态按钮）。
    btn_join: DRectButton,
    /// 参加请求任务；`Some` 期间页面显示全屏 loading 并吞掉输入，避免重复提交。
    join_task: Option<Task<Result<Option<String>>>>,

    /// 用户是否已手动滚动过页面（滚动过就不再提示「下滑查看更多」）。
    scrolled: bool,
    /// 本场景的进入时刻（`tm.now()`），用于提示语与按钮的入场计时。
    start_time: f32,

    /// 侧边排行榜的转场计时基准：正值为「正在/已经滑入」，负值表示正在滑出，
    /// `f32::NAN` 表示排行榜当前不可用（未参加、活动未开始/已结束或已收回）。
    side_enter_time: f32,

    /// 侧边排行榜的滚动容器与淡入淡出控制器。
    ldb_scroll: Scroll,
    ldb_fader: Fader,
    /// 排行榜请求任务与结果缓存（`None` 表示尚未加载或正在加载）。
    ldb_task: Option<Task<Result<Vec<LdbItem>>>>,
    ldb: Option<Vec<LdbItem>>,

    /// 共享图标集（返回键等）。
    icons: Arc<Icons>,
    /// 第 1..=8 名的名次图标，由调用方注入（与资料页复用同一套资源）。
    rank_icons: [SafeTexture; 8],

    /// 上一次触摸是否已被滚动容器消费。用于区分「滑动页面」与「点击 UML 控件」：
    /// 同一次滑动手势中既不该触发滚动又触发控件，因此需要记住上一次的归属。
    last_scroll_handled_touch: bool,

    /// 子场景转场控制器（用于从排行榜条目进入 `ProfileScene`）。
    sf: SFader,
}

// `EventScene` 的构造与活动数据加载：
// 构造时只发起 UML 拉取（或调试模式下的空占位），参与状态在 `enter` 时才查询，
// 排行榜进一步推迟到用户真正要看时才加载，避免进入活动页瞬间并发多个请求。
impl EventScene {
    /// 创建活动场景。
    ///
    /// `event`/`illu`/`icons`/`rank_icons` 由活动列表页传入（插图与图标是全局共享资源）。
    /// 非调试模式下会立即发起 `/event/{id}/uml` 请求，并附带客户端版本号
    /// 以便服务端为旧版本下发兼容的脚本；失败由 `update` 统一提示。
    pub fn new(event: Event, illu: Illustration, icons: Arc<Icons>, rank_icons: [SafeTexture; 8]) -> Self {
        let id = event.id;
        Self {
            event,
            illu,

            scroll: Scroll::new(),

            btn_back: RectButton::new(),

            status_task: None,
            status: None,

            // 调试模式：不发请求，改由 `update` 监听本地 `test.uml` 的热重载。
            uml_task: if DEBUG_MODE {
                None
            } else {
                // 上报客户端版本号，服务端可据此下发与当前客户端兼容的 UML 脚本。
                Some(Task::new(async move {
                    Ok(recv_raw(Client::get(format!("/event/{id}/uml")).query(&[("version", env!("CARGO_PKG_VERSION"))]))
                        .await?
                        .text()
                        .await?)
                }))
            },
            // 初值为空 UML：脚本到达前页面只显示背景、返回键与参加按钮。
            uml: Uml::default(),
            // 记录构造时刻，调试模式下用它和 `test.uml` 的 mtime 比较以触发首次加载。
            last_modified: SystemTime::now(),

            next_scene: None,

            btn_join: DRectButton::new(),
            join_task: None,

            scrolled: false,
            start_time: 0.,

            // NAN 表示侧栏初始不可用：只有在活动时间内且已参加时才会被赋值。
            side_enter_time: f32::NAN,

            ldb_scroll: Scroll::new(),
            ldb_fader: Fader::new(),
            ldb_task: None,
            ldb: None,

            icons,
            rank_icons,

            last_scroll_handled_touch: false,

            sf: SFader::new(),
        }
    }

    /// （重新）查询本用户的参与状态。
    ///
    /// 先清空缓存，使参加按钮回到 loading 态（避免在请求返回前用旧状态误导用户）；
    /// 在 `enter` 以及参加成功后被调用。
    fn load_status(&mut self) {
        self.status = None;
        let id = self.event.id;
        self.status_task = Some(Task::new(async move { Ok(recv_raw(Client::get(format!("/event/{id}/status"))).await?.json().await?) }));
    }

    /// 拉取活动排行榜（服务端只返回前若干名，故接口名为 list15）。
    /// 同样先清空缓存让渲染端显示 loading；重复调用是幂等的。
    fn load_ldb(&mut self) {
        let id = self.event.id;
        self.ldb = None;
        self.ldb_task = Some(Task::new(async move { Ok(recv_raw(Client::get(format!("/event/{id}/list15"))).await?.json().await?) }));
    }

    /// 是否处于「参加请求进行中」的阻塞态：此时页面绘制全屏 loading 并吞掉所有输入。
    fn loading(&self) -> bool {
        self.join_task.is_some()
    }

    /// 参加活动的 HTTP 请求实现（写成静态异步函数是为了能直接构造 `Task`）。
    ///
    /// # Returns
    /// - `Ok(Some(msg))`：服务端以业务错误拒绝（403/409，如「已参加」「活动已结束」），
    ///   `msg` 是可直接展示给用户的文案；
    /// - `Ok(None)`：参加成功，调用方应重新拉取参与状态；
    /// - `Err(_)`：网络错误或未预期的状态码。
    async fn join_task(id: i32) -> Result<Option<String>> {
        let request = Client::post(format!("/event/{id}/join"), &());
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.context("failed to receive text")?;
            let status_str = status.as_str().to_owned();
            // 403/409 属于可预期的业务拒绝：把服务端文案交还给 UI 展示；
            // 其他状态码（5xx 等）一律作为错误上抛走统一错误提示。
            if let Ok(what) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(detail) = what["error"].as_str() {
                    if matches!(status, StatusCode::FORBIDDEN | StatusCode::CONFLICT) {
                        return Ok(Some(detail.to_string()));
                    }
                    bail!("request failed ({status_str}): {detail}");
                }
            }
            // 响应体不是预期 JSON（如网关错误页）时，退化为原始文本报错。
            bail!("request failed ({status_str}): {text}");
        };
        Ok(None)
    }

    /// 「参加」入口的统一处理：按当前已知的参与状态决定行为。
    /// - 已参加且处于活动时间内：滑出侧边排行榜（首次需要时加载榜单）；
    /// - 未参加：发起参加请求，由 `update` 消费结果并刷新状态；
    /// - 状态未知（`status` 为 `None`）：不做任何事，避免在查询返回前重复提交。
    fn join_or(&mut self, rt: f32) {
        if let Some(status) = &self.status {
            if status.joined {
                // 只有在活动开始之后、结束之前才允许查看榜单（否则榜单无意义）。
                if (self.event.time_start..self.event.time_end).contains(&Utc::now()) {
                    if self.ldb_task.is_none() && self.ldb.is_none() {
                        self.load_ldb();
                    }
                    self.side_enter_time = rt;
                }
            } else {
                self.join_task = Some(Task::new(Self::join_task(self.event.id)));
            }
        }
    }
}

// `EventScene` 的引擎钩子：
// - `enter`：记录进入时刻并发起参与状态查询；
// - `on_result`：只认引擎回传的 `bool`（UML 内部弹窗的确认结果），其余类型原样丢弃；
// - `touch`：按 侧栏排行榜 → 返回/参加按钮 → 页面滚动 → UML 控件 → 排行榜条目 的顺序分发；
// - `update`：推进滚动，并轮询 UML 热重载/拉取、参与状态、参加请求、排行榜四个异步任务；
// - `render`：插图 → 返回键 → 滚动主体（活动名 + UML）→ 提示 → 参加按钮 → 侧栏榜单 → UML 顶层 → 子场景；
// - `next_scene`：优先返回子场景（玩家资料页），其次是本场景与 UML 请求的切换。
impl Scene for EventScene {
    /// 引擎回传结果：只有 UML 内部弹窗的确认结果以 `bool`（删除/确认）形式回传，
    /// 转交 `Uml::on_result` 处理；其他类型无处消费，直接忽略（`_res` 仅用于持有所有权）。
    fn on_result(&mut self, tm: &mut TimeManager, res: Box<dyn Any>) -> Result<()> {
        let _res = match res.downcast::<bool>() {
            Err(res) => res,
            Ok(delete) => {
                self.uml.on_result(tm.now() as _, *delete);
                return Ok(());
            }
        };
        Ok(())
    }

    /// 进入场景：重置入场计时（提示语与按钮动画据此计时），并查询参与状态。
    fn enter(&mut self, tm: &mut TimeManager, _target: Option<RenderTarget>) -> Result<()> {
        self.start_time = tm.now() as _;
        self.load_status();
        Ok(())
    }

    /// 触摸分发，优先级：阻塞态 → 侧栏排行榜（独占）→ 返回/参加按钮（仅在页面顶部时）
    /// → 页面滚动 → UML 控件 → 排行榜条目。
    ///
    /// 侧栏排行榜展开时独占输入，是因为它覆盖在页面之上：此时任何手势都应先用于
    /// 收放侧栏或滚动榜单，落到下层页面会造成「点击穿透」。
    fn touch(&mut self, tm: &mut TimeManager, touch: &Touch) -> Result<bool> {
        let t = tm.now() as f32;
        let rt = tm.real_time() as f32;

        // 参加请求进行中：吞掉输入，防止重复提交或误触其他控件。
        if self.loading() {
            return Ok(true);
        }

        // 侧栏排行榜处于激活状态（滑入中/已展开/滑出中）。
        if !self.side_enter_time.is_nan() {
            // 已完全展开后，点击左侧露出的空条带即收回侧栏（只在 Started 时判定，
            // 避免滑动手势被误判为关闭）。
            if self.side_enter_time > 0.
                && tm.real_time() as f32 > self.side_enter_time + TRANSIT_TIME
                && touch.position.x < 1. - LDB_WIDTH
                && touch.phase == TouchPhase::Started
            {
                self.side_enter_time = -rt;
                return Ok(true);
            }
            if self.ldb_scroll.touch(touch, t) {
                return Ok(true);
            }
            // 侧栏激活期间不把触摸透传给下层页面。
            return Ok(false);
        }

        // 仅在页面接近顶部时才响应返回键与参加按钮：这两个按钮会随滚动淡出，滚动后不可见也不应可点。
        if self.scroll.y_scroller.offset < 0.3 {
            if self.btn_back.touch(touch) {
                button_hit();
                self.next_scene = Some(NextScene::Pop);
                return Ok(true);
            }
            if self.btn_join.touch(touch, t) {
                self.join_or(rt);
                return Ok(true);
            }
        }

        // 滑动优先级：若上一次触摸已由滚动容器消费，本次仍优先交给滚动，
        // 这样一次连续滑动不会被 UML 控件「截胡」而中断。
        if self.last_scroll_handled_touch && self.scroll.touch(touch, t) {
            self.scrolled = true;
            return Ok(true);
        } else {
            self.last_scroll_handled_touch = false;
        }

        // UML 控件：命中后由脚本返回一个动作字符串，约定 exit / join / open:<url>。
        let mut action = None;
        if self.uml.touch(touch, t, rt, &mut action)? {
            if let Some(action) = action {
                match action.as_str() {
                    "exit" => {
                        // 退出活动页（返回活动列表）。
                        self.next_scene = Some(NextScene::Pop);
                    }
                    "join" => {
                        // 与参加按钮等价的行为。
                        self.join_or(rt);
                    }
                    x => {
                        // 其他动作统一视为外部链接（由脚本给出完整 URL）。
                        if let Some(url) = x.strip_prefix("open:") {
                            open_url(url)?;
                        }
                    }
                }
            }
            return Ok(true);
        }

        // UML 未命中时再尝试滚动：此时记录归滚动所有，供下一次触摸的优先级判断使用。
        if !self.last_scroll_handled_touch && self.scroll.touch(touch, t) {
            self.scrolled = true;
            self.last_scroll_handled_touch = true;
            return Ok(true);
        }

        // 排行榜条目：点击进入对应玩家的资料页（作为子场景，带转场动画）。
        if let Some(ldb) = &mut self.ldb {
            for item in ldb {
                if item.btn.touch(touch) {
                    button_hit();
                    self.sf
                        .goto(t, ProfileScene::new(item.player, self.icons.user.clone(), self.rank_icons.clone()));
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    /// 每帧推进：滚动容器 → 侧栏榜单触发加载 → UML（调试热重载或拉取结果）
    /// → 参与状态 → 参加请求 → 排行榜请求。
    ///
    /// 每个任务都用 `task.take()` 一次性取走结果，保证同一任务不会被重复消费；
    /// 任务结果都只在本函数里落地，渲染/触摸读到的状态因此始终是已提交的。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        let t = tm.now() as f32;

        self.scroll.update(t);

        // 侧栏榜单被下拉到底时重新拉取排行榜（下拉触发，无需额外按钮）。
        if self.ldb_scroll.y_scroller.pulled {
            self.load_ldb();
        }
        self.ldb_scroll.update(t);

        // UML 来源二选一（由 `DEBUG_MODE` 编译期决定）：
        if DEBUG_MODE {
            // 调试模式：轮询本地 `test.uml` 的修改时间，变化即重新解析；
            // 解析失败时打印错误并回落为空 UML，保证调试过程中页面不崩。
            let path = std::path::Path::new("test.uml");
            if let Ok(meta) = path.metadata() {
                let new_modified = meta.modified()?;
                if new_modified != self.last_modified {
                    self.last_modified = new_modified;
                    self.uml = parse_uml(&std::fs::read_to_string(path)?, &self.icons, &self.rank_icons).unwrap_or_else(|e| {
                        eprintln!("{e:?}");
                        Uml::default()
                    });
                }
            }
        } else if let Some(task) = &mut self.uml_task {
            // 正式模式：脚本拉取完成后解析；解析失败会中断本帧（`uml_task` 已清空，
            // 因此不会无限重试），由上层错误处理展示原因。
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("load-failed")));
                    }
                    Ok(res) => {
                        self.uml = parse_uml(&res, &self.icons, &self.rank_icons).map_err(anyhow::Error::msg)?;
                    }
                }
                self.uml_task = None;
            }
        }

        // 参与状态返回：写入缓存后参加按钮与侧栏可用性随之更新。
        if let Some(task) = &mut self.status_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("load-status-failed")));
                    }
                    Ok(val) => {
                        self.status = Some(val);
                    }
                }
                self.status_task = None;
            }
        }

        // 参加请求返回：`Some(msg)` 是服务端的业务性拒绝（用普通对话框提示即可），
        // `None` 表示成功，此时重新拉取状态以刷新按钮与榜单权限。
        if let Some(task) = &mut self.join_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("join-failed")));
                    }
                    Ok(message) => {
                        if let Some(message) = message {
                            Dialog::simple(message).show();
                        } else {
                            self.load_status();
                        }
                    }
                }
                self.join_task = None;
            }
        }

        // 排行榜返回：先为榜上玩家批量预取用户信息（昵称/头像），再写入缓存，
        // 这样渲染时能立刻拿到名字而不是先显示 id 再刷新。
        if let Some(task) = &mut self.ldb_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("load-ldb-failed")));
                    }
                    Ok(ldb) => {
                        for item in ldb.iter() {
                            UserManager::request(item.player);
                        }
                        self.ldb = Some(ldb);
                    }
                }
                self.ldb_task = None;
            }
        }

        Ok(())
    }

    /// 绘制活动页：插图遮罩 → 返回键 → 滚动主体（活动名 + UML 内容）→ 下滑提示
    /// → 参加按钮 → 侧栏排行榜 → UML 顶层元素 → 子场景 → 加载指示。
    ///
    /// 滚动偏移会同时驱动返回键/参加按钮的透明度与整屏遮罩深度，
    /// 形成「下滑后固定控件淡出」的观感；因此绘制顺序里它们必须晚于滚动内容。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        set_camera(&ui.camera());
        let t = tm.now() as f32;
        let rt = tm.real_time() as f32;

        // 插图背景 + 固定半透明黑遮罩，保证页面上的白字始终可读。
        let r = ui.screen_rect();
        ui.fill_rect(r, self.illu.shading(r, t));
        ui.fill_rect(r, semi_black(0.4));

        // 固定控件的透明度：随滚动偏移（0..0.4）线性衰减到 0。
        let p = 1. - (self.scroll.y_scroller.offset / 0.4).clamp(0., 1.);

        let r = ui.back_rect();
        ui.fill_rect(r, (*self.icons.back, r, ScaleType::Fit, semi_white(p)));
        self.btn_back.set(ui, r);

        // 滚动越深，整屏遮罩越重（最多 0.7），把滚动内容「压暗」在后面的固定控件之下。
        ui.fill_rect(ui.screen_rect(), semi_black((self.scroll.y_scroller.offset / 0.3).min(1.) * 0.7));
        ui.scope(|ui| {
            // 滚动区以屏幕左边界为原点向左平移一整屏，配合宽度 2 的容器实现「只纵向滚动」。
            ui.dx(-1.);
            ui.dy(-ui.top);
            let o = self.scroll.y_scroller.offset;
            self.scroll.size((2., ui.top * 2.));
            self.scroll.render(ui, |ui| {
                let top = ui.top;
                // 活动名固定在滚动内容顶部，随内容一起滚走。
                ui.text(&self.event.name)
                    .pos(EventPage::LB_PAD, top * 2. - EventPage::LB_PAD)
                    .anchor(0., 1.)
                    .size(1.5)
                    .draw();
                ui.dy(ui.top * 2.);
                if self.uml_task.is_some() {
                    // UML 尚未到达：显示一个加载动画占位，并按占位高度返回内容高度。
                    let pad = 0.06;
                    ui.loading(1., pad + 0.05, t, WHITE, ());
                    (2., ui.top * 2. + (pad + 0.05) * 2.)
                } else {
                    // UML 内容：上下文变量由脚本按名字读取（时间、滚动偏移、屏幕半高、
                    // 是否已参加——未参加用 -1 表示「未知」）。
                    let h = match self.uml.render(
                        ui,
                        t,
                        rt,
                        &[
                            ("t", t),
                            ("o", o),
                            ("top", ui.top),
                            ("joined", self.status.as_ref().map_or(-1., |it| it.joined as u32 as f32)),
                        ],
                    ) {
                        Ok((_, h)) => h,
                        Err(e) => {
                            // UML 渲染出错不阻断整帧：打印后按 0 高度继续绘制其余元素。
                            eprintln!("{e:?}");
                            0.
                        }
                    };
                    (2., ui.top * 2. + h + 0.02)
                }
            });
        });

        // 下滑提示：进入 2 秒后才出现（先让用户注意到右上角的参加按钮），
        // 之后以正弦缓慢呼吸；用户一旦滚动过页面就不再提示。
        let elapsed = t - self.start_time;
        if !self.scrolled && elapsed > 2. {
            let top = ui.top;
            ui.text(tl!("scroll-down-for-more"))
                .pos(0., top - 0.03)
                .anchor(0.5, 1.)
                .size(0.4)
                .color(semi_white((((elapsed - 2.) * 1.5 - std::f32::consts::FRAC_PI_2).sin() + 1.) / 2.))
                .draw();
        }

        // 参加按钮：透明度同时受滚动偏移（`p`）与入场时间影响，状态未知时显示 loading 圈。
        ui.alpha(p * (elapsed / 0.3).min(1.), |ui| {
            let r = Rect::new(1. - 0.24, ui.top - 0.12, 0., 0.).nonuniform_feather(0.19, 0.07);
            let ct = r.center();
            if let Some(status) = &self.status {
                let bc = ui.background();
                // 绘制工具：按钮底色按状态变化，文字尺寸随按钮高度缩放。
                let mut draw = |text, bc| {
                    let oh = r.h;
                    self.btn_join.render_shadow(ui, r, t, |ui, path| {
                        ui.fill_path(&path, Color { a: p, ..bc });
                        ui.text(text)
                            .pos(ct.x, ct.y)
                            .anchor(0.5, 0.5)
                            .no_baseline()
                            .size(0.8 * (1. - (1. - r.h / oh).powf(1.3)))
                            .max_width(r.w)
                            .draw();
                    });
                };
                if status.joined {
                    // 已参加：按当前时间判定三种展示 —— 已结束 / 未开始 / 进行中显示名次。
                    if Utc::now() > self.event.time_end {
                        draw(tl!("btn-ended"), semi_black(0.4));
                    } else if Utc::now() < self.event.time_start {
                        draw(tl!("btn-not-started"), Color::from_hex_rgb(0xe3f2fd));
                    } else {
                        // 进行中：橙色按钮上显示 `#名次` 与前缀榜单图标。
                        // 注意：此处按服务端约定「已参加且活动进行中必有成绩」直接 unwrap。
                        self.btn_join
                            .render_shadow(ui, r, t, |ui, path| ui.fill_path(&path, Color::from_hex_rgb(0xf57c00)));
                        let mut text = ui.text(format!("#{}", status.rank.unwrap())).anchor(0., 0.5).no_baseline().size(0.7);
                        let w = text.measure().w;
                        let mut ir = Rect::new(ct.x, ct.y, 0., 0.).feather(r.h / 2. - 0.02);
                        let w = w + 0.01 + ir.w;
                        ir.x += (ir.w - w) / 2.;
                        text.pos(ir.right() + 0.01, ct.y).draw();
                        ui.fill_rect(ir, (*self.icons.ldb, ir, ScaleType::Fit));
                    }
                } else {
                    draw(tl!("btn-join"), bc);
                }
            } else {
                // 状态查询中：灰色按钮 + 中央 loading 圈（此时按钮不可点）。
                self.btn_join.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.4));
                });
                ui.loading(
                    ct.x,
                    ct.y,
                    t,
                    WHITE,
                    LoadingParams {
                        radius: 0.03,
                        width: 0.008,
                        ..Default::default()
                    },
                );
            }
        });

        // 侧栏排行榜：进度 `p` 由「进入时间 + 转场时长」推导并做缓出；
        // `side_enter_time` 为负表示正在滑出，滑出动画播完后复位为 NAN（回到未激活态）。
        if !self.side_enter_time.is_nan() {
            let p = ((rt - self.side_enter_time.abs()) / TRANSIT_TIME).min(1.);
            let p = 1. - (1. - p).powi(3);
            let p = if self.side_enter_time < 0. {
                if p >= 1. {
                    self.side_enter_time = f32::NAN;
                }
                1. - p
            } else {
                p
            };
            ui.fill_rect(ui.screen_rect(), semi_black(p * 0.6));
            let w = LDB_WIDTH;
            let lf = f32::tween(&1.04, &(1. - w), p);
            ui.scope(|ui| {
                ui.dx(lf);
                ui.dy(-ui.top);
                // 面板左侧额外留出 0.2 宽的黑色条带：既是渐变阴影，也是「点击收回」的判定区域。
                let r = Rect::new(-0.2, 0., 0.2 + w, ui.top * 2.);
                ui.fill_rect(r, (Color::default(), (r.x, r.y), Color::new(0., 0., 0., p * 0.7), (r.right(), r.y)));
                // 榜单条目与点击区域由 `render_ldb` 统一绘制/回填（与资料页共用同一实现）。
                render_ldb(
                    ui,
                    &tl!("ldb"),
                    LDB_WIDTH,
                    rt,
                    &mut self.ldb_scroll,
                    &mut self.ldb_fader,
                    &self.icons.user,
                    self.ldb.as_mut().map(|it| {
                        it.iter_mut().map(|it| LdbDisplayItem {
                            player_id: it.player,
                            rank: it.rank as _,
                            score: it.score.to_string(),
                            alt: None,
                            btn: &mut it.btn,
                        })
                    }),
                );
            });
        }

        // UML 的顶层元素（浮层、弹窗）绘制在侧栏之后，保证脚本可以盖住固定 UI。
        self.uml.render_top(ui, t, rt)?;

        // 子场景（玩家资料页）与全屏 loading 最后绘制。
        self.sf.render(ui, t);

        if self.loading() {
            ui.full_loading_simple(t);
        }

        Ok(())
    }

    /// 返回场景切换请求，优先级为：子场景（玩家资料页）→ 本场景请求的切换（如返回）
    /// → UML 内部请求的切换（如打开曲库/某张谱面）。
    ///
    /// 只要返回非 `None` 引擎就会离开本场景，因此子场景转场结束前不会轮到后两者。
    fn next_scene(&mut self, tm: &mut TimeManager) -> NextScene {
        if let Some(scene) = self.sf.next_scene(tm.now() as _) {
            return scene;
        }
        self.next_scene.take().or_else(|| self.uml.next_scene()).unwrap_or_default()
    }
}
