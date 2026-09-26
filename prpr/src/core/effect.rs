//! 全屏后处理滤镜（Effect）。
//!
//! 思路：把谱面当前的渲染结果当作输入纹理，用一个覆盖全屏的矩形跑一遍片元着色器，
//! 把结果写回离屏目标，从而叠加色差、模糊、故障等视觉效果。
//!
//! 两个关键设计：
//! 1. 内置滤镜是一张编译期静态表（名称 → GLSL 源码），谱面也可以自带自定义 shader；
//! 2. 着色器通过在源码注释里写 `// %默认值%` 自带默认参数元数据，加载时由 `DEF_REGEX`
//!    解析出来，这样谱面作者只写一个 shader 就能得到可用的默认值，不必逐个补齐 uniform。
use super::{Anim, Resource, Tweenable};
use crate::ext::{get_viewport, screen_aspect};
use anyhow::{anyhow, bail, Result};
use macroquad::prelude::*;
use miniquad::UniformType;
use once_cell::sync::Lazy;
use phf::phf_map;
use regex::Regex;
use std::{collections::HashSet, ops::Range};

// 内置滤镜注册表：预设名 → GLSL 片元着色器源码。
// 用 `include_str!` 在编译期把 shaders/*.glsl 内联进来，运行时零文件 IO，也便于打包进 wasm。
// 名称采用小驼峰，与谱面文件中引用的预设名一一对应。各预设的视觉用途：
// * chromatic  - 色差：红/蓝通道水平错位，营造头晕、故障感；
// * circleBlur - 圆形模糊：以某点为中心做环形方向模糊，常用于转场；
// * fisheye    - 鱼眼畸变：中心放大、边缘压缩；
// * glitch     - 故障抖动：随机水平错位 + 色偏，模拟信号损坏；
// * grayscale  - 灰度：去色，用于回忆/过去时叙事；
// * noise      - 噪点：叠加随机颗粒；
// * pixel      - 像素化：马赛克块化；
// * radialBlur - 放射模糊：从画面中心向外拖影，表现速度/冲击；
// * shockwave  - 冲击波：以点击位置为中心的环形扭曲，打击反馈常用；
// * vignette   - 暗角：压暗画面四角，聚焦中心。
static SHADERS: phf::Map<&'static str, &'static str> = phf_map! {
    "chromatic" => include_str!("shaders/chromatic.glsl"),
    "circleBlur" => include_str!("shaders/circle_blur.glsl"),
    "fisheye" => include_str!("shaders/fisheye.glsl"),
    "glitch" => include_str!("shaders/glitch.glsl"),
    "grayscale" => include_str!("shaders/grayscale.glsl"),
    "noise" => include_str!("shaders/noise.glsl"),
    "pixel" => include_str!("shaders/pixel.glsl"),
    "radialBlur" => include_str!("shaders/radial_blur.glsl"),
    "shockwave" => include_str!("shaders/shockwave.glsl"),
    "vignette" => include_str!("shaders/vignette.glsl"),
};

/// 能直接作为着色器 uniform 上传的标量/向量类型。
///
/// 通过关联常量 `UNIFORM_TYPE` 把 Rust 类型映射到 miniquad 的 `UniformType`，
/// 目的是在收集 `MaterialParams::uniforms` 时无需对每种具体类型写一遍 match，
/// 也让新增 uniform 类型只需实现这个 trait。
pub trait UniformValue: Clone + Default {
    /// 该类型对应的 GLSL uniform 类型（决定上传时占用几个 float 分量）。
    const UNIFORM_TYPE: UniformType;
}

// f32 → GLSL float（分量数 1）。
impl UniformValue for f32 {
    const UNIFORM_TYPE: UniformType = UniformType::Float1;
}

// Vec2 → GLSL vec2（分量数 2）。
impl UniformValue for Vec2 {
    const UNIFORM_TYPE: UniformType = UniformType::Float2;
}

// Color → GLSL vec4（分量数 4），内部按 r,g,b,a 顺序上传。
impl UniformValue for Color {
    const UNIFORM_TYPE: UniformType = UniformType::Float4;
}

