//! 加载场景：在真正开局（[`GameScene`]）之前预解析谱面与资源，并向玩家展示加载进度与错误。
//!
//! 该场景是「选曲/重开」与「游玩」之间的过渡层：它把耗时的谱面解析、贴图解码、音频加载
//! 包成一个本地异步任务（`load_task`）逐帧轮询推进，同时显示曲绘、曲名、谱师与加载动画。
//!
//! 三种由上层应用（phira）注入的回调决定了「游玩结果如何处理」，本模块只负责把它们
//! 原样转交给 [`GameScene`]：
//! - [`UploadFn`]：把编码后的成绩上传到服务器；
//! - [`UpdateFn`]：每帧回调，让上层驱动自定义逻辑（如外部判定、自动游玩）；
//! - [`SaveFn`]：把本局最佳成绩持久化到本地。
//!
//! 加载完成后，成功则用 [`NextScene::Replace`] 直接替换为游玩场景（不再需要回到加载界面）；
//! 失败则用 [`NextScene::PopWithResult`] 把错误交回下层场景处理，由它决定提示还是重试。
use super::{draw_background, ending::RecordUpdateState, game::GameMode, GameScene, NextScene, Scene};
use crate::{
    config::Config,
    core::{Resource, BOLD_FONT},
    ext::{poll_future, semi_black, semi_white, LocalTask, RectExt, SafeTexture, BLACK_TEXTURE},
    fs::FileSystem,
    info::ChartInfo,
    judge::Judge,
    scene::game::SimpleRecord,
    task::Task,
    time::TimeManager,
    ui::{clip_rounded_rect, rounded_rect_shadow, LoadingParams, ShadowConfig, Ui, PREFER_REDUCED_MOTION},
};
use ::rand::{seq::SliceRandom, thread_rng};
use anyhow::{Context, Result};
use macroquad::prelude::*;
use regex::Regex;
use std::sync::{atomic::Ordering, Arc};
use tracing::warn;

/// 加载完成到切场景之间的停顿时间（秒），给玩家一点「加载完毕」的视觉确认。
const BEFORE_TIME: f32 = 1.;
/// 加载界面的淡入时长（秒）：从黑屏过渡到加载卡片。
const FADE_IN_TIME: f32 = 0.6;

/// 成绩上传回调：接收序列化后的成绩字节流，返回一个可轮询的任务，最终给出服务端刷新后的成绩状态。
///
/// 用 `Arc` 而非 `Box`，是因为同一个回调还需要被传给结算场景（重试上传时复用）。
pub type UploadFn = Arc<dyn Fn(Vec<u8>) -> Task<Result<RecordUpdateState>>>;
/// 每帧更新回调：参数为当前曲目时间、资源包与判定器，供上层注入自定义逻辑而无需改动引擎。
///
/// 用 `FnMut` + `&mut` 参数，是因为它需要就地读写资源与判定状态（例如推进特效、外部判定）。
pub type UpdateFn = Box<dyn FnMut(f64, &mut Resource, &mut Judge)>;
/// 本地成绩保存回调：写入本局成绩，返回错误时由调用方决定是否中断结算流程。
pub type SaveFn = Box<dyn Fn(SimpleRecord) -> Result<()>>;

/// 返回界面切换动画的时长；遵守系统的「减少动态效果」设置。
///
/// 开启无障碍选项时返回 `None`，调用方据此跳过过渡动画（既照顾晕动症用户，
/// 也避免低端设备在动画上浪费帧）。
fn transition_time() -> Option<f32> {
    if PREFER_REDUCED_MOTION.load(Ordering::Relaxed) {
        None
    } else {
        Some(1.4)
    }
}

/// 返回切场景前的等待时长；同样遵守「减少动态效果」设置，开启时立即切换。
fn wait_time() -> f32 {
    if PREFER_REDUCED_MOTION.load(Ordering::Relaxed) {
        0.
    } else {
        0.4
    }
}

/// 当前玩家在本局的静态信息，由上层应用从账号数据中组装后传入。
///
/// 之所以只传这几个字段而不是整个玩家对象，是为了让引擎不依赖上层的账号模型。
pub struct BasicPlayer {
    /// 玩家头像贴图；`None` 表示头像尚未加载或玩家未登录，界面需回退到占位图。
    pub avatar: Option<SafeTexture>,
    /// 服务端玩家 id，用于成绩上传时标识归属。
    pub id: i32,
    /// 玩家当前的 RKS（Rating，Phigros 的段位分），用于结算时展示变化量。
    pub rks: f32,
    /// 该谱面的历史最高分，用于判断本局是否刷新纪录并计算提升分值。
    pub historic_best: u32,
}

