//! `phira` 的页面栈（page stack）框架——应用层的 UI 组织单位。
//!
//! 这里把“一屏交互”（首页、曲库、活动、消息、设置……）抽象为 [`Page`] trait，
//! 由常驻场景 `scene::main::MainScene` 用 `Vec<Box<dyn Page>>` 持有并按栈的顺序驱动。
//!
//! 与 [`prpr::scene::Scene`] 的职责边界（很容易搞混，故先说明）：
//! - `Scene` 是引擎级渲染单元。`MainScene` 本身是**被引擎长期保留**的场景，负责所有页面共用的部分：
//!   背景、主菜单 BGM、右上/左上返回按钮、多人面板、导入任务、deeplink 等；
//! - [`Page`] 只在 `MainScene` 内部工作，粒度更细，只关心自己这一屏的布局、命中与动画。
//!   页面**不能**直接切换场景，只能通过 [`Page::next_page`] / [`Page::next_scene`] 提出请求，
//!   由 `MainScene` 在“转场动画播完”这类安全时机真正入栈/出栈（见 `scene/main.rs` 的 `update`）。
//!
// 栈的不变量与转场时序：
// - 页面栈永不为空：索引 0 的根页面（`home::HomePage`）不会被弹出；
// - [`SharedState`] 是整栈共享的唯一一份，页面通过它读写时间、[`Fader`] 与本地谱面缓存；
// - 转场动画进行中时（[`Fader::transiting`]），`MainScene` 会**同时**驱动栈顶与其下方一个页面，
//   让“旧页滑出”和“新页滑入”在同一段时间里并行，因此页面实现必须容忍 `update`/`render`
//   在“自己已不在栈顶”时仍被调用。

// 各页面模块在此统一聚合导出：
// 大多数页面（EventPage/MessagePage/OffsetPage 等）只在本 crate 内部被别的页面 new 出来，
// 所以模块保持私有、仅 `pub use` 类型；`coll` 与 `favorites` 则需要暴露模块路径本身
// （外部要直接引用子模块里的类型），故写成 `pub mod`。
// 调用方请统一通过 `page::XxxPage` 引用，不要依赖具体子模块路径。
pub mod coll;
pub use coll::CollectionPage;

mod event;
pub use event::EventPage;

pub mod favorites;
pub use favorites::FavoritesPage;

mod home;
pub use home::HomePage;

mod library;
// `library` 额外导出了导入/导出相关的全局状态与函数：这些接口由 `scene::main` 等
// 页面之外的调用方使用（如系统文件拖入、deeplink），因此不能只导出类型。
pub use library::{request_export, resolve_export, take_export, ExportInfo, LibraryPage, CHOOSE_COVER, CHOSEN_COVER, FAV_UPDATED};

mod message;
pub use message::MessagePage;

mod offset;
pub use offset::OffsetPage;

mod respack;
pub use respack::{ResPackItem, ResPackPage};

mod settings;
pub use settings::SettingsPage;
use tokio::sync::Notify;

