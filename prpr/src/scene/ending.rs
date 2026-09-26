//! 结算场景：一局游玩结束后展示成绩，并处理成绩上传与后续去向。
//!
//! 界面内容由 [`PlayResult`]（判定统计）与 [`ChartInfo`]（谱面信息）驱动，包含：
//! 分数滚动动画、评级图标、准度/误差/平均帧率、各判定档位的明细（可展开 early/late 细分）、
//! 最大连击进度条、RKS 变化量，以及「重试 / 继续」两个按钮。
//!
//! 成绩上传是异步且可能失败的：本场景在上传期间会禁用两个按钮（并在点击时提示「正在上传」），
//! 失败时弹出对话框让玩家选择「取消」或「重试」，重试通过模块内的 [`RE_UPLOAD`] 标记
//! 跨帧传递到 [`Scene::update`] 中重新发起。
//!
//! 与 [`SimpleRecord`] 的关系：[`SimpleRecord`] 是「本地最好成绩」的精简表示（分数/准度/FC），
//! 通过 [`NextScene::PopNWithResult`] 交回上层用于更新本地记录；而
//! [`RecordUpdateState`] 是服务端返回的「本次上传带来的变化」，仅用于本界面展示。
prpr_l10n::tl_file!("ending");

use super::{draw_background, game::SimpleRecord, loading::UploadFn, NextScene, Scene};
use crate::{
    config::{Config, Mods},
    core::{BOLD_FONT, PGR_FONT},
    ext::{create_audio_manger, rect_shadow, semi_black, semi_white, RectExt, SafeTexture, ScaleType},
    info::ChartInfo,
    judge::{icon_index, PlayResult},
    scene::show_message,
    task::Task,
    time::TimeManager,
    ui::{button_hit, clip_sector, DRectButton, Dialog, MessageHandle, RectButton, Ui},
};
use anyhow::Result;
use macroquad::prelude::*;
use sasa::{AudioClip, AudioManager, Music, MusicParams};
use serde::Deserialize;
use std::{cell::RefCell, ops::DerefMut};

/// 成绩上传后服务端返回的「本次成绩带来的变化」。
///
/// 它同时也是本界面的本地兜底状态：当没有可上传的成绩时，[`EndingScene::new`] 会用本地数据
/// 自行构造一份（`gain_exp` 为 0、`new_rks` 为 `None`），这样展示逻辑无需区分「上传成功」
/// 与「无需上传」两条路径。
#[derive(Deserialize)]
pub struct RecordUpdateState {
    /// 是否刷新了该谱面的历史最高分。
    pub best: bool,
    /// 相对历史最高分的提升分值；仅在 `best` 为 true 时有意义。
    pub improvement: u32,
    /// 本次获得的经验值；本地兜底状态下为 0。
    pub gain_exp: f32,
    /// 服务端刷新后的 RKS；`None` 表示未变化或未登录（界面沿用旧值）。
    pub new_rks: Option<f32>,
}

/// 结算界面场景。
///
/// 通过 [`EndingScene::new`] 一次性接收本局所需的全部素材与数据（为避免在结算时再去做
/// 磁盘/网络 IO，资源由上游提前准备好并传入），之后只做展示与上传。
pub struct EndingScene {
    // 背景贴图（非 FC 时用于转场填充）。
    background: SafeTexture,
    // 曲绘，用于转场填充与卡片展示。
    illustration: SafeTexture,
    // 玩家头像贴图。
    player: SafeTexture,
    // 8 张评级图标，索引由 judge::icon_index 按分数与是否 FC 决定。
    icons: [SafeTexture; 8],
    // 「重试」按钮图标。
    icon_retry: SafeTexture,
    // 「继续」按钮图标。
    icon_proceed: SafeTexture,
    // 7 张修饰符（mods）图标，索引见 render 中的 active_mod_indices 映射表。
    mod_icons: [SafeTexture; 7],
    // 渲染目标；None 表示直接绘制到窗口。
    target: Option<RenderTarget>,
    // 音频管理器；每帧需调用 recover_if_needed 以应对设备音频中断后恢复的情形。
    audio: AudioManager,
    // 结算 BGM。延迟到时间轴归零后才播放，避免与游玩过程末尾的音乐重叠。
    bgm: Music,

    // 谱面信息（曲名、难度、曲绘作者等）。
    info: ChartInfo,
    // 本局判定统计结果：分数、准度、连击、各档计数与 early/late 细分。
    result: PlayResult,
    // 玩家昵称，用于左上角的名牌。
    player_name: String,
    // 玩家上传前的 RKS；与 update_state.new_rks 相减得到变化量。
    player_rks: Option<f32>,
    // 本局是否为自动演奏；自动演奏的成绩不计入排行，界面会显示 UNRATED。
    autoplay: bool,
    // 本局是否使用键盘输入；使用键盘的成绩同样不计入排行。
    use_keyboard: bool,
    // 本局使用的倍速。非 1.0 倍速会使成绩无效，界面会附加显示倍速。
    speed: f32,
    // 本局启用的修饰符集合，用于展示 mod 图标。
    mods: Mods,
    // 下一步去向：0 表示停留（转场动画中或未做选择），1 表示重试，2 表示继续。
    // 已有的英文注释写作 "exit"，但实际执行的是一次两级出栈（见 next_scene）。
    next: u8, // 0 -> none, 1 -> pop, 2 -> exit
    // 服务端返回（或本地兜底）的成绩变化状态；上传进行中时为 None。
    update_state: Option<RecordUpdateState>,
    // 本局成绩是否已进入计分（上传）流程。为 false 且非自动/键盘时界面显示 UNRATED。
    rated: bool,

