//! 首页（主菜单）页面。
//!
//! 应用启动后默认进入的 [`Page`]，承担三类职责：
//! - **导航分发**：把「开始/活动/资源包/消息/设置/个人资料」六个入口路由到对应页面或场景；
//! - **氛围展示**：以随机本地谱面曲绘轮播 +（可选）角色立绘作为两层背景；
//! - **启动期任务**：检查更新、热更新加粗字体、恢复会话、探测未读私信等异步副作用。
//!
//! 纯渲染逻辑分两层且互斥：`char_screen_p` 为 0 时绘制主菜单（`render_not_char`），
//! 为 1 时绘制角色介绍面板；中间值用于横向滑动切换，故两层用 `ui.alpha` 交叉淡入。

prpr_l10n::tl_file!("home");

use super::{
    load_font_with_cksum, set_bold_font, EventPage, LibraryPage, MessagePage, NextPage, Page, ResPackPage, SFader, SettingsPage, SharedState,
    BOLD_FONT_CKSUM,
};
use crate::{
    anim::Anim,
    client::{recv_raw, Character, Client, ErrorCode, LoginParams, User, UserManager},
    dir, get_data, get_data_mut,
    icons::Icons,
    login::Login,
    save_data,
    scene::{check_read_tos_and_policy, ProfileScene, JUST_LOADED_TOS},
    sync_data,
    threed::ThreeD,
};
use ::rand::{random, thread_rng, Rng};
use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use image::DynamicImage;
use macroquad::prelude::*;
use prpr::{
    core::BOLD_FONT,
    ext::{open_url, screen_aspect, semi_black, semi_white, RectExt, SafeTexture, ScaleType},
    info::ChartInfo,
    scene::{show_error, NextScene},
    task::Task,
    ui::{button_hit_large, clip_rounded_rect, ClipType, DRectButton, Dialog, FontArc, RectButton, Scroll, Ui},
};
use prpr_l10n::LANG_IDENTS;
use reqwest::StatusCode;
use serde::Deserialize;
use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicI8, Ordering},
        Arc,
    },
};
use tap::Tap;
use tracing::{info, warn};

// 曲绘轮播的停留时长（秒）：上一张展示满这么久后才开始加载下一张，避免频繁读盘。
const BOARD_SWITCH_TIME: f32 = 4.;
// 曲绘切换的过渡时长（秒）：新旧两张图沿垂直方向「推挤」交换，取大于 1 秒是为了让运镜从容。
const BOARD_TRANSIT_TIME: f32 = 1.2;

// 加粗字体热更新任务的类型别名：成功时返回 `Some((字体, 校验和))`，`None` 表示服务端返回「未修改」。
type BoldFontUpdateTask = Task<Result<Option<(FontArc, String)>>>;

/// `/check-update` 接口返回的新版本信息。
///
/// 仅在 `version` 比玩家「忽略的版本」更新时才弹更新对话框，见 [`HomePage::update`]。
#[derive(Deserialize)]
struct Version {
    /// 服务端发布的最新语义化版本号，用于忽略比较与展示。
    version: semver::Version,
    /// 该版本的发布日期，仅用于在更新提示里展示。
    date: NaiveDate,
    /// 更新说明（多行文本），直接展示给玩家。
    description: String,
    /// 下载/公告链接，点「前往更新」时用系统浏览器打开。
    url: String,
}

/// 首页（主菜单）页面状态。
///
/// 一个实例贯穿整个进程生命周期：从首页进入子页面时通过 [`NextPage::Overlay`] 压栈，
/// 子页面弹出后回到同一个实例，因此这里保存的轮播/立绘/登录面板状态会被保留。
pub struct HomePage {
    /// 图标资源集的强引用，供本页按钮与进入子页面时共享。
    icons: Arc<Icons>,

    /// 「开始」按钮：进入曲库（[`LibraryPage`]），尺寸与其它按钮不同故样式单独配置。
    btn_play: DRectButton,
    /// 「活动」按钮：进入活动页（[`EventPage`]），未登录时会先唤起登录面板。
    btn_event: DRectButton,
    /// 「资源包」按钮：进入资源包管理页（[`ResPackPage`]）。
    btn_respack: DRectButton,
    /// 「消息」按钮：进入私信页（[`MessagePage`]），右上角红点由 `has_new` 决定。
    btn_msg: DRectButton,
    /// 「设置」按钮：进入设置页（[`SettingsPage`]）。
    btn_settings: DRectButton,
    /// 右上角用户头像按钮：已登录跳个人资料场景，未登录唤起登录面板。
    btn_user: DRectButton,

    /// 本帧要跳转的目标页面；在 [`Page::next_page`] 中被取走，取走后恢复为 `None`。
    next_page: Option<NextPage>,

    /// 登录面板（含 HYKB 渠道的选择/注册子流程），由 `login.rs` 统一实现。
    login: Login,
    /// 启动时会话恢复任务；HYKB 构建下它还会静默验证 SDK 登录，期间首页保持不可交互。
    update_task: Option<Task<Result<User>>>,
    /// Outcome of the session-restore pending-delete dialog: 0 = none,
    /// 1 = confirm (cancel the deletion and retry), 2 = cancel (log out).
    ///
    /// 用原子量而非普通字段，是因为对话框回调与页面更新不在同一借用期，
    /// 回调只负责写入选择，下一帧 `update` 再消费（`swap(0)`）。
    pending_delete_choice: Arc<AtomicI8>,

    /// 从子页面返回时是否需要播放淡入动效（仅在跳转过子页面/场景后置位）。
    need_back: bool,
    /// 首页自身的场景/页面过渡控制器，同时用于进入个人资料场景。
    sf: SFader,