use crate::{
    client::{Chart, ChartRef, File},
    data::BriefChartInfo,
    dir, get_data,
    images::Images,
    scene::fs_from_path,
};
use anyhow::Result;
use image::DynamicImage;
use macroquad::prelude::*;
use prpr::{
    core::{Resource, BOLD_FONT},
    ext::{semi_black, semi_white, SafeTexture, ScaleType, BLACK_TEXTURE},
    fs,
    scene::{NextScene, Scene},
    task::Task,
    time::TimeManager,
    ui::{FontArc, IntoShading, Shading, TextPainter, Ui},
};
use std::{
    any::Any,
    borrow::Cow,
    cell::RefCell,
    ops::DerefMut,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing::warn;

/// 计算某个谱面/活动插图的本地缩略图缓存路径。
///
/// 插图的逻辑路径形如 `res/song/xxx/cover`，这里把其中的 `/` 替换成 `_` 再拼到缓存目录下，
/// 目的是让缓存目录保持**扁平**：一来避免为每条谱面创建中间目录，
/// 二来保证不同谱面中同名的插图（如都叫 `cover`）不会互相覆盖。
///
/// # Errors
/// 系统未提供本地缓存目录（`dir::cache_image_local` 失败）时返回错误。
pub fn thumbnail_path(path: &str) -> Result<PathBuf> {
    Ok(format!("{}/{}", dir::cache_image_local()?, path.replace('/', "_")).into())
}

/// 构造“等通知后才读盘”的插图异步加载任务，返回 `(缩略图, 可选原图)`。
///
/// 为什么是任务而不是直接 `await`：曲库/活动列表可能一次展示成百上千条，若进页面就解码全部插图，
/// 会同时吃掉大量 CPU 与显存，并让首帧卡住。这里用 [`Notify`] 充当闸门，只有调用方
/// （[`Illustration::notify`]，通常在渲染循环里对“本帧可见的项”调用）放行后才真正开始解码，
/// 把 IO 与解码成本摊到“该项马上要显示”的时刻。
///
/// `full` 控制是否在缩略图之外额外保留原图：进入曲目详情这类需要大图的位置传 `true`，
/// 纯列表传 `false` 以丢掉原图省内存。注意原图是**惰性**的——如果缩略图命中了磁盘缓存，
/// 就需要再读一次源文件，这也是 `img` 只在此处被填充的原因。
///
/// # Errors
/// 打开谱面包、读取 `info.yml` 或解码插图失败时返回错误，由调用方（[`Illustration::settle`]）降级处理。
pub fn illustration_task(notify: Arc<Notify>, path: String, full: bool) -> Task<Result<(DynamicImage, Option<DynamicImage>)>> {
    Task::new(async move {
        notify.notified().await;
        let mut fs = fs_from_path(&path)?;
        let info = fs::load_info(fs.deref_mut()).await?;
        let mut img = None;
        let thumbnail = Images::local_or_else(thumbnail_path(&path)?, async {
            let image = image::load_from_memory(&fs.load_file(&info.illustration).await?)?;
            let thumbnail = Images::thumbnail(&image);
            img = Some(image);
            Ok(thumbnail)
        })
        .await?;
        if full {
            if img.is_none() {
                img = Some(image::load_from_memory(&fs.load_file(&info.illustration).await?)?);
            }
        } else {
            img = None;
        }
        Ok((thumbnail, img))
    })
}

/// 为“本地谱面包”创建插图句柄：先用 `def`（调用方一般传黑图）占位，等淡入动画就绪后由
/// [`Illustration::settle`] 替换为真实图片。
///
/// 这里刻意只申请缩略图（`full = false`）：本地谱面数量不受限，全部保留原图会显著抬高内存占用。
pub fn local_illustration(path: String, def: SafeTexture, full: bool) -> Illustration {
    let notify = Arc::new(Notify::new());
    Illustration {
        texture: (def.clone(), def),
        notify: Arc::clone(&notify),
        task: Some(illustration_task(notify, path, full)),
        loaded: Arc::default(),
        load_time: f32::NAN,
    }
}

/// 把 `get_data().charts` 里已导入的本地谱面投影为列表项。
///
/// 这里**只做内存数据的映射，不碰磁盘**：插图句柄是惰性的，真正解码要等
/// [`Illustration::notify`] 放行，因此本函数可以在任意线程/任意时刻廉价调用。
pub fn load_local() -> Vec<ChartItem> {
    let tex = BLACK_TEXTURE.clone();
    get_data()
        .charts
        .iter()
        .map(|it| ChartItem {
            info: it.info.clone(),
            local_path: Some(it.local_path.clone()),
            illu: local_illustration(it.local_path.clone(), tex.clone(), false),
            chart_type: ChartType::Imported,
        })
        .collect()
}

/// 插图加载任务的返回类型：`(缩略图, 可选原图)`。
///
/// 单独起别名的原因：这个类型在 [`Illustration::task`]、[`illustration_task`] 与
/// [`local_illustration`] 三处重复出现，直接内联会让签名难以阅读。
type IllustrationTask = Task<Result<(DynamicImage, Option<DynamicImage>)>>;

/// 一张谱面/活动插图的**异步资源句柄**。
///
/// 关键设计：本类型可 `Clone`（[`SafeTexture`] 是引用计数器、其余字段都是 `Arc`），
/// 因此页面之间传递插图只复制句柄、不复制像素；也正因为句柄廉价，
/// [`Illustration::from_file`] 可以被同一个文件创建多次而不会重复占用显存。
/// 生命周期是状态机：`占位图 --notify--> 解码任务 --settle--> 真实纹理 --alpha--> 淡入完成`。
#[derive(Clone)]
pub struct Illustration {
    /// 当前可用的纹理对：`.0` 是列表用缩略图，`.1` 是详情用原图（未就绪时二者都可能是黑图占位）。
    pub texture: (SafeTexture, SafeTexture),
    /// 加载闸门。持有它的一方在收到通知前不会真正开始读盘，见 [`illustration_task`]。
    pub notify: Arc<Notify>,
    /// 尚未结算的加载任务。`None` 有两种含义：还在闸门后等待（首次 `notify` 之前），
    /// 或者已经结算完毕（[`Illustration::settle`] 取走结果后会置回 `None`）。
    pub task: Option<IllustrationTask>,
    /// 已解码结果的共享缓存。同一张插图若在多处被分别创建句柄，后来的实例可以直接命中这里，
    /// 省掉一次重复解码。用 `Mutex` 而非 `Rc` 是因为放入/取出可能发生在异步任务所在线程。
    pub loaded: Arc<Mutex<Option<(SafeTexture, SafeTexture)>>>,
    /// 首次拿到可用纹理的时间戳；`NaN` 表示尚未就绪。仅用于计算淡入透明度。
    pub load_time: f32,
}

// `Illustration` 实现插图的生命周期管理：创建占位 → 靠 `notify` 放行异步解码 → `settle` 结算
// → `alpha` 驱动淡入。所有方法都是“查询状态 / 推进状态”，不做阻塞等待，
// 因此可以在任意页面的 update/render 里无顾虑地调用。
impl Illustration {
    /// 结算后从透明淡入到完全不透明所需的时长（秒）。
    ///
    /// 取值偏短（0.4）是刻意的：插图是列表滚动的背景元素，淡入太慢会让快速滚动看起来“糊”。
    const TIME: f32 = 0.4;

    /// 从远端接口返回的 [`File`] 构造，加载**原图**（列表卡片、详情页的大图位置使用）。
    ///
    /// 起点纹理为黑图：在真实图片到达前，黑底比透明更符合 Phira 的深色视觉，也能避免
    /// “先闪一下背景再出现图片”的跳变。
    pub fn from_file(file: File) -> Self {
        let notify = Arc::default();
        Self {
            texture: (BLACK_TEXTURE.clone(), BLACK_TEXTURE.clone()),
            notify: Arc::clone(&notify),
            task: Some(Task::new(async move {
                notify.notified().await;
                Ok((file.load_image().await?, None))
            })),
            loaded: Arc::default(),
            load_time: f32::NAN,
        }
    }

    /// 与 [`Illustration::from_file`] 同源，但只取**缩略图**。
    ///
    /// 用于活动列表这类只展示小图的场景：省一次大图下载，也省一份显存。
    pub fn from_file_thumbnail(file: File) -> Self {
        let notify = Arc::default();
        Self {
            texture: (BLACK_TEXTURE.clone(), BLACK_TEXTURE.clone()),
            notify: Arc::clone(&notify),
            task: Some(Task::new(async move {
                notify.notified().await;
                Ok((file.load_thumbnail().await?, None))
            })),
            loaded: Arc::default(),
            load_time: f32::NAN,
        }
    }

    /// 构造一个“已经加载完成”的插图句柄，用于随包内置、没有异步 IO 的资源。
    ///
    /// 此时 `task` 为 `None`（永不结算），但 `load_time` 仍是 `NaN`，
    /// 需要首次 [`Illustration::settle`] 补记时间才能开始淡入——这样内置资源与网络资源
    /// 的出场动画保持一致。
    pub fn from_done(tex: SafeTexture) -> Self {
        Self {
            texture: (tex.clone(), tex),
            notify: Arc::default(),
            task: None,
            loaded: Arc::default(),
            load_time: f32::NAN,
        }
    }

    /// 放行一次加载闸门，唤醒 [`Illustration::task`] 中等待的异步任务。
    ///
    /// 调用时机由页面自己决定：典型做法是在渲染时对本帧**可见**的列表项调用，
    /// 这样滚动到哪里、就加载到哪里。重复调用是安全的（`notify_one` 只会积攒一次许可）。
    pub fn notify(&self) {
        self.notify.notify_one();
    }

    /// 每帧结算一次加载状态：把已完成的任务结果搬进 `texture`，并记录 `load_time` 以启动淡入。
    ///
    /// 三条分支各自对应一种情况（这是本函数最容易被误读的地方）：
    /// - `task.take()` 拿到结果：成功则换纹理并回写 `loaded` 缓存；失败只 `warn` 不 panic——
    ///   插图损坏不应让整个页面崩掉，保持黑图占位即可；
    /// - `task.take()` 返回 `None`（任务未就绪）但缓存里有值：说明同一张图在别处已加载完成，
    ///   直接复用，避免重复解码；
    /// - 本实例根本没有任务（同步资源或已结算过）：仅补记 `load_time`。
    ///
    /// `t` 是逻辑时间（`SharedState::t`），用它与 [`Illustration::alpha`] 的时间轴对齐。
    pub fn settle(&mut self, t: f32) {
        if let Some(task) = &mut self.task {
            if let Some(illu) = task.take() {
                match illu {
                    Err(err) => {
                        warn!(?err, "failed to load illustration");
                    }
                    Ok(illu) => {
                        self.texture = Images::into_texture(illu);
                    }
                };
                *self.loaded.lock().unwrap() = Some(self.texture.clone());
                self.task = None;
                self.load_time = t;
            } else if let Some(loaded) = self.loaded.lock().unwrap().clone() {
                self.texture = loaded;
                self.load_time = t;
                self.task = None;
            }
        } else if self.load_time.is_nan() {
            self.load_time = t;
        }
    }

    /// 计算插图淡入的当前透明度（0 未出现 → 1 完全显示）。
    ///
    /// 尚未拿到纹理时返回 0，保证“黑图占位”不会先于淡入被看见；
    /// 若用户开启了「减弱动态效果」，直接返回 1 跳过淡入——无障碍优先于观感。
    pub fn alpha(&self, t: f32) -> f32 {
        if self.load_time.is_nan() {
            0.
        } else if get_data().prefer_reduced_motion {
            1.
        } else {
            ((t - self.load_time) / Self::TIME).min(1.)
        }
    }

    /// 生成绘制用的着色器：裁剪居中（[`ScaleType::CropCenter`]）铺满 `r`，
    /// 并叠加 [`Illustration::alpha`] 的淡入透明度。
    ///
    /// 用 `CropCenter` 而非拉伸，是为了让不同宽高比的封面保持比例、不产生变形。
    pub fn shading(&self, r: Rect, t: f32) -> impl Shading {
        (*self.texture.0, r, ScaleType::CropCenter, semi_white(self.alpha(t))).into_shading()
    }
}

/// 列表中的一条谱面条目。
///
/// 这是曲库、收藏、合辑等所有谱面列表的公共数据单元：把“展示所需的一切”
/// （摘要信息、封面句柄、来源分类）打包在一起，让各页面只关心布局，
/// 不必各自处理“本地还是远端”的分支。
#[derive(Clone)]
pub struct ChartItem {
    /// 谱面摘要信息（id、标题、难度、作者等展示字段）。
    pub info: BriefChartInfo,
    /// 本地谱面文件路径。为 `None` 表示该条目只有远端记录，需要先下载才能游玩/编辑。
    pub local_path: Option<String>,
    /// 封面句柄（惰性，见 [`Illustration`]）。
    pub illu: Illustration,
    /// 来源分类，决定列表角标与可执行的操作（能否删除、能否上传等）。
    pub chart_type: ChartType,
}
// 列表项与 HTTP 层之间的适配：把“展示用条目”降级为“请求所需的引用”。
// 之所以要降级而不是直接传 `ChartItem`：请求只需要 id 或本地路径，
// 不应把句柄与展示字段带进异步任务里（那会把纹理的引用计数也带进去）。
impl ChartItem {
    /// 转成不带完整信息的 [`ChartRef`]，用于发起下载/打开等只需要 id（或本地路径）的请求。
    pub fn to_bare_ref(&self) -> ChartRef {
        ChartRef::new_bare(self.info.id, self.local_path.as_deref())
    }

    /// 由远端谱面对象构造列表项。
    ///
    /// 只取缩略图（列表场景不需要原图）；`local_path` 故意留空——
    /// 即便该谱面其实已经下载到本地，匹配本地文件也是调用方的责任，
    /// 因为网络层无从判断磁盘上的谱面包属于哪一个 id。
    pub fn from_remote(chart: &Chart) -> Self {
        ChartItem {
            info: chart.to_info(),
            illu: Illustration::from_file_thumbnail(chart.illustration.clone()),
            local_path: None,
            chart_type: ChartType::Downloaded,
        }
    }
}

/// 谱面条目的来源分类。
///
/// 它不参与渲染，但决定列表项上可用的操作集合（例如能否删除本地文件、能否重新下载），
/// 因此不能简单用 `local_path.is_some()` 代替：内置谱面与下载谱面都可能有本地路径，
/// 但前者不允许删除。
#[derive(Clone, Copy)]
pub enum ChartType {
    /// 从服务端下载到本地的谱面。
    Downloaded,
    /// 用户从文件导入到本地的谱面。
    Imported,
    /// 随客户端资源包内置、只读的谱面。
    Integrated,
}

// srange name, isn't it?
/// 页面栈的转场动画器：把一次“入栈/出栈”变成带位移与透明度变化的过渡。
///
/// 之所以叫 Fader：它的核心产物是 [`Fader::progress`] 给出的进度 `p`，调用方用同一个 `p`
/// 同时驱动**纵向位移**与**透明度**，两者组合出页面整体滑出/滑入的观感。
///
/// 同一份 `Fader` 被整栈共享（放在 [`SharedState`] 里），但每个页面在渲染时会通过
/// `sub` 标志拿到方向不同的进度：栈顶元素自下而上滑入，其下方的旧页面则向下滑出。
/// 这正是 `MainScene::render` 里 “先给旧页翻转 `distance` 再渲染，然后临时置 `sub = true`
/// 渲染栈顶页” 的原因。
pub struct Fader {
    /// 过渡期间元素的最大纵向位移量（UI 单位）。旧页渲染时 `MainScene` 会临时把它乘以 `-0.6`，
    /// 让旧页朝反方向、以更小的幅度移动，形成视差错觉而不是硬邦邦的整体平移。
    pub distance: f32,
    /// 过渡的起始时间；`NaN` 是本类型的哨兵值，表示“当前没有过渡在跑”。
    start_time: f32,
    /// 一次完整过渡的时长（秒），默认 0.7，可用 [`Fader::with_time`] 覆写。
    pub time: f32,
    /// 本轮渲染已消费掉的“层”计数。每次 [`Fader::render`] 调用都会自增，
    /// 用来给同一帧内的多个作用域分配递增的延迟（`index * DELTA`），实现层与层之间的错峰动画。
    index: usize,
    /// 方向标志：`true` 表示“返回/出栈”（页面往后收），`false` 表示“进入/入栈”。
    /// [`Fader::done`] 会把它原样返回给 `MainScene`，后者据此判断该弹栈还是该等新页动画结束。
    back: bool,
    /// 本次渲染中是否作为“子层级”（新推入的那一层）参与动画。由 `MainScene` 在渲染栈顶页时
    /// 临时置位（[`Fader::for_sub`]）。置位后进度的符号被反转，使子层与父层相向而行。
    pub sub: bool,
}

// 转场动画的状态机与数学：
// `start_time = NaN` 且 `index = 0` 是“静止”态；`sub()`/`back()` 负责启动一轮过渡并选择方向；
// `done()` 负责判定结束并把方向回报给 `MainScene`；`render()`/`progress()` 只做只读的进度求值。
// 注意 `index` 是“每帧渲染时累加”的，所以调用方必须在每次渲染前 `reset()`，否则延迟会越积越大。
impl Fader {
    /// 同一帧内相邻两层之间的延迟（秒）。用它把“整屏一起动”变成“逐层错峰动”，
    /// 视觉上更像卡片层叠而不是一块整体平移。
    const DELTA: f32 = 0.04;

    /// 创建静止状态的动画器：`start_time = NaN`、方向默认“进入”。
    /// 默认时长 0.7 秒、位移 0.2，两个值都可以用 builder 方法覆写。
    pub fn new() -> Self {
        Self {
            distance: 0.2,
            start_time: f32::NAN,
            time: 0.7,
            index: 0,
            back: false,
            sub: false,
        }
    }

    /// builder 风格地覆写过渡时长；时长越长，整屏的层叠感越强。
    #[inline]
    pub fn with_time(mut self, time: f32) -> Self {
        self.time = time;
        self
    }

    /// builder 风格地覆写最大位移量；传 0 可得到“只淡入淡出、不平移”的过渡。
    #[inline]
    pub fn with_distance(mut self, distance: f32) -> Self {
        self.distance = distance;
        self
    }

    /// 复位层计数。必须在每帧、每个页面开始渲染之前调用，
    /// 否则上一帧累加的 `index` 会让本帧所有作用域都带上一个巨大的延迟。
    #[inline]
    pub fn reset(&mut self) {
        self.index = 0;
    }

    /// 从 `t` 时刻开始一轮“进入/展开”方向的过渡（`back = false`）。
    ///
    /// `MainScene` 在新页面入栈后调用它：旧页向下滑出，新页自下而上滑入。
    #[inline]
    pub fn sub(&mut self, t: f32) {
        self.start_time = t;
        self.back = false;
    }

    /// 在闭包执行期间临时把动画器标记为“子层级”，结束后**无条件**恢复为 `false`。
    ///
    /// 语义是取反进度（见 [`Fader::progress_scaled`]），让闭包内绘制的内容（栈顶页、
    /// 标题、返回按钮）与不在闭包内绘制的内容（下层旧页）朝相反方向运动。
    /// 之所以用闭包而不是直接暴露字段：需要保证异常/提前返回时标记也能被还原。
    #[inline]
    pub fn for_sub<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.sub = true;
        let res = f(self);
        self.sub = false;
        res
    }

    /// 从 `t` 时刻开始一轮“返回/出栈”方向的过渡（`back = true`）。
    ///
    /// 与 [`Fader::sub`] 的区别只在于方向：这里被弹出的页面向下滑出、
    /// 其下方被露出的页面向上归位。`MainScene::pop` 会在真正弹栈**之前**调用它，
    /// 因此动画期间栈顶元素仍在栈里、仍会被渲染。
    #[inline]
    pub fn back(&mut self, t: f32) {
        self.start_time = t;
        self.back = true;
    }

    /// 求取当前过渡进度。返回值的**范围与符号取决于方向和 `sub`**，这是理解整个转场的关键
    /// （`render` 用同一个 `p` 计算 `dy = p * distance` 与 `alpha = 1 - |p|`，因此 `|p|` 越大越透明）：
    ///
    /// | 状态 | `p` 起始 → 结束 |
    /// | --- | --- |
    /// | 无过渡（`start_time` 为 `NaN`） | 恒为 0，即不位移且完全不透明 |
    /// | 进入：`back = false` 且非 `sub`（栈下方的旧页） | 0 → -1 |
    /// | 进入：`back = false` 且 `sub`（刚推入的新页） | 1 → 0 |
    /// | 退出：`back = true` 且非 `sub`（被露出的下层页） | -1 → 0 |
    /// | 退出：`back = true` 且 `sub`（正在被弹出的页） | 0 → 1 |
    ///
    /// 注意 `distance` 的符号由调用方决定：`MainScene` 渲染旧页时会临时把它取反并缩到 0.6 倍，
    /// 两组内容才会相向而行，否则会一起朝同一方向平移。
    ///
    /// 内部步骤：先按经过时间与 `scale` 求出线性进度并夹到 `0..=1`（`scale` 大于 1 时该元素
    /// 走得更快，标题栏就是这样领先于页面内容的）；再取 `(1 - p)^3` 做 ease-out，
    /// 让运动一开始快、接近终点时明显减速；最后按 `back`/`sub` 决定符号。
    ///
    /// 注意 `prefer_reduced_motion` 分支直接把线性进度置为 1：此时所有元素立刻到达终态，
    /// 相当于跳过动画（无障碍要求），因此调用方不需要自己判断这个开关。
    ///
    /// # Arguments
    /// * `t` — 逻辑/真实时间（秒），必须与 `MainScene` 启动过渡时传入的时钟一致
    /// * `scale` — 进度缩放系数，1.0 为正常速度
    pub fn progress_scaled(&self, t: f32, scale: f32) -> f32 {
        if self.start_time.is_nan() {
            0.
        } else {
            // 阶段一：经过时间 → 线性进度（含减速动画开关的短路处理）
            let p = if get_data().prefer_reduced_motion {
                1.
            } else {
                ((t - self.start_time) / self.time * scale).clamp(0., 1.)
            };
            // 阶段二：立方 ease-out，把匀速变为“先快后慢”
            let p = (1. - p).powi(3);
            // 阶段三：选定方向符号（back 决定时间轴正向，sub 决定是否取反）
            let p = if self.back { p } else { 1. - p };
            if self.sub {
                1. - p
            } else {
                -p
            }
        }
    }

    /// 以正常速度（`scale = 1.0`）求取进度，是 [`Fader::progress_scaled`] 的常用简写。
    pub fn progress(&self, t: f32) -> f32 {
        self.progress_scaled(t, 1.)
    }

    /// 把层计数回退一格（饱和到 0），让紧随其后的一次 [`Fader::render`] 与上一次使用**同一个**
    /// 层延迟。
    ///
    /// 用途：当两组内容需要同步动画、不能被 [`Fader::DELTA`] 拉开错峰时
    /// （例如首页把按钮与装饰画在同一层里），先调用本方法抵消上一次的自增。
    pub fn roll_back(&mut self) {
        self.index = self.index.saturating_sub(1);
    }

    /// 以“一层”的身份渲染 `f`：按当前进度施加纵向位移与全局透明度，并把层计数加一。
    ///
    /// 位移与透明度来自同一个 `p`，所以元素一定沿直线平移着淡出/淡入，不会出现“淡完了还在动”的割裂感。
    /// 副作用是 `index` 自增，因此同一帧内的多次调用会依次获得一点额外延迟——
    /// 这正是“多层错峰”效果的实现方式；调用方若想取消延迟需先 [`Fader::reset`] 或 [`Fader::roll_back`]。
    pub fn render<R>(&mut self, ui: &mut Ui, t: f32, f: impl FnOnce(&mut Ui) -> R) -> R {
        let p = self.progress(t - self.index as f32 * Self::DELTA);
        let (dy, alpha) = (p * self.distance, 1. - p.abs());
        self.index += 1;
        ui.scope(|ui| {
            ui.dy(dy);
            ui.alpha(alpha, f)
        })
    }

    /// 是否正处于过渡中（即 `start_time` 已被启动、尚未被 [`Fader::done`] 清除）。
    ///
    /// `MainScene` 用它决定：是否要多驱动一层页面（下方旧页也要 `update`/`render`）、
    /// 是否要拒绝输入（过渡期不响应触摸，避免动画途中重复入栈）。
    #[inline]
    pub fn transiting(&self) -> bool {
        !self.start_time.is_nan()
    }

    /// 判定过渡是否已结束；结束时清除 `start_time` 并**返回本次过渡的方向**。
    ///
    /// # Returns
    /// - `None` — 还在过渡中，请继续等待；
    /// - `Some(true)` — “返回”方向结束，`MainScene` 据此真正弹栈；
    /// - `Some(false)` — “进入”方向结束，新页此前已入栈，无需额外动作。
    ///
    /// `prefer_reduced_motion` 会让判定立即成立（与 [`Fader::progress_scaled`] 的短路保持一致），
    /// 否则动画被跳过但 `done` 永远不返回，栈就会被卡住——两处必须同时处理这个开关。
    pub fn done(&mut self, t: f32) -> Option<bool> {
        if !self.start_time.is_nan() && (t - self.start_time > self.time || get_data().prefer_reduced_motion) {
            self.start_time = f32::NAN;
            Some(self.back)
        } else {
            None
        }
    }

    /// 在标题栏绘制页面标题，并让标题随转场一起滑入/滑出。
    ///
    /// 实现要点（这几处细节都是有原因的，改动前请先读懂）：
    /// - 标题栏只有一行高度，所以用 `ui.scissor` 裁出“以返回按钮中线为轴、高度等于一个字符高”的窄带。
    ///   标题**不做透明度淡入淡出**，而是靠竖直位移走出这条窄带——在裁切之外的部分自然不可见，
    ///   视觉上就等价于“旧标题沉下、新标题升起”；
    /// - 用 `progress_scaled(t, 1.6)` 以 1.6 倍速求进度：标题比页面主体更快到位，
    ///   这样转场过程中标题总是先就位，形成“标题先行”的层级感；
    /// - 字体高度用一个 `"L"` 的实测高度得到，而不是写死常量，以便适配不同字体的度量；
    /// - `"PHIRA"` 是根页面的标题，做两处特判：整体左移一个返回按钮的宽度（根页面不显示返回按钮，
    ///   需要据此重新居中），并在标题右下方补上编译期版本号，方便用户报 bug 时对齐版本。
    pub fn render_title(&mut self, ui: &mut Ui, t: f32, s: &str) {
        // 阶段一：量出标题栏基线与一个字符的高度，作为 scissor 的裁剪带
        let tp = ui.back_rect().center().y;
        let h = ui.text("L").size(1.2).no_baseline().measure_using(&BOLD_FONT).h;
        ui.scissor(Rect::new(-1., tp - h / 2., 2., h), |ui| {
            // 阶段二：按进度求本帧的竖直偏移（p 为正即向下沉，负即向上浮）
            let p = self.progress_scaled(t, 1.6);
            let tp = tp + h * p - h / 2.;
            let mut x = -0.87;
            if s == "PHIRA" {
                x -= ui.back_rect().w;
            }
            // 阶段三：逐字符绘制。之所以不整串绘制，是因为要用每字实测宽度累加固定字距（0.012），
            // 让标题在粗体下也保持均匀疏朗的观感。
            for c in s.chars() {
                x += ui
                    .text(c.to_string())
                    .pos(x, tp)
                    .anchor(0., 0.)
                    .size(1.2)
                    .color(WHITE)
                    .draw_using(&BOLD_FONT)
                    .w
                    + 0.012;
            }
            // 阶段四：根页面标题附带的版本号（低透明度小字，不参与字距累加）
            if s == "PHIRA" {
                ui.text(concat!('v', env!("CARGO_PKG_VERSION")))
                    .pos(x + 0.01, tp + h - 0.027)
                    .anchor(0., 1.)
                    .color(semi_white(0.4))
                    .size(0.5)
                    .draw_using(&BOLD_FONT);
            }
        });
    }
}

