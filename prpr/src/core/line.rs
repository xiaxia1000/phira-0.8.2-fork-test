//! 判定线（`JudgeLine`）的更新与绘制实现。
//!
//! 本模块承载 Phira 谱面中「判定线」这一核心概念：它同时是音符的容器、
//! 坐标变换的宿主以及视觉元素（普通线 / 贴图 / GIF / 文本 / 画布）的载体。
//! 需要重点理解两个设计：
//! 1. 音符的纵向位置不以「时间」为准，而是以 `height`（对速度积分得到的绝对 y）定位，
//!    这样才能保证谱面速度变化时，已出现音符的高度不会被后续速度段反向影响；
//! 2. 所有绘制都发生在归一化世界坐标中，屏幕裁剪、缩放与偏移统一由 `Resource`
//!    的模型矩阵栈承担，本模块因此不需要关心具体分辨率。

use super::{chart::ChartSettings, object::CtrlObject, Anim, AnimFloat, BpmList, Matrix, Note, Object, Point, RenderConfig, Resource, Vector};
use crate::modify_base::static_color_gradient::{StaticColorGradient, StaticColorGradientType};
use crate::{
    ext::{get_viewport, NotNanExt, SafeTexture},
    judge::JudgeStatus,
    ui::Ui,
};
use macroquad::prelude::*;
use miniquad::{RenderPass, Texture, TextureParams, TextureWrap};
use nalgebra::Rotation2;
use serde::Deserialize;
use std::cell::RefCell;
use crate::config::Mods;

/// 可绑定到判定线上的 HUD 元素类型。
///
/// 谱面可以为某条判定线指定一个 `attach_ui` 元素，使其随该线的变换一起运动
/// （见 [`crate::core::chart::Chart::with_element`]）。判别值与谱面格式中
/// 对应的整数字段一一对应，因此必须保持稳定，不能为「整齐」而重排。
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum UIElement {
    /// 1：暂停按钮
    Pause = 1,
    /// 2：连击数（COMBO 的数字本身）
    ComboNumber = 2,
    /// 3：连击文字标签
    Combo = 3,
    /// 4：分数
    Score = 4,
    /// 5：进度条
    Bar = 5,
    /// 6：曲名
    Name = 6,
    /// 7：难度等级
    Level = 7,
}

// 由原始判别值还原枚举，用于把谱面数据（存为 u8）恢复成绑定关系。
impl UIElement {
    /// 将谱面中保存的原始编号还原为 `UIElement`。
    ///
    /// 之所以不用 `TryFrom<u8>` 或 `transmute`：非法编号（例如来自更新版本的谱面）
    /// 应当被安静地判为 `None` 并忽略绑定，而不是触发 panic 中断游戏。
    ///
    /// # Returns
    /// 编号在 1..=7 时返回对应变体，否则返回 `None`。
    pub fn from_u8(val: u8) -> Option<Self> {
        Some(match val {
            1 => Self::Pause,
            2 => Self::ComboNumber,
            3 => Self::Combo,
            4 => Self::Score,
            5 => Self::Bar,
            6 => Self::Name,
            7 => Self::Level,
            _ => return None,
        })
    }
}

/// 逐帧动画数据集：把一串「持续时长 + 纹理」按顺序播放。
///
/// 与 `Anim<f32>` 的插值不同，这里在离散帧之间跳变。之所以要缓存 `total_time`：
/// 取帧是每帧都可能发生的操作，预先求和后可以用取模 O(1) 定位，避免每次都 O(n) 累加。
pub struct GifFrames {
    /// time of each frame in milliseconds
    /// 每帧的持续毫秒数与纹理，按播放顺序排列
    frames: Vec<(u128, SafeTexture)>,
    /// milliseconds
    /// 所有帧时长之和（毫秒），用于对播放时间取模以实现循环
    total_time: u128,
}

// GIF 线的帧调度：构造期一次性求和，播放期按绝对时间或进度定位。
impl GifFrames {
    /// 用「时长 + 纹理」序列构造动画，并预计算总时长。
    pub fn new(frames: Vec<(u128, SafeTexture)>) -> Self {
        let total_time = frames.iter().map(|(time, _)| *time).sum();
        Self { frames, total_time }
    }