/// 一个可绑定到材质的 uniform 的抽象：既可能是常量，也可能是随时间变化的动画。
///
/// 之所以把 `set_time` 与 `apply` 拆开，是为了让“推进动画”和“写值”发生在两个阶段：
/// `update()`（每帧一次，按谱面时间）推进动画状态，`render()`（可能多次或与绘制交错）只读取
/// 当前值写入材质。这样渲染阶段不必知道动画细节，也不会因为一帧内多次 apply 而重复推进时间。
pub trait Uniform {
    /// 返回 (uniform 名字, uniform 类型)，用于在编译材质前收集需要声明的 uniform 列表。
    fn uniform_pair(&self) -> (String, UniformType);
    /// 按给定时间推进内部动画（常量 uniform 为空实现）。
    fn set_time(&mut self, t: f64);
    /// 把当前值写进材质。
    fn apply(&self, material: &Material);
}

// “常量 uniform”的实现：`(名字, 值)` 形式。值一经构造就固定，因此 `set_time` 什么都不做，
// `apply` 直接写值；用户只给一个固定参数时走这条路径，省掉一层动画状态机。
impl<T: UniformValue> Uniform for (String, T) {
    fn uniform_pair(&self) -> (String, UniformType) {
        (self.0.clone(), T::UNIFORM_TYPE)
    }

    fn set_time(&mut self, _t: f64) {}

    fn apply(&self, material: &Material) {
        material.set_uniform(&self.0, self.1.clone());
    }
}

// “动画 uniform”的实现：`(名字, Anim<T>)` 形式。与常量版本用不同 impl 区分，是因为这里
// 需要额外维护时间轴：一帧内 `update` 推进动画、`render` 取 `now()`，两者职责清晰。
impl<T: UniformValue + Tweenable> Uniform for (String, Anim<T>) {
    fn uniform_pair(&self) -> (String, UniformType) {
        (self.0.clone(), T::UNIFORM_TYPE)
    }

    fn set_time(&mut self, t: f64) {
        self.1.set_time(t);
    }

    fn apply(&self, material: &Material) {
        material.set_uniform(&self.0, self.1.now());
    }
}

/// 一个全屏后处理滤镜实例。
///
/// 生命周期：`new()` 加载并解析着色器 → 每帧 `update()` 推进动画 uniform → `render()`
/// 执行一次全屏 pass。多个滤镜串联时共享 `Resource::chart_target` 这组乒乓缓冲，
/// 依次在 `swap()` 后的输出目标上叠加，前一环的结果成为后一环的输入纹理。
pub struct Effect {
    /// 该滤镜的生效时间区间（谱面时间，秒）；区间外既不更新也不渲染。
    time_range: Range<f64>,
    /// 最近一次 `update()` 传入的时间；`render()` 用它判断是否仍在有效区间内。
    t: f64,
    /// 编译好的 macroquad 材质（含片元着色器与 `screenTexture` 输入纹理绑定）。
    material: Material,
    /// 从 GLSL 源码 `// %...%` 注释解析出的默认值 uniform，渲染时先于用户 uniform 写入。
    defaults: Vec<Box<dyn Uniform>>,
    /// 谱面为这个滤镜额外指定的 uniform（常量或动画）。
    uniforms: Vec<Box<dyn Uniform>>,
    /// true 用屏幕宽高比全屏铺开，false 用谱面宽高比。
    /// 决定滤镜的作用范围是“整个可视区域”还是“谱面区域”，在非 16:9 屏幕上两者明显不同。
    pub global: bool,
}

