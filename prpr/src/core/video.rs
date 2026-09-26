//! YUV420P 视频背景。
//!
//! 流程：`prpr_avc` 解码出 YUV420P 帧 → 每帧把 Y/U/V 三个平面分别上传到三张单通道纹理 →
//! 片元着色器在 GPU 上完成 YUV→RGB 转换 → 画到一个全屏矩形上。
//!
//! 视频由上层在正确的坐标空间里调用 `render`（跟随绑定判定线的矩阵变换），
//! 因此本模块只负责“解码 + 上传 + 转换”，不关心谱面坐标系。
use super::Anim;
use crate::ext::{source_of_image, ScaleType};
use anyhow::Result;
use macroquad::prelude::*;
use miniquad::{Texture, TextureFormat, TextureParams, TextureWrap};
use prpr_avc::AVPixelFormat;
use serde::Deserialize;
use std::{cell::RefCell, io::Write};
use tempfile::NamedTempFile;

// 每个线程复用的三块解码平面缓冲（顺序固定为 Y/U/V）。
// 用 thread_local 是为了避免每帧为上传重新分配内存（视频每帧都要写入这三块缓冲）；
// 用 RefCell 是因为取帧发生在 `with_frame` 闭包里，需要在只持有 `&self` 的情况下可变借用，
// 若用普通字段会与 `self` 的其他借用在闭包中冲突。
thread_local! {
    static VIDEO_BUFFERS: RefCell<[Vec<u8>; 3]> = RefCell::default();
}

/// 谱面里“把视频挂到某条判定线”的配置项，对应谱面 JSON 的 `video` 节点。
#[derive(Deserialize)]
pub struct VideoAttach {
    /// 视频绑定的判定线下标：视频跟随该判定线的坐标变换一起移动/旋转。
    pub line: usize,
}

/// 一路视频背景：解码器 + 三张 YUV 平面纹理 + 渲染材质与动画参数。
pub struct Video {
    /// prpr_avc 解码器实例。
    video: prpr_avc::Video,
    /// 视频数据的临时文件。解码器按文件路径打开，所以谱面里的视频字节先落盘；
    /// 该字段与解码器同生命周期，保证文件在播放期间不被删除。
    pub video_file: NamedTempFile,

    /// YUV→RGB 的片元着色器材质，绑定了 tex_y/tex_u/tex_v 三个采样器。
    material: Material,
    /// Y 平面（亮度）纹理，全分辨率。人眼对亮度最敏感，所以它不下采样。
    tex_y: Texture2D,
    /// U 平面（蓝色差）纹理，宽高各为 Y 的一半（YUV420P 的色度下采样）。
    tex_u: Texture2D,
    /// V 平面（红色差）纹理，宽高各为 Y 的一半。
    tex_v: Texture2D,

    /// 视频在谱面时间轴上的起始时间（秒）。
    start_time: f64,
    /// 视频总时长（秒），用于时间窗判断。
    pub duration: f64,
    /// 上一次已上传帧的时间戳（pts）；-1 表示尚未拿到有效帧。
    last_pts: i64,
    /// 画面缩放策略，决定视频矩形内如何裁切/拉伸（Fit/Fill 等）。
    scale_type: ScaleType,
    /// 不透明度动画。
    alpha: Anim<f32>,
    /// 变暗程度动画（1 表示全黑）。
    dim: Anim<f32>,

    /// 单帧时长（秒），用于把谱面时间换算成帧序号，从而限制解码频率。
    frame_duration: f64,
    /// 上一次解码对应的帧序号；与当前序号相同就不重复 seek，避免每帧都触发解码。
    last_frame_idx: i64,
}

