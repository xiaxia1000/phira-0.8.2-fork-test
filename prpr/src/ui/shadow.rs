//! UI 层唯一的自定义材质所在地。
//!
//! 普通 UI 绘制全部走 macroquad 的默认材质（顶点色 + 可选纹理），但有三种效果在固定
//! 管线里做不了，必须写片元着色器：
//! 1. [`SHADOW_MATERIAL`]：高斯模糊的圆角矩形投影；
//! 2. [`RR_MATERIAL`]：圆角矩形 SDF 裁剪（用于给滚动区域一个圆角边界）；
//! 3. [`SECTOR_MATERIAL`]：扇形/环形裁剪（用于轨道、扇形高亮一类形状）。
//!
//! 三个材质共用同一份顶点着色器 [`shader::VERTEX`]，只替换片元部分。
//! `varying pos0` 把顶点的几何坐标原样传给片元着色器，片元用它和 uniform 里的矩形/圆心
//! 做隐式表面（SDF）比较。注意 `pos0` 取自 `position` 属性本身而没有再乘 `Model`，
//! 因此坐标系与 `Ui::rect_to_global` 输出的「全局设计坐标」一致，两边必须用同一套单位，
//! 否则裁剪区域会错位。
//!
//! **不变量**：`gl_use_material` 与 `gl_use_default_material` 必须严格成对出现
//! （包括 `f(ui)` panic 的情况）。材质会一直生效到被显式切回为止，一旦泄漏就会污染
//! 后续所有 draw call，表现为后续 UI 整体被裁剪/被阴影覆盖。

use super::Ui;
use macroquad::prelude::*;
use miniquad::{BlendFactor, BlendState, BlendValue, Equation};
use once_cell::sync::Lazy;

/// 构造一个「标准 alpha 混合」的材质参数。
///
/// 默认的 `PipelineParams` 不做颜色混合，若不显式设置 `color_blend`，片元着色器输出的
/// 半透明像素会直接覆盖背景而不是与之混合，阴影和抗锯齿边缘就都会变成硬边黑块。
/// 这里固定为标准 `src_alpha / one_minus_src_alpha` 的 `Add` 混合。
/// `textures` 置空是有意为之：`Texture` 采样器由 macroquad 的默认材质槽位提供，
/// 不需要在这里额外绑定。
fn alpha_blend_material_params(uniforms: Vec<(String, UniformType)>) -> MaterialParams {
    MaterialParams {
        pipeline_params: PipelineParams {
            color_blend: Some(BlendState::new(
                Equation::Add,
                BlendFactor::Value(BlendValue::SourceAlpha),
                BlendFactor::OneMinusValue(BlendValue::SourceAlpha),
            )),
            ..Default::default()
        },
        uniforms,
        textures: Vec::new(),
    }
}

/// 快速圆角阴影材质（算法见 `shader::SHADOW_FRAGMENT`）。
///
/// 之所以用 `once_cell::sync::Lazy` 而不是普通 `static` 常量：`load_material` 需要已经
/// 存在的 OpenGL 上下文，无法在程序启动时初始化，只能推迟到首次渲染时惰性编译，
/// 并顺带避免未使用该效果的平台白白付出一次着色器编译。
static SHADOW_MATERIAL: Lazy<Material> =
    Lazy::new(|| load_material(shader::VERTEX, shader::SHADOW_FRAGMENT, alpha_blend_material_params(ShadowConfig::uniforms())).unwrap());

/// 圆角矩形裁剪材质：片元里用圆角矩形的 SDF 计算覆盖率并乘到 alpha 上。
///
/// uniform 只有 `rect`（`vec4(lower.xy, upper.xy)`）和 `radius`，因此它只负责
/// 「裁掉圆角外的像素」，不改变颜色与纹理，可以安全地包住任意既有绘制代码。
static RR_MATERIAL: Lazy<Material> = Lazy::new(|| {
    load_material(
        shader::VERTEX,
        shader::RR_FRAGMENT,
        alpha_blend_material_params(vec![("rect".to_owned(), UniformType::Float4), ("radius".to_owned(), UniformType::Float1)]),
    )
    .unwrap()
});

/// 扇形裁剪材质：按「相对圆心的角度区间 + 半径区间」保留像素。
///
/// `center` 是扇形顶点（全局设计坐标），`angle` 是 `(起始角, 结束角)` 弧度，
/// `blur` 是 `(外半径, 内半径)` 组成的环形带。用它包住一段绘制即可得到扇形/环形描边效果。
static SECTOR_MATERIAL: Lazy<Material> = Lazy::new(|| {
    load_material(
        shader::VERTEX,
        shader::SECTOR_FRAGMENT,
        alpha_blend_material_params(vec![
            ("center".to_owned(), UniformType::Float2),
            ("angle".to_owned(), UniformType::Float2),
            ("blur".to_owned(), UniformType::Float2),
        ]),
    )
    .unwrap()
});