/// “页面 → 场景”切换时使用的整屏遮罩淡出器（Shading Fader）。
///
/// 与 [`Fader`] 的区别：`Fader` 负责页面栈内部的层叠滑动，只在 `MainScene` 内部生效；
/// 而本类型负责**跨越页面边界**的切换——要跳去曲目页、用户页等完全不同的场景时，
/// 先把整屏压黑，等黑到看不出来的时候再真正把新场景交给引擎，避免露出两套 UI 交叠的一帧。
///
/// 典型的驱动顺序（以 [`message::MessagePage`] 为例）：
/// 1. 事件触发时调用 [`SFader::goto`] / [`SFader::next`]，记下开始时间与待切换的场景；
/// 2. 每帧 [`SFader::render`] 把遮罩从透明渐变到全黑；
/// 3. 每帧 [`SFader::next_scene`] 反复询问“可以切了吗”，到点后**只返回一次**场景并清空内部状态。
pub struct SFader {
    /// 遮罩动画的起始时间；`NaN` 表示空闲（同时也是“尚未启动”的哨兵）。
    time: f32,
    /// 待切换的目标场景。`None` 表示当前正处于“从黑屏淡入”的回场阶段。
    next_scene: Option<NextScene>,
}

// 遮罩淡出器的状态机：`goto`/`next`/`enter` 三个入口都只是“记录起始时间 + 设定目标”，
// 真正的推进全部集中在 `render`（画遮罩）与 `next_scene`（交还场景）里，
// 这样调用方每个渲染帧调用它们即可，不需要自己维护计时。
impl SFader {
    /// 一次遮罩淡出/淡入的时长（秒）。
    ///
    /// 取值 0.35：比页面栈转场（[`Fader`] 默认 0.7）短一半，因为整屏压黑本身就很“重”，
    /// 时间长了会让人误以为卡顿；同时它必须长于一次场景初始化的最短耗时，否则会闪。
    const TIME: f32 = 0.35;

