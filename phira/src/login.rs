//! 登录 / 注册 UI 面板（由首页 `HomePage` 持有并渲染）。
//!
//! 职责边界：本模块只负责**界面与表单状态机**——输入框内容、本地校验、子面板切换、
//! 按钮反馈与结果提示；真正的网络请求全部委托给 `crate::client`（如 `Client::login`、
//! `Client::register`、`Client::login_hykb` 等）。因此这里不持有 HTTP 客户端，也不关心
//! 重试、鉴权头等细节。
//!
//! 面板有若干互相切换的形态：
//! - 邮箱登录表单 / 邮箱注册表单（`in_reg` 决定显示哪一个）；
//! - HYKB 渠道的「登录方式选择」面板（邮箱 or HYKB）；
//! - HYKB 渠道登录成功但账号尚未建立时的「注册新账号 / 绑定已有账号」选择；
//! - HYKB 新账号的「选择用户名」面板。
//!
//! 表单校验规则集中在 [`validate_username`] 与 [`EMAIL_REGEX`]；这些约束需要与服务端
//! 保持一致，否则会出现「本地校验通过、服务端仍拒绝」的割裂体验。
prpr_l10n::tl_file!("login");

use crate::{
    client::{Client, ErrorCode, LoginParams, User, UserManager, API_URL},
    get_data_mut,
    icons::Icons,
    page::Fader,
    save_data,
    scene::{check_read_tos_and_policy, confirm_dialog, dispatch_tos_task, JUST_ACCEPTED_TOS},
};
use anyhow::Result;
use inputbox::{InputBox, InputMode};
use macroquad::prelude::*;
use once_cell::sync::Lazy;
#[cfg(feature = "hykb")]
use prpr::ext::ScaleType;
use prpr::{
    core::BOLD_FONT,
    ext::{open_url, semi_black, semi_white, RectExt},
    scene::{request_input, return_input, show_error, show_message, take_input},
    task::Task,
    ui::{button_hit, DRectButton, Dialog, RectButton, Ui},
};
use regex::Regex;
use std::{future::Future, sync::atomic::AtomicBool, sync::atomic::Ordering, sync::Arc};

/// 用户名允许的最短长度（按字符数计，非字节数）。
const USERNAME_LEN_MIN: usize = 2;
/// 用户名允许的最长长度：过长的名字会在列表/排行榜里撑破布局，也便于服务端存储约束。
const USERNAME_LEN_MAX: usize = 14;

/// 密码最短长度，用于强制最低强度的复杂度门槛。
const PWD_LEN_MIN: usize = 8;
/// 密码最长长度：与服务端存储/哈希约束对齐，避免提交后才发现被截断或拒绝。
const PWD_LEN_MAX: usize = 32;

// HYKB（渠道登录）相关的导入：只在启用 `hykb` feature 时引入，保证开源构建不带渠道依赖。
#[cfg(feature = "hykb")]
use crate::{client::HykbLoginOutcome, obtain_hykb_credential};
#[cfg(feature = "hykb")]
use prpr::scene::take_input_cancelled;
#[cfg(feature = "hykb")]
use std::sync::Mutex;

/// The user's choice in the HYKB "register or claim" dialog.
/// 中文说明：首次用 HYKB 登录时，服务端无法判断该渠道账号对应的 Phira 账号该新建还是
/// 关联到已有账号，故由用户三选一。
#[cfg(feature = "hykb")]
#[derive(Clone, Copy)]
enum HykbChoice {
    /// 渠道账号是全新的，创建一个与之绑定的新 Phira 账号。
    Register,
    /// 渠道账号对应的 Phira 账号已存在，输入已有账号的邮箱密码完成关联（账号合并）。
    Claim,
    /// The player dismissed the dialog without choosing; back out to the picker.
    /// 中文说明：用户点了对话框外部/关闭——退回上层（登录方式选择面板），不作任何绑定。
    Cancel,
}

/// Result of the initial HYKB login step.
/// 中文说明：HYKB 登录第一阶段（校验渠道凭据）的两种可能结果。
#[cfg(feature = "hykb")]
enum HykbStep {
    /// The HYKB account was already bound; carries the fetched user.
    /// 中文说明：该渠道账号此前已绑定，直接拿到已登录用户，流程结束。
    LoggedIn(Box<User>),
    /// The account is new; carries the short-lived token for register/claim and
    /// the HYKB nickname used to prefill the username input.
    /// 中文说明：需要用户选择注册或绑定。`hykb_token` 是服务端下发的短时凭据，
    /// 只在后续的注册/绑定请求中一次性使用；`nick` 用于预填新账号的用户名输入框，
    /// 减少用户输入成本（渠道昵称不保证合法，故仅作预填、仍需本地校验）。
    NeedChoice { hykb_token: String, nick: String },
}

/// 邮箱格式校验用的正则（编译一次后全局复用）。
/// 中文说明：这是 HTML5 `input[type=email]` 规范的经典正则（local-part 允许点号分段、
/// 域名至少一个点且各级不以连字符开头/结尾）。它只做**结构合法性**检查，不验证邮箱
/// 是否真实存在；服务端仍会做权威校验与可达性确认。
static EMAIL_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"\A[a-z0-9!#$%&'*+/=?^_‘{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_‘{|}~-]+)*@(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\z",
    )
    .unwrap()
});

/// 校验用户名，合法返回 `None`，否则返回可直接展示给用户的本地化错误串。
///
/// 具体规则（按顺序短路返回首个错误）：
/// 1. 长度必须落在 `[USERNAME_LEN_MIN, USERNAME_LEN_MAX]` 内，且**按字符数**统计
///    （`chars().count()`），这样中文等宽字符按「一个字」计，符合用户直觉；用字节数
///    会误伤中文名、也会与展示宽度脱节。
/// 2. 字符集白名单：只允许 `_`、`-` 以及 `is_alphanumeric()`（含中文）的字符。
///    这条白名单用于阻止注入类符号、控制/不可见字符与表情符号——它们可用于伪造
///    显示名（冒名）、破坏排版，或在下游拼接（如 URL、日志、HTML）时造成注入风险。
///
/// 注意：代码**未对首字符做额外限制**，因此 `_abc`、`-abc`、`1abc` 均可通过；
/// 唯一的硬约束就是上面两条，规则以服务端为准。
fn validate_username(username: &str) -> Option<String> {
    if !(USERNAME_LEN_MIN..=USERNAME_LEN_MAX).contains(&username.chars().count()) {
        return Some(tl!("name-length-req", "min" => USERNAME_LEN_MIN, "max" => USERNAME_LEN_MAX));
    }
    if username.chars().any(|it| it != '_' && it != '-' && !it.is_alphanumeric()) {
        return Some(tl!("name-has-illegal-char").into_owned());
    }
    None
}

/// 登录 / 注册面板的状态与控件集合。
/// 由 `HomePage` 持有一个实例，通过 [`Login::touch`] / [`Login::update`] / [`Login::render`]
/// 参与每帧的输入、逻辑与绘制；它自身不发起网络请求，只通过 `task` 交给 `Client`。
pub struct Login {
    /// HYKB 渠道按钮上的图标资源；仅在 `hykb` 构建中用到。
    #[cfg(feature = "hykb")]
    icons: Arc<Icons>,

    /// 邮箱表单整体的淡入淡出控制器（同时用于带动画切换登录/注册两页）。
    fader: Fader,
    /// 邮箱表单当前是否展示（与 `fader` 的动画进度共同决定可见性）。
    show: bool,

