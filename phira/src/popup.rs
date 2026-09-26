//! 通用下拉/长按菜单。
//!
//! `Popup` 是一个浮动选项列表：支持展开/收起动画、单选、点击外部关闭、可选自动收起；
//! `ChooseButton` 在固定按钮上组合出一个 `Popup`，实现「点击展开下拉选择」。
//! 复用了 prpr 的 `Scroll`（选项过多时可滚动）、`DRectButton`/`RectButton`（命中与按压）、
//! `rounded_rect_shadow`（面板投影）。典型使用场景：曲库排序菜单、谱面长按操作菜单、
//! 单曲页的难度/操作菜单。

use crate::page::Fader;
use macroquad::prelude::*;
use nalgebra::Translation2;
use prpr::{
    core::Matrix,
    ext::{semi_black, semi_white, RectExt},
    ui::{button_hit, rounded_rect_shadow, DRectButton, RectButton, Scroll, ShadowConfig, Ui},
};

/// 浮动选项菜单。
pub struct Popup {
    /// 选项列表滚动容器（选项多时超出面板高度可滚动）。
    scroll: Scroll,
    /// 弹出面板的全局矩形。
    rect: Rect,
    /// 是否处于展开状态。
    showing: bool,
    /// 选项文本与对应命中按钮。
    options: Vec<(String, RectButton)>,
    /// 当前选中索引；`usize::MAX` 表示无选中（首次弹出时不预选任何项）。
    selected: usize,
    /// 文本左边距。
    left: f32,
    /// 文本字号。
    size: f32,
    /// 每行高度。
    height: f32,
    /// 展开/收起补间（默认时长 0.4、位移 0.04）。
    fader: Fader,
    /// 自上次查询以来选中项是否变化（一次性标志）。
    changed: bool,
    /// 选中后是否自动收起；长按菜单等场景可关闭以避免误收起。
    auto_dismiss: bool,
    /// 已决定关闭、待抬手（`Ended`）时才真正收起。
    pending_dismiss: bool,
    /// 若提供，弹出面板会被限制在该矩形内，避免越出屏幕或父容器。
    auto_adjust: Option<Rect>,
}

// 菜单的构造、配置与展示。配置类方法多为 builder 风格或 `set_`，展示/交互在 `show`/`render`/`update`/`touch`。
impl Popup {
    /// 创建菜单：默认无选项、无选中、行高 `0.1`、字号 `0.6`、左内边距 `0.024`，
    /// 展开动画时长 `0.4`、位移 `0.04`，并默认选中后自动收起。
    pub fn new() -> Self {
        Self {
            scroll: Scroll::new(),
            rect: Rect::default(),
            showing: false,
            options: Vec::new(),
            selected: usize::MAX,
            left: 0.024,
            size: 0.6,
            height: 0.1,
            fader: Fader::new().with_time(0.4).with_distance(0.04),
            changed: false,
            auto_dismiss: true,
            pending_dismiss: false,
            auto_adjust: None,
        }
    }

    /// builder：设置选项列表后返回自身。
    #[inline]
    pub fn with_options(mut self, options: Vec<String>) -> Self {
        self.set_options(options);
        self
    }

    /// builder：设置文本字号后返回自身。
    #[inline]
    pub fn with_size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    /// 当前选中索引（`usize::MAX` 表示无选中）。
    #[inline]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// 重建选项列表（同时重置各项命中按钮，丢弃旧的按压状态）。
    #[inline]
    pub fn set_options(&mut self, options: Vec<String>) {
        self.options = options.into_iter().map(|it| (it, RectButton::new())).collect();
    }

    /// 直接设置当前选中索引（不触发 `changed`，用于初始化默认选中项）。
    #[inline]
    pub fn set_selected(&mut self, selected: usize) {
        self.selected = selected;
    }

    /// 设置选中后是否自动收起。
    #[inline]
    pub fn set_auto_dismiss(&mut self, auto_dismiss: bool) {
        self.auto_dismiss = auto_dismiss;
    }

    /// 设置边界矩形，`show` 时据此把面板钳制在范围内。
    #[inline]
    pub fn set_auto_adjust(&mut self, auto_adjust: Option<Rect>) {
        self.auto_adjust = auto_adjust;
    }

    /// 设置展开方向：`bottom` 为 true 时面板向下展开，否则向上。
    /// 实现上是给补间位移取正负号，使淡入方向与视觉展开方向一致。
    pub fn set_bottom(&mut self, bottom: bool) {
        self.fader.distance = self.fader.distance.abs() * if bottom { 1. } else { -1. };
    }