    /// 创建空闲状态的淡出器：`time = NaN`、无待切换场景。
    pub fn new() -> Self {
        Self {
            time: f32::NAN,
            next_scene: None,
        }
    }

    /// 是否正在遮罩动画中（等价于“本轮还没结束”）。用于屏蔽输入、防止重复触发切换。
    pub fn transiting(&self) -> bool {
        !self.time.is_nan()
    }

    /// 从 `t` 时刻开始淡出，并把 `scene` 作为目标场景。
    ///
    /// 这里把场景包成 [`NextScene::Overlay`]（覆盖而非替换），
    /// 因此返回时底下的 `MainScene` 仍然存活，用户返回后能直接回到原来的页面栈。
    pub fn goto(&mut self, t: f32, scene: impl Scene + 'static) {
        self.time = t;
        self.next_scene = Some(NextScene::Overlay(Box::new(scene)));
    }

    /// 与 [`SFader::goto`] 相同，但直接接受已经构造好的 [`NextScene`]，
    /// 供需要在“压黑之后”才决定目标（例如先等异步结果）的调用方使用。
    pub fn next(&mut self, t: f32, next: NextScene) {
        self.time = t;
        self.next_scene = Some(next);
    }

    /// 启动一次“从黑屏淡入”：不挂目标场景，只让本页/本场景从全黑渐渐显现。
    ///
    /// 典型用法是从别的场景返回本页时调用（`MainScene::enter` 路径），
    /// 让回场也有一个柔和的过渡，而不是瞬间切换。
    pub fn enter(&mut self, t: f32) {
        self.time = t;
    }