    /// In HYKB builds an account is mandatory: while set, the panel is kept
    /// open and cannot be dismissed by tapping outside — only a successful
    /// login clears it.
    /// 中文说明：`forced` 来自渠道（HYKB）强制登录的合规要求——渠道构建下必须先取得
    /// 账号才能进入游戏（例如满足防沉迷/实名相关要求）。置位后面板**不可点外部关闭**，
    /// 点外部只会退回上一层子面板；只有登录成功才会清除。UI 上表现为没有“关闭”出口。
    #[cfg(feature = "hykb")]
    forced: bool,

    /// The method-choice panel ("email vs HYKB"), shown before the form when
    /// HYKB is available.
    /// 中文说明：登录方式选择面板的淡入淡出控制器；仅在 HYKB 可用时作为邮箱表单的入口。
    #[cfg(feature = "hykb")]
    picker_fader: Fader,
    /// 选择面板是否展示。
    #[cfg(feature = "hykb")]
    picker_show: bool,
    /// 「邮箱登录」按钮。
    #[cfg(feature = "hykb")]
    btn_method_email: DRectButton,
    /// 「HYKB 登录」按钮（品牌绿）。
    #[cfg(feature = "hykb")]
    btn_method_hykb: DRectButton,

    /// 登录表单的邮箱输入区域（点击后拉起原生输入框）。
    input_email: DRectButton,
    /// 登录表单的密码输入区域。
    input_pwd: DRectButton,
    /// 注册表单的邮箱输入区域。
    input_reg_email: DRectButton,
    /// 注册表单的用户名输入区域。
    input_reg_name: DRectButton,
    /// 注册表单的密码输入区域。
    input_reg_pwd: DRectButton,

    /// 「去注册」切换按钮（登录页 → 注册页）。
    btn_to_reg: DRectButton,
    /// 「返回登录」切换按钮（注册页 → 登录页）。
    btn_to_login: DRectButton,
    /// 「注册」提交按钮。
    btn_reg: DRectButton,
    /// 「登录」提交按钮。
    btn_login: DRectButton,
    /// 「忘记密码」链接按钮，跳转到官网重置密码页。
    btn_forget_pwd: RectButton,

    /// 登录表单当前输入的邮箱（跨帧保存在面板里，切换页签不丢失）。
    t_email: String,
    /// 登录表单当前输入的密码；只用于提交，成功后会被清空。
    t_pwd: String,
    /// 注册表单当前输入的邮箱。
    t_reg_email: String,
    /// 注册表单当前输入的用户名。
    t_reg_name: String,
    /// 注册表单当前输入的密码；同样只用于提交。
    t_reg_pwd: String,

    /// 登录/注册两页切换动画的起始时刻；`NaN` 表示当前没有切换动画在进行。
    start_time: f32,
    /// 当前是否处于注册页（与 `start_time` 驱动的滑动动画共同决定展示哪一页）。
    in_reg: bool,

    /// 当前进行中的请求任务：`desc` 是动作名（用于拼装“XX成功/失败”提示），
    /// 结果中的 `Option<User>` 为 `None` 表示该动作不产生登录态（如注册仅发验证邮件）。
    task: Option<(&'static str, Task<Result<Option<User>>>)>,
    /// 用户因未同意服务条款而被中断的动作；同意后（`JUST_ACCEPTED_TOS`）据此续跑。
    after_accept_tos: Option<NextAction>,
    /// Email/password held while the player decides whether to cancel a pending
    /// account deletion request and retry the login with `cancel_delete_request`.
    /// 中文说明：登录失败若因“账号存在待处理的注销申请”，需要用户确认撤销后重试；
    /// 这里缓存当时的邮箱密码，供确认后以 `cancel_delete_request = true` 重新提交。
    pending_delete_retry: Option<(String, String)>,
    /// Result flag for the pending-delete confirmation dialog.
    /// 中文说明：确认对话框的结果标志（由对话框监听器写入、本面板轮询读取）。
    pending_delete_confirm: Arc<AtomicBool>,
    /// HYKB login phase 1 (verify uid/token), distinct from `task` which handles
    /// the register/claim follow-up that resolves to a `User`.
    /// 中文说明：渠道登录第一阶段的任务（校验渠道凭据），与 `task` 分开保存，
    /// 因为它产出的是 [`HykbStep`] 而非最终用户。
    #[cfg(feature = "hykb")]
    hykb_task: Option<Task<Result<HykbStep>>>,
    /// Pending HYKB token awaiting the user's register/claim choice.
    /// 中文说明：等待用户选择「注册/绑定」期间暂存的渠道短时凭据。
    #[cfg(feature = "hykb")]
    hykb_pending_token: Option<String>,
    /// HYKB token kept while the player types the username for their new account.
    /// 中文说明：进入“选择用户名”子面板后凭据转移到这里保存，直到提交注册。
    #[cfg(feature = "hykb")]
    hykb_reg_token: Option<String>,
    /// HYKB nickname, used only to prefill the username input for a new account.
    /// 中文说明：渠道昵称，仅用于预填用户名输入框，不作信任来源。
    #[cfg(feature = "hykb")]
    hykb_nick: Option<String>,
    /// Choice written by the register/claim dialog listener.
    /// 中文说明：对话框监听器与面板主循环跨闭包/跨帧通信的槽位——
    /// 监听器写、[`Login::update_hykb`] 读并清空。
    #[cfg(feature = "hykb")]
    hykb_choice: Arc<Mutex<Option<HykbChoice>>>,

    /// The in-app "choose your username" panel shown for a new HYKB account,
    /// in place of popping the native InputBox directly. The InputBox only
    /// appears when the player taps the input slot inside this panel.
    /// 中文说明：不用系统输入框直接收集用户名，而是自绘一层面板：这样能展示长度/字符
    /// 规则提示、保持视觉一致，也避免输入框被覆盖层遮挡。原生输入框只在点击面板内输入槽时唤起。
    #[cfg(feature = "hykb")]
    reg_name_fader: Fader,
    /// 「选择用户名」面板是否展示。
    #[cfg(feature = "hykb")]
    reg_name_show: bool,
    /// 「选择用户名」面板中的输入槽（点击后请求原生输入框）。
    #[cfg(feature = "hykb")]
    input_hykb_name: DRectButton,
    /// 「选择用户名」面板的确认按钮。
    #[cfg(feature = "hykb")]
    btn_hykb_name_confirm: DRectButton,
    /// 「选择用户名」面板当前输入的名字。
    #[cfg(feature = "hykb")]
    t_hykb_name: String,
}

/// 因未同意服务条款（TOS）而被挂起的动作，待用户同意后继续执行。
enum NextAction {
    /// 继续邮箱登录。
    Login,
    /// 继续邮箱注册。
    Register,
    /// 继续 HYKB 登录。
    #[cfg(feature = "hykb")]
    Hykb,
}

// 面板的构造、显隐控制与表单提交逻辑。整体是一个手写的状态机：
// touch 负责“输入”，update 负责“推进任务与动画”，render 负责“绘制”，
// 三者都由 HomePage 每帧调用，面板自身不持有循环。
impl Login {
    /// 登录/注册两页切换动画的时长（秒）。
    const TIME: f32 = 0.7;

