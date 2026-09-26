//! 文字绘制与字形图集缓存。
//!
//! 本模块**不使用 macroquad 的 `draw_text`**：后者每次调用都会重新上传字体纹理，
//! 且只支持单一字体，无法满足本项目的需求（大量动态文本、主字体缺字时的回退字体、
//! 以及跟随 UI 变换与整体透明度的绘制）。因此这里自维护一套 `GlyphBrush` 字形图集：
//! - 新字形只把对应的小矩形区域用 `glTexSubImage2D` **增量**写入图集，避免整张纹理重传；
//! - 图集放不下时（`BrushError::TextureTooSmall`）才按建议尺寸重建更大的纹理；
//! - `redraw` 把已就绪的字形 quad 逐批提交，纹理只绑定一次。
//!
//! 图集边长上限由 `TEXTURE_DIM` 决定（`GL_MAX_TEXTURE_SIZE` 与 2048 取小）。
use super::Ui;
use crate::{
    core::{Matrix, Point, Vector},
    ext::get_viewport,
};
use glyph_brush::{
    ab_glyph::{Font, FontArc, ScaleFont},
    BrushAction, BrushError, FontId, GlyphBrush, GlyphBrushBuilder, GlyphCruncher, HorizontalAlign, Layout, Section, SectionGlyph, Text,
};
use macroquad::{
    miniquad::{Texture, TextureParams},
    prelude::*,
};
use once_cell::sync::Lazy;
use std::{borrow::Cow, cell::RefCell, thread::LocalKey};
use tracing::debug;

/// 文字绘制的惰性构建器（builder）：链式设置字号/位置/锚点/颜色等，最后调用 `draw()`
/// 或 `measure()` 才真正产生绘制/布局结果。
/// 之所以让 `ui.text(..)` 返回 builder 而不是立即绘制：一是需要在绘制前拿到尺寸用于居中、
/// 截断等布局计算；二是同一段文本可能需要多次测量后再决定是否绘制。
#[must_use = "DrawText does nothing until you 'draw' it"]
pub struct DrawText<'a, 's, 'ui> {
    /// 所属的 UI 上下文。持有 `&mut` 意味着同一个 `Ui` 上不能同时存在两个活跃 builder。
    pub ui: &'ui mut Ui<'a>,
    /// 待绘制文本。`draw`/`measure` 需要把文本 `take` 出来（避免与 `&mut ui` 的借用冲突，
    /// 见 `draw_with_font`），用完再放回，因此正常流程结束后它始终是 `Some`。
    text: Option<Cow<'s, str>>,
    /// 逻辑字号，实际缩放由 `get_scale` 按视口宽度换算成字形像素尺寸。
    size: f32,
    /// 绘制参考点（局部归一化坐标）；含义随 `baseline` 变化。
    pos: (f32, f32),
    /// 锚点：包围盒相对 `pos` 的偏移比例。`0` 表示以 `pos` 为左上边、`1` 为右下边、`0.5` 为居中。
    anchor: (f32, f32),
    /// 文字颜色；其 alpha 还会再乘上 `Ui::alpha`。
    color: Color,
    /// 最大宽度限制（局部归一化长度）。超出时单行模式会截断并追加省略号。
    max_width: Option<f32>,
    /// `true` 时 `pos` 表示文本**基线**；`false`（`no_baseline()`）时表示文本行顶部。
    baseline: bool,
    /// 是否允许多行（支持 `\n` 换行与多行高度测量）。
    multiline: bool,
    /// 额外的字形局部变换（在归一化坐标系中作用于每个字形，可用于斜体/旋转）。
    scale: Matrix,
    /// 段内水平对齐方式；`h_center()` 会设为居中。
    h_align: HorizontalAlign,
}

