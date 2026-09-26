//! 难度评分控件（五星半点）及其对话框。
//!
//! `Rate` 是可拖动取值的五角星控件，粒度到半星；`RateDialog` 是承载它的弹窗，
//! 既用于给谱面打分，也用于按「评分区间」筛选谱面（此时显示上下两个 `Rate`）。

prpr_l10n::tl_file!("rate");

use crate::page::Fader;
use macroquad::prelude::*;
use prpr::{
    core::BOLD_FONT,
    ext::{semi_black, semi_white, RectExt, SafeTexture, ScaleType},
    ui::{DRectButton, Ui},
};

/// 五星半点评分控件。
///
/// 内部以整数 `score` 表示评分，`0` 表示「未评分」，每 `1` 点对应半颗星（满 5 星 = 10 点），
/// 因此取值域为 `0..=10`。
pub struct Rate {
    /// 当前评分：0 = 未评分，1 = 半星…10 = 五星。
    pub score: i16,

    /// 当前拖动触点的 x 坐标；`None` 表示未处于拖动中。
    touch_x: Option<f32>,
    /// 控件可拖动区域（全局坐标）；由上一帧 `render` 写入，供下一帧 `touch` 做命中判定。
    touch_rect: Rect,
}

// 评分控件的交互。`render` 与 `touch` 通过 `touch_rect` 形成「上一帧记录区域、下一帧判定命中」的约定。
impl Rate {
    /// 构造一个未评分（`score = 0`）、未拖动的控件。
    pub fn new() -> Self {
        Self {
            score: 0,

            touch_x: None,
            touch_rect: Rect::default(),
        }
    }

    /// 处理触摸：只有在「已在拖动中」或「触点落在控件区内」时才更新状态。
    /// 抬手/取消时清空 `touch_x` 结束拖动，否则持续记录当前 x —— 一旦按下便跟随手指，
    /// 即使手指移出控件区也不会中断拖动（这是拖动评分控件应有的行为）。
    /// 注意命中判定依赖上一帧 `render` 写入的 `touch_rect`。
    pub fn touch(&mut self, touch: &Touch) {
        if self.touch_x.is_some() || self.touch_rect.contains(touch.position) {
            if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.touch_x = None;
            } else {
                self.touch_x = Some(touch.position.x);
            }
        }
    }

    /// 绘制五颗星，并根据拖动位置更新评分，最后返回控件矩形。
    ///
    /// 取值算法：把触点 x 换算成控件内的水平偏移 `rw`，以每颗星 `pad + s` 为步长取整得到
    /// 整星序号（每星 2 点），再按残差 `rem` 分档补半星——越过 `pad/2` 加半点、越过
    /// `pad + s/2` 再加半点，于是取值总落在最近的半星刻度上；末了 `clamp(0, 10)` 防止越界。
    ///
    /// 绘制：`score` 足够时画实心星；否则画半透明星，并在 `score` 为奇数（半星）时
    /// 于该星左半区域再叠画一颗实心星，用「左半亮」表达半星状态。
    ///
    /// # Returns
    /// 控件的全局矩形 `touch_rect`，供后续布局（如上下限控件间距）与命中判定复用。
    pub fn render(&mut self, ui: &mut Ui, icon_star: &SafeTexture) -> Rect {
        let wr = Ui::dialog_rect();
        ui.scope(|ui| {
            ui.dx(wr.center().x);
            let s = 0.1;
            let pad = 0.03;
            let cc = semi_white(0.5);
            let tw = s * 2.5 + pad * 2.;
            self.touch_rect = ui.rect_to_global(Rect::new(-tw, s / 2., tw * 2., s));
            if let Some(x) = self.touch_x {
                let rw = (x - self.touch_rect.x) / self.touch_rect.w * tw * 2. + pad;
                let index = (rw / (pad + s)) as i16;
                let rem = rw - index as f32 * (pad + s);
                self.score = index * 2;
                if rem > pad / 2. {
                    self.score += 1;
                    if rem > pad + s / 2. {
                        self.score += 1;
                    }
                }
                self.score = self.score.clamp(0, 10);
            }
            for i in 0..5 {
                let pos = (i as f32 - 2.) * (pad + s);
                let r = Rect::new(pos, s / 2., 0., 0.).feather(s / 2.);
                if self.score >= (i + 1) * 2 {
                    ui.fill_rect(r, (**icon_star, r, ScaleType::Fit));
                } else {
                    ui.fill_rect(r, (**icon_star, r, ScaleType::Fit, cc));
                    if self.score == i * 2 + 1 {
                        let hr = Rect { w: r.w / 2., ..r };
                        ui.fill_rect(hr, (**icon_star, r, ScaleType::Fit));
                    }
                }
            }
        });
        self.touch_rect
    }
}