    /// 构造面板：所有子面板初始均为隐藏，输入内容为空，无进行中的任务。
    /// 各控件的动画参数（下沉量、圆角、淡入距离与时长）在此一次性配置。
    /// # Arguments
    /// * `icons` - 图标资源；仅 `hykb` 构建会保存并使用，其他构建下显式忽略该参数。
    pub fn new(icons: Arc<Icons>) -> Self {
        #[cfg(not(feature = "hykb"))]
        let _ = icons;
        // 逐字段初始化：交互控件统一设置较小的下沉量（`with_delta`），
        // 使按下时产生轻微的缩放反馈；`start_time` 置为 NaN 表示当前无页面切换动画。
        Self {
            #[cfg(feature = "hykb")]
            icons,

            fader: Fader::new().with_distance(-0.4).with_time(0.5),
            show: false,

            #[cfg(feature = "hykb")]
            forced: false,

            #[cfg(feature = "hykb")]
            picker_fader: Fader::new().with_distance(-0.4).with_time(0.5),
            #[cfg(feature = "hykb")]
            picker_show: false,
            #[cfg(feature = "hykb")]
            btn_method_email: DRectButton::new().with_radius(0.012).with_elevation(0.004),
            #[cfg(feature = "hykb")]
            btn_method_hykb: DRectButton::new().with_radius(0.012).with_elevation(0.004),

            input_email: DRectButton::new().with_delta(-0.002),
            input_pwd: DRectButton::new().with_delta(-0.002),
            input_reg_email: DRectButton::new().with_delta(-0.002),
            input_reg_name: DRectButton::new().with_delta(-0.002),
            input_reg_pwd: DRectButton::new().with_delta(-0.002),

            btn_to_reg: DRectButton::new(),
            btn_to_login: DRectButton::new(),
            btn_reg: DRectButton::new(),
            btn_login: DRectButton::new(),
            btn_forget_pwd: RectButton::new(),

            t_email: String::new(),
            t_pwd: String::new(),
            t_reg_email: String::new(),
            t_reg_name: String::new(),
            t_reg_pwd: String::new(),

            start_time: f32::NAN,
            in_reg: false,

            task: None,
            after_accept_tos: None,
            pending_delete_retry: None,
            pending_delete_confirm: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "hykb")]
            hykb_task: None,
            #[cfg(feature = "hykb")]
            hykb_pending_token: None,
            #[cfg(feature = "hykb")]
            hykb_reg_token: None,
            #[cfg(feature = "hykb")]
            hykb_nick: None,
            #[cfg(feature = "hykb")]
            hykb_choice: Arc::new(Mutex::new(None)),

