//! 多重采样（MSAA）离屏渲染目标，以及全屏后处理所依赖的公共渲染基础设施。
//!
//! 本模块绕过 macroquad 的高层封装，直接用 miniquad 的裸 GL 句柄自造多重采样 FBO。
//! 为什么必须自造而不用 `RenderTarget`：
//! 1. 官方 macroquad 的 `RenderTarget.render_pass` 字段是私有的，外部无法替换它；
//! 2. 官方 miniquad 没有 `RenderPass::from_raw`，无法用已经创建好的 GL 帧缓冲名字
//!    包装出一个可被 quad_gl 使用的 pass。
//!
//! 本项目使用的 fork（prpr-macroquad / prpr-miniquad）才开放了上述两项能力，因此本文件里
//! 对 `gl_internal_id`、`RenderPass::from_raw` 的调用都强依赖该 fork，不能在原版依赖上编译。
//!
//! 核心设计：一个 `MSRenderTarget` 同时持有“多重采样绘制目标”与两个普通纹理目标（乒乓缓冲），
//! 以支持后处理滤镜“把上一帧结果当输入、写入当前帧目标”的链式执行。
use macroquad::{
    texture::{RenderTarget, Texture2D},
    window::get_internal_gl,
};
use miniquad::{gl::GLuint, RenderPass, Texture, TextureFormat};

// TODO: doc
/// 支持 MSAA 的离屏渲染目标，附带一对乒乓（ping-pong）输出纹理。
///
/// 之所以要自己持有一整套 GL 对象，而不是复用 macroquad 的 `RenderTarget`：
/// 高层 API 只能表达“一张纹理目标”，既不能开启多重采样，也无法把“多重采样解析”与
/// “后处理链读写”拆成两个可分别使用的目标。
pub struct MSRenderTarget {
    /// 逻辑尺寸（宽, 高，单位像素）；MSAA 缓冲与输出纹理都用该尺寸创建。
    dim: (u32, u32),
    /// 多重采样帧缓冲对象（FBO）的名字，由 `glGenFramebuffers` 生成。
    /// 所有绘制先写到这里（内部是多采样、抗锯齿的），随后由 `copy_fbo` 解析成普通纹理。
    fbo: GLuint,
    /// 多重采样 renderbuffer 的名字，挂在 `fbo` 的颜色附件 0 上。
    /// 用 renderbuffer 而非纹理：多采样 renderbuffer 在 GL ES / 移动端支持最稳，
    /// 且它只负责“被解析”，不需要被采样，因此无需纹理身份。
    rbo: GLuint,
    /// 占位用的 `RenderTarget`：其 render_pass 通过 `RenderPass::from_raw` 包装了 `fbo`。
    /// 其中的 texture 是普通（非多采样）纹理，只为让 macroquad 认为这是个完整合法的目标；
    /// 外部通过 `input()` 取到它来获得“画进多采样 FBO”的能力。
    dummy: RenderTarget,
    /// 乒乓输出缓冲：`output[0]` 是当前可写/可采样的目标，`output[1]` 是上一次的结果。
    /// 必须有两个的原因：后处理链里每个滤镜都要把“上一帧的输出”作为输入纹理采样，
    /// 同时又把结果写进“当前目标”，两者不能是同一张纹理；`swap()` 负责交换读写两侧。
    output: [Option<RenderTarget>; 2],
}

/// 把多重采样 FBO 解析（resolve）成普通纹理所在的 FBO。
///
/// `glBlitFramebuffer` 的过滤参数必须是 `GL_NEAREST`：按 GL 规范，在多采样缓冲之间做
/// blit 解析时不允许指定线性过滤，多重采样的平均由驱动内部完成，这里只是把结果取出来。
///
/// # Returns
/// blit 后 `glGetError() == GL_NO_ERROR` 时返回 true（解析成功），否则 false，供调用方诊断。
///
/// # Safety
/// 调用者契约：`src`/`dst` 必须是当前 GL 上下文中已创建、颜色附件格式与尺寸兼容的 FBO。
/// 函数内部直接改写 GL 的读/绘帧缓冲绑定且不恢复，调用后当前绑定状态会变。
pub fn copy_fbo(src: GLuint, dst: GLuint, dim: (u32, u32)) -> bool {
    unsafe {
        use miniquad::gl::*;
        // SAFETY: 三个 GL 调用只作用于调用方传入的 FBO 名字与当前上下文，不涉及 Rust 内存；
        // src/dst 的有效性由调用者保证（见 # Safety）。
        glBindFramebuffer(GL_READ_FRAMEBUFFER, src);
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dst);
        let (w, h) = (dim.0 as i32, dim.1 as i32);
        glBlitFramebuffer(0, 0, w, h, 0, 0, w, h, GL_COLOR_BUFFER_BIT, GL_NEAREST);
        glGetError() == GL_NO_ERROR
    }
}

