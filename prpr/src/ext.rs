//! Miscellaneous utilities.
//!
//! 本模块是绘制 / 纹理 / 并发三类辅助工具的集合，共同特点是“跨模块复用的小工具”，
//! 而非某个业务概念的完整实现：
//! - 几何与颜色：矩形羽化、平行四边形、阴影；
//! - 绘制提交：多数函数最终通过 `get_internal_gl().quad_gl` 直接提交 draw call，
//!   因此**必须在渲染线程、且在渲染阶段内调用**；
//! - 纹理生命周期：`SafeTexture` 用引用计数 + 延迟删除解决 GL 上下文线程安全问题；
//! - 并发/平台：`thread_as_future`、`poll_future`、`create_audio_manger`、`open_url`。

use crate::{
    config::Config,
    core::{Matrix, Point, Vector},
    ui::Ui,
};
use anyhow::{anyhow, Result};
use image::DynamicImage;
use lyon::{
    math::Box2D,
    path::{builder::BorderRadii, Path, Winding},
};
use macroquad::prelude::*;
use miniquad::{gl::GLenum, BlendFactor, BlendState, BlendValue, CompareFunc, Equation, PrimitiveType, StencilFaceState, StencilOp, StencilState};
use once_cell::sync::Lazy;
use ordered_float::{FloatCore, NotNan};
use sasa::AudioManager;
use serde::Deserialize;
use std::{
    future::Future,
    ops::Deref,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Poll, RawWaker, RawWakerVTable, Waker},
};
use tracing::{debug, info_span};

/// 一次性局部任务的存放形式：尚未开始 / 已完成的 future 都放在 `Option` 里。
///
/// 用 `Option` 而不是直接持有 future，是为了让调用方可以在拿到 [`Poll::Ready`] 后
/// 把 future 丢弃（置 `None`），从而及时释放闭包捕获的资源。
pub type LocalTask<R> = Option<Pin<Box<dyn Future<Output = R>>>>;

/// 为任意迭代器补上 `join`（Rust 标准库只有 `Vec<&str>` 的 join）。
///
/// 之所以自己定义 trait 而不是引入 itertools，是为了让 `join` 也能作用于
/// 产生 `String` / `Cow<str>` 等实现了 `AsRef<str>` 的迭代器。
pub trait JoinToString {
    /// 用 `sep` 连接所有元素。
    ///
    /// 空迭代器返回空字符串；实现里用 `next()` 单独取出首个元素，
    /// 因此分隔符只会出现在元素之间，不会出现前导或尾随分隔符。
    fn join(self, sep: &str) -> String;
}

// 对“元素可按 &str 借用”的迭代器统一实现，不消耗元素所有权。
impl<V: AsRef<str>, T: Iterator<Item = V>> JoinToString for T {
    fn join(mut self, sep: &str) -> String {
        let mut result = String::new();
        if let Some(first) = self.next() {
            result += first.as_ref();
            for element in self {
                result += sep;
                result += element.as_ref();
            }
        }
        result
    }
}

/// 为浮点类型提供“断言非 NaN”的便捷构造。
///
/// [`NotNan`] 的构造函数返回 `Result`，在数值来自内部计算（已保证非 NaN）时
/// 逐处处理错误会淹没业务代码，故提供本扩展方法直接 `unwrap`。
pub trait NotNanExt: Sized {
    /// 包装为 `NotNan`；调用方需自行保证 `self` 不是 NaN，否则 panic。
    fn not_nan(self) -> NotNan<Self>;
}

// 对全部浮点类型（含 f32/f64）统一实现。
impl<T: FloatCore> NotNanExt for T {
    fn not_nan(self) -> NotNan<Self> {
        NotNan::new(self).unwrap()
    }
}

/// 矩形（[`Rect`]）的几何扩展：羽化、转 lyon 类型、圆角路径。
///
/// 这些操作在绘制判定线、按钮和阴影时反复出现，抽成 trait 以免到处手写
/// `Rect::new(...)` 的坐标算术。
pub trait RectExt: Sized {
    /// 四边各向外扩张 `radius`（宽高增加 `2 * radius`，中心不变）。
    /// 用于把遮罩区域外扩，配合渐变实现“羽化”边缘。
    fn feather(&self, radius: f32) -> Self;
    /// 与 [`RectExt::feather`] 相同，但横纵方向使用不同的扩张量。
    fn nonuniform_feather(&self, x: f32, y: f32) -> Self;
    /// 转换为 lyon 的欧氏包围盒，供路径 / 几何运算使用（坐标系与 `Rect` 一致）。
    fn to_euclid(&self) -> Box2D;
    /// 生成四个角半径为 `radius` 的圆角矩形路径。
    fn rounded(&self, radius: f32) -> Path;
}

// 对 macroquad 的 `Rect`（左上角 + 宽高表示）实现上述几何操作。
impl RectExt for Rect {
    fn feather(&self, radius: f32) -> Self {
        Self::new(self.x - radius, self.y - radius, self.w + radius * 2., self.h + radius * 2.)
    }

    fn nonuniform_feather(&self, x: f32, y: f32) -> Self {
        Self::new(self.x - x, self.y - y, self.w + x * 2., self.h + y * 2.)
    }

    fn to_euclid(&self) -> Box2D {
        // lyon 用 (min, max) 两个点表示包围盒，故这里把右下角换算出来。
        Box2D::new(lyon::math::point(self.x, self.y), lyon::math::point(self.right(), self.bottom()))
    }

    fn rounded(&self, radius: f32) -> Path {
        // Winding::Positive 是 lyon 的默认环绕方向，填充时无需再做奇偶判断。
        let mut path = Path::builder();
        path.add_rounded_rectangle(&self.to_euclid(), &BorderRadii::new(radius), Winding::Positive);
        path.build()
    }
}

