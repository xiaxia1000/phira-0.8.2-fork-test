//! 全局渲染/音频状态聚合体 [`Resource`] 及其配套数据结构。
//!
//! `Resource` 承载整局游戏「唯一共享的可变状态」：配置、曲目信息、纹理与音效、资源包、
//! 粒子系统、批渲染缓冲、模型矩阵栈，以及离屏渲染目标。这样各绘制模块只需接收
//! `&mut Resource`，无需层层传递大量参数；代价是其中任何不变量被破坏都会全局可见——
//! 最典型的两条是「模型矩阵栈必须成对 push/pop」与「批渲染缓冲必须在帧末提交并清空」。

use super::{MSRenderTarget, Matrix, Point, NOTE_WIDTH_RATIO_BASE};
use crate::{
    config::Config,
    ext::{create_audio_manger, nalgebra_to_glm, SafeTexture},
    fs::FileSystem,
    info::ChartInfo,
    particle::{AtlasConfig, ColorCurve, Emitter, EmitterConfig},
};
use anyhow::{bail, Context, Result};
use macroquad::prelude::*;
use miniquad::{
    gl::{GLuint, GL_LINEAR},
    Texture, TextureWrap,
};
use sasa::{AudioClip, AudioManager, Sfx};
use serde::Deserialize;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    ops::DerefMut,
    path::Path,
    sync::atomic::AtomicU32,
};

/// 单个绘制批次允许容纳的最大顶点数。
///
/// 之所以取 64 而不是更大：`NoteBuffer` 会按 (层级, 纹理) 分组，再把每组切成
/// 固定规模的小块。块太大时同一批次里会混入大量顶点，既降低排序粒度，
/// 也让 GPU 端的顶点缓冲切换收益变差；64 是「批次切换开销」与「分组粒度」的折中，
/// 注释也提示该值仍可调整。它同时决定 `gl_set_drawcall_buffer_capacity` 的预分配规模。
pub const MAX_SIZE: usize = 64; // needs tweaking
/// 当前设备的像素密度（DPI），以原子量保存于全局，供纹理尺寸等换算使用。
///
/// 之所以需要它：游戏内部一切坐标都是归一化的（与分辨率无关），但 flick 判定速度、
/// 纹理像素尺寸等量必须落到真实像素上才有意义。用 `AtomicU32` 是因为它可能由窗口
/// 系统回调在其他线程更新，而读取处（[`Resource::new`]）希望无锁取得当时的快照。
pub static DPI_VALUE: AtomicU32 = AtomicU32::new(250);
/// 音频 Sfx 的缓冲区帧数。
///
/// 1024 帧在「低延迟（点击音立刻响应）」与「避免欠载爆音」之间较为平衡；
/// 所有打击音效统一使用该值，保证彼此延迟一致。
pub const BUFFER_SIZE: usize = 1024;

/// `hit_fx_scale` 的默认值（1.0），供 serde 在字段缺省时使用。
#[inline]
fn default_scale() -> f32 {
    1.
}

/// `hit_fx_duration` 的默认值（0.5 秒），供 serde 在字段缺省时使用。
#[inline]
fn default_duration() -> f32 {
    0.5
}

/// `color_perfect` 的默认值（ARGB 淡青白色），与 [`ResPackInfo::fx_perfect`] 配对。
#[inline]
fn default_perfect() -> u32 {
    0xe1ffec9f
}

/// `color_good` 的默认值（ARGB 淡蓝色），与 [`ResPackInfo::fx_good`] 配对。
#[inline]
fn default_good() -> u32 {
    0xebb4e1ff
}

/// `hit_fx_tinted` 的默认值：默认开启染色，使打击特效颜色能反映命中质量。
#[inline]
fn default_tinted() -> bool {
    true
}

/// 资源包的清单（对应资源包目录下的 `info.yml`）。
///
/// 字段全部由外部文件反序列化而来，因此每个可选项都写了 `#[serde(default)]`
/// 或默认函数，保证旧资源包缺少新字段时仍能加载。
/// `allow(dead_code)` 是因为其中部分字段只被本 crate 之外的代码读取。
#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResPackInfo {
    /// 资源包名称（必填，长度限制见 [`ResPackInfo::verify`]）
    pub name: String,
    /// 作者
    pub author: String,

    /// 打击特效贴图的横向 / 纵向帧数，用于把 hit_fx.png 切成动画帧
    pub hit_fx: (u32, u32),
    /// 单次打击特效的总播放时长（秒）
    #[serde(default = "default_duration")]
    pub hit_fx_duration: f32,
    /// 打击特效的整体缩放系数
    #[serde(default = "default_scale")]
    pub hit_fx_scale: f32,
    /// 特效是否随判定线旋转
    #[serde(default)]
    pub hit_fx_rotate: bool,
    /// 是否隐藏白色方块辅粒子（部分资源包只需要主特效）
    #[serde(default)]
    pub hide_particles: bool,
    /// 特效是否按判定结果染色；关闭则一律使用白色
    #[serde(default = "default_tinted")]
    pub hit_fx_tinted: bool,

    /// Hold 贴图中 body 与 head/tail 的纵向分界高度（普通贴图）
    pub hold_atlas: (u16, u16),
    /// 多押提示所用 hold 贴图的分界高度
    #[serde(rename = "holdAtlasMH")]
    pub hold_atlas_mh: (u16, u16),

    /// Hold 头部是否在按住期间持续显示
    #[serde(default)]
    pub hold_keep_head: bool,
    /// Hold 主体是否使用可平铺贴图（超长条不会被拉伸变形）
    #[serde(default)]
    pub hold_repeat: bool,
    /// Hold 头部/尾部是否紧凑贴合（不额外向外扩张）
    #[serde(default)]
    pub hold_compact: bool,

    /// Perfect 判定对应的配色（打包为 u32，解析方式见 `parse_color_guess_alpha`）
    #[serde(default = "default_perfect")]
    color_perfect: u32,
    /// Good 判定对应的配色（同样打包为 u32）
    #[serde(default = "default_good")]
    color_good: u32,

    /// 资源包描述文字
    #[serde(default)]
    pub description: String,
}