    /// 曲绘背景的加载任务；`None` 表示当前没有在加载。
    board_task: Option<Task<Result<Option<DynamicImage>>>>,
    /// 上一张曲绘完成（或开始）切换的时间戳，用于计算停留与过渡进度。
    board_last_time: f32,
    /// 上一张曲绘对应的本地谱面 `local_path`，用于避免连续抽到同一张。
    board_last: Option<String>,
    /// 正在退场的曲绘纹理（过渡结束后置 `None`）。
    board_tex_last: Option<SafeTexture>,
    /// 当前展示的曲绘纹理。
    board_tex: Option<SafeTexture>,
    /// 过渡方向：随机决定新旧曲绘谁从上、谁从下推入，令切换不显单调。
    board_dir: bool,

    /// 未读私信探测任务。
    has_new_task: Option<Task<Result<bool>>>,
    /// 缓存「是否有未读私信」的结果，为真时在消息按钮上画红点。
    has_new: bool,

    /// 版本检查任务（结果可能为 `None`，表示无更新）。
    check_update_task: Option<Task<Result<Option<Version>>>>,
    /// 加粗字体热更新任务，成功后调用 [`set_bold_font`] 替换全局字体。
    check_bold_font_update_task: Option<BoldFontUpdateTask>,

    /// 「开始」按钮的伪 3D 倾斜状态（鼠标/触摸位置驱动）。
    btn_play_3d: ThreeD,
    /// 其余按钮所在整块区域的伪 3D 倾斜状态，锚点/角度与「开始」按钮不同以形成层次。
    btn_other_3d: ThreeD,

    /// 当前展示的角色（看板娘）数据，来自本地缓存，联网后再异步刷新。
    character: Character,
    /// 角色立绘的淡入进度（0→1），加载完成后 0.5 秒内渐显。
    char_appear_p: Anim<f32>,
    /// 上次加载立绘所用的键，用于避免同一角色重复加载。
    char_last_illu: Option<String>,
    /// 上次拉取角色数据时的用户 id（未登录记 -1），用于在切换账号时重新拉取。
    char_last_user_id: Option<i32>,
    /// 联网刷新角色数据的任务。
    char_fetch_task: Option<Task<Result<Character>>>,
    /// 当前角色立绘纹理。
    char_illu: Option<SafeTexture>,
    /// 立绘图片的加载任务。
    char_illu_task: Option<Task<Result<DynamicImage>>>,
    // progress of character screen
    /// 角色介绍面板的出场进度：0 = 主菜单，1 = 全屏角色介绍，中间值为滑动切换中。
    char_screen_p: Anim<f32>,
    /// 点击角色立绘区域的命中框，用于触发/关闭角色介绍。
    char_btn: RectButton,
    /// 上次打开角色介绍的时刻，用于让文案随出场进度做延迟位移。
    char_text_start: f32,
    /// 角色英文名的自适应字号缓存（按宽度试算后缓存，避免每帧循环）。
    char_cached_size: f32,
    /// 角色介绍文本的滚动容器。
    char_scroll: Scroll,
    /// 角色介绍里「更换角色」按钮，点击打开网页版账号设置。
    char_edit_btn: RectButton,

    /// HYKB 渠道合规要求展示的备案号按钮，点击跳转工信部备案查询页。
    #[cfg(feature = "hykb")]
    beian_btn: RectButton,
}

// 首页的构造与内部数据加载。构造过程会发起若干启动期异步任务，
// 但除 HYKB 会话恢复外都不阻塞页面出现。
impl HomePage {
    /// 构造首页并启动启动期异步任务。
    ///
    /// # Arguments
    /// * `icons` - 全局图标资源集。
    ///
    /// # Errors
    /// 初始化登录面板等资源加载失败时返回错误。
    pub async fn new(icons: Arc<Icons>) -> Result<Self> {
        // 阶段 1：决定是否恢复会话。离线模式不联网；已登录则用 refresh token
        // 静默换回 access token，并顺带取回最新用户资料。
        let update_task = if get_data().config.offline_mode {
            None
        } else if let Some(u) = &get_data().me {
            UserManager::request(u.id);
            Some(Task::new(async {
                Client::login(LoginParams::RefreshToken {
                    token: &get_data().tokens.as_ref().unwrap().1,
                    cancel_delete_request: false,
                })
                .await?;
                let me = Client::get_me().await?;
                // On HYKB builds a restored session still requires anti-addiction
                // coverage, which is driven by a signed-in native HYKB account.
                // Restore that session silently (no account picker) and tear the
                // in-game session down if the player cancels (`ok_or_err`). The
                // credential is used only for the SDK's online anti-addiction —
                // it is not verified against the restored account, so any
                // successful HYKB login is accepted whether or not the Phira
                // account is bound.
                #[cfg(feature = "hykb")]
                crate::obtain_hykb_credential_silent().await?.ok_or_err()?;
                Ok(me)
            }))
        } else {
            None
        };

        // 阶段 2：读取本地 "flavor"（发行渠道标识），随版本检查一并上报，
        // 使服务端能按渠道下发不同的更新策略；缺失时记为 "none"。
        let flavor = match load_file("flavor").await.map(String::from_utf8) {
            Ok(Ok(flavor)) => flavor.trim().to_owned(),
            _ => "none".to_owned(),
        };

        // 阶段 3：装配页面状态。按钮的 delta/elevation/radius 是按键反馈强度与
        // 阴影高度的微调参数；「开始」按钮体量更大因而带轻微下沉（负 delta）。
        let mut res = Self {
            icons: Arc::clone(&icons),

            btn_play: DRectButton::new().with_delta(-0.01).no_sound(),
            btn_event: DRectButton::new().with_elevation(0.002).no_sound(),
            btn_respack: DRectButton::new().with_elevation(0.002).no_sound(),
            btn_msg: DRectButton::new().with_radius(0.008).with_delta(-0.003).with_elevation(0.002),
            btn_settings: DRectButton::new().with_radius(0.008).with_delta(-0.003).with_elevation(0.002),
            btn_user: DRectButton::new().with_delta(-0.003),

            next_page: None,

            login: Login::new(icons),
            update_task,
            pending_delete_choice: Arc::new(AtomicI8::new(0)),

            need_back: false,
            sf: SFader::new(),

            board_task: None,
            board_last_time: f32::NEG_INFINITY,
            board_last: None,
            board_tex_last: None,
            board_tex: None,
            board_dir: false,

            has_new_task: None,
            has_new: false,

            check_update_task: Some(Task::new(async move {
                Ok(recv_raw(Client::get("/check-update").query(&[("version", env!("CARGO_PKG_VERSION")), ("flavor", &flavor)]))
                    .await?
                    .json()
                    .await?)
            })),
            check_bold_font_update_task: {
                let cksum = BOLD_FONT_CKSUM.with(|it| it.borrow().clone());
                Some(Task::new(async move {
                    let resp = Client::get("/font-bold")
                        .query(&[("cksum", cksum)])
                        .query(&[("new_bold_font", "true")])
                        .send()
                        .await?;
                    if resp.status() == StatusCode::NOT_MODIFIED {
                        info!("bold font not modified");
                        return Ok(None);
                    }
                    if !resp.status().is_success() {
                        let status = resp.status().as_str().to_owned();
                        let text = resp.text().await.context("failed to receive text")?;
                        if let Ok(what) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(detail) = what["error"].as_str() {
                                bail!("request failed ({status}): {detail}");
                            }
                        }
                        bail!("request failed ({status}): {text}");
                    }
                    info!("downloading new bold font");
                    let bytes = resp.bytes().await?;
                    std::fs::write(dir::bold_font_path()?, &bytes).context("failed to save font")?;
                    Ok(Some(load_font_with_cksum(bytes.to_vec())?))
                }))
            },

            btn_play_3d: ThreeD::new(),
            // 其余按钮块的倾斜锚点固定在右上方（0.2, -0.2），倾角更大（0.14），
            // 与「开始」按钮的默认倾斜叠加出更有纵深的层次感；`sync` 让动画立即从锚点开始。
            btn_other_3d: ThreeD::new().tap_mut(|it| {
                it.anchor = vec2(0.2, -0.2);
                it.angle = 0.14;
                it.sync();
            }),

            character: get_data().character.clone().unwrap_or_default(),
            char_appear_p: Anim::new(0.),
            char_last_illu: None,
            char_last_user_id: None,
            char_fetch_task: None,
            char_illu: None,
            char_illu_task: None,
            char_screen_p: Anim::new(0.),
            char_btn: RectButton::new(),
            char_text_start: 0.,
            char_cached_size: 0.,
            char_scroll: Scroll::new().use_clip(ClipType::Clip),
            char_edit_btn: RectButton::new(),

            #[cfg(feature = "hykb")]
            beian_btn: RectButton::new(),
        };
        // 阶段 4：预加载本地缓存的角色立绘，使首页出现时背景尽量不闪空。
        res.load_char_illu();