// 默认值语义：单行、左对齐、左上锚点、白色、按基线定位。
impl<'a, 's, 'ui> DrawText<'a, 's, 'ui> {
    /// 仅供 `Ui::text` 调用；外部请用 `ui.text(..)` 构造。
    pub(crate) fn new(ui: &'ui mut Ui<'a>, text: Cow<'s, str>) -> Self {
        Self {
            ui,
            text: Some(text),
            size: 1.,
            pos: (0., 0.),
            anchor: (0., 0.),
            color: WHITE,
            max_width: None,
            baseline: true,
            multiline: false,
            scale: Matrix::identity(),
            h_align: HorizontalAlign::Left,
        }
    }

    /// 段内水平居中对齐（`Layout::h_align`）。
    /// 与 `anchor(0.5, ..)` 的区别：`anchor` 决定整个包围盒相对 `pos` 的位置，
    /// 而本方法决定**多行文本每行**在包围盒内的排布方式。
    pub fn h_center(mut self) -> Self {
        self.h_align = HorizontalAlign::Center;
        self
    }

    /// 设置逻辑字号。
    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    /// 设置绘制参考点（局部归一化坐标，含义随 `baseline` 变化）。
    pub fn pos(mut self, x: f32, y: f32) -> Self {
        self.pos = (x, y);
        self
    }

    /// 设置锚点：`pos` 相对文字包围盒的位置，取 0/0.5/1。
    pub fn anchor(mut self, x: f32, y: f32) -> Self {
        self.anchor = (x, y);
        self
    }

    /// 设置文字颜色。
    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    /// 限制最大宽度（局部归一化长度）。单行模式下超出部分会被替换成省略号（见 `measure_inner`）。
    pub fn max_width(mut self, max_width: f32) -> Self {
        self.max_width = Some(max_width);
        self
    }

    /// 让 `pos` 表示文本行的顶部而非基线。
    /// 布局代码（如按钮内居中）通常希望“以盒子对齐”，而不必关心字体自身的 ascent/descent。
    pub fn no_baseline(mut self) -> Self {
        self.baseline = false;
        self
    }

    /// 允许换行：`\n` 生效且测量返回多行高度。默认单行，避免意外的宽度溢出。
    pub fn multiline(mut self) -> Self {
        self.multiline = true;
        self
    }

    /// 设置作用于每个字形的附加变换（在归一化坐标系中，`Ui::text` 派生出的子绘制器常用它做整体缩放）。
    pub fn scale(mut self, scale: Matrix) -> Self {
        self.scale = scale;
        self
    }

    /// 逻辑字号 -> 字形像素尺寸的换算：`0.04 * size * 视口宽度`。
    /// 系数 0.04 的来源是归一化坐标区间长度为 2（屏幕宽度对应 2 个单位），
    /// 因此 `size = 1` 时字高约为屏幕宽度的 4%；这样同一套 UI 在任何分辨率与宽高比下观感一致。
    fn get_scale(&self, w: i32) -> f32 {
        0.04 * self.size * w as f32
    }

    /// 把 `measure_inner` 返回的字形空间包围盒换算成 UI 归一化矩形。
    /// `s = 2 / 视口宽` 是“像素 -> 归一化”的比例；`anchor` 决定包围盒相对 `pos` 向哪个方向
    /// 偏移自身尺寸（0 表示 `pos` 是左上角，1 表示右下角）。
    fn bounds(&self, (x, y, w, h): (f32, f32, f32, f32)) -> Rect {
        let vp = get_viewport();
        let s = 2. / vp.2 as f32;
        let mut rect = Rect::new(self.pos.0 - x * s, self.pos.1 - y * s, w * s, h * s);
        rect.x -= rect.w * self.anchor.0;
        rect.y -= rect.h * self.anchor.1;
        rect
    }