/// 兼容两种颜色编码：大于 0xFFFFFF 时按 ARGB 解析，否则按不带 alpha 的 RGB 解析。
///
/// 之所以「猜」而不强制一种格式：历史资源包有的写了 alpha、有的没写，
/// 强行统一必然破坏其中一部分的显示效果。
fn parse_color_guess_alpha(c: u32) -> Color {
    if c > 0xffffff {
        Color::from_hex_argb(c)
    } else {
        Color::from_hex_rgb(c)
    }
}

// 资源包清单的校验与颜色访问。
impl ResPackInfo {
    /// 校验清单元数据是否在允许范围内。
    ///
    /// 目的是尽早失败：资源包来自用户自定义目录，非法值（空名、超长描述、荒诞的帧数）
    /// 若不在加载期拦下，会在运行期表现为难以定位的渲染异常或内存问题。
    ///
    /// # Errors
    /// 名称/描述非法，或 `hit_fx` 帧数乘积不在 1..=10240 之内时返回错误。
    pub fn verify(&self) -> Result<()> {
        if self.name.is_empty() {
            bail!("empty name");
        }
        if self.name.len() > 100 {
            bail!("name too long");
        }
        if self.description.len() > 1000 {
            bail!("description too long");
        }
        if !(1..=10240).contains(&self.hit_fx.0.saturating_mul(self.hit_fx.1)) {
            bail!("Invalid hit_fx");
        }
        Ok(())
    }
    /// Perfect 判定对应的颜色。
    pub fn color_perfect(&self) -> Color {
        parse_color_guess_alpha(self.color_perfect)
    }

    /// Good 判定对应的颜色。
    pub fn color_good(&self) -> Color {
        parse_color_guess_alpha(self.color_good)
    }

    /// 打击特效在 Perfect 时应使用的颜色：未开启染色时一律返回白色。
    pub fn fx_perfect(&self) -> Color {
        if self.hit_fx_tinted {
            self.color_perfect()
        } else {
            WHITE
        }
    }

    /// 打击特效在非 Perfect（Good 及以下）时应使用的颜色。
    pub fn fx_good(&self) -> Color {
        if self.hit_fx_tinted {
            self.color_good()
        } else {
            WHITE
        }
    }
}

/// 一套音符贴图（普通与多押各有一套独立的）。
///
/// 之所以把 click/hold/flick/drag 收进同一个结构体：它们必须成套出现，混用不同风格的
/// 单张贴图会显得突兀；集中存放也便于按 `double_hint` 一次性整组切换。
pub struct NoteStyle {
    /// 点击音符贴图
    pub click: SafeTexture,
    /// 长按音符的 atlas 贴图，纵向包含 body / head / tail 三段
    pub hold: SafeTexture,
    /// 滑动音符贴图
    pub flick: SafeTexture,
    /// 拖拽音符贴图
    pub drag: SafeTexture,
    /// 可平铺的长条主体贴图；仅在资源包开启 `hold_repeat` 时才有值，
    /// 由 `hold` 裁剪中间区域而来（见 [`ResourcePack::load`]）
    pub hold_body: Option<SafeTexture>,
    /// 该套 hold 贴图的 body 与 head/tail 分界高度
    pub hold_atlas: (u16, u16),
}

// 音符贴图集的校验与 atlas UV 计算。
impl NoteStyle {
    /// 校验 atlas 分界高度是否落在贴图高度之内。
    ///
    /// 分界值越界会导致 body/head/tail 的 UV 互换甚至超出 `[0,1]`，采出随机纹理。
    ///
    /// # Errors
    /// `hold_atlas.0 + hold_atlas.1 >= hold.height()` 时返回错误。
    pub fn verify(&self) -> Result<()> {
        if self.hold_atlas.0.saturating_add(self.hold_atlas.1) as f32 >= self.hold.height() {
            bail!("Invalid atlas");
        }
        Ok(())
    }

    /// 把贴图像素坐标（纵向）换算为归一化 UV。
    #[inline]
    fn to_uv(&self, t: u16) -> f32 {
        t as f32 / self.hold.height()
    }

    /// hold 贴图的宽高比，用于按绘制宽度反推高度，避免贴图被拉伸。
    pub fn hold_ratio(&self) -> f32 {
        self.hold.height() / self.hold.width()
    }

    /// 头部贴图区域：位于贴图底部（UV 从 `1 - atlas.1` 到 1）。
    ///
    /// 之所以让 head 与 tail 分占贴图两端、body 落在中间：一张纵向 atlas 就能同时放下
    /// 三段，且 body 的中间区间可以直接裁剪出来做 Repeat 平铺。
    pub fn hold_head_rect(&self) -> Rect {
        let sy = self.to_uv(self.hold_atlas.1);
        Rect::new(0., 1. - sy, 1., sy)
    }

    /// 主体贴图区域：`atlas.1` 与 `1 - atlas.1` 之间的纵向区间。
    pub fn hold_body_rect(&self) -> Rect {
        let sy = self.to_uv(self.hold_atlas.0);
        let ey = 1. - self.to_uv(self.hold_atlas.1);
        Rect::new(0., sy, 1., ey - sy)
    }

    /// 尾部贴图区域：贴图顶部（UV 从 0 到 `atlas.0`）。
    pub fn hold_tail_rect(&self) -> Rect {
        let ey = self.to_uv(self.hold_atlas.0);
        Rect::new(0., 0., 1., ey)
    }
}

/// 一个完整资源包的运行期表示：清单 + 已解码的纹理与音效。
///
/// 音效在此阶段只拿到 `AudioClip`（解码后的样本）；真正用于播放的 `Sfx`
/// 需要等音频管理器初始化后再创建（见 [`Resource::new`]），因为 `Sfx` 与音频设备绑定，
/// 无法在纯资源加载阶段构造。
pub struct ResourcePack {
    /// 清单信息
    pub info: ResPackInfo,
    /// 普通音符贴图集
    pub note_style: NoteStyle,
    /// 多押提示音符贴图集
    pub note_style_mh: NoteStyle,
    /// 点击音效
    pub sfx_click: AudioClip,
    /// 拖拽音效
    pub sfx_drag: AudioClip,
    /// 滑动音效
    pub sfx_flick: AudioClip,
    /// 曲终音效
    pub ending: AudioClip,
    /// 打击特效的 atlas 贴图
    pub hit_fx: SafeTexture,
}