    /// 每帧绘制遮罩：有待切换场景时逐渐压黑，没有时逐渐由黑转透明。
    ///
    /// 两支都依赖 [`SFader::next_scene`] 在到点后把场景取走：一旦取走且进度已满，
    /// 本函数就会把 `time` 复位为 `NaN` 结束动画（也就是回场阶段的终点）。
    pub fn render(&mut self, ui: &mut Ui, t: f32) {
        if self.time.is_nan() {
            return;
        }
        let p = if get_data().prefer_reduced_motion {
            1.
        } else {
            ((t - self.time) / Self::TIME).min(1.)
        };
        if p >= 1. && self.next_scene.is_none() {
            self.time = f32::NAN;
        } else {
            ui.fill_rect(ui.screen_rect(), semi_black(if self.next_scene.is_some() { p } else { 1. - p }));
        }
    }

    /// 到点后交还待切换场景；未到点或已被取走时返回 `None`。
    ///
    /// 用 `t >= self.time + TIME`（而非 `>`）判定，保证在“刚好等于时长”的那一帧就能切换，
    /// 不会因为浮点恰好落在边界上而多黑一帧。
    pub fn next_scene(&mut self, t: f32) -> Option<NextScene> {
        if t >= self.time + Self::TIME {
            self.next_scene.take()
        } else {
            None
        }
    }
}

