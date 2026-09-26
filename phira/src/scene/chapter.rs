//! 内置章节场景（[`ChapterScene`]）。
//!
//! 「内置曲目」指**随客户端分发**的曲目（资源在 `assets/res/song/<id>/`），与网络曲库、
//! 本地谱面并列存在：无需下载、不可上传或删除，也没有解锁视频。本模块负责这类曲目的
//! 展示与挑选，并把选中的曲目交给 `SongScene` 继续走统一的加载流程。
//!
//! 本文件同时是「内置资源伪路径 + 虚拟文件系统」机制的**写侧**：点击曲目时把生成的
//! `ChartInfo` 写进全局槽 `ASSET_CHART_INFO`，读侧（`:info` 伪路径）在 `scene.rs`。

prpr_l10n::tl_file!("chapter");

use crate::{
    anim::Anim,
    data::BriefChartInfo,
    dir,
    icons::Icons,
    load_res_tex,
    page::{ChartItem, ChartType, Illustration, SFader},
    resource::rtl,
};
use anyhow::Result;
use macroquad::prelude::*;
use prpr::{
    config::Mods,
    core::BOLD_FONT,
    ext::{semi_black, semi_white, RectExt, SafeTexture},
    info::ChartInfo,
    scene::{NextScene, Scene},
    time::TimeManager,
    ui::{button_hit, DRectButton, RectButton, Scroll, Ui},
};
use serde::Deserialize;
use std::{borrow::Cow, sync::Arc};
use tap::Tap;

// 与单曲页的耦合点。进入某首内置曲目时要先把它的元信息（`ChartInfo`）写进全局槽
// `ASSET_CHART_INFO`，再由 `SongScene` 经内置资源虚拟文件系统的 `:info` 伪路径把
// 这份 YAML 读回来。绕这一圈的目的：让「随客户端分发的内置曲目」与网络/本地谱面
// 复用**同一套**加载、预览、游玩流程，而不必为内置曲目单独维护一条代码分支。
use super::{SongScene, ASSET_CHART_INFO};

/// 难度档位。
///
/// `#[repr(usize)]` 不是装饰：`levels` 在 `info.yml` 中按 **EZ / HD / EX** 顺序书写，
/// 而本枚举的判别值恰好就是该数组下标（`self.diff as usize`），二者必须保持同序。
/// 一旦在中间插入档位，就会与所有 `info.yml` 的书写顺序错位。
#[repr(usize)]
#[derive(Clone, Copy)]
enum Difficulty {
    /// Easy（`levels[0]`）。
    Easy,
    /// Hard（`levels[1]`），也是场景初始档位。
    Hard,
    /// Extreme（`levels[2]`）。
    Extreme,
}
impl Difficulty {
    /// 该档位的本地化名称，用于难度切换按钮上的文字。
    pub fn name(&self) -> Cow<'static, str> {
        match self {
            Self::Easy => tl!("diff-easy"),
            Self::Hard => tl!("diff-hard"),
            Self::Extreme => tl!("diff-extreme"),
        }
    }

    /// 该档位的主题色（沿用系列惯例的绿/橙/红），用于难度按钮及切换时的颜色动画。
    pub fn color(&self) -> Color {
        Color::from_hex_rgb(match self {
            Self::Easy => 0x16a34a,
            Self::Hard => 0xf97316,
            Self::Extreme => 0xdc2626,
        })
    }
}

/// 内置曲目 `info.yml` 中单个难度的条目。
///
/// 字段名与 YAML 逐字对应（未使用 `rename_all`），见 `assets/res/song/<id>/info.yml`。
#[derive(Deserialize)]
struct LevelInfo {
    /// 展示用等级文本（形如 `"HD Lv. 10"`），直接画在卡片角标上。
    level: String,
    /// 谱师署名。
    charter: String,
    /// 难度数值（如 `10.3`），用于排序与生成 `BriefChartInfo`。
    difficulty: f32,
}

/// 内置曲目 `info.yml` 的整体结构。
///
/// 这是**随客户端分发**的资源文件，不含任何 id/上传者等网络字段，因此不能直接当作
/// [`ChartInfo`]：进入游玩前会先转换成 [`BriefChartInfo`]/[`ChartInfo`] 的形态。
#[derive(Deserialize)]
struct SongInfo {
    /// 曲名。
    name: String,
    /// 曲目简介（可为空串）。
    intro: String,
    /// 曲师。
    composer: String,
    /// 曲绘师。
    illustrator: String,
    /// 各难度条目，顺序固定为 EZ/HD/EX，与 [`Difficulty`] 的判别值对应。
    levels: Vec<LevelInfo>,
}