    /// 按绝对时间取帧（循环播放）。
    ///
    /// 语义是「动画开始以来经过的时间」，先对总时长取模以支持无限循环，
    /// 再逐帧扣减定位。兜底返回最后一帧，避免取模误差导致越界。
    ///
    /// # Panics
    /// `frames` 为空时会 panic（构造时至少应有一帧）。
    pub fn get_time_frame(&self, time: u128) -> &SafeTexture {
        let mut time = time % self.total_time;
        for (t, frame) in &self.frames {
            if time < *t {
                return frame;
            }
            time -= t;
        }
        &self.frames.last().unwrap().1
    }

    /// 按进度（0..1）取帧。
    ///
    /// 与 [`GifFrames::get_time_frame`] 的差别在于参数语义是「归一化进度」而非绝对时间，
    /// 因此先乘总时长再复用同一套查表逻辑；这正好匹配判定线 `TextureGif` 中那个
    /// 被谱面事件归一化到 0..1 的 `Anim<f32>`。
    pub fn get_prog_frame(&self, prog: f32) -> &SafeTexture {
        let time = (prog * self.total_time as f32) as u128;
        self.get_time_frame(time)
    }

    /// 返回动画一轮的总时长（毫秒）。
    pub fn total_time(&self) -> u128 {
        self.total_time
    }
}

/// 判定线的外观种类。
///
/// 之所以做成「带数据的枚举」而不是若干布尔开关：不同外观需要完全不同的绘制路径
/// 与副作用（贴图 / 取帧 / 文本布局 / 离屏渲染）。用枚举可让 `render` 在编译期穷尽
/// 所有分支，也杜绝了「两个开关同时为真」这类非法状态。
#[derive(Default)]
pub enum JudgeLineKind {
    /// 默认外观：一条纯色线段，颜色取 `JudgeLine::color`，缺省回退到全局判定线颜色
    #[default]
    Normal,
    /// 静态贴图线。附带的 `String` 是贴图路径，仅用于调试与日志定位。
    Texture(SafeTexture, String),
    /// GIF 贴图线：`Anim<f32>` 给出 0..1 进度，`GifFrames` 负责按进度选帧
    TextureGif(Anim<f32>, GifFrames, String),
    /// 文本线：直接把 `Anim<String>` 的当前值绘制为居中文本
    Text(Anim<String>),
    /// 画布线：内容不来自纹理，而是先在自建的离屏 FBO 中绘制，再整体贴回屏幕。
    ///
    /// 第一个参数是本帧要绘制的圆半径；第二个参数是惰性创建的离屏资源：
    /// `RenderPass` 必须等到窗口尺寸确定后才能创建（需要 viewport 大小），
    /// `bool` 记录上一帧是否真的画过内容——只有画过才需要贴回或清屏，
    /// 从而省掉空闲帧的无谓绘制。
    /// 用 `RefCell` 是因为 `render` 只拿到 `&self`，而 FBO 又必须惰性初始化。
    Paint(Anim<f32>, RefCell<(Option<RenderPass>, bool)>),
}

/// 判定线的音符索引缓存：把每帧都要用到的排序与分组结果固化下来。
///
/// 音符集合在一首曲子里是静态的，但排序与分组会随判定状态变化（被判定掉的音符
/// 需要前移分组起点）。因此这里保存的是**索引**而非音符本身，让 `update`/`render`
/// 能以「分组数」而非「音符数」为代价跳过已完成的部分。
/// 之所以可以 `Clone`：它只含纯索引数据，复制远比重新排序便宜。
#[derive(Clone)]
pub struct JudgeLineCache {
    /// 音符的更新顺序（即排序后的索引序列）
    update_order: Vec<u32>,
    /// 前 `not_plain_count` 个音符是非 plain 的（假音符 / Hold / 带纵向位移关键帧），
    /// 它们不参与按速度分组的快速裁剪，需要单独遍历
    not_plain_count: usize,
    /// `above == true` 的音符分组起始索引，每组内部速度相同
    above_indices: Vec<usize>,
    /// `above == false` 的音符分组起始索引，每组内部速度相同
    below_indices: Vec<usize>,
}

