//! 音符（`Note`）的数据模型、状态更新与绘制。
//!
//! 把握三条主线：
//! 1. **坐标与量纲**：音符的纵向位置由 `height`（对速度积分得到的绝对高度）与判定线
//!    高度之差决定，渲染时统一除以 `aspect_ratio` 变成归一化世界坐标；横向宽度则统一
//!    来自 `Resource::note_width`，因此音符在任意分辨率下的视觉大小一致。
//! 2. **批渲染**：音符不会自己发 draw call，所有几何都经 `draw_tex_pts` 压入
//!    `Resource::note_buffer`，由 `NoteBuffer::draw_all` 按 (层级, 纹理) 合并后一次提交，
//!    这是本项目最关键的绘制路径。
//! 3. **状态机**：可见性由 `judge`（判定状态）与当前时间共同决定；Hold 按住期间持续
//!    发射粒子，miss 的音符交给 `BadNote` 保留一段残留特效，假音符只做提示不参与判定。

use super::{chart::ChartSettings, BpmList, CtrlObject, JudgeLine, Matrix, Object, Point, Resource};
pub use crate::{
    config::Mods,
    judge::{HitSound, JudgeStatus, LIMIT_BAD},
    parse::RPE_HEIGHT,
};
use macroquad::prelude::*;

/// Hold 按住期间发射持续粒子的时间间隔（秒，还会再按曲速缩放）。
///
/// 0.15 s 是观感与开销的折中：更密会迅速堆积大量粒子拖慢帧率，更疏则看不出
/// 「一直按住」的持续反馈。
const HOLD_PARTICLE_INTERVAL: f64 = 0.15;
/// 音符越过判定点后的淡出时长（秒）。
///
/// 命中后需要一点视觉残留来确认打到了，0.16 s 约相当于 1~2 帧的余韵。
const FADEOUT_TIME: f64 = 0.16;
/// miss 音符残留特效（`BadNote`）的存活时长（秒）。
///
/// 0.5 s 足够玩家看清漏掉的是哪个音符，又不会长期占用绘制与内存。
const BAD_TIME: f64 = 0.5;

/// 音符种类。
///
/// 之所以需要 `Clone`：`BadNote` 会保存一份种类副本用于绘制残留特效。
/// `Debug` 便于谱面解析出错时打印上下文。
#[derive(Clone, Debug)]
pub enum NoteKind {
    /// 点击：单点判定，最基础的音符
    Click,
    /// 长按：从 `time` 持续到 `end_time`，纵向从 `height` 延伸到 `end_height`。
    /// 其中 `end_time` 是长按结束时间（秒），`end_height` 是末端高度（与
    /// `Note::height` 同量纲，同样是对速度积分得到的高度的结果）。
    Hold { end_time: f64, end_height: f64 },
    /// 滑动：需要横向划动，判定相对宽松
    Flick,
    /// 拖拽：按住即可，无需精确到帧
    Drag,
}

// 绘制层级查询：层级决定遮挡关系，也决定批渲染的分组。
impl NoteKind {
    /// 返回该种类的绘制层级（数值越小越先绘制，即越靠下层）。
    ///
    /// 顺序 Hold(0) < Drag(1) < Click(2) < Flick(3) 的考量：
    /// 长条体量最大、占据的纵向范围最广，垫底才不会遮住其他音符；
    /// 拖拽与点击居中；Flick 是打击感最强的音符，放最上层确保不被任何音符压住。
    /// 该值同时是 `NoteBuffer` 的分组键之一，因此不同种类无法合并进同一批次。
    pub fn order(&self) -> i8 {
        match self {
            Self::Hold { .. } => 0,
            Self::Drag => 1,
            Self::Click => 2,
            Self::Flick => 3,
        }
    }
}