/// 所有页面共享的运行时状态。
///
/// 它是页面栈的“公共黑板”：每个页面在每个回调里都会拿到 `&mut SharedState`，
/// 因此它承载的必须是**跨页面共享且必须全局唯一**的东西——时间轴、转场动画器、
/// 本地谱面缓存、段位图标。页面私有的 UI 状态（按钮、滚动位置、异步任务）绝不能放进来。
pub struct SharedState {
    /// 游戏内时间（秒，`TimeManager::now`）。会受暂停与速度倍率影响，用于与音乐/谱面同步的动画与音频判定。
    pub t: f32,
    /// 真实时间（秒，`TimeManager::real_time`）。不受暂停与倍速影响，用于纯 UI 动画与超时判断。
    ///
    /// 两个时间轴并存正是为了让“页面转场动画”在游戏暂停时依然能跑完。
    pub rt: f32,
    /// 整栈共享的转场动画器，见 [`Fader`]。
    pub fader: Fader,
    /// 本地谱面的列表缓存。
    ///
    /// 它是 `get_data().charts` 的**投影副本**，需要在使用前保持有效；
    /// 一旦 `get_data().charts` 被修改（导入、删除、下载完成），必须调用
    /// [`SharedState::reload_local_charts`] 重建，否则页面会读到过期数据。
    pub charts_local: Vec<ChartItem>,

    /// 8 个段位图标（对应 Phigros 的 1~8 段），由 [`prpr::core::Resource::load_icons`] 加载。
    ///
    /// 放在共享状态里的原因：曲目页、活动页、个人页都要用同一套图标，
    /// 而纹理加载成本高，重复加载会浪费显存。
    pub icons: [SafeTexture; 8],
}

// 两个 thread_local 都服务于“粗体字体热替换”这一个需求：
// `FALLBACK` 保存回退字体（缺字时使用），`BOLD_FONT_CKSUM` 保存当前粗体字体的校验和。
// 之所以用 thread_local 而不是普通 static：字体绘制器内部持有 `!Send` 的 GPU 资源，
// 只能与 `prpr::core::BOLD_FONT` 一样按线程存放。
thread_local! {
    static FALLBACK: RefCell<Option<FontArc>> = RefCell::default();
    pub static BOLD_FONT_CKSUM: RefCell<Option<String>> = RefCell::default();
}

/// 计算数据的 SHA-256 十六进制摘要。
///
/// 用途：作为粗体字体的“内容指纹”上报给服务端做条件请求（未变更则返回 304，不重复下载），
/// 因此这里必须对**完整文件内容**求摘要，不能改成文件名或长度之类更省的标识。
fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// 解析字体字节并同时算出其校验和，返回 `(字体, 校验和)`。
///
/// 校验和与字体绑定返回，是为了避免调用方“加载了却忘了记录指纹”而导致
/// 后续的条件请求永远判定为“已变更”。
///
/// # Errors
/// 字节流不是合法字体（`FontArc::try_from_vec` 失败）时返回错误。
fn load_font_with_cksum(data: Vec<u8>) -> Result<(FontArc, String)> {
    let cksum = sha256(&data);
    Ok((FontArc::try_from_vec(data)?, cksum))
}

