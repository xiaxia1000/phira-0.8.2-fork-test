//! 顶点着色抽象层。
//!
//! UI 的几何绘制最终都交给 lyon 做三角化，但「同一份几何用不同方式上色」的需求很多：
//! 纯色、线性渐变、贴图、径向渐变。本模块把这些差异收敛到 [`Shading`] 一个 trait：
//! `new_vertex` 负责把「模型坐标 + 全局透明度」映射成最终 [`Vertex`]（屏幕坐标、UV、颜色）。
//! 于是 `VertexBuilder` / `ShadedConstructor` 这些几何与三角化代码完全不需要知道自己在画
//! 什么东西，只管把点丢给 [`Shading`]，新增一种着色方式也不用碰绘制管线。
//!
//! [`IntoShading`] 是配套的「统一调用入口」：`Ui::fill_rect` 等函数签名只写
//! `impl IntoShading`，调用方就能直接传 `Color`、`(tex, rect)`、渐变元组等形式，由编译器
//! 在调用点完成转换。这是刻意的隐式 API 设计——避免每个绘制调用都手写构造结构体，
//! 代价是类型推断出错时报错信息会绕着 `IntoShading` 的实现来回提示。

use crate::{
    core::{Point, Tweenable},
    ui::{source_of_image, Matrix, ScaleType},
};
use macroquad::prelude::*;

/// 单个顶点的着色策略。
///
/// 设计意图：把「顶点颜色 / UV 怎么算」从几何生成中彻底解耦。几何代码只提供模型坐标，
/// 着色策略负责补上颜色与 UV；这样矩形、圆、任意路径都能复用同一套填充/描边逻辑。
pub trait Shading {
    /// 把模型坐标 `p` 变换到屏幕坐标，并按本策略算出颜色与 UV，产出一个顶点。
    ///
    /// `mat` 是当前 UI 变换矩阵（可能带有滚动/缩放产生的平移），`alpha` 是整套 UI 的
    /// 全局透明度，需由实现方乘进顶点颜色的 alpha 通道。
    fn new_vertex(&self, mat: &Matrix, p: &Point, alpha: f32) -> Vertex;
    /// 返回本次绘制要采样的纹理；`None` 表示纯顶点色，绘制层据此决定是否绑定贴图。
    fn texture(&self) -> Option<Texture2D>;
}

/// 线性渐变着色：颜色沿 `vector` 方向按距离从 `color` 过渡到 `color_end`。
///
/// 采用 CPU 侧逐顶点算色 + 顶点色插值的方式实现，因此**不需要自定义材质**，
/// 与其它 UI 绘制共用同一条管线；代价是渐变精度受几何细分程度限制。
pub struct GradientShading {
    /// 渐变起点（插值参数 t = 0 的位置），使用未变换的模型坐标。
    origin: (f32, f32),
    /// 起点颜色。
    color: Color,
    /// 渐变方向的**单位**向量，`new_vertex` 用它点乘偏移量得到带符号距离作为 t。
    vector: (f32, f32),
    /// 终点颜色。
    ///
    /// 注意：构造时已按 `1 / |end - origin|` 做了预缩放，使得 t 不必再除以长度，
    /// 详见 `IntoShading` 的四元组实现。
    color_end: Color,
}

// 实现语义：把「沿渐变方向的带符号距离」直接当作插值参数使用。
// 因为终点色已在构造时预缩放，这里无需再做归一化除法；又因 `Tweenable::tween`
// 不做 clamp（f32 上是 x + (y - x) * t），t 超出 [0, 1] 会线性外推，
// 这正是渐变端点之外颜色仍然连续、不会突变的边界行为。
impl Shading for GradientShading {
    /// 用 `(p - origin) · vector` 求插值参数；UV 无意义，固定填 0。
    fn new_vertex(&self, mat: &Matrix, p: &Point, alpha: f32) -> Vertex {
        let t = mat.transform_point(p);
        let mut color = {
            let (dx, dy) = (p.x - self.origin.0, p.y - self.origin.1);
            Color::tween(&self.color, &self.color_end, dx * self.vector.0 + dy * self.vector.1)
        };
        color.a *= alpha;

        Vertex::new(t.x, t.y, 0., 0., 0., color)
    }

    /// 纯顶点色渐变不采样纹理，因此绘制时无需绑定贴图。
    fn texture(&self) -> Option<Texture2D> {
        None
    }
}