/// 单个音符。
pub struct Note {
    /// 音符自身的关键帧动画（透明度 / 缩放 / 旋转 / 纵向位移等）
    pub object: Object,
    /// 音符种类，决定判定方式与绘制图形
    pub kind: NoteKind,
    /// 该音符使用的打击音效
    pub hitsound: HitSound,
    /// 判定时间点（秒）
    pub time: f64,
    /// 音符的绝对高度（对线速积分得到，语义见 [`JudgeLine::height`]）。
    /// 与所属判定线当前高度之差，就是音符相对判定线的纵向位移依据
    pub height: f64,
    /// 该音符所属速度段的速度；渲染时用于把高度换算成屏幕纵向位移
    pub speed: f64,
    /// 音符基础颜色（来自谱面，缺省为白）
    pub color: Color,
    /// 自定义打击特效颜色；为 `None` 时按判定结果取资源包的 perfect/good 配色
    pub fx_color: Option<Color>,
    /// 横向判定宽度倍率：谱面可为个别音符放宽或收紧横向命中范围
    pub judge_area: f32,

    /// From the other side of the line
    /// 是否位于判定线的另一侧：为真时音符贴在线的上方，
    /// 否则渲染前会整体旋转 180°（见 [`Note::rotation`]）
    pub above: bool,
    /// 多押提示标记，由 `process_lines` 标出：为真且开启 `double_hint` 时，
    /// 改用更宽的 `note_style_mh` 贴图提示玩家需要多指同按
    pub multiple_hint: bool,
    /// 假音符：只做视觉提示、不参与判定，到达时间点后直接淡出
    pub fake: bool,
    /// 当前判定状态（未判定 / 已判定 / Hold 保持中 / 已被 miss 等）
    pub judge: JudgeStatus,
}

/// 单条判定线渲染其音符时的共享配置。
///
/// 之所以把它单独抽出并在同一判定线的所有音符间复用：
/// - `incline_sin` 等值对整条线是常量，预先算好可省下每个音符一次三角函数；
/// - `ctrl_obj` 是可变借用，多个音符需要共享同一份控制对象状态（横向缩放等）。
/// 生命周期 `'a` 把借用范围限制在一次 `render` 调用之内。
pub struct RenderConfig<'a> {
    /// 谱面设置（例如 Hold 是否半覆盖）
    pub settings: &'a ChartSettings,
    /// 本判定线的控制对象（可变借用，音符绘制时读取其横向缩放）
    pub ctrl_obj: &'a mut CtrlObject,
    /// 判定线当前高度（世界单位）
    pub line_height: f64,
    /// 音符最多提前多少拍出现；`f64::INFINITY` 表示不做限制。
    /// 由判定线 alpha 的负值「特效扩展」改写（见 [`JudgeLine::render`]）。
    pub appear_before: f64,
    /// 是否绘制「线下」（时间轴上已越过判定点）的音符，见 [`JudgeLine::show_below`]
    pub draw_below: bool,
    /// 判定线当前倾角的正弦值，供音符计算横向透视缩放
    pub incline_sin: f32,
}