    // 成绩上传回调；None 表示该构建不带上传能力（例如离线版）。
    upload_fn: Option<UploadFn>,
    // 进行中的上传任务及其「正在上传」提示消息句柄；两者同生命周期，完成时一并清空。
    upload_task: Option<(Task<Result<RecordUpdateState>>, MessageHandle)>,
    // 待上传的成绩原始字节（由游玩场景编码好）；重试上传时复用它。
    record_data: Option<Vec<u8>>,
    // 本谱面的本地最好成绩。为 Some 时，退出结算会把它随 PopN 一起交回上层。
    best_record: Option<SimpleRecord>,

    // 三个按钮的可点击区域与按压动画状态。
    btn_retry: DRectButton,
    btn_proceed: DRectButton,
    btn_detail: RectButton,
    // 判定明细是否展开（展开后各档位显示 early/late 细分而非总数）。
    detail_mode: bool,

    // 转场动画的起始时刻；NaN 表示当前没有转场（见 next_scene 判定）。
    tr_start: f32,

    // 本局平均帧率；None 表示未统计（界面会隐藏该项）。
    avg_fps: Option<f32>,
}

// 结算场景的构造：把上游准备好的素材与数据装箱，并决定「本局是否要上传成绩」。
impl EndingScene {
    /// 组装结算场景。
    ///
    /// # Arguments
    ///
    /// * `background` / `illustration` / `player` / `icons` / `icon_retry` / `icon_proceed` / `mod_icons`
    ///   - 界面所需的全部贴图，由上游提前加载好一次性传入。
    /// * `info` / `result` - 谱面信息与本局判定结果。
    /// * `config` - 当前配置，用于音量、玩家名、是否自动演奏、倍速与修饰符。
    /// * `bgm` - 结算 BGM 的音频片段（此处按配置音量创建为可播放的 `Music`）。
    /// * `upload_fn` - 上传回调；`None` 表示不参与排行上传。
    /// * `player_rks` - 上传前 RKS，用于计算并显示变化量。
    /// * `historic_best` - 谱面历史最高分，用于在无上传时本地判断是否刷新纪录。
    /// * `record_data` - 已编码的待上传成绩字节；与 `upload_fn` 同时存在才会真正发起上传。
    /// * `best_record` - 更新后的本地最好成绩，退出时随 `PopN` 交回上层。
    /// * `avg_fps` - 本局平均帧率，可选展示。
    ///
    /// # Errors
    ///
    /// 音频管理器或 BGM 创建失败时返回错误（此时结算界面无法正常出声，视为致命）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        background: SafeTexture,
        illustration: SafeTexture,
        player: SafeTexture,
        icons: [SafeTexture; 8],
        icon_retry: SafeTexture,
        icon_proceed: SafeTexture,
        mod_icons: [SafeTexture; 7],
        info: ChartInfo,
        result: PlayResult,
        config: &Config,
        bgm: AudioClip,
        upload_fn: Option<UploadFn>,
        player_rks: Option<f32>,
        historic_best: u32,
        record_data: Option<Vec<u8>>,
        best_record: Option<SimpleRecord>,
        avg_fps: Option<f32>,
    ) -> Result<Self> {
        // BGM 循环混音时间设为 0，是为了让结算曲目从头干净地播放（结算通常只听一小段）。
        let mut audio = create_audio_manger(config)?;
        let bgm = audio.create_music(
            bgm,
            MusicParams {
                amplifier: config.volume_music,
                loop_mix_time: 0.,
                ..Default::default()
            },
        )?;
        // 只有在「有上传回调」且「有成绩数据」时才立即发起上传，并同时弹出
        // 「正在上传」提示；句柄随任务一起保存，任务结束时用于取消该提示。
        let upload_task = upload_fn
            .as_ref()
            .and_then(|f| record_data.clone().map(|data| (f(data), show_message(tl!("uploading")).handle())));
        Ok(Self {
            background,
            illustration,
            player,
            icons,
            icon_retry,
            icon_proceed,
            mod_icons,
            target: None,
            audio,
            bgm,
            // 上传进行中时先不设置状态，等服务器返回后再由 update 填入；
            // 无上传时立即用本地数据构造兜底状态，让界面从第一帧就能显示"是否刷新纪录"。
            update_state: if upload_task.is_some() {
                None
            } else {
                let (best, improvement) = if result.score > historic_best {
                    (true, result.score - historic_best)
                } else {
                    (false, 0)
                };
                Some(RecordUpdateState {
                    best,
                    improvement,
                    gain_exp: 0.,
                    new_rks: None,
                })
            },
            // rated 由「是否处于上传流程」推导：没有上传就没有计分，界面据此显示 UNRATED。
            rated: upload_task.is_some(),

            info,
            result,
            player_name: config.player_name.clone(),
            player_rks,
            autoplay: config.autoplay(),
            use_keyboard: config.use_keyboard,
            speed: config.speed,
            mods: config.mods,
            next: 0,

            upload_fn,
            upload_task,
            record_data,
            best_record,
            detail_mode: false,

            btn_retry: DRectButton::new(),
            btn_proceed: DRectButton::new(),
            btn_detail: RectButton::new(),

            // 用 NaN 作为「无转场动画」的哨兵值：转场开始时写入真实时间，动画播完再置回 NaN，
            // 这样 next_scene 只需判断 is_nan() 就能知道是否可以离场。
            tr_start: f32::NAN,

            avg_fps,
        })
    }
}