/// 纹理着色：把一块纹理的指定区域映射到几何矩形上。
///
/// 与另两种着色的差别在于它**产出 UV 并声明纹理**，因此绘制时必须绑定纹理、
/// 由片元着色器采样，颜色只作为乘算色调（tint）。
pub struct TextureShading {
    /// `(纹理, 源区域, 目标区域)`：源区域是纹理中的 UV 矩形（归一化坐标），
    /// 目标区域是顶点所在的模型空间矩形。两者宽高比不一致时纹理会被拉伸。
    texture: (Texture2D, Rect, Rect),
    /// 纹理乘算色调；与 `alpha` 相乘后写入顶点色 alpha。
    color: Color,
}

// 实现语义：先把顶点在目标矩形中的相对位置归一化到 [0, 1]（即 ux/uy），
// 再线性映射进源 UV 矩形。两处 clamp 被注释掉是有意为之——不裁剪意味着
// ux/uy 超出 [0, 1] 时会采样到源矩形之外的区域，配合纹理的 wrap 模式可实现平铺。
impl Shading for TextureShading {
    /// 输出 UV 到顶点的 texcoord，颜色仅做 tint（不含逐像素渐变）。
    fn new_vertex(&self, mat: &Matrix, p: &Point, alpha: f32) -> Vertex {
        let t = mat.transform_point(p);
        let (_, tr, dr) = self.texture;
        let ux = (p.x - dr.x) / dr.w;
        let uy = (p.y - dr.y) / dr.h;
        // let ux = ux.clamp(0., 1.);
        // let uy = uy.clamp(0., 1.);
        Vertex::new(
            t.x,
            t.y,
            0.,
            tr.x + tr.w * ux,
            tr.y + tr.h * uy,
            Color {
                a: self.color.a * alpha,
                ..self.color
            },
        )
    }

    /// 返回被采样的纹理，绘制层据此绑定贴图并选择合适的批处理策略。
    fn texture(&self) -> Option<Texture2D> {
        Some(self.texture.0)
    }
}

/// 径向渐变着色：以 `origin` 为圆心，颜色随半径从 `color` 过渡到 `color_end`。
///
/// 与 [`GradientShading`] 的差别是插值参数取的是**欧氏距离**而非方向投影，
/// 因此适合表现光晕、辉光、圆形遮罩等效果。
pub struct RadialShading {
    /// 渐变圆心（模型坐标）。
    origin: Point,
    /// 渐变半径：距离等于该值时到达终点色；传入 0 或非有限值会导致颜色未定义。
    radius: f32,
    /// 圆心处颜色。
    color: Color,
    /// 半径处颜色；与线性渐变不同，这里不做预缩放，因为 t 本身就是半径比例。
    color_end: Color,
}

// 实现语义：t = |p - origin| / radius，同样是线性外推（不 clamp），
// 所以圆外区域会继续远离终点色，画带透明终点色的光晕时正是靠这一点自然淡出。
impl Shading for RadialShading {
    /// 以到圆心的归一化距离作为插值参数；UV 无意义，固定填 0。
    fn new_vertex(&self, mat: &Matrix, p: &Point, alpha: f32) -> Vertex {
        let e = (p - self.origin).norm() / self.radius;
        let mut color = Color::tween(&self.color, &self.color_end, e);
        color.a *= alpha;
        let t = mat.transform_point(p);
        Vertex::new(t.x, t.y, 0., 0., 0., color)
    }

    /// 纯顶点色径向渐变不采样纹理。
    fn texture(&self) -> Option<Texture2D> {
        None
    }
}

/// 从「调用方顺手能写出的类型」到具体 [`Shading`] 的统一转换入口。
///
/// 设计意图：让所有绘制 API 只需写 `impl IntoShading`，调用方就能直接传 `Color`、
/// `(tex, rect)`、`(color, origin, color_end, end)` 等元组，由类型系统在调用点挑选
/// 正确的实现。这样新增一种着色参数组合只需新增一个 `From`/`IntoShading` 实现，
/// 不必给每个绘制函数加重载。
pub trait IntoShading {
    /// 转换目标的着色类型，必须实现 [`Shading`]。
    type Target: Shading;

    /// 执行转换；对已是 [`Shading`] 的类型是恒等操作（见下面的 blanket impl）。
    fn into_shading(self) -> Self::Target;
}

