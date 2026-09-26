//! This is from https://raw.githubusercontent.com/not-fl3/macroquad/master/particles/src/lib.rs
//! We can't use macroquad-particles directly, since it implicitly depends on quad-snd (which is
//! obviously a mistake) and that conflicts with kira.

//! Edits:
//! 1. nanoserde related parts are removed for simplicity's sake.
//! 2. apply_viewport
//! 3. clippy
//! 4. time can be customized by input argument
//! 5. Remove EmittersCache

//! 中文说明（对应上面的英文原注释，保留原文不动）：
//! 本文件 fork 自 macroquad 的 `particles` 模块
//! （来源：<https://raw.githubusercontent.com/not-fl3/macroquad/master/particles/src/lib.rs>）。
//! 之所以不复用上游的 `macroquad-particles` 包：它隐式依赖 quad-snd（显然是打包失误），
//! 会与本项目使用的 kira 音频库产生冲突。
//!
//! 相对上游的改动及各自原因：
//! 1. 移除所有 nanoserde 相关的序列化代码——本项目不需要它，去掉可减少依赖与编译时间；
//! 2. 去掉 `apply_viewport` 的封装，视口改由调用方直接设置，便于与 quad_gl 的渲染目标协同；
//! 3. 修正若干 clippy 警告；
//! 4. 时间改为由参数传入（`dt`），不再读取全局时间，从而与谱面时间轴/暂停逻辑对齐；
//! 5. 移除 `EmittersCache`，发射器由上层自行管理生命周期，避免全局缓存造成状态泄漏。
//!
//! 渲染路径：粒子走一条自建的 miniquad 管线（每帧上传实例缓冲、一次实例化绘制提交全部粒子），
//! 完全绕开 quad_gl 的高层封装。因此 `draw()` 里必须先 `gl.flush()` 排空 quad_gl 的批，
//! 否则 quad_gl 缓存的绘制会和新建的 pass 互相破坏 GL 状态。
use macroquad::prelude::*;
use macroquad::window::miniquad::*;

/// 粒子曲线在控制点之间的插值方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpolation {
    /// 线性插值（当前唯一可用）。
    Linear,
    /// 贝塞尔插值（尚未实现，见 `Curve::batch` 中的 `unimplemented!()`）。
    Bezier,
}

/// 一条一维粒子曲线，用来描述某个标量随生命周期（0..1）的变化，如尺寸、透明度。
///
/// `points` 是 (时间, 值) 的控制点序列；使用时先由 `batch()` 采样成等距数组，
/// 之后每帧查表（O(1)）而不是逐个控制点搜索，因为粒子数可达上万，采样开销必须摊薄到零。
#[derive(Debug, Clone)]
pub struct Curve {
    /// Key points for building a curve
    /// 构建曲线的关键点：x 为归一化时间（0..1），y 为对应的值。
    pub points: Vec<(f32, f32)>,
    /// The way middle points is interpolated during building a curve
    /// Only Linear is implemented now
    /// 控制点之间的插值方式；按原注释，目前只实现了 `Linear`。
    pub interpolation: Interpolation,
    /// Interpolation steps used to build the curve from the key points
    /// 从控制点构建曲线时使用的采样步数（越大曲线越平滑，内存占用也越大）。
    pub resolution: usize,
}

// 把控制点展开成等距采样数组，供 `BatchedCurve::get` 做常数时间查表。
impl Curve {
    /// 按 `resolution` 把关键点线性展开为等距采样点。
    ///
    /// 实现思路：以固定步长 `1/resolution` 递增时间轴 x，对每段相邻控制点做线性插值。
    /// 由于步长固定，得到的 `points` 天然是等距的，因此取值时可以“用下标表示时间”。
    ///
    /// # Panics
    /// `interpolation == Bezier` 时触发 `unimplemented!()`——贝塞尔插值尚未实现。
    fn batch(&self) -> BatchedCurve {
        if self.interpolation == Interpolation::Bezier {
            unimplemented!()
        }

        let step_f32 = 1.0 / self.resolution as f32;
        let mut x = 0.0;
        // 预分配正好 resolution 个元素，避免构建过程中的多次扩容（曲线在特效重载时常被重建）。
        let mut points = Vec::with_capacity(self.resolution);

        for curve_part in self.points.windows(2) {
            let start = curve_part[0];
            let end = curve_part[1];

            while x <= end.0 {
                let t = (x - start.0) / (end.0 - start.0);
                let point = start.1 + (end.1 - start.1) * t;
                points.push(point);
                x += step_f32;
            }
        }

        BatchedCurve { points }
    }
}

/// 采样后的等距曲线：值按时间均匀排列，可用下标直接定位。
#[derive(Debug, Clone)]
pub struct BatchedCurve {
    /// 等距采样得到的值序列（长度约为 `resolution`）。
    pub points: Vec<f32>,
}

// 按归一化时间取曲线值（每帧对每个粒子调用，必须廉价）。
impl BatchedCurve {
    /// 在归一化时间 `t`（0..1）处取值。
    /// 用“下标 = t * 长度”定位区间，再在相邻两个采样点间线性插值；
    /// 下标用 `min(len-1)` 夹紧，保证 t==1（或浮点误差导致略微越界）时不 panic。
    fn get(&self, t: f32) -> f32 {
        let t_scaled = t * self.points.len() as f32;
        let previous_ix = (t_scaled as usize).min(self.points.len() - 1);
        let next_ix = (previous_ix + 1).min(self.points.len() - 1);
        let previous = self.points[previous_ix];
        let next = self.points[next_ix];

        previous + (next - previous) * (t_scaled - previous_ix as f32)
    }
}
// 默认曲线：无控制点、线性插值、resolution = 20。
// 20 的来源与取舍：一条曲线最多 20 个 f32，内存开销可忽略；
// 采样点太少会看出明显折线感，太多则每帧对每个粒子的查表访存不划算，20 是经验折中值。
impl Default for Curve {
    fn default() -> Curve {
        Curve {
            points: vec![],
            interpolation: Interpolation::Linear,
            resolution: 20,
        }
    }
}

/// 粒子的初始生成区域。
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum EmissionShape {
    /// 所有粒子都在发射器原点生成（点发射，如单次点击的火花）。
    Point,
    /// 在以原点为中心、宽 `width` 高 `height` 的矩形内均匀生成。
    Rect { width: f32, height: f32 },
    /// 在以原点为圆心、半径 `radius` 的圆盘内生成。
    Sphere { radius: f32 },
}

/// 颜色随粒子生命周期变化的曲线。
///
/// 只给三个控制点（起/中/末）就能表达“起 → 中 → 末”的三段渐变，
/// 足以覆盖绝大多数打击特效（例如 白 → 黄 → 透明），同时保持谱面书写与插值都足够简单。
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct ColorCurve {
    /// 生命周期起点（t=0）的颜色。
    pub start: Color,
    /// 生命周期中点（t=0.5）的颜色。
    pub mid: Color,
    /// 生命周期终点（t=1）的颜色。
    pub end: Color,
}

// 默认色曲线全白：未配置颜色渐变时，粒子不会被曲线额外染色。
impl Default for ColorCurve {
    fn default() -> ColorCurve {
        ColorCurve {
            start: WHITE,
            mid: WHITE,
            end: WHITE,
        }
    }
}

