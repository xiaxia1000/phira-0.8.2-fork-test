//! 对象变换：把“透明度 / 缩放 / 旋转 / 平移”四条独立动画合成为渲染矩阵。
//!
//! 坐标约定（全项目统一）：谱面用归一化坐标系描述，x ∈ [-1, 1] 直接对应屏幕左右
//! 边缘；y 与 x 使用同名单位（对 16:9 基准画布而言 ±1 即上下边缘），而渲染空间的
//! 纵向可视范围只有 ±1/aspect_ratio，因此落到矩阵前要把 y 除以宽高比，
//! 见 [`Object::now_translation`]。旋转一律用**角度制**给出，取用时转弧度。
//!
//! 本模块还包含 [`CtrlObject`]：谱面的“控制对象”，把事件作用到挂在判定线上的音符
//! （大小 / 位置 / 下落速度 / 透明度），与 [`Object`] 的差别只在量的含义与驱动方式。
use super::{AnimFloat, AnimVector, Color, Matrix, Resource, Vector};
use macroquad::prelude::*;
use nalgebra::Rotation2;

/// Describes the animation of an in-game object's local coordinate system in the parent coordinate system
/// 描述游戏内对象在父坐标系中的局部坐标系动画。
///
/// 四个属性各自独立成一条曲线：谱面的 alpha / scale / rotate / move 事件分开下发，
/// 且允许只覆盖某个轴，因此不合并为一条 `Anim`。各量的单位见字段说明；合成顺序由
/// [`Object::now`] 一族函数决定（旋转与平移复合、缩放绕指定中心进行）。
#[derive(Default)]
pub struct Object {
    /// 不透明度，无量纲（0 = 全透明，1 = 完全不透明）。
    ///
    /// 允许为负：Phira 谱面把负 alpha 复用为“特效扩展”通道（如隐藏判定线、
    /// 让音符提前出现），是否解释由谱面设置 `ChartSettings::pe_alpha_extension` 决定。
    pub alpha: AnimFloat,
    /// x、y 方向的缩放系数，无量纲；某轴缺省为 1，即该轴保持原尺寸不缩放。
    pub scale: AnimVector,
    /// Rotation in degrees
    /// 旋转角，**角度制**（不是弧度）；取用时经 `to_radians()` 交给旋转矩阵，
    /// 因此谱面里的角度数值可直接书写。旋向取决于坐标系 y 轴朝向，
    /// 在 UI 等 y 轴向下的场景中会取负号抵消，见 `Chart::with_element`。
    pub rotation: AnimFloat,
    /// 平移量，单位与谱面坐标一致：x 直接以“屏幕半宽”为单位，
    /// y 需除以宽高比后才成为渲染坐标，见 [`Object::now_translation`]。
    pub translation: AnimVector,
}

// 对象属性的批量查询与矩阵合成；均为纯函数，不推进时间（时间由 Chart 统一 set_time）。
impl Object {
    /// 四个属性是否全部为空动画（谱面从未对它们赋值）。
    ///
    /// 六个分量（scale.x/y、translation.x/y 分别算）都要检查，因为只要有一个被
    /// 赋值，本对象就不再是“静态”的，不能整体跳过更新；用于对象级裁剪。
    pub fn is_default(&self) -> bool {
        self.alpha.is_default()
            && self.scale.0.is_default()
            && self.scale.1.is_default()
            && self.rotation.is_default()
            && self.translation.0.is_default()
            && self.translation.1.is_default()
    }

    /// 把时间同步给全部六个分量动画（逐分量转发各 `AnimFloat` 的 `set_time`）。
    pub fn set_time(&mut self, time: f64) {
        self.alpha.set_time(time);
        self.scale.0.set_time(time);
        self.scale.1.set_time(time);
        self.rotation.set_time(time);
        self.translation.0.set_time(time);
        self.translation.1.set_time(time);
    }

    /// 六个分量动画是否都进入“已越过末关键帧”状态。
    ///
    /// 只有全部 `dead` 才返回真：任一属性仍在插值中就还需要继续渲染。
    /// 注意它不是 [`Object::is_default`] 的否定——空动画同样是 `dead`。
    pub fn dead(&self) -> bool {
        self.alpha.dead()
            && self.scale.0.dead()
            && self.scale.1.dead()
            && self.rotation.dead()
            && self.translation.0.dead()
            && self.translation.1.dead()
    }

    /// 合成“旋转 + 平移”矩阵（不含缩放），作为对象局部坐标系到父坐标系的变换。
    ///
    /// 先旋转后平移：平移是在父坐标系中量取的，因此不受自身旋转影响。
    /// 缩放不在其中，因为它必须绕指定中心进行，见 [`Object::now_scale`]。
    /// 结果只依赖当前 `time` 与屏幕宽高比。
    pub fn now(&self, res: &Resource) -> Matrix {
        self.now_rotation().append_translation(&self.now_translation(res))
    }

    /// 当前旋转矩阵（角度 -> 弧度后构造二维旋转）。
    ///
    /// 用齐次坐标 `to_homogeneous()` 是为了与平移/缩放矩阵直接相乘，
    /// 避免在调用处反复做维度转换。
    #[inline]
    pub fn now_rotation(&self) -> Matrix {
        Rotation2::new(self.rotation.now().to_radians()).to_homogeneous()
    }