// 实现语义：恒等转换（blanket impl），使任何已实现 `Shading` 的具体着色结构体本身
// 也能直接传给 `impl IntoShading` 参数。注意这条 blanket impl 与后面的具体实现
// 不会冲突，因为 `Color`、元组等类型都没有实现 `Shading`。
impl<T: Shading> IntoShading for T {
    type Target = T;

    fn into_shading(self) -> Self::Target {
        self
    }
}

// 实现语义：单个 `Color` 视为「起点即终点」的退化线性渐变，等价于纯色填充。
// 之所以复用 GradientShading 而不是单独做一种 SolidShading，是为了减少一条代码路径：
// 颜色恒定意味着无论 t 取何值结果都相同，数学上退化为纯色。
impl IntoShading for Color {
    type Target = GradientShading;

    fn into_shading(self) -> Self::Target {
        GradientShading {
            origin: (0., 0.),
            color: self,
            vector: (1., 0.),
            color_end: self,
        }
    }
}

// 实现语义：由「起点色、起点、终点色、终点」构造线性渐变。
// 这里做了一步关键的数值预处理：把方向向量单位化（同时防止除零前的 norm 为 0 情况
// 由调用方保证），并把终点色提前按 `1 / norm` 向起点色插值。
// 这样 `new_vertex` 里 t = 投影距离，乘上已缩放的色差后，恰好在终点处得到原始终点色，
// 省掉了每个顶点一次除法。
impl IntoShading for (Color, (f32, f32), Color, (f32, f32)) {
    type Target = GradientShading;

    fn into_shading(self) -> Self::Target {
        let (color, origin, color_end, end) = self;
        let vector = (end.0 - origin.0, end.1 - origin.1);
        let norm = vector.0.hypot(vector.1);
        let vector = (vector.0 / norm, vector.1 / norm);
        let color_end = Color::tween(&color, &color_end, 1. / norm);
        GradientShading {
            origin,
            color,
            vector,
            color_end,
        }
    }
}

// 实现语义：由「圆心色、圆心、边缘色、半径」构造径向渐变。
// 半径原样保存而不是像线性渐变那样预缩放，因为径向的插值参数本身就是半径比例，
// 无需再做除法归一化。
impl IntoShading for (Color, (f32, f32), Color, f32) {
    type Target = RadialShading;

    fn into_shading(self) -> Self::Target {
        let (color, origin, color_end, radius) = self;
        RadialShading {
            origin: Point::new(origin.0, origin.1),
            radius,
            color,
            color_end,
        }
    }
}

// 以下三个实现是贴图着色的「参数逐级补全」链：低元数签名的实现把缺省参数
// （缩放方式、色调）补上后委托给更高元数的实现，避免重复计算逻辑。
// 这也解释了为什么 `(tex, rect)` 与 `(tex, rect, ScaleType, Color)` 能同时被接受。
impl IntoShading for (Texture2D, Rect) {
    type Target = TextureShading;

    /// 默认按 [`ScaleType`] 的缺省值（`CropCenter`）裁剪，色调为白（不改变原色）。
    #[inline]
    fn into_shading(self) -> Self::Target {
        let (tex, rect) = self;
        (tex, rect, ScaleType::default(), WHITE).into_shading()
    }
}

impl IntoShading for (Texture2D, Rect, ScaleType) {
    type Target = TextureShading;

    /// 只补默认色调，缩放方式由调用方指定。
    #[inline]
    fn into_shading(self) -> Self::Target {
        let (tex, rect, scale_type) = self;
        (tex, rect, scale_type, WHITE).into_shading()
    }
}

// 实现语义：参数最全的一档，真正计算源 UV 矩形。
// `source_of_image` 依据目标矩形的宽高比与纹理实际宽高比算出应采样的子区域；
// `ScaleType::Fit` 会返回 `None`，此处回退为整张纹理 `(0, 0, 1, 1)`，
// 也就是「不裁剪、直接拉伸铺满」，是唯一不需要知道纹理尺寸的兜底路径。
impl IntoShading for (Texture2D, Rect, ScaleType, Color) {
    type Target = TextureShading;

    fn into_shading(self) -> Self::Target {
        let (tex, rect, scale_type, color) = self;
        let source = source_of_image(&tex, rect, scale_type).unwrap_or_else(|| Rect::new(0., 0., 1., 1.));
        TextureShading {
            texture: (tex, source, rect),
            color,
        }
    }
}