// 资源包加载入口：支持自定义路径，也支持内置默认资源。
impl ResourcePack {
    /// 从给定路径加载资源包；`path` 为 `None` 时回退到内置资源包。
    ///
    /// 回退到内置资源的意义在于：玩家没有安装资源包时游戏依然可玩。
    ///
    /// # Errors
    /// 路径无法打开，或资源包内容缺失/非法时返回错误。
    pub async fn from_path<T: AsRef<Path>>(path: Option<T>) -> Result<Self> {
        Self::load(
            if let Some(path) = path {
                crate::fs::fs_from_file(path.as_ref())?
            } else {
                crate::fs::fs_from_assets("respack/")?
            }
            .deref_mut(),
        )
        .await
    }

    /// 从给定的文件系统抽象加载资源包。
    ///
    /// 之所以接收 `FileSystem` 而非直接的路径：资源可能来自磁盘目录，也可能被打包进
    /// 可执行文件（不同发行方式/平台下形式不同），用统一抽象屏蔽差异。
    ///
    /// # Errors
    /// 缺少 `info.yml`/`hit_fx.png`/音符贴图，清单校验失败，或音效解码失败时返回错误。
    pub async fn load(fs: &mut dyn FileSystem) -> Result<Self> {
        // 该宏只是为了避免四套贴图重复书写「加载字节 → 解码 → 转纹理 → 设线性过滤」。
        // 之所以统一用线性过滤：音符贴图会被频繁缩放绘制，最近邻采样会出现明显锯齿。
        macro_rules! load_tex {
            ($path:literal) => {
                SafeTexture::from(image::load_from_memory(&fs.load_file($path).await.with_context(|| format!("Missing {}", $path))?)?)
                    .with_filter(GL_LINEAR)
            };
        }
        let info: ResPackInfo = serde_yaml::from_str(&String::from_utf8(fs.load_file("info.yml").await.context("Missing info.yml")?)?)?;
        info.verify()?;
        let mut note_style = NoteStyle {
            click: load_tex!("click.png"),
            hold: load_tex!("hold.png"),
            flick: load_tex!("flick.png"),
            drag: load_tex!("drag.png"),
            hold_body: None,
            hold_atlas: info.hold_atlas,
        };
        note_style.verify()?;
        let mut note_style_mh = NoteStyle {
            click: load_tex!("click_mh.png"),
            hold: load_tex!("hold_mh.png"),
            flick: load_tex!("flick_mh.png"),
            drag: load_tex!("drag_mh.png"),
            hold_body: None,
            hold_atlas: info.hold_atlas_mh,
        };
        note_style_mh.verify()?;

        // 开启 hold_repeat 时，从 hold 贴图中裁出中间的主体区域，作为独立的可平铺纹理。
        // 之所以必须单独裁剪并设为 Repeat 环绕：长条可能极长，用 atlas 的固定区间拉伸
        // 会严重糊化，平铺则能保持纹理密度不变。
        if info.hold_repeat {
            fn get_body(style: &mut NoteStyle) {
                let pixels = style.hold.get_texture_data();
                let width = style.hold.width() as u16;
                let height = style.hold.height() as u16;
                let atlas = style.hold_atlas;
                let res = Texture2D::from_rgba8(
                    width,
                    height - atlas.0 - atlas.1,
                    &pixels.bytes[(atlas.0 as usize * width as usize * 4)..(pixels.bytes.len() - atlas.1 as usize * width as usize * 4)],
                );
                // 环绕模式必须直接操作 miniquad 句柄：macroquad 的高层 API 未暴露该参数。
                let context = unsafe { get_internal_gl() }.quad_context;
                res.raw_miniquad_texture_handle().set_wrap(context, TextureWrap::Repeat);
                style.hold_body = Some(res.into());
            }
            get_body(&mut note_style);
            get_body(&mut note_style_mh);
        }
        let hit_fx = image::load_from_memory(&fs.load_file("hit_fx.png").await.context("Missing hit_fx.png")?)?.into();

        // 音效格式探测顺序：ogg → wav → mp3，全部缺失时回退到内置默认 ogg。
        // 允许三种格式是因为资源包作者未必统一导出格式；最后回退而不是直接报错，
        // 保证即使资源包不自带音效，谱面依然可以正常游玩。
        macro_rules! load_clip {
            ($path:literal) => {
                if let Some(sfx) = fs
                    .load_file(format!("{}.ogg", $path).as_str())
                    .await
                    .ok()
                    .map(|it| AudioClip::new(it))
                    .transpose()?
                {
                    sfx
                } else if let Some(sfx) = fs
                    .load_file(format!("{}.wav", $path).as_str())
                    .await
                    .ok()
                    .map(|it| AudioClip::new(it))
                    .transpose()?
                {
                    sfx
                } else if let Some(sfx) = fs
                    .load_file(format!("{}.mp3", $path).as_str())
                    .await
                    .ok()
                    .map(|it| AudioClip::new(it))
                    .transpose()?
                {
                    sfx
                } else {
                    AudioClip::new(load_file(format!("{}.ogg", $path).as_str()).await?)?
                }
            };
        }

        Ok(Self {
            info,
            note_style,
            note_style_mh,
            sfx_click: load_clip!("click"),
            sfx_drag: load_clip!("drag"),
            sfx_flick: load_clip!("flick"),
            ending: load_clip!("ending"),
            hit_fx,
        })
    }
}