/// 加载界面场景。
///
/// 构造时已经把 [`GameScene`] 的创建 Future 存进 `load_task`，因此真正的加载工作是
/// 逐帧在 [`Scene::update`] 中推进的——这既让界面能边加载边渲染，也避免阻塞主线程。
pub struct LoadingScene {
    // 谱面信息（曲名、曲师、谱师、难度等），构造时已补全 tip 字段。
    info: ChartInfo,
    // 用于背景的模糊版曲绘。
    background: SafeTexture,
    // 清晰版曲绘，显示在加载卡片中。
    illustration: SafeTexture,
    /// 正在推进的加载任务。返回 `None` 表示加载已结束（无论成功或失败），
    /// 之后 `next_scene` 将接管切场景动作。对外可见是为了让宿主能在外部提前放弃/替换它。
    pub load_task: LocalTask<Result<GameScene>>,
    // 加载完成（或失败）后要执行的场景切换意向；加载期间一直为 None。
    next_scene: Option<NextScene>,
    // 加载完成的时刻（`tm` 时间），驱动后续的停留与切场动画。
    finish_time: f32,
    // 渲染目标；None 表示直接绘制到窗口。加载期间若为目标渲染，需要持续排空任务，
    // 因此 update 中的轮询策略会随之不同（见 Scene::update 实现）。
    target: Option<RenderTarget>,
    // 已剥离控制字符的谱师名，用于界面显示（原始 `info.charter` 可能含 [!..] 标记）。
    charter: String,

    // 从曲绘中提取的主题色，用于加载卡片的底色。
    theme_color: Color,
    // 主题色是否偏亮。偏亮时前景文字改用深色，保证对比度可读。
    use_black: bool,
}

// LoadingScene 的构造与资源预处理。load 是纯函数式解码（可单独复用），new 负责组装场景。
impl LoadingScene {
    /// 加载曲绘并做后处理，返回 `(清晰曲绘, 模糊背景, 主题色)`。
    ///
    /// 处理链路：解码原图 → 取调色板得到主题色 → 对 RGB 缓冲做高斯模糊 → 转成 RGBA
    /// 供 GPU 上传。模糊是在 CPU 上做的，因为只需要在加载时执行一次。
    ///
    /// # Errors
    ///
    /// 文件读取失败或图片解码失败时返回错误（错误信息中带上「Failed to decode image」上下文）。
    pub async fn load(fs: &mut dyn FileSystem, path: &str) -> Result<(SafeTexture, SafeTexture, Color)> {
        let image = image::load_from_memory(&fs.load_file(path).await?).context("Failed to decode image")?;
        let (w, h) = (image.width(), image.height());
        let size = w as usize * h as usize;

        let mut blurred_rgb = image.to_rgb8();
        // 只取调色板的第一个（占比最高的）颜色作为主题色；取不到颜色时直接向上返回错误。
        let color = color_thief::get_palette(&blurred_rgb, color_thief::ColorFormat::Rgb, 10, 2)?[0];
        // SAFETY: 这里把 RGB 缓冲按 3 字节像素重新解释为 `Vec<[u8; 3]>`，以满足 fastblur 的按元素模糊接口。
        // 前置条件（三条都由上面的代码保证）：
        // - 指针来自 `blurred_rgb.as_mut_ptr()`，指向长度为 `size * 3` 的有效分配；
        // - 元素类型 `[u8; 3]` 大小为 3、对齐为 1，与 u8 分配的对齐要求兼容；
        // - `size` 恰好是像素数，因此重解释后的长度与原分配长度一致。
        // 下方的 `std::mem::forget` 会阻止这个临时 Vec 释放内存——真正的所有者仍是 `blurred_rgb`。
        let mut vec = unsafe { Vec::from_raw_parts(std::mem::transmute::<*mut u8, *mut [u8; 3]>(blurred_rgb.as_mut_ptr()), size, size) };
        fastblur::gaussian_blur(&mut vec, w as _, h as _, 50.);
        // 交出临时 Vec 的所有权（不析构），避免与 blurred_rgb 造成双重释放。
        std::mem::forget(vec);
        // 模糊只作用于 RGB 三通道，这里补回 alpha=255 组装成 GPU 需要的 RGBA 格式。
        let mut blurred = Vec::with_capacity(size * 4);
        for input in blurred_rgb.chunks_exact(3) {
            blurred.extend_from_slice(input);
            blurred.push(255);
        }
        Ok((
            Texture2D::from_rgba8(w as _, h as _, &image.into_rgba8()).into(),
            Texture2D::from_image(&Image {
                width: w as _,
                height: h as _,
                bytes: blurred,
            })
            .into(),
            Color::from_rgba(color.r, color.g, color.b, 255),
        ))
    }

