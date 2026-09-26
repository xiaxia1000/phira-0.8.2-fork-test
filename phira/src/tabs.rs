//! 左侧竖排选项卡组件（设置页与曲库页使用）。
//!
//! 视觉上是一个贴左的窄条，选中项用一块上下滑动的白色高亮条（反色显示黑字）表示，
//! 右侧内容区在切换时做交叉淡入淡出 + 轻微纵向滑动。高亮条由上下两条边（各自独立的
//! `Anim<f32>`）插值而成，切换方向不同会给两边分配不同时长以制造轻微的拉伸/收回手感。

use crate::{anim::Anim, get_data, Result};
use macroquad::prelude::*;
use prpr::{
    ext::{semi_black, RectExt},
    ui::{button_hit, rounded_rect_shadow, RectButton, ShadowConfig, Ui},
};
use std::borrow::Cow;

/// 选项卡标题工厂。返回函数指针（而非闭包），使标题在每次渲染时按当前语言重新求值
/// （语言切换后无需重建 `Tabs`），同时避免给泛型 `T` 增加额外约束。
pub type TitleFn = fn() -> Cow<'static, str>;

/// 单个选项卡：业务值 + 标题工厂 + 命中按钮。
struct TabItem<T> {
    /// 该选项卡承载的值，由外部通过 `selected`/`iter_mut` 访问。
    value: T,
    /// 标题文本工厂，每帧调用以支持动态语言。
    title: TitleFn,
    /// 命中区域按钮。
    btn: RectButton,
}

/// 竖排选项卡容器：持有若干个 `TabItem` 以及切换动画状态。
pub struct Tabs<T> {
    /// 全部选项卡。
    items: Vec<TabItem<T>>,
    /// 当前选中项索引。
    selected: usize,

    /// 高亮条上边的 y 坐标（补间）。
    y_upper: Anim<f32>,
    /// 高亮条下边的 y 坐标（补间）；上下边独立动画，故高亮条在切换时可短暂拉伸。
    y_lower: Anim<f32>,

    /// 内容区切换进度（0=旧内容，1=新内容），用于交叉淡入淡出。
    content_progress: Anim<f32>,
    /// 上次切换是否为「向下」（索引增大）；决定内容进出场的滑动方向。
    prev_go_up: bool,
    /// 上次选中的索引，切换期间用于继续绘制正在淡出的旧内容。
    prev: usize,

    /// 自上次查询以来选中项是否变化（一次性标志）。
    changed: bool,
}

// 选项卡布局常量与切换动画参数，以及构造/访问/切换/绘制方法。
// 注意高亮条的目标位置并不在此处预设，而是在绘制时由选中项的实际行位置决定（见 `render_plain`）。
impl<T> Tabs<T> {
    /// 选项卡条左边距（相对屏幕，负值表示贴左）。
    const LEFT: f32 = -0.94;
    /// 选项卡条宽度。
    const WIDTH: f32 = 0.2;
    /// 高亮条上下边的补间时长（上边, 下边）。切换时按方向交换使用，
    /// 让靠前的一边先到位、另一边稍慢，形成细微的「拉伸-收回」弹性观感。
    const DURATIONS: (f32, f32) = (0.24, 0.35);
    /// 内容切换时的纵向位移幅度：旧内容与新内容朝相反方向偏移该距离，叠加淡入淡出。
    const CONTENT_DY: f32 = 0.06;
    /// 内容交叉淡入淡出的时长。
    const CONTENT_DURATION: f32 = 0.4;

    /// 构造选项卡：默认选中第 0 项；`content_progress` 初值为 1 表示初始不做切换动画；
    /// 两条指示条边补间初值为 0（首帧渲染会立即被 `alter_to` 对齐到实际布局）。
    pub fn new(items: impl IntoIterator<Item = (T, TitleFn)>) -> Self {
        Tabs {
            items: items
                .into_iter()
                .map(|(value, title)| TabItem {
                    value,
                    title,
                    btn: RectButton::new(),
                })
                .collect(),
            selected: 0,

            y_upper: Anim::new(0.),
            y_lower: Anim::new(0.),

            content_progress: Anim::new(1.),
            prev_go_up: false,
            prev: 0,

            changed: false,
        }
    }

    /// 返回当前选中项的值（不可变）。
    pub fn selected(&self) -> &T {
        &self.items[self.selected].value
    }

    /// 返回当前选中项的值（可变）。
    pub fn selected_mut(&mut self) -> &mut T {
        &mut self.items[self.selected].value
    }