/// 把新字体安装为全局粗体字体：同时更新绘制器（`prpr::core::BOLD_FONT`）与指纹。
///
/// 绘制器需要显式传入回退字体（[`FALLBACK`]），因为粗体字库可能缺少中日韩字形，
/// 缺字时必须退回用户所选语言的字体，否则会显示成方块。
fn set_bold_font((font, cksum): (FontArc, String)) {
    BOLD_FONT.with(move |it| *it.borrow_mut() = Some(TextPainter::new(font, FALLBACK.with(|it| it.borrow().clone()))));
    BOLD_FONT_CKSUM.with(move |it| *it.borrow_mut() = Some(cksum));
}

// 共享状态的构建、时间刷新与缓存失效三件事。
// 注意这里**不做**任何页面相关的初始化：`new` 只负责字体与图标这类“进程级一次”的资源，
// 谱面缓存刻意留空，交给需要它的页面按需重建（见 `reload_local_charts`）。
impl SharedState {
    /// 创建共享状态，并完成全局字体的安装。
    ///
    /// # Arguments
    /// * `fallback` — 用户所选语言的回退字体，用于粗体字库缺字时兜底
    ///
    /// 流程（顺序有意义）：
    /// 1. 先登记回退字体，因为后续构造绘制器时要立刻用到它；
    /// 2. 优先读取本地已缓存的粗体字体：它可能是用户手动放置或上一轮热更新下载的版本；
    /// 3. 缓存缺失或损坏（读取失败、不是合法字体）则退回随包资源 `bold.ttf`——
    ///    这里刻意用 `.ok()` 吞掉错误而不是 `?`，因为字体缓存损坏不该导致应用无法启动；
    /// 4. 安装字体并记录指纹；指纹会在首页发起的字体更新检查里作为条件请求参数使用。
    ///
    /// # Errors
    /// 随包字体也加载失败，或段位图标资源加载失败时返回错误（这两者属于安装损坏，必须显式失败）。
    pub async fn new(fallback: FontArc) -> Result<Self> {
        FALLBACK.with(|it| *it.borrow_mut() = Some(fallback));
        let path: PathBuf = dir::bold_font_path()?.into();
        let mut font = None;
        if path.exists() {
            font = std::fs::read(&path).ok().and_then(|it| load_font_with_cksum(it).ok());
        }
        let loaded = match font {
            Some(it) => it,
            None => load_font_with_cksum(load_file("bold.ttf").await?)?,
        };
        set_bold_font(loaded);
        // `charts_local` 故意从空开始：扫描本地谱面有成本，且应用启动时还没有页面需要它。
        Ok(Self {
            t: 0.,
            rt: 0.,
            fader: Fader::new(),
            charts_local: Vec::new(),

            icons: Resource::load_icons().await?,
        })
    }

    /// 从时间管理器刷新两条时间轴。
    ///
    /// 由 `MainScene` 在每个回调（`enter`/`update`/`touch`/`pause`/`resume`/`render`）的开始处调用，
    /// 因此页面里读到的 `s.t` / `s.rt` 一定与本次驱动属于同一帧，不需要各自去取时间。
    pub fn update(&mut self, tm: &mut TimeManager) {
        self.t = tm.now() as _;
        self.rt = tm.real_time() as _;
    }

    /// 用共享的 [`Fader`] 渲染一段内容，时间取共享状态的 `t`。
    ///
    /// 这只是语法糖，但它统一了“用哪条时间轴”这个决定：转场动画一律走 `t`，
    /// 页面自定义的动画一律走 `rt`。
    pub fn render_fader<R>(&mut self, ui: &mut Ui, f: impl FnOnce(&mut Ui) -> R) -> R {
        self.fader.render(ui, self.t, f)
    }

    /// 重建本地谱面缓存。
    ///
    /// **何时必须调用**：只要 `get_data().charts` 发生变化就必须重建——导入成功、下载完成、
    /// 删除谱面、调整收藏顺序都属于此列。实践中这个信号由全局标志
    /// `charts_view::NEED_UPDATE` 传播：改动方置位，`LibraryPage::update` 检测到后调用本方法；
    /// `MainScene` 在导入完成后也会直接调用一次。
    ///
    /// 之所以做成显式的“重建”而不是自动失效：本函数会重新构造全部列表项与插图句柄，
    /// 调用频率必须由调用方控制，不能每帧都重建一次。
    pub fn reload_local_charts(&mut self) {
        self.charts_local = load_local();
    }
}

/// 页面向 `MainScene` 提出的“栈操作请求”。
///
/// 页面自己不持有栈、也不直接增删页面（避免在 `update`/`render` 过程中改动正在遍历的栈），
/// 而是把意图写进返回值，由 `MainScene` 在**帧末、且转场动画已结束时**统一执行，
/// 见 `scene/main.rs` 中 `update` 里对 `next_page()` 的匹配。
///
/// 之所以需要 `#[allow(dead_code)]`：各页面只会用到其中一部分变体，
/// 但枚举必须完整定义以免调用方需要判断“不可能出现的分支”。
#[derive(Default)]
#[allow(dead_code)]
pub enum NextPage {
    /// 什么都不做。默认值，绝大多数帧都会返回它。
    #[default]
    None,
    /// 把参数中的页面**压入**栈顶（成为新的栈顶）。原页面不销毁，用户返回时还会回到这里。
    ///
    /// `Box<dyn Page>` 由页面在自身逻辑里构造，因此新页面的依赖（图标、rank 数据等）需要
    /// 在构造时就从当前页面传进去——这也是各页面 `touch` 里到处 `Arc::clone` 的原因。
    Overlay(Box<dyn Page>),
    /// 弹出栈顶页面（回到上一层）。根页面不会被弹出，`MainScene` 会忽略来自根页面的该请求。
    Pop,
}

/// 一屏交互的抽象。
///
/// 实现者只需提供 [`Page::update`]、[`Page::touch`]、[`Page::render`] 与 [`Page::label`]，
/// 其余回调都有默认实现（空操作），因此新页面可以从最简形式起步。
///
/// 生命周期与调用时机（由 `MainScene` 驱动，`pages.last()` 是当前页）：
/// `enter` → 每帧 (`update` → `touch`? → `render` → `render_top`) → `pause`/`resume`（切出/切回应用时）
/// → `exit`（被弹出时）。其中：
/// - `enter` 会在**两类**时机被调用：页面刚入栈时，以及上层页面被弹出、本页重新成为栈顶时。
///   因此它是“重新激活”的语义，不只是“创建后第一次”，重新进入时要负责复位动画状态；
/// - `exit` 只在真正出栈时调用一次，适合做资源释放与数据持久化；
/// - 转场动画进行期间，栈顶与其下方一页会**同时**收到 `update`/`render`，
///   所以这些方法不能假设自己一定在栈顶，也不能因为被调用两次而破坏状态。
pub trait Page {
    /// 本页在标题栏显示的文案。
    ///
    /// 每帧都会被 `Fader::render_title` 读取（旧页与新页的标题要同时绘制），
    /// 因此实现要么返回 `Cow::Borrowed` 静态串，要么承担一次分配的成本。
    fn label(&self) -> Cow<'static, str>;

