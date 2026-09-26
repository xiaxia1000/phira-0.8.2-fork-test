//! 卡片「伪 3D 倾斜」交互（首页/其它页的卡片区域使用）。
//!
//! 由触点相对卡片中心的方向与距离合成一个绕切向轴的小角度旋转，让卡片看起来朝手指倾斜，
//! 松手后用补间平滑复位到 `anchor`。这里只做单轴旋转 + 透视投影，不引入真正的 3D 场景与模型，
//! 既省开销，又能提供一致的「被按压」反馈。

use crate::anim::Anim;
use macroquad::prelude::*;
use prpr::ui::{RectButton, Ui};

/// 绑定到一块卡片矩形上的倾斜状态。
///
/// 视觉中心 `center` 会随触摸移动并被补间，空闲时收敛回 `anchor`；
/// 每帧由 `now` 依据当前中心重新计算变换矩阵。
pub struct ThreeD {
    /// 当前视觉中心，动画目标为触摸点或 `anchor`。
    center: Anim<Vec2>,
    /// 用于命中判定的按钮区域（触摸时才产生倾斜）。
    inner: RectButton,
    /// 空闲（无触摸）时中心的复位点。
    pub anchor: Vec2,
    /// 最大倾斜角度（弧度）：触点离中心越远越接近该值。
    pub angle: f32,
}

// 倾斜状态的驱动。`touch` 记录触点、`now` 每帧依当前中心计算变换矩阵，二者共同实现「跟手倾斜 + 松手复位」。
impl ThreeD {
    /// 中心补间的时长（秒）。取值偏短，既保证卡片能「跟手」移动，又让松手后迅速回位。
    const DURATION: f32 = 0.2;

    /// 创建倾斜状态：中心初值为零点、默认最大倾斜 `0.08` 弧度（约 4.6°），足以被察觉而不夸张。
    pub fn new() -> Self {
        Self {
            center: Anim::new(Vec2::default()),
            inner: RectButton::new(),
            anchor: Vec2::default(),
            angle: 0.08,
        }
    }

    /// 把动画的起止值都对齐到 `anchor`（以 `t = 0` 重新起算）；由于起止同值，取值恒为 `anchor`，
    /// 等效于瞬时复位。用于布局发生跳变（如换页/窗口尺寸变化）后立即复位，避免残留的倾斜被补间慢慢带过去。
    pub fn sync(&mut self) {
        self.center.start(self.anchor, self.anchor, 0., Self::DURATION);
    }

    /// 把触摸事件转交给命中区域；命中期间让视觉中心补间到触点，实现卡片朝手指倾斜。
    /// 未命中则不改变状态（`now` 会自动把中心拉回 `anchor`）。
    pub fn touch(&mut self, touch: &Touch, t: f32) {
        self.inner.touch(touch);
        if self.inner.touching() {
            self.center.goto(touch.position, t, Self::DURATION);
        }
    }

    /// 计算把矩形平面绕其中心做透视旋转的变换矩阵。
    ///
    /// - 旋转轴：由触点相对中心的方向向量取其垂线，故卡片朝触点方向倾斜；
    /// - 角度幅度：由触点到中心的距离经三次缓动 `(1-(1-length)^3)/0.6` 放大后钳制到 1，再乘 `angle`；
    ///   即越靠边倾斜越明显，超过约 1 个单位距离后饱和，避免触摸点接近边缘时角度突变。
    /// - 若触点几乎落在中心（`length <= eps`），返回单位矩阵以避免对零向量归一化（除零）。
    pub fn build(point: Vec2, r: Rect, angle: f32) -> Mat4 {
        let ct = r.center();
        let mut delta = point - ct;
        let length = delta.length();
        let eps = 1e-4;
        if length > eps {
            delta /= length;
            Mat4::from_translation(vec3(ct.x, ct.y, 0.))
                * Mat4::perspective_infinite_rh(std::f32::consts::FRAC_PI_2, 1., 1.)
                * Mat4::from_rotation_translation(
                    Quat::from_axis_angle(vec3(-delta.y, delta.x, 0.), ((1. - (1. - length).powi(3)) / 0.6).min(1.) * angle),
                    vec3(0., 0., -1.),
                )
                * Mat4::from_translation(vec3(-ct.x, -ct.y, 0.))
        } else {
            Mat4::IDENTITY
        }
    }

    /// 每帧调用：刷新命中区域；若当前未被触摸则把中心补间回 `anchor`；
    /// 返回该时刻的变换矩阵供绘制使用。
    pub fn now(&mut self, ui: &mut Ui, rect: Rect, t: f32) -> Mat4 {
        self.inner.set(ui, rect);

        if !self.inner.touching() {
            self.center.goto(self.anchor, t, Self::DURATION);
        }

        Self::build(self.center.now(t), rect, self.angle)
    }
}