/// 取出 macroquad `RenderTarget` 底层的 GL 帧缓冲名字。
///
/// 不能把 FBO 名字缓存到结构体里自己维护：只有通过 `render_pass.gl_internal_id(ctx)`
/// 才能拿到 pass 内部真正的句柄（这是 fork 才公开的接口），而且 pass 可能被 macroquad
/// 内部重建，缓存下来的旧名字会失效。因此每次需要时都重新查询。
pub fn internal_id(target: RenderTarget) -> GLuint {
    // SAFETY: 只在已初始化 GL 上下文的主渲染线程调用，取到的是全局单例内部上下文。
    target.render_pass.gl_internal_id(unsafe { get_internal_gl() }.quad_context)
}

// 构造与访问：创建多重采样目标、解析结果、交换乒乓缓冲，以及提供五个语义不同的访问器。
impl MSRenderTarget {
    /// 创建一个指定尺寸与 MSAA 采样数的离屏渲染目标。
    ///
    /// # Arguments
    /// * `dim` - 输出尺寸（宽, 高，单位像素）。
    /// * `samples` - MSAA 采样数（常见 4；GL ES 只保证支持 4，更高采样数取决于驱动）。
    ///
    /// # Errors
    /// 不返回错误：若驱动不支持该采样数，`glRenderbufferStorageMultisample` 只会置位
    /// GL 错误而不中断，画面会退化为无抗锯齿（此处未做检查，属上游已知取舍）。
    pub fn new(dim: (u32, u32), samples: u32) -> Self {
        let mut fbo = 0;
        let mut rbo = 0;
        // 阶段 1：用裸 GL 创建多重采样 renderbuffer 与 FBO，并把 renderbuffer 挂为颜色附件 0。
        // 这里不能再走 macroquad，因为它的目标创建不支持多采样参数。
        unsafe {
            use miniquad::gl::*;
            // SAFETY: fbo/rbo 是本函数栈上的局部变量，作为 out-parameter 传给创建函数；
            // 全部 GL 调用都在已就绪的当前上下文中执行，且新建名字随后被本结构体接管所有权。
            glGenRenderbuffers(1, &mut rbo as *mut _);
            glBindRenderbuffer(GL_RENDERBUFFER, rbo);
            glRenderbufferStorageMultisample(GL_RENDERBUFFER, samples as _, GL_RGB8, dim.0 as _, dim.1 as _);
            glGenFramebuffers(1, &mut fbo as *mut _);
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rbo);
        }
        // SAFETY: 在渲染线程上取全局上下文单例，此时 GL 上下文已由上游初始化。
        let gl = unsafe { get_internal_gl() };
        // 阶段 2：创建一张普通 RGB8 渲染纹理及其 RenderPass，作为解析目标与乒乓输出之一。
        // 普通纹理才能被后处理采样；多采样 renderbuffer 只能被 blit，不能被采样。
        let texture = Texture::new_render_texture(
            gl.quad_context,
            miniquad::TextureParams {
                width: dim.0,
                height: dim.1,
                format: TextureFormat::RGB8,
                ..Default::default()
            },
        );
        let render_pass = RenderPass::new(gl.quad_context, texture, None);
        // 阶段 3：用 fork 提供的 `from_raw` 把裸 FBO 名字包装成 pass，形成“可绘制多采样目标”。
        // 这是原版 miniquad 缺失的能力，也是本结构成立的前提。
        let dummy_render_pass = RenderPass::from_raw(gl.quad_context, fbo, texture);
        Self {
            dim,
            fbo,
            rbo,
            dummy: RenderTarget {
                texture: Texture2D::from_miniquad_texture(texture),
                render_pass: dummy_render_pass,
            },
            // 阶段 4：初始化乒乓缓冲——先只有 output[0]，output[1] 留待第一次 swap 时创建，
            // 避免在没有后处理链时白白多占一张全屏纹理。
            output: [
                Some(RenderTarget {
                    texture: Texture2D::from_miniquad_texture(texture),
                    render_pass,
                }),
                None,
            ],
        }
    }

    /// 把多重采样 FBO 解析进当前的 `output[0]`，使其内容变为可采样的普通纹理。
    ///
    /// 应在“一帧的所有绘制已提交进多采样目标”之后调用，否则会拿到不完整的结果。
    pub fn blit(&self) {
        copy_fbo(self.fbo, internal_id(self.output[0].unwrap()), self.dim);
    }

    /// 交换乒乓读写目标，为下一轮后处理准备“可写目标 + 可采样旧值”。
    ///
    /// 调用后：`old()` 是上一轮已解析完成的结果（用作输入纹理），`output()` 是本轮写入目标。
    pub fn swap(&mut self) {
        // 阶段 1：交换两个槽位，让上一轮的输出退到 output[1] 充当输入。
        self.output.swap(0, 1);
        // 阶段 2：第一次交换时 output[0]（即旧的 output[1]）为 None，需要新建一张纹理目标，
        // 并把刚解析出来的旧结果复制过去，作为后处理链的初始输入；否则新目标里是未定义内容。
        if self.output[0].is_none() {
            // SAFETY: 渲染线程内取全局上下文单例，上下文已初始化。
            let gl = unsafe { get_internal_gl() };
            let texture = miniquad::Texture::new_render_texture(
                gl.quad_context,
                miniquad::TextureParams {
                    width: self.dim.0,
                    height: self.dim.1,
                    format: TextureFormat::RGB8,
                    ..Default::default()
                },
            );
            let render_pass = RenderPass::new(gl.quad_context, texture, None);
            self.output[0] = Some(RenderTarget {
                texture: Texture2D::from_miniquad_texture(texture),
                render_pass,
            });
            copy_fbo(internal_id(self.output[1].unwrap()), internal_id(self.output[0].unwrap()), self.dim);
        }
    }

    /// 返回“绘制入口”目标：外部把画面画进它，就等于画进了多重采样 FBO。
    ///
    /// 注意它内部的纹理是普通纹理，直接采样它拿不到 MSAA 结果，只有“画进去”才有意义；
    /// 要采样请用 `output()`（当帧结果）或 `old()`（上一帧结果）。
    pub fn input(&self) -> RenderTarget {
        self.dummy
    }

    /// 返回当前可写、且可在下一轮被采样的输出目标（`output[0]`）。
    ///
    /// 后处理渲染时把 pass 切到它，着色器写出的像素会落在对应的普通纹理上。
    pub fn output(&self) -> RenderTarget {
        self.output[0].unwrap()
    }

    /// 返回上一次已经解析完成的结果（`output[1]`）。
    ///
    /// 典型用法：`swap()` 之后，把 `old().texture` 作为后处理滤镜的 `screenTexture` 输入。
    pub fn old(&self) -> RenderTarget {
        self.output[1].unwrap()
    }
}

// 释放本结构体持有的全部 GL 资源（FBO、renderbuffer、乒乓输出纹理）。
// 必须显式释放的原因：GL 对象只有整数名字，不随 Rust 值的生命周期自动析构；若不在这里
// glDelete*，每当渲染目标随窗口尺寸变化被重建时都会持续泄漏显存。
impl Drop for MSRenderTarget {
    /// 删除原生 FBO/renderbuffer，并逐个删除乒乓缓冲里的 `RenderTarget`。
    fn drop(&mut self) {
        unsafe {
            use miniquad::gl::*;
            // SAFETY: rbo 与 fbo 均由 `Self::new` 通过 glGen* 在当前上下文创建，
            // 本结构体是它们的唯一所有者，drop 之后不会再被任何代码引用。
            glDeleteRenderbuffers(1, &self.rbo as *const _);
            glDeleteFramebuffers(1, &self.fbo as *const _);
        }
        // 输出纹理由 macroquad 的 RenderTarget 持有，走它自己的 delete 释放纹理与 pass。
        for target in self.output.iter().flatten() {
            target.delete();
        }
    }
}