    /// 测量与排版的**核心实现**：选定绘制器、按需做字体回退分段、按需截断或换行，
    /// 返回 `(待入队的 Section, 字形空间包围盒 (x, y, w, h))`。
    ///
    /// 量纲说明：`Section` 的缩放是视口**像素**（见 `get_scale`），因此这里的包围盒也是像素，
    /// 由 `bounds` 负责换算成 UI 归一化坐标。
    /// `painter` 为 `None` 时使用 `ui.text_painter`（默认字体）。
    fn measure_inner<'c>(&mut self, text: &'c str, painter: &mut Option<&mut TextPainter>) -> (Section<'c>, (f32, f32, f32, f32)) {
        use glyph_brush::ab_glyph;
        let vp = get_viewport();
        let scale = self.get_scale(vp.2);

        // 选择绘制器：显式传入的优先（如宿主提供的 PGR_FONT/BOLD_FONT），否则退回 Ui 自带的默认绘制器。
        let default_text_painter = &mut self.ui.text_painter;
        let painter = painter.as_deref_mut().unwrap_or(default_text_painter);

        let mut section = Section::new().with_layout(Layout::default().h_align(self.h_align));
        // 字体回退：按“主字体能否渲染该字符”把文本切成若干段。
        // 判定依据是 glyph_id 为 0（.notdef，表示主字体缺字），此时该段交给 FontId(1) 即回退字体；
        // 空白字符固定归主字体，否则两套字体的空格宽度不同会破坏对齐。
        // 只在状态翻转时才新增一个 Text，因此连续可渲染的长文本仍然只是一段。
        if painter.brush.fonts().len() > 1 {
            let mut last = 0;
            let mut last_contain = false;
            for (i, c) in text.char_indices() {
                let contain = " \n\t".contains(c) || painter.brush.fonts()[0].glyph_id(c).0 != 0;
                if last_contain != contain {
                    if last != i {
                        section = section.add_text(
                            Text::new(&text[last..i])
                                .with_scale(scale)
                                .with_color(self.color)
                                .with_font_id(FontId((!last_contain) as usize)),
                        );
                    }
                    last = i;
                    last_contain = contain;
                }
            }
            if last != text.len() {
                section = section.add_text(
                    Text::new(&text[last..])
                        .with_scale(scale)
                        .with_color(self.color)
                        .with_font_id(FontId((!last_contain) as usize)),
                );
            }
        } else {
            section = section.add_text(Text::new(text).with_scale(scale).with_color(self.color));
        }

        // 把归一化宽度换算成像素交给 glyph_brush（除以 s 等于乘以视口宽/2）；高度不限制，
        // 是否换行完全由宽度决定。
        let s = 2. / vp.2 as f32;
        if let Some(max_width) = self.max_width {
            section = section.with_bounds((max_width / s, f32::INFINITY));
        }
        // 行高按定位方式区分：基线定位用 ascent（基线以上部分），
        // 否则用整行高度 `font.height()`，便于调用方按“盒子”对齐。
        let font = painter.brush.fonts()[0].as_scaled(scale);
        let line_height = if self.baseline { font.ascent() } else { font.height() };

        if !self.multiline {
            let bounds = section.bounds;
            let bounds = ab_glyph::Rect {
                min: ab_glyph::Point { x: 0., y: 0. },
                max: ab_glyph::Point { x: bounds.0, y: bounds.1 },
            };
            // 先解除宽度限制：要截断就得先知道“不换行时整行有多长”，
            // 才能算出从哪个字形开始放不下。这一改动只影响本次测量，不回写调用方状态。
            section.bounds.0 = f32::INFINITY;
            let glyphs: Vec<_> = painter.brush.glyphs(section.clone()).cloned().collect();
            // 没有任何字形（空串或全是不可见字符）时仍返回行高：让空标签/空输入框
            // 在布局上依然占据一行，避免周围元素跳动。
            let Some(last) = glyphs.last() else {
                return (section, (0., 0., 0., line_height));
            };
            let end = |glyph: &SectionGlyph| glyph.glyph.position.x + painter.brush.fonts()[glyph.font_id].as_scaled(scale).h_advance(glyph.glyph.id);
            // 恰好放得下：无需截断，宽度直接取最后一个字形的右边界。
            if end(last) <= bounds.max.x {
                return (section, (0., 0., end(last), line_height));
            }
            // 连省略号自身都放不下时干脆不画（宽度记为 0），
            // 否则会出现只剩半个省略号、或超出容器的绘制。
            let font = painter.brush.fonts()[0].as_scaled(scale);
            let id = font.glyph_id('…');
            let w = font.h_advance(id);
            if w > bounds.max.x {
                return (section, (0., 0., 0., line_height));
            }
            // 用二分找到第一个“加上省略号后会超出”的字形位置，即可以保留的前缀长度。
            // 用 `partition_point` 而非线性扫描，保证长文本的测量仍是 O(log n)。
            let index = glyphs.partition_point(|it| end(it) <= bounds.max.x - w);
            let st = if index == 0 { 0. } else { end(&glyphs[index - 1]) };
            let byte_index = if index == 0 { 0 } else { glyphs[index - 1].byte_index };
            // Round to char boundary
            // 把字节下标退回到合法的字符边界：`byte_index` 可能落在多字节字符中间，
            // 直接切片会 panic，故取前一个字符的起始位置作为安全边界。
            let byte_index = text[..byte_index].char_indices().next_back().map_or(0, |(i, _)| i);
            // 用“截断后的前缀 + 单独一个省略号”两段替换原内容，
            // 这样省略号不受回退字体影响（固定用主字体），宽度上报为最后一字形的右边界加省略号宽度。
            return (
                section.with_text(vec![
                    Text::new(&text[..byte_index]).with_scale(scale).with_color(self.color),
                    Text::new("…").with_scale(scale).with_color(self.color),
                ]),
                (0., 0., st + w, line_height),
            );
        }
        // 多行模式：直接采用 glyph_brush 计算的字形包围盒，并补上两处缺口——
        // 1) 开头的连续换行不会产生字形，包围盒里体现不出高度，但用户期望它们占位，
        //    因此按 `\n` 个数补 `line_gap * 3`（系数 3 是经验值，使空行接近正常行高）；
        // 2) 基线定位时包围盒需要向下扩展到 descent，否则紧邻的元素会侵入文本下沿。
        let bound = painter.brush.glyph_bounds(&section).unwrap_or_default();
        let mut height = bound.height();
        height += text.chars().take_while(|it| *it == '\n').count() as f32 * painter.line_gap(scale) * 3.;
        if self.baseline {
            height += painter.brush.fonts()[0].as_scaled(scale).descent();
        }
        (section, (bound.min.x, bound.min.y, bound.width(), height))
    }

    /// 用指定绘制器测量文字包围盒（局部归一化坐标）；`None` 表示使用 `ui.text_painter`。
    /// 其中 `take` 出文本是为了绕开“`&mut ui` 与 `self.text` 同时被可变借用”的问题，
    /// 测完原样放回，使 builder 可以重复 measure/draw 而不消耗自身。
    pub fn measure_with_font(&mut self, mut painter: Option<&mut TextPainter>) -> Rect {
        let text = self.text.take().unwrap();
        let (_, bound) = self.measure_inner(&text, &mut painter);
        self.text = Some(text);
        self.bounds(bound)
    }

    /// 用宿主提供的 thread_local 字体绘制器测量（如 `PGR_FONT`/`BOLD_FONT`）。
    /// 与 `draw_using` 配对使用，保证“测量结果”与“实际绘制”来自同一字体与同一图集。
    pub fn measure_using(&mut self, font: &'static LocalKey<RefCell<Option<TextPainter>>>) -> Rect {
        font.with(|it| self.measure_with_font(it.borrow_mut().as_mut()))
    }

    /// 用 `ui.text_painter` 测量文字包围盒（最常用入口，返回局部归一化坐标）。
    #[inline]
    pub fn measure(&mut self) -> Rect {
        self.measure_with_font(None)
    }

    /// 用指定绘制器绘制文字，返回其归一化包围盒。
    ///
    /// 关键点：字形是以**视口像素**坐标入队的（见 `measure_inner`），
    /// 因此这里先叠一层 `Matrix::new_scaling(1/s)` 把像素换算回归一化坐标，
    /// 再附加调用方通过 `scale` 给出的变换；整体平移则由 `bounds` 算出的位置决定。
    /// 提交发生在 `ui.with(..)` 内部，使字形与其它 UI 元素共享同一套变换、裁剪与全局透明度。
    pub fn draw_with_font(&mut self, mut painter: Option<&mut TextPainter>) -> Rect {
        let text = std::mem::take(&mut self.text).unwrap();
        let (section, bound) = self.measure_inner(&text, &mut painter);
        let rect = self.bounds(bound);
        let vp = get_viewport();
        let s = vp.2 as f32 / 2.;
        // 入队：显式传入的绘制器优先，否则用 ui 自带的；与 `measure` 保持一致，
        // 避免“测量用一种字体、绘制用另一种字体”导致宽度错位。
        if let Some(painter) = &mut painter {
            painter.brush.queue(section);
        } else {
            self.ui.text_painter.brush.queue(section);
        }
        // 在套上 1/s 缩放与整体平移后提交：提交必须发生在正确的 `ui.transform` 之下，
        // 字形才会跟随外层布局与裁剪（上面注释掉的旧实现直接传单位矩阵，故已废弃）。
        self.ui
            .with((Matrix::new_scaling(1. / s) * self.scale).append_translation(&Vector::new(rect.x, rect.y)), |ui| {
                /* ui.apply(|ui| {
                    let tr = Matrix::identity();
                    if let Some(painter) = painter {
                        painter.submit(tr, ui.alpha);
                    } else {
                        ui.text_painter.submit(tr, ui.alpha);
                    }
                }); */
                if let Some(painter) = painter {
                    painter.submit(ui.transform, ui.alpha);
                } else {
                    ui.text_painter.submit(ui.transform, ui.alpha);
                }
            });
        // 把文本放回 builder（字段是 `Option`，用 `take` 取出后再放回），
        // 使同一次绘制之后仍可继续 measure/draw 而不会丢失内容。
        self.text = Some(text);
        rect
    }

    /// 用宿主提供的 thread_local 绘制器绘制文字（`PGR_FONT` 为谱面风格字体、`BOLD_FONT` 为粗体标题）。
    /// 这两个绘制器由宿主在加载字体资源后初始化；未初始化（`None`）时退回 `ui.text_painter`。
    pub fn draw_using(&mut self, font: &'static LocalKey<RefCell<Option<TextPainter>>>) -> Rect {
        font.with(|it| self.draw_with_font(it.borrow_mut().as_mut()))
    }

    /// 用 `ui.text_painter` 绘制文字（默认路径）。
    #[inline]
    pub fn draw(&mut self) -> Rect {
        self.draw_with_font(None)
    }
}

