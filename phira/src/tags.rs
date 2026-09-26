//! 谱面标签系统。
//!
//! 标签分两类：**分区标签**（`DIVISION_TAGS`，互斥，表示谱面归类）与**自定义标签**
//! （用户自由增删，用于检索）。`Tags` 管理一组自定义标签的增删与自动换行布局；
//! `TagsDialog` 是编辑/筛选对话框：编辑模式下确定后写回谱面元信息，
//! 筛选模式下再加上「不想要的标签」与若干开关，结果经 `confirmed` 等公开字段回传给页面。
//! 对话框与页面之间不靠返回值传递，而是页面每帧读取其公开字段来推进流程。

prpr_l10n::tl_file!("tags");

use crate::{client::Permissions, page::Fader};
use inputbox::InputBox;
use macroquad::prelude::*;
use prpr::{
    core::BOLD_FONT,
    ext::{semi_black, RectExt},
    scene::{request_input, return_input, show_message, take_input},
    ui::{DRectButton, Scroll, Ui},
};
use smallvec::{smallvec, SmallVec};

/// 分区标签集合。分区是互斥的「谱面类别」，与普通标签分开存储；
/// 索引 0 同时作为默认分区。这些名字不允许再作为自定义标签添加（见 `Tags::add`）。
const DIVISION_TAGS: &[&str] = &["regular", "troll", "plain", "visual"];

/// 一组自定义标签的编辑状态：文本、命中按钮与「新增」按钮一一对应。
pub struct Tags {
    /// 输入框请求 id：同一帧可能存在多个标签区（如「想要」与「不想要」），用它区分回传的输入。
    input_id: &'static str,
    /// 当前标签文本。
    tags: Vec<String>,
    /// 与 `tags` 逐项对应的命中按钮（删除用），始终保持与 `tags` 等长。
    btns: Vec<DRectButton>,
    /// 「+」新增按钮。
    add: DRectButton,
}

// 标签集的增删与布局。核心不变量：`tags` 与 `btns` 长度始终一致，任何增删都成对进行。
impl Tags {
    /// 创建空标签集；`input_id` 用于向全局输入系统请求文本输入框。
    pub fn new(input_id: &'static str) -> Self {
        Self {
            input_id,
            tags: Vec::new(),
            btns: Vec::new(),
            add: DRectButton::new(),
        }
    }

    /// 返回当前标签切片。
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// 追加一个标签：先去除首尾空白；分区标签不允许作为自定义标签（直接忽略）；
    /// 同时 push 一个按钮以维持与 `tags` 的长度一致。
    pub fn add(&mut self, s: String) {
        let s = s.trim().to_owned();
        if DIVISION_TAGS.contains(&s.as_str()) {
            return;
        }
        self.tags.push(s);
        self.btns.push(DRectButton::new());
    }

    /// 用给定列表重建标签，并返回其中解析出的分区标签。
    ///
    /// 语义：传入的列表中若含分区标签，则把它从自定义标签中剔除，并作为「当前分区」返回；
    /// 若没有任何分区标签则回退到 `DIVISION_TAGS[0]`（默认分区）。
    /// 同时重建按钮列表以对齐新的标签数量。
    ///
    /// # Returns
    /// 解析出的分区标签（`&'static str`）。
    pub fn set(&mut self, tags: Vec<String>) -> &'static str {
        let mut div = DIVISION_TAGS[0];
        let tags: Vec<_> = tags
            .into_iter()
            .map(|it| it.trim().to_owned())
            .filter(|it| {
                if let Some(division) = DIVISION_TAGS.iter().find(|div| *div == it) {
                    div = division;
                    false
                } else {
                    true
                }
            })
            .collect();
        self.btns = vec![DRectButton::new(); tags.len()];
        self.tags = tags;
        div
    }

    /// 命中检测：点到某个标签按钮则删除该标签（文本与按钮同步删除）并返回 true；
    /// 否则点到「+」则请求输入框（用户输入经 `TagsDialog::update` 回传到这里）。
    /// 返回事件是否被消费。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        for (index, btn) in self.btns.iter_mut().enumerate() {
            if btn.touch(touch, t) {
                self.tags.remove(index);
                self.btns.remove(index);
                return true;
            }
        }
        if self.add.touch(touch, t) {
            request_input(self.input_id, InputBox::new());
            return true;
        }
        false
    }

    /// 绘制标签的流式布局（自动换行）并返回整体高度。
    ///
    /// 布局规则：每个标签宽度按文本实测宽度并夹在 `[0.08, tmw]` 之间（过短不至于太小、过长截断），
    /// 放下一个标签若超出可用宽度 `mw` 则换行（`x` 归零、行高累加）；每个标签四周留
    /// `margin + pad` 的外扩内边距。最后把「+」按钮也当作一个标签绘制。
    /// 返回的 `h + row_height` 供对话框计算滚动内容总高。
    pub fn render(&mut self, ui: &mut Ui, mw: f32, t: f32) -> f32 {
        let row_height = 0.1;
        let tmw = 0.3;
        let sz = 0.5;
        let margin = 0.03;
        let pad = 0.01;

        let mut h = 0.;
        let mut x = 0.;
        let mut draw = |btn: &mut DRectButton, text: &str| {
            let w = ui.text(text).size(sz).measure().w.clamp(0.08, tmw);
            if x + w + (margin + pad) * 2. > mw {
                x = 0.;
                h += row_height;
            }
            btn.render_text(ui, Rect::new(x, h, w + (margin + pad) * 2., row_height).feather(-pad), t, text, sz, true);
            x += w + (margin + pad) * 2.;
        };
        for (tag, btn) in self.tags.iter().zip(self.btns.iter_mut()) {
            draw(btn, tag);
        }
        draw(&mut self.add, "+");
        h + row_height
    }

    /// 校验并添加用户输入的标签：只允许字母数字与连字符 `-`（否则弹出 `invalid-tag` 错误提示），
    /// 且与已有标签去重后才真正加入。
    pub fn try_add(&mut self, s: &str) {
        if !s.chars().all(|it| it == '-' || it.is_alphanumeric()) {
            show_message(tl!("invalid-tag")).error();
            return;
        }
        if self.tags.iter().all(|it| it != s) {
            self.add(s.into());
        }
    }
}