// Effect 的构造与渲染：解析着色器默认值、补齐内建 uniform、执行全屏 pass。
impl Effect {
    /// 按名字查询内置滤镜的 GLSL 源码（谱面按预设名引用时使用）。
    pub fn get_preset(name: &str) -> Option<&'static str> {
        SHADERS.get(name).copied()
    }

    /// 创建一个滤镜实例。
    ///
    /// # Arguments
    /// * `time_range` - 生效时间区间（谱面时间，秒）。
    /// * `shader` - 片元着色器源码（预设或自定义）。
    /// * `uniforms` - 谱面额外传入的 uniform 列表。
    /// * `global` - 是否以屏幕宽高比全屏渲染。
    ///
    /// # Errors
    /// 默认值注释解析失败（类型未知或分量数不符）或着色器编译失败时返回错误。
    pub fn new(time_range: Range<f64>, shader: &str, uniforms: Vec<Box<dyn Uniform>>, global: bool) -> Result<Self> {
        // 从 GLSL 源码中的注释解析 uniform 默认值，约定语法形如：
        //     uniform float power;  // %0.01%
        //     uniform vec2  offset; // %0,0%
        //     uniform vec4  tint;   // %1,1,1,1%
        // 三个捕获组依次是 (类型, 名字, `%...%` 之间的值文本)。
        // 设计意图：让着色器自带默认参数元数据，谱面作者只写 shader 就能得到合理默认值，
        // 无需在谱面里逐个列 uniform；这也是 shader playground 类工具的常见约定。
        // 匹配规则：必须严格是 `类型 名字;` 后跟 `// %...%`，且值文本内不能含 `%`。
        // 失败行为：`captures_iter` 会静默跳过不匹配的行（不报错），但一旦匹配到却不认识类型、
        // 或 vec2/vec4 的分量数不对，则整个构造返回 Err，避免把错误的默认值传给着色器。
        // 注释里额外的范围提示（如 `0..1`）只是给人看的说明，不参与解析。
        static DEF_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"uniform\s+(\w+)\s+(\w+);\s+//\s+%([^%]+)%").unwrap());
        // 阶段 1：逐条解析默认值，按声明的类型构造对应的常量 uniform。
        let defaults = DEF_REGEX
            .captures_iter(shader)
            .map(|caps| -> Result<Box<dyn Uniform>> {
                let type_name = caps.get(1).unwrap().as_str();
                let name = caps.get(2).unwrap().as_str().to_owned();
                let value = caps.get(3).unwrap().as_str();
                Ok(match type_name {
                    "float" => Box::new((name, value.parse::<f32>()?)),
                    "vec2" => Box::new((name, {
                        let (x, y) = value.split_once(',').ok_or_else(|| anyhow!("Expected x,y"))?;
                        vec2(x.trim().parse()?, y.trim().parse()?)
                    })),
                    "vec4" => Box::new((name, {
                        let values: Vec<_> = value.split(',').map(|it| it.trim()).collect();
                        if values.len() != 4 {
                            bail!("Expected r,g,b,a");
                        }
                        Color::new(values[0].parse()?, values[1].parse()?, values[2].parse()?, values[3].parse()?)
                    })),
                    _ => bail!("Unknown type: {type_name}"),
                })
            })
            .collect::<Result<Vec<Box<dyn Uniform>>>>()?;
        // 阶段 2：汇总需要在材质里声明的 uniform 列表，用 HashSet 去重（第一次出现者胜）。
        // 必须去重：GLSL 里同一个 uniform 只声明一次，重复上报会让 miniquad 的 uniform 布局错位。
        let mut ocurred_uniforms = HashSet::new();
        let mut new_uniforms = Vec::new();
        let mut add_uniform = |(name, its_type): (String, UniformType)| {
            if ocurred_uniforms.insert(name.clone()) {
                new_uniforms.push((name, its_type));
            }
        };
        // 阶段 3：先放默认值，再补齐三个内建 uniform，最后放谱面自定义的 uniform。
        // 顺序即优先级：同名时先注册者保留，因此“谱面值”必须在“默认值”之后追加才不会被覆盖，
        // 而实际写值发生在 render 阶段（先 defaults 后 uniforms），两者一致。
        for def in &defaults {
            add_uniform(def.uniform_pair());
        }
        // 内建 uniform 无论用户是否传空 `Vec::new()` 都必须声明，否则着色器里引用它们会导致
        // 整个 uniform 块布局与 GLSL 不匹配、编译/链接失败。三者含义：
        // * time：谱面当前时间（秒），供需要随时间变化的滤镜（如 noise、glitch）使用；
        // * screenSize：输入纹理的像素尺寸，用于把像素坐标/屏幕坐标换算成 UV；
        // * UVScale：屏幕与谱面尺寸不一致时的 UV 校正系数，见 VERTEX_SHADER 说明。
        add_uniform(("time".to_owned(), UniformType::Float1));
        add_uniform(("screenSize".to_owned(), UniformType::Float2));
        add_uniform(("UVScale".to_owned(), UniformType::Float2));
        for u in &uniforms {
            add_uniform(u.uniform_pair());
        }
        Ok(Self {
            time_range,
            // 用负无穷作初始值，保证 `render` 在首帧 `update` 之前一定被判定为“不在区间内”。
            t: f64::NEG_INFINITY,
            defaults,
            material: load_material(
                VERTEX_SHADER,
                shader,
                MaterialParams {
                    uniforms: new_uniforms,
                    textures: vec!["screenTexture".to_owned()],
                    ..Default::default()
                },
            )?,
            uniforms,
            global,
        })
    }

    /// 按谱面时间推进动画 uniform。
    ///
    /// 只在生效区间内推进：区间外的滤镜时间应当冻结，否则时间跳到区间起点时动画会跳变。
    pub fn update(&mut self, res: &Resource) {
        let t = res.time;
        self.t = t;
        if self.time_range.contains(&t) {
            for uniform in &mut self.uniforms {
                uniform.set_time(t);
            }
        }
    }

    /// 执行一次全屏后处理 pass。
    ///
    /// 流程：`swap()` 让上一轮结果成为可采样的 `screenTexture` → 把渲染目标切到
    /// `target.output()` → 用一个覆盖全屏的矩形跑片元着色器，把结果写回离屏纹理。
    pub fn render(&self, res: &mut Resource) {
        if !self.time_range.contains(&self.t) {
            return;
        }
        // SAFETY: 渲染线程内取全局上下文单例；调用前上下文已初始化，且本调用不跨线程。
        let mut gl = unsafe { get_internal_gl() };
        // 先把 quad_gl 累积的绘制批次提交掉：下面要直接改写渲染目标，若不 flush，
        // 先前排队的绘制会落到错误的 pass 上。
        gl.flush();

        // 阶段 1：写 uniform。先默认值再用户值，保证用户显式指定的同名参数覆盖默认值。
        for def in &self.defaults {
            def.apply(&self.material);
        }
        for uniform in &self.uniforms {
            uniform.apply(&self.material);
        }
        self.material.set_uniform("time", self.t as f32);
        // 阶段 2：交换乒乓缓冲，把“上一轮解析出的结果”作为本滤镜的输入纹理。
        let target = res.chart_target.as_mut().unwrap();
        target.swap();
        let tex = target.old().texture;
        self.material.set_texture("screenTexture", tex);
        let screen_dim = vec2(tex.width(), tex.height());
        self.material.set_uniform("screenSize", screen_dim);
        // 阶段 3：把渲染目标切到本轮的输出纹理，后续绘制都会写到这里。
        gl.quad_gl.render_pass(Some(target.output().render_pass));

        // 阶段 4：UVScale 修正。屏幕（可视区）尺寸与输入纹理尺寸可能不同，
        // 直接用 0..1 的 uv 采样会取到错误区域，因此按两者比例缩放 uv。
        let vp = get_viewport();
        self.material.set_uniform("UVScale", vec2(vp.2 as _, vp.3 as _) / screen_dim);

        gl_use_material(self.material);
        // 画一个覆盖全屏的矩形：global=true 时按屏幕宽高比铺满整个可视区，
        // 否则按谱面宽高比铺，使滤镜只作用于谱面区域（宽屏下两侧留白不受影响）。
        let top = 1. / if self.global { screen_aspect() } else { res.aspect_ratio };
        draw_rectangle(-1., -top, 2., top * 2., WHITE);
        gl_use_default_material();
    }
}

