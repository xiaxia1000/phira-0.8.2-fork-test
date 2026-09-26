//! 曲目解锁动画场景（[`UnlockScene`]）。
//!
//! 首次游玩带解锁视频的谱面时，先用本场景播放「解锁」视频与配套音频，播完再无缝
//! 切到真正的游戏场景。它并不是一个独立页面，而是作为 `NextScene::Overlay`
//! 盖在加载流程之上，因此进入/退出的动画完全由本场景自己掌控。
//!
//! 本模块**仅在启用 `video` feature 时编译**（见 `scene.rs` 的 `#[cfg(feature = "video")]`
//! 与 `scene/song.rs` 的分支）：视频解码依赖 `prpr` 的 `prpr-avc`/FFmpeg 链路，
//! 部分平台无法携带，因此这些平台上会退回普通 `LoadingScene` 并打一条 warn 日志。
//!
//! 场景内部是一个显式状态机 [`State`]：`Before → Playing → Blanking → Loading →
//! Transforming`，最后用 `NextScene::Replace` 顶替掉自身——之所以用 `Replace` 而不是
//! `Push`，是为了让玩家返回时**不会回到本场景**（否则会重复播放一遍解锁视频）。

use anyhow::{bail, Context, Result};
use macroquad::prelude::*;
use prpr::{
    config::Config,
    core::{demux_audio, Anim, Keyframe, Video},
    ext::{create_audio_manger, semi_black, semi_white, SafeTexture, ScaleType},
    fs::FileSystem,
    info::ChartInfo,
    scene::{BasicPlayer, GameMode, LoadingScene, NextScene, SaveFn, Scene, UpdateFn, UploadFn},
    time::TimeManager,
    ui::LoadingParams,
};
use sasa::{AudioClip, AudioManager, Music, MusicParams};

/// 解锁动画的状态机相位。转移只发生在 [`Scene::update`] 里，且每进入一个新相位都会
/// `tm.reset()`（`Transforming` 除外，见下），因此各相位内的 `t` 都从 0 起算。
enum State {
    /// 静场等待：先黑屏停 0.5 秒，让上一层场景的过渡动画播完，再起播视频/音频。
    Before,
    /// 播放中：视频与音频同步推进（以音频播放位置为时间基准），直到两者都放完。
    Playing,
    /// 黑屏收尾：等待 1 秒且游戏场景就绪后立刻切走；若尚未就绪，则转入 `Loading`
    /// 并重置时间轴。因此该相位通常只停留一帧，真正等待的是 `Loading`。
    Blanking,
    /// 加载中：显示加载圈，等游戏场景就绪且已停留超过 1 秒。
    Loading,
    /// 收尾过渡：加载圈淡出、曲绘淡入，随后用 `NextScene::Replace` 交出游戏场景。
    /// 此相位不再重置时间轴，所以会紧接着 `Loading` 的计时继续前进。
    Transforming,
}

/// 解锁视频的音频部分。
///
/// 两个字段必须**绑定在一起持有**：`AudioManager` 是音频后端的句柄，`Music` 依赖它
/// 存活，单独丢掉任一方都会导致播放中断或资源提前释放。
struct Bgm {
    /// 音频后端句柄，用于创建/回收 Music，并在必要时从设备丢失中恢复。
    audio_manager: AudioManager,
    /// 解锁视频的音轨实例。
    music: Music,
}

/// 曲目解锁动画场景。
///
/// 构造时即完成视频解码与音频解复用，随后在 `update` 中推进一个五相状态机；
/// 同时并行驱动 [`LoadingScene`]，等它在后台把游戏场景加载好。
pub struct UnlockScene {
    /// 真正负责加载谱面与资源的加载场景，与解锁动画并行推进。
    loading_scene: Box<LoadingScene>,
    /// 加载完成后得到的游戏场景。它一旦就绪就会被 `take()` 走并交给 `NextScene::Replace`，
    /// 因此本字段同时充当「游戏是否已准备好」的判据。
    game_scene: Option<Box<dyn Scene>>,
    /// 排队中的场景切换（仅退出时使用）。
    next_scene: Option<NextScene>,

    /// 由 `enter` 传入的离屏渲染目标；本场景作为 Overlay 时可能需要渲染到指定目标。
    render_target: Option<RenderTarget>,
    /// 已解码的解锁视频。
    video: Video,
    /// 视频音轨对应的音频；视频不带音轨时为 `None`，此时播放时长完全由视频决定。
    bgm: Option<Bgm>,
    /// 音轨时长（秒）。无音轨时为 0，用于判断「音频是否也放完了」。
    music_length: f64,