    /// 构造加载场景，并立即创建（但不推进）游玩场景的加载任务。
    ///
    /// # Arguments
    ///
    /// * `mode` - 游玩模式（普通/调偏移/练习/不可重开/观战），会原样传给 [`GameScene`]。
    /// * `info` - 谱面信息；若其中没有 tip（加载界面提示语），会随机挑一条补全。
    /// * `fs` - 虚拟文件系统，加载任务会依赖它读取谱面与素材。
    /// * `player` - 当前玩家信息；`None` 表示未登录，结算不上传成绩。
    /// * `upload_fn` / `update_fn` / `save_fn` - 上层注入的回调，原样转交给 [`GameScene`]。
    /// * `preloaded` - 已预加载的 `(曲绘, 模糊背景, 主题色)`。上层若已经解码过曲绘就从这里传入，
    ///   避免重复解码；为 `None` 时本方法自行 [`load`](Self::load)。
    ///
    /// # Errors
    ///
    /// 仅在无法构造场景本身（例如资源包/音频初始化失败）时返回错误；
    /// 曲绘加载失败不视为致命错误，会退化为黑色贴图 + 白色主题色并继续。
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        mode: GameMode,
        mut info: ChartInfo,
        config: Config,
        mut fs: Box<dyn FileSystem>,
        player: Option<BasicPlayer>,
        upload_fn: Option<UploadFn>,
        update_fn: Option<UpdateFn>,
        save_fn: Option<SaveFn>,

        preloaded: Option<(SafeTexture, SafeTexture, Color)>,
    ) -> Result<Self> {
        // 曲绘是加载界面里唯一可能失败的大资源，这里刻意「降级而非报错」：
        // 用黑色贴图兜底，保证玩家仍能进入游玩，而不是卡在加载界面出不去。
        let (background, theme_color) = match preloaded {
            Some((ill, bg, color)) => (Some((ill, bg)), color),
            None => match Self::load(fs.as_mut(), &info.illustration).await {
                Ok((ill, bg, color)) => (Some((ill, bg)), color),
                Err(err) => {
                    warn!("failed to load background: {err:?}");
                    (None, WHITE)
                }
            },
        };
        // 按人眼亮度感知（ITU-R BT.601 权重）判断主题色明暗，再决定前景文字用黑还是白。
        // 阈值 186/255 略高于中灰，是让「较浅的彩色」也走黑字，避免浅色背景上白字发虚。
        let use_black = (theme_color.r * 0.299 + theme_color.g * 0.587 + theme_color.b * 0.114) > 186. / 255.;
        let (illustration, background) = background.unwrap_or_else(|| (BLACK_TEXTURE.clone(), BLACK_TEXTURE.clone()));
        // tip 是可选的加载提示语，缺失时从全局提示库随机选一条，保证界面上总有内容可读。
        if info.tip.is_none() {
            info.tip = Some(crate::config::TIPS.choose(&mut thread_rng()).unwrap().to_owned());
        }
        // 注意：这里只是「创建」Future 并装箱，并未 poll；真正的加载发生在 Scene::update 中。
        // 因此构造函数可以保持 async 但耗时极短（除了可能的曲绘解码）。
        let future =
            Box::pin(GameScene::new(mode, info.clone(), config, fs, player, background.clone(), illustration.clone(), upload_fn, update_fn, save_fn));
        // 谱师字段可能带有形如 `[!:3:name]` 的难度标记前缀，这里只保留冒号后的可读名字。
        // 正则每次构造都会重新编译；由于该模式固定不变，若日后成为性能热点可提取为静态量。
        let charter = Regex::new(r"\[!:[0-9]+:([^:]*)\]").unwrap().replace_all(&info.charter, "$1").to_string();

        Ok(Self {
            info,
            background,
            illustration,
            load_task: Some(future),
            next_scene: None,
            finish_time: f32::INFINITY,
            target: None,
            charter,

            theme_color,
            use_black,
        })
    }
}

// LoadingScene 的场景契约实现：自身没有交互，全部职责是「推进加载并展示进度」。
impl Scene for LoadingScene {
    /// 记录本次的渲染目标并重置时间轴，使淡入动画每次进入都从头开始。
    ///
    /// 时间轴必须 reset：本场景被复用（重开同一首歌）时，若沿用旧时间会让淡入/进度动画
    /// 从中间开始，观感错乱。
    fn enter(&mut self, tm: &mut TimeManager, target: Option<RenderTarget>) -> Result<()> {
        self.target = target;
        tm.reset();
        Ok(())
    }