// 构建按绘制先后关系排好序的索引缓存。
// 注意 `new` 会就地重排传入的音符切片，调用方需接受音符顺序被改变。
impl JudgeLineCache {
    /// 对音符排序并生成索引缓存。
    ///
    /// 排序键 `(plain(), !above, speed, (height + translation.y) * speed)` 的用意：
    /// - `plain()` 为真者排在后段：非 plain 音符行为特殊，集中放在前面以便用
    ///   `not_plain_count` 一次性切出；
    /// - `!above` 让线上音符在前、线下音符在后；
    /// - 再按 `speed` 分组，使同一速度段的音符连续，从而能「一越界就 break」提前收工；
    /// - 最后一维 `(height + translation.y) * speed` 是**音符在屏幕上纵向位置的单调等价量**，
    ///   把它纳入排序键，是为了让屏幕同一位置重叠的音符按正确的前后关系绘制
    ///   （后画的覆盖先画的），从而避免相邻音符因速度/位移不同而在视觉上穿插。
    ///
    /// `not_nan()` 用来把可能出现的 NaN 键规约掉，保证 `sort_by_key` 需要的全序性。
    pub fn new(notes: &mut [Note]) -> Self {
        notes
            .sort_by_key(|it| (it.plain(), !it.above, it.speed.not_nan(), ((it.height + it.object.translation.1.now() as f64) * it.speed).not_nan()));
        let mut res = Self {
            update_order: Vec::new(),
            not_plain_count: 0,
            above_indices: Vec::new(),
            below_indices: Vec::new(),
        };
        res.reset(notes);
        res
    }

    /// 依据当前 `notes` 的顺序重建索引分组（不重新排序）。
    ///
    /// 当音符顺序因内容变化而改变时（例如控制对象位移导致排序键变化）会被调用；
    /// 此时旧索引会指向错误的音符，必须整体重建。实现是一次线性扫描：
    /// 先定位第一个 plain 音符得到 `not_plain_count`，再按 `(above, speed)` 的连续段
    /// 记录每段起点，供 `render` 逐段裁剪。
    pub(crate) fn reset(&mut self, notes: &mut [Note]) {
        self.update_order = (0..notes.len() as u32).collect();
        self.above_indices.clear();
        self.below_indices.clear();
        let mut index = notes.iter().position(|it| it.plain()).unwrap_or(notes.len());
        self.not_plain_count = index;
        while notes.get(index).is_some_and(|it| it.above) {
            self.above_indices.push(index);
            let speed = notes[index].speed;
            loop {
                index += 1;
                if !notes.get(index).is_some_and(|it| it.above && it.speed == speed) {
                    break;
                }
            }
        }
        while index != notes.len() {
            self.below_indices.push(index);
            let speed = notes[index].speed;
            loop {
                index += 1;
                if !notes.get(index).is_some_and(|it| it.speed == speed) {
                    break;
                }
            }
        }
    }
}

/// 一条判定线。
///
/// 判定线是谱面结构的主干：它既决定音符的坐标变换（父级的旋转/位移会层层传递），
/// 也是绘制批次与裁剪范围的划分单位。
pub struct JudgeLine {
    /// 判定线自身的基础变换（位置 / 旋转 / 缩放 / 透明度）与关键帧动画
    pub object: Object,
    /// 运行期控制对象，承载谱面事件对高度、位置、大小等的临时覆盖。
    /// 用 `RefCell` 是因为更新与渲染都需要读写它，而 `render` 只拿到 `&self`。
    pub ctrl_obj: RefCell<CtrlObject>,
    /// 外观类型，决定 `render` 走哪条绘制分支
    pub kind: JudgeLineKind,
    /// Height Animation, decribes the `height` of the line at a specific time
    ///
    /// The `height` here can be considered as the absolute 'y' coordinate of the notes attached to this line, which is calculated by
    /// ∫ v(t) dt, where v(t) is the speed of the line at time t.
    /// 判定线高度动画。它的物理含义是「挂在本线上的音符的绝对 y 坐标」，
    /// 由线速 v(t) 对时间积分得到（∫v(t)dt），而不是由谱面直接给定。
    ///
    /// 之所以要积分成「高度」而不保留「速度」，是因为位置必须只依赖一个标量：
    /// 若直接用速度描述位置，任何时刻的速度变化都会同时改变此前所有音符的落点；
    /// 改为积分后的高度后，速度变化只影响其后的新增位移，音符一旦确定高度即与
    /// 后续速度段无关，谱面手感才可预期。
    pub height: AnimFloat,
    /// 判定线倾斜量（角度）。倾斜会让线上音符按行产生横向缩放（见
    /// [`Note::now_transform`] 中的 `incline_val`），形成整排音符被「斜着看」的透视感。
    pub incline: AnimFloat,
    /// 本线拥有的音符；顺序由 `cache` 维护（`JudgeLineCache::new`/`reset` 会重排）
    pub notes: Vec<Note>,
    /// 判定线自身的颜色；为 `None` 时回退到全局 `judge_line_color` 或白色
    pub color: Anim<Color>,
    /// 父判定线在谱面判定线数组中的下标，仅用于坐标变换的继承
    pub parent: Option<usize>,
    /// 是否连同父级旋转一起继承：为假时只继承父级位移，不继承旋转
    pub rot_with_parent: bool,
    /// 绘制层级，数值越大越先绘制（越靠底层），用于控制线与线之间的遮挡
    pub z_index: i32,
    /// Whether to show notes below the line, here below is defined in the time axis, which means the note should already be judged
    ///
    /// 是否绘制「线下方」的音符。此处的「下方」是时间轴意义上的（已越过判定点），
    /// 而非几何意义上的下侧。
    ///
    /// TODO: Not sure
    pub show_below: bool,
    /// 绑定到本线上的 HUD 元素：非 `None` 时该元素随判定线变换一起运动，
    /// 由 `Chart::with_element` 在绘制 HUD 时使用。
    pub attach_ui: Option<UIElement>,