    /// 曲绘纹理，作为 `Transforming` 阶段淡入的背景（此时视频已结束，画面不能留黑）。
    background: SafeTexture,

    /// 当前相位。
    state: State,
}

// 构造阶段：本场景的构造是「重」的——它当场完成视频解码与音频解复用，
// 因此只应在确实要播放解锁动画时调用（`scene/song.rs` 里判定了 `played_unlock`
// 与 `feature = "video"` 之后才走到这里）。构造完成后场景起始于 `State::Before`。
impl UnlockScene {
    /// 加载并解码解锁视频、准备音频，同时把真正的加载工作交给 [`LoadingScene`]。
    ///
    /// 参数几乎原样透传给 [`LoadingScene::new`]（`mode`/`info`/`config`/`fs`/`player`/
    /// `upload_fn`/`update_fn`/`save_fn`/`preloaded`），因为解锁动画结束后要接着走
    /// 常规的加载与游玩流程；`preloaded` 是上层已经备好的曲绘与背景色，传进来可以
    /// 避免重复解码。
    ///
    /// # Errors
    /// 下列任一环节失败都会向上抛出，并最终使本次启动失败（不会静默跳过动画）：
    /// - 谱面文件系统里找不到解锁视频（`unlock_video` 指定或默认 `unlock.mp4`）；
    /// - 视频解码失败；
    /// - 音频设备初始化或音轨创建失败；
    /// - 曲绘等资源加载失败。
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        mode: GameMode,
        info: ChartInfo,
        config: Config,
        mut fs: Box<dyn FileSystem>,
        player: Option<BasicPlayer>,
        upload_fn: Option<UploadFn>,
        update_fn: Option<UpdateFn>,
        save_fn: Option<SaveFn>,
        preloaded: Option<(prpr::ext::SafeTexture, prpr::ext::SafeTexture, Color)>,
    ) -> Result<UnlockScene> {
        // 阶段一：取视频字节流。文件名来自谱面自带的 `unlock_video` 字段，缺省为
        // `unlock.mp4`；读取走该谱面自己的 FileSystem，所以本地谱面是磁盘文件，
        // 内置曲目则是虚拟文件系统里的伪路径。
        let bytes = fs
            .load_file(&info.unlock_video.clone().unwrap_or_else(|| "unlock.mp4".to_owned()))
            .await
            .context("Cannot find unlock video file!")?;
        // 阶段二：解码视频。起点 0 秒、按 `Inside` 缩放适配，配一条淡入动画（黑场→画面）
        // 与默认的退场动画；此处失败会连带上面的 context 一起上报。
        let video = Video::new(bytes, 0., ScaleType::Inside, Anim::new(vec![Keyframe::new(0., 1., 0)]), Anim::default())?;
        // 阶段三：解复用音轨。`demux_audio` 需要视频的落地路径（解码器会先把字节流写到
        // 临时文件），因此这里直接取 `video_file().path()`。返回 `None` 表示容器里没有
        // 音轨，此时音频相关逻辑整体降级：`music_length` 记 0，`bgm` 为 None，
        // 播放是否结束只看视频时长。
        let clip = demux_audio(video.video_file().path().to_str().unwrap())?;
        let music_length = clip.as_ref().map_or(0., AudioClip::length);

        // 阶段四：按玩家配置的音乐音量建音频实例。音量取 `config.volume_music`，
        // 与游玩内保持同一口径；`AudioManager` 必须与 `Music` 一起存活，故打包进 `Bgm`。
        let bgm = match clip {
            Some(clip) => {
                let mut audio_manager = create_audio_manger(&config)?;
                let music = audio_manager.create_music(
                    clip,
                    MusicParams {
                        amplifier: config.volume_music,
                        ..Default::default()
                    },
                )?;
                Some(Bgm { audio_manager, music })
            }
            None => None,
        };

        // 阶段五：准备背景图与加载场景。`preloaded` 命中时直接复用上层的纹理（只取其中
        // 的插图，另两项此处用不到），否则自己加载 `info.illustration`。
        // 注意 `fs`/`info`/`config` 等所有权自此转移给 `LoadingScene`，本场景之后
        // 无法再访问它们——这也是解锁动画只能靠构造期拿到的数据推进的原因。
        let (_, background, _) = preloaded.clone().unwrap_or(LoadingScene::load(&mut *fs, &info.illustration).await?);
        let loading_scene = Box::new(LoadingScene::new(mode, info, config, fs, player, upload_fn, update_fn, save_fn, preloaded).await?);

        Ok(UnlockScene {
            loading_scene,
            next_scene: None,
            game_scene: None,

            render_target: None,
            video,
            bgm,
            music_length,

            background,

            state: State::Before,
        })
    }
}