/// 字形图集的边长上限，取 `GL_MAX_TEXTURE_SIZE` 与 2048 的较小值。
/// 为什么还要再夹到 2048：超大图集在多平台上会显著增加显存占用与纹理上传带宽，
/// 而 UI 同一屏内的字形数量有限；2048² 的图集已能容纳数千个中文字形，
/// 超出时由 `submit` 的重建逻辑按建议尺寸扩容，因此上限只是首次分配的规模。
static TEXTURE_DIM: Lazy<u32> = Lazy::new(|| unsafe {
    use miniquad::gl::*;
    let mut size = 0;
    // SAFETY: 只在 UI 渲染线程读取 GL 常量上限，写出目标是本地栈变量 `size`，无别名风险；
    // 调用前 `size` 已初始化为 0，因此不会读取到未初始化的值。
    glGetIntegerv(GL_MAX_TEXTURE_SIZE, &mut size);
    (size as u32).min(2048)
});

/// 图集绘制用的单个顶点。四个顶点（左上/右上/左下/右下）组成一个字形的 quad，
/// 因此 `GlyphBrush` 的顶点输出类型是 `[MyVertex; 4]`。
/// `pos` 是**屏幕像素**位置（尚未乘 UI 变换），`uv` 是图集内的归一化坐标，
/// 两者量纲不同、由 `transform` 统一换算，切勿混用。
#[derive(Clone)]
struct MyVertex {
    /// 位置，单位是屏幕像素（视口坐标），后续由 `TextPainter::transform` 投到 UI 坐标。
    pos: (f32, f32),
    /// 字形在图集内的归一化 uv 坐标。
    uv: (f32, f32),
    /// 该字形的颜色（alpha 已乘上 `Ui::alpha`）。
    color: Color,
}
// 仅用于 `redraw` 内部构造：用独立构造函数而不是字面量，避免四元参数的顺序写错。
impl MyVertex {
    /// 按（像素位置, uv, 颜色）构造顶点。
    pub fn new(x: f32, y: f32, u: f32, v: f32, color: Color) -> Self {
        Self {
            pos: (x, y),
            uv: (u, v),
            color,
        }
    }
}