    /// 进入本页时是否允许主菜单 BGM 继续播放。
    ///
    /// 返回 `false` 时 `MainScene` 会把 BGM 淡出（[`offset::OffsetPage`] 就是靠它静音），
    /// 返回 `true` 且上一层原本允许播放时则淡入。默认 `true`。
    fn can_play_bgm(&self) -> bool {
        true
    }
    /// 收到下层返回结果时的回调。
    ///
    /// 传递链是：某个子场景/子页面以 `NextScene::PopWithResult(Box<dyn Any>)` 退出，
    /// 引擎把这个不透明结果交给回到栈顶的 `MainScene`，`MainScene::on_result` 再转交给
    /// 页面栈顶的**本方法**——即“上层界面替用户做了选择，回来通知下层界面刷新”。
    ///
    /// 实现时需要 `downcast` 成自己认识的具体类型，认不出来就原样忽略
    /// （[`library::LibraryPage::on_result`] 就是把这个 `Any` 当 `bool` 解出“是否删除”的例子）。
    /// 默认实现直接丢弃结果。
    fn on_result(&mut self, _result: Box<dyn Any>, _s: &mut SharedState) -> Result<()> {
        Ok(())
    }
    /// 页面被激活时调用（入栈，或上层被弹出后重新成为栈顶）。
    ///
    /// 适合在这里启动异步加载、重播音频、复位入场动画；不适合做重活（它可能每返回一层就被调用一次）。
    /// # Errors
    /// 启动必要资源失败时返回错误，由 `MainScene` 向上传播并终止当帧。
    fn enter(&mut self, _s: &mut SharedState) -> Result<()> {
        Ok(())
    }
    /// 每帧的逻辑推进（**必需实现**）。
    ///
    /// 无论是否是栈顶都会在“本页处于可见范围内”时被调用，所以这里只应更新状态、
    /// 读取异步任务结果，不要做输入处理（输入在 [`Page::touch`]）。
    /// # Errors
    /// 逻辑失败（如保存失败、任务返回错误）时返回错误。
    fn update(&mut self, s: &mut SharedState) -> Result<()>;
    /// 处理一个触摸事件（**必需实现**）。
    ///
    /// # Arguments
    /// * `touch` — 本帧的触摸事件（含 id、相位、位置）
    /// * `s` — 共享状态
    ///
    /// # Returns
    /// `true` 表示本事件已被页面消费，`MainScene` 不会再把它转给返回按钮等其他处理者；
    /// `false` 表示放行（例如点在空白处）。
    ///
    /// 注意 `MainScene` 在转场动画期间会整体跳过输入，因此实现里不必再判断动画状态。
    /// # Errors
    /// 触摸处理中触发失败（如保存失败）时返回错误。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool>;
    /// 绘制本页主体（**必需实现**）。
    ///
    /// 内容会被 `MainScene` 施加转场用的位移与透明度；实现里通常把绘制包在
    /// [`SharedState::render_fader`] 里，否则转场时本页不会跟着动。
    /// # Errors
    /// 绘制过程中不可恢复的失败。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()>;
    /// 在整屏最上层绘制的覆盖层（全屏加载指示、放大转场、弹出菜单等）。
    ///
    /// 与 [`Page::render`] 分开是为了控制层次：该回调在 `MainScene` 绘制完页面主体、
    /// 标题栏与返回按钮**之后**才被调用，因此它能盖住包括标题栏在内的所有全局 UI——
    /// 这正是章节卡片放大动画能遮住标题栏的原因。默认空实现。
    /// # Errors
    /// 绘制过程中不可恢复的失败。
    fn render_top(&mut self, _ui: &mut Ui, _s: &mut SharedState) -> Result<()> {
        Ok(())
    }
    /// 应用被切到后台（失去焦点）时调用。
    ///
    /// 实现应暂停自己持有的音频/计时器，并**持久化**尚未保存的改动——
    /// 移动端进程可能被系统直接回收，来不及走到 [`Page::exit`]。默认空实现。
    /// # Errors
    /// 保存失败等错误。
    fn pause(&mut self) -> Result<()> {
        Ok(())
    }
    /// 应用回到前台时调用。与 [`Page::pause`] 对称，负责恢复音频与计时器。默认空实现。
    /// # Errors
    /// 恢复失败（如音频设备被占用）。
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
    /// 本页希望执行的栈操作，每帧由 `MainScene` 查询一次。
    ///
    /// 实现应当“取出即清空”（用 `Option::take` + `unwrap_or_default`），
    /// 否则同一个请求会在后续每一帧被重复执行。默认返回 [`NextPage::None`]。
    fn next_page(&mut self) -> NextPage {
        NextPage::None
    }
    /// 本页希望切换到的**场景**（离开页面栈，进入曲目页/活动页等）。
    ///
    /// 与 [`Page::next_page`] 的区别：`NextPage` 变的是页面栈内部，`NextScene` 会由引擎接管渲染，
    /// 通常还需要配合 [`SFader`] 先把屏幕压黑再返回，避免露出两套 UI 的交叠帧。默认不切换。
    fn next_scene(&mut self, _s: &mut SharedState) -> NextScene {
        NextScene::None
    }
    /// 页面被弹出、即将销毁时调用。
    ///
    /// 适合释放音频、停止后台任务、落盘数据。注意它**只**在出栈时触发一次，
    /// 与“被上层页面遮住”（不会触发任何回调）不是一回事。默认空实现。
    /// # Errors
    /// 清理或保存失败。
    fn exit(&mut self) -> Result<()> {
        Ok(())
    }
    /// 系统返回键（或界面左上返回按钮）被按下时询问本页是否自行处理。
    ///
    /// # Returns
    /// - `true` — 本页已经消费了这次返回，`MainScene` 不会弹栈。
    ///   用于“返回”在本页有更细粒度含义的场景，例如活动页在入场动画途中把返回解释为“取消进入”；
    /// - `false`（默认）— 交给 `MainScene` 执行默认行为（弹出栈顶页面）。
    ///
    /// 无论返回什么，`MainScene` 都只在栈深大于 1 时才询问本方法（根页面的返回键用于退出应用）。
    fn on_back_pressed(&mut self, _s: &mut SharedState) -> bool {
        false
    }
}