// 本场景的生命周期与其它场景有两点关键差异：
// - 它内部**寄生着**一个 `LoadingScene`，并且两者共用同一个 `TimeManager`：本场景在
//   状态机里调用 `tm.reset()`，会同时改变加载场景看到的时间，因此加载场景的动画进度
//   实际上被解锁动画的相位节奏牵引（这是刻意的——两者要同步收尾）。
// - 退出时一律用 `NextScene::Replace` 顶替自身而不是 `Pop`，保证玩家之后返回时直接
//   回到曲目页，不会重新播放一遍解锁动画。
//
// 各钩子行为：
// - `enter`：记下渲染目标并重置时间轴（从 `State::Before` 的计时重新开始）；
// - `pause`/`resume`：暂停/恢复时间轴与音频，避免切到后台仍在播放；
// - `touch`/`on_result`：未实现——播放期间不接受任何输入，也不能被打断；
// - `update`：先保活音频、再并行驱动加载场景、最后推进状态机；
// - `render`：按相位画视频帧 / 加载圈 / 曲绘，其余相位只清屏；
// - `next_scene`：取走排队中的切换（只有 `Replace` 一种）。
impl Scene for UnlockScene {
    /// 记录渲染目标并重置时间轴，让 `State::Before` 的 0.5 秒静场从头开始计。
    fn enter(&mut self, tm: &mut TimeManager, target: Option<RenderTarget>) -> Result<()> {
        self.render_target = target;
        tm.reset();
        Ok(())
    }

    /// 暂停时间轴与解锁音频（无音轨时只停时间轴）。
    ///
    /// # Errors
    /// 音频后端拒绝暂停时返回错误。
    fn pause(&mut self, tm: &mut TimeManager) -> Result<()> {
        tm.pause();
        if let Some(bgm) = &mut self.bgm {
            bgm.music.pause()?;
        }
        Ok(())
    }

    /// 恢复时间轴与解锁音频。
    ///
    /// # Errors
    /// 音频后端拒绝播放时返回错误。
    fn resume(&mut self, tm: &mut TimeManager) -> Result<()> {
        tm.resume();
        if let Some(bgm) = &mut self.bgm {
            bgm.music.play()?;
        }
        Ok(())
    }

    /// 每帧推进：音频保活 → 驱动加载场景 → 状态机转移。
    ///
    /// # Errors
    /// 音频设备恢复失败、`LoadingScene` 加载失败，以及状态机走到 `Transforming` 却
    /// 仍未拿到游戏场景（`bail!`）时返回错误。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        // 音频保活：设备热插拔/丢失后尝试恢复，否则视频还在播、声音却没了。
        if let Some(bgm) = &mut self.bgm {
            bgm.audio_manager.recover_if_needed()?;
        }

        // 阶段一：驱动后台加载场景。只要游戏场景还没拿到手，就持续更新它并接收它的
        // 转场请求；`PopWithResult` 表示加载失败并想带结果退回上层，原样转发；
        // `Replace` 则是加载成功，收下游戏场景（此后不再驱动加载场景）。
        if self.game_scene.is_none() {
            self.loading_scene.update(tm)?;
            let loading_next_scene = self.loading_scene.next_scene(tm);
            match loading_next_scene {
                NextScene::PopWithResult(_) => self.next_scene = Some(loading_next_scene),
                NextScene::Replace(game_scene) => self.game_scene = Some(game_scene),
                _ => (),
            }
        }