/// 待删除纹理队列。
///
/// 这是 [`flush_pending_texture_deletions`] 与 [`queue_texture_deletion`] 之间的
/// 唯一交接点：写入方可能来自任意线程（任何 drop 掉最后一个 `SafeTexture` 的地方），
/// 读取方只应是渲染线程。用 `Mutex` 而非无锁结构，因为该路径不是热路径，
/// 而正确性比吞吐更重要。
static PENDING_TEXTURE_DELETIONS: Lazy<Mutex<Vec<Texture2D>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Deletes all textures queued up by `SafeTexture` drops so far.
///
/// Deleting a GL texture from a thread other than the one owning the GL
/// context crashes, so `SafeTextureInner::drop` cannot call `delete`
/// directly (it may run on any thread, e.g. when a background task drops
/// the last `Arc`). Instead it queues the texture here, and this must be
/// called periodically from the main (rendering) thread.
/// 由渲染线程周期性调用（通常每帧一次），清空队列并真正释放 GL 纹理。
///
/// 用 `std::mem::take` 一次性取走整个队列，把持锁时间压到最短——
/// 之后的 `delete()` 调用都在锁外执行，避免删除过程中的 GL 调用阻塞提交删除的线程。
pub fn flush_pending_texture_deletions() {
    let textures = std::mem::take(&mut *PENDING_TEXTURE_DELETIONS.lock().unwrap());
    for texture in textures {
        texture.delete();
    }
}

/// Queues a texture for deletion on the main thread instead of deleting it
/// immediately. See [`flush_pending_texture_deletions`].
/// 把纹理登记到待删除队列，实际释放推迟到渲染线程执行。
pub fn queue_texture_deletion(texture: Texture2D) {
    PENDING_TEXTURE_DELETIONS.lock().unwrap().push(texture);
}

/// 私有包装，唯一职责是在“最后一个引用被丢弃”的时刻请求一次延迟删除。
///
/// 不直接实现 [`Drop`] 于 [`SafeTexture`] 上，是因为 `SafeTexture` 可能被克隆，
/// 只有包在 `Arc` 里的这一层才能准确表达“引用计数归零”。
struct SafeTextureInner(Texture2D);
// 该析构可能在任意线程运行（例如后台加载任务持有最后一份引用），
// 因此这里绝不能直接删纹理，只能入队等待渲染线程处理。
impl Drop for SafeTextureInner {
    fn drop(&mut self) {
        queue_texture_deletion(self.0);
    }
}

/// 线程安全的纹理句柄：[`Texture2D`] 的引用计数包装，解决“纹理只能在 GL 线程删除”的问题。
///
/// 背景：macroquad 的 [`Texture2D`] 被 drop 时会立即调用 `glDeleteTextures`，
/// 而 GL 上下文只属于渲染线程。一旦最后一个引用是在后台线程（异步加载、资源
/// 卸载等）被丢弃，就会在错误的线程上操作 GL，导致崩溃或上下文损坏。
/// 因此这里用 `Arc` 计数，把删除动作改写为“入队”，由渲染线程通过
/// [`flush_pending_texture_deletions`] 完成真正的释放。
/// 代价是：只要有一份引用泄漏（例如 [`SafeTexture::into_inner`]），纹理就永远不会被回收。
pub struct SafeTexture(Arc<SafeTextureInner>);
// 纹理句柄的构造、属性调整与取回。
impl SafeTexture {
    /// 取出内部的 [`Texture2D`] 并把所有权完全交给调用方。
    ///
    /// 实现上用 `std::mem::forget` 泄漏掉 `Arc`：因为 `into_inner` 消费了 self，
    /// 若不 forget，`Arc` 的 drop 会把纹理排进删除队列，而返回出去的副本仍在使用。
    /// 因此调用方必须自行负责该纹理的最终释放；这也意味着此后再无延迟删除保护。
    pub fn into_inner(self) -> Texture2D {
        let arc = self.0;
        let res = arc.0;
        std::mem::forget(arc);
        res
    }

    /// 生成 mipmap 链并把缩小过滤切换为三线性插值，然后返回自身（便于链式调用）。
    ///
    /// 用裸 GL 调用是因为 macroquad 的高层 API 不暴露 mipmap 生成；
    /// 以 `self` 值接收的链式风格可以避免在临时对象上误用该设置。
    pub fn with_mipmap(self) -> Self {
        let id = self.0 .0.raw_miniquad_texture_handle().gl_internal_id();
        // SAFETY: 这里假设当前处于渲染线程且 GL 上下文已激活——本函数只应在
        // 渲染阶段调用。`id` 来自本纹理自身的句柄，绑定后立即改参数，
        // 不依赖外部 GL 状态，也不会泄漏到其它纹理。
        unsafe {
            use miniquad::gl::*;
            glBindTexture(GL_TEXTURE_2D, id);
            glGenerateMipmap(GL_TEXTURE_2D);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR as _);
        }
        self
    }

    /// 同时设置放大与缩小过滤方式（传入 GL 的过滤常量，例如 `GL_NEAREST`），返回自身。
    ///
    /// 放大与缩小一起设置是为了避免出现“放大用最近邻、缩小用线性”的混搭观感；
    /// 同样依赖调用方处于渲染线程。
    pub fn with_filter(self, filter: GLenum) -> Self {
        let id = self.0 .0.raw_miniquad_texture_handle().gl_internal_id();
        // SAFETY: 与 `with_mipmap` 相同的约定——必须在渲染线程、GL 上下文激活时调用；
        // 只修改本纹理自身的过滤参数，不影响其它纹理。
        unsafe {
            use miniquad::gl::*;
            glBindTexture(GL_TEXTURE_2D, id);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, filter as _);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, filter as _);
        }
        self
    }
}