    /// 可变遍历所有选项卡的值（例如批量刷新某项状态而不改变选中项）。
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.items.iter_mut().map(|item| &mut item.value)
    }

    /// 读取并清空「选中项已变化」标志，供调用方在变更时重建内容。
    pub fn changed(&mut self) -> bool {
        let changed = self.changed;
        self.changed = false;
        changed
    }

    /// 切换到 `index` 项并启动切换动画。
    ///
    /// - 若与当前项相同则直接返回（幂等，避免重复触发动画）；
    /// - 开启「减少动态效果」时把两条边时长都置 0（瞬移）；否则使用 `DURATIONS`；
    /// - 按切换方向交换上下边时长并记录 `prev_go_up`，使滑动方向与位移方向一致；
    /// - 用 `begin`（以当前值为起点）驱动指示条，用 `start` 把内容进度从 0 推到 1，
    ///   即指示条从当前位置续接、内容从零开始交叉淡入；
    /// - 最后置 `changed = true` 通知调用方内容需重建。
    pub fn goto(&mut self, t: f32, index: usize) {
        if index == self.selected {
            return;
        }

        let (mut upper, mut lower) = if get_data().prefer_reduced_motion { (0., 0.) } else { Self::DURATIONS };
        if index > self.selected {
            std::mem::swap(&mut upper, &mut lower);
            self.prev_go_up = true;
        } else {
            self.prev_go_up = false;
        }

        self.prev = self.selected;
        self.selected = index;
        self.y_upper.begin(t, upper);
        self.y_lower.begin(t, lower);
        self.content_progress.start(0., 1., t, Self::CONTENT_DURATION);

        self.changed = true;
    }

    /// 命中检测：点到任一选项卡则切换（并播放点击音效），返回事件是否被消费。
    /// 命中区域来自上一帧 `render_plain` 写入的矩形，故必须先渲染再触摸。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        for (index, item) in self.items.iter_mut().enumerate() {
            if item.btn.touch(touch) {
                button_hit();
                self.goto(t, index);
                return true;
            }
        }

        false
    }

    /// 绘制一「层」选项卡（同一位置绘制两遍以实现反色效果）：
    /// 先按固定行高自上而下排布各项并更新命中区；绘制到选中项时，把高亮条上下边的目标
    /// 位置 `alter_to` 到该行的顶/底——即高亮条位置由实际布局驱动，而非另行预计算。
    /// `first` 为 true 时是底层（白字 + 半透明底），false 时是高亮条内的黑字层。
    fn render_plain(&mut self, ui: &mut Ui, c: Color, first: bool) {
        let mut r = Rect::new(Self::LEFT, -ui.top + 0.16, Self::WIDTH, 0.125);
        for (index, item) in self.items.iter_mut().enumerate() {
            if index == self.selected {
                self.y_upper.alter_to(r.y);
                self.y_lower.alter_to(r.bottom());
            }
            item.btn.set(ui, r);
            if first {
                ui.fill_rect(r, semi_black(0.4 * c.a));
            }
            ui.text((item.title)())
                .pos(r.center().x, r.center().y)
                .anchor(0.5, 0.5)
                .no_baseline()
                .size(0.5)
                .color(c)
                .draw();
            r.y += 0.125;
        }
    }

    /// 组合绘制整块选项卡与右侧内容区。
    ///
    /// 阶段：① 底层白字层（同时刷新命中区与高亮条目标位置）；
    /// ② 取高亮条当前上下边，绘制带阴影的白色高亮块；③ 用 `scissor` 在高亮块内重绘黑字，
    /// 实现「选中项反色」；④ 内容区铺半透明底并裁切；⑤ 内容切换：当进度 `p < 1` 时，
    /// 先绘制正在淡出的旧内容（按 `prev_go_up` 方向偏移 + 透明度 `1-p`），
    /// 再绘制新内容（反向偏移 + 透明度 `p`），形成交叉淡入淡出与轻微滑动；
    /// 「减少动态效果」时直接令 `p = 1` 跳过过渡，只画新内容。
    pub fn render(&mut self, ui: &mut Ui, t: f32, cr: Rect, mut f: impl FnMut(&mut Ui, &mut T) -> Result<()>) -> Result<()> {
        self.render_plain(ui, WHITE, true);

        let y_upper = self.y_upper.now(t);
        let y_lower = self.y_lower.now(t);
        let r = Rect::new(Self::LEFT, y_upper, Self::WIDTH, y_lower - y_upper).nonuniform_feather(0.007, -0.012);
        rounded_rect_shadow(
            ui,
            r,
            &ShadowConfig {
                radius: 0.008,
                base: 0.5,
                ..Default::default()
            },
        );
        ui.fill_path(&r.rounded(0.008), WHITE);
        ui.scissor(r, |ui| self.render_plain(ui, BLACK, false));

        ui.fill_path(&cr.rounded(0.005), semi_black(0.4));
        ui.scissor::<Result<()>>(cr, |ui| {
            let p = if get_data().prefer_reduced_motion {
                1.
            } else {
                self.content_progress.now(t)
            };
            if p < 1. {
                ui.scope(|ui| {
                    let dy = Self::CONTENT_DY * p;
                    ui.dy(if self.prev_go_up { -dy } else { dy });
                    ui.alpha(1. - p, |ui| f(ui, &mut self.items[self.prev].value))
                })?;
            }

            ui.scope(|ui| {
                let dy = Self::CONTENT_DY * (1. - p);
                ui.dy(if self.prev_go_up { dy } else { -dy });
                ui.alpha(p, |ui| f(ui, &mut self.items[self.selected].value))
            })?;

            Ok(())
        })?;

        Ok(())
    }
}