/// 粒子发射器的全部可配置参数。
///
/// 大量字段成对出现“基准值 + 随机比例”，实际取值为 `base - base * rand(0, randomness)`，
/// 即随机范围落在 `base * (1 - randomness) ..= base`。randomness = 0 时结果完全确定，
/// 便于谱面作者调试；调大可让整批粒子错开，避免看起来像“机械复制”。
#[derive(Debug, Clone)]
pub struct EmitterConfig {
    /// If false - particles spawns at position supplied to .draw(), but afterwards lives in current camera coordinate system.
    /// If false particles use coordinate system originated to the emitter draw position
    /// 粒子是否在发射器的本地坐标系中运动：本地坐标下发射器后续移动/旋转不会带走已有粒子；
    /// 世界坐标下粒子生成后固定在场景中。影响 `emit_particle` 是否叠加 `self.position`，
    /// 也影响着色器里的变换分支（`local_coords` uniform）。
    /// 注：原英文注释两句都以 "If false" 开头，语义自相矛盾，此处按代码实际行为补充说明，原文保持不动。
    pub local_coords: bool,
    /// Particles will be emitted inside that region.
    /// 粒子的初始生成区域（点 / 矩形 / 圆盘）。
    pub emission_shape: EmissionShape,
    /// If true only one emission cycle occurs. May be re-emitted by .emit() call.
    /// 只发射一个生命周期批次；该批次结束后自动把 `emitting` 置为 false，
    /// 需要再次触发只能显式调用 `emit()`。适合“命中即一次爆发”的特效。
    pub one_shot: bool,
    /// Lifespan of individual particle.
    /// 单个粒子的存活时长（秒）。
    pub lifetime: f32,
    /// Particle lifetime randomness ratio.
    /// Each particle will spawned with "lifetime = lifetime - lifetime * rand::gen_range(0.0, lifetime_randomness)".
    /// 生命周期随机比例：让粒子错开死亡时刻，避免整批粒子同一帧一起消失。
    pub lifetime_randomness: f32,
    /// 0..1 value, how rapidly particles in emission cycle are emitted.
    /// With 0 particles will be emitted with equal gap.
    /// With 1 all the particles will be emitted at the beginning of the cycle.
    /// 发射节奏（0..1）：0 表示整批粒子在一个生命周期内均匀发射；1 表示全部在周期开头一次喷出。
    /// 具体体现在发射间隔 `gap = lifetime / amount * (1 - explosiveness)`。
    pub explosiveness: f32,
    /// Amount of particles emitted in one emission cycle.
    /// 每个发射周期发射的粒子数量；同时也被 `update` 用作同时存活数的上限。
    pub amount: u32,
    /// Shape of each individual particle mesh.
    /// 单个粒子的几何形状（矩形 / 圆 / 自定义网格）。
    pub shape: ParticleShape,
    /// Particles are emitting when "emitting" is true.
    /// If its a "one-shot" emitter, emitting will switch to false after active emission cycle.
    /// 是否正在发射；one_shot 发射器在周期结束后会被自动置为 false。
    pub emitting: bool,
    /// Unit vector specifying emission direction.
    /// 初始速度的方向单位向量（默认 (0,-1)，屏幕坐标下即向上喷发）。
    pub initial_direction: Vec2,
    /// Angle from 0 to "2 * Pi" for random fluctuation for direction vector.
    /// 方向随机张角（0..2π），在 `initial_direction` 两侧各张开一半。
    pub initial_direction_spread: f32,
    /// Initial speed for each emitted particle.
    /// Direction of the initial speed vector is affected by "direction" and "spread"
    /// 初始速度大小（世界单位/秒）。默认 50.0 与默认 lifetime=1.0 搭配，
    /// 使粒子大约移动半个屏幕量级，既不会“瞬移”也不会原地不动。
    pub initial_velocity: f32,
    /// Initial velocity randomness ratio.
    /// Each particle will spawned with "initial_velocity = initial_velocity - initial_velocity * rand::gen_range(0.0, initial_velocity_randomness)".
    /// 初始速度的随机比例。
    pub initial_velocity_randomness: f32,
    /// Velocity acceleration applied to each particle in the direction of motion.
    /// 沿速度方向的加速度（按比例放大速度），用于模拟喷气/加速消散。
    pub linear_accel: f32,

    // Initial rotation for each emitted particle.
    /// 每个粒子的初始旋转角（弧度）。旋转量被打包进 `GpuParticle.pos.z` 交给着色器，
    /// 因此 CPU 侧不需要保存旋转角本身，只需保存角速度。
    pub initial_rotation: f32,
    /// Initial rotation randomness.
    /// Each particle will spawned with "initial_rotation = initial_rotation - initial_rotation * rand::gen_range(0.0, initial_rotation_randomness)".
    /// 初始旋转角的随机比例。
    pub initial_rotation_randomness: f32,
    // Initial rotational speed
    /// 初始角速度（弧度/秒）。
    pub initial_angular_velocity: f32,
    /// Initial angular velocity randomness.
    /// Each particle will spawned with "initial_angular_velocity = initial_angular_velocity - initial_angular_velocity * rand::gen_range(0.0, initial_angular_velocity_randomness)".
    /// 初始角速度的随机比例。
    pub initial_angular_velocity_randomness: f32,
    /// Angular velocity acceleration applied to each particle .
    /// 角速度的加速度（按比例放大角速度）。
    pub angular_accel: f32,
    /// Angluar velocity damping
    /// Each frame angular velocity will be transformed "angular_velocity *= (1.0 - angular_damping)".
    /// 角速度阻尼：每帧 `angular_velocity *= (1 - angular_damping)`。
    /// 注意这里没有乘 dt，因此阻尼强度与帧率相关（上游遗留行为，未做修改）。
    pub angular_damping: f32,
    /// Each particle is a "size x size" square.
    /// 粒子的边长（世界单位）；粒子是 size×size 的方片。
    pub size: f32,
    /// Each particle will spawned with "size = size - size * rand::gen_range(0.0, size_randomness)".
    /// 尺寸的随机比例；让同一批粒子有大小差异，层次感更强。
    pub size_randomness: f32,
    /// If curve is present in each moment of particle lifetime size would be multiplied by the value from the curve
    /// 尺寸随生命周期变化的曲线；存在时每帧把尺寸乘以曲线在 `lived/lifetime` 处的采样值。
    pub size_curve: Option<Curve>,

    /// Particles rendering mode.
    /// 混合模式：打击特效通常用 Additive 以获得叠加发光感。
    pub blend_mode: BlendMode,

    /// 粒子的基础颜色；与 `colors_curve` 的采样值相乘得到最终颜色。
    pub base_color: Color,
    /// How particles should change base color along the lifetime.
    /// 颜色随生命周期变化的曲线（起/中/末三段）。
    pub colors_curve: ColorCurve,

    /// Gravity applied to each individual particle.
    /// 施加在每个粒子上的重力加速度向量。
    pub gravity: Vec2,

    /// Particle texture. If none particles going to be white squares.
    /// 粒子纹理；为 None 时绑定 1×1 白色纹理，粒子显示为纯色方片。
    pub texture: Option<Texture2D>,

    /// For animated texture specify spritesheet layout.
    /// If none the whole texture will be used.
    /// 精灵表（spritesheet）布局，用于逐帧动画；为 None 时使用整张纹理。
    pub atlas: Option<AtlasConfig>,

    /// Custom material used to shade each particle.
    /// 自定义粒子材质；为 None 时使用内置的 `shader::VERTEX` / `shader::FRAGMENT`。
    pub material: Option<ParticleMaterial>,

    /// If none particles will be rendered directly to the screen.
    /// If not none all the particles will be rendered to a rectangle and than this rectangle
    /// will be rendered to the screen.
    /// This will allows some effects affecting particles as a whole.
    /// NOTE: this is not really implemented and now Some will just make hardcoded downscaling
    /// 后处理开关：为 None 时粒子直接画进当前渲染目标；为 Some 时先画到一个固定
    /// 320×200 的离屏纹理，再整屏贴回，从而能对整片粒子统一施加效果。
    /// 按原注释，这里并非真正可配置的后处理，Some 目前只会触发硬编码的降采样。
    pub post_processing: Option<PostProcessing>,
}