/// 圆角阴影的三个可调参数（外加调用时注入的矩形）。
///
/// 三个参数共同决定观感：
/// - `elevation`：片元里被当作高斯核的 sigma，即「模糊半径」。越大阴影越软越向外扩散；
///   同时 `rounded_rect_shadow` 会按它把绘制矩形外扩 3 倍，为高斯拖尾留出采样空间，
///   因为片元着色器只在被光栅化的像素上运行，画布不够大阴影就会被直接切掉。
/// - `radius`：圆角矩形的圆角半径，阴影形状必须与元素自身的圆角一致，否则转角处会露出直角。
/// - `base`：阴影的最深不透明度上限（片元输出 alpha 直接乘以它）；数值越大阴影越重。
///
/// 单位与 UI 设计坐标一致（`rect` 由 `Ui::rect_to_global` 得到）。
#[derive(Clone, Copy)]
pub struct ShadowConfig {
    /// 阴影模糊强度（高斯 sigma），同时决定绘制矩形向外扩张的幅度。
    pub elevation: f32,
    /// 圆角半径，需与阴影所依附元素的圆角保持相同数值。
    pub radius: f32,
    /// 阴影最大不透明度；调用方通常还会再乘上 `ui.alpha` 做整体淡入淡出。
    pub base: f32,
}
// 默认值取「细微投影」：elevation/radius 同为 0.005 表示几乎不模糊的小圆角，
// base 0.7 让阴影足够可见但不至于发黑。这组数值是全 UI 下拉按钮等的统一观感基线。
impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            elevation: 0.005,
            radius: 0.005,
            base: 0.7,
        }
    }
}

impl ShadowConfig {
    /// 声明阴影材质需要的 uniform 列表（含矩形本身）。
    ///
    /// 必须与实际 set 的 uniform 名称、类型一一对应：`load_material` 时声明的 uniform
    /// 就是着色器里能拿到的全部变量，拼错名字不会报错，只会静默拿到默认值 0。
    pub fn uniforms() -> Vec<(String, UniformType)> {
        vec![
            ("rect".to_owned(), UniformType::Float4),
            ("elevation".to_owned(), UniformType::Float1),
            ("radius".to_owned(), UniformType::Float1),
            ("base".to_owned(), UniformType::Float1),
        ]
    }

    /// 把自身的三个参数写入材质的 uniform。
    ///
    /// 注意不含 `rect`——矩形是每次绘制都不同的量，由调用方单独设置；
    /// 因此一个材质可以连续绘制多个不同矩形，只需在每次 draw 前重设 `rect`。
    pub fn apply(&self, mat: &Material) {
        mat.set_uniform("elevation", self.elevation);
        mat.set_uniform("radius", self.radius);
        mat.set_uniform("base", self.base);
    }
}

/// 在 `r` 的位置画一圈圆角矩形阴影。
///
/// 实现要点：
/// - 片元着色器自己输出黑色与算好的 alpha，因此这里用 `WHITE` 画矩形即可，
///   顶点颜色与纹理对结果没有影响；
/// - `base` 会被乘上 `ui.alpha`，让阴影跟随 UI 的整体透明度（弹窗淡出时阴影一起淡出）；
/// - 绘制范围外扩 `elevation * 3`，与高斯核 3σ 的有效区间对应。
pub fn rounded_rect_shadow(ui: &mut Ui, r: Rect, config: &ShadowConfig) {
    // r.y += elevation * 0.5;
    let mat = *SHADOW_MATERIAL;
    let gr = ui.rect_to_global(r);
    mat.set_uniform("rect", vec4(gr.x, gr.y, gr.right(), gr.bottom()));
    ShadowConfig {
        base: config.base * ui.alpha,
        ..*config
    }
    .apply(&mat);
    gl_use_material(mat);
    let r3 = config.elevation * 3.0;
    draw_rectangle(gr.x - r3, gr.y - r3, gr.w + r3 * 2., gr.h + r3 * 2., WHITE);
    gl_use_default_material();
}