    /// 音符索引缓存，见 [`JudgeLineCache`]
    pub cache: JudgeLineCache,
}

// 判定线的运行期更新与绘制。`update` 与 `render` 分属两个阶段：
// 前者推进所有动画与音符状态，后者只把当前状态画出来，互不修改对方关心的数据。
impl JudgeLine {
    /// 推进本线及其所有音符的时间。
    ///
    /// # Arguments
    /// * `res` - 全局渲染/音频状态，提供当前时间与资源。
    /// * `tr` - 本线在世界中的总变换矩阵（由 `Chart` 预先算好，已含父级影响）；
    ///   之所以由 `Chart` 传入，是因为音符粒子必须知道线的世界位置。
    /// * `parent_rot` - 父级累计旋转角（度），用于给音符粒子定向。
    pub fn update(&mut self, res: &mut Resource, tr: Matrix, parent_rot: f32) {
        // self.object.set_time(res.time); // this is done by chart, chart has to calculate transform for us
        // 本线自身的 object 时间由 Chart 统一推进（因为 Chart 要先算好变换矩阵），
        // 这里只负责推进高度动画。
        self.height.set_time(res.time);
        let line_height = self.height.now();
        let mut ctrl_obj = self.ctrl_obj.borrow_mut();
        // 阶段一：按缓存顺序推进所有音符，并顺手剔除已经死亡的音符。
        // 把「更新」与「死亡判定」内联在同一个 retain 里，避免再遍历一次；
        // `ctrl_obj` 需要可变借用，而 retain 闭包内还要访问 self，故先取出再
        // 在循环结束后立即 drop，防止与其他借用冲突。
        self.cache.update_order.retain(|id| {
            let note = &mut self.notes[*id as usize];
            note.update(res, parent_rot, &tr, &mut ctrl_obj, line_height as f64);
            !note.dead()
        });
        drop(ctrl_obj);
        // 阶段二：推进与外观绑定的动画。只有会随时间变化的三种外观需要，
        // Normal 与 Texture 是静态的，无需任何推进。
        match &mut self.kind {
            JudgeLineKind::Text(anim) => {
                anim.set_time(res.time);
            }
            JudgeLineKind::Paint(anim, ..) => {
                anim.set_time(res.time);
            }
            JudgeLineKind::TextureGif(anim, ..) => {
                anim.set_time(res.time);
            }
            _ => {}
        }
        self.color.set_time(res.time);
        // 阶段三：回收已被判定的音符分组。
        // 每个分组起点指向该速度段中「最靠后但仍未判定」的音符；一旦它被判定，
        // 就把起点后移到同速度的下一个音符，若没有则删除整组。
        // 这样 `render` 就无需从数组头部逐个跳过大量已判定音符，是长 Hold/密集谱面的关键优化。
        self.cache.above_indices.retain_mut(|index| {
            while matches!(self.notes[*index].judge, JudgeStatus::Judged) {
                if self
                    .notes
                    .get(*index + 1)
                    .is_some_and(|it| it.above && it.speed == self.notes[*index].speed)
                {
                    *index += 1;
                } else {
                    return false;
                }
            }
            true
        });
        self.cache.below_indices.retain_mut(|index| {
            while matches!(self.notes[*index].judge, JudgeStatus::Judged) {
                if self.notes.get(*index + 1).is_some_and(|it| it.speed == self.notes[*index].speed) {
                    *index += 1;
                } else {
                    return false;
                }
            }
            true
        });
    }