// 在生成区域内随机取点。
impl EmissionShape {
    /// 按形状生成一个相对发射器原点的随机点。
    ///
    /// 圆盘用 `sqrt(rand(0, r²))` 反解半径，是为了保证在**面积**上均匀分布：
    /// 若直接对半径线性取样，取到的是“半径上均匀”，面积越小的内圈会越密（粒子向圆心聚集）。
    fn gen_random_point(&self) -> Vec2 {
        match self {
            EmissionShape::Point => vec2(0., 0.),
            EmissionShape::Rect { width, height } => vec2(rand::gen_range(-width / 2., width / 2.0), rand::gen_range(-height / 2., height / 2.0)),
            EmissionShape::Sphere { radius } => {
                let ro = rand::gen_range(0., radius * radius).sqrt();
                let phi = rand::gen_range(0., std::f32::consts::PI * 2.);

                macroquad::math::polar_to_cartesian(ro, phi)
            }
        }
    }
}

/// 后处理参数的占位类型：当前无字段，仅用于标记“启用后处理”这一开关状态。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PostProcessing;

/// 单个粒子的几何形状。
#[derive(Clone, Debug, PartialEq)]
pub enum ParticleShape {
    /// 宽高比为 `aspect_ratio` 的矩形（宽 = aspect_ratio，高 = 1，即按比例拉伸方片）。
    Rectangle { aspect_ratio: f32 },
    /// 用 `subdivisions` 个扇形三角形逼近的圆；分段越多越圆滑。
    Circle { subdivisions: u32 },
    /// 自定义网格：交错排布的顶点数据 + 索引数据，布局须与着色器属性一致。
    CustomMesh { vertices: Vec<f32>, indices: Vec<u16> },
}

// 把形状转换成 miniquad 的绘制绑定（几何缓冲 + 索引缓冲 + 实例缓冲 + 图像槽）。
impl ParticleShape {
    /// 构建绘制绑定。
    ///
    /// 顶点布局固定为 [pos(3) + uv(2) + color(4)] 的交错格式，必须与 `shader::meta()` 中
    /// 声明的属性顺序、类型完全一致，否则 GPU 会把数据按错误偏移解释。
    /// `positions_vertex_buffer` 是调用方传进来的实例缓冲（第二个顶点缓冲槽），
    /// 它存放逐粒子的数据，与几何顶点缓冲以 PerInstance 步进配对。
    ///
    /// 纹理为 None 时退化为 1×1 白色纹理：这样片元着色器永远有纹理可采样，
    /// 无需为“无纹理”写 shader 分支，颜色完全由逐粒子顶点色决定。
    fn build_bindings(&self, ctx: &mut miniquad::Context, positions_vertex_buffer: Buffer, texture: Option<Texture2D>) -> Bindings {
        // 按形状生成几何顶点/索引缓冲；三种形状共用同一套顶点布局。
        let (geometry_vertex_buffer, index_buffer) = match self {
            ParticleShape::Rectangle { aspect_ratio } => {
                #[rustfmt::skip]
                let vertices: &[f32] = &[
                    // positions          uv          colors
                    -1.0 * aspect_ratio, -1.0, 0.0,   0.0, 0.0,  1.0, 1.0, 1.0, 1.0,
                     1.0 * aspect_ratio, -1.0, 0.0,   1.0, 0.0,  1.0, 1.0, 1.0, 1.0,
                     1.0 * aspect_ratio,  1.0, 0.0,   1.0, 1.0,  1.0, 1.0, 1.0, 1.0,
                    -1.0 * aspect_ratio,  1.0, 0.0,   0.0, 1.0,  1.0, 1.0, 1.0, 1.0,
                ];

                let vertex_buffer = Buffer::immutable(ctx, BufferType::VertexBuffer, vertices);

                #[rustfmt::skip]
                let indices: &[u16] = &[
                    0, 1, 2, 0, 2, 3
                ];
                let index_buffer = Buffer::immutable(ctx, BufferType::IndexBuffer, indices);

                (vertex_buffer, index_buffer)
            }
            ParticleShape::Circle { subdivisions } => {
                let mut vertices = Vec::<f32>::new();
                let mut indices = Vec::<u16>::new();

                let rot = 0.0;
                vertices.extend_from_slice(&[0., 0., 0., 0., 0., 1.0, 1.0, 1.0, 1.0]);
                for i in 0..subdivisions + 1 {
                    let rx = (i as f32 / *subdivisions as f32 * std::f32::consts::PI * 2. + rot).cos();
                    let ry = (i as f32 / *subdivisions as f32 * std::f32::consts::PI * 2. + rot).sin();
                    vertices.extend_from_slice(&[rx, ry, 0., rx, ry, 1., 1., 1., 1.]);

                    if i != *subdivisions {
                        indices.extend_from_slice(&[0, i as u16 + 1, i as u16 + 2]);
                    }
                }

                let vertex_buffer = Buffer::immutable(ctx, BufferType::VertexBuffer, &vertices);
                let index_buffer = Buffer::immutable(ctx, BufferType::IndexBuffer, &indices);
                (vertex_buffer, index_buffer)
            }
            ParticleShape::CustomMesh { vertices, indices } => {
                let vertex_buffer = Buffer::immutable(ctx, BufferType::VertexBuffer, vertices);
                let index_buffer = Buffer::immutable(ctx, BufferType::IndexBuffer, indices);
                (vertex_buffer, index_buffer)
            }
        };

        // 组装绑定：槽 0 = 几何顶点缓冲，槽 1 = 实例缓冲（与 Pipeline 的 BufferLayout 顺序对应）。
        Bindings {
            vertex_buffers: vec![geometry_vertex_buffer, positions_vertex_buffer],
            index_buffer,
            images: vec![
                texture.map_or_else(|| Texture::from_rgba8(ctx, 1, 1, &[255, 255, 255, 255]), |texture| texture.raw_miniquad_texture_handle())
            ],
        }
    }
}

/// 自定义粒子材质：一对顶点/片元着色器源码。
///
/// 只保存源码字符串而不保存编译结果，是因为 `Emitter` 需要在拿到 GL 上下文后
/// 用 `preprocess_shader`（注入 `particles.glsl`）再编译，源码要留到那一步。
#[derive(Debug, Clone)]
pub struct ParticleMaterial {
    /// 顶点着色器源码。
    vertex: String,
    /// 片元着色器源码。
    fragment: String,
}

// 构造自定义粒子材质。
impl ParticleMaterial {
    /// 用给定的着色器源码创建材质描述（此时尚未编译）。
    pub fn new(vertex: &str, fragment: &str) -> ParticleMaterial {
        ParticleMaterial {
            vertex: vertex.to_owned(),
            fragment: fragment.to_owned(),
        }
    }
}

/// 粒子混合模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    /// Colors of overlapped particles will be blended by alpha channel.
    /// 常规 alpha 混合：重叠粒子按各自的 alpha 做“over”合成，越叠越不透明，亮度不会溢出。
    Alpha,
    /// Colors of overlapped particles will be added to each other.
    /// 叠加混合：重叠粒子的颜色直接相加，越叠越亮，用于打击特效的发光/过曝感。
    Additive,
}

// 把 BlendMode 映射为 miniquad 的混合状态。
impl BlendMode {
    /// 返回对应的混合因子。
    /// Alpha：`(srcAlpha, 1 - srcAlpha)`，标准的源覆盖目标混合；
    /// Additive：`(srcAlpha, One)`，保留目标已有颜色并叠加新颜色，形成发光堆积。
    fn blend_state(&self) -> BlendState {
        match self {
            BlendMode::Alpha => {
                BlendState::new(Equation::Add, BlendFactor::Value(BlendValue::SourceAlpha), BlendFactor::OneMinusValue(BlendValue::SourceAlpha))
            }
            BlendMode::Additive => BlendState::new(Equation::Add, BlendFactor::Value(BlendValue::SourceAlpha), BlendFactor::One),
        }
    }
}