// 克隆只增加引用计数、共享同一张底层纹理，不复制 GPU 资源。
impl Clone for SafeTexture {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

// 透明地把 SafeTexture 当作 Texture2D 使用，省去每处都写 `.0.0`。
impl Deref for SafeTexture {
    type Target = Texture2D;

    fn deref(&self) -> &Self::Target {
        &self.0.as_ref().0
    }
}

// 按底层纹理身份（`Arc` 指针）判等，而不是按像素内容：
// 同一次加载得到的多个句柄相等，两次分别加载出的“同源”纹理不相等。
impl PartialEq for SafeTexture {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
// 指针相等天然满足自反、对称、传递，故可以安全地标记为 `Eq`。
impl Eq for SafeTexture {}

// 由已有 GPU 纹理直接构造句柄；所有权随 Arc 迁移，不做任何拷贝。
impl From<Texture2D> for SafeTexture {
    fn from(tex: Texture2D) -> Self {
        Self(Arc::new(SafeTextureInner(tex)))
    }
}

// 由 CPU 侧图像上传为 GPU 纹理（统一转成 RGBA8，与渲染管线格式一致）。
impl From<DynamicImage> for SafeTexture {
    fn from(image: DynamicImage) -> Self {
        Texture2D::from_rgba8(image.width() as _, image.height() as _, &image.into_rgba8()).into()
    }
}

/// 1x1 纯黑纹理的全局单例。
///
/// 用于着色器仍要求绑定一张纹理、但实际只想画纯色的场合；用单例而不是每次新建，
/// 是为了避免重复上传与重复占用纹理单元。`Lazy` 保证进程内只创建一次。
pub static BLACK_TEXTURE: Lazy<SafeTexture> = Lazy::new(|| Texture2D::from_rgba8(1, 1, &[0, 0, 0, 255]).into());

/// 把 nalgebra 的 3x3 矩阵转换为 glm 的 4x4 矩阵（macroquad 的 `Model` 类型）。
///
/// nalgebra 的索引是 `m[行][列]`（`m11` 即第 1 行第 1 列），而 GL 的 `Mat4`
/// 采用列主序，因此这里用 `from_cols_array` 逐列填写，实现“转置 + 嵌入”：
/// 3x3 的线性部分进入左上 2x2，平移与投影分量分别落到第 4 列与第 4 行，
/// 空缺的 z 维保持单位向量，从而保证二维变换在三维齐次空间中等价。
pub fn nalgebra_to_glm(mat: &Matrix) -> Mat4 {
    /*
        [11] [12]  0  [13]
        [21] [22]  0  [23]
          0    0   1    0
        [31] [32]  0  [33]
    */
    Mat4::from_cols_array(&[
        mat.m11, mat.m21, 0., mat.m31, mat.m12, mat.m22, 0., mat.m32, 0., 0., 1., 0., mat.m13, mat.m23, 0., mat.m33,
    ])
}

/// 获取当前 GL 视口，返回 `(x, y, width, height)`（单位像素，原点在左下）。
///
/// 取值按以下顺序逐级退化，因为不同渲染阶段可用的信息不同：
/// 1. 已显式设置的 viewport；
/// 2. 当前渲染目标的纹理尺寸——渲染到离屏帧缓冲时必须用这一项，
///    否则会退回窗口尺寸，导致画面比例与分辨率不匹配；
/// 3. 屏幕尺寸（默认渲染目标）。
/// 返回值的宽高被 [`screen_aspect`] 用于计算画面比例，所以此处的回退顺序
/// 直接决定 UI 与谱面几何是否正确。
pub fn get_viewport() -> (i32, i32, i32, i32) {
    let gl = unsafe { get_internal_gl() };
    gl.quad_gl.get_viewport().unwrap_or_else(|| {
        let (w, h) = gl
            .quad_gl
            .get_active_render_pass()
            .map(|it| {
                let tex = it.texture(gl.quad_context);
                (tex.width as i32, tex.height as i32)
            })
            .unwrap_or_else(|| (screen_width() as _, screen_height() as _));
        (0, 0, w, h)
    })
}

/// 以 `anchor` 为锚点绘制一行文本：锚点各分量取 `0.0~1.0`，`(0.5, 0.5)` 即居中。
///
/// 把“位置 + 锚点 + 字号 + 颜色”的链式调用收敛为一个函数，避免各处重复书写；
/// 返回文本实际占据的矩形，可直接用于点击热区判定或后续排版。
#[inline]
pub fn draw_text_aligned(ui: &mut Ui, text: &str, x: f32, y: f32, anchor: (f32, f32), scale: f32, color: Color) -> Rect {
    ui.text(text).pos(x, y).anchor(anchor.0, anchor.1).size(scale).color(color).draw()
}

/// 图片在目标矩形内的适配方式，用于曲绘之类的“填满一个框”的绘制。
///
/// `rename_all = "camelCase"` 使图谱 / 配置中写的是 `cropCenter` 等形式。
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScaleType {
    /// 等比放大填满目标矩形，多出的部分从中心对称裁掉（cover 语义）。
    /// 作为默认值，因为曲绘几乎总是要求铺满背景，留白会非常显眼。
    #[default]
    CropCenter,
    /// 与 [`ScaleType::CropCenter`] 取图方向相反：在“有富余”的那一维上取
    /// 比纹理更宽的归一化源区域，使纹理相对目标区域显得更小（contain 方向）。
    /// 注意此时源区域可能越出纹理边界，越界部分由采样器的边缘钳制补齐。
    Inside,
    /// 不指定源区域，整张纹理被拉伸到目标矩形（`source` 留 `None` 交绘制调用处理）。
    Fit,
}

/// 计算把 `tex` 映射到 `rect` 时应采用的归一化源区域（相对纹理尺寸，取值 `0.0~1.0`）。
///
/// # Returns
/// - `Some(rect)`：应采用的源区域，该区域之外的纹理内容不参与绘制；
/// - `None`：[`ScaleType::Fit`] 的标记，表示无需裁剪、由调用方按整张纹理处理。
///
/// 判据是两个宽高比的比较：`exp` 是目标矩形的期望比例，`act` 是纹理自身比例。
/// 两者不等时，多出的那一维按类型决定是裁掉（CropCenter）还是缩进（Inside）；
/// `0.5 - k / 2` 的形式表示在此轴上居中，保证裁剪 / 收缩是中心对称的。
pub fn source_of_image(tex: &Texture2D, rect: Rect, scale_type: ScaleType) -> Option<Rect> {
    match scale_type {
        // cover：目标比纹理宽则纵向裁，比纹理窄则横向裁。
        ScaleType::CropCenter => {
            let exp = rect.w / rect.h;
            let act = tex.width() / tex.height();
            Some(if exp > act {
                let h = act / exp;
                Rect::new(0., 0.5 - h / 2., 1., h)
            } else {
                let w = exp / act;
                Rect::new(0.5 - w / 2., 0., w, 1.)
            })
        }
        // contain：与 cover 方向相反，在富余的那一维上取更宽的源区域。
        ScaleType::Inside => {
            let exp = rect.w / rect.h;
            let act = tex.width() / tex.height();
            Some(if exp > act {
                let w = exp / act;
                Rect::new(0.5 - w / 2., 0., w, 1.)
            } else {
                let h = act / exp;
                Rect::new(0., 0.5 - h / 2., 1., h)
            })
        }
        ScaleType::Fit => None,
    }
}

/// 把 `tex` 按 `scale_type` 适配绘制到 `rect`。
///
/// [`source_of_image`] 返回的是归一化源区域，而 macroquad 的
/// `DrawTextureParams::source` 接受纹素坐标，故这里乘回纹理宽高完成换算。
/// `dest_size` 固定为目标矩形大小——最终绘制总是铺满 `rect`，比例差异只通过选取源区域体现。
pub fn draw_image(tex: Texture2D, rect: Rect, scale_type: ScaleType) {
    let source = source_of_image(&tex, rect, scale_type);
    let (w, h) = (tex.width(), tex.height());
    draw_texture_ex(
        tex,
        rect.x,
        rect.y,
        WHITE,
        DrawTextureParams {
            source: source.map(|it| Rect::new(it.x * w, it.y * h, it.w * w, it.h * h)),
            dest_size: Some(rect.size()),
            ..Default::default()
        },
    );
}

/// 平行四边形的倾斜斜率，即“水平偏移量 / 高度”。
///
/// 保留 `0.13 / (7 / 13)` 的算式而不直接写常量，是为了让两项来源可见：
/// 判定线贴图本身带 7/13 的斜切比例，而视觉上需要的水平位移是高度的 0.13 倍，
/// 便于日后对照设计稿调整。量纲为无量纲比值，乘以 `rect.h` 后得到像素偏移。
pub const PARALLELOGRAM_SLOPE: f32 = 0.13 / (7. / 13.);

/// 绘制上下同色的平行四边形（判定线与部分 UI 的通用形状），可选投影。
/// 是允许上下异色的 [`draw_parallelogram_ex`] 的简化封装。
pub fn draw_parallelogram(rect: Rect, texture: Option<(Texture2D, Rect)>, color: Color, shadow: bool) {
    draw_parallelogram_ex(rect, texture, color, color, shadow);
}

/// 绘制平行四边形，上下可分别指定颜色（实现纵向渐变），并可选叠加投影。
///
/// # Arguments
/// * `rect` - 目标外接矩形；上下边的水平错开量为 `l = rect.h * PARALLELOGRAM_SLOPE`
/// * `texture` - 可选的 `(纹理, 源区域)`；为 `None` 时用纯顶点色填充
/// * `top` / `bottom` - 上、下顶点颜色，经光栅化插值形成渐变
/// * `shadow` - 是否在图形下方叠加投影
///
/// 顶点顺序为左上、右上、左下、右下，索引 `[0, 2, 3, 0, 1, 3]` 拼成两个三角形。
/// 贴图分支的 u 坐标同样按斜率错开（`lt`），否则纹理会被“正着”贴到斜边上而出现剪切扭曲。
/// 绘制通过 `quad_gl` 直接提交，因此必须在渲染阶段调用。
pub fn draw_parallelogram_ex(rect: Rect, texture: Option<(Texture2D, Rect)>, top: Color, bottom: Color, shadow: bool) {
    let l = rect.h * PARALLELOGRAM_SLOPE;
    let gl = unsafe { get_internal_gl() }.quad_gl;
    let p = [
        Point::new(rect.x + l, rect.y),
        Point::new(rect.right(), rect.y),
        Point::new(rect.x, rect.bottom()),
        Point::new(rect.right() - l, rect.bottom()),
    ];
    let v = if let Some((tex, tex_rect)) = texture {
        // 源区域的水平偏移量需从屏幕像素换算回归一化纹理坐标：
        // 高度方向的比例（tex_rect.h * 纹理高）先转成像素，再乘斜率，最后除以纹理宽。
        let lt = tex_rect.h * tex.height() * PARALLELOGRAM_SLOPE / tex.width();
        gl.texture(Some(tex));
        [
            Vertex::new(p[0].x, p[0].y, 0., tex_rect.x + lt, tex_rect.y, top),
            Vertex::new(p[1].x, p[1].y, 0., tex_rect.right(), tex_rect.y, top),
            Vertex::new(p[2].x, p[2].y, 0., tex_rect.x, tex_rect.bottom(), bottom),
            Vertex::new(p[3].x, p[3].y, 0., tex_rect.right() - lt, tex_rect.bottom(), bottom),
        ]
    } else {
        // 不贴图时 uv 无意义，全部填 0；顶点色仍参与渐变。
        gl.texture(None);
        [
            Vertex::new(p[0].x, p[0].y, 0., 0., 0., top),
            Vertex::new(p[1].x, p[1].y, 0., 0., 0., top),
            Vertex::new(p[2].x, p[2].y, 0., 0., 0., bottom),
            Vertex::new(p[3].x, p[3].y, 0., 0., 0., bottom),
        ]
    };
    gl.draw_mode(DrawMode::Triangles);
    gl.geometry(&v, &[0, 2, 3, 0, 1, 3]);
    if shadow {
        drop_shadow(p, top.a.min(bottom.a));
    }
}

/// 为平行四边形绘制投影：从四条边向外插值到透明黑，形成柔和的边缘过渡。
///
/// 阴影半径固定为 `RADIUS = 0.018`（屏幕空间的比例量级），颜色是
/// `alpha * 0.11` 的黑色——0.11 的系数让阴影足够淡，不会在深色曲绘上糊成一团。
/// 偏移方向由斜率推导出单位法线，保证投影垂直于图形边缘而不是简单水平位移。
/// `alpha` 传入上下顶点透明度的较小值，避免渐变到透明的一端出现硬边。
fn drop_shadow(p: [Point; 4], alpha: f32) {
    const RADIUS: f32 = 0.018;
    let len = (PARALLELOGRAM_SLOPE * PARALLELOGRAM_SLOPE + 1.).sqrt();
    let n1 = Vector::new(PARALLELOGRAM_SLOPE / len - 1., -1. / len) * RADIUS;
    let n2 = Vector::new(n1.x + RADIUS * 2., n1.y);
    let c1 = Color::new(0., 0., 0., alpha * 0.11);
    let c2 = Color::default();
    let v = |p: Point, c: Color| Vertex::new(p.x, p.y, 0., 0., 0., c);
    // 每条边由一对顶点组成：内圈取 c1（有透明度），外移后取 c2（全透明）。
    let p = [
        v(p[0], c1),
        v(p[0] + n1, c2),
        v(p[1], c1),
        v(p[1] + n2, c2),
        v(p[2], c1),
        v(p[2] - n2, c2),
        v(p[3], c1),
        v(p[3] - n1, c2),
    ];
    let gl = unsafe { get_internal_gl() }.quad_gl;
    gl.texture(None);
    gl.draw_mode(DrawMode::Triangles);
    gl.geometry(&p, &[0, 1, 2, 1, 2, 3, 0, 1, 5, 0, 5, 4, 4, 5, 6, 5, 6, 7, 6, 7, 2, 7, 2, 3]);
}

/// 为矩形绘制四周渐隐的投影。
///
/// 做法是把原矩形 `r` 与按 `radius` 羽化后的外框 `t` 组成两圈顶点：
/// 内圈用 `alpha` 的纯黑，外圈完全透明，光栅化插值后即得到边缘柔和消散的阴影。
/// 索引数组负责把这两圈顶点缝合成一个闭合的环带；
/// 因此 `radius` 越大阴影越“软”，但也会向四周多扩张同样的像素距离。
pub fn rect_shadow(r: Rect, radius: f32, alpha: f32) {
    let t = r.feather(radius);
    let v = |x: f32, y: f32, c: Color| Vertex::new(x, y, 0., 0., 0., c);
    let a = Color::new(0., 0., 0., alpha);
    let b = Color::default();
    let p = [
        v(t.x, t.y, b),
        v(t.right(), t.y, b),
        v(r.x, r.y, a),
        v(r.right(), r.y, a),
        v(r.x, r.bottom(), a),
        v(r.right(), r.bottom(), a),
        v(t.x, t.bottom(), b),
        v(t.right(), t.bottom(), b),
    ];
    let gl = unsafe { get_internal_gl() }.quad_gl;
    gl.texture(None);
    gl.draw_mode(DrawMode::Triangles);
    gl.geometry(&p, &[0, 1, 2, 1, 2, 3, 0, 2, 4, 4, 0, 6, 4, 5, 6, 5, 6, 7, 1, 3, 5, 5, 1, 7]);
}

/// 把一次性的阻塞闭包包装成 future：在独立 OS 线程上执行 `f`，主线程轮询结果。
///
/// 为什么需要它：桌面端的谱面加载、图片解码等操作没有异步 API，只能阻塞执行；
/// 放在主线程会卡住渲染，而走 [`spawn_task`] 又要求调用方处于 tokio 上下文中。
/// 这里直接用 `std::thread`，因此可在任意上下文调用。
///
/// 返回的 future 是“轮询式”的伪 future：它不注册有意义的 waker，
/// 每次 poll 只是查一次共享结果槽，因此**只能轮询、不能 `await` 等待**——
/// `await` 会立即得到 `Pending` 且无人唤醒。这与 [`crate::task::Task`] 的用法一致，
/// 都配合“每帧轮询”的游戏主循环使用。
/// 另外它不捕获 panic：`f` 一旦 panic，结果槽永远为空，future 将永不完成。
pub fn thread_as_future<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> impl Future<Output = R> {
    // 只做“查询结果槽”的 future：`Some` 即完成，`None` 即仍在运行。
    // 放在函数体内是因为它仅服务于本函数，无需污染模块命名空间。
    struct DummyFuture<R>(Arc<Mutex<Option<R>>>);
    // 实现最小可用的 Future 协议；没有 waker 注册，故 poll 是纯粹的状态查询。
    impl<R> Future for DummyFuture<R> {
        type Output = R;

        fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<Self::Output> {
            // take() 保证结果只被取出一次，避免重复交付。
            match self.0.lock().unwrap().take() {
                Some(res) => Poll::Ready(res),
                None => Poll::Pending,
            }
        }
    }
    let arc = Arc::new(Mutex::new(None));
    // 起一条独立线程执行闭包，结果写入共享槽；线程结束即完成，无需 join。
    std::thread::spawn({
        let arc = Arc::clone(&arc);
        move || {
            let res = f();
            *arc.lock().unwrap() = Some(res);
        }
    });
    DummyFuture(arc)
}

/// 在线程池中执行阻塞闭包 `f`，把结果作为可 `await` 的 future 返回。
///
/// 平台差异：wasm 没有抢占式多线程（`spawn_blocking` 语义不可用），
/// 于是直接同步执行 `f` 并返回就绪结果——代价是这一帧会被卡住，但浏览器下别无选择；
/// 其它平台交给 tokio 的阻塞线程池，`?` 会把 `JoinError`（任务 panic 或被取消）转成错误。
pub async fn spawn_task<R: Send + 'static>(f: impl FnOnce() -> Result<R> + Send + 'static) -> Result<R> {
    #[cfg(target_arch = "wasm32")]
    {
        f()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::task::spawn_blocking(f).await?
    }
}

/// 非阻塞地推进 `future` 一次：就绪返回 `Some(值)`，未就绪返回 `None`。
///
/// 实现方式是用一个“什么都不做”的自定义 waker 构造 `Context`：
/// 这类 future 的进度由外部（其它线程写入的结果槽 + 调用方每帧轮询）驱动，
/// 不需要唤醒回调，因此空 waker 完全够用。
/// 好处是任意 future 都能被塞进同步游戏循环，无需引入 executor。
/// 注意：必须在同一个 `Pin` 上反复调用同一次 poll 序列，重新构造 future 会从头执行。
pub fn poll_future<R>(future: Pin<&mut (impl Future<Output = R> + ?Sized)>) -> Option<R> {
    // 构造一个无状态 waker：不保存任务上下文，也不做任何事。
    // 之所以可行，是因为上层不依赖“被唤醒”来继续推进任务，只靠每帧主动 poll。
    fn waker() -> Waker {
        // 以下四个函数组成 RawWakerVTable。
        // SAFETY: `data` 恒为 null（见下方 `RawWaker::new`），这些函数都不解引用它，
        // 因此被任意线程、任意时刻调用都不会产生 UB。
        unsafe fn clone(data: *const ()) -> RawWaker {
            RawWaker::new(data, &VTABLE)
        }
        // 唤醒是空操作：真正的推进由调用方每帧 poll 驱动。
        //（原实现里保留了 `panic!()` 的痕迹，说明此处有意不实现唤醒语义。）
        unsafe fn wake(_data: *const ()) {
            // panic!()
        }
        unsafe fn wake_by_ref(data: *const ()) {
            wake(data)
        }
        unsafe fn drop(_data: *const ()) {}
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        let raw_waker = RawWaker::new(std::ptr::null(), &VTABLE);
        // SAFETY: vtable 的四个函数对任意 data 值（含 null）都安全，
        // 满足 `Waker::from_raw` 要求调用方保证的契约。
        unsafe { Waker::from_raw(raw_waker) }
    }
    let waker = waker();
    let mut futures_context = std::task::Context::from_waker(&waker);
    match future.poll(&mut futures_context) {
        Poll::Ready(val) => Some(val),
        Poll::Pending => None,
    }
}

/// 当前渲染目标的宽高比（视口宽 / 视口高）。
///
/// 注意取值来自 [`get_viewport`] 而非窗口尺寸：渲染到离屏帧缓冲时返回的是该
/// 帧缓冲的比例。谱面几何（判定线长度、音符大小）依赖这个比例换算，
/// 因此必须与实际绘制目标一致。
pub fn screen_aspect() -> f32 {
    let vp = get_viewport();
    vp.2 as f32 / vp.3 as f32
}
// This function is used to create in-game audio manager
/// 按平台创建游戏内音频管理器。
///
/// 三个分支对应三套原生后端，差异来自各平台可用的音频 API 与可靠性：
/// - Android：Oboe，显式声明 `LowLatency` 与 `Usage::Game`，让系统走游戏低延迟通路；
/// - OpenHarmony：OHOS 后端，缓冲大小缺省取 256 帧（在延迟与稳定性之间折中），
///   且强制双声道——部分 OHOS 设备的声道探测结果不可靠；
/// - 其它平台（桌面 / wasm）：cpal，直接透传用户配置的采样率与缓冲大小。
///
/// 函数名中的 `manger` 是历史拼写错误，为保持既有调用点不变而未重命名。
pub fn create_audio_manger(config: &Config) -> Result<AudioManager> {
    #[cfg(target_os = "android")]
    {
        use sasa::backend::oboe::*;
        AudioManager::new(OboeBackend::new(OboeSettings {
            buffer_size: config.audio_buffer_size,
            performance_mode: PerformanceMode::LowLatency,
            usage: Usage::Game,
        }))
    }
    #[cfg(target_env = "ohos")]
    {
        use sasa::backend::ohos::*;
        AudioManager::new(OhosBackend::new(OhosSettings {
            sample_rate: config.preferred_sample_rate.into(),
            buffer_size: config.audio_buffer_size.or(Some(256)),
            channels: 2,
        }))
    }
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    {
        use sasa::backend::cpal::*;
        AudioManager::new(CpalBackend::new(CpalSettings {
            preferred_sample_rate: config.preferred_sample_rate,
            buffer_size: config.audio_buffer_size,
        }))
    }
}

/// 使用本模块内置着色器创建一条渲染管线。
///
/// 之所以要自建管线：判定线的绘制需要模板测试（遮罩、裁剪），
/// 而 macroquad 的默认材质不暴露模板状态。这里固定了几项约定：
/// - 颜色是否写入由 `write_color` 控制，关闭时只写模板（stencil-only pass）；
/// - 混合固定为标准 alpha 混合 `src * srcA + dst * (1 - srcA)`；
/// - 正面与背面使用同一份模板状态，避免因子面设置不同导致半边失效；
/// - 模板读写掩码全为 1（即使用全部位）。
///
/// # Arguments
/// * `write_color` - 是否写入颜色缓冲；仅做遮罩时传 `false`
/// * `pass_op` - 模板测试通过时对模板缓冲的操作
/// * `test_func` - 模板比较函数
/// * `test_ref` - 参与比较的参考值
///
/// 末尾 `.unwrap()`：管线创建失败只可能源于着色器编译错误，属于启动期不可恢复故障。
pub fn make_pipeline(write_color: bool, pass_op: StencilOp, test_func: CompareFunc, test_ref: i32) -> GlPipeline {
    // 管线创建必须直接接触 quad_gl 与 quad_context，二者只能通过内部 GL 接口取得。
    let InternalGlContext {
        quad_gl: gl,
        quad_context: context,
    } = unsafe { get_internal_gl() };
    gl.make_pipeline(
        context,
        shader::VERTEX,
        shader::FRAGMENT,
        PipelineParams {
            color_write: (write_color, write_color, write_color, write_color),
            color_blend: Some(BlendState::new(
                Equation::Add,
                BlendFactor::Value(BlendValue::SourceAlpha),
                BlendFactor::OneMinusValue(BlendValue::SourceAlpha),
            )),
            stencil_test: {
                let state = StencilFaceState {
                    fail_op: StencilOp::Keep,
                    depth_fail_op: StencilOp::Keep,
                    pass_op,
                    test_func,
                    test_ref,
                    test_mask: u32::MAX,
                    write_mask: u32::MAX,
                };
                Some(StencilState { front: state, back: state })
            },
            primitive_type: PrimitiveType::Triangles,
            ..Default::default()
        },
        Vec::new(),
        Vec::new(),
    )
    .unwrap()
}

/// 生成指定透明度的黑色，常用于压暗谱面背景的遮罩层。
#[inline]
pub fn semi_black(alpha: f32) -> Color {
    Color::new(0., 0., 0., alpha)
}

/// 生成指定透明度的白色，常用于高亮或淡出用的遮罩层。
#[inline]
pub fn semi_white(alpha: f32) -> Color {
    Color::new(1., 1., 1., alpha)
}

/// 把 zip 归档全部解压到 `dir` 指定的目录。
///
/// # Arguments
/// * `reader` - 可定位的 zip 数据源（磁盘文件或内存缓冲）
/// * `dir` - 解压根目录，所有条目路径都经它做安全拼接
/// * `strip_root` - 是否剥掉包内唯一的顶层目录：很多谱面包会把所有文件塞在
///   一个同名子目录里，剥掉后目录结构才与预期一致
///
/// 剥壳判定条件刻意写得很严格：只有“最短的名字以 `/` 结尾”且“所有条目都以它为前缀”
/// 时才认定它是唯一的顶层目录，否则保持原结构，避免误判导致丢文件。
/// 所有路径都经过 `Dir::create_dir_all` / `Dir::create`，因此天然免疫 `../` 穿越；
/// 同时用 `enclosed_name` 拒绝畸形条目，确保解压结果不越出目标目录。
pub fn unzip_into<R: std::io::Read + std::io::Seek>(reader: R, dir: &crate::dir::Dir, strip_root: bool) -> Result<()> {
    let mut zip = zip::ZipArchive::new(reader)?;
    // 步骤 1：确定需要剥掉的顶层目录前缀（无需剥壳时为空串，等价于不剥）。
    let root = if strip_root {
        if let Some(root) = zip.file_names().min_by_key(|it| it.len()) {
            if root.ends_with('/') && zip.file_names().all(|it| it.starts_with(root)) {
                root.to_owned()
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let _span = info_span!("unzip").entered();
    debug!("root is {root}");
    // 步骤 2：逐条解压。目录直接创建；文件先按需补建父目录再写出内容。
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let path = entry.enclosed_name().ok_or_else(|| anyhow!("invalid zip"))?;
        let path = path.display().to_string();
        debug!("entry: {path}");
        if entry.is_dir() && entry.name() != root {
            // 目录条目：剥掉 root 前缀后按相对路径创建（`root` 自身会落在
            // 上面的 `entry.name() != root` 判断里被跳过）。
            if let Some(after) = path.strip_prefix(&root) {
                debug!("mkdir: {after}");
                dir.create_dir_all(after)?;
            }
        } else if entry.is_file() {
            if let Some(after) = path.strip_prefix(&root) {
                // zip 不保证父目录条目存在（有些打包器会省略），故写文件前先补建。
                if let Some(p) = std::path::Path::new(after).parent() {
                    if !dir.exists(p)? {
                        debug!("mkdir {}", p.display());
                        dir.create_dir_all(p)?;
                    }
                }
                debug!("create {}", after);
                let mut file = dir.create(after)?;
                std::io::copy(&mut entry, &mut file)?;
            }
        }
    }
    Ok(())
}

/// 解析 `[小时:][分钟:]秒` 形式的时长文本，返回秒数。
///
/// 规则：空串返回 `None`；最多三段，超过三段视为非法；从右往左依次按
/// 秒 / 分 / 时解释，因此 `"90"` 是 90 秒，而 `"1:30"` 是 1 分 30 秒。
/// 秒段允许小数且必须非负（负值直接判非法），分与时只接受非负整数。
/// 任意一段解析失败都返回 `None`，由调用方决定是否回退到默认值。
pub fn parse_time(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let r = s.split(':').collect::<Vec<_>>();
    if r.len() > 3 {
        return None;
    }
    // 反转后迭代：第一项是秒、第二项是分、第三项是时。
    let mut iter = r.into_iter().rev();
    let mut res = iter.next().unwrap().parse::<f64>().ok()?;
    if res < 0. {
        return None;
    }
    if let Some(mins) = iter.next() {
        res += mins.parse::<u32>().ok()? as f64 * 60.;
    }
    if let Some(hrs) = iter.next() {
        res += hrs.parse::<u32>().ok()? as f64 * 3600.;
    }
    Some(res)
}

/// 用系统默认方式打开一个 URL（例如跳转官网、反馈页或用户主页）。
///
/// 之所以要分四个平台分支：打开外部链接在各平台没有统一 API。
/// - Android：通过 JNI 反射调用宿主 Activity 的 `openUrl(String)`，
///   只能借助 Java 层启动 Intent，Rust 侧无法直接打开浏览器；
/// - iOS：使用 `UIApplication::openURL`，它要求持有 `MainThreadMarker`；
/// - OpenHarmony：把动作编码为 JSON，经 miniquad 的请求回调交给宿主 ArkTS 处理；
/// - 其它平台（桌面）：交给 `open` crate 调用系统默认程序。
///
/// # Errors
/// 仅桌面分支会把启动失败作为错误返回。
/// 移动端分支内部使用了 `unwrap`：宿主环境缺失属于集成问题，无法在运行时优雅恢复。
pub fn open_url(url: &str) -> Result<()> {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "android")] {
            // 走 JNI 调用宿主 Activity 的方法：需要手工取类、取方法 id 并构造 Java 字符串，
            // 方法签名 `(Ljava/lang/String;)V` 必须与 Java 侧完全一致。
            unsafe {
                let env = miniquad::native::attach_jni_env();
                let ctx = ndk_context::android_context().context();
                let class = (**env).GetObjectClass.unwrap()(env, ctx);
                let method =
                    (**env).GetMethodID.unwrap()(env, class, c"openUrl".as_ptr() as _, c"(Ljava/lang/String;)V".as_ptr() as _);
                let url = std::ffi::CString::new(url.to_owned()).unwrap();
                (**env).CallVoidMethod.unwrap()(
                    env,
                    ctx,
                    method,
                    (**env).NewStringUTF.unwrap()(env, url.as_ptr()),
                );
            }
        } else if #[cfg(target_os = "ios")] {
            // iOS 的 UI 操作必须在主线程：`MainThreadMarker::new()` 在非主线程返回 `None`，
            // 因此本分支（实际上整个函数）在 iOS 上只能由主线程调用。
            use objc2::MainThreadMarker;
            use objc2_foundation::{NSString, NSURL, NSDictionary};
            use objc2_ui_kit::UIApplication;

            let mtm = MainThreadMarker::new().unwrap();
            let url = NSURL::URLWithString(&NSString::from_str(url)).unwrap();
            // SAFETY: options are empty
            unsafe {
                UIApplication::sharedApplication(mtm).openURL_options_completionHandler(&url, &NSDictionary::new(), None);
            }
        } else if #[cfg(target_env = "ohos")] {
            // 沙箱限制使 Rust 侧无法直接拉起浏览器，改为把动作序列化成 JSON 交给宿主处理。
            miniquad::native::call_request_callback(format!("{{\"action\":\"openurl\",\"payload\":\"{}\"}}", url));
        }
        else {
            // 桌面平台：交给系统默认程序，失败会向上传播为错误。
            open::that(url)?;
        }
    }

    Ok(())
}