    /// 返回面板当前的全局矩形（供外部做命中判定/定位）。
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// 在给定矩形处弹出面板。
    /// 记录全局矩形；若设置了 `auto_adjust` 则把面板的 x/y 钳制进该区域，保证完整可见；
    /// 随后标记为展开并启动展开补间。
    pub fn show(&mut self, ui: &mut Ui, t: f32, r: Rect) {
        self.rect = ui.rect_to_global(r);
        if let Some(area) = self.auto_adjust {
            self.rect.x = self.rect.x.clamp(area.x, area.right() - self.rect.w);
            self.rect.y = self.rect.y.clamp(area.y, area.bottom() - self.rect.h);
        }
        self.showing = true;
        self.fader.sub(t);
    }

    /// 收起面板并播放退场补间。
    pub fn dismiss(&mut self, t: f32) {
        self.showing = false;
        self.fader.back(t);
    }

    /// 绘制菜单。
    ///
    /// 阶段：① 若既未展开也不在过渡中则直接返回（零开销）；
    /// ② 设定滚动内容尺寸，用 `abs_scope` 摆脱父级变换、按面板绝对坐标定位（弹出层应相对屏幕定位）；
    /// ③ 绘制投影与背景圆角矩形；④ 用 `Scroll` 逐行渲染选项：非首行画分隔线、选中行加深底色、
    /// 文本左对齐垂直居中，并返回内容尺寸 `(宽度, 选项数 × 行高)`。
    /// `alpha` 用于叠加透明度（例如页面本身也在淡入/淡出时同步淡出菜单）。
    pub fn render(&mut self, ui: &mut Ui, t: f32, alpha: f32) {
        if !self.fader.transiting() && !self.showing {
            return;
        }
        let r = self.rect;
        self.scroll.size((r.w, r.h));
        self.fader.reset();
        ui.abs_scope(|ui| {
            ui.dx(r.x);
            ui.dy(r.y);
            self.fader.for_sub(|f| {
                f.render(ui, t, |ui| {
                    let r = Rect::new(0., 0., r.w, r.h);
                    let mut cfg = ShadowConfig {
                        radius: 0.01,
                        elevation: 0.01,
                        ..Default::default()
                    };
                    cfg.base *= alpha;
                    rounded_rect_shadow(ui, r, &cfg);
                    ui.fill_path(&r.rounded(0.01), Color { a: alpha, ..ui.background() });
                    self.scroll.render(ui, |ui| {
                        for (id, (opt, btn)) in self.options.iter_mut().enumerate() {
                            if id != 0 {
                                ui.fill_rect(Rect::new(0.02, -0.001, r.w - 0.04, 0.002), semi_white(0.7 * alpha));
                            }
                            let r = Rect::new(0., 0., r.w, self.height);
                            btn.set(ui, r);
                            let chosen = id == self.selected;
                            if chosen {
                                ui.fill_rect(r.feather(-0.007), semi_black(0.4 * alpha));
                            }
                            ui.text(opt.as_str())
                                .pos(self.left, self.height / 2.)
                                .anchor(0., 0.5)
                                .no_baseline()
                                .size(self.size)
                                .max_width(r.w - self.left * 2.)
                                .color(semi_white(alpha))
                                .draw();
                            ui.dy(self.height);
                        }
                        (r.w, self.options.len() as f32 * self.height)
                    });
                });
            });
        });
    }

    /// 推进滚动与补间。
    ///
    /// 展开时 `Scroll` 内部坐标以面板左上为原点，故先把面板偏移的逆变换临时设入滚动矩阵，
    /// 让触点坐标换算正确，更新后再还原矩阵（避免影响后续绘制）；收起时直接更新。
    /// 最后调用 `fader.done` 推进补间状态。
    pub fn update(&mut self, t: f32) {
        if self.showing {
            let old_matrix = self.scroll.matrix();
            let mut transform = Matrix::identity();
            transform *= Translation2::new(self.rect.x, self.rect.y).to_homogeneous();
            if let Some(inv) = transform.try_inverse() {
                self.scroll.set_matrix(Some(inv));
            }
            self.scroll.update(t);
            self.scroll.set_matrix(old_matrix);
        } else {
            self.scroll.update(t);
        }
        self.fader.done(t);
    }