/// 标签编辑/筛选对话框。
///
/// 两种模式由 `unwanted` 是否为 `Some` 决定：`None` 为编辑模式（给谱面增删标签+选分区），
/// `Some` 为筛选模式（增加「不想要的标签」与「只看我的/未审核/稳定请求」等开关）。
/// 结果通过公开字段回传给页面：`confirmed` 表示确认/取消，`show_rating` 请求打开评分筛选。
pub struct TagsDialog {
    /// 弹窗入场/退场补间。
    fader: Fader,
    /// 是否处于显示（前进）状态。
    show: bool,

    /// 内容滚动容器（标签过多/开关较多时滚动）。
    scroll: Scroll,
    /// 「想要」标签编辑区。
    pub tags: Tags,
    /// 「不想要的」标签编辑区；`Some` 表示处于筛选模式。
    pub unwanted: Option<Tags>,

    /// 当前选中的分区标签。
    pub division: &'static str,
    /// 分区按钮（与 `DIVISION_TAGS` 一一对应）。
    div_btns: Vec<DRectButton>,

    /// 「只看我的」筛选开关按钮。
    pub btn_me: DRectButton,
    /// 该开关是否开启。
    pub show_me: bool,
    /// 「只看未审核」筛选开关按钮。
    pub btn_unreviewed: DRectButton,
    /// 该开关是否开启。
    pub show_unreviewed: bool,
    /// 「只看稳定请求」筛选开关按钮。
    pub btn_stabilize: DRectButton,
    /// 该开关是否开启。
    pub show_stabilize: bool,
    /// 当前用户权限，决定是否展示「未审核/稳定请求」筛选项。
    pub perms: Permissions,

    /// 取消按钮。
    btn_cancel: DRectButton,
    /// 确认按钮。
    btn_confirm: DRectButton,
    /// 「按评分筛选」按钮（筛选模式下替代取消/确认）。
    btn_rating: DRectButton,
    /// 结果回传：`Some(true)` = 确认，`Some(false)` = 取消，`None` = 尚无结果。
    pub confirmed: Option<bool>,
    /// 请求打开评分对话框（调用方读取后据此弹出 `RateDialog`）。
    pub show_rating: bool,
}

// 对话框的构造与交互。对话框不返回结果，而是把状态写进公开字段，由页面每帧读取；
// 触摸事件在对话框内被完全消费（返回 true），避免穿透到底层页面。
impl TagsDialog {
    /// 创建对话框。`search_mode` 为 true 时启用筛选模式（多出 `unwanted` 标签区与筛选项）。
    /// 补间距离 `-0.4`、时长 `0.5`，即自下方较缓地滑入。
    pub fn new(search_mode: bool) -> Self {
        Self {
            fader: Fader::new().with_distance(-0.4).with_time(0.5),
            show: false,

            scroll: Scroll::new(),
            tags: Tags::new("add_tag"),
            unwanted: if search_mode { Some(Tags::new("add_tag_unwanted")) } else { None },

            division: DIVISION_TAGS[0],
            div_btns: DIVISION_TAGS.iter().map(|_| DRectButton::new()).collect(),

            btn_me: DRectButton::new(),
            show_me: false,
            btn_unreviewed: DRectButton::new(),
            show_unreviewed: false,
            btn_stabilize: DRectButton::new(),
            show_stabilize: false,
            perms: Permissions::empty(),

            btn_cancel: DRectButton::new(),
            btn_confirm: DRectButton::new(),
            btn_rating: DRectButton::new(),
            confirmed: None,
            show_rating: false,
        }
    }