/// 字形图集绘制器：持有 `GlyphBrush`（负责把文本排成字形并管理图集布局）与自建的 GL 纹理。
///
/// 与 macroquad 自带 `draw_text` 的关键差别：字形只在首次出现时上传一次，
/// 此后每帧只把新字形的**小块区域**写进图集（`glTexSubImage2D`），因此文本量增大时开销稳定；
/// 代价是需要自己管理图集尺寸、纹理生命周期与重建。
pub struct TextPainter {
    /// glyph_brush 的排版/图集状态机；泛型参数 `[MyVertex; 4]` 决定每个字形的顶点输出格式。
    brush: GlyphBrush<[MyVertex; 4]>,
    /// 字形图集纹理。RGBA8、线性过滤、`Clamp` 寻址——Clamp 可避免相邻字形的边缘互相渗色。
    cache_texture: Texture2D,
    /// 复用的 RGBA 上传缓冲：`glyph_brush` 回调只给单通道 alpha 数据，
    /// 需要就地展开成 RGBA 才能上传，复用缓冲可避免每次上传都分配内存。
    data_buffer: Vec<u8>,
    /// 复用的顶点缓冲，避免每帧为字形 quad 重新分配 `Vec`。
    vertices_buffer: Vec<MyVertex>,
}

// 构造与查询。构造时即按 `TEXTURE_DIM` 预分配图集，避免首帧才扩容。
impl TextPainter {
    /// 用主字体与可选回退字体创建绘制器。
    /// 两个字体注册在同一个 `GlyphBrush`：`FontId(0)` 为主字体、`FontId(1)` 为回退字体
    /// （分段逻辑见 `DrawText::measure_inner`），共用一张图集以简化纹理管理。
    pub fn new(font: FontArc, fallback: Option<FontArc>) -> Self {
        let mut fonts = vec![font];
        if let Some(fallback) = fallback {
            fonts.push(fallback);
        }
        let mut brush = GlyphBrushBuilder::using_fonts(fonts).build();
        let dim = *TEXTURE_DIM;
        brush.resize_texture(dim, dim);
        // TODO optimize
        let cache_texture = Self::new_cache_texture(brush.texture_dimensions());
        Self {
            brush,
            cache_texture,
            data_buffer: Vec::new(),
            vertices_buffer: Vec::new(),
        }
    }