/// 精灵表配置：n×m 网格，以及本粒子动画使用其中哪一段连续帧。
#[derive(Debug, Clone)]
pub struct AtlasConfig {
    /// 横向分格数。
    n: u16,
    /// 纵向分格数。
    m: u16,
    /// 起始帧下标（含），0 表示第一格。
    start_index: u16,
    /// 结束帧下标（含）。
    end_index: u16,
}

// 由范围语法构造精灵表配置，并把范围两端的开闭语义归一化成两个闭区间下标。
impl AtlasConfig {
    /// 用 n×m 网格与一个范围创建配置（`range` 支持 `..`、`a..`、`..=b` 等 `RangeBounds` 写法）。
    ///
    /// 构造阶段就把开闭语义归一化为 `start_index`/`end_index` 两个闭下标，
    /// 这样 `update` 里推进帧号时只需一次线性映射，不必反复判断范围边界。
    pub fn new<T: std::ops::RangeBounds<u16>>(n: u16, m: u16, range: T) -> AtlasConfig {
        // 起点：无界取 0，含端直接用，排他端 +1（把 Excluded 语义转成 Included）。
        let start_index = match range.start_bound() {
            std::ops::Bound::Unbounded => 0,
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => i + 1,
        };
        // 终点：无界取全部格子数 n*m，含端 -1，排他端直接用。
        let end_index = match range.end_bound() {
            std::ops::Bound::Unbounded => n * m,
            std::ops::Bound::Included(i) => i - 1,
            std::ops::Bound::Excluded(i) => *i,
        };

        AtlasConfig {
            n,
            m,
            start_index,
            end_index,
        }
    }
}

// 默认配置：一组“温和但可见”的参数，保证不填任何字段也能看到粒子效果。
// 关键默认值的取舍：
// * amount = 8               —— 一批 8 个足以看出形状，又不会在小特效上过密；
// * initial_velocity = 50.0  —— 配合默认 lifetime = 1.0，粒子约移动 50 个世界单位，量级合适；
// * size = 10.0              —— 与上述速度量级匹配的可见尺寸；
// * 各类 randomness 一律 0   —— 保证默认行为完全确定，便于作者调试后再逐步加随机；
// * initial_direction = (0,-1) —— 屏幕坐标 y 轴向下，所以这是“向上喷发”；
// * lifetime = 1.0、gravity = 0、无纹理/曲线/材质 —— 最简可用形态。
impl Default for EmitterConfig {
    fn default() -> EmitterConfig {
        EmitterConfig {
            local_coords: false,
            emission_shape: EmissionShape::Point,
            one_shot: false,
            lifetime: 1.0,
            lifetime_randomness: 0.0,
            amount: 8,
            shape: ParticleShape::Rectangle { aspect_ratio: 1.0 },
            explosiveness: 0.0,
            emitting: true,
            initial_direction: vec2(0., -1.),
            initial_direction_spread: 0.,
            initial_velocity: 50.0,
            initial_velocity_randomness: 0.0,
            linear_accel: 0.0,
            initial_rotation: 0.0,
            initial_rotation_randomness: 0.0,
            initial_angular_velocity: 0.0,
            initial_angular_velocity_randomness: 0.0,
            angular_accel: 0.0,
            angular_damping: 0.0,
            size: 10.0,
            size_randomness: 0.0,
            size_curve: None,
            blend_mode: BlendMode::Alpha,
            base_color: WHITE,
            colors_curve: ColorCurve::default(),
            gravity: vec2(0.0, 0.0),
            texture: None,
            atlas: None,
            material: None,
            post_processing: None,
        }
    }
}

/// 传给 GPU 的逐粒子实例数据，每帧整体重传一遍。
///
/// `#[repr(C)]` 必不可少：这四组 Vec4 要按固定步长与偏移被 GPU 顶点属性读取，
/// 若允许 Rust 重排字段，属性偏移就会对不上。
#[repr(C)]
struct GpuParticle {
    /// xy = 位置，z = 旋转角，w = 当前尺寸。
    /// 把 4 个 float 打包成一个 vec4，正好对应一个 Float4 顶点属性，减少属性槽数量。
    pos: Vec4,
    /// 精灵表 UV：xy = 采样起点，zw = 单格尺寸（无精灵表时为 (0,0,1,1)）。
    uv: Vec4,
    /// x = 粒子生成序号（单调递增，仅供 shader 做去相关用），y = 生命周期归一化进度 0..1。
    data: Vec4,
    /// 当前 RGBA 颜色（每帧由 CPU 侧颜色曲线计算后写入）。
    color: Vec4,
}

/// 只在 CPU 侧维护的粒子状态：GPU 用不到、但做数值积分必需的量。
/// 与 `gpu_particles` 一一对应、同下标配对；分离存储是为了让每帧只上传 GPU 真正需要的部分。
struct CpuParticle {
    /// 当前速度（位置积分用）。
    velocity: Vec2,
    /// 当前角速度（旋转积分用）。
    angular_velocity: f32,
    /// 已存活时长（秒）。
    lived: f32,
    /// 该粒子的总存活时长（秒，可能带随机扰动）。
    lifetime: f32,
    /// 精灵表当前帧下标。
    frame: u16,
    /// 初始尺寸（尺寸曲线在此基础上缩放）。
    initial_size: f32,
    /// 粒子自身的基色，与颜色曲线相乘。
    color: Color,
}

/// GPU 粒子发射器。
///
/// 设计取舍：CPU 侧只保留少量积分状态（`cpu_counterpart`），每帧把结果整理成
/// `GpuParticle` 实例数据整体上传，再用一次“实例化绘制”提交所有粒子。
/// 这样数据流是单向的（只上传、不回读），不需要 transform feedback 之类的回读机制，
/// 也避免了每个粒子上一次 draw call 的开销。
pub struct Emitter {
    /// 粒子绘制管线：第二个顶点缓冲按 PerInstance 步进。
    pipeline: Pipeline,
    /// 粒子绘制绑定：几何缓冲 + 实例缓冲 + 纹理。
    bindings: Bindings,
    /// 后处理离屏 pass（固定 320×200 的 render target）。
    post_processing_pass: RenderPass,
    /// 后处理整屏回贴的管线。
    post_processing_pipeline: Pipeline,
    /// 后处理整屏回贴的绑定（全屏四边形 + 离屏纹理）。
    post_processing_bindings: Bindings,

    /// GPU 实例数据；每帧从 `cpu_counterpart` 重算并整体重传。
    gpu_particles: Vec<GpuParticle>,
    /// CPU 侧积分状态，与 `gpu_particles` 同下标一一配对。
    cpu_counterpart: Vec<CpuParticle>,

    /// 上一次发射发生的时刻（相对 `time_passed`）。
    last_emit_time: f32,
    /// 发射器累计推进的时间（秒），只在 `emitting` 为 true 时增长。
    time_passed: f32,

    /// 累计生成过的粒子数，用作粒子的单调递增 id（写进 `GpuParticle.data.x`）。
    particles_spawned: u64,
    /// 发射器的世界位置，每次 `draw` 时由参数更新。
    position: Vec2,

    /// 采样后的尺寸曲线缓存，避免每帧重新采样；配置变更时由 `rebuild_size_curve` 重建。
    batched_size_curve: Option<BatchedCurve>,

    /// 当前已应用到管线上的混合模式，用于检测配置变化并重设混合状态。
    blend_mode: BlendMode,
    /// 网格脏标记：形状被外部修改后置位，下一次 `update` 时重建几何缓冲。
    mesh_dirty: bool,