/// 打击特效粒子发射器。
///
/// 之所以需要**两个**发射器：主特效是资源包提供的 hit_fx atlas 动画（带旋转、
/// 按判定结果染色，负责表达「准不准」），辅特效是一批纯白色方块向外飞散
/// （无贴图、统一观感，负责表达「打到了没有、力度多大」）。两者的运动学参数
/// （初速度、加速度、扩散角、尺寸）完全不同，无法用同一个 `Emitter` 表达，
/// 因此并列维护；`hide_particles` 允许资源包只保留主特效。
pub struct ParticleEmitter {
    /// 资源包给出的整体缩放系数（基准尺寸，见 [`ParticleEmitter::set_scale`]）
    pub scale: f32,
    /// 主发射器：绘制资源包的 hit_fx atlas
    pub emitter: Emitter,
    /// 辅助发射器：绘制白色方块
    pub emitter_square: Emitter,
    /// 是否禁用辅发射器
    pub hide_particles: bool,
}

// 粒子发射器的构造与调用。
impl ParticleEmitter {
    /// 依据资源包配置创建两个发射器。
    ///
    /// # Arguments
    /// * `scale` - 玩家设置的整体缩放（通常与音符缩放一致），使特效大小与音符协调。
    /// * `hide_particles` - 是否隐藏白色方块辅粒子。
    ///
    /// # Errors
    /// 底层粒子系统初始化失败时返回错误。
    pub fn new(res_pack: &ResourcePack, scale: f32, hide_particles: bool) -> Result<Self> {
        // 颜色包络：白色起、中途 alpha 0.7、结束为 0。
        // 之所以用三段而非全程线性淡出：打击特效需要「瞬间最亮、随后断崖式消失」的观感，
        // 全程线性会显得拖沓、糊成一团。
        let colors_curve = {
            let start = WHITE;
            let mut mid = start;
            let mut end = start;
            mid.a *= 0.7;
            end.a = 0.;
            ColorCurve { start, mid, end }
        };
        let mut res = Self {
            scale: res_pack.info.hit_fx_scale,
            // 主发射器：使用资源包 atlas，所有随机量都关闭（旋转/寿命/方向），
            // 保证每一次打击特效完全一致，玩家才能据此判断命中质量。
            // `local_coords: false` 表示粒子位置用世界坐标，不随发射器本体移动。
            emitter: Emitter::new(EmitterConfig {
                local_coords: false,
                texture: Some(*res_pack.hit_fx),
                lifetime: res_pack.info.hit_fx_duration,
                lifetime_randomness: 0.0,
                initial_rotation_randomness: 0.0,
                initial_direction_spread: 0.0,
                initial_velocity: 0.0,
                atlas: Some(AtlasConfig::new(res_pack.info.hit_fx.0 as _, res_pack.info.hit_fx.1 as _, ..)),
                emitting: false,
                colors_curve,
                ..Default::default()
            }),
            // 辅发射器：无贴图的白色方块，360° 全向扩散，带随机尺寸与初速度；
            // 负的 `linear_accel` 让方块飞出后迅速减速，形成「炸开后立刻收住」的手感。
            // 初速度与加速度都按 `scale` 缩放（尺寸缩放见 set_scale）。
            emitter_square: Emitter::new(EmitterConfig {
                local_coords: false,
                lifetime: res_pack.info.hit_fx_duration,
                lifetime_randomness: 0.0,
                initial_direction_spread: 2. * std::f32::consts::PI,
                size_randomness: 0.3,
                emitting: false,
                initial_velocity: 2.5 * scale,
                initial_velocity_randomness: 1. / 10.,
                linear_accel: -6. / 1.,
                colors_curve,
                ..Default::default()
            }),
            hide_particles,
        };
        res.set_scale(scale);
        Ok(res)
    }

    /// 在指定屏幕位置发射一次打击特效。
    ///
    /// 主特效发 1 个粒子、辅特效发 4 个粒子：单个主粒子保证命中判定看起来清晰不糊，
    /// 4 个方块提供足够的视觉冲击，又不会在连打时堆成一片白。
    /// 颜色由调用方按判定结果（perfect/good 或谱面自定义色）传入。
    ///
    /// # Arguments
    /// * `pt` - 屏幕坐标，因此调用前必须经 `world_to_screen` 转换。
    /// * `rotation` - 主特效初始朝向（弧度）；未开启 `hit_fx_rotate` 时调用方传 0。
    pub fn emit_at(&mut self, pt: Vec2, rotation: f32, color: Color) {
        self.emitter.config.initial_rotation = rotation;
        self.emitter.config.base_color = color;
        self.emitter.emit(pt, 1);
        if !self.hide_particles {
            self.emitter_square.config.base_color = color;
            self.emitter_square.emit(pt, 4);
        }
    }

    /// 按帧间隔推进并绘制两个发射器的全部粒子。
    pub fn draw(&mut self, dt: f32) {
        self.emitter.draw(vec2(0., 0.), dt);
        self.emitter_square.draw(vec2(0., 0.), dt);
    }

    /// 更新整体缩放。
    ///
    /// 除数 5 与 44 是经验系数：让主特效与方块粒子的基准尺寸都落在「约一个音符宽度」的
    /// 视觉尺度上（两者原始尺寸不同，故除数差别很大）。改动这两个常数会直接改变所有
    /// 谱面的打击观感，因此不应随意调整。
    pub fn set_scale(&mut self, scale: f32) {
        self.emitter.config.size = self.scale * scale / 5.;
        self.emitter_square.config.size = self.scale * scale / 44.;
    }
}

/// 顶点批次表：键为 (绘制层级, 纹理 GL id)，值为若干「顶点数组 + 索引数组」网格。
///
/// 之所以用 `BTreeMap` 而非 `HashMap`：`draw_all` 必须**按层级有序**遍历，
/// 层级顺序直接决定音符间的遮挡关系（见 `NoteKind::order`），哈希表会打乱该顺序。
/// 每个键下再切成多个不超过 [`MAX_SIZE`] 顶点的网格，用于控制单次 `geometry` 的规模。
type NoteBufferMap = BTreeMap<(i8, GLuint), Vec<(Vec<Vertex>, Vec<u16>)>>;