/// 以矩形区域左上角为锚点，把一个纹理四边形排入绘制队列。
///
/// 参数较多是因为它同时承担「定位、缩放、裁剪、翻转」四件事，最终由
/// `draw_tex_pts` 落地。`clip` 为真时会把超出 y=0 的部分裁掉：
/// 线上/线下音符的绘制基于镜像坐标，越界部分本就不该显示。
/// 裁剪采用调整纹理 `source` 区域的方式而非 GPU 裁剪面，避免额外的状态切换开销。
#[allow(clippy::too_many_arguments)]
fn draw_tex(res: &Resource, texture: Texture2D, order: i8, x: f32, y: f32, color: Color, mut params: DrawTextureParams, clip: bool) {
    let Vec2 { x: w, y: h } = params.dest_size.unwrap();
    if h < 0. {
        return;
    }
    let mut p = [Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h)];
    if clip {
        if y + h <= 0. {
            return;
        }
        if y <= 0. {
            let r = -y / (y + h);
            p[0].y = 0.;
            p[1].y = 0.;
            let mut source = params.source.unwrap_or_else(|| Rect::new(0., 0., 1., 1.));
            source.y += source.h * r;
            params.source = Some(source);
        }
    }
    params.flip_y = true;
    draw_tex_pts(res, texture, order, p, color, params);
}
/// 把世界坐标下的 4 个角点转成屏幕顶点并压入 [`Resource::note_buffer`]（批渲染入队点）。
///
/// 几个关键处理：
/// - 先用 `world_to_screen` 把角点变换到屏幕空间，并做整体剔除：
///   完全落在 [-1,1] 之外直接丢弃，省下无谓的顶点提交；
/// - `flip_x`/`flip_y` 通过交换角点顺序实现，而不是改 UV，
///   这样 UV 与纹理方向的对应关系保持不变；
/// - 索引固定为 `(0,1,2)` 与 `(0,2,3)` 两个三角形，
///   与 `NoteBuffer::push` 中生成的索引严格对应，故顶点顺序必须保持「左上/右上/右下/左下」。
fn draw_tex_pts(res: &Resource, texture: Texture2D, order: i8, p: [Point; 4], color: Color, params: DrawTextureParams) {
    let mut p = p.map(|it| res.world_to_screen(it));
    if p[0].x.min(p[1].x.min(p[2].x.min(p[3].x))) > 1.
        || p[0].x.max(p[1].x.max(p[2].x.max(p[3].x))) < -1.
        || p[0].y.min(p[1].y.min(p[2].y.min(p[3].y))) > 1.
        || p[0].y.max(p[1].y.max(p[2].y.max(p[3].y))) < -1.
    {
        return;
    }
    let Rect { x: sx, y: sy, w: sw, h: sh } = params.source.unwrap_or(Rect { x: 0., y: 0., w: 1., h: 1. });

    if params.flip_x {
        p.swap(0, 1);
        p.swap(2, 3);
    }
    if params.flip_y {
        p.swap(0, 3);
        p.swap(1, 2);
    }

    #[rustfmt::skip]
    let vertices = [
        Vertex::new(p[0].x, p[0].y, 0., sx     , sy     , color),
        Vertex::new(p[1].x, p[1].y, 0., sx + sw, sy     , color),
        Vertex::new(p[2].x, p[2].y, 0., sx + sw, sy + sh, color),
        Vertex::new(p[3].x, p[3].y, 0., sx     , sy + sh, color),
    ];
    res.note_buffer
        .borrow_mut()
        .push((order, texture.raw_miniquad_texture_handle().gl_internal_id()), vertices);
}

/// 以原点为中心、按宽度 `scale` 等比缩放地绘制一个纹理（音符与 `BadNote` 的通用画法）。
///
/// 高度由纹理宽高比推导（`tex.height() * scale / tex.width()`），保证贴图不被拉伸变形；
/// `clip` 传 `false`，因为音符本体本来就跨越判定线两侧，裁剪会切掉一半图形。
fn draw_center(res: &Resource, tex: Texture2D, order: i8, scale: f32, color: Color) {
    let hf = vec2(scale, tex.height() * scale / tex.width());
    draw_tex(
        res,
        tex,
        order,
        -hf.x,
        -hf.y,
        color,
        DrawTextureParams {
            dest_size: Some(hf * 2.),
            ..Default::default()
        },
        false,
    );
}

// 音符的变换、状态更新与绘制。
impl Note {
    /// 计算音符相对判定线应处的旋转角（度）。
    ///
    /// 判定线旋转会带着音符一起转；若音符在线的另一侧（`above == false`），
    /// 还需额外转 180°，效果等同于「把音符翻到线的下方朝外」。
    pub fn rotation(&self, line: &JudgeLine) -> f32 {
        line.object.rotation.now() + if self.above { 0. } else { 180. }
    }