    /// 暂停时间轴。加载场景没有音频等外部资源，因此只需暂停引擎时间。
    fn pause(&mut self, tm: &mut TimeManager) -> Result<()> {
        tm.pause();
        Ok(())
    }

    /// 恢复时间轴，与 [`pause`](Self::pause) 配对。
    fn resume(&mut self, tm: &mut TimeManager) -> Result<()> {
        tm.resume();
        Ok(())
    }

    /// 推进加载任务：每帧轮询一次 Future，直到完成或需要让出控制权。
    ///
    /// 这里对「离屏渲染目标」做了特殊处理：`target` 为 `Some` 时说明宿主希望立即拿到
    /// 加载界面的画面（例如用于过渡动画素材），因此本帧必须把加载跑完，用
    /// `yield_now` 让出 CPU 时间片但不退出循环；`target` 为 `None`（直接上屏）时则
    /// 每次只推进一轮就返回，让界面能持续刷新、保持响应。
    ///
    /// 加载完成后把结果包装成场景切换意向：
    /// - 成功：`Replace` 换成游玩场景（加载界面无需保留在栈里，也不该被返回）；
    /// - 失败：`PopWithResult` 把错误交回下层场景，由它决定提示或重试。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        if let Some(future) = self.load_task.as_mut() {
            loop {
                match poll_future(future.as_mut()) {
                    None => {
                        if self.target.is_none() {
                            break;
                        }
                        std::thread::yield_now();
                    }
                    Some(game_scene) => {
                        // 置空任务即表示「加载已结束」，next_scene 据此判断是否可以切场。
                        self.load_task = None;
                        self.next_scene =
                            Some(game_scene.map_or_else(|e| NextScene::PopWithResult(Box::new(e)), |it| NextScene::Replace(Box::new(it))));
                        // 记录完成时刻，之后还会停留 BEFORE_TIME 让玩家看清「加载完成」。
                        self.finish_time = tm.now() as f32 + BEFORE_TIME;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// 绘制加载界面：模糊曲绘背景 + 中央加载卡片（曲绘、曲名、曲师、谱师、插画师）+ 加载动画 + 随机提示语。
    ///
    /// 动画分两段：进入时整体淡入（`FADE_IN_TIME`）；加载完成后再按 `transition_time`
    /// 把卡片向左推出屏幕，为即将到来的游玩场景让位。开启「减少动态效果」时用
    /// `map_or(1., ...)` 直接取满偏移量，等同于瞬间移出（无过渡）。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        // 手动设置相机（而不是依赖 Ui 默认相机），因为需要把 render_target 指定为
        // 本次的离屏目标；相机 zoom 的 y 分量取负即纵向半高，用于把 UI 坐标换算到屏幕边缘。
        let mut cam = ui.camera();
        let asp = -cam.zoom.y;
        let top = 1. / asp;
        let t = tm.now() as f32;
        cam.render_target = self.target;
        set_camera(&cam);
        draw_background(*self.background);

        // 整块界面（卡片 + 文字）统一淡入，避免各元素出现时间不一致造成的闪烁。
        ui.alpha((t / FADE_IN_TIME).min(1.), |ui| {
            // 淡入完成后进入「退出阶段」：把整块界面水平推出屏幕。
            // 用三次方曲线（powi(3)）让启动慢、结束快，观感上更接近"被抽走"。
            let dx = if t > self.finish_time {
                transition_time().map_or(1., |tt| {
                    let p = ((t - self.finish_time) / tt).min(1.);
                    p.powi(3) * 2.
                })
            } else {
                0.
            };

            ui.dx(-dx);

            // 卡片占满逻辑屏幕，底部留出 bar_height 高的信息条（曲名/曲师/谱师等）。
            let r = Rect::default().nonuniform_feather(0.65, top * 0.7);
            let config = ShadowConfig {
                radius: 0.008,
                ..Default::default()
            };
            let bar_height = 0.16;
            let ir = Rect { h: r.h - bar_height, ..r };

            // 依据主题色明暗取前景色对：主色用于标题、次色用于副标题/标签。
            let (main, sub) = Ui::main_sub_colors(self.use_black, 1.);

            // 先用阴影勾勒卡片轮廓，再在裁剪区域内填充主题色底与曲绘。
            rounded_rect_shadow(ui, r, &config);
            // 裁剪到圆角卡片：先铺一层 60% 透明的主题色（卡片被曲绘挡住的部分也保持色调），
            // 再把曲绘拉伸铺满信息条以上的区域，最后叠一层自上而下（不透明→透明）的黑色渐变，
            // 让卡片顶部与背景的衔接更柔和。
            clip_rounded_rect(ui, r, config.radius, |ui| {
                ui.fill_rect(r, Color { a: 0.6, ..self.theme_color });
                ui.fill_rect(ir, (*self.illustration, ir));
                ui.fill_rect(ir, (semi_black(0.5), (ir.x, ir.bottom()), Color::default(), (ir.x, ir.y)));
            });

            // 左侧 65% 宽度放曲名与曲师，右侧放谱师/插画师，用同一基准线 ct 对齐。
            let ct = ir.bottom() + bar_height / 2.;
            let lf = r.x + 0.04;
            let rt = r.x + r.w * 0.65;
            let mw = rt - lf - 0.02;
            ui.text(&self.info.name)
                .pos(lf, ct)
                .anchor(0., 1.)
                .size(0.7)
                .color(main)
                .max_width(mw)
                .draw();
            ui.text(&self.info.composer)
                .pos(lf, ct + 0.012)
                .anchor(0., 0.)
                .size(0.4)
                .color(sub)
                .max_width(mw)
                .draw();

            // 右半区是「标签 + 值」两行布局：标签统一用次色，值用主色，方便快速扫读。
            // 注意 "Chart"/"Cover" 两个标签是硬编码的英文（未走本地化）。
            let lf = rt + 0.03;
            let dy = bar_height / 6.;
            let size = 0.45;
            ui.text("Chart")
                .pos(lf, ct - dy)
                .anchor(0., 0.5)
                .no_baseline()
                .size(size)
                .color(sub)
                .draw_using(&BOLD_FONT);
            ui.text("Cover")
                .pos(lf, ct + dy)
                .anchor(0., 0.5)
                .no_baseline()
                .size(size)
                .color(sub)
                .draw_using(&BOLD_FONT);

            let lf = lf + 0.12;
            let mw = r.right() - lf - 0.01;
            ui.text(&self.charter)
                .pos(lf, ct - dy)
                .anchor(0., 0.5)
                .no_baseline()
                .size(size)
                .color(main)
                .max_width(mw)
                .draw();
            ui.text(&self.info.illustrator)
                .pos(lf, ct + dy)
                .anchor(0., 0.5)
                .no_baseline()
                .size(size)
                .color(main)
                .max_width(mw)
                .draw();

            // 右下角的加载转圈。加载完成后 0.4 秒内把颜色淡出（用「剩余量的三次方」做缓出），
            // 使转圈先于卡片滑出消失，形成"已完成"的层次感。
            let r = 0.07;
            ui.loading(
                1. - r,
                top - r,
                t,
                if t > self.finish_time {
                    let p = ((t - self.finish_time) / 0.4).min(1.);
                    semi_white((1. - p).powi(3))
                } else {
                    WHITE
                },
                LoadingParams {
                    radius: 0.04,
                    width: 0.01,
                    ..Default::default()
                },
            );

            // 左下角的随机提示语。`tip` 已在 new() 中保证被填充，故此处 unwrap 安全。
            ui.text(self.info.tip.as_ref().unwrap())
                .pos(-0.95, top - 0.05)
                .anchor(0., 1.)
                .size(0.47)
                .color(WHITE)
                .draw();
        });

        Ok(())
    }

    /// 决定何时真正切场景。
    ///
    /// 这里区分两种结果：
    /// - 失败（[`NextScene::PopWithResult`]）**立即**返回，不做任何等待——加载失败时让玩家
    ///   尽快看到原因更重要，也让上层能立刻显示错误弹窗。
    /// - 成功（[`NextScene::Replace`]）必须等「停留时间 + 过渡动画 + 额外等待」全部走完再切，
    ///   否则卡片还没滑出就被替换掉，玩家会感觉画面突跳。
    ///
    /// 注意：`next_scene` 一旦被 take 走就只剩 [`NextScene::None`]，因此本方法天然幂等，
    /// 不会重复触发切换。
    fn next_scene(&mut self, tm: &mut TimeManager) -> NextScene {
        if matches!(self.next_scene, Some(NextScene::PopWithResult(_))) {
            return self.next_scene.take().unwrap();
        }
        if tm.now() as f32 > self.finish_time + transition_time().unwrap_or_default() + wait_time() {
            if let Some(scene) = self.next_scene.take() {
                return scene;
            }
        }
        NextScene::None
    }
}