/// 音符批渲染缓冲。
///
/// 所有音符（以及 `BadNote`）都不直接发 draw call，而是先把四边形顶点压进来，
/// 帧末由 [`NoteBuffer::draw_all`] 统一提交。这样能把成百上千个音符的 draw call
/// 压缩到「层级数 × 纹理数」的量级，是本项目性能的关键。
/// 用 `RefCell` 包装，是因为绘制路径只拿得到 `&Resource`，而缓冲又必须可变借用。
#[derive(Default)]
pub struct NoteBuffer(NoteBufferMap);

// 顶点入队与批量提交。
impl NoteBuffer {
    /// 把一个四边形（4 个顶点）追加到指定 (层级, 纹理) 分组。
    ///
    /// # Arguments
    /// * `key` - (绘制层级, 纹理 GL id)。
    /// * `vertices` - 四个顶点，顺序必须是左上、右上、右下、左下。
    ///
    /// 索引固定生成为 `0,1,2` 与 `0,2,3` 两个三角形，与调用方压入的顶点顺序严格对应，
    /// 顺序颠倒会导致纹理错乱。当当前网格再加 4 个顶点会超过 [`MAX_SIZE`] 时新开一个网格，
    /// 从而把单个绘制单元限制在固定规模内。
    pub fn push(&mut self, key: (i8, GLuint), vertices: [Vertex; 4]) {
        let meshes = self.0.entry(key).or_default();
        if meshes.last().is_none_or(|it| it.0.len() + 4 > MAX_SIZE * 4) {
            meshes.push(Default::default());
        }
        let last = meshes.last_mut().unwrap();
        let i = last.0.len() as u16;
        last.0.extend_from_slice(&vertices);
        last.1.extend_from_slice(&[i, i + 1, i + 2, i, i + 2, i + 3]);
    }

    /// 提交本帧累积的所有批次，并清空缓冲。
    ///
    /// 关键点：
    /// - 先 `gl.flush()`：macroquad 自身的绘制命令与本项目直接调用的低层 `geometry`
    ///   共用同一条命令流，不先冲刷就会让我们的顶点插到错误的 render_pass / 模型矩阵状态下；
    /// - `std::mem::take` 把缓冲整体移走，保证提交后不留残余（否则下一帧会重复绘制）；
    /// - 每个网格依据键中记录的原始 miniquad 纹理句柄重建 `Texture2D`：之所以不直接
    ///   保存 `Texture2D`，是因为该结构在本项目多处会被移动/克隆，而 GL id 稳定且复制成本极低。
    pub fn draw_all(&mut self) {
        let mut gl = unsafe { get_internal_gl() };
        gl.flush();
        let gl = gl.quad_gl;
        gl.draw_mode(DrawMode::Triangles);
        // SAFETY: `Texture::from_raw_id` 用裸 id 重建纹理句柄，其前提是该 id 仍然有效。
        // 这里的 id 全部来自本帧 `push` 时从存活纹理上取得的 gl_internal_id，
        // 且缓冲在同一帧内被 take 走、用完即弃，因此不会出现悬垂引用。
        // 格式固定为 RGBA8，与资源包贴图和离屏渲染目标的格式保持一致。
        for ((_, tex_id), meshes) in std::mem::take(&mut self.0).into_iter() {
            gl.texture(Some(Texture2D::from_miniquad_texture(unsafe { Texture::from_raw_id(tex_id, miniquad::TextureFormat::RGBA8) })));
            for mesh in meshes {
                gl.geometry(&mesh.0, &mesh.1);
            }
        }
    }
}

/// 额外音效映射表：键为谱面/事件中引用的音效名。
///
/// 之所以用 `HashMap` 且键为 `String`：键直接来自谱面文本，用法是「按名查找若干次」，
/// 条目数量很少且无需保持顺序。
pub type SfxMap = HashMap<String, Sfx>;

/// 整个游戏渲染/音频状态的聚合体。
///
/// 所有绘制模块只接收 `&mut Resource`，从而避免层层传递大量参数；
/// 它同时充当「当前帧状态」的载体（当前时间、模型矩阵栈、批渲染缓冲与离屏目标）。
pub struct Resource {
    /// 玩家配置（分辨率、曲速、模组、音符缩放等）
    pub config: Config,
    /// 当前谱面的元信息（曲名、谱面图片、判定线长度、宽高比等）
    pub info: ChartInfo,
    /// 逻辑宽高比，所有纵向换算都依赖它。与 `info.aspect_ratio` 的差别在于它会依据
    /// 窗口实际比例收窄（见 [`Resource::update_size`]），保证画面不被拉伸
    pub aspect_ratio: f32,
    /// 本机像素密度，取自 [`DPI_VALUE`]
    pub dpi: u32,
    /// 上一次已知的 viewport (x, y, w, h)，用于检测窗口尺寸是否发生变化
    pub last_vp: (i32, i32, i32, i32),
    /// 音符基准宽度（归一化世界单位），由 `config.note_scale * NOTE_WIDTH_RATIO_BASE` 推导
    pub note_width: f32,

    /// 当前曲目时间（秒），是全部渲染与判定的时间基准
    pub time: f64,

    /// 全局透明度倍率，由各处渲染流程按场景（如暂停淡出）统一设置
    pub alpha: f32,
    /// 判定线的默认颜色（取自资源包的 perfect 配色）
    pub judge_line_color: Color,

    /// 2D 摄像机：把归一化坐标映射到窗口，内含逻辑 viewport 与 y 轴翻转
    pub camera: Camera2D,

    /// 谱面背景图
    pub background: SafeTexture,
    /// 曲绘（用于加载/结算等界面）
    pub illustration: SafeTexture,
    /// 评级图标，顺序为 F / C / B / A / S / V / FC / φ
    pub icons: [SafeTexture; 8],
    /// 模组图标，顺序与 `Mods` 的展示顺序一致（见 [`Resource::load_mod_icons`]）
    pub mod_icons: [SafeTexture; 7],
    /// 当前生效的资源包
    pub res_pack: ResourcePack,
    /// 玩家头像
    pub player: SafeTexture,
    /// UI 图标：返回
    pub icon_back: SafeTexture,
    /// UI 图标：重试
    pub icon_retry: SafeTexture,
    /// UI 图标：继续
    pub icon_resume: SafeTexture,
    /// UI 图标：确认
    pub icon_proceed: SafeTexture,