// 判定线渲染所用的最小着色器对：只做顶点变换、颜色归一化与纹理采样。
// 自带着色器而非复用 macroquad 默认材质，是因为判定线需要“自定义 Model 矩阵 +
// 模板测试”的组合，而默认材质不暴露模板状态（相关管线见 make_pipeline）。
mod shader {
    /// 顶点着色器（GLSL ES 1.00，兼容移动端 GLES2）。
    /// 注意 `color0` 按 macroquad 的 `Vertex` 约定以 0~255 传入，故此处除以 255 归一化。
    pub const VERTEX: &str = r#"#version 100
attribute vec3 position;
attribute vec2 texcoord;
attribute vec4 color0;

varying lowp vec2 uv;
varying lowp vec4 color;

uniform mat4 Model;
uniform mat4 Projection;

void main() {
    gl_Position = Projection * Model * vec4(position, 1);
    color = color0 / 255.0;
    uv = texcoord;
}"#;

    /// 片段着色器：把顶点色与纹理采样值相乘作为最终颜色。
    /// 纯色绘制时由调用方绑定 1x1 的纹理（见 [`super::BLACK_TEXTURE`]），
    /// 使乘法退化为直接输出顶点色。
    pub const FRAGMENT: &str = r#"#version 100
varying lowp vec4 color;
varying lowp vec2 uv;

uniform sampler2D Texture;

void main() {
    gl_FragColor = color * texture2D(Texture, uv);
}"#;
}