// 释放材质持有的 GL 资源（着色器程序、纹理引用等）。
// 必须显式 delete：材质内部的 GL 对象不会随 Rust 值析构，谱面切换/特效重建时会持续泄漏。
impl Drop for Effect {
    /// 删除底层材质。
    fn drop(&mut self) {
        self.material.delete();
    }
}

/// 所有后处理滤镜共用的顶点着色器。
///
/// 只做两件事：把全屏矩形顶点变换到裁剪空间，以及对 uv 施加 `UVScale` 修正。
/// `UVScale` 的作用：输入纹理（上一帧输出）的尺寸与当前可视区/谱面矩形的宽高比可能不一致，
/// 若直接用 0..1 的原始 uv 采样，会取到纹理上错误的位置。这里以 (0.5, 0.5) 为中心按比例
/// 缩放 uv，把采样区域对齐到实际要显示的那一块。
/// 使用 GLSL 100（对应 GL ES 2.0 / WebGL 1），以保证桌面、移动端与 wasm 都能编译。
const VERTEX_SHADER: &str = r#"#version 100
attribute vec3 position;
attribute vec2 texcoord;
attribute vec4 color0;

varying vec2 uv;

uniform mat4 Model;
uniform mat4 Projection;
uniform vec2 UVScale;

void main() {
    gl_Position = Projection * Model * vec4(position, 1);
    uv = (texcoord - vec2(0.5)) * UVScale + vec2(0.5);
}"#;