    /// 打击特效粒子发射器
    pub emitter: ParticleEmitter,

    /// 音频管理器，负责音效播放与混音
    pub audio: AudioManager,
    /// 曲目音频（已解码）
    pub music: AudioClip,
    /// 曲目总时长（秒），用于进度条与结算
    pub track_length: f64,
    /// 点击音效
    pub sfx_click: Sfx,
    /// 拖拽音效
    pub sfx_drag: Sfx,
    /// 滑动音效
    pub sfx_flick: Sfx,

    /// 谱面/事件额外引用的音效（按名索引），见 `SfxMap`
    pub extra_sfxs: SfxMap,

    /// 离屏渲染目标：用于对谱面做 MSAA 与特效后处理。
    /// 既不需要特效、样本数又为 1 时保持 `None`，此时直接绘制到默认帧缓冲
    pub chart_target: Option<MSRenderTarget>,
    /// 是否禁用所有特效（不创建离屏目标、不发射粒子）。
    /// 来源是 `config.disable_effect || 谱面声明的 has_no_effect`
    pub no_effect: bool,

    /// 顶点批渲染缓冲，见 `NoteBuffer`
    pub note_buffer: RefCell<NoteBuffer>,

    /// 模型矩阵栈，栈底恒为单位矩阵，push/pop 必须成对，机制见 [`Resource::with_model`]
    pub model_stack: Vec<Matrix>,
}

// 批量加载若干纹理并组成数组。宏参数是路径字面量列表，展开为逐个 `await` 的加载表达式，
// 避免为每组 UI 图标手写重复代码。注意展开时对每个元素顺序 `await`，
// 因此只适合启动期的少量小图；需要并发的场景应另写加载逻辑。
macro_rules! loads {
    ($($path:literal),*) => {
        [$(loads!(@detail $path)),*]
    };

    (@detail $path:literal) => {
        Texture2D::from_image(&load_image($path).await?).into()
    };
}

// 加载、构造与运行期状态访问。
impl Resource {
    /// 加载全部评级图标。
    ///
    /// # Errors
    /// 任一图标文件缺失或解码失败时返回错误。
    pub async fn load_icons() -> Result<[SafeTexture; 8]> {
        Ok(loads![
            "rank/F.png",
            "rank/C.png",
            "rank/B.png",
            "rank/A.png",
            "rank/S.png",
            "rank/V.png",
            "rank/FC.png",
            "rank/phi.png"
        ])
    }
    /// 加载全部模组图标。
    ///
    /// 顺序必须与下一行注释列出的模组顺序、以及 UI 中的遍历顺序严格一致，
    /// 否则图标会与模组名错配。
    ///
    /// # Errors
    /// 任一图标文件缺失或解码失败时返回错误。
    pub async fn load_mod_icons() -> Result<[SafeTexture; 7]> {
        // FLIP_X, FADE_OUT, FADE_IN, NIGHTCORE, RAINBOW, AUTOPLAY, NO_SHADER
        Ok(loads![
            "mod/flip_x.png",
            "mod/fade_out.png",
            "mod/fade_in.png",
            "mod/nightcore.png",
            "mod/rainbow.png",
            "mod/autoplay.png",
            "mod/no-shader.png"
        ])
    }