/// 评分对话框，同时支持两种模式：
/// - 普通评分：单个 `Rate`（`rate_upper == None`），给谱面打分；
/// - 区间筛选：上下两个 `Rate`（`rate_upper == Some`），`rate` 作下限、`rate_upper` 作上限。
pub struct RateDialog {
    /// 弹窗的入场/退场补间。
    fader: Fader,
    /// 是否处于显示（前进）状态。
    show: bool,

    /// 星星图标。
    icon_star: SafeTexture,

    /// 取消按钮。
    btn_cancel: DRectButton,
    /// 确认按钮。
    btn_confirm: DRectButton,
    /// 「按标签筛选」按钮（筛选模式下替代取消/确认）。
    btn_tags: DRectButton,
    /// 结果回传：`Some(true)` = 确认，`Some(false)` = 取消，`None` = 尚无结果。
    pub confirmed: Option<bool>,
    /// 请求切换到标签筛选（调用方读取后据此弹出 `TagsDialog`）。
    pub show_tags: bool,

    /// 评分（普通模式）或评分下限（区间模式）。
    pub rate: Rate,
    /// 评分上限；`Some` 表示当前处于「按评分区间筛选」模式。
    pub rate_upper: Option<Rate>,
}

// 对话框的构造与交互。结果通过 `confirmed`/`show_tags` 字段回传给页面，
// 由页面在每帧 update 后读取并据此推进流程（而非直接把返回值抛给调用点）。
impl RateDialog {
    /// 构造对话框。`range` 为 true 时启用双控件（区间筛选模式）。
    /// 补间距离为 `-0.4`、时长为 `0.5`，即对话框自下方稍远处较缓地滑入。
    pub fn new(icon_star: SafeTexture, range: bool) -> Self {
        Self {
            fader: Fader::new().with_distance(-0.4).with_time(0.5),
            show: false,

            icon_star,

            btn_cancel: DRectButton::new(),
            btn_confirm: DRectButton::new(),
            btn_tags: DRectButton::new(),
            confirmed: None,
            show_tags: false,

            rate: Rate::new(),
            rate_upper: if range { Some(Rate::new()) } else { None },
        }
    }

    /// 当前是否可见（含退场期间的判断见 `update`）。
    pub fn showing(&self) -> bool {
        self.show
    }

    /// 进入显示：启动入场补间。
    pub fn enter(&mut self, t: f32) {
        self.fader.sub(t);
    }

    /// 对话框矩形。区间模式需要容纳两行评分，故底部 `feather` 较小以加高对话框。
    fn dialog_rect(&self) -> Rect {
        Ui::dialog_rect().nonuniform_feather(0., if self.rate_upper.is_some() { -0.02 } else { -0.1 })
    }

    /// 收起：标记为不显示并播放退场补间。
    pub fn dismiss(&mut self, t: f32) {
        self.show = false;
        self.fader.back(t);
    }