    /// 用给定标签列表初始化内容：标签进 `tags`，分区经解析写回 `division`。
    pub fn set(&mut self, tags: Vec<String>) {
        self.division = self.tags.set(tags);
    }

    /// 当前是否可见。
    pub fn showing(&self) -> bool {
        self.show
    }

    /// 进入显示：启动入场补间。
    pub fn enter(&mut self, t: f32) {
        self.fader.sub(t);
    }

    /// 收起：标记不显示并播放退场补间。
    pub fn dismiss(&mut self, t: f32) {
        self.show = false;
        self.fader.back(t);
    }

    /// 对话框矩形。筛选模式内容更多，故底部 `feather` 较小以加高对话框。
    fn dialog_rect(&self) -> Rect {
        if self.unwanted.is_some() {
            Ui::dialog_rect().nonuniform_feather(0.04, 0.05)
        } else {
            Ui::dialog_rect()
        }
    }

    /// 处理触摸，返回事件是否被消费（对话框展开时恒为 true，以免穿透到底层）。
    ///
    /// 阶段：① 过渡动画期间吞掉输入；② 触点落在对话框外且为按下起始则关闭；
    /// ③ 依次把事件交给滚动区、两个标签区、分区按钮与各开关按钮，
    /// 命中分区/标签/开关时都会 `halt` 滚动惯性（避免误触发后列表仍在滑动）；
    /// ④ 取消/确认写回 `confirmed`，评分按钮置 `show_rating` 并关闭（跳转到评分筛选）。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        if self.fader.transiting() {
            return true;
        }
        if self.show {
            if !self.dialog_rect().contains(touch.position) && touch.phase == TouchPhase::Started {
                self.dismiss(t);
                return true;
            }
            if self.scroll.touch(touch, t) {
                return true;
            }
            if self.tags.touch(touch, t) {
                self.scroll.y_scroller.halt();
                return true;
            }
            if let Some(unwanted) = &mut self.unwanted {
                if unwanted.touch(touch, t) {
                    self.scroll.y_scroller.halt();
                    return true;
                }
            }
            for (div, btn) in DIVISION_TAGS.iter().zip(&mut self.div_btns) {
                if btn.touch(touch, t) {
                    self.scroll.y_scroller.halt();
                    self.division = div;
                }
            }
            if self.btn_me.touch(touch, t) {
                self.show_me ^= true;
                return true;
            }
            if self.btn_unreviewed.touch(touch, t) {
                self.show_unreviewed ^= true;
                return true;
            }
            if self.btn_stabilize.touch(touch, t) {
                self.show_stabilize ^= true;
                return true;
            }
            if self.btn_cancel.touch(touch, t) {
                self.confirmed = Some(false);
                self.dismiss(t);
                return true;
            }
            if self.btn_confirm.touch(touch, t) {
                self.confirmed = Some(true);
                self.dismiss(t);
                return true;
            }
            if self.btn_rating.touch(touch, t) {
                self.show_rating = true;
                self.dismiss(t);
                return true;
            }
            return true;
        }
        false
    }

    /// 推进滚动与补间，并处理输入框回传。
    ///
    /// 补间完成后按结果切换 `show`（使退场动画期间仍可见）；标签输入按 `input_id` 分派到
    /// `tags` 或 `unwanted`，若 id 不匹配（例如属于其它组件的输入框）则原样 `return_input` 交还，
    /// 避免吞掉不属于本对话框的输入。
    pub fn update(&mut self, t: f32) {
        if let Some(done) = self.fader.done(t) {
            self.show = !done;
        }
        self.scroll.update(t);
        if let Some((id, text)) = take_input() {
            match id.as_str() {
                "add_tag" => {
                    self.tags.try_add(text.trim());
                }
                "add_tag_unwanted" => {
                    self.unwanted.as_mut().unwrap().try_add(text.trim());
                }
                _ => {
                    return_input(id, text);
                }
            }
        }
    }

    /// 绘制对话框。
    ///
    /// 阶段：① 全屏半透明遮罩 + 对话框背板，标题按模式显示 `filter`/`edit`；
    /// ② 用 `Scroll` 承载内容，依次绘制：分区按钮行、筛选按钮行（每个筛选按钮按 `perms`
    /// 决定是否展示，再按可见数量等分宽度）、「想要」标签区；
    /// ③ 筛选模式下追加「不想要的」标题与标签区；④ 底部按钮：编辑模式为取消+确认，
    /// 筛选模式为「按评分筛选」。滚动内容返回累计高度 `h` 供滚动容器使用。
    pub fn render(&mut self, ui: &mut Ui, t: f32) {
        self.fader.reset();
        if self.show || self.fader.transiting() {
            let p = if self.show { 1. } else { -self.fader.progress(t) };
            ui.fill_rect(ui.screen_rect(), semi_black(p * 0.7));
            let wr = self.dialog_rect();
            self.fader.for_sub(|f| {
                f.render(ui, t, |ui| {
                    ui.fill_path(&wr.rounded(0.02), ui.background());
                    let r = ui
                        .text(if self.unwanted.is_some() { tl!("filter") } else { tl!("edit") })
                        .pos(wr.x + 0.04, wr.y + 0.033)
                        .size(0.9)
                        .draw_using(&BOLD_FONT);
                    let mw = wr.w - 0.08;
                    let bh = 0.09;
                    ui.scope(|ui| {
                        ui.dx(r.x);
                        ui.dy(r.bottom() + 0.02);
                        self.scroll.size((mw, wr.bottom() - r.bottom() - 0.06 - bh));
                        self.scroll.render(ui, |ui| {
                            let pad = 0.015;
                            let bw = mw / DIVISION_TAGS.len() as f32;
                            let mut r = Rect::new(pad / 2., 0., bw, bh).nonuniform_feather(-0.01, -0.004);
                            for (div, btn) in DIVISION_TAGS.iter().zip(&mut self.div_btns) {
                                btn.render_text(ui, r, t, tl!(*div), 0.5, self.division == *div);
                                r.x += bw;
                            }
                            let mut h = bh + 0.01;
                            ui.dy(h);
                            if self.unwanted.is_some() {
                                let mut row: SmallVec<[_; 3]> = smallvec![(&mut self.btn_me, "filter-me", self.show_me)];
                                if self.perms.contains(Permissions::SEE_UNREVIEWED) {
                                    row.push((&mut self.btn_unreviewed, "filter-unreviewed", self.show_unreviewed));
                                }
                                if self.perms.contains(Permissions::SEE_STABLE_REQ) {
                                    row.push((&mut self.btn_stabilize, "filter-stabilize", self.show_stabilize));
                                }
                                let bw = mw / row.len() as f32;
                                let mut r = Rect::new(pad / 2., 0., bw, bh).nonuniform_feather(-0.01, -0.004);
                                for (btn, text, on) in row.into_iter() {
                                    btn.render_text(ui, r, t, tl!(text), 0.5, on);
                                    r.x += bw;
                                }
                                let dh = bh + 0.01;
                                h += dh;
                                ui.dy(dh);
                            }
                            if self.unwanted.is_some() {
                                let th = ui.text(tl!("wanted")).size(0.5).draw().h + 0.01;
                                ui.dy(th);
                                h += th;
                            }
                            let th = self.tags.render(ui, mw, t);
                            ui.dy(th);
                            h += th;
                            if let Some(unwanted) = &mut self.unwanted {
                                ui.dy(0.02);
                                h += 0.02;
                                let th = ui.text(tl!("unwanted")).size(0.5).draw().h + 0.01;
                                ui.dy(th);
                                h += th;
                                h += unwanted.render(ui, mw, t);
                            }
                            (mw, h)
                        });
                    });
                    let pad = 0.02;
                    if self.unwanted.is_none() {
                        let bw = (wr.w - pad * 3.) / 2.;
                        let mut r = Rect::new(wr.x + pad, wr.bottom() - 0.02 - bh, bw, bh);
                        self.btn_cancel.render_text(ui, r, t, tl!("cancel"), 0.5, true);
                        r.x += bw + pad;
                        self.btn_confirm.render_text(ui, r, t, tl!("confirm"), 0.5, true);
                    } else {
                        let r = Rect::new(wr.x, wr.bottom() - 0.02 - bh, wr.w, bh).nonuniform_feather(-pad, 0.);
                        self.btn_rating.render_text(ui, r, t, tl!("filter-by-rating"), 0.5, true);
                    }
                });
            });
        }
    }
}