/// 创建一张单通道渲染纹理，作为 YUV 平面的载体。
///
/// 用 `TextureFormat::Alpha` 的原因：YUV420P 的三个平面各自都是单通道灰度数据，
/// 单通道纹理的显存与带宽只有 RGB 的三分之一，且 GPU 侧无需任何通道重排。
/// 过滤用线性：色度平面是半分辨率，线性采样相当于免费的双线性上采样，边缘更平滑；
/// 环绕用 Clamp：防止 uv 落在边缘外时把对侧像素拉进来形成条纹。
fn new_tex(w: u32, h: u32) -> Texture2D {
    Texture2D::from_miniquad_texture(Texture::new_render_texture(
        // SAFETY: 渲染线程内取全局上下文单例，上下文已初始化。
        unsafe { get_internal_gl() }.quad_context,
        TextureParams {
            width: w,
            height: h,
            format: TextureFormat::Alpha,
            filter: FilterMode::Linear,
            wrap: TextureWrap::Clamp,
        },
    ))
}

// 视频的加载、逐帧推进与渲染。
impl Video {
    /// 打开一路视频并创建三条平面纹理与渲染材质。
    ///
    /// # Arguments
    /// * `data` - 视频文件的完整字节。
    /// * `start_time` - 视频在谱面时间轴上的起始时间（秒）。
    /// * `scale_type` - 画面缩放策略。
    /// * `alpha` / `dim` - 不透明度与变暗动画。
    ///
    /// # Errors
    /// 临时文件写入失败、解码器无法打开视频、或着色器编译失败时返回错误。
    pub fn new(data: Vec<u8>, start_time: f64, scale_type: ScaleType, alpha: Anim<f32>, dim: Anim<f32>) -> Result<Self> {
        // 阶段 1：把视频字节写入临时文件。prpr_avc 按路径打开媒体，不能直接喂内存；
        // 写完立刻 drop 数据以提前释放这份可能很大的缓冲。
        let mut video_file = NamedTempFile::new()?;
        video_file.write_all(&data)?;
        drop(data);
        // 阶段 2：打开解码器，并强制要求输出 YUV420P。
        // 明确指定像素格式是为了让三平面布局稳定、避免运行时做色彩空间转换（转换放 GPU 做更省）。
        let video = prpr_avc::Video::open(video_file.path().as_os_str().to_str().unwrap(), AVPixelFormat::YUV420P)?;
        let duration = video.duration();
        let format = video.stream_format();
        let w = format.width as u32;
        let h = format.height as u32;

        // 阶段 3：建材质。uniform 留空：本 shader 不需要参数，只靠纹理采样。
        let material = load_material(
            shader::VERTEX,
            shader::FRAGMENT,
            MaterialParams {
                pipeline_params: PipelineParams::default(),
                uniforms: Vec::new(),
                textures: vec!["tex_y".to_owned(), "tex_u".to_owned(), "tex_v".to_owned()],
            },
        )?;
        // 阶段 4：三张单通道纹理。色度平面（U/V）取半分辨率，与 YUV420P 的内存布局一致，
        // 上传时无需做任何重采样。
        let tex_y = new_tex(w, h);
        let tex_u = new_tex(w / 2, h / 2);
        let tex_v = new_tex(w / 2, h / 2);
        material.set_texture("tex_y", tex_y);
        material.set_texture("tex_u", tex_u);
        material.set_texture("tex_v", tex_v);
        // 阶段 5：帧率换算成单帧时长。帧率信息缺失（num == 0）时退化为假定的 30fps，
        // 保证节流逻辑始终有一个正的除数，不会出现除零或永不刷新。
        let frame_rate = video.frame_rate();
        let frame_duration = if frame_rate.num > 0 { frame_rate.to_f64_inv() } else { 1.0 / 30.0 };

        Ok(Self {
            video,
            video_file,

            material,
            tex_y,
            tex_u,
            tex_v,

            start_time,
            duration,
            // -1 作为“尚无有效帧”的哨兵值，与解码器返回的无效 pts 约定一致。
            last_pts: -1,
            scale_type,
            alpha,
            dim,
            frame_duration,
            last_frame_idx: -1,
        })
    }

    /// 返回底层临时文件（供上层管理其生命周期，避免文件被提前清理）。
    pub fn video_file(&self) -> &NamedTempFile {
        &self.video_file
    }

