//! 手动 offset（判定延迟）校准页。
//!
//! **重要：本页不使用自动偏移算法。** 自动偏移（音频 SuperFlux 起始点检测 + 音符加权高斯脉冲
//! + 归一化互相关搜索最佳 lag）实现在 `prpr/src/ui/offset_analysis.rs` 与 `prpr-auto-offset` 包中，
//! 入口在游玩场景的延迟面板里；本页提供的是另一条路径——**靠耳朵对拍的手动校准**：
//! 循环播放伴奏 `cali.ogg`，同时让落点方块沿判定线扫过，到线时播放 `cali_hit.ogg`，
//! 用户听打击音与伴奏节拍是否吻合，并用滑块微调延迟值。
//!
//! 交互设计的两个要点：
//! - 判据是**听感**而非画面：方块在 `t <= 1.` 时可见、越过基准线即刻消失并响一声打击音
//!   （见 [`OffsetPage::render`]）。用户要调的是那一声打击音与鼓点的对齐；
//! - 校准结果**直接写进** `get_data_mut().config.offset`，因此拖动滑块立刻生效、没有确认按钮，
//!   只在离开页面或切后台时才落盘（[`Page::exit`] / [`Page::pause`]）。

prpr_l10n::tl_file!("cali");

use std::borrow::Cow;

use super::{Page, SharedState};
use crate::{get_data, get_data_mut, save_data};
use anyhow::{Context, Result};
use macroquad::prelude::*;
use prpr::{
    core::{ParticleEmitter, ResourcePack, NOTE_WIDTH_RATIO_BASE},
    ext::{create_audio_manger, semi_black, RectExt, SafeTexture, ScaleType},
    time::TimeManager,
    ui::{Slider, Ui},
};
use sasa::{AudioClip, AudioManager, Music, MusicParams, PlaySfxParams, Sfx};

/// 手动校准页。
pub struct OffsetPage {
    /// 音频管理器。**必须由本页持有**：`Music`/`Sfx` 都依赖它存活，
    /// 一旦提前 drop，正在播放的伴奏与打击音会一起失效。字段本身不被直接访问，故加下划线前缀。
    _audio: AudioManager,
    /// 校准伴奏（2 秒一循环，循环边界即一个节拍周期）。
    cali: Music,
    /// 越过基准线时播放的打击音，用于与伴奏节拍做听感比对。
    cali_hit: Sfx,

    /// 本页**私有**的时间管理器。
    ///
    /// 不复用 [`SharedState`] 的 `t`：那个时间由 `MainScene` 用引擎的时间管理器统一驱动，
    /// 页面无权 `seek_to`，而校准需要一条可被精确回卷到循环起点的时间轴。
    /// 它以 `adjust_time = true` 构造，配合被调大的 `force`，让画面时间快速收敛到音乐播放位置
    /// ——校准页最不能容忍音画漂移。
    tm: TimeManager,
    /// 上一帧落点方块是否可见。用它检测“可见 → 越过”的**跨越瞬间**，
    /// 保证每次经过只播一次打击音、只发一次粒子。
    cali_last: bool,

    /// 落点方块的贴图（取自当前资源包的点击特效）。
    click: SafeTexture,
    /// 资源包的打击特效贴图。本页不直接绘制它（故加下划线），但随资源包一起加载，
    /// 以尽早暴露资源包损坏的问题。
    _hit_fx: SafeTexture,
    /// 落点处的粒子发射器（与游玩时同一套）。
    emitter: ParticleEmitter,
    /// 粒子颜色，取资源包的“Perfect”判定色，与游玩时的视觉反馈保持一致。
    color: Color,

    /// 延迟滑块。范围与步长的含义见 [`OffsetPage::new`]。
    slider: Slider,

    /// “本帧刚按下”的标记：由 [`Page::touch`] 置位，由 [`Page::render`] 消费。
    ///
    /// 之所以要这样交接：按下发生在 `touch`，而这次按下的时刻要对应的**纵向位置**，
    /// 只有 `render` 算出当前音符时间后才能得到，于是拆成“置标记”与“配对”两步。
    touched: bool,
    /// 最近一次点击留下的 `(按下时刻, 落点位置)`，用于绘制一条逐渐淡出的提示线。
    /// 淡出结束（超过 `FADE_TIME`）后置回 `None`。
    touch: Option<(f32, f32)>,
}

// 构造：把所有资源（音频、时间轴、资源包）一次性准备好，进入后即可立刻开始校准。
impl OffsetPage {
    /// 点击提示线从出现到完全淡出的时长（秒）。
    const FADE_TIME: f32 = 0.8;