    /// 创建（或重建）图集纹理。
    /// 用 `new_render_texture` 而不是上传初始数据：需要的是一张**空**的、
    /// 可以被 `glTexSubImage2D` 局部写入的纹理；尺寸直接取自 `brush.texture_dimensions()`，
    /// 保证布局器认定的图集大小与真实纹理一致（不一致会导致字形错位）。
    fn new_cache_texture(dim: (u32, u32)) -> Texture2D {
        debug!("creating cache texture: {}x{}", dim.0, dim.1);
        Texture2D::from_miniquad_texture(Texture::new_render_texture(
            // SAFETY: 纹理创建必须发生在持有 GL 上下文的主渲染线程，本函数只由 `TextPainter`
            // 的构造与 `submit` 调用，两者都在该线程内。
            unsafe { get_internal_gl() }.quad_context,
            TextureParams {
                width: dim.0,
                height: dim.1,
                filter: FilterMode::Linear,
                format: miniquad::TextureFormat::RGBA8,
                wrap: miniquad::TextureWrap::Clamp,
            },
        ))
    }

    /// 主字体在给定缩放下的行间距，供调用方（如多行文本排版）计算行高。
    /// 固定用 `fonts()[0]`：行距属于版式参数，用主字体可保证不同回退组合下的行距一致。
    pub fn line_gap(&self, scale: f32) -> f32 {
        self.brush.fonts()[0].as_scaled(scale).line_gap()
    }