    /// 递归计算本线在世界中的旋转角（度）。
    ///
    /// 当 `rot_with_parent` 为真时，结果是「自身旋转 + 父级累计旋转」；之所以递归调用
    /// `lines[parent].fetch_rot` 而不缓存，是因为父级旋转本身也可能继承自它自己的父级。
    /// 注意：若谱面数据形成父子环，此递归会无限深入直至栈溢出——这里不做环检测，
    /// 依赖谱面解析阶段保证父指针无环。
    pub fn fetch_rot(&self, lines: &[JudgeLine]) -> f32 {
        let mut rot = self.object.rotation.now();
        if self.rot_with_parent {
            if let Some(parent) = self.parent {
                rot += lines[parent].fetch_rot(lines);
            }
        }
        rot
    }

    /// 递归计算本线在世界中的位置。
    ///
    /// 语义是「父级的世界位移 + 父级旋转作用后的本地位移」：子线挂在父级的局部坐标系中，
    /// 因此父级旋转时子线的偏移方向也随之旋转。这里只依赖父级旋转而不依赖父级缩放，
    /// 与 [`JudgeLine::now_transform`] 不含缩放的设计保持一致。
    /// 与 `fetch_rot` 一样，父指针成环会导致无限递归。
    pub fn fetch_pos(&self, res: &Resource, lines: &[JudgeLine]) -> Vector {
        if let Some(parent) = self.parent {
            let parent = &lines[parent];
            let parent_translation = parent.fetch_pos(res, lines);
            return parent_translation + Rotation2::new(parent.fetch_rot(lines).to_radians()) * self.object.now_translation(res);
        }
        self.object.now_translation(res)
    }

    /// 本线当前的世界变换矩阵（旋转 + 平移组成的齐次矩阵）。
    ///
    /// 刻意**不含缩放**：缩放由 `object.now_scale` 在 `render` 中单独应用，
    /// 目的是让线体/贴图的尺寸缩放与坐标变换解耦——否则缩放会连带改变子线与
    /// 音符的定位，违背谱面作者的预期。
    pub fn now_transform(&self, res: &Resource, lines: &[JudgeLine]) -> Matrix {
        Rotation2::new(self.fetch_rot(lines).to_radians())
            .to_homogeneous()
            .append_translation(&self.fetch_pos(res, lines))
    }