    /// 处理触摸，返回事件是否被消费。
    ///
    /// 过渡动画期间一律吞掉输入（避免按钮位置尚在移动时被误点）；显示时按顺序判定：
    /// 点在对话框外且为按下起始则关闭；取消按钮置 `confirmed = Some(false)` 并关闭；
    /// 确认按钮仅当 `score != 0`（已评分）才产生 `Some(true)`；标签按钮置 `show_tags` 并关闭；
    /// 其余事件转发给两个 `Rate` 以供拖动取值。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        if self.fader.transiting() {
            return true;
        }
        if self.show {
            if !self.dialog_rect().contains(touch.position) && touch.phase == TouchPhase::Started {
                self.dismiss(t);
                return true;
            }
            if self.btn_cancel.touch(touch, t) {
                self.confirmed = Some(false);
                self.dismiss(t);
                return true;
            }
            if self.btn_confirm.touch(touch, t) {
                if self.rate.score != 0 {
                    self.confirmed = Some(true);
                }
                return true;
            }
            if self.btn_tags.touch(touch, t) {
                self.show_tags = true;
                self.dismiss(t);
                return true;
            }
            self.rate.touch(touch);
            if let Some(upper) = &mut self.rate_upper {
                upper.touch(touch);
            }
            return true;
        }
        false
    }

    /// 推进补间；补间完成后依据结果切换 `show`，使退场动画期间仍保持可见。
    pub fn update(&mut self, t: f32) {
        if let Some(done) = self.fader.done(t) {
            self.show = !done;
        }
    }

    /// 绘制对话框。
    ///
    /// 阶段：① 全屏半透明遮罩 + 对话框背板；② 标题按模式显示 `filter` / `rate`；
    /// ③ 区间模式下先画「下界」标签与下限控件，再画「上界」标签与上限控件，并在两者间
    /// 互相钳制（上限不低于下限、下限不高于上限），保证区间始终合法；
    /// ④ 底部按钮：普通模式为取消 + 确认，区间模式为「按标签筛选」。
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
                        .text(if self.rate_upper.is_some() { tl!("filter") } else { tl!("rate") })
                        .pos(wr.x + 0.04, wr.y + 0.033)
                        .size(0.9)
                        .draw_using(&BOLD_FONT);
                    let bh = 0.09;
                    ui.scope(|ui| {
                        ui.dy(r.bottom() + 0.04);
                        if self.rate_upper.is_some() {
                            let h = ui.text(tl!("lower-bound")).pos(wr.center().x, 0.).anchor(0.5, 0.).size(0.5).draw().h;
                            ui.dy(h + 0.02);
                        } else {
                            ui.dy(0.03);
                        }
                        let h = self.rate.render(ui, &self.icon_star).h;
                        if let Some(upper) = &mut self.rate_upper {
                            upper.score = upper.score.max(self.rate.score);
                        }
                        ui.dy(h + 0.03);
                        if let Some(upper) = &mut self.rate_upper {
                            let h = ui.text(tl!("upper-bound")).pos(wr.center().x, 0.).anchor(0.5, 0.).size(0.5).draw().h;
                            ui.dy(h + 0.02);
                            upper.render(ui, &self.icon_star);
                            self.rate.score = self.rate.score.min(upper.score);
                        }
                    });
                    let pad = 0.02;
                    if self.rate_upper.is_none() {
                        let bw = (wr.w - pad * 3.) / 2.;
                        let mut r = Rect::new(wr.x + pad, wr.bottom() - 0.02 - bh, bw, bh);
                        self.btn_cancel.render_text(ui, r, t, tl!("cancel"), 0.5, true);
                        r.x += bw + pad;
                        self.btn_confirm.render_text(ui, r, t, tl!("confirm"), 0.5, true);
                    } else {
                        let r = Rect::new(wr.x, wr.bottom() - 0.02 - bh, wr.w, bh).nonuniform_feather(-pad, 0.);
                        self.btn_tags.render_text(ui, r, t, tl!("filter-by-tags"), 0.5, true);
                    }
                });
            });
        }
        // TODO magical. removing this line will make the title disappear.
        ui.text("").draw_using(&BOLD_FONT);
    }
}