    /// 创建校准页。
    ///
    /// # Errors
    /// 音频文件（`cali.ogg` / `cali_hit.ogg`）解析失败、音频后端初始化失败，
    /// 或资源包加载失败时返回错误。
    ///
    /// 构造中的几处有意为之：
    /// - 音频管理器按用户配置创建：校准必须在**与游玩相同的输出链路与音量**下进行，
    ///   换了音量或后端，听到的延迟都可能不一样；
    /// - `loop_mix_time: 0.` 关闭循环交叉淡化：若开启，每 2 秒会有一次音量凹陷，
    ///   足以掩盖节拍点、破坏对拍；
    /// - `tm.force = 3e-2` 把对齐收敛系数调到默认值（`3e-3`）的十倍，
    ///   让画面时间尽快贴住音乐位置，避免校准时音画之间还在缓慢漂移；
    /// - 落点方块与粒子取自**当前资源包**：校准页看到的落点大小与特效应与实际游玩一致，
    ///   否则用户按校准页调好的延迟进了谱面会偏。
    pub async fn new() -> Result<Self> {
        let mut audio = create_audio_manger(&get_data().config)?;
        let cali = audio.create_music(
            AudioClip::new(load_file("cali.ogg").await?)?,
            MusicParams {
                amplifier: get_data().config.volume_music,
                loop_mix_time: 0.,
                ..Default::default()
            },
        )?;
        let cali_hit = audio.create_sfx(AudioClip::new(load_file("cali_hit.ogg").await?)?, None)?;

        // 1 倍速 + 开启音乐位置对齐，再用远大于默认值的收敛系数快速贴合
        let mut tm = TimeManager::new(1., true);
        tm.force = 3e-2;

        let respack = ResourcePack::from_path(get_data().config.res_pack_path.as_ref())
            .await
            .context("Failed to load resource pack")?;
        let click = respack.note_style.click.clone();
        let emitter = ParticleEmitter::new(&respack, get_data().config.note_scale, respack.info.hide_particles)?;
        Ok(Self {
            _audio: audio,
            cali,
            cali_hit,

            tm,
            cali_last: false,

            click,
            _hit_fx: respack.hit_fx,
            emitter,
            color: respack.info.fx_perfect(),

            // 单位是毫秒：±500ms 足以覆盖常见的音频输出延迟（蓝牙耳机通常 100~300ms），
            // 步长 5ms 是“手感上还能再调一点”与“不至于滑半天到不了头”之间的折中
            slider: Slider::new(-500.0..500.0, 5.),

            touched: false,
            touch: None,
        })
    }
}

// 本页在页面栈中的行为约定：
// - 进入时静音主菜单 BGM，并从头开始播放校准伴奏；
// - 校准值**立即**作用于全局配置，只在离场/切后台时落盘；
// - 暂停与恢复必须同时处理音频与私有时间轴，否则回来时会音画错位。
impl Page for OffsetPage {
    /// 进入本页时**不允许**主菜单 BGM 播放。
    ///
    /// 这是本页唯一**返回非默认值**的生命周期钩子，原因很直接：校准的判据是“听打击音与伴奏节拍
    /// 是否对齐”，任何背景音乐都会污染听感、也会占用音频输出链路影响延迟表现。
    /// `MainScene` 收到 `false` 后会把 BGM 淡出（见 `scene/main.rs`）。
    fn can_play_bgm(&self) -> bool {
        false
    }

