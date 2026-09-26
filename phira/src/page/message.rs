//! 站内消息页：左侧消息标题列表（`btns_scroll`）+ 右侧正文详情（`scroll`）的双栏布局。
//!
//! 数据来自 `/message/list`，采用**游标式分页**：每次请求带上当前列表最后一条的时间作为
//! `before`，只拉取更旧的消息，因此后续页是“追加”到列表尾部而不是替换。
//!
//! 「未读」并非服务端状态，而是客户端本地记录：打开一条比 `data.message_check_time` 更新的消息时，
//! 把它记下来并落盘；首页拿这个时间戳向 `/message/has_new` 询问是否有更新，来决定是否显示红点。
//!
//! 消息可携带 [`crate::client::MessageAction`]，`action` 字段是形如 `type:param` 的机器可读标识，
//! 由本页翻译成具体行为（打开外链 / 跳转谱面 / 打开用户主页）。
//! 未知动作只记录告警而不失败——服务端可能下发比当前客户端更新的动作类型。

prpr_l10n::tl_file!("message");

use std::{borrow::Cow, sync::Arc};

use super::{Page, SharedState};
use crate::{
    client::{recv_raw, Chart, Client, Message, Ptr},
    get_data, get_data_mut,
    icons::Icons,
    page::{ChartItem, SFader},
    save_data,
    scene::{ProfileScene, SongScene},
};
use anyhow::Result;
use chrono::Local;
use macroquad::prelude::*;
use prpr::{
    ext::{open_url, semi_black, semi_white, RectExt, SafeTexture},
    scene::show_error,
    task::Task,
    ui::{DRectButton, Scroll, Ui},
};

/// 站内消息页。
pub struct MessagePage {
    /// 已加载的消息列表，每项与自己在左栏的按钮**成对存放**。
    ///
    /// 之所以把按钮和消息绑在一起而不是另开一个平行的 `Vec`：两者必须同增同减，
    /// 分开存放极易在分页追加时错位。`None` 表示尚没有任何数据（加载中或加载失败）。
    msgs: Option<Vec<(Message, DRectButton)>>,
    /// 拉取“更旧一页”消息的请求。`Some` 时左栏顶部显示加载遮罩并暂停左栏交互，
    /// 但右栏的正文滚动仍然可用（用户可以在加载期间继续读已打开的消息）。
    load_task: Option<Task<Result<Vec<Message>>>>,

    /// 当前展开的消息索引；`None` 表示没有选中任何一条（右栏空白）。
    index: Option<usize>,

    /// 左栏（标题列表）的滚动容器，同时承担“下拉刷新”的手势检测。
    btns_scroll: Scroll,
    /// 右栏（正文）的滚动容器。
    scroll: Scroll,

    /// 消息动作按钮池，长度与当前消息的 `actions` 对齐（用 `resize_with` 复用，
    /// 避免每条消息都重新分配一遍按钮）。
    action_btns: Vec<DRectButton>,

    /// 跳到谱面/用户主页前的整屏遮罩淡出器，见 [`SFader`]。
    sf: SFader,
    /// 异步拉取谱面的任务（消息动作引用了某张谱面时需要先取回数据）。
    chart_task: Option<Task<Result<Arc<Chart>>>>,
    /// 通用图标资源。
    icons: Arc<Icons>,
    /// 8 个段位图标，构造用户主页/谱面场景时需要一并移交。
    rank_icons: [SafeTexture; 8],
}

// 本页的加载与动作分发逻辑：数据加载是**惰性**的（不在构造时发起），
// 动作分发负责把服务端下发的字符串协议翻译成客户端行为。
impl MessagePage {
    /// 创建消息页，但不发起任何网络请求。
    ///
    /// 与活动页不同，这里把加载留到 [`Page::enter`]：本页会被 `MainScene` 长期持有，
    /// 每次进入都希望看到最新消息，因此在 `enter` 里刷新比在构造时一次性加载更合适。
    pub fn new(icons: Arc<Icons>, rank_icons: [SafeTexture; 8]) -> Self {
        Self {
            msgs: None,
            load_task: None,

            index: None,

            btns_scroll: Scroll::new(),
            scroll: Scroll::new(),

            action_btns: Vec::new(),

            sf: SFader::new(),
            chart_task: None,
            icons,
            rank_icons,
        }
    }