    /// 构造一局游戏的完整状态。
    ///
    /// 构造顺序刻意把「资源包 → 音频 → 摄像机 → 粒子 → 尺寸相关」串成一条链：
    /// 粒子尺寸依赖资源包给出的缩放系数，音效需要音频管理器先就绪，
    /// 摄像机则依赖最终确定的宽高比。
    ///
    /// # Arguments
    /// * `fs` - 谱面所在文件系统（可能来自磁盘，也可能是内置资源）。
    /// * `player` - 玩家头像；为 `None` 时回退到内置默认图。
    /// * `has_no_effect` - 谱面自身声明的「关闭特效」，与玩家配置取或得到 [`Resource::no_effect`]。
    ///
    /// # Errors
    /// 资源包、音频或纹理加载失败，或音频设备初始化失败时返回错误。
    pub async fn new(
        config: Config,
        info: ChartInfo,
        mut fs: Box<dyn FileSystem>,
        player: Option<SafeTexture>,
        background: SafeTexture,
        illustration: SafeTexture,
        has_no_effect: bool,
    ) -> Result<Self> {
        // 与资源包加载中的同名宏同理：只是为了避免重复书写「加载 + 转纹理」的样板代码。
        macro_rules! load_tex {
            ($path:literal) => {
                SafeTexture::from(Texture2D::from_image(&load_image($path).await?))
            };
        }
        let res_pack = ResourcePack::from_path(config.res_pack_path.as_ref())
            .await
            .context("Failed to load resource pack")?;
        // 摄像机的 y 缩放取负值，等价于把世界坐标的 y 轴翻转成屏幕方向；
        // 这正是绘制代码里大量 `flip_y: true` 与 `append_nonuniform_scaling(1, -1)` 的根源。
        let camera = Camera2D {
            target: vec2(0., 0.),
            zoom: vec2(1., -config.aspect_ratio.unwrap_or(info.aspect_ratio)),
            ..Default::default()
        };

        // 音效在此才从 `AudioClip` 转成可播放的 `Sfx`：`Sfx` 依赖音频管理器，
        // 而音频管理器必须等音频设备就绪后才能创建。
        // 缓冲区大小统一取 BUFFER_SIZE，保证各打击音效的延迟一致。
        let mut audio = create_audio_manger(&config)?;
        let music = AudioClip::new(fs.load_file(&info.music).await?)?;
        let track_length = music.length();
        let buffer_size = Some(BUFFER_SIZE);
        let sfx_click = audio.create_sfx(res_pack.sfx_click.clone(), buffer_size)?;
        let sfx_drag = audio.create_sfx(res_pack.sfx_drag.clone(), buffer_size)?;
        let sfx_flick = audio.create_sfx(res_pack.sfx_flick.clone(), buffer_size)?;

        // `note_width` 是音符的**基准宽度**（归一化单位）= 玩家音符缩放 × `NOTE_WIDTH_RATIO_BASE`。
        // 之所以引入这个比例常量：让音符视觉大小与谱面的判定线长度解耦，
        // 不同作者按不同长度设计谱面时，音符大小仍能保持一致。
        // `aspect_ratio` 此处取「玩家覆盖值 > 谱面声明值」，随后还会在 update_size 中
        // 依据实际窗口比例再次收窄。
        let aspect_ratio = config.aspect_ratio.unwrap_or(info.aspect_ratio);
        let note_width = config.note_scale * NOTE_WIDTH_RATIO_BASE as f32;
        let note_scale = config.note_scale;

        let emitter = ParticleEmitter::new(&res_pack, note_scale, res_pack.info.hide_particles)?;

        // 全局无特效 = 玩家设置关闭 || 谱面要求关闭。任一为真即走无特效路径，
        // 它决定是否创建离屏渲染目标（见 update_size），也决定是否发射粒子。
        let no_effect = config.disable_effect || has_no_effect;

        // 预分配绘制调用缓冲容量：每批次最多 MAX_SIZE 个顶点（4 个一组）与
        // MAX_SIZE * 6 个索引，避免 `NoteBuffer::draw_all` 在运行时反复扩容。
        macroquad::window::gl_set_drawcall_buffer_capacity(MAX_SIZE * 4, MAX_SIZE * 6);
        Ok(Self {
            config,
            info,
            aspect_ratio,
            // DPI 从全局原子量取一次快照：它由窗口系统回调在其他线程更新，
            // 同一帧内不应变化，因此无需每次都重新读取。
            dpi: DPI_VALUE.load(std::sync::atomic::Ordering::SeqCst),
            last_vp: (0, 0, 0, 0),
            note_width,

            time: 0.,

            alpha: 1.,
            // 判定线默认颜色复用资源包的 perfect 特效色，使线与命中反馈视觉统一。
            judge_line_color: res_pack.info.fx_perfect(),

            camera,

            background,
            illustration,
            icons: Self::load_icons().await?,
            mod_icons: Self::load_mod_icons().await?,
            res_pack,
            player: if let Some(player) = player { player } else { load_tex!("player.jpg") },
            icon_back: load_tex!("back.png"),
            icon_retry: load_tex!("retry.png").with_mipmap(),
            icon_resume: load_tex!("resume.png"),
            icon_proceed: load_tex!("proceed.png").with_mipmap(),

            emitter,

            audio,
            music,
            track_length,
            sfx_click,
            sfx_drag,
            sfx_flick,
            extra_sfxs: SfxMap::new(),

            // 离屏目标延迟到 update_size 中创建，因为它的尺寸取决于实际 viewport。
            chart_target: None,
            no_effect,

            note_buffer: RefCell::new(NoteBuffer::default()),

            // 矩阵栈以单位矩阵为底，保证 `last()` 永远可用，也让 `with_model` 的
            // 「栈顶 × 新矩阵」在首次调用时退化为新矩阵本身。
            model_stack: vec![Matrix::identity()],
        })
    }

    /// 把一段音频转成可播放的音效（用于谱面额外引用的音效）。
    ///
    /// # Errors
    /// 音频管理器创建音效失败时返回错误。
    pub fn create_sfx(&mut self, clip: AudioClip) -> Result<Sfx> {
        self.audio.create_sfx(clip, Some(BUFFER_SIZE))
    }

    /// 在当前模型矩阵栈顶所表示的「原点」处发射一次打击粒子。
    ///
    /// 这是所有打击特效的统一入口：调用方只需先用 `with_model` 把栈顶设到目标位置，
    /// 无需自行做坐标换算。内部处理为：
    /// 1. 玩家关闭粒子时直接返回，省掉后续的全部换算；
    /// 2. 用 `world_to_screen` 把原点（即栈顶矩阵的位移部分）转成屏幕坐标，
    ///    因此必须处于正确的 `with_model` 作用域内；
    /// 3. `flip_x` 时水平镜像（配合镜像模组），y 取负是为了抵消摄像机的 y 轴翻转，
    ///    因为粒子系统工作在屏幕像素坐标系；
    /// 4. 只有在资源包声明 `hit_fx_rotate` 时才把角度转成弧度，否则粒子恒为 0°。
    ///
    /// # Arguments
    /// * `rotation` - 期望的粒子朝向（度），是否生效取决于资源包配置。
    /// * `color` - 粒子颜色（按判定结果或谱面自定义特效色决定）。
    pub fn emit_at_origin(&mut self, rotation: f32, color: Color) {
        if !self.config.particle {
            return;
        }
        let pt = self.world_to_screen(Point::default());
        self.emitter.emit_at(
            vec2(if self.config.flip_x() { -pt.x } else { pt.x }, -pt.y),
            if self.res_pack.info.hit_fx_rotate { rotation.to_radians() } else { 0. },
            color,
        );
    }