        Ok(res)
    }
}

// 首页的运行时辅助逻辑（立绘加载、未读探测）。
impl HomePage {
    /// 按当前角色加载立绘；若立绘键未变化则直接返回。
    ///
    /// 立绘来源有二：`illust == "@"` 表示内置角色（按 id 从资源目录读取，仅 `closed` 构建可用），
    /// 否则视为远程 URL。请求会记录到 `char_illu_task`，由 `update` 收尾。
    fn load_char_illu(&mut self) {
        let key = if self.character.illust == "@" {
            format!("@{}", self.character.id)
        } else {
            self.character.illust.clone()
        };
        if self.char_last_illu.as_ref() == Some(&key) {
            return;
        }
        self.char_last_illu = Some(key);

        self.char_appear_p.set(0.);

        // 内置角色（`closed` 构建）从本地资源读取按 id 命名的 .char 文件；
        // 其余情况按 URL 下载，二者最终都交给 `image` 解码。
        #[cfg(closed)]
        if self.character.illust == "@" {
            let id = self.character.id.clone();
            self.char_illu_task =
                Some(Task::new(
                    async move { Ok(image::load_from_memory(&crate::inner::resolve_data(load_file(&format!("res/{id}.char")).await?))?) },
                ));
        } else {
            let file = crate::page::File {
                url: self.character.illust.clone(),
            };
            self.char_illu_task =
                Some(Task::new(async move { Ok(image::load_from_memory(&crate::inner::resolve_data(file.fetch().await?.to_vec()))?) }));
        }
    }

    /// 向服务端查询是否存在未读私信，结果缓存到 `has_new`。
    ///
    /// 离线/未登录时直接清零，避免无意义的网络请求；查询以本地记录的最后检查时间为增量基准。
    fn fetch_has_new(&mut self) {
        if get_data().config.offline_mode || get_data().me.is_none() || get_data().tokens.is_none() {
            self.has_new_task = None;
            self.has_new = false;
            return;
        }
        let time = get_data().message_check_time.unwrap_or_default();
        self.has_new_task = Some(Task::new(async move {
            #[derive(Deserialize)]
            struct Resp {
                has: bool,
            }
            let resp: Resp = recv_raw(Client::get("/message/has_new").query(&[("checked", time)]))
                .await?
                .json()
                .await?;
            Ok(resp.has)
        }));
    }