    /// 按谱面时间推进视频：必要时定位到对应帧并把三个平面上传到纹理。
    ///
    /// # Errors
    /// 解码或 seek 失败时返回错误。
    pub fn update(&mut self, t: f64) -> Result<()> {
        // 阶段 1：时间窗检查。视频只在 [start_time, start_time + duration) 内有效，
        // 区间外直接跳过，既不推进动画也不解码（视频可能在谱面中途才开始/结束）。
        let elapsed = t - self.start_time;
        if !(0f64..self.duration).contains(&elapsed) {
            return Ok(());
        }
        self.alpha.set_time(t);
        self.dim.set_time(t);

        // Throttle to video frame rate
        // 阶段 2：按帧率节流。每次都 seek 会让解码器做无意义的重复工作，
        // 因此把谱面时间取整成帧序号，只有帧序号变化时才 seek 到对应时间戳。
        let frame_idx = (elapsed / self.frame_duration).round() as i64;
        if frame_idx != self.last_frame_idx {
            self.last_frame_idx = frame_idx;
            self.video.seek(self.video.elapsed_to_timestamp(elapsed));
        }

        // 阶段 3：取当前帧并上传三平面。
        // `with_frame` 把“解码器内部持有的最新帧”借给闭包，闭包内完成像素平面的拷贝与上传；
        // 用 pts 去重：若解码器返回的仍是同一帧，则跳过上传，省一次全屏纹理写入。
        self.video.with_frame(|frame, pts| {
            if self.last_pts == pts {
                return;
            }
            self.last_pts = pts;
            // pts == -1 表示解码器当前没有可用帧（例如还没解出关键帧），此时不更新纹理。
            if pts == -1 {
                return;
            }

            VIDEO_BUFFERS.with_borrow_mut(|buf| {
                // 平面 0 是 Y（全尺寸）；平面 1/2 是 U/V，用 get_data_half 取半分辨率数据，
                // 与纹理尺寸严格对应。
                frame.get_data(0, &mut buf[0]);
                frame.get_data_half(1, &mut buf[1]);
                frame.get_data_half(2, &mut buf[2]);

                // SAFETY: 渲染线程内取全局上下文单例，上下文已初始化。
                let ctx = unsafe { get_internal_gl() }.quad_context;
                // 直接用 miniquad 的纹理句柄上传，绕过 macroquad 的高层 API：
                // 一是高频（每帧）更新，二是需要逐平面分别写入，高层 API 无法表达这种粒度。
                self.tex_y.raw_miniquad_texture_handle().update(ctx, &buf[0]);
                self.tex_u.raw_miniquad_texture_handle().update(ctx, &buf[1]);
                self.tex_v.raw_miniquad_texture_handle().update(ctx, &buf[2]);
            });
        });

        Ok(())
    }

    /// 把当前视频帧画到全屏矩形上。
    ///
    /// 调用方需已设置好坐标变换（视频跟随判定线矩阵），本函数只负责顶点与 uv。
    ///
    /// # Arguments
    /// * `t` - 谱面时间（秒），用于判断是否在有效时段内。
    /// * `aspect_ratio` - 当前坐标系的宽高比，决定矩形高度。
    /// * `color` - 与 `alpha`/`dim` 动画相乘的基色。
    pub fn render(&self, t: f64, aspect_ratio: f32, color: Color) {
        // 与 update 相同的时间窗判断；此外要求已经至少上传过一帧，
        // 否则纹理内容未定义，画出来会是垃圾数据。
        if !(0f64..self.duration).contains(&(t - self.start_time)) {
            return;
        }
        if self.last_pts == -1 {
            return;
        }
        // 注意 gl_use_material / gl_use_default_material 必须成对：macroquad 的材质是全局状态，
        // 若不还原，后续所有 draw call 都会被套上这个 YUV 材质，整个画面都会变成视频。
        gl_use_material(self.material);
        let top = 1. / aspect_ratio;
        let r = Rect::new(-1., -top, 2., top * 2.);
        // 按 scale_type 计算实际采样区域；`source_of_image` 不适用时（例如 Fit 且尺寸信息缺失）
        // 兜底为整张纹理 (0,0,1,1)，保证始终有一个合法 uv 范围。
        let s = source_of_image(&self.tex_y, r, self.scale_type).unwrap_or_else(|| Rect::new(0., 0., 1., 1.));
        // dim 动画表示“变暗程度”，因此取反得到亮度系数；alpha 缺失时按 1（完全不透明）处理。
        let dim = 1. - self.dim.now();
        let color = Color::new(dim * color.r, dim * color.g, dim * color.b, self.alpha.now_opt().unwrap_or(1.) * color.a);
        let vertices = [
            Vertex::new(r.x, r.y, 0., s.x, s.y, color),
            Vertex::new(r.right(), r.y, 0., s.right(), s.y, color),
            Vertex::new(r.x, r.bottom(), 0., s.x, s.bottom(), color),
            Vertex::new(r.right(), r.bottom(), 0., s.right(), s.bottom(), color),
        ];
        // SAFETY: 渲染线程内取全局上下文单例，上下文已初始化。
        let gl = unsafe { get_internal_gl() }.quad_gl;
        gl.draw_mode(DrawMode::Triangles);
        // 两个三角形组成矩形（顶点序 0-2-3 / 0-1-3），uv 与位置一一对应。
        gl.geometry(&vertices, &[0, 2, 3, 0, 1, 3]);
        gl_use_default_material();
    }