/// 在闭包 `f` 执行期间开启圆角矩形裁剪，返回闭包的返回值。
///
/// `radius` 为 0 时退化为普通矩形裁剪；配合 `radius > 0` 可让滚动区域以圆角边缘收尾。
/// 适用于需要「真实圆角」而非 `scissor` 直角裁剪的场合（`scissor` 只能裁矩形）。
pub fn clip_rounded_rect<R>(ui: &mut Ui, r: Rect, radius: f32, f: impl FnOnce(&mut Ui) -> R) -> R {
    let mat = *RR_MATERIAL;
    let gr = ui.rect_to_global(r);
    mat.set_uniform("rect", vec4(gr.x, gr.y, gr.right(), gr.bottom()));
    mat.set_uniform("radius", radius);
    gl_use_material(mat);
    let res = f(ui);
    gl_use_default_material();
    res
}

/// 在闭包 `f` 执行期间开启扇形裁剪，返回闭包的返回值。
///
/// 参数：`ct` 是扇形圆心（UI 局部坐标），`start`/`end` 是起止角度（弧度，逆时针，
/// `atan2` 的取值域）。片元着色器用极坐标判定点是否落在 `[start, end]` 区间内。
///
/// `blur` 的计算容易被误解，这里说明数值来源：着色器需要一个「半径区间」来归一化，
/// 而本函数只有 `ui.top`（UI 的垂直半高）这一个尺度可用。假设扇形结束边是一条从圆心
/// 出发的射线，角度为 `end`，射线上距离 r 处的 y 坐标为 `r * sin(end)`，
/// 于是反解出 `r = y / sin(end)`。代码取 `t = -sin(end)`，把 `ct.y ± ui.top`
/// 这两个垂直边界换算成沿该射线的半径，从而让归一化区间恰好覆盖整个 UI 高度——
/// 这样 `p` 在可见区域内始终落在合法范围，裁剪结果不会随 UI 尺寸突变。
pub fn clip_sector<R>(ui: &mut Ui, ct: Vec2, start: f32, end: f32, f: impl FnOnce(&mut Ui) -> R) -> R {
    let mat = *SECTOR_MATERIAL;
    mat.set_uniform("center", ui.to_global((ct.x, ct.y)));
    mat.set_uniform("angle", vec2(start, end));
    let t = -end.sin();
    mat.set_uniform("blur", vec2((ct.y - ui.top) / t, (ct.y + ui.top) / t));
    gl_use_material(mat);
    let res = f(ui);
    gl_use_default_material();
    res
}

// 内联 GLSL 源码。
//
// 统一使用 `#version 100`（GLSL ES 1.00）而不是 `#version 300 es` 之类：
// miniquad 在移动端/WebGL1 上只保证 GLES2 级别的着色器能力，而桌面 GL 也能向后兼容
// GLSL ES 1.00，因此 100 是唯一「一份代码全平台都能编译」的版本。
// 代价是不能用 `in/out`、整数取模等新语法，所以这里一律用 `attribute`/`varying` 与浮点运算。
mod shader {
    /// 三个材质共用的顶点着色器。
    ///
    /// 顶点布局必须与 macroquad 的 `Vertex` 保持一致，否则从同一个顶点缓冲读出的属性会错位：
    /// `position` 是已经过 UI 变换的顶点坐标，`texcoord` 是 UV，`color0` 是顶点色。
    ///
    /// 两处细节：
    /// - `color = color0 / 255.0;`：macroquad 把顶点色按 u8 打包上传（省内存），
    ///   这里归一化到 [0, 1] 才能作为 RGBA 使用；
    /// - `pos0 = position.xy;`：把**未经 Projection/Model 变换**的原始几何坐标传给片元，
    ///   于是片元里的 `pos0` 与 uniform `rect`/`center`（由 `rect_to_global` 生成的全局
    ///   设计坐标）处在同一坐标系。若改成传 `gl_Position` 或乘了 Model，裁剪区域会错位。
    pub const VERTEX: &str = r#"#version 100
attribute vec3 position;
attribute vec2 texcoord;
attribute vec4 color0;

varying lowp vec4 color;
varying highp vec2 pos0;
varying lowp vec2 uv;

uniform mat4 Model;
uniform mat4 Projection;

void main() {
    gl_Position = Projection * Model * vec4(position, 1);
    color = color0 / 255.0;
    pos0 = position.xy;
    uv = texcoord;
}"#;