    /// 是否为「普通」音符：只有普通音符才能走按速度分组的快速裁剪路径。
    ///
    /// 三类音符被排除在外：
    /// - 假音符：有独立的淡出逻辑，判定点一到就必须消失；
    /// - Hold：有独立的 body/head/tail 绘制与判定语义；
    /// - 带纵向位移关键帧（`translation.1` 关键帧数 > 1）的音符：
    ///   它们的位置会随控制对象变化，无法用「高度 × 速度」这样稳定的排序键分组。
    pub fn plain(&self) -> bool {
        !self.fake && !matches!(self.kind, NoteKind::Hold { .. }) && self.object.translation.1.keyframes.len() <= 1
        // && self.ctrl_obj.is_default()
    }

    /// 推进音符自身动画，并在按住 Hold 期间周期性发射持续粒子。
    ///
    /// # Arguments
    /// * `parent_rot` - 所属判定线的累计旋转角（度），用于粒子朝向。
    /// * `parent_tr` - 所属判定线的世界变换，用于把粒子发射点放到音符的实际位置。
    /// * `ctrl_obj` - 判定线的控制对象，补充横向缩放等事件修正。
    /// * `line_height` - 判定线当前高度，用于把音符绝对高度换算成相对位移。
    pub fn update(&mut self, res: &mut Resource, parent_rot: f32, parent_tr: &Matrix, ctrl_obj: &mut CtrlObject, line_height: f64) {
        self.object.set_time(res.time);
        // Hold 持续粒子：只在「保持中」状态产生（`JudgeStatus::Hold`）。
        // `at` 是下一次应当发射粒子的时间点，每触发一次便推后一个间隔；
        // 间隔除以曲速 `config.speed` 是为了让粒子密度随音乐速度同步——
        // 曲速翻倍时音乐也翻倍，粒子按绝对时间间隔发射会显得越来越疏。
        if let Some(color) = if let JudgeStatus::Hold(perfect, at, ..) = &mut self.judge {
            if res.time > *at {
                *at += HOLD_PARTICLE_INTERVAL / res.config.speed as f64;
                // 颜色优先级：谱面自定义特效色 > 按起手判定结果取资源包配色。
                // 用起手时记录的 `perfect` 而非实时状态，可以让整条 Hold 的粒子
                // 颜色保持一致，中途迟一点也不会突然换色。
                Some(self.fx_color.unwrap_or_else(|| {
                    if *perfect {
                        res.res_pack.info.fx_perfect()
                    } else {
                        res.res_pack.info.fx_good()
                    }
                }))
            } else {
                None
            }
        } else {
            None
        } {
            // 发射位置取音符在世界中的真实位置（父级变换 × 本音符的入场变换），
            // `base` 与 `incline_sin` 传 0 表示只取基础变换、不含透视修正。
            // 朝向在判定线旋转的基础上，对线另一侧的音符再补 180°，
            // 保证粒子始终沿音符自身朝外的方向飞散。
            self.init_ctrl_obj(ctrl_obj, line_height);
            res.with_model(parent_tr * self.now_transform(res, ctrl_obj, 0., 0.), |res| {
                res.emit_at_origin(parent_rot + if self.above { 0. } else { 180. }, color)
            });
        }
    }

    /// 判断音符是否可以从判定线中移除。
    ///
    /// 条件由两部分相与得到：非 Hold 或已被判定完毕的 Hold，且 `object` 的动画已结束
    /// （`object.dead()`）。之所以 Hold 必须等到判定结束：未结束的 Hold 仍要持续
    /// 发射粒子并提供视觉反馈；而普通音符只要动画播完就没有存在意义。
    pub fn dead(&self) -> bool {
        (!matches!(self.kind, NoteKind::Hold { .. }) || matches!(self.judge, JudgeStatus::Judged)) && self.object.dead()
        // && self.ctrl_obj.dead()
    }