    /// 绘制主菜单本体（不含角色立绘层与登录面板）。
    ///
    /// 布局以 0.83 宽度为列宽：上方是大幅「开始」按钮，下方一行是活动/资源包按钮
    /// 与右列的消息/设置小图标。所有坐标使用相对屏幕中心的归一化单位，
    /// 由 `ui` 的变换映射到实际像素。
    fn render_not_char(&mut self, ui: &mut Ui, s: &mut SharedState) {
        let t = s.t;

        let pad = 0.04;
        // 阶段 1：「开始」按钮。先取伪 3D 矩阵，再在 `with_gl` 建立的透视空间内绘制，
        // 使按钮随指针位置产生倾斜。
        // play button
        let r = Rect::new(0., -0.33, 0.83, 0.45);
        let mat = self.btn_play_3d.now(ui, r, t);
        let top = ui.with_gl(mat, |ui| {
            s.render_fader(ui, |ui| {
                let top = r.bottom() + 0.02;
                let rad = self.btn_play.config.radius;
                self.btn_play.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.4));
                    // 阶段 1a：把当前曲绘作为按钮底图。切换时用 `BOARD_TRANSIT_TIME`
                    // 归一的进度 `p`：新旧两张图沿垂直方向上下推挤；`p >= 1` 表示过渡结束，
                    // 直接画当前图并丢弃旧图。下方遮罩渐变压暗以突出按钮文字。
                    if let Some(cur) = &self.board_tex {
                        let p = (t - self.board_last_time) / BOARD_TRANSIT_TIME;
                        if p > 1. {
                            self.board_tex_last = None;
                            ui.fill_path(&path, (**cur, r));
                        } else if let Some(last) = &self.board_tex_last {
                            let (cur, last) = if self.board_dir { (last, cur) } else { (cur, last) };
                            let p = 1. - (1. - p).powi(3);
                            let p = if self.board_dir { 1. - p } else { p };
                            clip_rounded_rect(ui, r, rad, |ui| {
                                let mut nr = r;
                                nr.h = r.h * (1. - p);
                                ui.fill_rect(nr, (**last, nr));

                                nr.h = r.h * p;
                                nr.y = r.bottom() - nr.h;
                                ui.fill_rect(nr, (**cur, nr));
                            });
                        } else {
                            ui.fill_path(&path, (**cur, r, ScaleType::CropCenter, semi_white(p)));
                        }
                    }
                    ui.fill_path(&path, (semi_black(0.7), (r.x, r.y), Color::default(), (r.x + 0.6, r.y)));
                    ui.text(tl!("play")).pos(r.x + pad, r.y + pad).draw();
                    let r = Rect::new(r.x + 0.02, r.bottom() - 0.18, 0.17, 0.17);
                    ui.fill_rect(r, (*self.icons.play, r, ScaleType::Fit, semi_white(0.6)));
                });
                top + 0.03
            })
        });

        // 阶段 2：局部闭包，绘制「文字 + 图标」样式的次级按钮（活动/资源包共用）。
        // 图文排版以矩形宽度为基准，`ow` 是基准宽，用于保证不同宽度下字号按比例一致。
        let text_and_icon = |s: &mut SharedState, ui: &mut Ui, r: Rect, btn: &mut DRectButton, text, icon| {
            let ow = r.w;
            s.render_fader(ui, |ui| {
                btn.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.4));
                    let ir = Rect::new(r.x + 0.02, r.bottom() - 0.08, 0.14, 0.14);
                    ui.text(text).pos(r.x + 0.026, r.y + 0.026).size(0.7 * r.w / ow).draw();
                    ui.fill_rect(
                        {
                            let mut ir = ir;
                            ir.h = ir.h.min(r.bottom() - ir.y);
                            ir
                        },
                        (icon, ir, ScaleType::Fit, semi_white(0.4)),
                    );
                });
            });
        };

        // 阶段 3：次级按钮行，整体套用 `btn_other_3d` 的倾斜矩阵。
        // 活动/资源包为宽矩形，消息/设置为 0.11 见方的小图标、竖排放在右侧。
        let mat = self.btn_other_3d.now(ui, Rect::new(0., top - 0.4, 0.83, 0.23), t);
        ui.with_gl(mat, |ui| {
            let r = Rect::new(0., top, 0.38, 0.23);
            text_and_icon(s, ui, r, &mut self.btn_event, tl!("event"), *self.icons.medal);

            let r = Rect::new(r.right() + 0.02, top, 0.29, 0.23);
            text_and_icon(s, ui, r, &mut self.btn_respack, tl!("respack"), *self.icons.respack);

            let lf = r.right() + 0.02;

            s.render_fader(ui, |ui| {
                let r = Rect::new(lf, top, 0.11, 0.11);
                self.btn_msg.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.4));
                    let r = r.feather(-0.01);
                    ui.fill_rect(r, (*self.icons.msg, r, ScaleType::Fit));
                    // 有未读私信时在图标右上角画一个小红点（无自绘能力，故用实心圆）。
                    if self.has_new {
                        let pad = 0.007;
                        ui.fill_circle(r.right() - pad, r.y + pad, 0.01, RED);
                    }
                });

                let r = Rect::new(lf, top + 0.12, 0.11, 0.11);
                self.btn_settings.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.4));
                    let r = r.feather(0.004);
                    ui.fill_rect(r, (*self.icons.settings, r, ScaleType::Fit));
                });
            });
        });
    }
}