    /// 快速圆角矩形阴影片元着色器。
    ///
    /// 算法来自 Evan Wallace 的 <https://madebyevan.com/shaders/fast-rounded-rectangle-shadows/>：
    /// 把「矩形与高斯核卷积」这一二维积分化简为一维可分离形式——
    /// `roundedBoxShadowX` 用误差函数 `erf` 的近似解析地算出沿 x 方向的模糊遮罩，
    /// 再沿 y 方向取 4 个采样点做数值积分（高斯在 ±3σ 之外几乎为 0，所以少量采样即可）。
    /// 这比直接对高斯核做二维卷积快得多，也避免了在每个像素上循环整个核。
    ///
    /// 几个关键点：
    /// - `factor` 项复用了与 [`RR_FRAGMENT`] 相同的圆角矩形判定，其在矩形内部为 0，
    ///   乘上去相当于把「元素自身覆盖的区域」从阴影里挖掉，否则阴影会把元素也涂黑；
    /// - `point.y -= sigma * 0.5` 把采样点整体上移，使阴影相对元素略微向下偏移，产生「光从上方来」的立体感；
    /// - `erf` 用的是 Abramowitz & Stegun 7.1.26 形式的近似多项式（系数 0.278393 / 0.230389 / 0.078108）；
    /// - 输出固定为黑色，alpha = 阴影覆盖率 × `base`，因此调用方传入的顶点色不影响结果；
    /// - `eps = 0.0003` / `ein = 0.0007` 是两个抗锯齿微调量：前者避免 `step` 在圆角边界处闪烁，
    ///   后者给矩形边界做一点平滑过渡。
    pub const SHADOW_FRAGMENT: &str = r#"#version 100
// Adapted from https://madebyevan.com/shaders/fast-rounded-rectangle-shadows/
precision highp float;

varying lowp vec4 color;
varying highp vec2 pos0;

// A standard gaussian function, used for weighting samples
float gaussian(float x, float sigma) {
  const float pi = 3.141592653589793;
  return exp(-(x * x) / (2.0 * sigma * sigma)) / (sqrt(2.0 * pi) * sigma);
}

// This approximates the error function, needed for the gaussian integral
vec2 erf(vec2 x) {
  vec2 s = sign(x), a = abs(x);
  x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
  x *= x;
  return s - s / (x * x);
}

// Return the blurred mask along the x dimension
float roundedBoxShadowX(float x, float y, float sigma, float corner, vec2 halfSize) {
  float delta = min(halfSize.y - corner - abs(y), 0.0);
  float curved = halfSize.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
  vec2 integral = 0.5 + 0.5 * erf((x + vec2(-curved, curved)) * (sqrt(0.5) / sigma));
  return integral.y - integral.x;
}

// Return the mask for the shadow of a box from lower to upper
float roundedBoxShadow(vec2 lower, vec2 upper, vec2 point, float sigma, float corner) {
  vec2 lowerp = lower + vec2(corner);
  vec2 upperp = upper - vec2(corner);
  float lf = step(point.x, lowerp.x);
  float tp = step(point.y, lowerp.y);
  float rt = step(upperp.x, point.x);
  float bt = step(upperp.y, point.y);
  float eps = 0.0003;
  float ein = 0.0007;
  float factor = 1.0 -
      (1.0 - step(corner - eps, distance(lowerp, point)) * lf * tp)
    * (1.0 - step(corner - eps, distance(upperp, point)) * rt * bt)
    * (1.0 - step(corner - eps, distance(vec2(lowerp.x, upperp.y), point)) * lf * bt)
    * (1.0 - step(corner - eps, distance(vec2(upperp.x, lowerp.y), point)) * rt * tp)
    * smoothstep(lower.x, lower.x + ein, point.x)
    * smoothstep(lower.y, lower.y + ein, point.y)
    * smoothstep(point.x, point.x + ein, upper.x)
    * smoothstep(point.y, point.y + ein, upper.y);

  point.y -= sigma * 0.5;
  // Center everything to make the math easier
  vec2 center = (lower + upper) * 0.5;
  vec2 halfSize = (upper - lower) * 0.5;
  point -= center;

  // The signal is only non-zero in a limited range, so don't waste samples
  float low = point.y - halfSize.y;
  float high = point.y + halfSize.y;
  float start = clamp(-3.0 * sigma, low, high);
  float end = clamp(3.0 * sigma, low, high);

  // Accumulate samples (we can get away with surprisingly few samples)
  float s = (end - start) / 4.0;
  float y = start + s * 0.5;
  float value = 0.0;
  for (int i = 0; i < 4; i++) {
    value += roundedBoxShadowX(point.x, point.y - y, sigma, corner, halfSize) * gaussian(y, sigma) * s;
    y += s;
  }

  return value * factor;
}

uniform highp vec4 rect;
uniform highp float elevation;
uniform highp float radius;
uniform highp float base;

void main() {
  gl_FragColor = vec4(0.0, 0.0, 0.0, roundedBoxShadow(rect.xy, rect.zw, pos0, elevation, radius) * base);
}"#;