    /// 把音符的纵向位置写入控制对象，使 `CtrlObject` 的事件系统能按「高度」匹配到本音符。
    ///
    /// 表达式的含义是 `(音符高度 − 线高度 + 自身位移 / 速度) × RPE_HEIGHT / 2`：
    /// - 减去线高度得到相对位移；
    /// - 自身位移除以 `speed`，是把「屏幕纵向位移」换算回该速度段的归一化高度，
    ///   否则快速度段下的位移会被重复放大；
    /// - 乘 `RPE_HEIGHT / 2` 是为了与谱面格式（RPE）使用的坐标单位对齐。
    fn init_ctrl_obj(&self, ctrl_obj: &mut CtrlObject, line_height: f64) {
        ctrl_obj.set_height((self.height - line_height + self.object.translation.1.now() as f64 / self.speed) * RPE_HEIGHT as f64 / 2.);
    }

    /// 计算音符当前的局部变换矩阵（旋转 → 缩放 → 平移，按 TRS 顺序右乘）。
    ///
    /// `base` 是音符相对判定线的纵向基准位移：线上（`above`）音符直接传
    /// `height - line_height`，线下音符因外层已套了 y 轴镜像坐标系，故传 0。
    ///
    /// # Arguments
    /// * `ctrl_obj` - 提供事件驱动的横向缩放（`pos`）与整体缩放（`size`）。
    /// * `incline_sin` - 判定线倾角的正弦，用于计算透视。
    ///
    /// 三个需要留意的地方：
    /// - `incline_val` 实现判定线倾斜的透视效果：离判定线越远的音符横向越窄；
    ///   Hold 不参与该修正（保持 1.），否则长条会被压缩得明显穿帮；
    /// - `note_uniform_scale` 为假时纵向固定为 1，只让横向随音符缩放变化，
    ///   这是部分谱面风格所依赖的观感；
    /// - 与判定线不同，音符的变换**包含缩放**，因为音符大小本就是其自身的表现属性。
    pub fn now_transform(&self, res: &Resource, ctrl_obj: &CtrlObject, base: f32, incline_sin: f32) -> Matrix {
        let incline_val = 1. - incline_sin * (base * res.aspect_ratio + self.object.translation.1.now()) * RPE_HEIGHT / 2. / 360.;
        let mut tr = self.object.now_translation(res);
        tr.x *= if matches!(self.kind, NoteKind::Hold { .. }) {
            1.
        } else {
            incline_val * ctrl_obj.pos.now_opt().unwrap_or(1.)
        };
        tr.y += base;
        let mut scale = self.object.scale.now_with_def(1.0, 1.0);
        scale.x *= ctrl_obj.size.now_opt().unwrap_or(1.0);
        if res.info.note_uniform_scale {
            scale.y *= ctrl_obj.size.now_opt().unwrap_or(1.0);
        } else {
            scale.y = 1.0;
        };
        self.object.now_rotation().append_nonuniform_scaling(&scale).append_translation(&tr)
    }