// 首页的页面钩子约定：
// - `enter`：从子页面返回时补放淡入，并刷新未读私信探测；
// - `touch`：过渡中一律吞掉输入，HYKB 启动检查未完成时页面整体不可交互；
// - `update`：驱动登录面板、轮播、立绘与各异步任务的收尾；
// - `render`：先画角色层（可滑动覆盖主菜单），再画主菜单、登录面板与顶层淡入。
//
// `SharedState` 提供两类时间基准：`t`（受暂停/变速影响的游戏时间，用于动效）与
// `rt`（真实时间，用于页面切换）；同时持有页面栈共享的 `fader` 与图标。
// 本页未重写 `can_play_bgm`，沿用默认 `true`——首页作为主菜单保持 BGM 播放；
// 压栈打开子页面（如曲库）时的低通/淡出由主场景统一处理。
impl Page for HomePage {
    /// 页面标识，返回固定字符串 "PHIRA"。
    fn label(&self) -> Cow<'static, str> {
        "PHIRA".into()
    }

    /// 从子页面返回时调用：若曾进过子页面则播放淡入，并重新探测未读私信。
    fn enter(&mut self, s: &mut SharedState) -> Result<()> {
        // 从子页面返回时播放一次淡入；首次进入由场景本身负责，故只在 `need_back` 时执行。
        if self.need_back {
            self.sf.enter(s.t);
            self.need_back = false;
        }
        // 每次回到首页都重新探测未读私信。
        self.fetch_has_new();
        Ok(())
    }

    /// 处理一次触摸；返回 `true` 表示本次触摸已被首页消费。
    ///
    /// # Returns
    /// `Ok(true)` = 触摸已被处理（含被「吞掉」的情况），`Ok(false)` = 未命中任何控件。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        // 页面过渡期间屏蔽所有输入，避免误触下一层按钮。
        if self.sf.transiting() {
            return Ok(true);
        }
        // The HYKB startup check (refresh + verify SDK session) is blocking: keep
        // the home page inert until it resolves.
        #[cfg(feature = "hykb")]
        if self.update_task.is_some() {
            return Ok(true);
        }
        let t = s.t;
        let rt = s.rt;
        // 登录面板（若已弹出）优先消费触摸，避免点击穿透到下层的菜单按钮。
        if self.login.touch(touch, s.t) {
            return Ok(true);
        }
        // 仅当角色介绍面板处于（接近）收起状态时才响应菜单按钮，
        // 否则本次触摸归角色面板所有。
        if self.char_screen_p.now(rt) < 1e-2 {
            self.btn_play_3d.touch(touch, t);
            // 阶段 1：菜单按钮的导航分发。均以「压栈 Overlay 子页面」方式打开，
            // 子页面关闭后自动回到本实例（状态得以保留）。
            if self.btn_play.touch(touch, t) {
                button_hit_large();
                // 曲库不涉网、无需登录，直接进入。
                self.next_page = Some(NextPage::Overlay(Box::new(LibraryPage::new(Arc::clone(&self.icons), s.icons.clone())?)));
                return Ok(true);
            }
            if self.btn_event.touch(touch, t) {
                // 活动内容涉网，先确保用户已读并同意协议/隐私政策。
                if check_read_tos_and_policy(true, true) {
                    button_hit_large();
                    // 活动需要账号：未登录时改为唤起登录面板，登录成功后的跳转由登录模块处理。
                    if get_data().me.is_none() {
                        self.login.enter(t);
                    } else {
                        self.next_page = Some(NextPage::Overlay(Box::new(EventPage::new(Arc::clone(&self.icons), s.icons.clone()))));
                    }
                }
                return Ok(true);
            }
            if self.btn_respack.touch(touch, t) {
                button_hit_large();
                // 资源包为纯本地管理，无需登录。
                self.next_page = Some(NextPage::Overlay(Box::new(ResPackPage::new(Arc::clone(&self.icons))?)));
                return Ok(true);
            }
            if self.btn_msg.touch(touch, t) {
                // 私信同样涉网，需先同意协议。
                if check_read_tos_and_policy(true, true) {
                    self.next_page = Some(NextPage::Overlay(Box::new(MessagePage::new(Arc::clone(&self.icons), s.icons.clone()))));
                }
                return Ok(true);
            }
            if self.btn_settings.touch(touch, t) {
                // 设置页需要两个图标：应用图标（关于页展示）与语言图标。
                self.next_page = Some(NextPage::Overlay(Box::new(SettingsPage::new(self.icons.icon.clone(), self.icons.lang.clone()))));
                return Ok(true);
            }
        } else {
            // 阶段 2：角色介绍已展开时，把触摸交给介绍文本的滚动区，
            // 以及「更换角色」按钮（打开网页版账号设置）。
            if self.char_scroll.touch(touch, t) {
                return Ok(true);
            }
            if self.char_edit_btn.touch(touch) {
                let _ = open_url("https://phira.moe/settings/account");
            }
        }
        // 阶段 3：右上角头像。已登录进入个人资料场景（记录 `need_back` 以便返回时淡入），
        // 未登录则唤起登录面板。
        if self.btn_user.touch(touch, t) {
            if let Some(me) = &get_data().me {
                self.need_back = true;
                self.sf.goto(t, ProfileScene::new(me.id, self.icons.user.clone(), s.icons.clone()));
            } else {
                self.login.enter(t);
            }
            return Ok(true);
        }
        // 阶段 4：HYKB 合规备案号，点击跳转工信部备案查询页。
        #[cfg(feature = "hykb")]
        if self.beian_btn.touch(touch) {
            let _ = open_url("https://beian.miit.gov.cn/#/home");
            return Ok(true);
        }
        // 阶段 5：点击角色立绘切换介绍面板显隐。以 0.5 为分界按当前进度决定目标态，
        // 并把切换时刻记入 `char_text_start`，供文案按出场进度做延迟位移。
        if self.char_btn.touch(touch) {
            if !self.char_screen_p.transiting(rt) {
                let to = if self.char_screen_p.now(rt) < 0.5 {
                    self.char_text_start = rt;
                    1.
                } else {
                    0.
                };
                self.char_screen_p.goto(to, rt, 0.5);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// 每帧推进：驱动登录面板、角色数据/立绘、曲绘轮播与各异步任务的收尾。
    ///
    /// # Errors
    /// 保存用户资料等 IO/网络错误会上抛（部分子任务选择仅记录日志）。
    fn update(&mut self, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        // 阶段 1：登录面板。HYKB 构建下未登录时强制弹出且不可关闭。
        // HYKB builds require an account: while signed out, keep the login panel
        // forced open. Polling here (rather than only on entry) also covers the
        // player manually logging out and popping back to the home page.
        #[cfg(feature = "hykb")]
        if get_data().me.is_none() {
            self.login.force(t);
        }
        self.login.update(t)?;
        // 阶段 2：账号变化检测。用 (-1) 代表未登录；仅当用户 id 变化时才重新拉取角色数据，
        // 避免每帧请求。离线/未登录则清空任务。
        let current_user = Some(get_data().me.as_ref().map_or(-1, |it| it.id));
        self.char_scroll.update(t);
        if self.char_last_user_id != current_user {
            let locale = get_data().language.clone().unwrap_or(LANG_IDENTS[0].to_string());
            self.char_last_user_id = current_user;
            if get_data().config.offline_mode || get_data().me.is_none() || get_data().tokens.is_none() {
                self.char_fetch_task = None;
            } else {
                self.char_fetch_task =
                    Some(Task::new(async move { Ok(recv_raw(Client::get("/me/char").query(&[("locale", locale)])).await?.json().await?) }));
            }
        }
        // 阶段 3：会话恢复任务收尾。
        if let Some(task) = &mut self.update_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        // 令牌失效：清空本地会话并同步数据；再针对「已申请注销」的专用
                        // 错误码弹出撤销/登出二选一对话框，其余错误统一提示重登。
                        // wtf bro
                        if format!("{err:?}").contains("invalid token") {
                            get_data_mut().me = None;
                            get_data_mut().tokens = None;
                            let _ = save_data();
                            sync_data();
                        }
                        if err.downcast_ref::<ErrorCode>() == Some(&ErrorCode::PENDING_DELETE_REQUEST) {
                            self.pending_delete_choice.store(0, Ordering::SeqCst);
                            use crate::login::{tl as ltl, L10N_LOCAL};
                            // 对话框按钮顺序为 [取消, 确认]；`id == -1` 表示点遮罩/ESC 关闭，
                            // 不写入选择，因此既不撤销注销也不会登出。
                            let choice = Arc::clone(&self.pending_delete_choice);
                            Dialog::plain(ltl!("pending-delete-title").into_owned(), ltl!("pending-delete-message").into_owned())
                                .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
                                .listener(move |_dialog, id| {
                                    if id == -1 {
                                        return true;
                                    }
                                    choice.store(if id == 1 { 1 } else { 2 }, Ordering::SeqCst);
                                    false
                                })
                                .show();
                        } else {
                            // TODO: better error handling
                            show_error(err.context(tl!("failed-to-update") + "\n" + tl!("note-try-login-again")));
                        }
                    }
                    Ok(val) => {
                        // 恢复成功：用最新资料刷新本地缓存并立即落盘。
                        get_data_mut().me = Some(val);
                        save_data()?;
                    }
                }
                self.update_task = None;
            }
        }
        // 消费对话框选择：1 = 撤销注销并以 `cancel_delete_request` 重试登录；2 = 执行登出。
        match self.pending_delete_choice.swap(0, Ordering::Relaxed) {
            1 => {
                // 用 refresh token 重新登录并显式取消注销请求，同时再次获取用户资料。
                let tokens = get_data().tokens.clone();
                self.update_task = tokens.map(|(_, refresh)| {
                    Task::new(async move {
                        Client::login(LoginParams::RefreshToken {
                            token: &refresh,
                            cancel_delete_request: true,
                        })
                        .await?;
                        let me = Client::get_me().await?;
                        #[cfg(feature = "hykb")]
                        crate::obtain_hykb_credential_silent().await?.ok_or_err()?;
                        Ok(me)
                    })
                });
            }
            2 => {
                // 玩家确认登出：清空凭据并回到未登录状态。
                crate::force_logout();
            }
            _ => {}
        }
        // 阶段 4：曲绘轮播。空闲满 `BOARD_SWITCH_TIME` 后挑一张与上次不同的曲绘异步加载；
        // 只有 0/1 张谱面时退化为纯黑或无切换。
        if self.board_task.is_none() && t - self.board_last_time > BOARD_SWITCH_TIME {
            let charts = &get_data().charts;
            let last_index = self
                .board_last
                .as_ref()
                .and_then(|path| charts.iter().position(|it| &it.local_path == path));
            if charts.is_empty() || (charts.len() == 1 && last_index.is_some()) {
                self.board_task = Some(Task::new(async move { Ok(None) }));
            } else {
                // 在「去掉上次那张」的集合里均匀抽样，再把索引映射回原数组，
                // 从而保证不会连续抽到同一张曲绘。
                let mut index = thread_rng().gen_range(0..(charts.len() - last_index.is_some() as usize));
                if last_index.is_some_and(|it| it <= index) {
                    index += 1;
                }
                let path = charts[index].local_path.clone();
                let dir = prpr::dir::Dir::new(format!("{}/{}", dir::charts()?, path))?;
                self.board_last = Some(path);
                self.board_task = Some(Task::new(async move {
                    let info: ChartInfo = serde_yaml::from_reader(dir.open("info.yml")?)?;
                    let bytes = dir.read(info.illustration)?;
                    Ok(Some(image::load_from_memory(&bytes)?))
                }));
            }
        }
        if let Some(task) = &mut self.board_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        // 加载失败不改变当前画面，仅记录日志。
                        warn!(?err, "failed to load illustration for board");
                    }
                    Ok(image) => {
                        if let Some(image) = image {
                            let tex: SafeTexture = image.into();
                            // 新图就位后旧图转入退场槽位，并随机决定推入方向；计时从此刻开始。
                            self.board_tex_last = self.board_tex.replace(tex);
                            self.board_dir = random();
                        }
                    }
                }
                self.board_last_time = t;
                self.board_task = None;
            }
        }
        // 阶段 5：未读私信探测结果收尾。
        if let Some(task) = &mut self.has_new_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!("fail to load has new {:?}", err);
                    }
                    Ok(has) => {
                        self.has_new = has;
                    }
                }
                self.has_new_task = None;
            }
        }
        // 阶段 6：版本检查收尾。仅当新版本比玩家「忽略的版本」更新时才弹窗，
        // 提供 取消/忽略此版本/前往更新 三个选项。
        if let Some(task) = &mut self.check_update_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!("fail to check update {:?}", err);
                    }
                    Ok(Some(ver)) => {
                        if get_data().ignored_version.as_ref().is_none_or(|it| it < &ver.version) {
                            Dialog::plain(
                                tl!("update", "version" => ver.version.to_string()),
                                tl!("update-desc", "date" => ver.date.to_string(), "desc" => ver.description),
                            )
                            .buttons(vec![
                                ttl!("cancel").into_owned(),
                                tl!("update-ignore").into_owned(),
                                tl!("update-go").into_owned(),
                            ])
                            .listener(move |_dialog, pos| {
                                match pos {
                                    // 「忽略」：记下版本号，之后不再提示该版本（落盘）。
                                    1 => {
                                        get_data_mut().ignored_version = Some(ver.version.clone());
                                        let _ = save_data();
                                    }
                                    // 「前往更新」：用系统浏览器打开下载链接。
                                    2 => {
                                        let _ = open_url(&ver.url);
                                    }
                                    _ => {}
                                }
                                false
                            })
                            .show();
                        }
                    }
                    _ => {}
                }
                self.check_update_task = None;
            }
        }
        // 阶段 7：加粗字体热更新收尾。`Ok(None)` 表示服务端仍是最新（HTTP 304），无需处理。
        if let Some(task) = &mut self.check_bold_font_update_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!("fail to check bold font update {:?}", err);
                    }
                    Ok(None) => {}
                    Ok(Some(parsed)) => {
                        // 替换全局加粗字体，并记录校验和以便下次比对。
                        info!(cksum = parsed.1, "new bold font");
                        set_bold_font(parsed);
                    }
                }
                self.check_bold_font_update_task = None;
            }
        }
        // 阶段 8：角色立绘加载收尾。成功后在 0.5 秒内淡入，并生成 mipmap 以适配不同缩放。
        if let Some(task) = &mut self.char_illu_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!(?err, "fail to load char illu");
                    }
                    Ok(image) => {
                        self.char_appear_p.goto(1., t, 0.5);
                        let tex: SafeTexture = image.into();
                        self.char_illu = Some(tex.with_mipmap());
                    }
                }
                self.char_illu_task = None;
            }
        }
        // 阶段 9：联网刷新角色数据收尾：写回缓存、落盘，并重新加载立绘。
        if let Some(task) = &mut self.char_fetch_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!(?err, "fail to load char");
                    }
                    Ok(char) => {
                        info!(?char, "char loaded");
                        self.character = char;
                        get_data_mut().character = Some(self.character.clone());
                        let _ = save_data();
                        // 角色已变，重置自适应字号缓存并触发立绘重载。
                        self.char_cached_size = 0.;
                        self.load_char_illu();
                    }
                }
                self.char_fetch_task = None;
            }
        }
        // 阶段 10：登录流程若刚加载并同意过协议，这里补一次检查以触发相应界面刷新。
        if JUST_LOADED_TOS.fetch_and(false, Ordering::Relaxed) {
            check_read_tos_and_policy(true, true);
        }

        Ok(())
    }

    /// 渲染首页：角色层 + 主菜单层 + 头像 + 登录面板 + 顶层淡入。
    ///
    /// 两层用 `char_screen_p`（`cp`）交叉淡入：`cp` 越大角色层越靠右占据画面、
    /// 主菜单越透明。角色层整体套一个 3D 倾斜矩阵，令其呈弧面纵深。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        let rt = s.rt;

        // 阶段 1：角色层。`cp` 同时驱动水平位移（-1 → 0）与两层的透明度。
        let cp = self.char_screen_p.now(rt);
        s.render_fader(ui, |ui| {
            let r = Rect::new(-1. + 0.14 * cp, -ui.top + 0.12, 1., 1.7);
            if let Some(illu) = &self.char_illu {
                // `illu_adjust` 是每张立绘按美术构图手工标定的四向偏移/留白修正，
                // 直接叠加到基准矩形上，使不同立绘的主体都落在合适位置。
                let p = self.char_appear_p.now(t);
                let (ox, oy, ow, oh) = self.character.illu_adjust;
                let r = Rect::new(r.x + ox, r.y + (1. - p) * 0.05 + oy, r.w + ow, r.h + oh);
                ui.fill_rect(ui.screen_rect(), (**illu, r, ScaleType::CropCenter, semi_white(p)));
            }
            self.char_btn.set(ui, r);

            // 阶段 2：角色介绍卡片。`cp` 极小时直接跳过，省掉矩阵与文本布局开销。
            // 卡片高度随屏幕宽高比收窄（越窄的屏卡片越矮），保证竖向留白。
            if cp > 1e-5 {
                let height = 0.8 - ((screen_aspect() - 16. / 9.) * 0.2).min(0.2);
                let r = Rect::new(0.16, (-height - height * cp) / 4., 0.6, height);
                // 用固定锚点 (0,0) 的倾斜矩阵把卡片沿入/出场方向做出透视位移。
                let mat = ThreeD::build(vec2(0., 0.), r, 0.12);
                // SAFETY: 本调用处于主线程的渲染帧内，`get_internal_gl` 返回的全局 GL
                // 上下文在本次压栈/弹栈（`push_model_matrix`/`pop_model_matrix`）之间独占，
                // 不会与其它线程并发访问。
                let gl = unsafe { get_internal_gl() }.quad_gl;
                gl.push_model_matrix(mat);

                ui.alpha(cp, |ui| {
                    // 卡片主体：半透明底 + 左侧一条 1px 白色竖线作为视觉分隔；
                    // 顶部预留 0.14 给上方的角色名字。
                    let mut r = Rect::new(r.x, r.y + 0.14, r.w, r.h - 0.14);
                    ui.fill_rect(r, semi_black(0.3));
                    ui.fill_rect(Rect::new(r.x, r.y, 0.01, r.h), WHITE);
                    // 「更换角色」按钮，其命中框按文字测量结果外扩少量留白以便点按。
                    let mut t = ui.text(tl!("change-char")).pos(r.x + 0.01, r.bottom() + 0.015).size(0.3);
                    let ir = t.measure().feather(0.007);
                    t.ui.fill_rect(ir, semi_black(0.2));
                    self.char_edit_btn.set(t.ui, ir);
                    t.draw();
                    let pad = 0.01;

                    let mut t = ui
                        .text(self.character.name_en())
                        .pos(r.right() - pad, r.bottom() - pad)
                        .anchor(1., 1.)
                        .color(semi_white(0.2));
                    // 角色英文名的自适应字号：从 2.0 起按 0.95 逐级缩小，直到渲染宽度
                    // 小于卡片的 70%；结果缓存到 `char_cached_size` 避免每帧重算。
                    if self.char_cached_size < 1e-6 {
                        let mut initial = 2.;
                        loop {
                            t = t.size(initial);
                            if t.measure().w < r.w * 0.7 {
                                break;
                            }
                            initial *= 0.95;
                        }
                        self.char_cached_size = initial;
                    } else {
                        t = t.size(self.char_cached_size);
                    }
                    t.draw();

                    // 收缩矩形（左侧让出竖线宽度）后作为介绍文本的滚动区。
                    r.x += 0.01;
                    r.w -= 0.01;

                    self.char_scroll.size((r.w, r.h));
                    ui.scope(|ui| {
                        ui.dx(r.x);
                        ui.dy(r.y);
                        let ow = r.w;
                        self.char_scroll.render(ui, |ui| {
                            let r = Rect::new(0., 0., r.w, r.h);
                            let r = r.feather(-0.03);
                            let r = ui.text(&self.character.intro).pos(r.x, r.y).max_width(r.w).multiline().size(0.4).draw();
                            (ow, r.h + 0.1)
                        });
                    });
                });

                // 阶段 3：卡片上方的角色名与作者/设计者署名。`(1 - cp)` 让文字在
                // 出场过程中略领先于卡片移动，形成视差。
                let r = Rect::new(r.x, r.y, 0.4, 0.12);

                ui.alpha(cp, |ui| {
                    let r = ui
                        .text(&self.character.name)
                        .pos(r.x + (1. - cp) * 0.12 + 0.01, r.center().y)
                        .anchor(0., 0.5)
                        .size(self.character.name_size.unwrap_or(1.4))
                        .draw_using(&BOLD_FONT);

                    // `baseline` 用于修正个别字体/立绘组合下的署名基线偏移。
                    let off = if self.character.baseline { 0. } else { 0.01 };
                    ui.text(format!("Artist: {}", self.character.artist))
                        .pos(r.right() + (1. - cp) * 0.1 + 0.02, r.bottom() + off - 0.03)
                        .anchor(0., 1.)
                        .size(0.34)
                        .color(semi_white(0.7))
                        .draw();
                    ui.text(format!("Designer: {}", self.character.designer))
                        .pos(r.right() + (1. - cp) * 0.1 + 0.016, r.bottom() + off)
                        .anchor(0., 1.)
                        .size(0.34)
                        .color(semi_white(0.7))
                        .draw();
                });

                gl.pop_model_matrix();
            }
        });

        // 阶段 4：主菜单层，与角色层按 `1 - cp` 交叉淡入。
        ui.alpha(1. - cp, |ui| {
            self.render_not_char(ui, s);
        });

        // 阶段 5：右上角用户信息（头像 + 昵称 + RKS）。先回退 fader 的层级索引，
        // 使此处内容不被当作更深一层而额外淡出。
        s.fader.roll_back();
        s.render_fader(ui, |ui| {
            // 头像为正圆：用零尺寸矩形 `feather(rad)` 生成边长 2*rad 的包围盒；
            // 未登录时 `unwrap_or(Err(...))` 分支会退回灰色占位图标。
            let rad = 0.05;
            let ct = (0.92, -ui.top + 0.08);
            self.btn_user.config.radius = rad;
            let r = Rect::new(ct.0, ct.1, 0., 0.).feather(rad);
            self.btn_user.build(ui, t, r, |ui, _| {
                ui.avatar(
                    ct.0,
                    ct.1,
                    r.w / 2.,
                    t,
                    get_data()
                        .me
                        .as_ref()
                        .map(|user| UserManager::opt_avatar(user.id, &self.icons.user))
                        .unwrap_or(Err(self.icons.user.clone())),
                );
            });
            // 昵称与 RKS 右对齐到头像左侧；RKS 由服务端下发并缓存于用户资料。
            let rt = ct.0 - rad - 0.02;
            if let Some(me) = &get_data().me {
                ui.text(&me.name).pos(rt, r.center().y + 0.002).anchor(1., 1.).size(0.6).draw();
                ui.text(format!("RKS {:.2}", me.rks))
                    .pos(rt, r.center().y + 0.008)
                    .anchor(1., 0.)
                    .size(0.4)
                    .color(semi_white(0.6))
                    .draw();
            } else {
                ui.text(tl!("not-logged-in"))
                    .pos(rt, r.center().y)
                    .anchor(1., 0.5)
                    .no_baseline()
                    .size(0.6)
                    .draw();
            }

            // HYKB 渠道合规：屏幕左下角常驻展示备案号，整行文字可点击。
            #[cfg(feature = "hykb")]
            {
                let r = ui.screen_rect();
                let r = ui
                    .text("备案号：闽ICP备18008307号-64A")
                    .pos(r.x + 0.02, r.bottom() - 0.03)
                    .size(0.5)
                    .anchor(0., 1.)
                    .draw();
                self.beian_btn.set(ui, r);
            }
        });

        // 阶段 6：登录面板与页面顶层淡入。登录面板最后绘制以保证其位于最上层。
        self.login.render(ui, t);
        // Cover the home page with a blocking loader during the HYKB startup check.
        #[cfg(feature = "hykb")]
        if self.update_task.is_some() {
            ui.full_loading_simple(t);
        }
        self.sf.render(ui, t);

        Ok(())
    }

    /// 取走本帧要打开的子页面；无则返回 [`NextPage::None`]。
    fn next_page(&mut self) -> NextPage {
        self.next_page.take().unwrap_or_default()
    }

    /// 取走本帧要切换到的场景（用于个人资料等整场景跳转）。
    fn next_scene(&mut self, s: &mut SharedState) -> NextScene {
        self.sf.next_scene(s.t).unwrap_or_default()
    }
}