    /// 对外可见的配置，可在运行时直接修改（下一帧生效）。
    pub config: EmitterConfig,
}

// 发射器的构造、逐帧更新（发射 + 积分 + 淘汰），以及自建 miniquad pass 的三段式绘制。
impl Emitter {
    /// 单个发射器可容纳的粒子上限。
    /// 12000 是经验值：足以容纳常见打击特效的单次爆发，又不会让实例缓冲过大
    /// （每粒子 64 字节，约 768 KB），同时也作为 `emit_particle` 的硬性截断值。
    const MAX_PARTICLES: usize = 12000;

    /// 创建发射器：预分配实例缓冲、构建几何绑定、编译着色器、准备后处理 pass。
    pub fn new(config: EmitterConfig) -> Emitter {
        // SAFETY: 渲染线程内取全局上下文单例，上下文已初始化；只借用其内部字段。
        let InternalGlContext { quad_context: ctx, .. } = unsafe { get_internal_gl() };

        // empty, dynamic instance-data vertex buffer
        // 阶段 1：一次性按上限预分配实例缓冲，并用 `Buffer::stream` 标记为“频繁更新”。
        // 用 stream 是因为每帧都要整块重写（动态顶点缓冲），
        // 预分配上限则避免粒子数增长过程中反复重建 GPU 缓冲。
        let positions_vertex_buffer = Buffer::stream(ctx, BufferType::VertexBuffer, Self::MAX_PARTICLES * std::mem::size_of::<GpuParticle>());

        // 阶段 2：构建几何/索引/纹理绑定（把上面的实例缓冲作为第二个顶点缓冲槽）。
        let bindings = config.shape.build_bindings(ctx, positions_vertex_buffer, config.texture);

        // 阶段 3：选择着色器源码。配置了自定义材质就用它，否则用内置的一对。
        let (vertex, fragment) = config
            .material
            .as_ref()
            .map_or_else(|| (shader::VERTEX, shader::FRAGMENT), |material| (&material.vertex, &material.fragment));

        // 阶段 4：预处理并编译着色器。
        // `preprocess_shader` 会把 `#include "particles.glsl"` 替换成 include_str! 内联进来的源码，
        // 让顶点/片元着色器共用同一份粒子变换逻辑（属性声明、旋转、生命周期缩放等）。
        // 之所以自己注入而不是让 GLSL 直接 include：GL ES 2.0 没有 #include 指令。
        let shader = {
            use macroquad::material::shaders::{preprocess_shader, PreprocessorConfig};

            let config = PreprocessorConfig {
                includes: vec![("particles.glsl".to_string(), include_str!("particles.glsl").to_owned())],
            };

            let vertex = preprocess_shader(vertex, &config);
            let fragment = preprocess_shader(fragment, &config);

            Shader::new(ctx, &vertex, &fragment, shader::meta()).unwrap()
        };

        // 阶段 5：主绘制管线。
        // 两个顶点缓冲槽：槽 0 是几何（每顶点步进），槽 1 是实例数据（PerInstance 步进，
        // 即每个粒子推进一次读取下标）。共 7 个属性：前 3 个来自几何（pos/uv/color），
        // 后 4 个来自实例缓冲，正好对应 `GpuParticle` 的 pos/uv/data/color 四个 Vec4。
        let blend_mode = config.blend_mode.blend_state();
        let pipeline = Pipeline::with_params(
            ctx,
            &[
                BufferLayout::default(),
                BufferLayout {
                    step_func: VertexStep::PerInstance,
                    ..Default::default()
                },
            ],
            &[
                VertexAttribute::with_buffer("in_attr_pos", VertexFormat::Float3, 0),
                VertexAttribute::with_buffer("in_attr_uv", VertexFormat::Float2, 0),
                VertexAttribute::with_buffer("in_attr_color", VertexFormat::Float4, 0),
                VertexAttribute::with_buffer("in_attr_inst_pos", VertexFormat::Float4, 1),
                VertexAttribute::with_buffer("in_attr_inst_uv", VertexFormat::Float4, 1),
                VertexAttribute::with_buffer("in_attr_inst_data", VertexFormat::Float4, 1),
                VertexAttribute::with_buffer("in_attr_inst_color", VertexFormat::Float4, 1),
            ],
            shader,
            PipelineParams {
                color_blend: Some(blend_mode),
                // alpha 通道固定用 (Zero, One)，即 result_a = 0*src_a + 1*dst_a = dst_a：
                // 保持目标原有 alpha 不被粒子改写，只混合颜色，避免粒子污染后续合成的透明度。
                alpha_blend: Some(BlendState::new(Equation::Add, BlendFactor::Zero, BlendFactor::One)),
                ..Default::default()
            },
        );

        // 阶段 6：后处理管线——极简的一趟“纹理拷贝”着色器，负责把离屏结果贴回屏幕。
        let post_processing_shader =
            Shader::new(ctx, post_processing_shader::VERTEX, post_processing_shader::FRAGMENT, post_processing_shader::meta()).unwrap();

        let post_processing_pipeline = Pipeline::with_params(
            ctx,
            &[BufferLayout::default(), BufferLayout::default()],
            &[
                VertexAttribute::with_buffer("pos", VertexFormat::Float2, 0),
                VertexAttribute::with_buffer("uv", VertexFormat::Float2, 0),
            ],
            post_processing_shader,
            PipelineParams {
                // 回贴时用标准 alpha 混合，使降采样后的粒子团与画面其余部分正常叠加。
                color_blend: Some(BlendState::new(
                    Equation::Add,
                    BlendFactor::Value(BlendValue::SourceAlpha),
                    BlendFactor::OneMinusValue(BlendValue::SourceAlpha),
                )),
                ..Default::default()
            },
        );
        // 阶段 7：后处理离屏目标，固定 320×200。
        // 尺寸固定且极小，是为了让粒子整体先被降采样成一张低分辨率纹理再放大回屏幕，
        // 从而得到柔和的“整片发光/模糊”观感；用 Nearest 过滤避免额外的插值开销。
        let post_processing_pass = {
            let color_img = Texture::new_render_texture(
                ctx,
                TextureParams {
                    width: 320,
                    height: 200,
                    format: TextureFormat::RGBA8,
                    ..Default::default()
                },
            );
            color_img.set_filter(ctx, FilterMode::Nearest);

            RenderPass::new(ctx, color_img, None)
        };

        // 阶段 8：回贴用的全屏四边形（两个三角形）与其纹理绑定。
        let post_processing_bindings = {
            #[rustfmt::skip]
            let vertices: &[f32] = &[
                // positions   uv
                -1.0, -1.0,    0.0, 0.0,
                 1.0, -1.0,    1.0, 0.0,
                 1.0,  1.0,    1.0, 1.0,
                -1.0,  1.0,    0.0, 1.0,
            ];

            let vertex_buffer = Buffer::immutable(ctx, BufferType::VertexBuffer, vertices);

            #[rustfmt::skip]
            let indices: &[u16] = &[
                0, 1, 2, 0, 2, 3
            ];
            let index_buffer = Buffer::immutable(ctx, BufferType::IndexBuffer, indices);
            Bindings {
                vertex_buffers: vec![vertex_buffer],
                index_buffer,
                images: vec![post_processing_pass.texture(ctx)],
            }
        };

        // 阶段 9：组装结构体。两个粒子数组都按上限预分配，避免运行时扩容导致换帧抖动。
        Emitter {
            blend_mode: config.blend_mode,
            batched_size_curve: config.size_curve.as_ref().map(|curve| curve.batch()),
            post_processing_pass,
            post_processing_pipeline,
            post_processing_bindings,
            config,
            pipeline,
            bindings,
            position: vec2(0.0, 0.0),
            gpu_particles: Vec::with_capacity(Self::MAX_PARTICLES),
            cpu_counterpart: Vec::with_capacity(Self::MAX_PARTICLES),
            particles_spawned: 0,
            last_emit_time: 0.0,
            time_passed: 0.0,
            mesh_dirty: false,
        }
    }