    /// 当前平移（渲染空间）向量。
    ///
    /// `tr.y /= res.aspect_ratio` 是全项目的坐标换算关键：谱面 y 与 x 使用同一套
    /// 归一化单位（16:9 基准下 ±1 即上下边缘），但渲染空间里纵向可视范围只有
    /// ±1/aspect_ratio，因此必须除以宽高比才能落在正确位置；等价地说，这一步把
    /// 玩家实际宽高比与谱面 16:9 设计基准之间的偏差折算掉，使谱面按比例铺满屏幕。
    /// 缺省值为 0（`AnimVector::now` 的分量默认值），即不产生位移。
    #[inline]
    pub fn now_translation(&self, res: &Resource) -> Vector {
        let mut tr = self.translation.now();
        tr.y /= res.aspect_ratio;
        tr
    }

    /// 构造绕点 `pt` 旋转 `rot` 的齐次矩阵 `T(pt) · R · T(-pt)`。
    ///
    /// 先平移到原点、旋转、再平移回去，使旋转中心由默认的原点挪到 `pt`。
    /// 典型用法：让 HUD 元素绕其自身锚点（而非屏幕中心）旋转，
    /// 例如 `Chart::with_element` 中绕 `rotation_point` 旋转挂载的谱面元素。
    pub fn new_rotation_wrt_point(rot: Rotation2<f32>, pt: Vector) -> Matrix {
        let translation_back = Matrix::new_translation(&pt);
        let translation_to = Matrix::new_translation(&-pt);
        translation_back * rot.to_homogeneous() * translation_to
    }

    /// 当前不透明度，被钳制到 `>= 0`。
    ///
    /// 空动画（未设 alpha）时取 1，即默认完全可见；钳负则把谱面用来表达
    /// “特效扩展”的负 alpha（见字段说明）在普通绘制路径上统一视为全透明。
    #[inline]
    pub fn now_alpha(&self) -> f32 {
        self.alpha.now_opt().unwrap_or(1.0).max(0.)
    }

    /// 当前颜色：白色 + [`Object::now_alpha`] 得到的 alpha。
    ///
    /// 只承载透明度，色相由调用方另行与判定线/音符自身颜色相乘，
    /// 这样“变色”和“淡出”两条互不干扰的通道可以分开表达。
    #[inline]
    pub fn now_color(&self) -> Color {
        Color::new(1.0, 1.0, 1.0, self.now_alpha())
    }

    /// 构造绕中心 `ct` 的非均匀缩放矩阵 `T(-ct) · S · T(ct)`。
    ///
    /// 按判定线/音符的局部中心缩放（而非原点），否则缩放会连带把对象平移出去；
    /// `ct` 一般为对象的锚点（判定线中点、音符中心等）。
    /// 缺省值取 `(1.0, 1.0)` 而不是 0：未指定的轴表示“不缩放”，
    /// 若用 [`AnimVector::now`] 的 0 会把该轴压成零尺寸（见 `now_with_def`）。
    /// 注意缩放与旋转的分工：本函数不含旋转，旋转由 [`Object::now`] 一族提供。
    #[inline]
    pub fn now_scale(&self, ct: Vector) -> Matrix {
        let scale = self.scale.now_with_def(1.0, 1.0);
        Matrix::new_translation(&-ct).append_nonuniform_scaling(&scale).append_translation(&ct)
    }
}

/// Describes the animation of an in-game object in its local coordinate system
/// 描述游戏内对象在**自身局部坐标系**中的动画，用于谱面的“控制对象”。
///
/// 与 [`Object`] 的区别在于语义与驱动时间轴：`Object` 描述判定线/音符自身的变换，
/// 时间轴是谱面时间；`CtrlObject` 描述事件对“挂在判定线上的音符”的批量修饰
/// （大小、位置、下落速度、透明度），它的时间轴由 [`CtrlObject::set_height`] 以
/// “音符高度”充当，见该函数的说明。
#[derive(Default, Clone)]
pub struct CtrlObject {
    /// 整体透明度倍率，缺省 1；乘到音符颜色 alpha 上。
    pub alpha: AnimFloat,
    /// 音符尺寸缩放倍率，缺省 1；作用于横向，`note_uniform_scale` 时也作用于纵向。
    pub size: AnimFloat,
    /// 音符横向位置偏移倍率，缺省 1（保持原位）；乘在倾斜修正后的 x 上。
    pub pos: AnimFloat,
    /// 音符下落速度倍率，缺省 1；直接乘到音符速度上，因此会改变可视提前量。
    pub y: AnimFloat,
}

// 控制对象的唯一入口：把“高度”作为统一参数一次性喂给四个子动画。
impl CtrlObject {
    /// 以同一个 `height` 参数推进四个子动画的时间游标。
    ///
    /// 这里的 `height` 并不是秒：RPE 谱面的音符控制事件以“音符相对判定线的高度”
    /// 为自变量，调用处（`Note::init_ctrl_obj`）把它由当前音符高度与判定线高度之差
    /// 换算成标量后传入。四个子动画共用同一参数，保证同一条事件在 alpha / size /
    /// pos / y 上作用于同一高度位置，不会出现彼此错位。
    pub fn set_height(&mut self, height: f64) {
        self.alpha.set_time(height);
        self.size.set_time(height);
        self.pos.set_time(height);
        self.y.set_time(height);
    }
}