/// 章节页里的一首内置曲目，缓存了渲染与命中判定所需的全部状态。
struct ChartInstance {
    /// 曲目目录名（如 `snow`），同时用作伪路径前缀（`:snow:hd`）。
    id: String,
    /// 从 `res/song/<id>/info.yml` 反序列化得到的元信息。
    info: SongInfo,
    /// 曲绘缩略图，用于卡片背景（与章节封面的高分辨率纹理不同）。
    illu: SafeTexture,
    /// 卡片点击热区，矩形在每帧 `render` 时回填。
    btn: DRectButton,
}

/// 内置章节（随客户端分发的章节，目前仅 `c1`）的曲目挑选场景。
///
/// # 与网络曲库 / 本地谱面的区别
/// 后两者是「点一首歌 → 进 [`SongScene`]」，谱面文件来自服务端下载或本地磁盘；
/// 本场景展示的是**打包进安装包**的固定曲目清单（清单本身硬编码在 [`ChapterScene::new`]），
/// 只能播放，不能上传/删除，也没有解锁视频。
///
/// # 章节与曲目清单的来源
/// 章节卡片列表由 `page/coll.rs` 硬编码（只有 `c1` 与一张 `"@"` 占位卡），
/// 点击后把章节 id 传给本场景；本场景再按 id 硬编码决定要加载哪些曲目。
/// 两处都是**编译期常量**，没有服务端拉取环节。
pub struct ChapterScene {
    /// 章节 id（如 `c1`）。用于拼出 `chap-<id>` / `chap-<id>-intro` 本地化键。
    id: String,

    /// 全局图标集（返回箭头等），由上层共享。
    icons: Arc<Icons>,
    /// 判定等级图标，构造 `SongScene` 时原样转交，保证游玩内与列表中共用同一批纹理。
    rank_icons: [SafeTexture; 8],
    /// 章节封面（高分辨率原图），由合辑页传入并铺满整屏作为背景。
    cover: SafeTexture,

    /// 左上角返回按钮。
    btn_back: RectButton,

    /// 待执行的场景切换（返回时用于 Pop，见 [`Scene::next_scene`]）。
    next_scene: Option<NextScene>,

    /// 是否尚未进入过本场景。仅在首次 `enter` 时重置时间轴，避免从 [`SongScene`]
    /// 返回时重播入场动画、并保留玩家原先的滚动位置。
    first_in: bool,

    /// 当前选中的难度，决定卡片角标、难度数值与进入游玩时的谱面档位。
    diff: Difficulty,
    /// 难度切换按钮（点一下循环切换一档）。
    diff_btn: DRectButton,
    /// 难度按钮的底色动画，切换时平滑过渡到新档位的主题色。
    diff_btn_color: Anim<Color>,

    /// 本场景的入场/出场淡入淡出，以及向 [`SongScene`] 的切换过渡。
    sf: SFader,

    /// 右侧曲目列表的纵向滚动容器。
    scroll: Scroll,
    /// 本章节的全部内置曲目。
    charts: Vec<ChartInstance>,
}

// 场景自身的尺寸与构造逻辑。三个常量只描述右侧曲目卡片的排版，改动时需连带
// 检查 `scroll.y_scroller.step`——步长由「卡高 + 间距」算出，用于松手后的吸附。
impl ChapterScene {
    /// 曲目卡片宽度。
    const WIDTH: f32 = 0.5;
    /// 曲目卡片高度。
    const HEIGHT: f32 = 0.3;
    /// 卡片的水平/垂直留白，同时参与滚动吸附步长计算。
    const PAD: f32 = 0.05;