    /// 圆角矩形裁剪片元着色器。
    ///
    /// 逐像素判定「是否在圆角矩形内」：把矩形四个**内缩了 radius 的角点**
    /// （`lowerp` / `upperp` 的四种组合）分别与当前点求距离，
    /// 只有落在某个角点判定圆内、且该角点确实位于点的对应象限时（由 `lf`/`tp`/`rt`/`bt`
    /// 四个 `step` 门控），才把该处覆盖率置 0；再乘上四条边的 `step` 得到矩形本体判定。
    /// 于是「内缩矩形的并集 + 四个半径圆」正好拼成圆角矩形，无需分支即可判断内外。
    ///
    /// 与 `scissor` 裁剪相比的取舍：本材质能裁圆角、且可以在同一批次里保持绘制顺序，
    /// 但每个像素都要算一次距离，因此只用于确实需要圆角的地方。
    ///
    /// 注意 `pos0` 在此声明为 `lowp`：它参与的是与 `radius` 的比较，
    /// 低精度在屏幕坐标量级下可能带来亚像素抖动，但换取了移动端更低的寄存器压力。
    pub const RR_FRAGMENT: &str = r#"#version 100
precision highp float;

varying lowp vec4 color;
varying lowp vec2 pos0;
varying lowp vec2 uv;

uniform highp vec4 rect;
uniform highp float radius;

uniform sampler2D Texture;

void main() {
  vec2 lower = rect.xy, upper = rect.zw, point = pos0;
  vec2 lowerp = lower + vec2(radius);
  vec2 upperp = upper - vec2(radius);
  float lf = step(point.x, lowerp.x);
  float tp = step(point.y, lowerp.y);
  float rt = step(upperp.x, point.x);
  float bt = step(upperp.y, point.y);
  float eps = 0.0003;
  float factor =
      (1.0 - step(radius - eps, distance(lowerp, point)) * lf * tp)
    * (1.0 - step(radius - eps, distance(upperp, point)) * rt * bt)
    * (1.0 - step(radius - eps, distance(vec2(lowerp.x, upperp.y), point)) * lf * bt)
    * (1.0 - step(radius - eps, distance(vec2(upperp.x, lowerp.y), point)) * rt * tp)
    * step(lower.x, point.x)
    * step(lower.y, point.y)
    * step(point.x, upper.x)
    * step(point.y, upper.y);
  gl_FragColor = texture2D(Texture, uv) * color;
  gl_FragColor.a *= factor;
}"#;

    /// 扇形（环形带）裁剪片元着色器。
    ///
    /// 思路：把像素相对圆心 `center` 的位移 `delta` 换成极坐标，用 `atan` 得到角度 `cur`，
    /// 与 `angle = (start, end)` 比较即可判断是否在扇形张角内。
    ///
    /// 三处细节：
    /// - `p` 是「半径的归一化位置」，本意是让张角边缘的软化宽度随半径变化
    ///   （越靠外越软，视觉上更自然）；但 `blur_range` 里那项被写成 `0.005 + 0.0 * p`，
    ///   `p` 目前恒不生效，等于固定 0.005 的软化宽度——属于留待启用/调试中的写法；
    /// - 起始边用 `step(angle.x, cur)`（硬边），结束边额外套了一层 `smoothstep` 做抗锯齿，
    ///   因为结束边在游戏中通常朝向屏幕外，硬边更容易看出锯齿；
    /// - `blur.x` / `blur.y` 作为归一化的两端，若两者相等会导致除零产生 NaN 像素，
    ///   调用方 `clip_sector` 已保证它们不相等。
    pub const SECTOR_FRAGMENT: &str = r#"#version 100
precision highp float;

varying lowp vec4 color;
varying lowp vec2 pos0;
varying lowp vec2 uv;

uniform highp vec2 center;
uniform highp vec2 angle;
uniform highp vec2 blur;

uniform sampler2D Texture;

void main() {
    vec2 delta = pos0.xy - center;
    float cur = atan(delta.y, delta.x);
    float p = clamp((length(delta) - blur.y) / (blur.x - blur.y), 0.0, 1.0);
    p = p * p;
    float blur_range = 0.005 + 0.0 * p;
    float factor = step(angle.x, cur) * smoothstep(angle.y, cur - blur_range * 0.5, cur + blur_range * 0.5) * step(cur, angle.y);
    gl_FragColor = texture2D(Texture, uv);
    gl_FragColor.a *= factor;
}"#;
}