    /// 处理触摸，返回事件是否被消费。
    ///
    /// 阶段：① 若已 `pending_dismiss`，等到抬手（`Ended`）才真正收起，期间持续消费事件；
    /// ② 展开时：非按下起始（拖动/抬手）或触点位于面板内，先交给 `Scroll` 处理滚动；
    /// ③ 触点位于面板内时逐项命中，命中即选中（若与旧选中不同则置 `changed`），
    /// 并按 `auto_dismiss` 决定是否收起；面板内的空白点击也消费事件；
    /// ④ 触点在面板外且为按下起始时，置 `pending_dismiss` 并消费 —— 实现「点外部关闭」，
    /// 同时避免抬手瞬间穿透到下层控件。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        if self.pending_dismiss {
            if touch.phase == TouchPhase::Ended {
                self.dismiss(t);
                self.pending_dismiss = false;
            }
            return true;
        }
        if self.showing {
            if touch.phase != TouchPhase::Started || self.rect.contains(touch.position) {
                if self.scroll.touch(touch, t) {
                    return true;
                }
                if self.rect.contains(touch.position) {
                    for (id, (_, btn)) in self.options.iter_mut().enumerate() {
                        if btn.touch(touch) {
                            button_hit();
                            if self.selected != id {
                                self.selected = id;
                                self.changed = true;
                            }
                            if self.auto_dismiss {
                                self.dismiss(t);
                            }
                            return true;
                        }
                    }
                    return true;
                }
                false
            } else if touch.phase == TouchPhase::Started {
                self.pending_dismiss = true;
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// 当前是否展开。
    #[inline]
    pub fn showing(&self) -> bool {
        self.showing
    }

    /// 读取并清空「选中项已变化」标志。
    #[inline]
    pub fn changed(&mut self) -> bool {
        if self.changed {
            self.changed = false;
            true
        } else {
            false
        }
    }
}

/// 「按钮 + 下拉面板」组合控件：主按钮显示当前选中项，点击后在其下方弹出选项列表。
pub struct ChooseButton {
    /// 主按钮。
    btn: DRectButton,
    /// 关联的下拉菜单。
    popup: Popup,
    /// 面板宽度；`None` 时跟随按钮宽度。
    width: Option<f32>,
    /// 面板高度。
    height: f32,
    /// 需要在下一帧 `render` 时弹出面板（`show` 需要 `Ui` 上下文，故延迟到渲染阶段执行）。
    need_to_show: bool,
}

// 组合控件的构造与交互。面板的绘制与触摸需由调用方在「顶层」调用（`render_top`/`top_touch`），
// 以保证弹出层覆盖在其它内容之上并优先接收事件。
impl ChooseButton {
    /// 创建组合控件：面板高度默认 `0.34`。
    pub fn new() -> Self {
        Self {
            btn: DRectButton::new(),
            popup: Popup::new(),
            width: None,
            height: 0.34,
            need_to_show: false,
        }
    }

    /// builder：设置下拉选项。
    #[inline]
    pub fn with_options(mut self, options: Vec<String>) -> Self {
        self.popup = self.popup.with_options(options);
        self
    }

    /// builder：设置默认选中项。
    #[inline]
    pub fn with_selected(mut self, selected: usize) -> Self {
        self.popup.selected = selected;
        self
    }

    /// 当前选中索引。
    #[inline]
    pub fn selected(&self) -> usize {
        self.popup.selected
    }

    /// 选中项是否变化（透传 `Popup::changed`）。
    #[inline]
    pub fn changed(&mut self) -> bool {
        self.popup.changed()
    }

    /// 绘制主按钮（显示当前选中项文本）；若 `need_to_show` 则计算面板矩形并弹出。
    /// 面板位于按钮正下方（间隔 `pad`），并左右各加宽 `delta` 使文字更舒展；同时设为向下展开。
    pub fn render(&mut self, ui: &mut Ui, r: Rect, t: f32) {
        self.btn
            .render_text(ui, r, t, &self.popup.options[self.popup.selected].0, self.popup.size, false);
        if self.need_to_show {
            let pad = 0.007;
            let mut rr = Rect::new(r.x, r.bottom() + pad, self.width.unwrap_or(r.w), self.height);
            let delta = 0.1;
            rr.x -= delta;
            rr.w += delta;
            self.popup.set_bottom(true);
            self.popup.show(ui, t, rr);
            self.need_to_show = false;
        }
    }

    /// 在顶层绘制面板（须在页面其它内容之后调用）。
    #[inline]
    pub fn render_top(&mut self, ui: &mut Ui, t: f32, alpha: f32) {
        self.popup.render(ui, t, alpha);
    }

    /// 推进面板的滚动与补间。
    pub fn update(&mut self, t: f32) {
        self.popup.update(t);
    }

    /// 顶层的触摸拦截：展开时先交给面板；若面板未消费但触点落在面板矩形内，也算已消费
    /// （避免点到面板空白处穿透到下层）；未展开但仍在退场动画中时同样消费，防止动画残留期间的误点。
    pub fn top_touch(&mut self, touch: &Touch, t: f32) -> bool {
        if self.popup.showing() {
            if self.popup.touch(touch, t) {
                return true;
            }
            self.popup.rect.contains(touch.position)
        } else {
            self.popup.fader.transiting()
        }
    }

    /// 主按钮自身所在层级的触摸：命中则请求弹出面板（实际弹出延迟到下一次 `render`）。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        if self.btn.touch(touch, t) {
            self.need_to_show = true;
            true
        } else {
            false
        }
    }
}