// 跨帧传递「玩家在失败对话框里点了重试」这一意图。
// 对话框的回调触发时机与 Scene::update 不在同一处，故用线程本地标志转交。
thread_local! {
    static RE_UPLOAD: RefCell<bool> = RefCell::default();
}

// EndingScene 的场景契约实现：无子场景、无返回值，只负责展示成绩、处理上传与选择去向。
impl Scene for EndingScene {
    /// 重置时间轴并额外回退 0.4 秒作为「预备时间」。
    ///
    /// 回退到负时间有两个作用：一是让各段动画（起始时刻都在 0.1 之后）有一小段静止缓冲，
    /// 玩家刚进入结算时不会立刻看到元素乱飞；二是给 [`Scene::update`] 里的
    /// `tm.now() >= 0.` 判断留出「音乐尚未开始」的窗口，避免切场景瞬间就拉起 BGM。
    fn enter(&mut self, tm: &mut TimeManager, target: Option<RenderTarget>) -> Result<()> {
        tm.reset();
        tm.seek_to(-0.4);
        self.target = target;
        Ok(())
    }

    /// 暂停 BGM 与时间轴（切后台、被覆盖时调用），保证结算音乐不会继续播放。
    fn pause(&mut self, tm: &mut TimeManager) -> Result<()> {
        self.bgm.pause()?;
        tm.pause();
        Ok(())
    }

    /// 恢复 BGM 与时间轴，与 [`pause`](Self::pause) 配对。
    fn resume(&mut self, tm: &mut TimeManager) -> Result<()> {
        self.bgm.play()?;
        tm.resume();
        Ok(())
    }

    /// 处理按钮点击：三个按钮都会「消费」触摸（返回 `true`），避免误触到下层。
    ///
    /// 「重试/继续」在上传未完成时被拒绝，并提示「仍在上传」——这是有意的保护：
    /// 若允许在上传途中离开，成绩可能只上传了一半，且上传任务无人接管。
    fn touch(&mut self, tm: &mut TimeManager, touch: &Touch) -> Result<bool> {
        let t = tm.now() as f32;
        if self.btn_retry.touch(touch, t) {
            if self.upload_task.is_some() {
                show_message(tl!("still-uploading"));
            } else {
                self.tr_start = t;
                self.next = 1;
            }
            return Ok(true);
        }
        if self.btn_proceed.touch(touch, t) {
            if self.upload_task.is_some() {
                show_message(tl!("still-uploading"));
            } else {
                self.tr_start = t;
                self.next = 2;
            }
            return Ok(true);
        }
        // 明细按钮是纯展示开关，随时可用（上传中也能查看），点击即切换展开状态。
        if self.btn_detail.touch(touch) {
            button_hit();
            self.detail_mode = !self.detail_mode;
            return Ok(true);
        }
        Ok(false)
    }