    /// 构造章节场景：解析内置曲目清单并预载曲绘。
    ///
    /// `id`、`icons`、`rank_icons`、`cover` 均由 `page/coll.rs` 在点击章节卡片后传入
    /// （`cover` 用的是封面的高分辨率原图，因为要铺满全屏）。
    ///
    /// # Errors
    /// `res/song/<id>/info.yml` 读取或 YAML 解析失败时返回错误。注意此处的读取走的是
    /// 打包资源（`load_file`），**不是**网络下载，因此失败即意味着安装包不完整。
    pub async fn new(id: String, icons: Arc<Icons>, rank_icons: [SafeTexture; 8], cover: SafeTexture) -> Result<Self> {
        // 阶段一：由章节 id 决定曲目清单。这是纯硬编码的「章节 → 曲目」映射，
        // 与 `page/coll.rs` 里的章节列表一样属于编译期常量：未列出的 id 会得到空章节
        // （也不会被点开，因为合辑页只提供 `c1`）。
        let songs = match id.as_str() {
            "c1" => vec!["snow", "jumping23"],
            _ => vec![],
        };
        // 阶段二：逐首读取 `res/song/<id>/info.yml`（元信息）与 `res/song/<id>/cover`
        // （曲绘）。封面失败不会中断构造——`load_res_tex` 内部降级为占位纹理。
        let mut charts = Vec::with_capacity(songs.len());
        for song in songs {
            let info = serde_yaml::from_slice(&load_file(&format!("res/song/{song}/info.yml")).await?)?;
            let illu = load_res_tex(&format!("res/song/{song}/cover")).await;
            charts.push(ChartInstance {
                id: song.to_owned(),
                info,
                illu,
                btn: DRectButton::new(),
            });
        }
        // 阶段三：装配场景状态。难度固定从 Hard 起步（与多数玩家的习惯档位一致），
        // 滚动步长按卡片高 + 间距设置，使松手后能吸附成整卡对齐。
        Ok(Self {
            id,

            icons,
            rank_icons,
            cover,
            btn_back: RectButton::new(),

            next_scene: None,

            first_in: true,

            diff: Difficulty::Hard,
            diff_btn: DRectButton::new(),
            diff_btn_color: Anim::new(Difficulty::Hard.color()),

            sf: SFader::new(),

            scroll: Scroll::new().tap_mut(|it| it.y_scroller.step = Self::HEIGHT + Self::PAD),
            charts,
        })
    }
}

// 本场景在场景栈中的行为约定：
// - `enter`：只在**首次**进入时把时间轴归零（播放淡入）；从 `SongScene` 返回时不碰时间轴，
//   以免重播入场动画，并让 `scroll` 保留玩家原来的滚动位置；
// - `pause`/`resume`：未实现，沿用 `Scene` 的默认空实现（本场景没有音视频需要暂停）；
// - `touch`：按「返回 → 难度切换 → 滚动 → 曲目卡片」的优先级分发；命中卡片时组装数据、
//   写入 `ASSET_CHART_INFO`、建立曲目目录，并启动向 `SongScene` 的过渡；
// - `update`：仅推进滚动惯性（本场景没有异步任务，数据在构造时就已全部就绪）；
// - `render`：全屏封面 → 标题/简介（缓出淡入）→ 难度按钮 → 曲目卡片列表 → 过渡遮罩；
// - `on_result`：未实现，本场景不接收上层回传的结果；
// - `next_scene`：优先返回 `touch` 里排队的切换（返回时的 `Pop`），否则交给 `SFader` 决定。
impl Scene for ChapterScene {
    /// 首次进入时重置时间轴以播放淡入。
    ///
    /// `_target` 未被使用——本场景不向离屏渲染目标输出。
    fn enter(&mut self, tm: &mut TimeManager, _target: Option<RenderTarget>) -> Result<()> {
        if self.first_in {
            self.first_in = false;
            tm.reset();
        }
        Ok(())
    }