    /// 配置里的尺寸曲线被修改后调用，重新采样曲线缓存。
    /// 曲线一旦采样就不再每帧变化，因此只在编辑期调用，避免逐帧重复计算。
    pub fn rebuild_size_curve(&mut self) {
        self.batched_size_curve = self.config.size_curve.as_ref().map(|curve| curve.batch());
    }

    /// 标记几何网格需要重建（形状被修改后调用）。
    /// 仅置位标记而不立即重建：重建需要 `&mut Context`，而修改配置的地方未必持有它；
    /// 实际重建推迟到下一次 `update`，那里能拿到上下文。
    pub fn update_particle_mesh(&mut self) {
        self.mesh_dirty = true;
    }

    /// 生成一个粒子，并把初始状态同时写入 GPU/CPU 两份数组。
    ///
    /// 达到 `MAX_PARTICLES` 时直接丢弃新粒子而不是淘汰旧粒子：特效应在爆发时保持稳定，
    /// 淘汰会让已有粒子突然消失，比“少发几个”更容易被肉眼察觉。
    fn emit_particle(&mut self, offset: Vec2) {
        if self.gpu_particles.len() == Self::MAX_PARTICLES {
            return;
        }
        // 在生成区域内取一个随机偏移，叠加到调用方给出的基准位置上。
        let offset = offset + self.config.emission_shape.gen_random_point();

        // 把方向向量在 ±spread/2 内随机旋转，再乘以速度得到初始速度矢量。
        // 用四元数绕 z 轴旋转而不是手写 sin/cos，可以避免角度正负约定写反这类错误。
        fn random_initial_vector(dir: Vec2, spread: f32, velocity: f32) -> Vec2 {
            let angle = rand::gen_range(-spread / 2.0, spread / 2.0);

            let quat = glam::Quat::from_rotation_z(angle);
            let dir = quat * vec3(dir.x, dir.y, 0.0);
            let res = dir * velocity;

            vec2(res.x, res.y)
        }

        // 尺寸与旋转角按各自的随机比例扰动（公式见 EmitterConfig 的字段说明）。
        let r = self.config.size - self.config.size * rand::gen_range(0.0, self.config.size_randomness);

        let rotation = self.config.initial_rotation - self.config.initial_rotation * rand::gen_range(0.0, self.config.initial_rotation_randomness);

        // 初始位置：本地坐标直接用偏移，世界坐标还要叠加发射器当前位置。
        // 两个分支其余字段完全一致；`data.y = 0` 表示生命周期进度从 0 开始，
        // `uv = (1,1,0,0)` 表示不使用精灵表时的默认采样参数。
        let particle = if self.config.local_coords {
            GpuParticle {
                pos: vec4(offset.x, offset.y, rotation, r),
                uv: vec4(1.0, 1.0, 0.0, 0.0),
                data: vec4(self.particles_spawned as f32, 0.0, 0.0, 0.0),
                color: self.config.colors_curve.start.to_vec(),
            }
        } else {
            GpuParticle {
                pos: vec4(self.position.x + offset.x, self.position.y + offset.y, rotation, r),
                uv: vec4(1.0, 1.0, 0.0, 0.0),
                data: vec4(self.particles_spawned as f32, 0.0, 0.0, 0.0),
                color: self.config.colors_curve.start.to_vec(),
            }
        };

        // 序号在写入 GpuParticle 之后自增，因此每个粒子拿到的是唯一的、单调递增的 id，
        // 着色器可用它做逐粒子的去相关（避免同批次粒子完全同步）。
        self.particles_spawned += 1;
        self.gpu_particles.push(particle);
        // CPU 侧记录积分所需的初值：速度矢量、角速度、寿命（含随机扰动）、基色。
        // 初始速度的扰动在 `random_initial_vector` 里由 `velocity` 参数完成。
        self.cpu_counterpart.push(CpuParticle {
            velocity: random_initial_vector(
                vec2(self.config.initial_direction.x, self.config.initial_direction.y),
                self.config.initial_direction_spread,
                self.config.initial_velocity - self.config.initial_velocity * rand::gen_range(0.0, self.config.initial_velocity_randomness),
            ),
            angular_velocity: self.config.initial_angular_velocity
                - self.config.initial_angular_velocity * rand::gen_range(0.0, self.config.initial_angular_velocity_randomness),
            lived: 0.0,
            lifetime: self.config.lifetime - self.config.lifetime * rand::gen_range(0.0, self.config.lifetime_randomness),
            frame: 0,
            initial_size: r,
            color: self.config.base_color,
        });
    }