    /// 把已入队的文本提交到图集与 GL：驱动 `glyph_brush` 的 `process_queued` 状态机直到得到可绘制的顶点。
    ///
    /// 三种结果：
    /// - 上传回调：把新增字形所在的脏矩形展开成 RGBA 后用 `glTexSubImage2D` **局部**写入图集，
    ///   避免整张图集重传（这是本模块存在的性能动机）；
    /// - `TextureTooSmall`：图集放不下新字形，删除并按建议尺寸重建后重试；
    /// - `Draw`/`ReDraw`：字形已就绪，交给 `redraw` 逐字形提交 quad。
    ///
    /// `tr` 是字形（像素坐标）到 UI 坐标的变换，`alpha` 是全局透明度乘子。
    /// `flushed` 保证同一轮里最多 `flush` 一次：上传纹理前必须让 `quad_gl` 把已累积的绘制批次刷出，
    /// 否则那些批次会带着被本函数改动的纹理绑定状态一起执行。
    fn submit(&mut self, tr: Matrix, alpha: f32) {
        let mut flushed = false;
        loop {
            match self.brush.process_queued(
                |rect, tex_data| unsafe {
                    // SAFETY: 回调在 UI 渲染线程同步执行；此处独占 GL 状态，
                    // 上传前先 flush 并把图集纹理绑定到当前纹理单元，保证后续绘制引用的是正确纹理。
                    if !flushed {
                        get_internal_gl().flush();
                        flushed = true;
                    }
                    use miniquad::gl::*;
                    glBindTexture(GL_TEXTURE_2D, self.cache_texture.raw_miniquad_texture_handle().gl_internal_id());
                    // 把单通道的覆盖度数据展开成 RGBA（RGB 固定为白，真正的颜色由顶点携带）。
                    self.data_buffer.clear();
                    self.data_buffer.reserve(tex_data.len() * 4);
                    for alpha in tex_data {
                        self.data_buffer.extend_from_slice(&[255, 255, 255, *alpha]);
                    }
                    // 只写 `rect` 指定的脏矩形：这是相对“整张图集重传”的关键优化，
                    // 新增字形的面积通常远小于整张 2048² 图集。
                    glTexSubImage2D(
                        GL_TEXTURE_2D,
                        0,
                        rect.min[0] as _,
                        rect.min[1] as _,
                        rect.width() as _,
                        rect.height() as _,
                        GL_RGBA,
                        GL_UNSIGNED_BYTE,
                        self.data_buffer.as_ptr() as _,
                    );
                },
                |vertex| {
                    let pos = &vertex.pixel_coords;
                    let uv = &vertex.tex_coords;
                    // 每个字形的颜色还要乘上 UI 的全局透明度，这样整体淡出对文字同样生效。
                    let mut color: Color = vertex.extra.color.into();
                    color.a *= alpha;
                    [
                        MyVertex::new(pos.min.x, pos.min.y, uv.min.x, uv.min.y, color),
                        MyVertex::new(pos.max.x, pos.min.y, uv.max.x, uv.min.y, color),
                        MyVertex::new(pos.min.x, pos.max.y, uv.min.x, uv.max.y, color),
                        MyVertex::new(pos.max.x, pos.max.y, uv.max.x, uv.max.y, color),
                    ]
                },
            ) {
                // 图集装不下新字形：按布局器建议的尺寸重建纹理与图集，然后重新 process。
                // 重建会丢弃原有字形布局，因此必须重跑一次（`loop` 而非递归，避免栈增长）。
                Err(BrushError::TextureTooSmall { suggested }) => {
                    if !flushed {
                        // SAFETY: 与上传回调相同——同一线程独占 GL 状态；先 flush 再删除纹理，
                        // 确保没有待执行的批次仍引用即将被删除的纹理。
                        unsafe { get_internal_gl() }.flush();
                        flushed = true;
                    }
                    self.cache_texture.delete();
                    self.cache_texture = Self::new_cache_texture(suggested);
                    self.brush.resize_texture(suggested.0, suggested.1);
                }
                // 有新字形需要绘制：把 `[MyVertex; 4]` 摊平成连续的顶点序列后一次性提交。
                Ok(BrushAction::Draw(vertices)) => {
                    self.vertices_buffer.clear();
                    self.vertices_buffer.extend(vertices.into_iter().flatten());
                    self.redraw(tr);
                    break;
                }
                // 没有新字形（例如只是变换/透明度/颜色变化）：复用上一轮的顶点缓冲直接重绘，
                // 省去重新排版与上传图集的开销。
                Ok(BrushAction::ReDraw) => {
                    self.redraw(tr);
                    break;
                }
            }
        }
    }