    /// 处理一次触摸。
    ///
    /// 返回 `Ok(true)` 表示事件已被消费（滚动或某个按钮命中），`Ok(false)` 则放行给下层。
    ///
    /// 分发顺序有讲究：先判返回与难度按钮这类固定控件（不应被滚动手势抢走），
    /// 再交给 `scroll`（它会吞掉已越过拖动阈值的触摸），最后才做卡片命中——
    /// 从而避免「横向/纵向拖动列表时误点进一首歌」。
    ///
    /// # Errors
    /// 卡片被点击时会做磁盘操作（创建曲目目录、读取 offset 文件），失败会把错误上抛。
    fn touch(&mut self, tm: &mut TimeManager, touch: &Touch) -> Result<bool> {
        let t = tm.now() as f32;
        // 优先级 1：返回上一页。用 `NextScene::Pop` 弹出本场景（章节合辑页仍在栈中）。
        if self.btn_back.touch(touch) {
            button_hit();
            self.next_scene = Some(NextScene::Pop);
            return Ok(true);
        }
        // 优先级 2：难度切换。循环 EZ → HD → EX → EZ，并让按钮底色动画过渡到新档位颜色。
        if self.diff_btn.touch(touch, t) {
            button_hit();
            self.diff = match self.diff {
                Difficulty::Easy => Difficulty::Hard,
                Difficulty::Hard => Difficulty::Extreme,
                Difficulty::Extreme => Difficulty::Easy,
            };
            self.diff_btn_color.goto(self.diff.color(), t, 0.4);
            return Ok(true);
        }
        // 优先级 3：滚动容器（拖动中或吸附中都会消费事件）。
        if self.scroll.touch(touch, t) {
            return Ok(true);
        }
        // 优先级 4：曲目卡片命中。命中后依次完成「组装数据 → 落盘 → 写全局槽 → 启动过渡」
        // 四步。全程同步执行：内置资源在构造场景时就已读入内存，这里不需要任何网络请求。
        for chart in &mut self.charts {
            if chart.btn.touch(touch, t) {
                button_hit();
                // 当前难度对应的元信息，用于填充 ChartItem 与 ChartInfo。
                let info = &chart.info;
                let level = &info.levels[self.diff as usize];
                // 内置曲目的定位方式：伪路径 `:<曲目目录>:<档位>`（如 `:snow:hd`，档位取
                // `ez`/`hd`/`ex`）。开头的冒号是约定，表示「这是虚拟文件系统里的条目，
                // 不是磁盘路径」；`SongScene` 会把它交给 `AssetsChartFileSystem`，
                // 再解析成 :music / :illu / :chart / :info 四条真正的资源入口。
                let local_path = format!(
                    ":{}:{}",
                    chart.id,
                    match self.diff {
                        Difficulty::Easy => "ez",
                        Difficulty::Hard => "hd",
                        Difficulty::Extreme => "ex",
                    }
                );
                // 待进入曲目的信息。网络字段（id/uploader/时间戳）一律为 None；
                // `has_unlock: false` 表示内置曲目没有解锁视频；
                // `chart_type: Integrated` 则告诉 SongScene「资源随安装包分发」。
                let item = ChartItem {
                    info: BriefChartInfo {
                        id: None,
                        uploader: None,
                        name: info.name.clone(),
                        level: level.level.clone(),
                        difficulty: level.difficulty,
                        intro: info.intro.clone(),
                        charter: level.charter.clone(),
                        composer: info.composer.clone(),
                        illustrator: info.illustrator.clone(),
                        created: None,
                        updated: None,
                        chart_updated: None,
                        has_unlock: false,
                    },
                    illu: Illustration::from_done(chart.illu.clone()),
                    local_path: Some(local_path.clone()),
                    chart_type: ChartType::Integrated,
                };
                // 落盘阶段。伪路径不能直接当目录名：把 `:` 换成 `_`，在用户谱面目录下
                // 建出（或复用）一个同名目录，用于持久化游玩产生的 offset 等数据。
                // 注意此处 `info` 重新绑定为 `item.info`（&BriefChartInfo），遮蔽了上面
                // 指向 `SongInfo` 的同名变量，后续取的字段均来自 BriefChartInfo。
                let info = &item.info;
                let dir = format!("{}/{}", dir::charts()?, item.local_path.as_ref().unwrap().replace(':', "_"));
                let path = std::path::Path::new(&dir);
                if !path.exists() {
                    std::fs::create_dir_all(path)?;
                }
                let dir = prpr::dir::Dir::new(dir)?;
                // 跨场景传递的关键一步：内置曲目没有磁盘上的 info.yml，因此把刚组装好的
                // ChartInfo 放进全局槽 `ASSET_CHART_INFO`，随后 `AssetsChartFileSystem`
                // 的 `:info` 伪路径会把它的 YAML 序列化结果当作「谱面自带的信息文件」读回，
                // 于是 SongScene 无需为内置曲目写任何特殊分支。写入时机就是现在——
                // 点击卡片、切换场景之前；同一时刻只会有一首内置曲目在展示，故单槽足够。
                *ASSET_CHART_INFO.lock().unwrap() = Some(ChartInfo {
                    id: None,
                    uploader: None,

                    name: info.name.clone(),
                    difficulty: info.difficulty,
                    level: info.level.clone(),
                    charter: info.charter.clone(),
                    composer: info.composer.clone(),
                    illustrator: info.illustrator.clone(),

                    // 三条伪路径把谱面本体、音乐、曲绘统统指向内置虚拟文件系统；解析由
                    // `AssetsChartFileSystem` 完成，:`chart`/`:music`/`:illu` 在开源构建下
                    // 需要随包分发的资源，缺失时由上层按「资源缺失」降级处理。
                    chart: ":chart".to_owned(),
                    format: None,
                    music: ":music".to_owned(),
                    illustration: ":illu".to_owned(),
                    // 内置曲目没有解锁视频，这也是 ChapterScene 走不到 UnlockScene 的原因。
                    unlock_video: None,

                    // 预览与铺面参数。内置曲目在其元信息里不携带这些字段，
                    // 故统一给一组通用默认值：从 0 秒起预览、16:9、背景压暗 0.6、判定线长 6。
                    preview_start: 0.,
                    preview_end: None,
                    aspect_ratio: 16. / 9.,
                    background_dim: 0.6,
                    line_length: 6.,
                    // 读取上一次游玩保存的谱面偏移（4 字节大端 f32 存于曲目目录的 `offset`
                    // 文件）；文件不存在、长度不足或路径不可读时静默回退为 0。
                    offset: dir
                        .read("offset")
                        .map(|d| {
                            f32::from_be_bytes(
                                d.get(0..4)
                                    .map(|first4| {
                                        let mut result = <[u8; 4]>::default();
                                        result.copy_from_slice(first4);
                                        result
                                    })
                                    .unwrap_or_default(),
                            )
                        })
                        .unwrap_or_default(),
                    tip: None,
                    tags: Vec::new(),

                    intro: info.intro.clone(),

                    // 铺面表现与兼容开关。内置谱面在其元信息里也不携带这些项，
                    // 这里按「最普通的官方谱面观感」写死：长条按判定线高度整体遮盖、
                    // 音符不做统一缩放、不强制谱面比例，且显式关闭 RPE 1.7.0 新速度算法、
                    // 开启 attachUI 定位修正（内置谱面按新版工具链制作，故不留 None 走默认）。
                    hold_partial_cover: true,
                    note_uniform_scale: false,
                    force_aspect_ratio: false,
                    use_rpe_170_speed: Some(false),
                    use_attach_ui_fix: Some(true),

                    // 时间戳字段只有服务端拥有，内置曲目一律为空。
                    created: None,
                    updated: None,
                    chart_updated: None,
                });
                // 启动向 SongScene 的过渡：`sf.goto` 会播完淡出后再把 SongScene 交出去。
                // 传入的 `Some(local_path)` 就是上面的伪路径，SongScene 依此选择
                // `AssetsChartFileSystem` 作为文件系统；`Mods::empty()` 表示内置曲目
                // 不套用任何玩家 mod（保证成绩与官方基准一致）。
                self.sf
                    .goto(t, SongScene::new(item, Some(local_path), Arc::clone(&self.icons), self.rank_icons.clone(), Mods::empty()));
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 每帧推进。本场景没有异步任务，只需推进滚动惯性/吸附；
    /// `t` 取自本场景的时间轴（首次进入时已归零），因此从 `SongScene` 返回不会重置动画。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        let t = tm.now() as f32;
        self.scroll.update(t);
        Ok(())
    }

    /// 绘制整个章节页。
    ///
    /// 绘制顺序即层级：背景 → 标题区 → 难度按钮 → 曲目列表 → 场景过渡遮罩。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        set_camera(&ui.camera());
        let t = tm.now() as f32;

        // 阶段一：整屏背景。章节封面铺满，再叠一层 0.3 的黑罩压暗，
        // 让前景文字在任何封面上都有足够对比度；右上角是返回按钮。
        let r = ui.screen_rect();
        ui.fill_rect(r, (*self.cover, r));
        ui.fill_rect(r, semi_black(0.3));
        let r = ui.back_rect();
        ui.fill_rect(r, (*self.icons.back, r));
        self.btn_back.set(ui, r);

        // `rtl!` 展开为 `L10N_LOCAL.with(..)`，这里局部引入 resource 模块的 `L10N_LOCAL`，
        // 使章节标题与本模块 `chapter.ftl` 生成的翻译取到同一份资源（与合辑页的做法一致）。
        use crate::resource::L10N_LOCAL;

        // 阶段二：标题与简介。两者都是 `chap-<id>` / `chap-<id>-intro` 形式的本地化键，
        // 因此新增章节时只需补翻译，无需改代码。
        let title = rtl!(format!("chap-{}", self.id)).into_owned();
        let intro = rtl!(format!("chap-{}-intro", self.id));

        // 入场进度 `p`：0.4 秒内线性推进，再套一条三次缓出曲线（1-(1-p)^3），
        // 让标题「先快后慢」地落位；同时用作简介的透明度与标题的起始位移。
        let p = (t / 0.4).min(1.);
        let p = 1. - (1. - p).powi(3);

        // 标题用 scissor 裁掉超出矩形区域的部分（大字号下若直接画会溢到邻栏），
        // 位移随 `p` 从下方 0.1 滑入。
        let r = Rect::new(-0.83, -0.35, 0.6, 0.12);
        ui.scissor(r, |ui| {
            ui.text(title).pos(r.x, r.y + (1. - p) * 0.1).size(1.4).draw_using(&BOLD_FONT);
        });
        ui.text(intro)
            .pos(r.x, r.bottom() + 0.02)
            .size(0.44)
            .max_width(0.74)
            .multiline()
            .color(semi_white(p))
            .draw();

        // 阶段三：难度按钮。x 与标题左对齐，底色取自当前档位主题色的动画值。
        let r = Rect::new(r.x, 0.3, 0.24, 0.1);
        self.diff_btn.render_shadow(ui, r, t, |ui, path| {
            let ct = r.center();
            ui.fill_path(&path, self.diff_btn_color.now(t));
            ui.text(self.diff.name())
                .pos(ct.x, ct.y)
                .anchor(0.5, 0.5)
                .no_baseline()
                .size(0.6)
                .draw_using(&BOLD_FONT);
        });

        // 阶段四：右侧曲目列表。容器只占屏幕右半（x=0.2 起），高度取上下可视野，
        // `ui.dx(r.w / 2.)` 把绘制原点移到栏内水平中线上，因此下面卡片的 x 以 0 为中心。
        let r = Rect::new(0.2, -ui.top, 0.6, ui.top * 2.);
        self.scroll.size((r.w, r.h));
        ui.scope(|ui| {
            ui.dx(r.x);
            ui.dy(r.y);
            self.scroll.render(ui, |ui| {
                ui.dx(r.w / 2.);
                ui.dy(ui.top);
                let mut y = 0.;
                let step = Self::HEIGHT + Self::PAD;
                // 逐首绘制卡片。同一张卡上叠三层：曲绘、压暗罩、文字；
                // 只有当前难度的一行角标会变，底部曲名固定取自 `info.name`。
                for chart in &mut self.charts {
                    let r = Rect::new(-Self::WIDTH / 2., y - Self::HEIGHT / 2., Self::WIDTH, Self::HEIGHT);
                    chart.btn.render_shadow(ui, r, t, |ui, path| {
                        ui.fill_path(&path, (*chart.illu, r));
                        ui.fill_path(&path, semi_black(0.4));
                        // 右上角角标显示**当前难度**的等级文本（如 `HD Lv. 10`），
                        // 先量出文本尺寸再垫一层半透明胶囊底，保证在任意曲绘上都可读。
                        let mut t = ui
                            .text(&chart.info.levels[self.diff as usize].level)
                            .pos(r.right() - 0.016, r.y + 0.016)
                            .max_width(r.w * 2. / 3.)
                            .anchor(1., 0.)
                            .size(0.52)
                            .color(WHITE);
                        let ms = t.measure();
                        t.ui.fill_path(&ms.feather(0.008).rounded(0.01), Color { a: 0.7, ..t.ui.background() });
                        t.draw();

                        // 左下角曲名，超出宽度自动截断。
                        ui.text(&chart.info.name)
                            .pos(r.x + 0.01, r.bottom() - 0.02)
                            .max_width(r.w)
                            .anchor(0., 1.)
                            .size(0.6)
                            .color(WHITE)
                            .draw();
                    });
                    y += step;
                }

                // 内容尺寸：宽固定，高按卡片数与步长算出，供 Scroll 决定可滚动范围。
                // 注意最后一个 `- 1` 让末尾卡片刚好贴底，同时要求 `charts` 非空。
                (Self::WIDTH, step * (self.charts.len() - 1) as f32 + ui.top * 2.)
            });
        });

        // 阶段五：场景切换遮罩（进场淡入 / 离开淡出）。
        self.sf.render(ui, t);

        Ok(())
    }

    /// 决定下一帧的场景切换。
    ///
    /// 优先级：`touch` 中排队的显式切换（返回时的 `Pop`）→ `SFader` 的过渡结果 → 无切换。
    /// 用 `take()` 取走待处理项，保证同一次切换只被消费一次。
    fn next_scene(&mut self, tm: &mut TimeManager) -> NextScene {
        self.next_scene.take().or_else(|| self.sf.next_scene(tm.now() as f32)).unwrap_or_default()
    }
}