    /// 推进一帧：重建脏网格 → 按速率发射 → 逐粒子积分 → 淘汰死亡粒子 → 上传实例缓冲。
    ///
    /// `dt` 由调用方传入（fork 的改动之一），因此暂停/变速时粒子时间可与谱面严格同步。
    fn update(&mut self, ctx: &mut Context, dt: f32) {
        // 阶段 1：几何网格若被标记为脏，用新的形状重建绑定（复用例缓冲槽，不重新分配实例缓冲）。
        if self.mesh_dirty {
            self.bindings = self
                .config
                .shape
                .build_bindings(ctx, self.bindings.vertex_buffers[1], self.config.texture);
            self.mesh_dirty = false;
        }
        // 阶段 2：发射。不采用“每帧固定发 N 个”，而是按“粒子数随时间铺开”的速率模型，
        // 这样发射效果与帧率解耦：帧率变化只会改变一次发几个，不会改变整体发射密度。
        if self.config.emitting {
            self.time_passed += dt;

            // 平均每个粒子占用的时间片：整批 amount 个粒子应在 lifetime 内均匀铺开。
            // explosiveness=1 时 gap 为 0，退化为“周期开头一次性全部喷出”。
            let gap = (self.config.lifetime / self.config.amount as f32) * (1.0 - self.config.explosiveness);

            let spawn_amount = if gap < 0.001 {
                // to prevent division by 0 problems
                // gap 过小时除法不可靠（explosiveness≈1），直接按整批数量发射。
                self.config.amount as usize
            } else {
                // how many particles fits into this delta time
                // 距上次发射累计的时间能容纳几个 gap 就补发几个。
                ((self.time_passed - self.last_emit_time) / gap) as usize
            };

            for _ in 0..spawn_amount {
                self.last_emit_time = self.time_passed;

                // 一个周期内最多生成 amount 个粒子：用累计生成数做闸门。
                if self.particles_spawned < self.config.amount as u64 {
                    self.emit_particle(vec2(0.0, 0.0));
                }

                // 同时存活数也不超过 amount，避免粒子过于密集。
                if self.gpu_particles.len() >= self.config.amount as usize {
                    break;
                }
            }
        }

        // 阶段 3：one_shot 复位。一个生命周期走完后把计时归零并关闭发射，
        // 让发射器回到“待命”状态；下次 `emit()` 可以重新触发。
        if self.config.one_shot && self.time_passed > self.config.lifetime {
            self.time_passed = 0.0;
            self.last_emit_time = 0.0;
            self.config.emitting = false;
        }

        // 阶段 4：逐粒子数值积分（半隐式欧拉：先更新速度，再更新位置）。
        // 两个数组按同下标 zip 遍历，天然保证 gpu/cpu 状态一一对应。
        for (gpu, cpu) in self.gpu_particles.iter_mut().zip(&mut self.cpu_counterpart) {
            // TODO: this is not quite the way to apply acceleration, this is not
            // fps independent and just wrong
            // 加速度按速度比例放大（而非固定增量），且阻尼没有乘 dt，因此严格来说与帧率相关。
            // 保留原行为未做改动。
            cpu.velocity += cpu.velocity * self.config.linear_accel * dt;
            cpu.angular_velocity += cpu.angular_velocity * self.config.angular_accel * dt;
            cpu.angular_velocity *= 1.0 - self.config.angular_damping;

            // 颜色：把生命周期归一化到 0..1，在 start→mid→end 三段之间做线性插值。
            // 之所以分两段：只有三个控制点，用 t<0.5 区分前后半程各做一次 lerp 即可。
            gpu.color = {
                let t = cpu.lived / cpu.lifetime;
                if t < 0.5 {
                    let t = t * 2.;
                    self.config.colors_curve.start.to_vec() * (1.0 - t) + self.config.colors_curve.mid.to_vec() * t
                } else {
                    let t = (t - 0.5) * 2.;
                    self.config.colors_curve.mid.to_vec() * (1.0 - t) + self.config.colors_curve.end.to_vec() * t
                }
            };
            // 再乘上粒子自身的基色，得到最终颜色。
            gpu.color *= cpu.color.to_vec();
            // 位置/旋转一次更新：把 (vx, vy, ω, 0) 乘 dt 加到 pos.xy（旋转角在 pos.z）上。
            gpu.pos += vec4(cpu.velocity.x, cpu.velocity.y, cpu.angular_velocity, 0.0) * dt;

            // 尺寸 = 初始尺寸 × 尺寸曲线在生命周期进度处的采样值（无曲线时系数为 1）。
            gpu.pos.w = cpu.initial_size * self.batched_size_curve.as_ref().map_or(1.0, |curve| curve.get(cpu.lived / cpu.lifetime));

            // 生命周期归一化进度写进 data.y，供着色器做淡出/缩放等效果。
            // 用 0 除保护：lifetime 理论上恒为正，但配置可能在运行期被改成 0。
            if cpu.lifetime != 0.0 {
                gpu.data.y = cpu.lived / cpu.lifetime;
            }

            //cpu.lived = f32::min(cpu.lived + dt, cpu.lifetime);
            // 这里只做累加、不夹紧到 lifetime：真正“过期”由下面的淘汰阶段处理，
            // 保留超出一点点的 lived 值可让淘汰判断更简单直观。
            cpu.lived += dt;
            // 重力作为独立的一项叠加（与 velocity 成正比的那部分加速度分开处理）。
            cpu.velocity += self.config.gravity * dt;

            // 精灵表帧推进：把生命周期进度线性映射到 [start_index, end_index] 的帧号区间，
            // 即“粒子活到一半，动画播到一半”。无精灵表时用整张纹理 (0,0,1,1)。
            if let Some(atlas) = &self.config.atlas {
                if cpu.lifetime != 0.0 {
                    cpu.frame = (cpu.lived / cpu.lifetime * (atlas.end_index - atlas.start_index) as f32) as u16 + atlas.start_index;
                }

                // 帧号 → 网格坐标 → uv：uv.xy 是格子起点，uv.zw 是单格尺寸（等比缩放）。
                let x = cpu.frame % atlas.n;
                let y = cpu.frame / atlas.n;

                gpu.uv = vec4(x as f32 / atlas.n as f32, y as f32 / atlas.m as f32, 1.0 / atlas.n as f32, 1.0 / atlas.m as f32);
            } else {
                gpu.uv = vec4(0.0, 0.0, 1.0, 1.0);
            }
        }

        // 阶段 5：淘汰死亡粒子。从后往前遍历以便配合 `swap_remove`。
        // `swap_remove` 是 O(1)（把末尾元素搬到空位），代价是打乱数组顺序；
        // 这里可以接受无序：粒子彼此独立，绘制顺序只影响混合叠加的先后，
        // 而 Alpha/Additive 混合的结果与顺序无关或近似无关，且每个粒子自带全部状态。
        for i in (0..self.gpu_particles.len()).rev() {
            // second if clause is just for the case when lifetime was changed in the editor
            // normally particle lifetime is always less or equal config lifetime
            // 第二个条件是为了应对编辑期把 lifetime 调小的情况：正常情况下
            // 粒子寿命不会超过配置寿命，但配置可能在运行中变小。
            if self.cpu_counterpart[i].lived >= self.cpu_counterpart[i].lifetime || self.cpu_counterpart[i].lived > self.config.lifetime {
                // 只有“自然死亡”（lived==lifetime）才回收计数；被配置改动提前淘汰的不回收，
                // 否则一个周期内的发射配额会被错误地重复释放。
                if self.cpu_counterpart[i].lived != self.cpu_counterpart[i].lifetime {
                    self.particles_spawned -= 1;
                }
                self.gpu_particles.swap_remove(i);
                self.cpu_counterpart.swap_remove(i);
            }
        }

        // 阶段 6：把活着的粒子整体重传到实例缓冲（槽 1）。用切片上传而非逐粒子更新，
        // 保证一次传输完成，避免大量小尺寸 GL 调用。
        self.bindings.vertex_buffers[1].update(ctx, &self.gpu_particles[..]);
    }

    /// Immediately emit N particles, ignoring "emitting" and "amount" params of EmitterConfig
    /// 立即发射 N 个粒子，忽略配置里的 `emitting` 与 `amount`。
    /// 这是打击命中特效的入口：命中瞬间调用即可喷出粒子，不受发射节奏限制。
    /// 注意：`emit_particle` 内部已经自增过一次生成计数，这里又自增一次，
    /// 因此一次命中会让 `particles_spawned` 增加 2N，进而可能提前触发 `update` 里的发射配额闸门。
    /// 这是上游遗留行为，本次未做修改。
    pub fn emit(&mut self, pos: Vec2, n: usize) {
        for _ in 0..n {
            self.emit_particle(pos);
            self.particles_spawned += 1;
        }
    }

    /// 执行粒子绘制：绑定几何/实例缓冲与 uniform，再用一次实例化绘制提交所有粒子。
    ///
    /// 这里直接调用 `ctx.draw(起始索引, 索引数, 实例数)`：索引数取索引缓冲的字节数除以 u16
    /// 大小（几何三角形的索引总数），实例数即当前粒子数，GPU 会对每个实例重复跑一遍几何。
    fn perform_render_pass(&mut self, quad_gl: &QuadGl, ctx: &mut Context) {
        ctx.apply_bindings(&self.bindings);
        ctx.apply_uniforms(&shader::Uniforms {
            mvp: quad_gl.get_projection_matrix(),
            emitter_position: vec3(self.position.x, self.position.y, 0.0),
            local_coords: if self.config.local_coords { 1.0 } else { 0.0 },
        });

        ctx.draw(0, self.bindings.index_buffer.size() as i32 / std::mem::size_of::<u16>() as i32, self.gpu_particles.len() as i32);
    }