    /// 更新与窗口尺寸相关的状态。
    ///
    /// 由主循环在每帧开始时调用。尺寸未变时立即返回 `false`，避免重建昂贵的离屏目标。
    /// 尺寸变化时会做两件事：
    /// 1. 视需要重建 [`Resource::chart_target`]（离屏 MSAA / 特效目标）；
    /// 2. 重新计算逻辑 viewport，使任何窗口比例下画面都不被拉伸。
    ///
    /// # Arguments
    /// * `vp` - (x, y, width, height)，来自 `get_viewport()`。
    ///
    /// # Returns
    /// 尺寸是否发生变化，调用方据此决定是否重建其他与尺寸相关的资源。
    pub fn update_size(&mut self, vp: (i32, i32, i32, i32)) -> bool {
        if self.last_vp == vp {
            return false;
        }
        self.last_vp = vp;
        // 需要离屏目标的两种情况：开启特效（`no_effect` 为假），或需要 MSAA
        // （`sample_count != 1`）。用「或」而非「与」是因为 MSAA 必须借助离屏 FBO
        // 才能实现，即便特效全关也应生效。
        if !self.no_effect || self.config.sample_count != 1 {
            self.chart_target = Some(MSRenderTarget::new((vp.2 as u32, vp.3 as u32), self.config.sample_count));
        }
        // 计算居中且保持宽高比的逻辑 viewport（即 letterbox / pillarbox）：
        // 窗口比谱面更宽就左右留黑边，更窄就上下留黑边。
        fn viewport(aspect_ratio: f32, (x, y, w, h): (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
            let w = w as f32;
            let h = h as f32;
            let (rw, rh) = {
                let ew = h * aspect_ratio;
                if ew > w {
                    let eh = w / aspect_ratio;
                    (w, eh)
                } else {
                    (ew, h)
                }
            };
            (x + ((w - rw) / 2.).round() as i32, y + ((h - rh) / 2.).round() as i32, rw as i32, rh as i32)
        }
        let aspect_ratio = self.config.aspect_ratio.unwrap_or(self.info.aspect_ratio);
        // `force_aspect_ratio` 由谱面声明：为真时严格采用谱面比例（窗口比例不符时会裁切），
        // 否则取「谱面比例」与「窗口比例」中的较小值——宁可显示更多纵向内容，
        // 也不裁掉判定区（判定坐标正是按 aspect_ratio 归一化的）。
        // `zoom.y` 取负值实现 y 轴翻转，与 `new` 中的相机设置保持一致；
        // 之所以只在 else 分支改 zoom，是因为强制比例时缩放保持不变。
        if self.info.force_aspect_ratio {
            self.aspect_ratio = aspect_ratio;
            self.camera.viewport = Some(viewport(aspect_ratio, vp));
        } else {
            self.aspect_ratio = aspect_ratio.min(vp.2 as f32 / vp.3 as f32);
            self.camera.zoom.y = -self.aspect_ratio;
            self.camera.viewport = Some(viewport(self.aspect_ratio, vp));
        };
        true
    }

    /// 世界（归一化）坐标 → 屏幕坐标变换。
    ///
    /// 使用的正是模型矩阵栈顶：栈顶已累积了从摄像机到当前绘制对象的全部变换，
    /// 因此调用方只需保证自己处在正确的 `with_model` 作用域内即可。
    ///
    /// # Panics
    /// 理论上不会发生：`model_stack` 始终至少包含单位矩阵（见 [`Resource::new`]，
    /// 且 `with_model` 的 push/pop 成对出现）。
    pub fn world_to_screen(&self, pt: Point) -> Point {
        self.model_stack.last().unwrap().transform_point(&pt)
    }

    /// 屏幕坐标 → 世界（归一化）坐标变换，即 [`Resource::world_to_screen`] 的逆。
    ///
    /// 判定线渲染时用它把屏幕四角反算回世界高度，从而得到真正需要绘制的纵向范围；
    /// 这样即便当前模型矩阵包含旋转/缩放，裁剪界依然正确。
    ///
    /// # Panics
    /// 若栈顶矩阵不可逆会 panic。理论上不会发生：栈中的变换由旋转、平移与非零缩放组成。
    pub fn screen_to_world(&self, pt: Point) -> Point {
        self.model_stack.last().unwrap().try_inverse().unwrap().transform_point(&pt)
    }

    /// 在现有变换之外再叠加一层局部变换，并在这层变换下执行 `f`。
    ///
    /// 新矩阵是「栈顶 × 传入矩阵」（右乘）：传入的 `model` 被视为**局部坐标系**中的变换，
    /// 即先应用局部、再应用父级，与常见的 TRS 组合约定一致。
    ///
    /// # 不变量
    /// 内部严格成对 push/pop，因此即便 `f` 提前 `return` 也不会泄漏栈深度
    /// ——用闭包而不是裸作用域正是为了让这种还原不依赖调用方自觉。
    /// 注意：`f` 内不应 panic，否则 `pop` 会被跳过，栈将永久变深。
    #[inline]
    pub fn with_model(&mut self, model: Matrix, f: impl FnOnce(&mut Self)) {
        let model = self.model_stack.last().unwrap() * model;
        self.model_stack.push(model);
        f(self);
        self.model_stack.pop();
    }

    /// 把当前栈顶矩阵推入 GL 状态，并在其下执行 `f`。
    ///
    /// 与 [`Resource::with_model`] 的分工：后者只更新 CPU 侧的矩阵栈（供 `world_to_screen`
    /// 等计算使用），本方法才把矩阵交给 GPU（供 macroquad 的高层绘制函数使用）。
    /// 两者常成对出现：先 `with_model` 定位，再 `apply_model` 落笔。
    #[inline]
    pub fn apply_model(&mut self, f: impl FnOnce(&mut Self)) {
        self.apply_model_of(&self.model_stack.last().unwrap().clone(), f);
    }

    /// 把指定矩阵压入 macroquad 的 GL 模型矩阵栈，并在其下执行 `f`。
    ///
    /// 之所以接收 `&Matrix`（引用）而不是按值传入，是为了让调用方在借用栈顶的同时
    /// 也能调用本方法（例如 `apply_model` 直接把栈顶引用传进来）。
    ///
    /// # Safety
    /// 本方法通过 `get_internal_gl()` 绕过 macroquad 的安全封装直接取得图形后端上下文，
    /// 其前提是当前正处于已初始化的渲染帧内。本项目只在绘制路径调用本方法，满足该前提。
    /// push 与 pop 在方法内部成对出现，保证 GL 矩阵栈不会泄漏。
    #[inline]
    pub fn apply_model_of(&mut self, mat: &Matrix, f: impl FnOnce(&mut Self)) {
        unsafe { get_internal_gl() }.quad_gl.push_model_matrix(nalgebra_to_glm(mat));
        f(self);
        unsafe { get_internal_gl() }.quad_gl.pop_model_matrix();
    }
}