    /// 追加拉取一页更旧的消息。
    ///
    /// 若已有请求在飞则直接返回：这是**幂等**保护，因为本方法会被下拉刷新、页面进入
    /// 等路径反复调用，重复发起会浪费流量并让列表出现重复项。
    ///
    /// 游标取当前列表**最后一条**的时间，配合 `before` 的语义，后续请求只会拿到更旧的消息；
    /// 首次加载时游标为空，即请求最新一页。
    pub fn load(&mut self) {
        if self.load_task.is_some() {
            return;
        }
        let before = self.msgs.as_ref().and_then(|it| it.last().map(|it| it.0.time));
        self.load_task = Some(Task::new(async move {
            let mut req = Client::get("/message/list");
            if let Some(before) = before {
                req = req.query(&[("before", before)]);
            }
            Ok(recv_raw(req).await?.json().await?)
        }));
    }

    /// 执行一条消息动作。
    ///
    /// 协议为 `type:param`，用 `split_once(':')` 只切**第一个**冒号，
    /// 因此 `param` 内部可以再含冒号——这对 `url`（`https://...`）是必需的。
    ///
    /// 容错策略：缺少冒号、id 解析失败、类型未知都只写日志并返回 `Ok(())`。
    /// 原因是动作内容由服务端下发，客户端无法保证认识所有类型；
    /// 把它当错误向上传播会导致整帧失败，用户看到的却是“点了个按钮就崩了”。
    ///
    /// # Arguments
    /// * `t` — 真实时间，仅用于启动 [`SFader`] 的遮罩动画
    /// * `action` — 服务端下发的动作字符串
    ///
    /// # Errors
    /// 只有 `url` 分支会真正返回错误（交给系统打开链接失败，如无可用浏览器）。
    fn execute_action(&mut self, t: f32, action: String) -> Result<()> {
        let (ty, param) = match action.split_once(':') {
            Some(it) => it,
            None => {
                warn!("invalid action: {action}");
                return Ok(());
            }
        };
        match ty {
            // 打开外部链接（交给系统浏览器）。这是唯一可能真正失败的分支，故用 `?` 向上报错
            "url" => {
                open_url(param)?;
            }
            // 跳转到某张谱面：谱面数据要先联网取回，因此只登记任务，
            // 由 `update` 轮询结果后再启动场景切换
            "chart" => {
                let id = match param.parse::<i32>() {
                    Ok(it) => it,
                    Err(_) => {
                        warn!("invalid chart id: {param}");
                        return Ok(());
                    }
                };
                self.chart_task = Some(Task::new(async move { Ptr::<Chart>::new(id).fetch().await }));
            }
            // 打开用户主页：资料场景是同步构造的，直接压黑切过去即可
            "user" => {
                let id = match param.parse::<i32>() {
                    Ok(it) => it,
                    Err(_) => {
                        warn!("invalid user id: {param}");
                        return Ok(());
                    }
                };
                self.sf.goto(t, ProfileScene::new(id, self.icons.user.clone(), self.rank_icons.clone()));
            }
            // 未知类型：静默忽略，保证旧客户端遇到新动作时不崩
            _ => {
                warn!("unknown action type: {ty}");
            }
        }

        Ok(())
    }
}