    /// 标题栏文案，取自本模块的 `cali.ftl`。
    fn label(&self) -> Cow<'static, str> {
        tl!("label")
    }

    /// 离开本页时落盘配置。
    ///
    /// 拖滑块时是直接改内存中的全局配置（为了即时生效），因此这里必须保存，
    /// 否则用户调好的延迟会在退出应用后丢失。
    /// # Errors
    /// 配置文件写入失败时返回错误。
    fn exit(&mut self) -> Result<()> {
        save_data()?;
        Ok(())
    }

    /// 进入（或重新成为栈顶）时从头开始播放伴奏。
    ///
    /// 三件事的顺序有讲究：先把音频 seek 回 0，再从 0 播放，最后把私有时间轴归零复位，
    /// 使两条时间轴从同一个基准出发。若不 `reset`，从上层页面返回时游戏时间会接着上次走，
    /// 而音乐是从头播的，落点会跑到屏幕之外。
    fn enter(&mut self, _s: &mut SharedState) -> Result<()> {
        self.cali.seek_to(0.)?;
        self.cali.play()?;
        self.tm.reset();
        Ok(())
    }

    /// 切到后台时先保存再暂停音频与时间轴。
    ///
    /// 先 `save_data` 是因为移动端进程可能在后台被系统直接回收，
    /// 来不及走到 [`Page::exit`]，用户刚调好的延迟就丢了。
    /// # Errors
    /// 保存配置或暂停音频失败时返回错误。
    fn pause(&mut self) -> Result<()> {
        save_data()?;
        self.tm.pause();
        self.cali.pause()?;
        Ok(())
    }

    /// 回到前台时恢复时间轴与伴奏。
    ///
    /// `resume` 会把暂停期间流逝的真实时长补加到时间原点上（暂停不占用游戏时间），
    /// 因此恢复后落点不会因为停顿而“跳”一段。
    /// # Errors
    /// 音频恢复播放失败时返回错误。
    fn resume(&mut self) -> Result<()> {
        self.tm.resume();
        self.cali.play()?;
        Ok(())
    }

    /// 处理触摸：拖动滑块改延迟，或点按屏幕左半边留下一条“我认为这里该响”的提示线。
    ///
    /// 单位换算值得留意：`config.offset` 以**秒**为单位参与所有时间运算，
    /// 而 [`Slider`] 按**毫秒**工作（显示也是 `xxms`），因此进出各乘/除 1000。
    ///
    /// 写入时机是“拖动的每一帧”——没有任何确认步骤，改完立刻作用于渲染与音频判定，
    /// 这样用户才能边听边调。副作用是这里拿到的是全局配置的**可变再借用**
    /// （一直存活到函数末尾），所以本函数内不能再调用 [`get_data`] 读取配置，
    /// 否则会构成别名可变引用。
    ///
    /// # Returns
    /// 滑块被操作时返回 `true`（消费事件），否则放行。
    /// # Errors
    /// 无（本函数不产生错误，签名中的 `Result` 来自 trait 约定）。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        let t = s.t;
        let config = &mut get_data_mut().config;
        let mut offset = config.offset * 1000.;
        // `Slider::touch` 返回 `Some` 表示本次触摸确实作用在滑块上
        if self.slider.touch(touch, t, &mut offset).is_some() {
            config.offset = offset / 1000.;
            return Ok(true);
        }
        // 屏幕左半边（校准区）任意按下都记录一次“手动标记”，
        // 供用户把“我听到的落点”标在画面上做对比；这里只置标记，纵向位置等 `render` 再算
        if touch.phase == TouchPhase::Started && touch.position.x < 0. {
            self.touched = true;
        }
        Ok(false)
    }

    /// 每帧维持“游戏时间 ↔ 音乐位置”的同步。
    ///
    /// 刻意忽略 `SharedState`：本页使用私有时间轴，主时间轴与本页无关。
    ///
    /// 这里做三件事，且顺序不可交换：
    /// 1. 伴奏暂停时不同步（暂停中 `now()` 已冻结，再对齐没有意义）；
    /// 2. 循环回卷：`cali.ogg` 只有 2 秒，游戏时间超过 2 秒就整体减去 2 秒，
    ///    让时间轴永远落在 `[0, 2)` 这个循环窗口内，既避免长期累积导致浮点精度下降，
    ///    也让「落点位置 ↔ 时间」的映射保持单值。回卷后立即 `dont_wait()` 取消 seek 静默窗口，
    ///    使下一帧就能重新对齐——否则每次回卷都会有一小段“失同步”的空窗；
    /// 3. 用音乐播放位置反向修正时间原点（受 `force` 控制，表现为平滑收敛而非跳变）。
    ///    这里用 `now - pos >= -1.` 作为前置条件：刚回卷完的那一刻，音乐位置还在 2 附近
    ///    而游戏时间已回到 0 附近，若照常对齐会把时间轴整个往回拽，因此必须跳过这一小段窗口。
    fn update(&mut self, _s: &mut SharedState) -> Result<()> {
        if !self.cali.paused() {
            let pos = self.cali.position();
            let now = self.tm.now();
            if now > 2. {
                self.tm.seek_to(now - 2.);
                self.tm.dont_wait();
            }
            let now = self.tm.now();
            if now - pos >= -1. {
                self.tm.update(pos);
            }
        }
        Ok(())
    }

    /// 绘制校准画面：左侧判定线与扫过的落点、右侧延迟滑块。
    ///
    /// 时间轴的换算关系是理解本函数的钥匙：
    /// 取私有时钟 `tm.now()` 再**减去** `config.offset`，就得到“扫过判定线的时间进度”。
    /// 减去 offset 的含义是：offset 表示“音频比画面晚多少”，因此落点必须相应提前出现，
    /// 二者抵消后用户听到的打击音才与伴奏节拍对齐。滑块一改，下一帧这里的换算立刻变化，
    /// 画面与声音同时平移——这就是“边听边调”能成立的原因。
    ///
    /// 时间进度被折进 `[0, 2)`（与 `cali.ogg` 的循环长度一致），
    /// 因此 `t = 1.` 正好经过判定线，`t` 每 2 秒重复一次。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        s.render_fader(ui, |ui| {
            // 阶段一：校准区面板，左边界固定为 -0.92，右边界沿用内容区
            let lf = -0.92;
            let mut r = ui.content_rect();
            r.w += r.x - lf;
            r.x = lf;
            ui.fill_path(&r.rounded(0.02), semi_black(0.4));

            // 阶段二：判定线——一条白色细长矩形，横坐标 -0.4、靠近面板下沿再上移 0.12。
            // hw/hh 分别是它的半宽与半厚，后续绘制落点与提示线都要复用这两个值以保证对齐
            let ct = (-0.4, r.bottom() - 0.12);
            let hw = 0.4;
            let hh = 0.005;
            ui.fill_rect(Rect::new(ct.0 - hw, ct.1 - hh, hw * 2., hh * 2.), WHITE);

            // `ot` 保存游戏时间：后面的 `t` 会被复用为“循环内的音符时间”，
            // 而滑块动画与提示线淡出仍需用原始游戏时间
            let ot = t;

            // 阶段三：算当前音符时间。先减掉用户配置的延迟，再折进 [0, 2) 的循环窗口
            let config = &get_data().config;
            let mut t = self.tm.now() as f32 - config.offset;
            if t < 0. {
                t += 2.;
            }
            if t >= 2. {
                t -= 2.;
            }
            // 纵向位置：t 从 0 到 2 映射到判定线上下各 0.6（y 轴向下为正），
            // 因此 t < 1 时落点在判定线**之上**（正在接近），t = 1 恰好在线上，
            // t > 1 越过线继续向下。系数 0.6 就是“落点下落速度”在画面上的体现
            let ny = ct.1 + (t - 1.) * 0.6;
            // 把 `touch` 阶段置位的标记落实为一条提示：记录按下时刻与**当时算出的落点位置**
            if self.touched {
                self.touch = Some((ot, ny));
                self.touched = false;
            }
            // 阶段四：绘制落点。t <= 1 时方块可见；越过判定线的那一帧改播打击音与粒子
            if t <= 1. {
                // 宽度公式与游玩时一致（基准比例 × 用户音符缩放 × 2），
                // 乘 2 是因为 UI 坐标的半宽为 1 而屏幕宽为 2
                let w = NOTE_WIDTH_RATIO_BASE as f32 * config.note_scale * 2.;
                let h = w * self.click.height() / self.click.width();
                let r = Rect::new(ct.0 - w / 2., ny, w, h);
                ui.fill_rect(r, (*self.click, r, ScaleType::Fit));
                self.cali_last = true;
            } else {
                // 用 `cali_last` 保证每次经过只触发一次：粒子与打击音都打在**判定线**上
                // （而不是方块当前位置），因为用户听到的应当是“到线的那一声”
                if self.cali_last {
                    let g = ui.to_global(ct);
                    self.emitter.emit_at(vec2(g.0, g.1), 0., self.color);
                    let _ = self.cali_hit.play(PlaySfxParams {
                        amplifier: config.volume_sfx,
                    });
                }
                self.cali_last = false;
            }

            // 阶段五：提示线淡出。前半段（p <= 0.5）保持全不透明，之后线性淡出到消失，
            // 这样短暂的点按也能被看清，而不会一闪就没
            if let Some((time, pos)) = &self.touch {
                let p = (ot - time) / Self::FADE_TIME;
                if p > 1. {
                    self.touch = None;
                } else {
                    let p = p.max(0.);
                    let c = Color {
                        a: (if p <= 0.5 { 1. } else { (1. - p) * 2. }) * self.color.a,
                        ..self.color
                    };
                    ui.fill_rect(Rect::new(ct.0 - hw, pos - hh, hw * 2., hh * 2.), c);
                }
            }

            // 阶段六：右侧滑块。动画时间传 `ot`（游戏时间）而非私有时钟，
            // 这样应用暂停时滑块的按压动画也会一起冻结
            let offset = config.offset * 1000.;
            self.slider
                .render(ui, Rect::new(0.46, -0.1, 0.45, 0.2), ot, offset, format!("{offset:.0}ms"));
        });

        // 阶段七：粒子画在 `render_fader` 之外，因此不受页面转场位移/透明度影响，
        // 打击特效始终出现在判定线的真实位置上。推进用真实帧时间，
        // 即便页面正在转场或暂停也会把粒子演完
        self.emitter.draw(get_frame_time());

        Ok(())
    }
}