            #[cfg(feature = "hykb")]
            reg_name_fader: Fader::new().with_distance(-0.4).with_time(0.5),
            #[cfg(feature = "hykb")]
            reg_name_show: false,
            #[cfg(feature = "hykb")]
            input_hykb_name: DRectButton::new().with_delta(-0.002),
            #[cfg(feature = "hykb")]
            btn_hykb_name_confirm: DRectButton::new().with_radius(0.012).with_elevation(0.004),
            #[cfg(feature = "hykb")]
            t_hykb_name: String::new(),
        }
    }

    /// 统一的“发起请求”入口：把 future 包装成可轮询任务挂到 `task` 上，
    /// 并记录动作名用于后续拼装“XX成功/失败”提示。
    /// 约定同一时刻只有一个请求在跑——加载遮罩会拦截输入，故无需排队机制。
    #[inline]
    fn start(&mut self, desc: &'static str, future: impl Future<Output = Result<Option<User>>> + Send + 'static) {
        self.task = Some((desc, Task::new(future)));
    }

    /// 打开登录流程的入口：有渠道（HYKB）时先展示“登录方式选择”面板，
    /// 否则直接进入邮箱表单。是否强制（不可关闭）由调用方用 [`Login::force`] 决定。
    pub fn enter(&mut self, t: f32) {
        // With HYKB available, show the method-choice panel before any form;
        // otherwise go straight to the email form.
        #[cfg(feature = "hykb")]
        self.show_picker(t);
        #[cfg(not(feature = "hykb"))]
        self.show_form(t);
    }

    /// Whether any part of the login flow is currently on screen or in flight
    /// (the picker, the form, a running task, or an in-between HYKB step such as
    /// the register/claim choice dialog or the username input). Used to keep
    /// `force()` from re-showing the picker over a sub-flow.
    /// 中文说明：把“可见”与“正在飞行中”的中间态一并纳入判定（包括动画进行中、
    /// 有待处理 token、有任务在跑、对话框已弹出但面板尚未刷新等），
    /// 目的是让 [`Login::force`] 幂等——不会在子流程进行中把选择面板重新盖上去。
    #[cfg(feature = "hykb")]
    fn is_active(&self) -> bool {
        self.show
            || self.picker_show
            || self.fader.transiting()
            || self.picker_fader.transiting()
            || self.reg_name_show
            || self.reg_name_fader.transiting()
            || self.task.is_some()
            || self.hykb_task.is_some()
            || self.hykb_pending_token.is_some()
            || self.hykb_reg_token.is_some()
            || !self.start_time.is_nan()
    }

    /// Force the login flow open and keep it non-dismissible until the player
    /// logs in. HYKB builds call this whenever the home page is shown while
    /// signed out (fresh launch, or after a manual logout). It is a no-op while
    /// the flow is already active, so it can safely be polled every frame.
    /// 中文说明：这是渠道强制登录约束的施加点。首页在“未登录”状态下每帧轮询调用它，
    /// 因此必须幂等（[`Login::is_active`] 为真时直接返回），否则会导致动画反复重启。
    #[cfg(feature = "hykb")]
    pub fn force(&mut self, t: f32) {
        if self.is_active() {
            return;
        }
        self.forced = true;
        self.enter(t);
    }

    /// Reveal the method-choice panel ("email vs HYKB").
    /// 中文说明：只负责置位并驱动淡入动画，面板真实可见性由 `picker_fader` 的进度决定。
    #[cfg(feature = "hykb")]
    fn show_picker(&mut self, t: f32) {
        self.picker_show = true;
        self.picker_fader.sub(t);
    }

    /// Reveal the email login/register form.
    /// 中文说明：表单的显隐由 `fader` 动画控制；`show` 字段的实际翻转在 [`Login::update`]
    /// 中随动画完成回调进行，保证动画期间仍继续渲染。
    fn show_form(&mut self, t: f32) {
        self.fader.sub(t);
    }

    /// Dismiss the method-choice panel.
    #[cfg(feature = "hykb")]
    fn dismiss_picker(&mut self, t: f32) {
        self.picker_show = false;
        self.picker_fader.back(t);
    }

    /// 关闭邮箱表单（退出登录流程），并清理所有与渠道绑定/注册相关的中间状态。
    /// 中文说明：清理是必要的——否则残留的待绑定 token 会让下一次普通登录被误判为
    /// “绑定已有账号”流程（同一个登录按钮会走不同分支）。
    pub fn dismiss(&mut self, t: f32) {
        self.show = false;
        self.fader.back(t);
        // Drop any pending claim so a later plain login isn't treated as a claim.
        #[cfg(feature = "hykb")]
        {
            self.forced = false;
            self.hykb_pending_token = None;
            self.hykb_reg_token = None;
            self.hykb_nick = None;
            self.reg_name_show = false;
            self.t_hykb_name.clear();
        }
    }

    /// 校验注册表单并发起注册请求；校验失败则返回可直接展示的错误串而不提交。
    /// 校验顺序：用户名（[`validate_username`]）→ 邮箱（[`EMAIL_REGEX`]）→ 密码长度。
    /// 先校验用户名是刻意的：它是用户最容易填错、且错误提示最具体的字段。
    /// 注意：密码长度用字符串 `len()`（字节数）判断，与常量同值；
    /// 邮箱正则只含小写字符类且未先做小写归一化，因此含大写字母的邮箱会被判为非法。
    fn register(&mut self) -> Option<String> {
        let email = self.t_reg_email.clone();
        let name = self.t_reg_name.clone();
        let pwd = self.t_reg_pwd.clone();
        if let Some(error) = validate_username(&name) {
            return Some(error);
        }
        if !EMAIL_REGEX.is_match(&email) {
            return Some(tl!("illegal-email").into_owned());
        }
        if !(8..=32).contains(&pwd.len()) {
            return Some(tl!("pwd-length-req", "min" => PWD_LEN_MIN, "max" => PWD_LEN_MAX));
        }
        self.start("register", async move {
            Client::register(&email, &name, &pwd).await?;
            Ok(None)
        });
        None
    }

    /// Kick off the native HYKB login: obtain credentials, then call `/login/hykb`.
    /// 中文说明：本函数只登记任务、立即返回，真正的异步流程在任务里跑：
    /// 先向渠道 SDK 取得凭据（用户取消授权会得到 `None`，故用 `ok_or_err` 转为错误），
    /// 再把它交给服务端换取 Phira 登录态或“需要选择注册/绑定”的短时 token。
    /// 服务端在“已绑定”时会直接返回可用的登录态，因此这里顺带拉取一次用户资料。
    #[cfg(feature = "hykb")]
    fn start_hykb_login(&mut self) {
        self.hykb_task = Some(Task::new(async move {
            // 阶段一：从渠道 SDK 获取 uid + access_token（静默失败即中止并报错）。
            let cred = obtain_hykb_credential().await?.ok_or_err()?;
            // 阶段二：用渠道凭据换取 Phira 侧结果，分流为“已绑定”或“需用户选择”。
            match Client::login_hykb(cred.uid, &cred.access_token).await? {
                HykbLoginOutcome::LoggedIn => Ok(HykbStep::LoggedIn(Box::new(Client::get_me().await?))),
                HykbLoginOutcome::NeedChoice { hykb_token } => Ok(HykbStep::NeedChoice { hykb_token, nick: cred.nick }),
            }
        }));
    }

    /// Show the "register a new account / claim an existing one" dialog after a
    /// first-time HYKB login. The chosen action is recorded in `hykb_choice`.
    /// 中文说明：之所以需要“绑定已有账号（Claim）”，是因为渠道账号与 Phira 账号是两套
    /// 体系：玩家可能早已用邮箱注册过 Phira，只是首次从渠道登录。此时若直接新建账号，
    /// 会造成一人多号与进度割裂，故提供“用已有邮箱密码把渠道账号并到老账号上”的选项。
    /// 回调把位置映射为选择并写入 `hykb_choice`（`-1` 表示点外部/关闭 → 取消）。
    #[cfg(feature = "hykb")]
    fn show_hykb_choice(&self) {
        let choice = Arc::clone(&self.hykb_choice);
        Dialog::plain(tl!("hykb-choice-title"), tl!("hykb-choice-sub"))
            .buttons(vec![tl!("hykb-choice-register").to_string(), tl!("hykb-choice-claim").to_string()])
            .listener(move |_, pos| {
                match pos {
                    // Outside click / dismiss: treat as backing out to the picker.
                    -1 => *choice.lock().unwrap() = Some(HykbChoice::Cancel),
                    0 => *choice.lock().unwrap() = Some(HykbChoice::Register),
                    1 => *choice.lock().unwrap() = Some(HykbChoice::Claim),
                    _ => {}
                }
                false
            })
            .show();
    }

    /// Reveal the in-app "choose your username" panel for a new HYKB account.
    #[cfg(feature = "hykb")]
    fn show_reg_name(&mut self, t: f32) {
        self.reg_name_show = true;
        self.reg_name_fader.sub(t);
    }

    /// Dismiss the username panel.
    #[cfg(feature = "hykb")]
    fn dismiss_reg_name(&mut self, t: f32) {
        self.reg_name_show = false;
        self.reg_name_fader.back(t);
    }

    /// Validate the username the player chose and create their HYKB-bound account.
    /// Called from the username panel's confirm button; the panel stays visible on
    /// an invalid name so it can be fixed.
    /// 中文说明：凭据用 `take()` 取出（一次性使用），取出后即从状态中移除，
    /// 避免重复提交；`Ok(Some(user))` 让成功路径与普通登录共用同一段收尾逻辑。
    #[cfg(feature = "hykb")]
    fn submit_hykb_register(&mut self, name: String) {
        if let Some(error) = validate_username(&name) {
            show_message(error).error();
            return;
        }
        let Some(token) = self.hykb_reg_token.take() else {
            return;
        };
        self.start("hykb-login", async move {
            Client::login_hykb_register(&token, &name).await?;
            Ok(Some(Client::get_me().await?))
        });
    }

    /// 处理一次触摸事件；返回 `true` 表示事件已被本面板消费（上层不应再转发给其他 UI）。
    ///
    /// 命中优先级自上而下：「选择用户名」面板 → 「登录方式选择」面板 → 邮箱表单。
    /// 任一层可见时，其下方的层不参与命中测试，避免多层重叠时误触到被遮挡的按钮。
    /// # Arguments
    /// * `touch` - 本次触摸事件（含位置与阶段）。
    /// * `t` - 当前时间（秒），用于控件动画与状态推进。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        // 阶段零：动画进行中或有请求在跑时吞掉全部触摸，避免过渡期产生错乱操作；
        // 同时这也让请求期间的“输入屏蔽”无需额外处理（加载遮罩自然生效）。
        if self.fader.transiting() || self.task.is_some() || !self.start_time.is_nan() {
            return true;
        }
        #[cfg(feature = "hykb")]
        if self.hykb_task.is_some() {
            return true;
        }
        // The "choose your username" panel for a new HYKB account.
        // 阶段一：HYKB 新账号的「选择用户名」面板——优先级最高，因为此时手上握着
        // 一次性凭据，任何误操作都可能导致流程中途丢失绑定上下文。
        #[cfg(feature = "hykb")]
        if self.reg_name_show {
            if self.reg_name_fader.transiting() {
                return true;
            }
            if !Self::reg_name_rect().contains(touch.position) && touch.phase == TouchPhase::Started {
                // Backing out returns to the register/claim choice dialog (the
                // previous level), keeping the token so the choice can be remade.
                self.dismiss_reg_name(t);
                if let Some(token) = self.hykb_reg_token.take() {
                    self.hykb_pending_token = Some(token);
                    self.show_hykb_choice();
                }
                return true;
            }
            if self.input_hykb_name.touch(touch, t) {
                request_input(
                    "hykb_reg_name",
                    InputBox::new()
                        .title(tl!("username"))
                        .prompt(tl!("hykb-reg-name-prompt", "min" => USERNAME_LEN_MIN, "max" => USERNAME_LEN_MAX))
                        .default_text(&self.t_hykb_name),
                );
                return true;
            }
            if self.btn_hykb_name_confirm.touch(touch, t) {
                if let Some(error) = validate_username(&self.t_hykb_name) {
                    show_message(error).error();
                } else {
                    self.dismiss_reg_name(t);
                    self.submit_hykb_register(self.t_hykb_name.clone());
                }
                return true;
            }
            return true;
        }
        // The method-choice panel sits on top of (and gates) the form.
        // 阶段二：登录方式选择面板。它“盖在”表单之上并充当闸门——只有从这里做出
        // 选择，下面的邮箱表单才可能出现。
        #[cfg(feature = "hykb")]
        if self.picker_show {
            if self.picker_fader.transiting() {
                return true;
            }
            if !Self::picker_rect().contains(touch.position) && touch.phase == TouchPhase::Started {
                // When login is mandatory, swallow the touch but keep the panel.
                if !self.forced {
                    self.dismiss_picker(t);
                }
                return true;
            }
            if self.btn_method_email.touch(touch, t) {
                self.dismiss_picker(t);
                self.show_form(t);
                return true;
            }
            if self.btn_method_hykb.touch(touch, t) {
                if !check_read_tos_and_policy(true, true) {
                    // Keep the picker up behind the TOS dialog: if the player
                    // denies (which never fires JUST_ACCEPTED_TOS), they simply
                    // stay on the picker rather than being stranded on a blank,
                    // forced home. The picker is dismissed once TOS is accepted.
                    self.after_accept_tos = Some(NextAction::Hykb);
                } else {
                    self.dismiss_picker(t);
                    self.start_hykb_login();
                }
                return true;
            }
            return true;
        }
        // 阶段三：邮箱表单本体。登录页与注册页共用同一块命中区域，
        // 具体处在哪一页由 `in_reg` 与切换动画共同决定（render 负责区分绘制）。
        if self.show {
            if !Ui::dialog_rect().contains(touch.position) && touch.phase == TouchPhase::Started {
                // In the HYKB claim flow, backing out of the credential form
                // returns to the register/claim choice dialog (the previous
                // level) instead of closing the login entirely. Keep the
                // pending token so the choice can be made again.
                #[cfg(feature = "hykb")]
                if self.hykb_pending_token.is_some() {
                    self.show = false;
                    self.fader.back(t);
                    self.show_hykb_choice();
                    return true;
                }
                // When login is mandatory, the flow can't be dismissed, but the
                // player may still back out of the email form to the method
                // picker (rather than being stranded on the form).
                #[cfg(feature = "hykb")]
                if self.forced {
                    self.show = false;
                    self.fader.back(t);
                    self.show_picker(t);
                    return true;
                }
                self.dismiss(t);
                return true;
            }
            if self.input_email.touch(touch, t) {
                request_input("email", InputBox::new().default_text(&self.t_email));
                return true;
            }
            if self.input_pwd.touch(touch, t) {
                request_input("pwd", InputBox::new().default_text(&self.t_pwd).mode(InputMode::Password));
                return true;
            }
            if self.input_reg_email.touch(touch, t) {
                request_input("reg_email", InputBox::new().default_text(&self.t_reg_email));
                return true;
            }
            if self.input_reg_name.touch(touch, t) {
                request_input("reg_name", InputBox::new().default_text(&self.t_reg_name));
                return true;
            }
            if self.input_reg_pwd.touch(touch, t) {
                request_input("reg_pwd", InputBox::new().default_text(&self.t_reg_pwd).mode(InputMode::Password));
                return true;
            }
            // 两个切换按钮只登记动画起始时刻，真正的页签翻转发生在 render 中的
            // 动画完成判定里，因此这里无需区分点的是哪一颗。
            if self.btn_to_reg.touch(touch, t) || self.btn_to_login.touch(touch, t) {
                self.start_time = t;
                return true;
            }
            // 注册前先过 TOS（服务条款/隐私政策）闸门：未同意则记下待续动作，
            // 待用户同意后由 update 续跑；校验放在 TOS 之后，避免用户先看到
            // 格式错误、同意条款后又要重新填一遍。
            if self.btn_reg.touch(touch, t) {
                if !check_read_tos_and_policy(true, true) {
                    self.after_accept_tos = Some(NextAction::Register);
                    return true;
                }
                if let Some(error) = self.register() {
                    show_message(error).error();
                }
                return true;
            }
            // 登录按钮的语义随状态变化：若存在待绑定的渠道凭据，则本次提交实际是
            // “用邮箱密码把渠道账号绑定到已有账号”，而不是普通登录。
            if self.btn_login.touch(touch, t) {
                // A pending HYKB claim already accepted TOS in the picker; only
                // gate a plain email login on the TOS check.
                #[cfg(feature = "hykb")]
                let pending_claim = self.hykb_pending_token.is_some();
                #[cfg(not(feature = "hykb"))]
                let pending_claim = false;
                if !pending_claim && !check_read_tos_and_policy(true, true) {
                    self.after_accept_tos = Some(NextAction::Login);
                    return true;
                }
                self.start_login();
                return true;
            }
            if self.btn_forget_pwd.touch(touch) {
                button_hit();
                let _ = open_url(&format!("{API_URL}/reset-password"));
            }
            return true;
        }
        false
    }

    /// Submit the email login form. When a HYKB token is pending (the user chose
    /// to claim an existing account), this claims it with the entered email and
    /// password instead of a plain password login.
    /// 中文说明：两条分支互斥——若手上还有待绑定的渠道凭据，则走“绑定已有账号”，
    /// 否则是普通密码登录。绑定分支仍需本地校验邮箱格式，因为服务端绑定接口同样要求
    /// 一个合法邮箱来定位目标账号。
    fn start_login(&mut self) {
        #[cfg(feature = "hykb")]
        if let Some(token) = self.hykb_pending_token.clone() {
            let email = self.t_email.clone();
            let pwd = self.t_pwd.clone();
            if !EMAIL_REGEX.is_match(&email) {
                show_message(tl!("illegal-email")).error();
                return;
            }
            // Keep the pending token: on success `dismiss` clears it, but on a
            // failed claim it must survive so backing out returns to the
            // register/claim dialog (and a retry still claims) rather than
            // dropping all the way back to the method picker.
            // 中文说明：这里刻意 `clone` 而不 `take`——绑定失败时凭据必须留到下一帧，
            // 用户才能重试或退回“注册/绑定”对话框。
            self.start("hykb-login", async move {
                Client::login_hykb_claim(&token, &email, &pwd).await?;
                Ok(Some(Client::get_me().await?))
            });
            return;
        }
        let email = self.t_email.clone();
        let pwd = self.t_pwd.clone();
        self.start_login_with(email, pwd, false);
    }

    /// 普通邮箱密码登录的公共实现。
    /// `cancel_delete_request` 为真时表示本次登录同时请求撤销该账号待处理的注销申请
    /// （用户已在确认对话框里同意），由 [`Login::update`] 在收到确认后携 `true` 重发。
    fn start_login_with(&mut self, email: String, pwd: String, cancel_delete_request: bool) {
        self.start("login", async move {
            // 阶段一：完成 Phira 账号登录，并拉取当前用户资料。
            Client::login(LoginParams::Password {
                email: &email,
                password: &pwd,
                cancel_delete_request,
            })
            .await?;
            let me = Client::get_me().await?;
            // Every email login — bound or not — must complete a native HYKB
            // login so the SDK's online anti-addiction enforcement runs (the
            // limits are tied to a signed-in HYKB account, not to any separate
            // "anti" entry point). We obtain the credential silently and tear
            // the session down if the player cancels, but we do NOT send it to
            // the server: the HYKB account is used purely for anti-addiction and
            // is not bound to the Phira account here. An unbound player may bind
            // HYKB later from the profile page; a bound account no longer has to
            // match its stored `hykb_uid` — any successful HYKB login is accepted.
            // 中文说明：这一句是“静默取得渠道凭据”，只在本地打通渠道会话，用于防沉迷
            // 判定；**不会**把凭据上传给 Phira 服务端，也不会在这里建立账号绑定关系。
            // 因此它对用户是透明的，失败/取消则整段登录失败。
            #[cfg(feature = "hykb")]
            crate::obtain_hykb_credential_silent().await?.ok_or_err()?;
            // 阶段二：返回登录态；`Some(me)` 表示本次动作确实建立了登录会话。
            Ok(Some(me))
        });
    }

    /// 每帧推进面板状态：动画完成回调、输入框回填、TOS 续跑、请求结果处理、
    /// 注销撤销确认，以及 HYKB 的分阶段流程。所有子流程都在这里收敛，`render` 只负责绘制。
    /// # Errors
    /// 保存登录后的用户数据（`save_data`）失败时向上传播，避免出现“内存里已登录、
    /// 磁盘上未落盘”的割裂状态。
    pub fn update(&mut self, t: f32) -> Result<()> {
        // 阶段一：查询各淡入淡出动画是否结束，并据此翻转对应的 `*_show` 标志。
        // 在动画结束后才翻标志，可保证过渡期间元素持续渲染、不会闪断。
        if let Some(done) = self.fader.done(t) {
            self.show = !done;
        }
        #[cfg(feature = "hykb")]
        if let Some(done) = self.picker_fader.done(t) {
            self.picker_show = !done;
        }
        #[cfg(feature = "hykb")]
        if let Some(done) = self.reg_name_fader.done(t) {
            self.reg_name_show = !done;
        }
        // 阶段二：推进全局 TOS（服务条款）任务，实际的同意流程由它执行。
        dispatch_tos_task();
        // 阶段三：把原生输入框的返回值回填到对应表单字段。
        // 用带标签的块配合 `break` 实现「命中即停」；未知 id 会原样退回输入队列，
        // 交给其他面板处理，而不是被本面板吞掉。
        if let Some((id, text)) = take_input() {
            'tmp: {
                // The HYKB register username feeds the in-app panel's input slot
                // rather than being stored into one of the email-form fields.
                #[cfg(feature = "hykb")]
                if id == "hykb_reg_name" {
                    self.t_hykb_name = text;
                    break 'tmp;
                }
                let tmp = match id.as_str() {
                    "email" => &mut self.t_email,
                    "pwd" => &mut self.t_pwd,
                    "reg_email" => &mut self.t_reg_email,
                    "reg_name" => &mut self.t_reg_name,
                    "reg_pwd" => &mut self.t_reg_pwd,
                    _ => {
                        return_input(id, text);
                        break 'tmp;
                    }
                };
                *tmp = text;
            }
        }
        // Cancelling the username InputBox simply returns to the in-app username
        // panel (still shown); consume the event so it doesn't leak to others.
        #[cfg(feature = "hykb")]
        if let Some(id) = take_input_cancelled() {
            let _ = id;
        }
        // 阶段四：若用户刚刚同意了 TOS，则续跑此前被打断的动作。
        // `fetch_and(false)` 保证该标志只被消费一次，避免同一动作被重复提交。
        if JUST_ACCEPTED_TOS.fetch_and(false, Ordering::Relaxed) {
            match self.after_accept_tos {
                Some(NextAction::Login) => {
                    self.start_login();
                }
                Some(NextAction::Register) => {
                    if let Some(error) = self.register() {
                        show_message(error).error();
                    }
                }
                #[cfg(feature = "hykb")]
                Some(NextAction::Hykb) => {
                    // The picker was kept visible through the TOS gate; drop it
                    // now that the player accepted and we're proceeding.
                    self.dismiss_picker(t);
                    self.start_hykb_login();
                }
                None => (),
            }
            self.after_accept_tos = None;
        }
        // 阶段五：处理已完成的请求，分为三条结局：报错、待注销确认、成功收尾。
        if let Some((action, task)) = &mut self.task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        // A pending account deletion request blocks the login.
                        // Prompt the player: confirming cancels the deletion by
                        // retrying the login with `cancelDeleteRequest: true`.
                        // 中文说明：只有“普通登录”会走到待注销判定（其他动作不会返回该错误码）；
                        // 用户确认撤销后，用同一份凭据携 `cancel_delete_request = true` 重试。
                        if *action == "login" && err.downcast_ref::<ErrorCode>() == Some(&ErrorCode::PENDING_DELETE_REQUEST) {
                            self.pending_delete_retry = Some((self.t_email.clone(), self.t_pwd.clone()));
                            self.pending_delete_confirm.store(false, Ordering::SeqCst);
                            confirm_dialog(
                                tl!("pending-delete-title").into_owned(),
                                tl!("pending-delete-message").into_owned(),
                                Arc::clone(&self.pending_delete_confirm),
                            );
                        } else {
                            show_error(err.context(tl!("action-failed", "action" => *action)));
                        }
                    }
                    Ok(user) => {
                        // 任何一次成功都清掉注销重试缓存，避免之后误用旧凭据重发。
                        self.pending_delete_retry = None;
                        // 登录成功后的收尾：把用户写入全局数据并落盘，同时通知
                        // `UserManager` 拉取该用户的数据。HomePage 通过读取全局 `me`
                        // 感知登录态，因此无需显式回调；鉴权令牌由 `Client::login`
                        // 在内部完成写入。
                        if let Some(user) = user {
                            UserManager::request(user.id);
                            get_data_mut().me = Some(user);
                            save_data()?;
                        }
                        // 无论成功与否都清空密码输入，避免明文长时间驻留内存与界面。
                        self.t_pwd.clear();
                        show_message(tl!("action-success", "action" => *action)).ok();
                        // 注册成功不产生登录态，而是提示“验证邮件已发送”，并清空注册
                        // 表单、切回登录页，避免用户对着已提交的内容重复操作。
                        if *action == "register" {
                            Dialog::simple(tl!("email-sent")).show();
                            self.t_reg_email.clear();
                            self.t_reg_name.clear();
                            self.t_reg_pwd.clear();
                            self.start_time = t;
                        }
                        // 登录类动作成功后关闭面板（同时清掉待绑定凭据等中间状态）；
                        // 注册动作则保持面板打开，让用户看到邮件提示并返回登录页。
                        if *action == "login" || *action == "hykb-login" {
                            self.dismiss(t);
                        }
                    }
                }
                self.task = None;
            }
        }
        // 阶段六：处理“撤销注销”确认框的结果。`swap(false)` 保证一次确认只重试一次；
        // 仅当确实缓存了待重试凭据时才重发，避免空提交。
        if self.pending_delete_confirm.swap(false, Ordering::Relaxed) {
            if let Some((email, pwd)) = self.pending_delete_retry.take() {
                self.start_login_with(email, pwd, true);
            }
        }
        // 阶段七：推进 HYKB 的分阶段流程（凭据校验 → 注册/绑定选择 → 提交注册）。
        #[cfg(feature = "hykb")]
        self.update_hykb(t)?;
        Ok(())
    }

    /// Drive the HYKB login phases: the initial verify task, the register/claim
    /// choice dialog, and the follow-up that resolves to a logged-in user.
    /// 中文说明：这是渠道登录的轮询驱动点，每帧在 [`Login::update`] 末尾调用一次。
    /// 它把“任务完成”与“对话框选择”两条异步输入收敛到同一处，因此写用户、落盘、
    /// 关面板这套成功收尾只写一遍。
    /// # Errors
    /// 与 [`Login::update`] 一致——保存用户数据失败时向上传播。
    #[cfg(feature = "hykb")]
    fn update_hykb(&mut self, t: f32) -> Result<()> {
        // 阶段一：收取第一阶段的校验结果，并按结果分流。
        if let Some(task) = &mut self.hykb_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => show_error(err.context(tl!("action-failed", "action" => "hykb-login"))),
                    Ok(HykbStep::LoggedIn(user)) => {
                        UserManager::request(user.id);
                        get_data_mut().me = Some(*user);
                        save_data()?;
                        show_message(tl!("action-success", "action" => "hykb-login")).ok();
                        self.dismiss(t);
                    }
                    Ok(HykbStep::NeedChoice { hykb_token, nick }) => {
                        self.hykb_pending_token = Some(hykb_token);
                        self.hykb_nick = Some(nick);
                        self.show_hykb_choice();
                    }
                }
                self.hykb_task = None;
            }
        }
        // The choice dialog records its result here; pick it up and start the
        // matching follow-up request, reusing `task` so the success path is shared.
        // 阶段二：读取对话框写入的选择并启动对应的后续请求。用 `take()` 清空槽位，
        // 保证同一次选择只被处理一次。
        let choice = self.hykb_choice.lock().unwrap().take();
        if let Some(choice) = choice {
            match choice {
                // 注册新账号：把凭据从「待选择」转移到「待命名」，并进入自绘的用户名面板。
                HykbChoice::Register => {
                    if let Some(token) = self.hykb_pending_token.take() {
                        // Let the player choose their own username before creating
                        // the account via the in-app panel (prefilled with their
                        // HYKB nickname). The token is kept until the name is submitted.
                        self.hykb_reg_token = Some(token);
                        self.t_hykb_name = self.hykb_nick.clone().unwrap_or_default();
                        self.show_reg_name(t);
                    }
                }
                // 绑定已有账号：复用邮箱表单收集“已有账号”的邮箱密码；凭据仍留在
                // pending 槽中，登录按钮据此改走 `login_hykb_claim`（见 `start_login`）。
                HykbChoice::Claim => {
                    // Keep the pending token; reveal the email form so the user can
                    // enter the credentials of the account they want to claim. The
                    // login button submits the claim while the token is set.
                    if self.hykb_pending_token.is_some() {
                        self.show_form(t);
                    }
                }
                // 取消：丢弃渠道身份（含昵称），退回登录方式选择面板，不残留半成品状态。
                HykbChoice::Cancel => {
                    // Backed out of register/claim: drop the pending identity and
                    // return to the method-choice panel.
                    self.hykb_pending_token = None;
                    self.hykb_nick = None;
                    self.show_picker(t);
                }
            }
        }
        // 阶段三：本轮处理结束；后续进度靠下一帧轮询继续推进（无事件唤醒机制）。
        Ok(())
    }

    /// 绘制登录面板。整体顺序为「先邮箱表单（含遮罩与页面切换动画）→ 加载遮罩 →
    /// 渠道相关的两个子面板 → 渠道请求的加载遮罩」，保证子面板与遮罩始终压在最上层。
    pub fn render(&mut self, ui: &mut Ui, t: f32) {
        // 阶段一：邮箱表单。`fader.reset()` 必须在每帧开头调用，避免上一帧的动画偏移
        // 被重复累加；遮罩透明度随动画进度变化，从而实现淡入/淡出。
        self.fader.reset();
        if self.show || self.fader.transiting() {
            let p = if self.show { 1. } else { -self.fader.progress(t) };
            ui.fill_rect(ui.screen_rect(), semi_black(p * 0.7));
            self.fader.for_sub(|f| {
                f.render(ui, t, |ui| {
                    let mut wr = Ui::dialog_rect();
                    wr.y -= 0.03;
                    wr.h += 0.06;
                    ui.fill_path(&wr.rounded(0.01), ui.background());
                    ui.scissor(wr, |ui| {
                        // 阶段二：计算登录页/注册页的纵向位移。无切换动画时按 `in_reg`
                        // 直接对齐到目标页；动画进行中则用缓动曲线插值，且在进度达 1 时
                        // 翻转 `in_reg` 并复位 `start_time`（标志动画结束）。
                        let p = (if self.start_time.is_nan() {
                            if self.in_reg {
                                0.
                            } else {
                                -1.
                            }
                        } else {
                            let p = ((t - self.start_time) / Self::TIME).clamp(0., 1.);
                            let p = 1. - (1. - p).powi(3);
                            let res = if self.in_reg { -p } else { p - 1. };
                            if p >= 1. {
                                self.in_reg = !self.in_reg;
                                self.start_time = f32::NAN;
                            }
                            res
                        }) * wr.h;
                        ui.dy(p);

                        // 阶段三：先绘制注册页（邮箱/用户名/密码三个输入槽 + 两个底部按钮），
                        // 再整体下移一个面板高度绘制登录页；`scissor` 负责裁掉越界部分，
                        // 于是同一套绘制代码就完成了两页的滑动切换。
                        let r = ui.text(tl!("register")).pos(wr.x + 0.045, wr.y + 0.037).size(1.1).draw_using(&BOLD_FONT);
                        let pad = 0.035;
                        let mut r = Rect::new(wr.x + pad, r.bottom() + 0.05, wr.w - pad * 2., 0.1);
                        self.input_reg_email.render_input(ui, r, t, &self.t_reg_email, tl!("email"), 0.62);
                        r.y += r.h + 0.02;
                        self.input_reg_name.render_input(ui, r, t, &self.t_reg_name, tl!("username"), 0.62);
                        r.y += r.h + 0.02;
                        self.input_reg_pwd
                            .render_input(ui, r, t, "*".repeat(self.t_reg_pwd.len()), tl!("password"), 0.62);
                        let h = 0.09;
                        let pad = 0.05;
                        let mut r = Rect::new(wr.x + pad, wr.bottom() - h - 0.04, (wr.w - pad) / 2. - pad, h);
                        self.btn_to_login.render_text(ui, r, t, tl!("back-login"), 0.66, false);
                        r.x += r.w + pad;
                        self.btn_reg.render_text(ui, r, t, tl!("register"), 0.66, false);

                        ui.dy(wr.h);
                        let r = ui.text(tl!("login")).pos(wr.x + 0.045, wr.y + 0.037).size(1.1).draw_using(&BOLD_FONT);
                        let r = ui
                            .text(tl!("login-sub"))
                            .pos(r.x + 0.006, r.bottom() + 0.032)
                            .size(0.4)
                            .color(semi_white(0.6))
                            .max_width(wr.w - 0.05)
                            .multiline()
                            .draw();
                        let pad = 0.037;
                        let mut r = Rect::new(wr.x + pad, r.bottom() + 0.06, wr.w - pad * 2., 0.1);
                        self.input_email.render_input(ui, r, t, &self.t_email, tl!("email"), 0.62);
                        r.y += r.h + 0.04;
                        self.input_pwd.render_input(ui, r, t, "*".repeat(self.t_pwd.len()), tl!("password"), 0.62);

                        let r = ui
                            .text(tl!("forget-password"))
                            .pos(r.right() - 0.02, r.y + r.h + 0.02)
                            .anchor(1., 0.)
                            .size(0.4)
                            .color(semi_white(0.6))
                            .draw();
                        self.btn_forget_pwd.set(ui, r.feather(0.02));

                        let h = 0.09;
                        let pad = 0.05;
                        let mut r = Rect::new(wr.x + pad, wr.bottom() - h - 0.04, (wr.w - pad) / 2. - pad, h);
                        self.btn_to_reg.render_text(ui, r, t, tl!("register"), 0.66, false);
                        r.x += r.w + pad;
                        self.btn_login.render_text(ui, r, t, tl!("login"), 0.66, false);
                    });
                });
            });
        }
        // 阶段四：邮箱表单请求进行中时的加载动画，同时也是“输入已被拦截”的视觉反馈。
        if self.task.is_some() {
            ui.full_loading_simple(t);
        }
        // 阶段五：绘制渠道相关的两个子面板（登录方式选择 / 选择用户名）。
        #[cfg(feature = "hykb")]
        self.render_picker(ui, t);
        #[cfg(feature = "hykb")]
        self.render_reg_name(ui, t);
        // 阶段六：渠道第一阶段（凭据校验）请求进行中的加载遮罩，与 `task` 的遮罩分开，
        // 因为两者对应互斥的两条流程。
        #[cfg(feature = "hykb")]
        if self.hykb_task.is_some() {
            ui.full_loading_simple(t);
        }
    }

    /// The bounding rect of the method-choice panel.
    /// 中文说明：以屏幕中心为原点的对称矩形（归一化坐标），宽 0.8、高 0.5；
    /// 命中测试与绘制共用同一个来源，避免两者尺寸不一致。
    #[cfg(feature = "hykb")]
    fn picker_rect() -> Rect {
        let hw = 0.4;
        let hh = 0.25;
        Rect::new(-hw, -hh, hw * 2., hh * 2.)
    }

    /// Render the method-choice panel: a title and two vertical, styled buttons
    /// (email login and the green HYKB login with its logo).
    /// 中文说明：按钮顺序刻意把 HYKB 放在上方并配“推荐”角标、品牌绿与 logo，
    /// 用于引导用户选择渠道登录（渠道侧通常有推广与防沉迷合规诉求）；
    /// 邮箱登录作为通用兜底放在下方。
    #[cfg(feature = "hykb")]
    fn render_picker(&mut self, ui: &mut Ui, t: f32) {
        // 未展示且无过渡动画时直接跳过，避免空转绘制；`reset` 同前，防止偏移累加。
        if !self.picker_show && !self.picker_fader.transiting() {
            return;
        }
        self.picker_fader.reset();
        let p = if self.picker_show { 1. } else { -self.picker_fader.progress(t) };
        ui.fill_rect(ui.screen_rect(), semi_black(p * 0.7));
        self.picker_fader.for_sub(|f| {
            f.render(ui, t, |ui| {
                let wr = Self::picker_rect();
                ui.fill_path(&wr.rounded(0.02), ui.background());

                let pad = 0.045;
                ui.text(tl!("login-method-title"))
                    .pos(wr.x + pad, wr.y + 0.037)
                    .size(1.1)
                    .draw_using(&BOLD_FONT);

                // 阶段：自下而上布局两颗等宽按钮——先算 HYKB 的基准位置（置于底部），
                // 邮箱按钮再叠到它上方；两者共用 `bh`/`gap`，间距天然一致。
                let bh = 0.13;
                let gap = 0.028;
                let bw = wr.w - pad * 2.;
                // HYKB login — brand green with the popcorn logo.
                let r = Rect::new(wr.x + pad, wr.bottom() - bh * 2. - gap - 0.05, bw, bh);
                let green = Color::from_rgba(0x5f, 0xb8, 0x78, 255);
                self.btn_method_hykb.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, green);
                    let ir = Rect::new(r.x + 0.03, r.center().y - 0.045, 0.09, 0.09);
                    ui.fill_rect(ir, (*self.icons.hykb, ir, ScaleType::Fit));
                    ui.text(tl!("login-method-hykb"))
                        .pos(ir.right() + 0.03, r.center().y - 0.016)
                        .anchor(0., 0.5)
                        .no_baseline()
                        .size(0.6)
                        .color(WHITE)
                        .draw();
                    ui.text(tl!("login-method-recommended"))
                        .pos(ir.right() + 0.03, r.center().y + 0.024)
                        .anchor(0., 0.5)
                        .no_baseline()
                        .size(0.45)
                        .color(Color::from_hex_rgb(0xffc107))
                        .draw();
                });
                // Email login — neutral dark with the envelope icon.
                let r = Rect::new(wr.x + pad, r.bottom() + gap, bw, bh);
                self.btn_method_email.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.4));
                    let ir = Rect::new(r.x + 0.03, r.center().y - 0.04, 0.08, 0.08);
                    ui.fill_rect(ir, (*self.icons.msg, ir, ScaleType::Fit, semi_white(0.9)));
                    ui.text(tl!("login-method-email"))
                        .pos(ir.right() + 0.035, r.center().y)
                        .anchor(0., 0.5)
                        .no_baseline()
                        .size(0.6)
                        .color(WHITE)
                        .draw();
                });
            });
        });
    }

    /// The bounding rect of the "choose your username" panel.
    /// 中文说明：同样以屏幕中心为原点，宽 0.8、高 0.48；比选择面板略矮，
    /// 因为其内容只有标题、提示、输入槽与一个确认按钮。
    #[cfg(feature = "hykb")]
    fn reg_name_rect() -> Rect {
        let hw = 0.4;
        let hh = 0.24;
        Rect::new(-hw, -hh, hw * 2., hh * 2.)
    }

    /// Render the "choose your username" panel: a title, a hint line, a tappable
    /// input slot (which opens the native InputBox) and a confirm button.
    /// 中文说明：提示行会显式写出长度规则（`min`/`max` 取自与 [`validate_username`]
    /// 同一组常量），做到“规则先行、失败可改”；非法用户名由确认按钮侧拦截并报错，
    /// 面板保持可见，用户无需重走整个流程。
    #[cfg(feature = "hykb")]
    fn render_reg_name(&mut self, ui: &mut Ui, t: f32) {
        // 未展示且无过渡动画时跳过绘制。
        if !self.reg_name_show && !self.reg_name_fader.transiting() {
            return;
        }
        self.reg_name_fader.reset();
        let p = if self.reg_name_show { 1. } else { -self.reg_name_fader.progress(t) };
        ui.fill_rect(ui.screen_rect(), semi_black(p * 0.7));
        self.reg_name_fader.for_sub(|f| {
            f.render(ui, t, |ui| {
                let wr = Self::reg_name_rect();
                ui.fill_path(&wr.rounded(0.02), ui.background());

                let pad = 0.045;
                let r = ui.text(tl!("username")).pos(wr.x + pad, wr.y + 0.037).size(1.1).draw_using(&BOLD_FONT);
                let r = ui
                    .text(tl!("hykb-reg-name-prompt", "min" => USERNAME_LEN_MIN, "max" => USERNAME_LEN_MAX))
                    .pos(wr.x + pad + 0.006, r.bottom() + 0.028)
                    .size(0.4)
                    .color(semi_white(0.6))
                    .max_width(wr.w - pad * 2.)
                    .multiline()
                    .draw();

                let r = Rect::new(wr.x + pad, r.bottom() + 0.04, wr.w - pad * 2., 0.1);
                self.input_hykb_name.render_input(ui, r, t, &self.t_hykb_name, tl!("username"), 0.62);

                let h = 0.09;
                let bpad = 0.05;
                let r = Rect::new(wr.x + bpad, wr.bottom() - h - 0.04, wr.w - bpad * 2., h);
                self.btn_hykb_name_confirm.render_text(ui, r, t, tl!("hykb-reg-name-confirm"), 0.66, true);
            });
        });
    }
}