    /// 开启渲染 pass 并设置管线与视口。
    ///
    /// 这里绕开 quad_gl 直接调用 `ctx.begin_pass` / `ctx.begin_default_pass`：
    /// quad_gl 内部的 pass 状态机无法与自建管线配套使用，因此改为“照着 quad_gl 当前的
    /// 渲染目标，自己开一个同样目标的 pass”，这样粒子的输出位置仍与调用者期望的一致。
    /// `PassAction::Nothing` 表示不清理已有内容（粒子要叠加在已画好的画面上）。
    pub fn setup_render_pass(&mut self, quad_gl: &QuadGl, ctx: &mut Context) {
        // 阶段 1：混合模式变了才重设管线，避免每帧都触发一次 GL 状态变更。
        if self.config.blend_mode != self.blend_mode {
            self.pipeline.set_blend(ctx, Some(self.config.blend_mode.blend_state()));
            self.blend_mode = self.config.blend_mode;
        }

        // 阶段 2：选择渲染目标。未启用后处理时直接画进 quad_gl 当前的目标（离屏或默认帧缓冲）；
        // 启用后处理时先画到 320×200 的离屏纹理，并清成透明黑作为底色。
        if self.config.post_processing.is_none() {
            let pass = quad_gl.get_active_render_pass();
            if let Some(pass) = pass {
                ctx.begin_pass(pass, PassAction::Nothing);
            } else {
                ctx.begin_default_pass(PassAction::Nothing);
            }
        } else {
            ctx.begin_pass(self.post_processing_pass, PassAction::clear_color(0.0, 0.0, 0.0, 0.0));
        };

        // 阶段 3：应用自建管线与视口。视口取 quad_gl 记录的值；若没有（例如不在其 track 之内），
        // 退化为整屏尺寸，保证至少有一个合法非零视口。
        ctx.apply_pipeline(&self.pipeline);
        // This is made
        let (x, y, w, h) = quad_gl
            .get_viewport()
            .unwrap_or_else(|| (0, 0, screen_width() as _, screen_height() as _));
        ctx.apply_viewport(x, y, w, h);
    }

    /// 结束粒子 pass；若启用了后处理，则再开一个 pass 把离屏纹理整屏贴回。
    ///
    /// 回贴阶段固定索引数 6（两个三角形），实例数 1——即一次全屏 quad 绘制，
    /// 这正是 `post_processing_shader` 里那个全屏四边形缓冲的用途。
    pub fn end_render_pass(&mut self, quad_gl: &QuadGl, ctx: &mut Context) {
        ctx.end_render_pass();

        if self.config.post_processing.is_some() {
            // 回到 quad_gl 当前的渲染目标（同样是绕开 quad_gl 自己开 pass）。
            let pass = quad_gl.get_active_render_pass();
            if let Some(pass) = pass {
                ctx.begin_pass(pass, PassAction::Nothing);
            } else {
                ctx.begin_default_pass(PassAction::Nothing);
            }

            ctx.apply_pipeline(&self.post_processing_pipeline);
            let (x, y, w, h) = quad_gl
                .get_viewport()
                .unwrap_or_else(|| (0, 0, screen_width() as _, screen_height() as _));
            ctx.apply_viewport(x, y, w, h);

            ctx.apply_bindings(&self.post_processing_bindings);

            ctx.draw(0, 6, 1);

            ctx.end_render_pass();
        }
    }

    /// 一帧的完整绘制入口。
    ///
    /// 顺序：先排空 quad_gl 的批 → 更新粒子状态 → 开 pass → 画粒子 → 关 pass（必要时回贴）。
    pub fn draw(&mut self, pos: Vec2, dt: f32) {
        // SAFETY: 渲染线程内取全局上下文单例，上下文已初始化；随后按值解构取出内部字段。
        let mut gl = unsafe { get_internal_gl() };

        // 必须先 flush：quad_gl 会把之前的绘制缓存成一个批次，若不先提交就直接切换自建 pass，
        // 那些迟到的绘制会写进错误的渲染目标，破坏上一批几何。
        gl.flush();

        let InternalGlContext { quad_context: ctx, quad_gl } = gl;

        // 记录本帧位置（`update` 里生成世界坐标粒子、绘制时 shader 计算本地坐标都要用到）。
        self.position = pos;

        self.update(ctx, dt);

        self.setup_render_pass(quad_gl, ctx);
        self.perform_render_pass(quad_gl, ctx);
        self.end_render_pass(quad_gl, ctx);
    }
}

// 粒子主管线的内联 GLSL 与 uniform 布局。
// 顶点着色器里 `#define DEF_VERTEX_ATTRIBUTES` 是给注入的 `particles.glsl` 用的预处理开关：
// 该 include 文件既被顶点着色器复用（需要声明属性），也可能被其他着色器片段复用（不需声明），
// 靠这个宏把“属性声明”部分条件编译进来，避免重复声明导致链接错误。
mod shader {
    use super::*;

    /// 粒子顶点着色器：只保留入口，真正的变换逻辑由注入的 `particles.glsl` 提供
    /// （`particle_transform_vertex` 做 MVP/旋转/尺寸，`particle_transform_uv` 做精灵表采样）。
    pub const VERTEX: &str = r#"#version 100
    #define DEF_VERTEX_ATTRIBUTES
    #include "particles.glsl"

    varying lowp vec2 texcoord;
    varying lowp vec4 color;

    void main() {
        gl_Position = particle_transform_vertex();
        color = in_attr_inst_color;
        texcoord = particle_transform_uv();
    }
    "#;

    /// 粒子片元着色器：采样纹理再乘以逐粒子颜色，即 gl_FragColor = tex * color。
    /// 无纹理时绑定的是 1×1 白纹理，因此结果退化为纯色方片。
    pub const FRAGMENT: &str = r#"#version 100
    varying lowp vec2 texcoord;
    varying lowp vec4 color;

    uniform sampler2D texture;

    void main() {
        gl_FragColor = texture2D(texture, texcoord) * color;
    }
    "#;

    /// 着色器元信息：声明图像槽与 uniform 块布局，供 miniquad 在链接后按名查找并绑定。
    /// 注意 uniform 名字带下划线前缀（`_mvp` 等），这是 miniquad 对 uniform 块字段的约定；
    /// 而 `Uniforms` 结构体的字段名不带前缀，两边顺序必须严格一致才能正确对应。
    pub fn meta() -> ShaderMeta {
        ShaderMeta {
            images: vec!["texture".to_string()],
            uniforms: UniformBlockLayout {
                uniforms: vec![
                    UniformDesc::new("_mvp", UniformType::Mat4),
                    UniformDesc::new("_local_coords", UniformType::Float1),
                    UniformDesc::new("_emitter_position", UniformType::Float3),
                ],
            },
        }
    }

    /// uniform 块的内存布局。
    /// `#[repr(C)]` 保证字段顺序与对齐与声明一致，从而可以整块上传而无需手工打包。
    #[repr(C)]
    pub struct Uniforms {
        /// 投影矩阵（来自 quad_gl 当前相机）。
        pub mvp: Mat4,
        /// 是否使用发射器本地坐标，用 0.0/1.0 表示（GLSL 侧当 float 用）。
        pub local_coords: f32,
        /// 发射器世界位置，本地坐标模式下用它把粒子变换回世界空间。
        pub emitter_position: Vec3,
    }
}

// 后处理整屏回贴用的内联 GLSL 与 uniform 布局：最简单的一趟纹理拷贝。
// 顶点直接使用裁剪空间坐标，不需要任何矩阵，因此也不需要 uniform 块。
mod post_processing_shader {
    use super::*;

    /// 全屏四边形顶点着色器：顶点坐标即裁剪空间坐标，uv 原样传给片元着色器。
    pub const VERTEX: &str = r#"#version 100
    attribute vec2 pos;
    attribute vec2 uv;

    varying lowp vec2 texcoord;

    void main() {
        gl_Position = vec4(pos, 0, 1);
        texcoord = uv;
    }
    "#;

    /// 片元着色器：直接采样离屏纹理输出，不做任何额外处理。
    /// 粒子整体“聚成一团光”的观感来自上游的 320×200 降采样与放大，而不是这个着色器。
    pub const FRAGMENT: &str = r#"#version 100
    precision lowp float;

    varying vec2 texcoord;
    uniform sampler2D tex;

    void main() {
        gl_FragColor = texture2D(tex, texcoord);
    }
    "#;

    /// 着色器元信息：只有一个图像槽，没有 uniform。
    pub fn meta() -> ShaderMeta {
        ShaderMeta {
            images: vec!["tex".to_string()],
            uniforms: UniformBlockLayout { uniforms: vec![] },
        }
    }
}