    /// 把字形顶点从像素空间投到 UI 坐标。
    /// z 固定为 0：UI 全在同一平面上，深度信息无意义；
    /// uv 与颜色原样透传，因为它们在像素空间与 UI 空间是一致的。
    fn transform(&self, vertex: &MyVertex, tr: Matrix) -> Vertex {
        let pos = tr.transform_point(&Point::new(vertex.pos.0, vertex.pos.1));
        Vertex::new(pos.x, pos.y, 0., vertex.uv.0, vertex.uv.1, vertex.color)
    }

    /// 把顶点缓冲逐字形（每 4 个顶点一个 quad）提交给 `quad_gl`。
    /// 纹理只在循环外绑定一次，因此整段文字只产生纹理切换一次的开销；
    /// 每个 quad 单独调用 `geometry` 是因为字形之间的顶点不共享，索引也固定，
    /// 交给 `quad_gl` 的批处理合并即可。
    fn redraw(&self, tr: Matrix) {
        // SAFETY: 只在 UI 渲染线程访问全局 GL 状态；本方法只读取 `self`（不可变借用），
        // 不与其他 GL 使用者并发。
        let gl = unsafe { get_internal_gl() }.quad_gl;
        gl.texture(Some(self.cache_texture));
        // 按 4 个一组切分，正好对应 submit 中产出的 (左上/右上/左下/右下) 顺序。
        for vertices in self.vertices_buffer.chunks_exact(4) {
            let vertices = [
                self.transform(&vertices[0], tr),
                self.transform(&vertices[1], tr),
                self.transform(&vertices[2], tr),
                self.transform(&vertices[3], tr),
            ];
            // 两个三角形拼成 quad：0-2-3 与 0-1-3（顶点顺序同构造时的上左/上右/下左/下右）。
            gl.geometry(&vertices, &[0, 2, 3, 0, 1, 3]);
        }
    }
}

// 析构时把图集纹理交给 `queue_texture_deletion` 排队删除，而不是直接 `Texture2D::delete()`：
// 绘制器可能在任何线程被 drop，而删除 GL 纹理只能在持有上下文的主线程进行
// （见 `ext::flush_pending_texture_deletions`）。
impl Drop for TextPainter {
    fn drop(&mut self) {
        crate::ext::queue_texture_deletion(self.cache_texture);
    }
}