        // 阶段二：状态机推进。`t` 是当前相位内的相对时间。
        let t = tm.now();
        match self.state {
            // Before：静场 0.5 秒后开始播放。把音频 seek 回 0 再 play，是因为 `Music`
            // 可能已被上一次 `pause`/`recover` 挪动过位置。
            State::Before => {
                if t > 0.5 {
                    self.state = State::Playing;
                    tm.reset();
                    if let Some(bgm) = &mut self.bgm {
                        bgm.music.seek_to(0.)?;
                        bgm.music.play()?;
                    }
                }
            }
            // Playing：以**音频播放位置**为准推进时间轴（`tm.update`），视频只是按这个
            // 时间戳取帧，因此音画不会因解码抖动而累积漂移；无音轨时才退化为挂钟时间。
            // 结束条件是视频与音频**都**放完（`&&`），即按两者中较长的那条收尾。
            State::Playing => {
                if t > self.video.duration && t > self.music_length {
                    self.state = State::Blanking;
                    tm.reset();
                } else {
                    if let Some(bgm) = &mut self.bgm {
                        tm.update(bgm.music.position() as _);
                    }
                    self.video.update(t)?;
                }
            }
            // Blanking：视频刚放完的黑屏收尾。若游戏场景已就绪且又等了 1 秒，就直接
            // 交出场景退出；否则转入 Loading 重新计时（所以本相位通常只停留一帧）。
            State::Blanking => {
                if t > 1. && self.game_scene.is_some() {
                    self.next_scene = self.game_scene.take().map(NextScene::Replace);
                } else {
                    self.state = State::Loading;
                    tm.reset();
                }
            }
            // Loading：显示加载圈，等游戏场景就绪（同样至少等满 1 秒）。
            // 就绪后转 Transforming 且**不重置时间轴**，让下面的收尾动画时间连续。
            State::Loading => {
                if t > 1. && self.game_scene.is_some() {
                    self.state = State::Transforming;
                }
            }
            // Transforming：真正退出。此处若仍无游戏场景说明流程异常（前面各相位都已在
            // 就绪时才放行），直接 `bail!` 而不是静默返回，避免卡在空白页。
            State::Transforming => {
                if t > 1. {
                    if self.game_scene.is_none() {
                        bail!("UnlockScene exited at State::Blank3 without GameScene");
                    }
                    self.next_scene = self.game_scene.take().map(NextScene::Replace);
                }
            }
        }

        Ok(())
    }

    /// 按相位绘制画面。
    ///
    /// 本场景始终黑底：`Before` 与 `Blanking` 就是刻意留黑，只有 `Playing`（视频）、
    /// `Loading`（加载圈）、`Transforming`（加载圈淡出 + 曲绘淡入）会画内容。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut prpr::ui::Ui) -> Result<()> {
        // 沿用 UI 相机（保证坐标与加载圈一致），并把 `enter` 传入的渲染目标写回相机，
        // 以便在离屏/叠加渲染时正确落点。`cam.zoom.y` 为负（y 轴翻转），取负得到正的
        // 宽高比 `asp`；`1. / asp` 即 UI 坐标下的半高。
        let mut cam = ui.camera();
        let asp = -cam.zoom.y;
        let t = tm.now();
        cam.render_target = self.render_target;
        set_camera(&cam);
        clear_background(BLACK);

        match self.state {
            // Playing：前 0.05 秒先留黑，等解码器/音频真正起步后再上第一帧，
            // 避免闪出一帧未初始化的画面。
            State::Playing => {
                if t > 0.05 {
                    self.video.render(t, asp, WHITE);
                }
            }
            // Loading：右下角的白色加载圈，位置随 `pad` 内缩。
            State::Loading => {
                let pad = 0.07;
                let top = 1. / asp;
                ui.loading(
                    1. - pad,
                    top - pad,
                    t as f32,
                    WHITE,
                    LoadingParams {
                        width: 0.01,
                        radius: 0.04,
                        ..Default::default()
                    },
                );
            }
            // Transforming：前半秒让加载圈淡出，之后换成曲绘背景并叠一层固定 0.3 的压暗，
            // 使画面过渡到「进入游戏前」的静止状态。
            State::Transforming => {
                let top = 1. / asp;
                if t < 0.5 {
                    let pad = 0.07;
                    let alpha = if t < 0.5 { 1. - t as f32 / 0.5 } else { 0. }; // TODO: more smoothly
                    ui.loading(
                        1. - pad,
                        top - pad,
                        t as f32,
                        semi_white(alpha),
                        LoadingParams {
                            width: 0.01,
                            radius: 0.04,
                            ..Default::default()
                        },
                    );
                } else {
                    let alpha = if t < 0.5 { t as f32 / 0.5 * 0.3 } else { 0.3 };
                    let r = ui.screen_rect();
                    ui.fill_rect(r, (*self.background, r));
                    ui.fill_rect(r, semi_black(alpha));
                }
            }
            _ => (),
        }

        Ok(())
    }

    /// 交出排队的场景切换。
    ///
    /// 这里的时间参数未使用（切换时机完全由状态机决定，不依赖 `SFader` 这类时间驱动
    /// 的过渡）；没有排队项时返回 `NextScene::default()`（即不切换）。
    fn next_scene(&mut self, _tm: &mut TimeManager) -> NextScene {
        self.next_scene.take().unwrap_or_default()
    }
}