    /// 每帧推进：恢复音频设备、按需播放 BGM、驱动上传任务的状态机。
    ///
    /// 上传在这里被实现成一个跨帧状态机，因为其生命周期跨越 `new`、`touch` 与对话框回调三处：
    /// - 首次上传：在 [`EndingScene::new`] 中发起；
    /// - 重试上传：由失败对话框的回调置位 `RE_UPLOAD`，本方法下一帧检测到后重新发起；
    /// - 完成/失败：都在这里统一处理并清理 `upload_task`。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        // 设备音频可能被系统中断（来电、其他应用抢占），需要每帧检查并重建播放器。
        self.audio.recover_if_needed()?;
        // 延迟播放 BGM 的时机被三重限定：时间轴已进入正片、当前是直接上屏渲染（离屏渲染时
        // 不发声，避免宿主录制/嵌套渲染时重复出声）、且音乐尚未在播放。
        if tm.now() >= 0. && self.target.is_none() && self.bgm.paused() {
            self.bgm.play()?;
        }
        // 处理「重试上传」意图：读完即清位（replace 成 false），保证一次点击只重试一次。
        // 此时 upload_task 必为 None（失败时已被清空），避免与进行中的任务冲突。
        if RE_UPLOAD.with(|it| std::mem::replace(it.borrow_mut().deref_mut(), false)) && self.upload_task.is_none() {
            // 这里 unwrap upload_fn 是安全的：RE_UPLOAD 只能由「上传失败对话框」的回调置位，
            // 而那个对话框的前提是曾经存在 upload_fn。
            self.upload_task = self
                .record_data
                .clone()
                .map(|data| ((self.upload_fn.as_ref().unwrap())(data), show_message(tl!("uploading")).handle()));
        }
        if let Some((task, handle)) = &mut self.upload_task {
            // take 语义：任务只被消费一次，取到结果即代表本次上传结束。
            if let Some(result) = task.take() {
                // 先收掉「正在上传」提示，再根据结果决定展示成功提示还是失败对话框。
                handle.cancel();
                match result {
                    Err(err) => {
                        // 失败对话框提供「取消 / 重试」两个按钮；listener 返回 false 表示
                        // 不关闭对话框（这里由按钮自身逻辑决定关闭），索引 1 即「重试」。
                        let error = format!("{:?}", err.context(tl!("upload-failed")));
                        Dialog::plain(tl!("upload-failed"), error)
                            .buttons(vec![tl!("upload-cancel").to_string(), tl!("upload-retry").to_string()])
                            .listener(move |_dialog, pos| {
                                if pos == 1 {
                                    RE_UPLOAD.with(|it| *it.borrow_mut() = true);
                                }
                                false
                            })
                            .show();
                    }
                    Ok(state) => {
                        // 上传成功后用服务端返回的状态替换本地兜底状态，使刷新纪录/RKS 变化
                        // 以服务器结果为准。
                        self.update_state = Some(state);
                        show_message(tl!("uploaded")).ok();
                    }
                }
                self.upload_task = None;
            }
        }
        Ok(())
    }

    /// 绘制结算界面。
    ///
    /// 整幅画面由三层构成，自后向前依次是：模糊背景、以「扇形扫过」方式露出的曲绘、
    /// 以及成绩明细面板（仅在扇形扫到足够位置后出现）。面板内所有元素都做了随时间推进的
    /// 入场动画，`time` 取负值时这些动画全部处于起始状态。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        // 手动设置相机以指定渲染目标；`top` 是纵向半高，用于把 UI 坐标对齐到屏幕顶端/底端。
        let mut cam = ui.camera();
        let asp = -cam.zoom.y;
        let top = 1. / asp;
        let t = tm.now() as f32;
        cam.render_target = self.target;
        let sr = ui.screen_rect();
        set_camera(&cam);
        draw_background(*self.background);

        // 把时间映射到 [0,1] 的线性进度并夹紧：l 为动画起始时刻，r 为结束时刻。
        // 定义在函数内部是因为只服务于本方法内的入场动画，不对外复用。
        fn ran(t: f32, l: f32, r: f32) -> f32 {
            ((t - l) / (r - l)).clamp(0., 1.)
        }

        // 扇形扫过的几何定义：以 ct 为圆心，从 start 方向扫到 end 方向，张角为 center_angle。
        // 角度按 0.4 倍缩放是为了让起始角度小一些，使扇形最初几乎贴着屏幕外侧、不遮挡内容。
        let ct = vec2(-0.55, 1.2);
        let start = vec2(1.25, 0.9) - ct;
        let end = vec2(-0.15, -0.7) - ct;
        let angle_start = start.y.atan2(start.x) * 0.4;
        let angle_end = end.y.atan2(end.x);
        let center_angle = 1.8;

        // p 为扇形扫掠进度（缓出），sector_start 是扇形前缘的起始角；
        // project_y 是扇形前缘在屏幕右边界处的投影纵坐标，用来判断"扇形是否已扫入可视区"。
        let p = ran(t, 0.1, 1.8);
        let p = 1. - (1. - p).powi(3);
        let sector_start = p * (angle_end - angle_start - center_angle) + angle_start;
        let project_y = ct.y + (1. - ct.x) * (sector_start + center_angle).sin();

        // 面板整体的淡入进度，2.0~2.4 秒之间完成，比扇形晚一些，形成层次。
        let pf = ran(t, 2., 2.4);

        // 只有扇形扫掠到可视区域后（前缘投影进入屏幕内）才绘制成绩面板，
        // 否则面板会"凭空出现"在曲绘之上，破坏扫掠转场的连贯性。
        if project_y < top {
            // 顶部横带：作为「明细」按钮的底衬，用渐变让下边缘自然淡出，
            // 避免出现一条生硬的矩形边界。
            let c = ui.background();
            let y = -top + 0.12;
            let br = Rect::new(-1., y, 2., 0.34);
            ui.fill_rect(br, (c, (-1., y), Color { a: 0.1, ..c }, (1., y + 0.3)));

            // 「明细」按钮：文字颜色随展开状态变化，点击热区由 btn_detail 记录。
            let r = ui
                .text(tl!("detail"))
                .pos(1. - 0.02, br.bottom() + 0.02)
                .anchor(1., 0.)
                .size(0.5)
                .color(if self.detail_mode { semi_white(0.4) } else { WHITE })
                .draw_using(&BOLD_FONT);
            self.btn_detail.set(ui, r.feather(0.02));

            let res = &self.result;

            // 曲名与难度一行；注意 x 坐标里的 `(1.2 - y) / 1.9 * 0.4` —— 面板整体是斜切的，
            // 所有元素都要按 y 做同样的水平偏移，否则会出现"没对齐"的观感。
            let y = y - 0.07;
            ui.fill_rect(Rect::new(-1., y, 2., 0.07), Color { a: 0.3, ..c });
            let r = ui
                .text(&self.info.name)
                .pos(-0.53 + (1.2 - y) / 1.9 * 0.4, y + 0.012)
                .color(semi_white(0.6))
                .max_width(0.8)
                .size(0.56)
                .draw();
            ui.text(&self.info.level)
                .pos(0.97, r.y)
                .anchor(1., 0.)
                .size(0.56)
                .color(semi_white(0.7))
                .draw();

            // 评级图标由 icon_index 依据分数与「是否 FC」共同决定（FC 另有专属图标）。
            // 动画用平方缓出，且羽化半径随进度收缩 —— 形成图标"冲入并聚焦"的效果。
            let icon = &self.icons[icon_index(res.score, res.max_combo == res.num_of_notes)];
            let p = ran(t, 1.7, 2.4).powi(2);
            let r = Rect::new(0.75, br.center().y, 0., 0.).feather(0.13 + (1. - p) * 0.05);
            ui.fill_rect(r, (**icon, r, ScaleType::Fit, semi_white(p)));

            // 分数滚动：把分数拆成固定 7 位十进制（高位补 0）逐位从下方滚上来。
            // 每位数字同时绘制「当前数字」与「下一位数字」并按小数部分偏移，
            // 因此视觉上是连续滚动而非跳变；位数靠左到右的索引给不同的延迟，形成波次感。
            let y = y + 0.16;
            let lf = -0.48 + (1.2 - y) / 1.9 * 0.4;
            let mut x = lf;
            let p = ran(t, 0.9, 2.6);
            let mut digits = Vec::with_capacity(7);
            let mut s = res.score;
            for _ in 0..7 {
                digits.push(s % 10);
                s /= 10;
            }
            digits.reverse();
            let s = 1.5;
            let sr = ui.text("0").size(s).measure_using(&PGR_FONT);
            let h = sr.h;
            ui.scissor(Rect::new(-1., y, 2., h + 0.01), |ui| {
                for (i, d) in digits.into_iter().enumerate() {
                    let p = (p * (1. + (0.16 * (6 - i) as f32).powi(2))).min(1.);
                    let p = 1. - (1. - p).powi(3);
                    let mut p = d as f32 + (1. - p) * 7.;
                    if p > 10. {
                        p -= 10.;
                    }
                    let up = p as u32;
                    let dw = (up + 1) % 10;
                    let o = -h * (p - up as f32);
                    ui.text(up.to_string())
                        .pos(x + sr.w / 2., y + o)
                        .anchor(0.5, 0.)
                        .size(s)
                        .draw_using(&PGR_FONT);
                    ui.text(dw.to_string())
                        .pos(x + sr.w / 2., y + h + o)
                        .anchor(0.5, 0.)
                        .size(s)
                        .draw_using(&PGR_FONT);
                    x += sr.w;
                }
            });

            // 刷新纪录时在分数右侧叠加「NEW BEST +提升分」。`{:+07}` 让提升分固定显示
            // 7 位并带正负号，保证多条记录的数值能竖向对齐。没有刷新纪录则完全不显示。
            if let Some(s) = &self.update_state {
                if s.best {
                    ui.text(format!("{}  {:+07}", tl!("new-best"), s.improvement))
                        .pos(x - 0.01, y - 0.016)
                        .anchor(1., 1.)
                        .color(semi_white(pf))
                        .size(0.5)
                        .draw_using(&BOLD_FONT);
                }
            }

            // 单行多段文本的排版手法：每段都以上一段返回的矩形右侧为起点继续排布，
            // 因此不需要手工累加字宽；三种灰度分别用于标签、数值与「|」分隔符。
            let cl = semi_white(0.6);
            let ct = semi_white(0.8);
            let cs = semi_white(0.4);
            let s = 0.5;

            let r = ui
                .text(tl!("accuracy"))
                .pos(lf - 0.017, y + h + 0.03)
                .color(cl)
                .size(s)
                .draw_using(&BOLD_FONT);
            let r = ui
                .text(format!("{:.2}%", res.accuracy * 100.))
                .pos(r.right() + 0.02, r.y)
                .color(ct)
                .size(s)
                .draw_using(&BOLD_FONT);

            let r = ui.text("|").pos(r.right() + 0.03, r.y).color(cs).size(s).draw();

            let r = ui.text(tl!("error")).pos(r.right() + 0.03, r.y).color(cl).size(s).draw_using(&BOLD_FONT);
            let r = ui
                .text(format!("±{}ms", (res.std * 1000.).round() as i32))
                .pos(r.right() + 0.02, r.y)
                .size(s)
                .color(ct)
                .draw_using(&BOLD_FONT);

            // 平均帧率是可选诊断信息，只有上游统计了才展示，避免显示无意义的占位值。
            if let Some(avg_fps) = self.avg_fps {
                let r = ui.text("|").pos(r.right() + 0.03, r.y).color(cs).size(s).draw();
                let r = ui.text("AVG FPS").pos(r.right() + 0.03, r.y).color(cl).size(s).draw_using(&BOLD_FONT);
                ui.text(format!("{:.1}", avg_fps))
                    .pos(r.right() + 0.02, r.y)
                    .size(s)
                    .color(ct)
                    .draw_using(&BOLD_FONT);
            }

            // 四个判定档位纵向罗列。每行都把 y 增大 dy、同时把 x 左移 dy/1.9*0.4，
            // 与前面面板的斜切保持一致，使整块统计区看起来贴合在同一条斜面上。
            let mut y = -top + 0.4 + ui.top * 0.3;
            let tp = y;
            let mut x = -0.26 + (1.2 - y) / 1.9 * 0.4;
            let lf = x;
            let s = 0.64;
            for (id, title) in ["PERFECT", "GOOD", "BAD", "MISS"].into_iter().enumerate() {
                ui.text(title)
                    .pos(x, y)
                    .anchor(1., 0.)
                    .color(semi_white(0.6))
                    .size(s)
                    .draw_using(&BOLD_FONT);
                // 展开明细时显示 early/late 细分：拖早为蓝色、拖晚为橙色。
                // 排除 id==3（MISS）：MISS 没有早/晚方向之分，只能显示总数。
                let r = if self.detail_mode && id != 3 {
                    let r = ui
                        .text(format!("-{}", res.early_kind[id]))
                        .pos(x + 0.03, y)
                        .size(s)
                        .color(Color::from_hex_rgb(0x81d4fa))
                        .draw_using(&BOLD_FONT);
                    ui.text(format!("+{}", res.late_kind[id]))
                        .pos(r.right() + 0.01, y)
                        .size(s)
                        .color(Color::from_hex_rgb(0xffab91))
                        .draw_using(&BOLD_FONT)
                } else {
                    ui.text(res.counts[id].to_string()).pos(x + 0.06, y).size(s).draw_using(&BOLD_FONT)
                };
                let dy = r.h + 0.03;
                y += dy;
                x -= dy / 1.9 * 0.4;
            }

            // 最大连击进度条。`draw_par` 画一个平行四边形（斜边斜率与面板斜切一致）：
            // 先按 p 比例填充颜色 c，顶点在超过右边界时再补一个三角形把斜边收口。
            // 之所以用平行四边形而不是矩形，是为了让进度条与整块面板的透视方向统一。
            let p = ran(t, 0.8, 1.8);
            let p = 1. - (1. - p).powi(3);
            let mut y = tp;
            let mut x = lf + 0.42;
            let r = ui
                .text(tl!("max-combo"))
                .pos(x, y)
                .anchor(1., 0.)
                .color(semi_white(0.6))
                .size(s)
                .draw_using(&BOLD_FONT);
            let mut r = Rect::new(r.right() + 0.03, r.y + 0.004, 0.45, r.h);
            let draw_par = |ui: &mut Ui, r: Rect, p: f32, c: Color| {
                let sl = 1.9 / 0.4;
                let w = p * r.w;
                let d = r.h / sl;
                let mut b = ui.builder(c);
                b.add(r.x, r.bottom());
                if w < d {
                    b.add(r.x + w, r.bottom());
                    b.add(r.x + w, r.bottom() - w * sl);
                    b.triangle(0, 1, 2);
                } else {
                    b.add(r.x + d, r.y);
                    b.add(r.x + w, r.y);
                    b.add(r.x + w.min(r.w - d), r.bottom());
                    b.triangle(0, 1, 2);
                    b.triangle(0, 2, 3);
                    if w + d > r.right() {
                        b.add(r.x + w, r.y + (r.w - w) * sl);
                        b.triangle(2, 3, 4);
                    }
                }
                b.commit();
            };
            // 底条固定铺满（暗色），作为进度条的"槽"。
            draw_par(ui, r, 1., semi_black(0.4));
            let ct = r.center();
            // 连击数同样随入场动画从 0 增长到实际值，避免数字突然跳出。
            let combo = (res.max_combo as f32 * p).round() as u32;
            let text = format!("{combo} / {}", res.num_of_notes);
            ui.text(&text)
                .pos(ct.x, ct.y)
                .anchor(0.5, 0.5)
                .no_baseline()
                .size(0.4)
                .draw_using(&BOLD_FONT);
            // 前景进度条按当前连击比例绘制；再把矩形宽度收窄到该比例，并在同一区域内
            // 用黑色重绘一遍文本——这样文字越过进度条边界时会自动"反色"，可读性最好。
            let p = combo as f32 / res.num_of_notes as f32;
            draw_par(ui, r, p, WHITE);
            r.w *= p;
            ui.scissor(r, |ui| {
                ui.text(text)
                    .pos(ct.x, ct.y)
                    .anchor(0.5, 0.5)
                    .no_baseline()
                    .size(0.4)
                    .color(BLACK)
                    .draw_using(&BOLD_FONT);
            });

            let dy = r.h + 0.03;
            y += dy;
            x -= dy / 1.9 * 0.4;

            // RKS 变化量：只有新旧值都存在时才能算出差值（未登录或服务端未返回则显示 "-"）。
            let r = ui
                .text(tl!("rks-delta"))
                .pos(x, y)
                .anchor(1., 0.)
                .color(semi_white(0.6))
                .size(s)
                .draw_using(&BOLD_FONT);
            // 用 1e-5 的容差判断"是否真的变化"：RKS 是浮点数，微小误差不应显示成 "+0.00"。
            let text = if let Some((new_rks, now)) = self.update_state.as_ref().and_then(|it| it.new_rks).zip(self.player_rks) {
                let delta = new_rks - now;
                if delta.abs() > 1e-5 {
                    format!("{:+.2}", delta)
                } else {
                    "-".to_owned()
                }
            } else {
                "-".to_owned()
            };
            ui.text(text).pos(r.right() + 0.03, y).size(s).draw_using(&BOLD_FONT);

            // 右下角按钮区：从屏幕右下角出发，先放「继续」，再把 x 左移放「重试」，
            // 因此视觉顺序是「重试 | 继续」，但代码里是先定位"主按钮"再推导次按钮。
            let mut r = Rect::new(0.96, ui.top - 0.04, 0.25, 0.1);
            r.x -= r.w;
            r.y -= r.h;
            self.btn_proceed.render_shadow(ui, r, t, |ui, path| {
                ui.fill_path(&path, Color::from_hex_rgb(0x3f51b5));
                let ir = Rect::new(r.x + 0.05, r.center().y, 0., 0.).feather(0.03);
                ui.fill_rect(ir, (*self.icon_proceed, ir));
                ui.text(tl!("proceed"))
                    .pos((ir.right() + r.right() - 0.01) / 2., r.center().y)
                    .anchor(0.5, 0.5)
                    .no_baseline()
                    .size(0.44)
                    .draw_using(&BOLD_FONT);
            });

            r.x -= r.w + 0.02;
            self.btn_retry.render_shadow(ui, r, t, |ui, path| {
                ui.fill_path(&path, Color::from_hex_rgb(0x78909c));
                let ir = Rect::new(r.x + 0.05, r.center().y, 0., 0.).feather(0.03);
                ui.fill_rect(ir, (*self.icon_retry, ir));
                ui.text(tl!("retry"))
                    .pos((ir.right() + r.right() - 0.01) / 2., r.center().y)
                    .anchor(0.5, 0.5)
                    .no_baseline()
                    .size(0.44)
                    .draw_using(&BOLD_FONT);
            });

            // 倍速文案：1.0 倍速（容差 1e-4）不显示，避免状态区出现无意义信息。
            let spd = if (self.speed - 1.).abs() <= 1e-4 {
                String::new()
            } else {
                format!("{:.2}x", self.speed)
            };
            // 状态文案的两级拼接：未计分（没走上传流程、且非自动、非键盘）时打上 UNRATED 标记，
            // 若同时还有倍速则一并附上，因为这两者都是「成绩无效」的说明。
            let status_text = if !self.rated && !self.autoplay && !self.use_keyboard {
                if spd.is_empty() {
                    "UNRATED".to_string()
                } else {
                    format!("UNRATED {spd}")
                }
            } else {
                spd
            };
            let status_text = status_text.trim();
            // mod_icons order: FLIP_X, FADE_OUT, FADE_IN, NIGHTCORE, RAINBOW
            // 该数组共 7 个元素：前 5 个对应下方列表中显式列出的 5 个视觉类修饰符，
            // 索引 5/6 分别对应 AUTOPLAY 与 NO_SHADER（这两个共用后两张图标）。
            // 顺序必须与 mod_icons 资源的实际排布一致，否则会张冠李戴。
            let active_mod_indices: Vec<usize> = [
                (Mods::FLIP_X, 0),
                (Mods::FADE_OUT, 1),
                (Mods::FADE_IN, 2),
                (Mods::NIGHTCORE, 3),
                (Mods::RAINBOW, 4),
                (Mods::AUTOPLAY, 5),
                (Mods::NO_SHADER, 6),
            ]
            .into_iter()
            .filter(|(m, _)| self.mods.contains(*m))
            .map(|(_, idx)| idx)
            .collect();
            // 状态区贴着明细条底部排布；skew_factor 即面板的斜切系数，用于把矩形切成平行四边形。
            let ty = br.bottom();
            let base_x = -0.55 + (1.2 - ty) / 1.9 * 0.4;
            let skew_factor = 0.4 / 1.9;
            let has_text = !status_text.is_empty();
            let has_icons = !active_mod_indices.is_empty();
            // 两者都为空时整块区域跳过绘制，避免画出没有内容的空标签牌。
            if has_text || has_icons {
                let text_size = 0.5;
                let skew_height_ratio = skew_factor;
                let mut current_x = base_x;
                let para_h = 0.04;
                if has_text {
                    // 文字标签牌：先画一条从左屏幕边缘延伸到文字右侧的白色斜条（几何体），
                    // 再在其上绘制半透明黑字，形成深色字压在浅色斜条上的可读效果。
                    let mut text = ui
                        .text(status_text)
                        .pos(current_x + 0.02, ty)
                        .anchor(0., 0.5)
                        .no_baseline()
                        .color(semi_black(0.6))
                        .size(text_size);
                    let tr = text.measure_using(&BOLD_FONT);
                    let r = Rect::new(-1., tr.y, tr.right() + 1.03, tr.h);
                    let mut b = text.ui.builder(WHITE);
                    b.add(-1., tr.y);
                    b.add(r.right(), tr.y);
                    b.add(r.right() - tr.h * skew_height_ratio, tr.bottom());
                    b.add(-1., tr.bottom());
                    b.triangle(0, 1, 2);
                    b.triangle(0, 2, 3);
                    b.commit();

                    text.draw_using(&BOLD_FONT);
                    current_x = tr.right() + 0.04;
                }
                // 每个已启用的 mod 都画一个平行四边形色块并居中放图标，块间留 0.02 间距；
                // 斜边偏移按块高的一半换算，保证与文字标签牌使用同一透视。
                for &mod_idx in &active_mod_indices {
                    let icon_size = para_h * 0.9;
                    let para_w = para_h + 0.02;
                    let skew_offset = para_h * skew_height_ratio;
                    let para_left = current_x;
                    let para_right = current_x + para_w;
                    let para_top = ty - para_h / 2.;
                    let para_bottom = ty + para_h / 2.;
                    let mut b = ui.builder(WHITE);
                    b.add(para_left + skew_offset, para_top);
                    b.add(para_right + skew_offset, para_top);
                    b.add(para_right, para_bottom);
                    b.add(para_left, para_bottom);
                    b.triangle(0, 1, 2);
                    b.triangle(0, 2, 3);
                    b.commit();
                    let icon_x = current_x + (para_w - icon_size) / 2. + skew_offset / 2.;
                    let icon_y = ty - icon_size / 2.;
                    let icon_rect = Rect::new(icon_x, icon_y, icon_size, icon_size);
                    ui.fill_rect(icon_rect, (*self.mod_icons[mod_idx], icon_rect, ScaleType::Fit, semi_black(0.6)));
                    current_x = para_right + 0.02;
                }
            }
        }
        // 曲绘露出的方式：用扇形裁剪（clip_sector）把曲绘"扫"进画面。
        // 先画主扇形（前缘为 sector_start，张角 center_angle），再用第二个扇形叠一层：
        // 它的前缘更靠前（p * 1.4 - 0.3）、张角只有一半，且带 0.15 的羽化，
        // 从而在主扇形边缘形成一圈柔和的过渡带，避免硬切边。
        clip_sector(ui, ct, sector_start, sector_start + center_angle, |ui| {
            ui.fill_rect(sr, (*self.illustration, sr));
        });
        let sector_start = (p * 1.4 - 0.3).max(0.) * (angle_end - angle_start - center_angle) + angle_start;
        clip_sector(ui, ct, sector_start, sector_start + center_angle * 0.5, |ui| {
            ui.fill_rect(sr, (*self.illustration, sr.feather(0.15)));
        });

        // 左上角玩家名牌：宽度由「两倍头像半径 + 内边距 + 昵称宽度（上限 mw） + 右边距」算出，
        // 昵称过长时截断而不撑破名牌。整块随 pf 淡入，与成绩面板同步。
        ui.alpha(pf, |ui| {
            let s = 0.05;
            let pad = 0.02;
            let mw = 0.4;
            let w = s * 2. + pad + ui.text(&self.player_name).size(0.6).measure().w.min(mw) + 0.02;
            let r = Rect::new(-0.96, -top + 0.04, w, s * 2.);
            ui.fill_path(&r.feather(0.01).rounded(s + 0.01), semi_black(0.6));
            // 复用场景时间 t 驱动头像动画（加载态/按压反馈），使头像与界面节奏一致。
            ui.avatar(r.x + s, r.y + s, s, t, Ok(Some(self.player.clone())));
            let lf = r.x + s * 2. + pad;
            ui.text(&self.player_name).pos(lf, r.y + s).anchor(0., 1.).max_width(mw).size(0.6).draw();
            // RKS 优先显示服务端刷新后的新值，其次退回上传前的旧值；都没有则留空（未登录）。
            ui.text(if let Some(new_rks) = self.update_state.as_ref().and_then(|it| it.new_rks) {
                format!("{new_rks:.2}")
            } else if let Some(rks) = &self.player_rks {
                format!("{rks:.2}")
            } else {
                String::new()
            })
            .pos(lf, r.y + s + 0.008)
            .size(0.4)
            .color(semi_white(0.7))
            .draw();
        });

        // 退场覆盖层：0.5 秒内从屏幕下方滑上一块矩形把画面盖住。
        // 覆盖层贴图用「重试 = 背景图 + 30% 黑 / 继续 = 曲绘 + 55% 黑」给出下一步去处的视觉暗示。
        // 动画播到 100%（p >= 1）即把 tr_start 置回 NaN，next_scene 由此才知道可以真正切换场景。
        if !self.tr_start.is_nan() {
            let p = ((t - self.tr_start) / 0.5).min(1.);
            if p >= 1. {
                self.tr_start = f32::NAN;
            }
            let p = 1. - (1. - p).powi(3);
            let mut r = sr;
            r.y -= r.h * (1. - p);
            rect_shadow(r, 0.01, 0.5);
            let (tex, alpha) = if self.next == 1 {
                (&self.background, 0.3)
            } else {
                (&self.illustration, 0.55)
            };
            ui.fill_rect(r, (**tex, r));
            ui.fill_rect(r, semi_black(alpha));
        }

        Ok(())
    }

    /// 把玩家的选择转成场景切换意向。
    ///
    /// 关键约束：转场动画未播完（`tr_start` 非 NaN）时一律返回 [`NextScene::None`]，
    /// 这样切场景只会发生在覆盖层完全盖住画面之后，不会出现"看到一半就跳屏"。
    ///
    /// 两种去向都先暂停 BGM：结算音乐不应延续到下一个场景里。
    /// 「继续」用 [`NextScene::PopNWithResult`] 一次弹出两层（结算与游玩），并把更新后的最好成绩
    /// 交给下层（选曲界面），让它刷新列表中的分数；没有最好成绩时退化为普通的两级出栈。
    fn next_scene(&mut self, _tm: &mut TimeManager) -> NextScene {
        if !self.tr_start.is_nan() {
            return NextScene::None;
        }
        if self.next != 0 {
            let _ = self.bgm.pause();
        }
        match self.next {
            0 => NextScene::None,
            1 => NextScene::Pop,
            2 => {
                if let Some(rec) = &self.best_record {
                    NextScene::PopNWithResult(2, Box::new(rec.clone()))
                } else {
                    NextScene::PopN(2)
                }
            }
            _ => unreachable!(),
        }
    }
}