// 本页在页面栈中的行为约定：
// - `enter` 时刷新列表（拉取更新的第一页），因此从谱面/用户主页返回也能看到新消息；
// - 不请求压栈/弹栈（`next_page` 用默认实现），只通过 `next_scene` 请求离开页面栈；
// - 左栏下拉到底触发加载更旧一页；加载中屏蔽左栏交互但保留右栏阅读与滚动；
// - 打开某条消息时同步更新本地的“已读时间”，从而让首页的红点消失。
impl Page for MessagePage {
    /// 标题栏文案，取自本模块的 `message.ftl`。
    fn label(&self) -> Cow<'static, str> {
        tl!("label")
    }

    /// 每次成为栈顶都触发一次 [`MessagePage::load`]。
    ///
    /// 不区分“首次进入”与“从子场景返回”；`load` 自带去重保护，因此不会产生并发请求。
    ///
    /// 需要注意游标语义带来的后果：`load` 用列表**最后一条**的时间作游标，
    /// 若服务端按时间倒序返回（即列表自上而下越来越旧），那么重新进入本页拿到的是
    /// **更旧的一页**并追加到列表尾部，而不是把最新消息插到顶部。
    fn enter(&mut self, _s: &mut SharedState) -> Result<()> {
        self.load();
        Ok(())
    }

    /// 处理触摸：左栏列表选择、消息动作按钮、右栏正文滚动。
    ///
    /// 三段优先级与各自的“冻结”规则：
    /// - 正在为某个动作拉取谱面时**直接放行**（返回 `false`，不消费）：此时整页不接受交互，
    ///   但仍然把事件让给 `MainScene`，所以返回键照常可用，用户不会被锁死；
    /// - 正在加载更旧消息时冻结左栏（列表与动作按钮都不响应），但右栏仍可滚动阅读——
    ///   加载与阅读互不干扰；
    /// - 其余情况先左栏后右栏，最后才轮到右栏滚动。
    ///
    /// 「已读」的判断有方向性：只有当被打开的消息**比已记录的检查时间更新**时才回写，
    /// 因此翻看历史消息不会把已读时间往回拨、也就不会让红点重新出现。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        let t = s.t;
        // 阶段一：动作触发的谱面加载中——整页冻结但不吞事件
        if self.chart_task.is_some() {
            return Ok(false);
        }
        if self.load_task.is_none() {
            // 阶段二：左栏滚动（含下拉刷新）优先于列表项命中
            if self.btns_scroll.touch(touch, t) {
                return Ok(true);
            }
            if let Some(msgs) = &mut self.msgs {
                // 阶段三：列表项命中。再次点击已展开的那条即收起（切换语义）
                for (index, item) in msgs.iter_mut().enumerate() {
                    if item.1.touch(touch, t) {
                        if self.index == Some(index) {
                            self.index = None;
                        } else {
                            // 打开即视为已读：把本地检查时间推进到这条消息的时间并落盘，
                            // 落盘是为了让首页下次启动时不会因为该消息又亮红点
                            if get_data().message_check_time.is_none_or(|it| it < item.0.time) {
                                get_data_mut().message_check_time = Some(item.0.time);
                                save_data()?;
                            }
                            self.index = Some(index);
                        }
                        return Ok(true);
                    }
                }
                // 阶段四：动作按钮。按钮池与当前消息的 actions 按位置一一对应，
                // 池的长度由 `render` 保证，因此这里的 zip 不会有剩余项
                if let Some(index) = self.index {
                    for (btn, action) in self.action_btns.iter_mut().zip(&msgs[index].0.actions) {
                        if btn.touch(touch, t) {
                            let action = action.action.clone();
                            self.execute_action(t, action)?;
                            return Ok(true);
                        }
                    }
                }
            }
        }
        // 阶段五：右栏正文滚动（不受左栏加载状态影响）
        if self.scroll.touch(touch, t) {
            return Ok(true);
        }
        Ok(false)
    }

    /// 每帧推进：下拉刷新检测、两个滚动容器、以及两个异步任务的轮询。
    ///
    /// 新消息是**追加**（`extend`）而不是替换，配合游标分页才不会丢掉已加载的历史。
    /// 按钮创建时传入 `with_delta(-0.001)`：该字段目前在 prpr 中**不参与渲染**
    /// （按下时的命中区收缩逻辑被注释掉了），这里只是沿用其他列表页的写法，没有实际视觉效果。
    ///
    /// 谱面取回成功后才计算 `local_path` 与 `mods`：消息里只带谱面 id，
    /// 若用户本地已有同一份谱面（按 id 匹配 `get_data().charts`），就直接把路径与模组带进场景，
    /// 从而跳过下载、也能沿用用户自定义的模组设置。
    fn update(&mut self, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        // 阶段一：下拉刷新（标志由上一帧的滚动器置位，这里读取并及时消费）
        if self.btns_scroll.y_scroller.pulled_down {
            self.load();
        }
        // 阶段二：滚动容器推进（左栏列表 + 右栏正文）
        self.btns_scroll.update(t);
        self.scroll.update(t);
        // 阶段三：分页加载结果
        if let Some(task) = &mut self.load_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("load-msg-fail")));
                    }
                    Ok(val) => {
                        // 首次加载时 `msgs` 还是 `None`，先就地插入空向量再统一追加
                        let mt = match &mut self.msgs {
                            None => self.msgs.insert(Vec::new()),
                            Some(x) => x,
                        };
                        mt.extend(val.into_iter().map(|it| (it, DRectButton::new().with_delta(-0.001))));
                    }
                }
                self.load_task = None;
            }
        }
        // 阶段四：消息动作引用的谱面是否已取回
        if self.chart_task.is_some() {
            if let Some(res) = self.chart_task.as_mut().unwrap().take() {
                match res {
                    Err(err) => {
                        show_error(err);
                    }
                    Ok(chart) => {
                        let data = get_data();
                        let (local_path, mods) = data
                            .charts
                            .iter()
                            .find(|it| it.info.id == Some(chart.id))
                            .map(|it| (Some(it.local_path.clone()), it.mods))
                            .unwrap_or_default();
                        // 用 SFader 压黑再切场景：跨越页面栈的切换都走整屏遮罩，
                        // 避免把引擎重建场景的过程暴露给用户（见 [`SFader`] 的说明）
                        self.sf.goto(
                            t,
                            SongScene::new(ChartItem::from_remote(chart.as_ref()), local_path, self.icons.clone(), self.rank_icons.clone(), mods),
                        );
                    }
                }
                self.chart_task = None;
            }
        }
        Ok(())
    }

    /// 绘制双栏界面：左栏标题列表、右栏正文与动作按钮。
    ///
    /// 布局上先算好两块面板的矩形，再各自用一个 [`SharedState::render_fader`] 绘制。
    /// 之所以**分成两个** `render_fader` 作用域而不是包一个大的：每调用一次就会消耗
    /// 一个 [`crate::page::Fader::DELTA`] 层延迟，于是转场时左栏先到、右栏稍后跟上，产生自然的错峰，
    /// 而不是整块面板一起平移。
    ///
    /// 右栏用“顺序布局 + 高度记账”的方式排版：标题、副标题、分隔线依次绘制并累加高度 `h`，
    /// 最后用面板总高减去 `h` 得到正文滚动区的可视高度。这也是 `self.scroll.size(...)`
    /// 出现在 `self.scroll.render(...)` **之前**的原因——本帧的裁剪范围必须先于内容确定。
    ///
    /// 两个 `Scroll` 的约定相同：先 `size` 声明可视区，再 `render` 里返回内容尺寸
    /// （垂直方向只用到内容高度，宽度只是名义值）。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        // 右栏面板的矩形：从内容区左侧让出 0.29 的宽度（正是左栏面板占用的部分）
        let mut cr = ui.content_rect();
        let d = 0.29;
        cr.x += d;
        cr.w -= d;
        // 左栏面板固定贴屏幕左侧，宽度 0.47
        let r = Rect::new(-0.92, cr.y, 0.47, cr.h);
        s.render_fader(ui, |ui| {
            // 阶段一：左栏底板与可视区尺寸（高度扣掉 pad，避免最后一项被底边裁掉一半）
            ui.fill_path(&r.rounded(0.005), semi_black(0.4));
            let ct = r.center();
            let pad = 0.014;
            self.btns_scroll.size((r.w, r.h - pad));
            if let Some(msgs) = &mut self.msgs {
                if msgs.is_empty() {
                    // 服务端确实返回了空列表（与“还在加载”是两种状态，后者有遮罩与转圈）
                    ui.text(tl!("no-msg")).pos(ct.x, ct.y).anchor(0.5, 0.5).no_baseline().size(0.8).draw();
                } else {
                    // 列表内容坐标原点设在面板左上角加一个 pad，因此每项只需向下累加
                    ui.scope(|ui| {
                        ui.dx(r.x);
                        ui.dy(r.y + pad);
                        self.btns_scroll.render(ui, |ui| {
                            let w = r.w - pad * 2.;
                            let mut h = 0.;
                            // 每项固定高 0.09，宽度撑满面板内容区
                            let r = Rect::new(pad, 0., r.w - pad * 2., 0.09);
                            for (index, item) in msgs.iter_mut().enumerate() {
                                // 最后那个参数是“是否选中”，用于高亮当前展开的消息
                                item.1.render_text_left(ui, r, t, 1., &item.0.title, 0.5, Some(index) == self.index);
                                ui.dy(r.h + pad);
                                h += r.h + pad;
                            }
                            // 末尾再补一个 pad：否则拉到最底时最后一项会紧贴面板下边缘
                            h += pad;
                            (w, h)
                        });
                    });
                }
            }
            // 阶段二：加载遮罩。用半透明白盖住列表并叠加转圈，而不是清空列表——
            // 保持旧内容可见能让用户对“正在追加”有预期，也不会造成布局跳动
            if self.load_task.is_some() {
                ui.fill_path(&r.rounded(0.005), semi_white(0.3));
                ui.loading(ct.x, ct.y, t, WHITE, ());
            }
        });
        // 阶段三：右栏。单独一个作用域，因此比左栏多消耗一层延迟（转场时稍晚跟上）
        s.render_fader(ui, |ui| {
            ui.fill_path(&cr.rounded(0.005), semi_black(0.4));

            // 未选中任何消息时整块面板保持空白，不做额外提示
            if let Some(msg) = self.index.and_then(|it| self.msgs.as_ref().map(|msgs| &msgs[it].0)) {
                let pad = 0.03;
                // 阶段四：动作按钮。从面板右下角开始向上排布：
                // 先把坐标系移到右下角，再用负的 `dy` 逐行上移，这样按钮数量变化时底部始终对齐
                ui.scope(|ui| {
                    ui.dx(cr.right() - pad);
                    ui.dy(cr.bottom() - pad);
                    // 按钮池长度与当前消息的动作数对齐：换一条消息时复用已有按钮，多余的直接丢弃
                    self.action_btns.resize_with(msg.actions.len(), DRectButton::new);
                    let mut r = Rect::new(0., 0., 0.28, 0.1);
                    r.x -= r.w;
                    r.y -= r.h;
                    for (btn, action) in self.action_btns.iter_mut().zip(&msg.actions) {
                        btn.render_text(ui, r, t, &action.name, 0.5, false);
                        ui.dy(-r.h - 0.01);
                    }
                });

                // 阶段五：正文排版。坐标系移到面板内容区左上角，`mw` 是可用文字宽度。
                // 下面的顺序布局把每段的高度都累加进 `h`，`h` 稍后用于算正文滚动区的可视高度
                ui.dx(cr.x + pad + 0.01);
                ui.dy(cr.y + pad);
                let mw = cr.w - pad * 2. - 0.01;
                let mut h = 0.;
                // 局部宏：绘制一行并把它的高度计入 `h`。
                // 用宏而不是闭包，是因为闭包需要同时可变借用 `ui` 与 `h`，宏展开后只是顺序语句，
                // 借用冲突自然消失。
                macro_rules! dy {
                    ($e:expr) => {{
                        let e = $e;
                        ui.dy(e);
                        h += e;
                    }};
                }
                dy!(ui.text(&msg.title).size(0.9).multiline().max_width(mw).draw().h + 0.017);
                // 副标题把作者与时间用 l10n 模板拼在一起，时间按本地时区格式化后再传入
                let th = ui.text(
                    tl!("subtitle", "author" => msg.author.as_str(), "time" => msg.time.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()),
                )
                .pos(0.01, 0.)
                .size(0.4)
                .color(semi_white( 0.7))
                .draw().h;
                dy!(th + 0.016);
                // 一条细分隔线，把“元信息”与“正文”在视觉上切开
                ui.fill_rect(Rect::new(0., 0., mw, 0.006), semi_white(0.8));
                dy!(0.015);
                // 正文滚动区：可视高度 = 面板高 - 已用高度 - 底部留白。
                // 必须在 `render` 之前调用 `size`，否则本帧仍按上一帧的高度裁剪，会出现
                // “内容被截断/多余空白”的一帧闪烁
                self.scroll.size((mw, cr.h - h - pad));
                self.scroll.render(ui, |ui| {
                    // 返回内容尺寸：高度额外加 0.04，让正文底部有呼吸空间
                    let r = ui.text(&msg.content).size(0.46).multiline().max_width(mw).draw();
                    (mw, r.h + 0.04)
                });
            }
        });
        // 阶段六：最后叠上场景切换遮罩与全屏加载。
        // 遮罩用游戏时间 `t`，与 `sf.goto(t, ..)`/`sf.next_scene(s.t)` 传入的时钟保持一致，
        // 否则暂停时会算出错误的渐暗进度
        self.sf.render(ui, t);
        if self.chart_task.is_some() {
            // 消息动作触发的谱面正在拉取：盖住整屏并显示转圈，同时（见 `touch`）拒绝一切交互
            ui.full_loading_simple(t);
        }
        Ok(())
    }

    /// 遮罩动画到点后交还待切换的场景（谱面页或用户主页）。
    ///
    /// 用 `s.t` 与 [`SFader::goto`] 的起始时间对齐；正常情况下只有 `Some` 一次，
    /// 之后 `SFader` 内部的目标被取空，返回值恒为 [`prpr::scene::NextScene::None`]。
    fn next_scene(&mut self, s: &mut SharedState) -> prpr::scene::NextScene {
        self.sf.next_scene(s.t).unwrap_or_default()
    }
}