    /// 把音符的几何排入 [`Resource::note_buffer`]（本函数不直接发 draw call）。
    ///
    /// 步骤依次为：可见性判定 → 尺寸与颜色 → 相对位移与裁剪基准 → 淡出与模组透明度
    /// → 按种类提交几何（Hold 额外绘制 body/head/tail 三段）。
    ///
    /// # Arguments
    /// * `config` - 同一判定线共享的渲染配置，见 [`RenderConfig`]。
    /// * `bpm_list` - 用于把 `appear_before` 的「拍数」换算成绝对时间。
    pub fn render(&self, res: &mut Resource, config: &mut RenderConfig, bpm_list: &mut BpmList) {
        // 已判定的非 Hold 音符立即停止绘制：命中即消失是打击反馈的核心。
        // Hold 例外，它要在按住期间继续显示长条。
        if matches!(self.judge, JudgeStatus::Judged) && !matches!(self.kind, NoteKind::Hold { .. }) {
            return;
        }
        // 「提前出现」限制：把音符时间换算成拍，再回退 `appear_before` 拍得到最早可见时刻，
        // 当前时间未到就不绘制。用于判定线 alpha 负值开启的渐显效果。
        if config.appear_before.is_finite() {
            // TODO optimize
            let beat = bpm_list.beat(self.time);
            let time = bpm_list.time_beats(beat - config.appear_before);
            if time > res.time {
                return;
            }
        }
        // 尺寸：基础宽度取全局 `note_width`（= 音符缩放 × NOTE_WIDTH_RATIO_BASE）；
        // 开启多押提示且本音符被标记时，再乘「mh 贴图宽度 / 普通贴图宽度」，
        // 使多押音符比普通音符更宽、更容易被识别。
        let scale = (if res.config.double_hint && self.multiple_hint {
            res.res_pack.note_style_mh.click.width() / res.res_pack.note_style.click.width()
        } else {
            1.0
        }) * res.note_width;
        let ctrl_obj = &mut config.ctrl_obj;
        // 先把音符高度同步给控制对象（事件系统按高度匹配），再刷新其动画。
        self.init_ctrl_obj(ctrl_obj, config.line_height);
        // 透明度叠加：音符自身 alpha × 全局 alpha × 控制对象 alpha。
        // `spd` 是「速度段速度 × 控制对象纵向倍率」，用于把高度差换算成屏幕上真实看到的位移。
        let mut color = Color {
            a: self.object.now_alpha(),
            ..self.color
        };
        color.a *= res.alpha * ctrl_obj.alpha.now_opt().unwrap_or(1.);
        let spd = self.speed * ctrl_obj.y.now_opt().unwrap_or(1.) as f64;

        // 世界高度归一化：除以 `aspect_ratio` 让不同屏幕比例下的音符落点一致，
        // 乘 `spd` 则保证快慢速度段下的音符间距按视觉速度缩放。
        let line_height = config.line_height / res.aspect_ratio as f64 * spd;
        let height = self.height / res.aspect_ratio as f64 * spd;

        // `base` 是音符相对判定线的位移。`cover_base` 是参与裁剪判断的基准：
        // 开启 Hold 半覆盖时改用「末端高度」而非起点高度，这样很长的长条
        // 不会因为起点刚滑出屏幕下缘就被整根剔除。
        let base = height - line_height;
        let cover_base = if !config.settings.hold_partial_cover {
            height - line_height
        } else {
            match self.kind {
                NoteKind::Hold { end_time: _, end_height } => {
                    let end_height = end_height / res.aspect_ratio as f64 * spd;
                    end_height - line_height
                }
                _ => height - line_height,
            }
        };

        // 可见性剔除（仅在不绘制线下音符时生效）：
        // - 越过判定点已超过 `FADEOUT_TIME` 的普通音符彻底消失，
        //   假音符则在到达时间点的瞬间就设为不可见（它本就不该有判定反馈）；
        // - 尚未到达判定点、但已滑出屏幕下缘（`cover_base <= -0.001`）的音符也提前跳过。
        //   留 1e-3 容差是为了避免浮点误差把恰好贴边的音符误判为不可见。
        if !config.draw_below
            && (((res.time - FADEOUT_TIME >= self.time || self.fake && res.time >= self.time) && !matches!(self.kind, NoteKind::Hold { .. }))
                || (self.time > res.time && cover_base <= -0.001))
        {
            return;
        }
        // 选择贴图集：开启多押提示且本音符被标记时换用 `note_style_mh` 一整套贴图。
        // `order` 同时是批渲染的分组键，保证同层级同纹理的几何能被合并提交。
        let order = self.kind.order();
        let style = if res.config.double_hint && self.multiple_hint {
            &res.res_pack.note_style_mh
        } else {
            &res.res_pack.note_style
        };
        // 模组对可见性的影响（`LIMIT_BAD` 既是 Bad 判定阈值，也复用为这里的过渡时间尺度）：
        // - FADE_OUT：越接近判定点透明度越低，到判定点附近彻底看不见；
        // - FADE_IN：与 FADE_OUT 相反，由不可见渐显，用于先看清谱面再动手；
        // - 两者都未开启（互斥）时不做处理。
        let mod_alpha = if res.config.has_mod(Mods::FADE_OUT) {
            ((self.time - res.time - LIMIT_BAD) / LIMIT_BAD).clamp(0., 1.)
        } else if res.config.has_mod(Mods::FADE_IN) {
            (1. - (self.time - res.time - LIMIT_BAD) / LIMIT_BAD).clamp(0., 1.)
        } else {
            1.
        };
        // Click / Flick / Drag 共用同一条绘制路径，只是换贴图，因此做成闭包避免重复代码。
        let draw = |res: &mut Resource, tex: Texture2D| {
            let mut color = color;
            if !config.draw_below {
                // 命中后的淡出：判定点之前 alpha 恒为 1；越过判定点后按
                // `1 - (res.time - note.time) / FADEOUT_TIME` 线性衰减。
                // `min(0.)` 正是实现这个分段的技巧：差值取负值再除以正数即为衰减量。
                // 假音符一旦到达时间点直接归零。
                let alpha = (self.time - res.time).min(0.) / FADEOUT_TIME + 1.;
                color.a *= if self.fake && res.time >= self.time { 0. } else { alpha as f32 };
            }
            color.a *= mod_alpha as f32;
            res.with_model(self.now_transform(res, ctrl_obj, base as f32, config.incline_sin), |res| {
                draw_center(res, tex, order, scale, color);
            });
        };
        // 按种类提交几何：Click / Flick / Drag 只是换一张贴图；
        // Hold 需要以判定线为基准单独绘制 body / head / tail 三段（按 `hold_atlas`
        // 给出的纵向 UV 区间从同一张 hold 贴图中切分）。
        match self.kind {
            NoteKind::Click => {
                draw(res, *style.click);
            }
            NoteKind::Hold { end_time, end_height } => {
                res.with_model(self.now_transform(res, ctrl_obj, 0., 0.), |res| {
                    // 长条三段共用同一张 hold 贴图；开启多押提示时需要整条换成 mh 贴图集，
                    // 因此这里按同样的条件再选一次。
                    let style = if res.config.double_hint && self.multiple_hint {
                        &res.res_pack.note_style_mh
                    } else {
                        &res.res_pack.note_style
                    };
                    if matches!(self.judge, JudgeStatus::Judged) {
                        // miss
                        // 已判为 miss 的 Hold 半透明显示，既提示这里漏了，
                        // 又让玩家仍能看出长条原本的长度
                        color.a *= 0.5;
                    }
                    // 时间已过末端则整段不画，避免长条在结束后仍然残留
                    if res.time >= end_time {
                        return;
                    }
                    let end_height = end_height / res.aspect_ratio as f64 * spd;
                    color.a *= mod_alpha as f32;

                    let h = if self.time <= res.time { line_height } else { height };
                    let bottom = (h - line_height) as f32;
                    let top = (end_height - line_height) as f32;
                    let tex = &style.hold;
                    let ratio = style.hold_ratio();
                    // body
                    // TODO (end_height - height) is not always total height
                    // 长条主体：从头部当前位置延伸至末端。
                    // `hold_repeat` 模式使用单独裁剪出的可平铺纹理，并按实际长度换算 UV，
                    // 避免超长条把 atlas 区间拉伸得极其模糊；否则直接拉伸 atlas 的 body 区间。
                    draw_tex(
                        res,
                        **(if res.res_pack.info.hold_repeat {
                            style.hold_body.as_ref().unwrap()
                        } else {
                            tex
                        }),
                        order,
                        -scale,
                        bottom,
                        color,
                        DrawTextureParams {
                            source: Some({
                                if res.res_pack.info.hold_repeat {
                                    let hold_body = style.hold_body.as_ref().unwrap();
                                    let width = hold_body.width();
                                    let height = hold_body.height();
                                    Rect::new(0., 0., 1., (top - bottom) / scale / 2. * width / height)
                                } else {
                                    style.hold_body_rect()
                                }
                            }),
                            dest_size: Some(vec2(scale * 2., top - bottom)),
                            ..Default::default()
                        },
                        false,
                    );
                    // head
                    // 头部：仅在音符尚未到达判定点时绘制，或资源包要求常显头部时绘制；
                    // `hold_compact` 让头部向内贴合，而不是向外多扩张一倍高度。
                    if res.time < self.time || res.res_pack.info.hold_keep_head {
                        let r = style.hold_head_rect();
                        let hf = vec2(scale, r.h / r.w * scale * ratio);
                        draw_tex(
                            res,
                            **tex,
                            order,
                            -scale,
                            bottom - if res.res_pack.info.hold_compact { hf.y } else { hf.y * 2. },
                            color,
                            DrawTextureParams {
                                source: Some(r),
                                dest_size: Some(hf * 2.),
                                ..Default::default()
                            },
                            false,
                        );
                    }
                    // tail
                    // 尾部：长按末端标记，位置随 `end_height` 移动；`hold_compact` 同样控制贴合方式。
                    let r = style.hold_tail_rect();
                    let hf = vec2(scale, r.h / r.w * scale * ratio);
                    draw_tex(
                        res,
                        **tex,
                        order,
                        -scale,
                        top - if res.res_pack.info.hold_compact { hf.y } else { 0. },
                        color,
                        DrawTextureParams {
                            source: Some(r),
                            dest_size: Some(hf * 2.),
                            ..Default::default()
                        },
                        false,
                    );
                });
            }
            NoteKind::Flick => {
                draw(res, *style.flick);
            }
            NoteKind::Drag => {
                draw(res, *style.drag);
            }
        }
    }
}