    /// 绘制本判定线及其全部音符。
    ///
    /// 整体流程：`now_transform`（世界变换）→ `now_scale`（外观缩放）→ 按 `kind`
    /// 分支绘制线体 → Paint 画布贴回 → 计算屏幕裁剪范围 → 分「线上 / 线下」两轮绘制音符。
    ///
    /// # Arguments
    /// * `ui` - 文本绘制与 HUD 的入口（Text 线、调试编号都经它绘制）。
    /// * `lines` - 谱面全部判定线，用于解析父级链。
    /// * `bpm_list` - 供音符把「提前出现拍数」换算为时间。
    /// * `settings` - 谱面设置（Hold 半覆盖等）。
    /// * `id` - 本线下标，仅在 `chart_debug` 时作为调试文本显示。
    pub fn render(&self, ui: &mut Ui, res: &mut Resource, lines: &[JudgeLine], bpm_list: &mut BpmList, settings: &ChartSettings, id: usize) {
        // 全局透明度 = 线自身 alpha × 全局 alpha；两者都可能为 `None`（未设关键帧）时取 1。
        // 注意 alpha 允许为负：Phira 把负 alpha 复用为「特效扩展」通道，见下方
        // pe_alpha_extension 分支。
        // `line_scaled` 判断纵向缩放是否明显偏离 1，用于给普通线换用更细的线宽。
        let alpha = self.object.alpha.now_opt().unwrap_or(1.0) * res.alpha;
        let color = self.color.now_opt();
        let line_scaled = (self.object.scale.1.now() - 1.).abs() > 1e-4;
        res.with_model(self.now_transform(res, lines), |res| {
            // 调试模式：在线的原点处显示本线编号，便于对着谱面文件排查问题。
            if res.config.chart_debug {
                res.apply_model(|_| {
                    ui.text(id.to_string()).pos(0., -0.01).anchor(0.5, 1.).size(0.8).draw();
                });
            }
            // 阶段一：叠加缩放后绘制线体本身。
            // 再套一层 `with_model` 是为了让缩放只作用于线体，不影响后续音符
            // （音符有自己的缩放与完整变换）。
            res.with_model(self.object.now_scale(Vector::default()), |res| {
                res.apply_model(|res| match &self.kind {
                    // 普通线：以判定线长度为半宽、沿 ±x 方向绘制的水平线段。
                    // 线宽按是否被纵向放大分支：放大后用更细的固定线宽（0.0076），
                    // 否则用 0.01，避免放大时线体显得过粗。
                    JudgeLineKind::Normal => {
                        // =========================================================================
                        static J_LINE_COLOR: StaticColorGradient =
                            StaticColorGradient::new(StaticColorGradientType::RGBTurning { s: 0.001, low: 0.3, high: 0.7 });

                        let mut color = color.unwrap_or(
                            if res.config.mods.contains(Mods::COLORFUL_JUDGELINE) {
                                J_LINE_COLOR.next_color()
                            } else { res.judge_line_color }
                        );
                        // =========================================================================
                        color.a *= alpha.max(0.0);
                        let len = res.info.line_length;
                        draw_line(-len, 0., len, 0., if line_scaled { 0.0076 } else { 0.01 }, color);
                    }
                    // 贴图线：以纹理原始像素尺寸为准居中绘制。
                    // `flip_y: true` 是因为摄像机把世界 y 轴翻转成了屏幕方向，
                    // 不翻转贴图会上下颠倒。alpha 为 0 时直接返回，省掉一次纹理提交。
                    JudgeLineKind::Texture(texture, _) => {
                        let mut color = color.unwrap_or(WHITE);
                        color.a = alpha.max(0.0);
                        if color.a == 0.0 {
                            return;
                        }
                        let hf = vec2(texture.width(), texture.height());
                        draw_texture_ex(
                            **texture,
                            -hf.x / 2.,
                            -hf.y / 2.,
                            color,
                            DrawTextureParams {
                                dest_size: Some(hf),
                                flip_y: true,
                                ..Default::default()
                            },
                        );
                    }
                    // GIF 线：先按动画进度取帧，再走与静态贴图完全相同的绘制路径。
                    // 采样方式与 `Texture` 一致（包含 `flip_y` 修正）。
                    JudgeLineKind::TextureGif(anim, frames, _) => {
                        let t = anim.now_opt().unwrap_or(0.0);
                        let frame = frames.get_prog_frame(t);
                        let mut color = color.unwrap_or(WHITE);
                        color.a = alpha.max(0.0);
                        let hf = vec2(frame.width(), frame.height());
                        draw_texture_ex(
                            **frame,
                            -hf.x / 2.,
                            -hf.y / 2.,
                            color,
                            DrawTextureParams {
                                dest_size: Some(hf),
                                flip_y: true,
                                ..Default::default()
                            },
                        );
                    }
                    // 文本线：文本由 `Ui` 绘制，其内部按屏幕方向使用 y 轴，
                    // 因此额外套一层 `append_nonuniform_scaling(1, -1)` 抵消世界的 y 翻转，
                    // 否则文字会上下镜像。这里用 `apply_model_of` 是因为要压入一个
                    // 与栈顶不同的临时矩阵。
                    JudgeLineKind::Text(anim) => {
                        let mut color = color.unwrap_or(WHITE);
                        color.a = alpha.max(0.0);
                        let now = anim.now();
                        res.apply_model_of(&Matrix::identity().append_nonuniform_scaling(&Vector::new(1., -1.)), |_| {
                            ui.text(&now).pos(0., 0.).anchor(0.5, 0.5).size(1.).color(color).multiline().draw();
                        });
                    }
                    // 画布线：不直接画线，而是先在自建的离屏 FBO 里画一个圆（半径取自 anim），
                    // 稍后再把整张 FBO 贴回屏幕（见本函数后半段的 Paint 分支）。
                    // 之所以要独立的 render_pass：圆需要画在一张干净纹理上才能整片贴回，
                    // 若直接画在屏幕上就无法参与后续坐标变换。
                    // alpha 乘 2.55 是历史遗留的缩放系数，把 0..1 的 alpha 拉到更直觉的
                    // 0..255 量级；改动它会破坏旧谱面的观感。
                    JudgeLineKind::Paint(anim, state) => {
                        let mut color = color.unwrap_or(WHITE);
                        color.a = alpha.max(0.0) * 2.55;
                        // SAFETY: 这里取底层 GL 上下文是为了自建纹理与切换 render_pass，
                        // 前提是当前正处于 macroquad 已初始化的渲染帧内（本方法只在绘制路径调用）。
                        let mut gl = unsafe { get_internal_gl() };
                        let mut guard = state.borrow_mut();
                        let vp = get_viewport();
                        // FBO 与 RenderPass 惰性创建且只创建一次：尺寸依赖首帧的 viewport，
                        // 且每帧重建会泄漏 GPU 资源。`get_or_insert_with` 让「首次调用」与
                        // 「后续复用」共用同一段代码。
                        let pass = *guard.0.get_or_insert_with(|| {
                            let ctx = &mut gl.quad_context;
                            let tex = Texture::new_render_texture(
                                ctx,
                                TextureParams {
                                    width: vp.2 as _,
                                    height: vp.3 as _,
                                    format: miniquad::TextureFormat::RGBA8,
                                    filter: FilterMode::Linear,
                                    wrap: TextureWrap::Clamp,
                                },
                            );
                            RenderPass::new(ctx, tex, None)
                        });
                        // 必须先把已排队的绘制命令冲刷掉，否则它们会被错误地画进这张 FBO。
                        gl.flush();
                        let old_pass = gl.quad_gl.get_active_render_pass();
                        gl.quad_gl.render_pass(Some(pass));
                        // viewport 置 None：让离屏绘制使用 FBO 的完整尺寸，
                        // 不受逻辑 viewport（letterbox 后的可见区域）限制。
                        gl.quad_gl.viewport(None);
                        let size = anim.now();
                        if size <= 0. {
                            // 半径非正表示本帧无内容：只有上一帧画过才需要清屏，
                            // 避免每帧都白清一次 FBO。
                            if guard.1 {
                                clear_background(Color::default());
                                guard.1 = false;
                            }
                        } else {
                            // 半径按 viewport 宽度归一化（再乘 2 换算为 NDC 尺度），
                            // 使圆的大小在不同分辨率下视觉一致。
                            ui.fill_circle(0., 0., size / vp.2 as f32 * 2., color);
                            guard.1 = true;
                        }
                        // 画完同样要 flush，再恢复原来的 render_pass 与 viewport，
                        // 否则后续绘制会继续落进 FBO。
                        gl.flush();
                        gl.quad_gl.render_pass(old_pass);
                        gl.quad_gl.viewport(Some(vp));
                    }
                })
            });
            // 阶段二：把 Paint 线上一帧画好的离屏纹理贴回屏幕。
            // 只有在 `guard.1`（确实画过内容）时才贴，避免出现整屏黑块。
            // 纵向半高取 `1 / aspect_ratio`，x 方向铺满 -1..1，从而与屏幕保持 1:1 像素比例。
            if let JudgeLineKind::Paint(_, state) = &self.kind {
                let guard = state.borrow_mut();
                if guard.1 {
                    let ctx = unsafe { get_internal_gl() }.quad_context;
                    let tex = guard.0.as_ref().unwrap().texture(ctx);
                    let top = 1. / res.aspect_ratio;
                    draw_texture_ex(
                        Texture2D::from_miniquad_texture(tex),
                        -1.,
                        -top,
                        WHITE,
                        DrawTextureParams {
                            dest_size: Some(vec2(2., top * 2.)),
                            ..Default::default()
                        },
                    );
                }
            }
            // 阶段三：构造音符绘制配置。
            // `appear_before` 默认取正无穷，表示不限制音符提前出现的时间；
            // `line_height` 传入当前高度，音符据此换算相对判定线的位移；
            // `incline_sin` 预先把倾角转成正弦，避免对每个音符重复算三角函数。
            let mut config = RenderConfig {
                settings,
                ctrl_obj: &mut self.ctrl_obj.borrow_mut(),
                line_height: self.height.now() as f64,
                appear_before: f64::INFINITY,
                draw_below: self.show_below,
                incline_sin: self.incline.now_opt().map(|it| it.to_radians().sin()).unwrap_or_default(),
            };
            // alpha 为负是 Phira 谱面约定的「特效扩展」通道（pe_alpha_extension）：
            // 取整数部分 w 表示不同的隐藏/预现策略；未在设置中开启该特性时直接不绘制。
            //   1            → 完全不可见
            //   2            → 不绘制线下音符
            //   100..1000    → 按 (w - 100)/10 拍提前出现
            //   1000..2000   → 预留（尚未实现）
            if alpha < 0.0 {
                if !settings.pe_alpha_extension {
                    return;
                }
                let w = (-alpha).floor() as u32;
                match w {
                    1 => {
                        return;
                    }
                    2 => {
                        config.draw_below = false;
                    }
                    w if (100..1000).contains(&w) => {
                        config.appear_before = (w as f64 - 100.) / 10.;
                    }
                    w if (1000..2000).contains(&w) => {
                        // TODO unsupported
                    }
                    _ => {}
                }
            }
            // 阶段四：求出屏幕上下边缘对应的世界高度，用于裁剪不可见音符。
            // 之所以取 (±1.1, ±1) 而不是恰好 (±1, ±1)：判定线可能带旋转，
            // 旋转后的屏幕四角会略微超出范围，留 10% 余量可避免边缘音符被误剔除。
            // `screen_to_world` 把屏幕坐标反算为世界坐标，这样无论当前模型矩阵如何都能得到正确的裁剪界。
            let (vw, vh) = (1.1, 1.);
            let p = [
                res.screen_to_world(Point::new(-vw, -vh)),
                res.screen_to_world(Point::new(-vw, vh)),
                res.screen_to_world(Point::new(vw, -vh)),
                res.screen_to_world(Point::new(vw, vh)),
            ];
            let height_above = p[0].y.max(p[1].y.max(p[2].y.max(p[3].y))) * res.aspect_ratio;
            let height_below = -p[0].y.min(p[1].y.min(p[2].y.min(p[3].y))) * res.aspect_ratio;
            // 阶段五：绘制线上音符。
            // 前 `not_plain_count` 个音符（非 plain）逐个绘制，它们无法按速度分组裁剪；
            // 其余按速度分组，组内音符高度递减（或递增），一旦超出屏幕范围即可提前结束循环。
            // 该提前终止只在 `aggressive` 模式下启用：它对音符高度随时间的缓慢变化做了近似，
            // 非激进模式宁可多画几个也不冒漏画的风险。
            let agg = res.config.aggressive;
            for note in self.notes.iter().take(self.cache.not_plain_count).filter(|it| it.above) {
                note.render(res, &mut config, bpm_list);
            }
            for index in &self.cache.above_indices {
                let speed = self.notes[*index].speed;
                let limit = height_above as f64 / speed;
                for note in self.notes[*index..].iter() {
                    if !note.above || speed != note.speed {
                        break;
                    }
                    if agg && note.height - config.line_height + note.object.translation.1.now() as f64 > limit {
                        break;
                    }
                    note.render(res, &mut config, bpm_list);
                }
            }
            // 阶段六：绘制线下音符。
            // 与线上音符逻辑完全对称，区别有二：只遍历 `above == false` 的音符，
            // 且整体套一层 y 轴镜像变换（`append_nonuniform_scaling(1, -1)`），
            // 把它们画到判定线的另一侧。
            res.with_model(Matrix::identity().append_nonuniform_scaling(&Vector::new(1.0, -1.0)), |res| {
                for note in self.notes.iter().take(self.cache.not_plain_count).filter(|it| !it.above) {
                    note.render(res, &mut config, bpm_list);
                }
                for index in &self.cache.below_indices {
                    let speed = self.notes[*index].speed;
                    let limit = height_below as f64 / speed;
                    for note in self.notes[*index..].iter() {
                        if speed != note.speed {
                            break;
                        }
                        if agg && note.height - config.line_height + note.object.translation.1.now() as f64 > limit {
                            break;
                        }
                        note.render(res, &mut config, bpm_list);
                    }
                }
            });
        });
    }
}