    /// 复位到视频开头，用于重播（例如回到谱面开头或循环播放）。
    ///
    /// # Errors
    /// seek 失败时返回错误。
    pub fn reset(&mut self) -> Result<()> {
        // 同时清掉两个缓存哨兵：pts 让下一次取帧必定上传，帧序号让下一次 update 必定 seek。
        self.last_pts = -1;
        self.last_frame_idx = -1;
        self.video.seek(0);
        Ok(())
    }
}

/// 视频渲染用的内联 GLSL（均为 GLSL 100，兼容 GL ES 2.0 / WebGL 1，保证 wasm 可用）。
mod shader {
    /// 顶点着色器：标准 MVP 变换 + 顶点色归一化。
    /// 注意宏 quad 的顶点色分量是 0..255 的数值，必须除以 255 才能当 0..1 的颜色使用，
    /// 否则颜色会被整体放大到过曝。
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

    /// 片元着色器：YUV420P → RGB 转换。
    ///
    /// 三张平面纹理都是单通道 `Alpha` 格式，所以采样取 `.a` 分量。
    /// 转换采用 ITU-R BT.601 的“有限范围（limited range）”变体：
    /// * Y 先去偏置再放大：`y = 1.1643 * (y - 0.0625)`。16/255 ≈ 0.0625 是有限范围的黑电平，
    ///   1/(219/255) ≈ 1.1643 把 16..235 的 Y 拉伸回 0..1；若不这样做，黑会发灰、白会不够白；
    /// * U/V 各减去 0.5（128/255）去掉色度的无符号偏置；
    /// * 再左乘 3×3 的 BT.601 矩阵（R/V 系数 1.402、G/U/V 系数 −0.344/−0.714、
    ///   B/U 系数 1.772）得到 RGB。
    /// `clamp` 用一个四重 step 的乘积实现：uv 越界时结果为 0，把画面外的像素涂黑，
    /// 与 `TextureWrap::Clamp` 形成双保险，避免边缘出现拖尾条纹。
    pub const FRAGMENT: &str = r#"#version 100
precision lowp float;

varying lowp vec4 color;
varying lowp vec2 uv;

uniform sampler2D tex_y;
uniform sampler2D tex_u;
uniform sampler2D tex_v;

void main() {
    float clamp = step(uv.x, 1.0) * step(0.0, uv.x) * step(uv.y, 1.0) * step(0.0, uv.y);
    vec3 yuv = vec3(
        texture2D(tex_y, uv).a,
        texture2D(tex_u, uv).a - 0.5,
        texture2D(tex_v, uv).a - 0.5
    );
    yuv.x = 1.1643 * (yuv.x - 0.0625);
    mat3 color_matrix = mat3(
        vec3(1.0,   0.0,     1.402),
        vec3(1.0,  -0.344,  -0.714),
        vec3(1.0,   1.772,   0.0  )
    );

    gl_FragColor = vec4(yuv * color_matrix, 1.0) * color * clamp;
}"#;
}