/// miss 音符的残留特效快照。
///
/// 之所以在音符消失后单独留存一份「时间 + 种类 + 当时的变换矩阵」，而不是让原音符
/// 继续绘制：原音符会被 `update` 从判定线中移除，拍下快照后特效即可完全脱离判定线
/// 状态独立存在，并保持固定的存活时长。
pub struct BadNote {
    /// 该音符本应被判定的时间点（秒）
    pub time: f64,
    /// 音符种类，决定残留特效使用哪张贴图
    pub kind: NoteKind,
    /// 音符消失瞬间的世界变换，用于把特效固定画在原位（此后不再随任何状态变化）
    pub matrix: Matrix,
}

// BadNote 的绘制与生命周期管理。
impl BadNote {
    /// 绘制 miss 残留特效。
    ///
    /// # Returns
    /// 是否仍需继续保留：超过 `BAD_TIME` 后返回 `false`，调用方据此移除本特效。
    ///
    /// 颜色固定为暗红 (0.423529, 0.262745, 0.262745)，与正常命中特效形成明显区分；
    /// alpha 同样按 `time - res.time` 线性衰减，`max(-1.)` 保证归一化结果不会越界。
    ///
    /// # Panics
    /// 若 `kind` 为 Hold 会 panic：Hold 不会被记录为 `BadNote`，`unreachable!()`
    /// 正是对这一不变量的断言。
    pub fn render(&self, res: &mut Resource) -> bool {
        if res.time > self.time + BAD_TIME {
            return false;
        }
        res.with_model(self.matrix, |res| {
            let style = &res.res_pack.note_style;
            draw_center(
                res,
                match &self.kind {
                    NoteKind::Click => *style.click,
                    NoteKind::Drag => *style.drag,
                    NoteKind::Flick => *style.flick,
                    _ => unreachable!(),
                },
                self.kind.order(),
                res.note_width,
                Color::new(0.423529, 0.262745, 0.262745, ((self.time - res.time).max(-1.) / BAD_TIME + 1.) as f32),
            );
        });
        true
    }
}
