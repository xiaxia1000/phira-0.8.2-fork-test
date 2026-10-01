//! 单曲详情页（[`SongScene`]）——Phira 中功能最密集的页面。
//!
//! 本模块承载单曲的“详情 + 全部操作”入口：谱面信息展示与编辑、难度/成绩/排行榜、
//! 标签与评分、收藏、上传与下载、练习模式、offset 调整与解锁动画。
//! 同时提供与 prpr 内核衔接的唯一入游戏接口 [`SongScene::global_launch`]，
//! 以及供导出/上传链路复用的打包工具 [`compress_folder`]。

prpr_l10n::tl_file!("song");

#[cfg(feature = "video")]
use super::UnlockScene;
use super::{
    confirm_delete, confirm_dialog, fs_from_path, gen_custom_dir, import_chart_to, render_ldb, LdbDisplayItem, ProfileScene, ASSET_CHART_INFO,
};
use crate::{
    charts_view::NEED_UPDATE,
    client::{
        basic_client_builder, recv_raw, Chart, ChartRef, ChartRefChartInfo, Client, Collection, CollectionUpdate, Permissions, Ptr, Record, User,
        UserManager, CLIENT_TOKEN,
    },
    data::{BriefChartInfo, LocalChart},
    dir, get_data, get_data_mut,
    icons::Icons,
    page::{
        local_illustration, request_export, resolve_export, take_export, thumbnail_path, ChartItem, ChartType, Fader, Illustration, SFader,
        FAV_UPDATED,
    },
    popup::Popup,
    rate::RateDialog,
    save_data,
    tags::TagsDialog,
};
use ::rand::{thread_rng, Rng};
use anyhow::{bail, Context, Error, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Utc};
use core::f32;
use futures_util::StreamExt;
use inputbox::{InputBox, InputMode};
use macroquad::prelude::*;
use once_cell::sync::Lazy;
use phira_mp_common::{ClientCommand, CompactPos, JudgeEvent, TouchFrame};
use prpr::{
    config::Mods,
    core::{Tweenable, BOLD_FONT},
    ext::{
        open_url, poll_future, rect_shadow, semi_black, semi_white, unzip_into, JoinToString, LocalTask, RectExt, SafeTexture, ScaleType,
        BLACK_TEXTURE,
    },
    fs::{self},
    info::ChartInfo,
    judge::{icon_index, Judge},
    scene::{
        request_file, request_input, return_file, return_input, show_error, show_message, take_file, take_input, BasicPlayer, GameMode, LoadingScene,
        LocalSceneTask, NextScene, RecordUpdateState, SaveFn, Scene, SimpleRecord, UpdateFn, UploadFn,
    },
    task::Task,
    time::TimeManager,
    ui::{button_hit, render_chart_info, ChartInfoEdit, DRectButton, Dialog, LoadingParams, LongTouchState, RectButton, Scroll, Ui, UI_AUDIO},
};
use regex::Regex;
use reqwest::Method;
use sanitize_filename::sanitize;
use sasa::{AudioClip, Frame, Music, MusicParams};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    any::Any,
    borrow::Cow,
    collections::{hash_map, BTreeMap, HashMap, VecDeque},
    fs::File,
    io::{BufWriter, Cursor, Seek, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc, Arc, Mutex, Weak,
    },
    thread_local,
};
use tap::Tap;
use tracing::{error, warn};
use uuid::Uuid;
use walkdir::WalkDir;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

// Things that need to be reloaded for chart info updates
/// 本地谱面“可整体热重载”的资源元组。
///
/// 谱面信息（`info.yml`）一旦变更，音频与插画必须与之一并重建，否则会出现元信息与
/// 资源不一致的中间态；因此这里把四项打包统一传递、统一回填。
/// 字段依次为：本地相对路径（`charts/` 下）、谱面信息、预览音频、插画。
type LocalTuple = (String, ChartInfo, AudioClip, Illustration);

/// 上传前的校验和校验结果信号。
///
/// 对话框监听器是普通闭包，无法借用 `&mut self`，只能把用户选择写进全局原子量，
/// 再由 [`SongScene::update`] 用 `fetch_and(false, ..)` 取出并驱动后续上传流程。
static CONFIRM_CKSUM: AtomicBool = AtomicBool::new(false);
/// “信息仍有未保存改动、仍要上传”是否已被用户确认（本次编辑期只需确认一次）。
static UPLOAD_NOT_SAVED: AtomicBool = AtomicBool::new(false);
/// 「用外部选择的文件覆盖网上谱面」的确认结果，由对话框回传。
static CONFIRM_OVERWRITE: AtomicBool = AtomicBool::new(false);
/// 上传须知对话框的确认结果，置位后由 `update` 启动校验和校验。
static CONFIRM_UPLOAD: AtomicBool = AtomicBool::new(false);
/// 协作者自动补全对话框选择「确认」的结果。
static CONFIRM_AUTOCOMPLETE: AtomicBool = AtomicBool::new(false);
/// 协作者自动补全对话框选择「跳过」的结果：跳过补全直接保存。
static SKIP_AUTOCOMPLETE: AtomicBool = AtomicBool::new(false);
/// 跨场景传递「待处理成绩记录 id」的全局槽。
///
/// 由内核结算时的成绩上传回调（`global_launch` 内的 `upload_fn`）写入服务端返回的记录 id，
/// 其他场景（主界面、档案页等）在需要跳转到刚打完的成绩时读取。初值 `-1` 表示“无待处理记录”。
pub static RECORD_ID: AtomicI32 = AtomicI32::new(-1);

/// Matches any `@name#id (role)` or `@name#id` or `@name (role)` or `@name`.
/// Parentheses may be ASCII `()` or fullwidth `（）`; whitespace before `(` is optional.
/// Groups: 1=name, 2=id (optional), 3=role (optional)
/// 用于解析简介中的协作者 @提及：既可提取“已带 `#id` 的已解析提及”，
/// 也可定位“缺 `#id` 的未解析提及”（交由自动补全补齐）。
static MENTION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"@([^\s#@(（]+)(?:#(\d+))?(?:\s*[（(]([^)）]+)[)）])?").unwrap());

/// Parse all `@name#id` resolved collaborator mentions and return `(id, role)` pairs.
/// 收集简介中“已解析”的协作者（必须带 `#id`），返回 `id -> (角色, 头像按钮)`。
/// 同一 id 多次出现时保留首次条目；若首次无角色、其后某次带角色则升级为带角色，
/// 角色的有无会影响信息抽屉中头像旁的排版（是否渲染副标题）。
fn parse_collaborators(intro: &str) -> BTreeMap<i32, (Option<String>, RectButton)> {
    use std::collections::btree_map::Entry;

    let mut result = BTreeMap::new();
    for (id, role) in MENTION_RE.captures_iter(intro).filter_map(|cap| {
        let id: i32 = cap.get(2)?.as_str().parse().ok()?;
        let role = cap.get(3).map(|m| m.as_str().to_owned());
        Some((id, role))
    }) {
        match result.entry(id) {
            Entry::Vacant(e) => {
                e.insert((role, RectButton::new()));
            }
            Entry::Occupied(mut e) => {
                if e.get().0.is_none() && role.is_some() {
                    e.insert((role, RectButton::new()));
                }
            }
        }
    }
    result
}

/// Find all unresolved `@name` or `@name (role)` mentions (those missing `#id`).
/// Returns `(start, end, name)` byte-offset pairs into `intro`.
/// Processing right-to-left preserves earlier offsets during replacement.
/// 找出所有缺 `#id` 的 `@name` / `@name (role)` 提及，返回 `(起始字节, 结束字节, 名字)`。
/// 之所以要求“从右向左”处理，是因为替换会改变后续文本长度，逆序才能保证更早的偏移仍然有效。
fn find_unresolved_mentions(intro: &str) -> Vec<(usize, usize, String)> {
    MENTION_RE
        .captures_iter(intro)
        .filter_map(|cap| {
            if cap.get(2).is_some() {
                // Already has #id — resolved
                return None;
            }
            let m = cap.get(0)?;
            let name = cap.get(1)?.as_str().to_owned();
            Some((m.start(), m.end(), name))
        })
        .collect()
}

/// 入场淡入时长（秒）；开启「减少动态效果」时返回 `None`，表示“立即显示、不做淡入”。
/// 返回 `Option` 而非固定值，是为了让调用方能区分“立刻显示”与“渐显”两种语义。
fn fade_in_time() -> Option<f32> {
    if get_data().prefer_reduced_motion {
        None
    } else {
        Some(0.3)
    }
}

/// 右侧抽屉滑入/滑出动画时长（秒）；同样受「减少动态效果」影响，返回 `None` 表示瞬时切换。
fn edit_transit() -> Option<f32> {
    if get_data().prefer_reduced_motion {
        None
    } else {
        Some(0.32)
    }
}

/// 用全局音频输出创建并立即播放预览音乐（页面背景试听）。
/// 音量取 `config.volume_music * 0.7`：预览只是试听，压低以免盖过 UI 反馈音。
fn create_music(clip: AudioClip) -> Result<Music> {
    let mut music = UI_AUDIO.with(|it| {
        it.borrow_mut().create_music(
            clip,
            MusicParams {
                amplifier: get_data().config.volume_music * 0.7,
                loop_mix_time: 0.,
                ..Default::default()
            },
        )
    })?;
    music.play()?;
    Ok(music)
}

/// 对解码后的原始音频做“区间截取 + 首尾淡入淡出”，生成预览片段。
///
/// `range` 给定时按秒把它裁剪到 `[begin, end)`，用于只试听谱面高潮段；
/// 之后对首尾各 0.8s 做线性渐变，避免预览播放到边界时出现爆音。
/// `len` 再与半长取小，保证极短音频（如不足 1.6s）不会越界或产生重叠淡变。
fn with_effects((mut frames, sample_rate): (Vec<Frame>, u32), range: Option<(f32, f32)>) -> Result<AudioClip> {
    if let Some((begin, end)) = range {
        frames.drain(((end * sample_rate as f32) as usize).min(frames.len())..);
        frames.drain(..((begin * sample_rate as f32) as usize));
    }
    let len = (0.8 * sample_rate as f64) as usize;
    let len = len.min(frames.len() / 2);
    for (i, frame) in frames[..len].iter_mut().enumerate() {
        let s = i as f32 / len as f32;
        frame.0 *= s;
        frame.1 *= s;
    }
    let st = frames.len() - len;
    for (i, frame) in frames[st..].iter_mut().rev().enumerate() {
        let s = i as f32 / len as f32;
        frame.0 *= s;
        frame.1 *= s;
    }
    Ok(AudioClip::from_raw(frames, sample_rate))
}

/// 从本地谱面目录加载可整体热重载的资源元组（[`LocalTuple`]）。
///
/// 步骤：打开本地目录 → 解码 `info.music` → 校验预览区间不越界 → 生成预览片段 →
/// 触发本地插画异步加载。越界时直接以 `edit-preview-invalid` 报错，
/// 因为这类信息错误若放任进入内核，会导致载入画面卡在音频长度不匹配上。
async fn load_local_tuple(local_path: &str, def_illu: SafeTexture, info: ChartInfo) -> Result<LocalTuple> {
    let dir = prpr::dir::Dir::new(format!("{}/{local_path}", dir::charts()?))?;
    let bytes = dir.read(&info.music)?;
    let (frames, sample_rate) = AudioClip::decode(bytes)?;
    let length = frames.len() as f32 / sample_rate as f32;
    if info.preview_end.unwrap_or(info.preview_start + 1.) > length {
        tl!(bail "edit-preview-invalid");
    }
    let preview = with_effects((frames, sample_rate), Some((info.preview_start, info.preview_end.unwrap_or(info.preview_start + 15.))))?;
    let illu = local_illustration(local_path.to_owned(), def_illu, true);
    illu.notify.notify_one();

    Ok((local_path.to_owned(), info, preview, illu))
}

/// 正在进行中的「谱面下载」任务及其对话框状态。
///
/// 下载本体运行在后台任务里（入口见 [`SongScene::global_start_download`]），本结构既是
/// 页面侧的进度对话框状态，也持有任务句柄与共享信号。任务的存活取决于强引用计数：
/// 一旦本结构被丢弃（用户点取消或页面离开），弱引用升级失败，下载线程据此中止并清理临时文件。
pub struct Downloading {
    /// 目标谱面简述信息；下载完成后会回填服务端返回的 id 等字段。
    info: BriefChartInfo,
    /// 已有本地谱面时表示“原地更新”；`None` 表示全新下载。
    local_path: Option<String>,
    /// 进度未知时旋转动画的上一帧时间戳，用于平滑不定进度动画。
    loading_last: f32,
    /// 取消按钮命中矩形。
    cancel_download_btn: DRectButton,
    /// 当前阶段文案（拉取谱面 / 解压 / 落盘），由下载任务跨线程更新。
    status: Arc<Mutex<Cow<'static, str>>>,
    /// 当前文件下载进度 0.0~1.0；`None` 表示总长度未知，走不定进度动画。
    prog: Arc<Mutex<Option<f32>>>,
    /// 落盘互斥锁：让“重命名为最终目录”与“取消时清理目录”互斥，
    /// 否则可能出现半成品目录被误当成完整谱面。
    atomicity: Arc<Mutex<()>>,
    /// 后台下载任务；`take()` 到结果即表示任务已结束（成功 / 失败 / 取消）。
    task: Task<Result<(LocalChart, LocalTuple)>>,
}

// 下载对话框：负责取消命中、进度渲染，并把任务结果落盘到本地谱面数据。
impl Downloading {
    /// 只处理取消按钮：点击返回 `true`，由 [`SongScene::touch`] 在持有 `atomicity` 锁的
    /// 前提下丢弃本结构（丢弃即释放强引用，任务随之感知取消并清理临时目录）。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        self.cancel_download_btn.touch(touch, t)
    }

    /// 绘制遮罩 + 阶段文案 + 进度环 + 取消按钮。
    /// `loading_last` 会被就地更新，用于进度未知时的循环动画相位。
    pub fn render(&mut self, ui: &mut Ui, t: f32) {
        ui.fill_rect(ui.screen_rect(), semi_black(0.6));
        ui.loading(0., -0.06, t, WHITE, (*self.prog.lock().unwrap(), &mut self.loading_last));
        ui.text(self.status.lock().unwrap().clone())
            .pos(0., 0.02)
            .anchor(0.5, 0.)
            .size(0.6)
            .draw();
        let size = 0.7;
        let r = ui.text(tl!("dl-cancel")).pos(0., 0.12).anchor(0.5, 0.).size(size).measure().feather(0.02);
        self.cancel_download_btn.render_text(ui, r, t, tl!("dl-cancel"), 0.6, true);
    }

    /// 轮询下载任务，并在完成时把结果写入本地数据。
    ///
    /// 返回语义：`Ok(None)` 表示仍在下载；`Ok(Some(None))` 表示失败或取消；
    /// `Ok(Some(Some(tuple)))` 表示成功并带回可热重载的资源元组。
    /// 失败时主动删除目标目录，避免留下损坏谱面被后续逻辑当成有效数据。
    /// “更新已有谱面”与“全新下载”在此分流：前者原地刷新信息，后者推入 `charts`
    /// 列表并置位 [`NEED_UPDATE`] 让列表页重排。
    pub fn check(&mut self) -> Result<Option<Option<LocalTuple>>> {
        if let Some(res) = self.task.take() {
            match res {
                Err(err) => {
                    let path = format!("{}/{}", dir::downloaded_charts()?, self.info.id.unwrap());
                    let path = Path::new(&path);
                    if path.exists() {
                        std::fs::remove_dir_all(path)?;
                    }
                    show_error(err.context(tl!("dl-failed")));
                    Ok(Some(None))
                }
                Ok((chart, tuple)) => {
                    self.info = chart.info.clone();
                    if let Some(local_path) = &self.local_path {
                        // update
                        SongScene::global_update_chart_info(local_path, self.info.clone())?;
                    } else {
                        NEED_UPDATE.store(true, Ordering::Relaxed);
                        self.local_path = Some(chart.local_path.clone());
                        get_data_mut().charts.push(chart);
                    }
                    save_data()?;
                    show_message(tl!("dl-success")).ok();
                    Ok(Some(Some(tuple)))
                }
            }
        } else {
            Ok(None)
        }
    }
}

/// 右侧抽屉当前展示的内容类型；不同类型宽度与交互各不相同，且切换会重置滚动位置。
enum SideContent {
    /// 谱面信息编辑（仅所有者可上传/覆盖）。
    Edit,
    /// 排行榜（分数 / 准确率两种口径）。
    Leaderboard,
    /// 只读信息面板（上传者、协作者、标签、评分等）。
    Info,
    /// mods 开关面板（autoplay、镜像、淡入淡出等）。
    Mods,
}

// 各内容的抽屉宽度：排行榜最宽以容纳名次与分数，信息面板最窄。
impl SideContent {
    /// 返回该内容对应的抽屉宽度（屏幕宽度比例），供布局与“点击抽屉外关闭”判定复用。
    fn width(&self) -> f32 {
        match self {
            Self::Edit => 0.9,
            Self::Leaderboard => 0.94,
            Self::Info => 0.75,
            Self::Mods => 0.8,
        }
    }
}

/// `/chart/{id}/stabilize` 接口的响应体：`status == 0` 表示“审核后直接通过”，
/// 非 0 表示进入待复核流程；界面文案随该状态二选一。
#[derive(Deserialize)]
struct StableR {
    /// 稳定化审核结果状态码。
    status: i8,
}

/// 排行榜中的一行：服务端记录 + 名次 + 本行按钮的交互状态。
#[derive(Deserialize)]
struct LdbItem {
    /// 服务端成绩记录（含玩家、分数、准确率、std 等）。
    #[serde(flatten)]
    pub inner: Record,
    /// 该记录在当前榜单中的名次。
    pub rank: u32,
    /// 点击查看该玩家档案的命中按钮；反序列化时跳过，运行时填充。
    #[serde(skip, default)]
    pub btn: RectButton,
}

/// 单曲详情页场景。
///
/// 职责：展示并编辑谱面信息、难度切换、成绩与排行榜、收藏/评分/标签、下载与上传谱面、
/// 进入练习模式 / offset 调整 / 解锁动画，并最终通过 [`SongScene::global_launch`]
/// 把参数注入 prpr 内核完成对局。
pub struct SongScene {
    /// 背景插画（本地缓存或远端异步加载）；同时充当页面背景与进游戏时的载入图。
    illu: Illustration,

    /// 是否首次进入本页；首帧需对齐淡入起点并触发一次排行榜加载。
    first_in: bool,

    /// 返回按钮。
    back_btn: RectButton,
    /// 页面主按钮：已下载显示“播放”，未下载显示“下载”。
    play_btn: DRectButton,

    /// 全套图标资源（含排行榜名次图标、玩家头像占位等）。
    icons: Arc<Icons>,

    /// 待切换的下一场景（返回、进游戏、跳转档案页等）。
    next_scene: Option<NextScene>,

    /// 当前预览音乐；进入页面即循环播放谱面高潮片段。
    preview: Option<Music>,
    /// 预览音频的异步解码任务。
    preview_task: Option<Task<Result<AudioClip>>>,

    /// 远端谱面实体（`Chart`）的加载任务；离线模式下不发起。
    load_task: Option<Task<Result<Option<Arc<Chart>>>>>,
    /// 加载到的远端谱面实体，用于权限判断（审核/稳定化/上传者）与信息展示。
    entity: Option<Chart>,
    /// 当前谱面简述信息（名称/曲师/谱师/难度/预览区间/id 等）。
    info: BriefChartInfo,
    /// 本地路径；`Some` 表示已下载或为内置谱面，`None` 表示尚未下载。
    local_path: Option<String>,

    /// 正在进行的下载任务（含进度对话框状态）。
    downloading: Option<Downloading>,
    /// 下载进度未知时旋转动画的上一帧时间。
    loading_last: f32,

    /// AC/FC/段位等 8 种评价图标，按 `icon_index` 索引取用。
    rank_icons: [SafeTexture; 8],
    /// 本谱面历史最佳成绩（本地缓存与远端拉取合并后的结果），用于底部展示。
    record: Option<SimpleRecord>,

    /// 向服务端拉取历史最佳成绩的任务。
    fetch_best_task: Option<Task<Result<SimpleRecord>>>,

    /// 右上角“更多操作”弹出菜单。
    menu: Popup,
    /// 打开该菜单的按钮。
    menu_btn: RectButton,
    /// 本帧是否需要在按钮下方展开菜单（延后到渲染阶段执行）。
    need_show_menu: bool,
    /// 「删除本地谱面」确认对话框的回传信号。
    should_delete: Arc<AtomicBool>,
    /// 菜单可选项（按当前权限与谱面状态动态生成，顺序与 `update` 中分支一一对应）。
    menu_options: Vec<&'static str>,

    /// 谱面信息编辑面板（仅在存在本地路径时可打开）。
    info_edit: Option<ChartInfoEdit>,
    /// 打开信息编辑抽屉的按钮。
    edit_btn: RectButton,
    /// 信息编辑面板的滚动状态。
    edit_scroll: Scroll,

    /// 本页选中的 mods，进入游戏时注入内核。
    mods: Mods,
    /// 打开 mods 抽屉的按钮。
    mod_btn: RectButton,
    /// mods 面板滚动状态。
    mod_scroll: Scroll,
    /// 每个 mod 行的按钮及其“本帧被点击”标记（渲染期置位、更新期生效）。
    mod_btns: Vec<(DRectButton, bool)>,

    /// 右侧抽屉当前内容。
    side_content: SideContent,
    /// 抽屉进入/退出的时间基点：正值表示正在进入、负值表示正在退出、
    /// `f32::INFINITY` 表示已完全关闭；用于驱动平移与遮罩动画的进度计算。
    side_enter_time: f32,

    /// 保存（编辑后的）谱面信息任务。
    save_task: Option<Task<Result<LocalTuple>>>,
    /// 上传谱面任务。
    upload_task: Option<Task<Result<BriefChartInfo>>>,

    /// 排行榜数据：`(本人名次(若上榜), 榜单条目列表)`。
    ldb: Option<(Option<u32>, Vec<LdbItem>)>,
    /// 排行榜加载任务。
    ldb_task: Option<Task<Result<Vec<LdbItem>>>>,
    /// 进入排行榜抽屉的按钮（位于底部名次区域）。
    ldb_btn: RectButton,
    /// 排行榜列表滚动状态。
    ldb_scroll: Scroll,
    /// 排行榜数据到达时的渐显控制器。
    ldb_fader: Fader,
    /// “分数/准确率”排序切换按钮。
    ldb_type_btn: DRectButton,
    /// 排行榜当前是否按 std（准度）排序；否则按分数排序。
    ldb_std: bool,

    /// 打开信息抽屉的按钮。
    info_btn: RectButton,
    /// 信息抽屉滚动状态。
    info_scroll: Scroll,

    /// 收藏按钮。
    fav_btn: RectButton,
    /// 收藏按钮的长按检测状态（长按弹出“收藏到哪个合集”菜单）。
    fav_long_touch: LongTouchState,
    /// 收藏夹选择弹出菜单。
    fav_menu: Popup,
    /// 收藏菜单各项对应的合集 uuid，与菜单索引一一对应。
    fav_menu_options: Vec<Uuid>,
    /// 本帧是否需要展开收藏菜单。
    need_show_fav_menu: bool,

    /// 审核类操作（通过/拒绝/删除/稳定化）的任务；成功后展示服务端返回文案。
    review_task: Option<Task<Result<String>>>,
    /// 「删除线上谱面」确认信号（审核路径）。
    chart_should_delete: Arc<AtomicBool>,
    /// 「审核通过」确认信号。
    should_review_approve: Arc<AtomicBool>,

    /// 编辑谱面标签的任务。
    edit_tags_task: Option<Task<Result<()>>>,
    /// 标签编辑对话框（编辑抽屉与审核两种用途共用）。
    tags: TagsDialog,

    /// 评分对话框。
    rate_dialog: RateDialog,
    /// 提交评分的任务。
    rate_task: Option<Task<Result<()>>>,

    /// 「线上有更新、是否覆盖本地」确认信号。
    should_update: Arc<AtomicBool>,

    /// 拉取本人对该谱面评分的任务。
    my_rating_task: Option<Task<Result<i16>>>,
    /// 本人已给出的评分；为 `Some(0)` 时会在结算后按概率主动弹评分框以引导评分。
    my_rate_score: Option<i16>,

    /// 申请稳定化的任务。
    stabilize_task: Option<Task<Result<()>>>,
    /// 「申请稳定化」确认信号。
    should_stabilize: Arc<AtomicBool>,
    /// 「稳定化审核通过（不进入 ranked）」确认信号。
    should_stabilize_approve: Arc<AtomicBool>,
    /// 「稳定化审核通过并进入 ranked」确认信号。
    should_stabilize_approve_ranked: Arc<AtomicBool>,

    /// 进入游戏的任务：产出下一场景（内核 `LoadingScene` / `UnlockScene`）。
    scene_task: LocalTask<Result<NextScene>>,

    /// 信息抽屉中点击上传者头像跳转档案页的按钮。
    uploader_btn: RectButton,

    /// 子场景淡入淡出切换器（如跳转档案页）。
    sf: SFader,
    /// 从游戏返回后的淡入时间基点。
    fade_start: f32,

    /// 内核回填的背景纹理（进游戏时的载入图）；返回本页后用于“落下”过渡动画。
    background: Arc<Mutex<Option<SafeTexture>>>,
    /// 上述过渡动画的起始时间；`NaN` 表示动画未进行。
    tr_start: f32,

    /// 「在网页中打开」按钮。
    open_web_btn: DRectButton,

    // Imported chart for overwriting
    /// 用于覆盖的、用户经文件选择器选中的本地文件路径。
    overwrite_from: Option<String>,
    /// 覆盖操作的异步任务。
    overwrite_task: Option<Task<Result<LocalTuple>>>,

    /// 校验和校验结果：`None` 未校验；`Some(true)` 可直接上传；`Some(false)` 需先提示清空排行榜。
    update_cksum_passed: Option<bool>,
    /// 校验和校验任务。
    update_cksum_task: Option<Task<Result<bool>>>,
    /// 谱面来源类型（内置/下载/自定义），影响 offset 与覆盖等写盘方式。
    chart_type: ChartType,

    /// 是否已收藏的缓存；`None` 表示需重新计算（收藏状态变化后置空）。
    is_fav: Option<bool>,
    /// 切换收藏状态的任务（含需要向服务端同步的合集）。
    toggle_fav_task: Option<Task<Result<(Collection, bool)>>>,

    /// 「放弃未保存的编辑」确认信号。
    confirm_cancel_edit: Arc<AtomicBool>,

    /// 简介中解析出的协作者及其头像按钮。
    collaborators: BTreeMap<i32, (Option<String>, RectButton)>,
    /// 协作者 @提及自动补全任务。
    autocomplete_task: Option<Task<Result<String>>>,

    /// 导出任务的完成信号；导出在独立线程中打包，避免阻塞渲染。
    export_task: Option<mpsc::Receiver<Result<()>>>,
}

// SongScene 的核心实现：构造、下载、排行榜、权限/菜单推导、进入游戏，以及各抽屉的渲染。
impl SongScene {
    /// 构造单曲详情页。
    ///
    /// 按顺序完成：
    /// 1. 若本地路径形如 `download/<id>`，据其回填远端谱面 `id`（下载目录名即线上 id）；
    /// 2. 选择插画来源——本地谱面走 [`local_illustration`] 缓存、远端谱面走异步加载任务、
    ///    否则沿用列表页已解出的插画；
    /// 3. 从本地谱面列表或“仅本地记录”中恢复历史成绩，避免进入页面时成绩闪烁；
    /// 4. 已登录且已知远端 id 时发起历史最佳成绩拉取；
    /// 5. 发起预览音频解码与本人评分拉取（离线模式跳过后者）。
    ///
    /// # Arguments
    /// * `chart` - 列表页传入的谱面条目（含插画与简述信息）
    /// * `local_path` - 本地路径；`None` 表示尚未下载
    /// * `icons` / `rank_icons` - 图标资源与评价图标（由上层缓存后传入）
    /// * `mods` - 上次使用的 mods，在本页继续沿用
    pub fn new(mut chart: ChartItem, local_path: Option<String>, icons: Arc<Icons>, rank_icons: [SafeTexture; 8], mods: Mods) -> Self {
        // 1) 由本地路径反推远端 id：下载目录以线上 id 命名，可据此补全缺失的信息。
        if let Some(path) = &local_path {
            if let Some(id) = path.strip_prefix("download/") {
                chart.info.id = Some(id.parse().unwrap());
            }
        }
        // 2) 选择插画来源：本地谱面走缓存、远端谱面异步拉取、否则沿用列表页结果。
        let illu = if let Some(path) = &chart.local_path {
            let illu = local_illustration(path.clone(), chart.illu.texture.1.clone(), true);
            illu.notify.notify_one();
            illu
        } else if let Some(id) = chart.info.id {
            Illustration {
                texture: chart.illu.texture.clone(),
                notify: Arc::default(),
                task: Some(Task::new({
                    async move {
                        let chart = Ptr::<Chart>::new(id).load().await?;
                        let image = chart.illustration.load_image().await?;
                        Ok((image, None))
                    }
                })),
                loaded: Arc::default(),
                load_time: f32::NAN,
            }
        } else {
            chart.illu
        };
        // 3) 恢复历史成绩：先查本地谱面列表，再回落到仅存于 local_records 的记录。
        let record = get_data()
            .charts
            .iter()
            .find(|it| Some(&it.local_path) == local_path.as_ref())
            .and_then(|it| it.record.clone())
            .or_else(|| local_path.as_ref().and_then(|path| get_data().local_records.get(path).cloned().flatten()));
        // 4) 已登录且已知远端 id 时才拉取历史最佳，避免未登录产生无谓请求。
        let fetch_best_task = if get_data().me.is_some() {
            chart.info.id.map(|id| Task::new(Client::best_record(id)))
        } else {
            None
        };
        // 5) 组装场景；预览解码与评分拉取任务一并在此挂载。
        let id = chart.info.id;
        let offline_mode = get_data().config.offline_mode;
        let icon_star = icons.star.clone();
        Self {
            illu,

            first_in: true,

            back_btn: RectButton::new(),
            play_btn: DRectButton::new(),

            icons,

            next_scene: None,

            preview: None,
            // 本地谱面直接解码文件并按预览区间裁剪；远端谱面拉取服务端已裁剪好的预览音频。
            preview_task: Some(Task::new({
                let local_path = local_path.clone();
                async move {
                    if let Some(path) = local_path {
                        let mut fs = fs_from_path(&path)?;
                        let info = fs::load_info(fs.as_mut()).await?;
                        with_effects(
                            AudioClip::decode(fs.load_file(&info.music).await?)?,
                            Some((info.preview_start, info.preview_end.unwrap_or(info.preview_start + 15.))),
                        )
                    } else {
                        let chart = Ptr::<Chart>::new(id.unwrap()).fetch().await?;
                        with_effects(AudioClip::decode(chart.preview.fetch().await?.to_vec())?, None)
                    }
                }
            })),

            // 离线模式无需远端实体，跳过谱面实体加载任务。
            load_task: if offline_mode {
                None
            } else {
                id.map(|it| Task::new(async move { Ptr::new(it).fetch_opt().await }))
            },
            entity: None,
            info: chart.info,
            local_path,

            downloading: None,
            loading_last: 0.,

            rank_icons,
            record,

            fetch_best_task,

            menu: Popup::new(),
            menu_btn: RectButton::new(),
            need_show_menu: false,
            should_delete: Arc::new(AtomicBool::default()),
            menu_options: Vec::new(),

            info_edit: None,
            edit_btn: RectButton::new(),
            edit_scroll: Scroll::new(),

            mods,
            mod_btn: RectButton::new(),
            mod_scroll: Scroll::new(),
            mod_btns: Vec::new(),

            side_content: SideContent::Edit,
            side_enter_time: f32::INFINITY,

            save_task: None,
            upload_task: None,

            ldb: None,
            ldb_task: None,
            ldb_btn: RectButton::new(),
            ldb_scroll: Scroll::new(),
            ldb_fader: Fader::new().with_distance(0.12),
            ldb_type_btn: DRectButton::new(),
            ldb_std: false,

            info_btn: RectButton::new(),
            info_scroll: Scroll::new(),

            fav_btn: RectButton::new(),
            fav_long_touch: LongTouchState::default(),
            fav_menu: Popup::new().tap_mut(|it| it.set_auto_dismiss(false)),
            fav_menu_options: Vec::new(),
            need_show_fav_menu: false,

            review_task: None,
            chart_should_delete: Arc::default(),
            should_review_approve: Arc::default(),

            edit_tags_task: None,
            tags: TagsDialog::new(false),

            rate_dialog: RateDialog::new(icon_star, false),
            rate_task: None,

            should_update: Arc::default(),

            // 离线模式不拉取评分；否则请求 /chart/{id}/rate 得到本人已给出的分数。
            my_rating_task: if offline_mode {
                None
            } else {
                id.map(|id| {
                    Task::new(async move {
                        #[derive(Deserialize)]
                        struct Resp {
                            score: i16,
                        }
                        let resp: Resp = recv_raw(Client::get(format!("/chart/{id}/rate"))).await?.json().await?;
                        Ok(resp.score)
                    })
                })
            },
            my_rate_score: None,

            stabilize_task: None,
            should_stabilize: Arc::default(),
            should_stabilize_approve: Arc::default(),
            should_stabilize_approve_ranked: Arc::default(),

            scene_task: None,

            uploader_btn: RectButton::new(),

            sf: SFader::new(),
            fade_start: 0.,

            tr_start: f32::NAN,
            background: Arc::default(),

            open_web_btn: DRectButton::new(),

            overwrite_from: None,
            overwrite_task: None,

            update_cksum_passed: None,
            update_cksum_task: None,
            chart_type: chart.chart_type,

            is_fav: None,
            toggle_fav_task: None,

            confirm_cancel_edit: Arc::default(),

            collaborators: BTreeMap::new(),
            autocomplete_task: None,

            export_task: None,
        }
    }

    /// 在本页发起下载：要求已拿到远端谱面实体（否则提示“仍在加载”），
    /// 然后委托 [`SongScene::global_start_download`] 创建任务并置入 `downloading`。
    fn start_download(&mut self) -> Result<()> {
        let chart = self.info.clone();
        let Some(entity) = self.entity.clone() else {
            show_message(tl!("still-loading")).error();
            return Ok(());
        };
        self.loading_last = 0.;
        self.downloading = Some(Self::global_start_download(chart, entity, self.local_path.clone())?);
        Ok(())
    }

    /// 全局下载入口：与页面生命周期解耦地启动一个谱面下载任务。
    ///
    /// 之所以做成不依赖 `self` 的关联函数，是因为下载可能跨越页面切换，且列表页等
    /// 其他入口也要复用同一套逻辑；返回的 [`Downloading`] 由调用方持有——任务的存活
    /// 与进度共享都依赖它持有的强引用，一旦被丢弃即视为“取消”。
    ///
    /// 分阶段流程（`status` 文案随阶段更新，`prog` 回填文件级进度）：
    /// 1. 先在 `data/charts/download/` 下建一个以随机 UUID 命名的临时目录，保证并发下载
    ///    互不覆盖、未完成时也不会被误认为有效谱面；
    /// 2. 把谱面包（zip）下载到内存，边下边按 `Content-Length` 刷新进度；
    /// 3. 解压到临时目录；下载/解压/落盘期间以 `prog_wk.strong_count()` 判断是否被取消；
    /// 4. 改写 `info.yml`，回填服务端 id/时间戳/上传者，使本地谱面与线上一致；
    /// 5. 在 `atomicity` 锁保护下把临时目录重命名为 `download/<id>`（已存在则先清理，
    ///    以支持“原地更新”场景）；若中途被取消则删除临时目录；
    /// 6. 用 [`load_local_tuple`] 解出可热重载资源并组装 [`LocalChart`] 交回页面落库。
    pub fn global_start_download(chart: BriefChartInfo, entity: Chart, local_path: Option<String>) -> Result<Downloading> {
        // 进度（0~1，可能未知）与阶段文案跨线程共享给页面渲染；atomicity 用于落盘互斥。
        let progress = Arc::new(Mutex::new(None));
        let prog_wk = Arc::downgrade(&progress);
        let status = Arc::new(Mutex::new(tl!("dl-status-fetch")));
        let status_shared = Arc::clone(&status);
        let atomicity = Arc::new(Mutex::new(()));
        Ok(Downloading {
            info: chart.clone(),
            local_path,
            loading_last: 0.,
            cancel_download_btn: DRectButton::new(),
            prog: progress,
            status: status_shared,
            atomicity: atomicity.clone(),
            task: Task::new({
                // 阶段一：创建 UUID 临时目录，所有中间产物先写到这里，取消时整体删除。
                let path = format!("{}/{}", dir::downloaded_charts()?, Uuid::new_v4());
                async move {
                    let path = std::path::Path::new(&path);
                    tokio::fs::create_dir(path).await?;
                    let dir = prpr::dir::Dir::new(path)?;

                    let chart = chart;
                    // 单文件下载：按 Content-Length 刷新进度；弱引用升级失败或仅剩任务自身
                    // （`strong_count() == 1`，说明页面已丢弃 `Downloading`）即视为取消并中断。
                    async fn download(mut file: impl Write, url: &str, prog_wk: &Weak<Mutex<Option<f32>>>) -> Result<()> {
                        let Some(prog) = prog_wk.upgrade() else { return Ok(()) };
                        *prog.lock().unwrap() = None;
                        let req = basic_client_builder().build().unwrap().get(url);
                        let req = if let Some(token) = CLIENT_TOKEN.load().as_ref() {
                            req.header("Authorization", format!("Bearer {token}"))
                        } else {
                            req
                        };
                        let res = req.send().await.with_context(|| tl!("request-failed"))?.error_for_status()?;
                        let size = res.content_length();
                        let mut stream = res.bytes_stream();
                        let mut count = 0;
                        while let Some(chunk) = stream.next().await {
                            let chunk = chunk?;
                            file.write_all(&chunk)?;
                            count += chunk.len() as u64;
                            if let Some(size) = size {
                                *prog.lock().unwrap() = Some(count.min(size) as f32 / size as f32);
                            }
                            if prog_wk.strong_count() == 1 {
                                // cancelled
                                break;
                            }
                        }
                        Ok(())
                    }

                    // 阶段二：把谱面包 zip 下载到内存（不落盘，便于取消时零残留）。
                    *status.lock().unwrap() = tl!("dl-status-chart");
                    let mut bytes = Vec::new();
                    download(Cursor::new(&mut bytes), &entity.file.url, &prog_wk).await?;
                    // 阶段三：解压到临时目录；已取消（无人强引用）则跳过。
                    *status.lock().unwrap() = tl!("dl-status-extract");
                    if prog_wk.strong_count() != 0 {
                        unzip_into(Cursor::new(bytes), &dir, false)?;
                    }
                    // 阶段四：改写 info.yml，把服务端元信息写回本地，
                    // 使后续逻辑（上传、更新判定、权限）能读到正确的 id/时间戳/上传者。
                    *status.lock().unwrap() = tl!("dl-status-saving");
                    if let Some(prog) = prog_wk.upgrade() {
                        *prog.lock().unwrap() = None;
                    }
                    let mut info: ChartInfo = serde_yaml::from_reader(dir.open("info.yml")?)?;
                    info.id = Some(entity.id);
                    info.created = Some(entity.created);
                    info.updated = Some(entity.updated);
                    info.chart_updated = Some(entity.chart_updated);
                    info.uploader = Some(entity.uploader.id);
                    serde_yaml::to_writer(dir.create("info.yml")?, &info)?;

                    // 若已被取消：删掉临时目录（不落盘），后续代码仍会走到清理路径。
                    if prog_wk.strong_count() == 0 {
                        // cancelled
                        drop(dir);
                        tokio::fs::remove_dir_all(&path).await?;
                    }

                    // 阶段五：加锁后把临时目录重命名为最终 `download/<id>`；
                    // 目标已存在（更新场景）则先删除，避免 rename 失败。
                    let local_path = format!("download/{}", chart.id.unwrap());
                    let to_path = format!("{}/{local_path}", dir::charts()?);
                    let to_path = Path::new(&to_path);
                    {
                        let _guard = atomicity.lock().unwrap();
                        if to_path.exists() {
                            if to_path.is_file() {
                                std::fs::remove_file(to_path)?;
                            } else {
                                std::fs::remove_dir_all(to_path)?;
                            }
                        }
                        std::fs::rename(path, to_path)?;
                    }

                    // 阶段六：构建可热重载资源与本地谱面记录，交回页面统一落库。
                    let tuple = load_local_tuple(&local_path, BLACK_TEXTURE.clone(), info).await?;

                    Ok((
                        LocalChart {
                            info: entity.to_info(),
                            local_path,
                            record: None,
                            mods: Mods::default(),
                            played_unlock: false,
                        },
                        tuple,
                    ))
                }
            }),
        })
    }

    /// 拉取排行榜（口径由 `ldb_std` 决定）。离线模式或未知远端 id 时直接跳过。
    /// 每次调用都先清空旧数据再重新请求，用于下拉刷新与切换排序。
    fn load_ldb(&mut self) {
        if get_data().config.offline_mode {
            return;
        }
        let Some(id) = self.info.id else { return };
        self.ldb = None;
        let std = self.ldb_std;
        self.ldb_task = Some(Task::new(async move {
            Ok(recv_raw(Client::get(format!("/record/list15/{id}")).query(&[("std", std)]))
                .await?
                .json()
                .await?)
        }));
    }

    /// 用新成绩更新本谱面的历史成绩。
    ///
    /// 优先更新 `charts` 中的本地谱面记录，其次回落到 `local_records`；两者都不存在时
    /// 只更新页面内存（本次会话可见但不持久化）。仅在成绩确实变好（`SimpleRecord::update`
    /// 返回 `true`）时才写盘，避免无意义的 IO。
    fn update_record(&mut self, new_rec: SimpleRecord) -> Result<()> {
        let rec = get_data_mut()
            .charts
            .iter_mut()
            .find(|it| Some(&it.local_path) == self.local_path.as_ref())
            .map(|it| &mut it.record)
            .or_else(|| {
                self.local_path
                    .clone()
                    .map(|path| get_data_mut().local_records.entry(path).or_insert(None))
            });
        let Some(rec) = rec else {
            if let Some(rec) = &mut self.record {
                rec.update(&new_rec);
            } else {
                self.record = Some(new_rec);
            }
            return Ok(());
        };
        if let Some(rec) = rec {
            if rec.update(&new_rec) {
                save_data()?;
            }
        } else {
            *rec = Some(new_rec);
            save_data()?;
        }
        self.record = rec.clone();
        Ok(())
    }

    /// 依据「本地/远端 + 身份 + 权限 + 谱面状态」动态生成“更多操作”菜单项。
    ///
    /// 菜单项是 `&'static str` 的 i18n key，渲染时再翻译；其推入顺序必须与
    /// [`SongScene::update`] 中 `match option` 的分支保持一致，否则会点错功能。
    fn update_menu(&mut self) {
        self.menu_options.clear();
        if self.local_path.as_ref().is_some_and(|it| !it.starts_with(':')) {
            self.menu_options.push("delete");
        }
        if self.info.id.is_some() {
            self.menu_options.push("rate");
        }
        if let Some(local_path) = &self.local_path {
            self.menu_options.push("exercise");
            self.menu_options.push("offset");
            if get_data()
                .charts
                .iter()
                .find(|it| it.local_path == *local_path)
                .is_some_and(|it| it.info.has_unlock && it.played_unlock)
            {
                self.menu_options.push("unlock");
            }
        }
        // 以上为本地/练习/榜单项；以下按当前用户权限与谱面审核状态追加管理类操作。
        let perms = get_data().me.as_ref().map(|it| it.perms()).unwrap_or_default();
        let is_uploader = get_data()
            .me
            .as_ref()
            .is_some_and(|it| Some(it.id) == self.info.uploader.as_ref().map(|it| it.id));
        if self.info.id.is_some() && (perms.contains(Permissions::REVIEW) || perms.contains(Permissions::REVIEW_PECJAM)) {
            if self.entity.as_ref().is_some_and(|it| !it.reviewed && !it.stable_request) {
                self.menu_options.push("review-approve");
                self.menu_options.push("review-deny");
            }
            self.menu_options.push("review-edit-tags");
        }
        if self.info.id.is_some() && is_uploader && self.entity.as_ref().is_some_and(|it| !it.stable && !it.stable_request) {
            self.menu_options.push("stabilize");
        }
        if self.info.id.is_some() && self.entity.as_ref().is_some_and(|it| it.stable_request) && perms.contains(Permissions::STABILIZE_CHART) {
            self.menu_options.push("stabilize-approve");
            self.menu_options.push("stabilize-approve-ranked");
            self.menu_options.push("stabilize-comment");
            self.menu_options.push("stabilize-deny");
        }
        if self.info.id.is_some()
            && self.entity.as_ref().is_some_and(|it| {
                if it.stable {
                    perms.contains(Permissions::DELETE_STABLE)
                } else {
                    is_uploader || perms.contains(Permissions::DELETE_UNSTABLE)
                }
            })
        {
            self.menu_options.push("review-del");
        }
        if self.local_path.as_ref().is_some_and(|it| !it.starts_with(':')) {
            self.menu_options.push("export");
        }
        // 统一翻译为显示文案；顺序与 update 中的分派一一对应。
        self.menu.set_options(self.menu_options.iter().map(|it| tl!(*it).into_owned()).collect());
    }

    /// 从本页进入游戏（普通 / 练习 / offset 调整 / 解锁）。
    ///
    /// 是否播放解锁动画由 `force_unlock`（显式解锁入口）或“普通模式且谱面含未播放过的
    /// 解锁内容”共同决定；随后把本地路径、mods、模式、背景回填槽与当前最佳成绩交给
    /// [`SongScene::global_launch`]，其产出的下一场景由 [`SongScene::update`] 轮询后切换。
    fn launch(&mut self, mode: GameMode, force_unlock: bool) -> Result<()> {
        let local_path = self.local_path.as_ref().unwrap();
        let is_unlock = force_unlock
            || (mode == GameMode::Normal
                && get_data()
                    .charts
                    .iter()
                    .find(|it| it.local_path == *local_path)
                    .is_some_and(|it| it.info.has_unlock && !it.played_unlock));

        // 交给唯一的入游戏接口；任务的产出是下一场景，成绩回填走 `on_result`。
        self.scene_task =
            Self::global_launch(self.info.id, local_path, self.mods, mode, None, Some(self.background.clone()), self.record.clone(), is_unlock)?;

        Ok(())
    }

    /// Phira 与 prpr 内核之间**唯一的入游戏接口**。
    ///
    /// 把本地谱面、配置、玩家信息与三个回调组装成内核可消费的 [`LoadingScene`]
    /// （播放解锁动画时改走 `UnlockScene`），并以 [`LocalSceneTask`] 返回下一场景的异步产出。
    ///
    /// 参数含义：
    /// - `id`：远端谱面 id；`None` 表示纯本地谱面，会覆盖进 `ChartInfo.id` 并影响成绩上传；
    /// - `local_path`：`charts/` 下的相对路径，交给 [`fs_from_path`] 打开为目录或快照 zip；
    /// - `mods`：本页选中的 mods，原样注入 `Config.mods`（部分 mods 会令成绩不计 ranked）；
    /// - `mode`：`Normal`（普通）/`Exercise`（练习）/`TweakOffset`（offset 调整），
    ///   内核据此决定是否记录成绩、是否只回传 offset；
    /// - `client`：联机对局客户端；仅在 live 状态时才会注入 `update_fn`；
    /// - `background_output`：内核回填载入图纹理的输出槽，供返回本页后的过渡动画使用；
    /// - `record`：历史最佳成绩，作为 `BasicPlayer::historic_best` 传给内核用于结算对比；
    /// - `is_unlock`：是否播放解锁动画（决定走 `UnlockScene` 还是 `LoadingScene`）。
    ///
    /// 三个回调的职责与调用时机：
    /// - `upload_fn`：内核上传成绩存档，成功后把服务端记录 id 写入 [`RECORD_ID`] 全局槽；
    /// - `update_fn`：仅联机 live 对局注入，按节流频率回传触摸帧与判定事件（见函数内注释）；
    /// - `save_fn`：本地成绩落库回调，内核产生新成绩时调用，写回 `charts`/`local_records`。
    ///
    /// 返回的 future 先补齐 `ChartInfo`/`Config`/`BasicPlayer`/纹理等，再 `await` 内核场景；
    /// 内核场景结束时产出 [`SimpleRecord`]（或 offset / 错误），由 [`SongScene::on_result`] 回填。
    ///
    /// # Errors
    /// 本地路径无法打开、`info.yml` 解析失败等会直接返回错误；对局运行期错误会作为
    /// `anyhow::Error` 场景结果回传，由 `on_result` 提示并可选切换到离线模式。
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    #[allow(clippy::too_many_arguments)]
    pub fn global_launch(
        id: Option<i32>,
        local_path: &str,
        mods: Mods,
        mode: GameMode,
        client: Option<Arc<phira_mp_client::Client>>,
        background_output: Option<Arc<Mutex<Option<SafeTexture>>>>,
        record: Option<SimpleRecord>,
        is_unlock: bool,
    ) -> Result<LocalSceneTask> {
        // 打开谱面文件系统：本地目录或快照 zip 统一抽象为同一接口。
        let mut fs = fs_from_path(local_path)?;
        // 是否“有资格 ranked”的静态前提：远端谱面或内置谱面（路径以 ':' 开头）。
        let can_rated = id.is_some() || local_path.starts_with(':');
        #[cfg(feature = "video")]
        let local_path = local_path.to_owned();
        // 仅闭源构建真正启用 ranked 判定：离线 / 键盘 / 变速 / 部分 mods 都会令成绩不计 ranked。
        #[cfg(closed)]
        let rated = {
            let config = &get_data().config;
            !config.offline_mode && can_rated && !mods.intersects(Mods::UNRATED) && !config.use_keyboard && config.speed >= 1.0 - 1e-3
        };
        #[cfg(not(closed))]
        let rated = false;
        // 本可 ranked 却未计入时给出提示，避免用户误以为成绩白打。
        if !rated && can_rated && mode == GameMode::Normal {
            show_message(tl!("warn-unrated")).warn();
        }
        // 联机对局回传：仅在客户端处于 live 状态时才为对局注入 update_fn。
        let update_fn = client.and_then(|mut client| {
            let live = client.blocking_state().unwrap().live;
            let token = get_data().tokens.as_ref().map(|it| it.0.clone()).unwrap();
            let addr = get_data().config.mp_address.clone();
            let mut reconnect_task: Option<Task<Result<phira_mp_client::Client>>> = None;
            let update_fn: Option<UpdateFn> = if live {
                Some(Box::new({
                    let mut touch_ids: HashMap<u64, i8> = HashMap::new();
                    let mut touch_last_update: HashMap<i8, f32> = HashMap::new();
                    let mut touches: VecDeque<TouchFrame> = VecDeque::new();
                    let mut judges: VecDeque<JudgeEvent> = VecDeque::new();
                    let mut last_send_touch_time: f32 = 0.;
                    move |t, res, judge| {
                        // 心跳失败即触发自动重连（同一时刻只保留一个重连任务）。
                        if client.ping_fail_count() >= 1 && reconnect_task.is_none() {
                            warn!("lost connection, auto re-connect");
                            let token = token.clone();
                            let addr = addr.clone();
                            reconnect_task = Some(Task::new(async move {
                                let client = phira_mp_client::Client::from_address(&addr).await?;
                                client.authenticate(token).await?;
                                Ok(client)
                            }));
                        }
                        // 轮询重连任务：成功则整体替换客户端，失败则等下次心跳再试。
                        if let Some(task) = &mut reconnect_task {
                            if let Some(res) = task.take() {
                                match res {
                                    Err(err) => {
                                        warn!(?err, "failed to reconnect");
                                    }
                                    Ok(new) => {
                                        warn!("reconnected!");
                                        client = new.into();
                                    }
                                }
                                reconnect_task = None;
                            }
                        }
                        // 触摸帧采集：丢弃 Stationary（位移未变）；Moved 按 20Hz 节流；
                        // 抬起（Ended/Cancelled）以取反后的负 id 表示，与按下区分并回收 id 映射。
                        let points: Vec<_> = Judge::get_touches()
                            .into_iter()
                            .filter_map(|it| {
                                if matches!(it.phase, TouchPhase::Stationary) {
                                    return None;
                                }
                                let len = touch_ids.len();
                                let mut id = match touch_ids.entry(it.id) {
                                    hash_map::Entry::Occupied(val) => *val.get(),
                                    hash_map::Entry::Vacant(place) => *place.insert(len.try_into().ok()?),
                                };
                                if matches!(it.phase, TouchPhase::Moved) && touch_last_update.get(&id).is_some_and(|it| *it as f64 + 1. / 20. >= t) {
                                    return None;
                                }
                                touch_last_update.insert(id, t as f32);
                                if matches!(it.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                                    touch_ids.remove(&it.id);
                                    id = !id;
                                }
                                Some((id, CompactPos::new(it.position.x, it.position.y * res.aspect_ratio)))
                            })
                            .collect();
                        // 非空点集入队，攒够 20 帧或满 1 秒再一并发送。
                        if !points.is_empty() {
                            touches.push_back(TouchFrame { time: t as f32, points });
                        }
                        // 节流发送：即便本时段无操作也补一帧空白帧充当心跳。
                        if last_send_touch_time as f64 + 1. < t || touches.len() > 20 {
                            if touches.is_empty() {
                                touches.push_back(TouchFrame {
                                    time: t as f32,
                                    points: Vec::new(),
                                });
                            }
                            let frames = Arc::new(touches.drain(..).collect());
                            client.blocking_send(ClientCommand::Touches { frames }).unwrap();
                            last_send_touch_time = t as f32;
                        }
                        // 判定事件回传：把内核 `Ok(Judgement)` / `Err(is_perfect)` 映射为联机协议枚举。
                        judges.extend(judge.judgements.borrow_mut().drain(..).map(|it| JudgeEvent {
                            time: it.0 as f32,
                            line_id: it.1,
                            note_id: it.2,
                            judgement: {
                                use phira_mp_common::Judgement::*;
                                use prpr::judge::Judgement as OJ;
                                match it.3 {
                                    Ok(OJ::Perfect) => Perfect,
                                    Ok(OJ::Good) => Good,
                                    Ok(OJ::Bad) => Bad,
                                    Ok(OJ::Miss) => Miss,
                                    Err(true) => HoldPerfect,
                                    Err(false) => HoldGood,
                                }
                            },
                        }));
                        // 判定事件聚合：满 10 条或队首已超 0.6s 即发送，减少网络往返。
                        if judges.len() > 10 || judges.front().is_some_and(|it| it.time + 0.6 < t as f32) {
                            let judges = Arc::new(judges.drain(..).collect());
                            client.blocking_send(ClientCommand::Judges { judges }).unwrap();
                        }
                    }
                }))
            } else {
                None
            };
            update_fn
        });

        // 本地成绩落库回调：内核结算时调用，按本地路径写入 charts 或 local_records。
        let save_fn: Option<SaveFn> = Some(Box::new({
            let local_path = local_path.to_string();
            move |new_rec| -> Result<()> {
                let rec = get_data_mut()
                    .charts
                    .iter_mut()
                    .find(|it| it.local_path == local_path)
                    .map(|it| &mut it.record)
                    .or_else(|| Some(get_data_mut().local_records.entry(local_path.clone()).or_insert(None)))
                    .unwrap();
                if let Some(rec) = rec {
                    if rec.update(&new_rec) {
                        save_data()?;
                    }
                } else {
                    *rec = Some(new_rec);
                    save_data()?;
                }
                Ok(())
            }
        }));

        // 入内核前的准备阶段：加载谱面信息、组装 Config 与玩家信息、构建成绩上传回调。
        Ok(Some(Box::pin(async move {
            // 谱面信息取自本地文件，id 用调用方传入值覆盖（纯本地谱面为 None）。
            let mut info = fs::load_info(fs.as_mut()).await?;
            info.id = id;
            let mut config = get_data().config.clone();
            config.player_name = get_data()
                .me
                .as_ref()
                .map(|it| it.name.clone())
                .unwrap_or_else(|| tl!("guest").into_owned());
            // 资源包路径：0 表示不使用自定义资源包，否则取 respacks 下的对应目录。
            config.res_pack_path = {
                let id = get_data().respack_id;
                if id == 0 {
                    None
                } else {
                    Some(format!("{}/{}", dir::respacks()?, get_data().respacks[id - 1]))
                }
            };
            let chart_updated = info.chart_updated;
            config.mods = mods;
            // 预加载插画（内核载入画面用），并回填给本页作为返回过渡的背景。
            let preload = LoadingScene::load(fs.as_mut(), &info.illustration).await?;
            if let Some(output) = background_output {
                *output.lock().unwrap() = Some(preload.1.clone());
            }
            // 玩家信息：未登录时为 None，内核以游客身份运行（成绩不上传）。
            let player = get_data().me.as_ref().map(|it| BasicPlayer {
                avatar: UserManager::get_avatar(it.id).flatten(),
                id: it.id,
                rks: it.rks,
                historic_best: record.map_or(0, |it| it.score as u32),
            });
            // 成绩上传回调：POST /play/upload；成功后把增益信息回传内核做结算演出。
            let upload_fn: Option<UploadFn> = Some(Arc::new(move |data: Vec<u8>| {
                Task::new(async move {
                    #[derive(Serialize)]
                    #[serde(rename_all = "camelCase")]
                    struct Req {
                        chart: i32,
                        token: String,
                        chart_updated: Option<DateTime<Utc>>,
                    }
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct Resp {
                        id: i32,
                        exp_delta: f64,
                        new_best: bool,
                        improvement: u32,
                        new_rks: f32,
                    }
                    let resp: Resp = recv_raw(Client::post(
                        "/play/upload",
                        &Req {
                            chart: id.unwrap(),
                            token: STANDARD.encode(data),
                            chart_updated,
                        },
                    ))
                    .await?
                    .json()
                    .await?;
                    // 记录本次成绩 id，供其他场景读取后跳转到刚打完的成绩。
                    RECORD_ID.store(resp.id, Ordering::Relaxed);
                    Ok(RecordUpdateState {
                        best: resp.new_best,
                        improvement: resp.improvement,
                        gain_exp: resp.exp_delta as f32,
                        new_rks: Some(resp.new_rks),
                    })
                })
            }));
            // 解锁动画：非 video 构建退化为普通载入；video 构建改走 UnlockScene。
            if is_unlock {
                #[cfg(not(feature = "video"))]
                {
                    warn!("this build does not support unlock video.");
                    LoadingScene::new(mode, info, config, fs, player, upload_fn, update_fn, save_fn, Some(preload))
                        .await
                        .map(|it| NextScene::Overlay(Box::new(it)))
                }
                #[cfg(feature = "video")]
                {
                    let chart = get_data_mut().charts.iter_mut().find(|it| it.local_path == local_path).unwrap();
                    // 首次真正播放解锁动画即标记并落盘，避免下次进入重复播放。
                    if !chart.played_unlock {
                        chart.played_unlock = true;
                        save_data()?;
                    }

                    // 走解锁场景；其返回的下一场景与普通载入一致，结果仍由 `on_result` 回填。
                    UnlockScene::new(mode, info, config, fs, player, upload_fn, update_fn, save_fn, Some(preload))
                        .await
                        .map(|it| NextScene::Overlay(Box::new(it)))
                }
            } else {
                // 普通对局：载入完成后把内核场景作为叠加场景返回，本页在底层等待其结束。
                LoadingScene::new(mode, info, config, fs, player, upload_fn, update_fn, save_fn, Some(preload))
                    .await
                    .map(|it| NextScene::Overlay(Box::new(it)))
            }
        })))
    }

    /// 判断当前用户是否为该谱面所有者：纯本地谱面（无远端 id）或上传者本人。
    fn is_owner(&self) -> bool {
        self.info.id.is_none()
            || (self.info.created.is_some() && self.info.uploader.as_ref().map(|it| it.id) == get_data().me.as_ref().map(|it| it.id))
    }

    /// 收起右侧抽屉：把时间基点置为负值以触发退出动画。
    fn hide_side(&mut self, rt: f32) {
        self.side_enter_time = -rt;
    }

    /// 渲染「编辑信息」抽屉：顶部为取消 / 上传（更新）/ 保存按钮，下方是可滚动的谱面信息表单。
    ///
    /// 上传前会依次做未保存确认、登录校验与内置谱面拦截，通过后弹出上传须知，经
    /// [`CONFIRM_UPLOAD`] 进入校验和核验流程；表单底部另提供标签编辑与「用外部文件覆盖」入口。
    fn side_chart_info(&mut self, ui: &mut Ui, rt: f32) -> Result<()> {
        let h = 0.11;
        let pad = 0.03;
        let width = self.side_content.width() - pad;

        let is_owner = self.is_owner();
        let online = self.info.id.is_some();
        let vpad = 0.02;
        let hpad = 0.01;
        let dx = width / if is_owner { 3. } else { 2. };
        let mut r = Rect::new(hpad, ui.top * 2. - h + vpad, dx - hpad * 2., h - vpad * 2.);
        if ui.button("cancel", r, tl!("edit-cancel")) {
            if self.info_edit.as_ref().is_some_and(|it| it.updated) {
                confirm_dialog(tl!("warn"), tl!("cancel-not-saved"), self.confirm_cancel_edit.clone());
            } else {
                self.hide_side(rt);
            }
        }
        if is_owner {
            r.x += dx;
            if ui.button(
                "upload",
                r,
                if self.info.id.is_none() {
                    tl!("edit-upload")
                } else {
                    tl!("edit-update")
                },
            ) {
                if self.info_edit.as_ref().unwrap().updated && !UPLOAD_NOT_SAVED.load(Ordering::SeqCst) {
                    Dialog::simple(tl!("upload-not-saved"))
                        .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
                        .listener(|_dialog, pos| {
                            if pos == 1 {
                                UPLOAD_NOT_SAVED.store(true, Ordering::SeqCst);
                            }
                            false
                        })
                        .show();
                } else {
                    let path = self.local_path.as_ref().unwrap();
                    if get_data().me.is_none() {
                        show_message(tl!("upload-login-first"));
                    } else if path.starts_with(':') {
                        show_message(tl!("upload-builtin"));
                    } else {
                        self.update_cksum_passed = None;
                        Dialog::plain(tl!("upload-rules"), tl!("upload-rules-content"))
                            .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
                            .listener(|_dialog, pos| {
                                if pos == 1 {
                                    CONFIRM_UPLOAD.store(true, Ordering::SeqCst);
                                }
                                pos == -2
                            })
                            .show();
                    }
                }
            }
        }
        r.x += dx;
        if ui.button("save", r, tl!("edit-save")) {
            self.try_save_with_autocomplete();
        }

        // 只保留落在编辑滚动区内的“按下”事件，避免滑动穿透到下层按钮。
        ui.ensure_touches()
            .retain(|it| !matches!(it.phase, TouchPhase::Started) || self.edit_scroll.contains(it));

        self.edit_scroll.size((width, ui.top * 2. - h));
        // 表单主体：谱面信息编辑表单，底部附标签编辑与（所有者可用的）覆盖入口。
        self.edit_scroll.render(ui, |ui| {
            let (w, mut h) = render_chart_info(ui, self.info_edit.as_mut().unwrap(), width);
            h += 0.06;
            ui.dy(h);
            let mut r = Rect::new(0.04, 0., 0.23, 0.07);
            if ui.button("edit_tags", r, tl!("edit-tags")) {
                self.tags.set(self.info_edit.as_ref().unwrap().info.tags.clone());
                self.tags.enter(rt);
            }
            if is_owner && online {
                r.x += r.w + 0.01;
                if ui.button("overwrite", r, tl!("edit-overwrite")) {
                    request_file("overwrite");
                }
            }
            (w, h + 0.1)
        });
        Ok(())
    }

    /// 渲染排行榜抽屉：右上角排序切换按钮 + 榜单列表。
    ///
    /// 复用通用 [`render_ldb`]，并按当前排序口径把总分或 std 准度填入主列、把准确率或 std(ms)
    /// 填入副列；点击某行会跳转到该玩家的档案页。
    fn side_ldb(&mut self, ui: &mut Ui, rt: f32) {
        let pad = 0.03;
        let width = self.side_content.width() - pad;
        ui.dy(0.03);
        self.ldb_type_btn.render_text(
            ui,
            Rect::new(width - 0.24, 0.01, 0.23, 0.08),
            rt,
            if self.ldb_std { tl!("ldb-std") } else { tl!("ldb-score") },
            0.6,
            true,
        );
        // 复用通用排行榜渲染；分数与副信息随排序口径切换。
        render_ldb(
            ui,
            &tl!("ldb"),
            self.side_content.width(),
            rt,
            &mut self.ldb_scroll,
            &mut self.ldb_fader,
            &self.icons.user,
            self.ldb.as_mut().map(|it| {
                it.1.iter_mut().map(|it| LdbDisplayItem {
                    player_id: it.inner.player.id,
                    rank: it.rank,
                    score: if self.ldb_std {
                        format!("{:07}", it.inner.std_score.unwrap_or(0.) as i64)
                    } else {
                        format!("{:07}", it.inner.score)
                    },
                    alt: Some(if self.ldb_std {
                        format!("{}ms", (it.inner.std.unwrap_or(0.) * 1000.) as i32)
                    } else {
                        format!("{:.2}%", it.inner.accuracy * 100.)
                    }),
                    btn: &mut it.btn,
                })
            }),
        );
    }

    /// 渲染只读信息抽屉：网页入口、上传者与协作者头像、以及谱面各项元信息。
    /// 协作者由简介中的 `@name#id` 解析得到，打开抽屉时会批量请求用户信息。
    fn side_info(&mut self, ui: &mut Ui, rt: f32) {
        let pad = 0.03;
        ui.dx(pad);
        ui.dy(0.03);
        let width = self.side_content.width() - pad;
        self.info_scroll.size((width - pad, ui.top * 2. - 0.06));
        self.info_scroll.render(ui, |ui| {
            let mut h = 0.;
            // 局部宏：累加并推进纵向偏移，同时维护总高度用于滚动区尺寸计算。
            macro_rules! dy {
                ($e:expr) => {{
                    let dy = $e;
                    h += dy;
                    ui.dy(dy);
                }};
            }
            let mw = width - pad * 3.;
            if self.info.id.is_some() {
                let r = Rect::new(0.03, 0., mw, 0.12).nonuniform_feather(-0.03, -0.01);
                self.open_web_btn.render_text(ui, r, rt, ttl!("open-in-web"), 0.6, true);
                dy!(r.h + 0.04);
            }
            if let Some(uploader) = &self.info.uploader {
                let c = 0.06;
                let s = 0.05;
                let r = ui.avatar(c, c, s, rt, UserManager::opt_avatar(uploader.id, &self.icons.user));
                self.uploader_btn.set(ui, Rect::new(c - s, c - s, s * 2., s * 2.));
                if let Some((name, color)) = UserManager::name_and_color(uploader.id) {
                    ui.text(name)
                        .pos(r.right() + 0.02, r.center().y)
                        .anchor(0., 0.5)
                        .no_baseline()
                        .max_width(width - 0.15)
                        .size(0.6)
                        .color(color)
                        .draw();
                }
                dy!(0.14);
            }
            if !self.collaborators.is_empty() {
                dy!(ui.text(tl!("info-collaborators")).size(0.4).color(semi_white(0.7)).draw().h + 0.02);
                for (collab_id, (role, btn)) in &mut self.collaborators {
                    let c = 0.06;
                    let s = 0.05;
                    let r = ui.avatar(c, c, s, rt, UserManager::opt_avatar(*collab_id, &self.icons.user));
                    btn.set(ui, Rect::new(c - s, c - s, s * 2., s * 2.));
                    if let Some((name, color)) = UserManager::name_and_color(*collab_id) {
                        let name_r = ui
                            .text(name)
                            .pos(r.right() + 0.02, r.center().y - if role.is_some() { 0.01 } else { 0. })
                            .anchor(0., 0.5)
                            .no_baseline()
                            .max_width(width - 0.15)
                            .size(0.5)
                            .color(color)
                            .draw();
                        if let Some(role_text) = role {
                            ui.text(role_text.as_str())
                                .pos(r.right() + 0.02, name_r.bottom() + 0.005)
                                .size(0.35)
                                .color(semi_white(0.6))
                                .draw();
                        }
                    }
                    dy!(0.14);
                }
            }

            // 统一的“标题 + 多行内容”条目：标题小号灰字，内容自动换行并推进偏移。
            let mut item = |title: Cow<'_, str>, content: Cow<'_, str>| {
                dy!(ui.text(title).size(0.4).color(semi_white(0.7)).draw().h + 0.02);
                dy!(ui.text(content).pos(pad, 0.).size(0.6).multiline().max_width(mw).draw().h + 0.03);
            };
            item(tl!("info-name"), self.info.name.as_str().into());
            item(tl!("info-composer"), self.info.composer.as_str().into());
            item(tl!("info-charter"), self.info.charter.as_str().into());
            item(tl!("info-difficulty"), format!("{} ({:.1})", self.info.level, self.info.difficulty).into());
            item(tl!("info-desc"), self.info.intro.as_str().into());
            if let Some(entity) = &self.entity {
                item(tl!("info-rating"), entity.rating.map_or(Cow::Borrowed("NaN"), |r| format!("{:.2} / 5.00", r * 5.).into()));
                item(
                    tl!("info-type"),
                    format!(
                        "{}{}",
                        if entity.reviewed { tl!("reviewed") } else { tl!("unreviewed") },
                        match (entity.stable, entity.ranked) {
                            (true, true) => ttl!("chart-ranked"),
                            (true, false) => ttl!("chart-special"),
                            (false, _) => ttl!("chart-unstable"),
                        }
                    )
                    .into(),
                );
                item(tl!("info-tags"), entity.tags.iter().map(|it| format!("#{it}")).join(" ").into());
            }
            if let Some(id) = self.info.id {
                item("ID".into(), id.to_string().into());
            }
            (width, h)
        });
    }

    /// 渲染 mods 抽屉：每行一个开关，点击状态在渲染期记录、更新期（或关闭抽屉时）生效并落盘。
    fn side_mods(&mut self, ui: &mut Ui, rt: f32) {
        let pad = 0.03;
        ui.dx(pad);
        ui.dy(0.03);
        let width = self.side_content.width() - pad;
        self.mod_scroll.size((width - pad, ui.top * 2. - 0.06));
        self.mod_scroll.render(ui, |ui| {
            const ITEM_HEIGHT: f32 = 0.15;
            let mut h = 0.;
            // 局部宏：累加并推进纵向偏移，同时维护总高度用于滚动区尺寸计算。
            macro_rules! dy {
                ($e:expr) => {{
                    let dy = $e;
                    h += dy;
                    ui.dy(dy);
                }};
            }
            dy!(ui.text(tl!("mods")).size(0.9).draw_using(&BOLD_FONT).h + 0.02);
            let rh = ITEM_HEIGHT * 3. / 5.;
            let rr = Rect::new(width - 0.24, (ITEM_HEIGHT - rh) / 2., 0.2, rh);
            let mut index = 0;
            // 每行渲染为“标题（+说明） + 右侧开关”；按钮按索引复用，以保留跨帧动画状态。
            let mut item = |title: Cow<'_, str>, subtitle: Option<Cow<'_, str>>, flag: Mods| {
                const TITLE_SIZE: f32 = 0.6;
                const SUBTITLE_SIZE: f32 = 0.35;
                const LEFT: f32 = 0.03;
                const PAD: f32 = 0.01;
                const SUB_MAX_WIDTH: f32 = 0.46;
                if let Some(subtitle) = subtitle {
                    let r1 = ui.text(Cow::clone(&title)).size(TITLE_SIZE).measure();
                    let r2 = ui
                        .text(Cow::clone(&subtitle))
                        .size(SUBTITLE_SIZE)
                        .max_width(SUB_MAX_WIDTH)
                        .no_baseline()
                        .measure();
                    let h = r1.h + PAD + r2.h;
                    ui.text(subtitle)
                        .pos(LEFT, (ITEM_HEIGHT + h) / 2. - r2.h)
                        .size(SUBTITLE_SIZE)
                        .max_width(SUB_MAX_WIDTH)
                        .multiline()
                        .color(semi_white(0.6))
                        .draw();
                    ui.text(title).pos(LEFT, (ITEM_HEIGHT - h) / 2.).no_baseline().size(TITLE_SIZE).draw();
                } else {
                    ui.text(title)
                        .pos(LEFT, ITEM_HEIGHT / 2.)
                        .anchor(0., 0.5)
                        .no_baseline()
                        .size(TITLE_SIZE)
                        .draw();
                }
                if self.mod_btns.len() <= index {
                    self.mod_btns.push(Default::default());
                }
                let (btn, clicked) = &mut self.mod_btns[index];
                if *clicked {
                    *clicked = false;
                    self.mods.toggle_mod(flag);
                }
                let on = self.mods.contains(flag);
                let oh = rr.h;
                btn.build(ui, rt, rr, |ui, path| {
                    let ct = rr.center();
                    ui.fill_path(&path, if on { WHITE } else { ui.background() });
                    ui.text(if on { ttl!("switch-on") } else { ttl!("switch-off") })
                        .pos(ct.x, ct.y)
                        .anchor(0.5, 0.5)
                        .no_baseline()
                        .size(0.5 * (1. - (1. - rr.h / oh).powf(1.3)))
                        .max_width(rr.w)
                        .color(if on { Color::new(0.3, 0.3, 0.3, 1.) } else { WHITE })
                        .draw();
                });
                dy!(ITEM_HEIGHT);
                index += 1;
            };
            item(tl!("mods-autoplay"), Some(tl!("mods-autoplay-sub")), Mods::AUTOPLAY);
            item(tl!("mods-flip-x"), Some(tl!("mods-flip-x-sub")), Mods::FLIP_X);
            item(tl!("mods-fade-in"), Some(tl!("mods-fade-in-sub")), Mods::FADE_IN);
            item(tl!("mods-fade-out"), Some(tl!("mods-fade-out-sub")), Mods::FADE_OUT);
            item(tl!("mods-nightcore"), Some(tl!("mods-nightcore-sub")), Mods::NIGHTCORE);
            item(tl!("mods-rainbow"), Some(tl!("mods-rainbow-sub")), Mods::RAINBOW);
            item(tl!("mods-instant-death-ap"), Some(tl!("mods-instant-death-ap-sub")), Mods::INSTANT_DEATH_AP);
            item(tl!("mods-instant-death-fc"), Some(tl!("mods-instant-death-fc-sub")), Mods::INSTANT_DEATH_FC);
            item(tl!("mods-no-shader"), Some(tl!("mods-no-shader-sub")), Mods::NO_SHADER);
            item(tl!("mods-maintain-flowing-rate"), Some(tl!("mods-maintain-flowing-rate-sub")), Mods::MAINTAIN_FLOWING_RATE);
            (width, h + 0.2)
        });
    }

    /// 保存信息编辑：先做本地文本敏感词检查（线上另有服务端校验），再把表单差异写回磁盘
    /// 并重建可热重载资源；非所有者若改动了谱面本体则拒绝保存，避免覆盖他人劳动成果。
    fn save_edit(&mut self) {
        let Some(edit) = &self.info_edit else { unreachable!() };
        let info = edit.info.clone();
        // Offline moderation of locally-edited chart metadata. Cloud uploads are
        // checked server-side, but this text is written to the local info.yml.
        // 本地落盘前的文本审查：线上另有服务端校验，但这份文本会写进本地 info.yml，仍需本地把关。
        {
            let mut texts = vec![
                info.name.as_str(),
                info.level.as_str(),
                info.charter.as_str(),
                info.composer.as_str(),
                info.illustrator.as_str(),
                info.intro.as_str(),
            ];
            if let Some(tip) = &info.tip {
                texts.push(tip.as_str());
            }
            texts.extend(info.tags.iter().map(String::as_str));
            if let Err(err) = crate::censor::check_texts(texts) {
                show_message(err.to_string()).error();
                return;
            }
        }
        let path = self.local_path.clone().unwrap();
        let edit = edit.clone();
        let is_owner = self.is_owner();
        let def_illu = self.illu.texture.1.clone();
        self.save_task = Some(Task::new(async move {
            let dir = prpr::dir::Dir::new(format!("{}/{path}", dir::charts()?))?;
            let patches = edit.to_patches().await.with_context(|| tl!("edit-load-file-failed"))?;
            if !is_owner && patches.contains_key(&info.chart) {
                bail!(tl!("edit-downloaded"));
            }
            for (name, bytes) in patches.into_iter() {
                dir.create(name)?.write_all(&bytes)?;
            }
            let _ = std::fs::remove_file(thumbnail_path(&path)?);
            load_local_tuple(&path, def_illu, info).await
        }));
    }

    /// 保存前的预处理：若简介中还有未解析的 `@name` 提及，先询问是否自动补全；
    /// 用户确认则走 [`SongScene::start_autocomplete`]，选择跳过则直接保存。
    fn try_save_with_autocomplete(&mut self) {
        let intro = &self.info_edit.as_ref().unwrap().info.intro;
        let unresolved = find_unresolved_mentions(intro);
        if unresolved.is_empty() {
            self.save_edit();
        } else {
            let mentions_list = unresolved
                .iter()
                .map(|(start, end, _)| &intro[*start..*end])
                .collect::<Vec<_>>()
                .join(", ");
            let content = tl!("collab-autocomplete-content", "mentions" => mentions_list);
            Dialog::plain(tl!("collab-autocomplete-title"), content)
                .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
                .listener(|_dialog, pos| {
                    if pos == 1 {
                        CONFIRM_AUTOCOMPLETE.store(true, Ordering::SeqCst);
                    } else if pos == 0 {
                        SKIP_AUTOCOMPLETE.store(true, Ordering::SeqCst);
                    }
                    false
                })
                .show();
        }
    }

    /// 把简介中未解析的 `@name` 提及逐个补全为 `@name#id`：查询首个精确同名用户，
    /// 任一提及无法解析即整体失败；替换从右向左进行，避免前面的字节偏移被破坏。
    fn start_autocomplete(&mut self) {
        let intro = self.info_edit.as_ref().unwrap().info.intro.clone();
        self.autocomplete_task = Some(Task::new(async move {
            let unresolved = find_unresolved_mentions(&intro);
            // Resolve each mention (bail on first failure), then apply right-to-left
            // so earlier byte offsets stay valid.
            // 逐个解析提及（首个失败即整体失败），并逆序回填，保证更早的字节偏移仍然有效。
            let mut resolved: Vec<(usize, usize, String)> = Vec::new();
            for (start, end, name) in unresolved {
                let name_owned = name.clone();
                let (users, _) = Client::query::<User>().search(name_owned).send().await?;
                let matched = users.into_iter().find(|u| u.name == name);
                let Some(user) = matched else {
                    bail!(tl!("collab-autocomplete-failed", "name" => name));
                };
                // Replacement: insert `#id` right after `@name`, keep the rest of
                // the original match (bracket/role if any).
                let suffix = &intro[start + 1 + name.len()..end];
                let new_text = format!("@{}#{}{}", name, user.id, suffix);
                resolved.push((start, end, new_text));
            }
            // Apply right-to-left so replacement lengths don't shift pending offsets.
            let mut result = intro.into_bytes();
            for (start, end, new_text) in resolved.into_iter().rev() {
                result.splice(start..end, new_text.into_bytes());
            }
            Ok(String::from_utf8(result).unwrap())
        }));
    }

    /// 用当前 `self.info` 刷新本地谱面信息（实例方法便捷包装）。
    fn update_chart_info(&self) -> Result<()> {
        Self::global_update_chart_info(self.local_path.as_ref().unwrap(), self.info.clone())
    }

    /// 全局刷新本地谱面信息：写回数据并置位 [`NEED_UPDATE`] 让列表页重排。
    /// 缩略图可能随信息变化，故先删除旧缩略图缓存（后续按需重建），最后立即落盘。
    fn global_update_chart_info(local_path: &str, info: BriefChartInfo) -> Result<()> {
        let _ = std::fs::remove_file(thumbnail_path(local_path)?);
        get_data_mut().charts[get_data().find_chart_by_path(local_path).unwrap()].info = info;
        NEED_UPDATE.store(true, Ordering::Relaxed);
        save_data()?;
        Ok(())
    }

    /// 应用一份热重载资源：切换本地路径、替换预览音乐与插画，并同步谱面信息。
    /// 替换前先暂停旧预览，避免两个 `Music` 实例同时播放。
    fn load_tuple(&mut self, (local_path, info, preview, illu): LocalTuple) -> Result<()> {
        self.local_path = Some(local_path);
        if let Some(preview) = &mut self.preview {
            preview.pause()?;
        }
        self.preview = Some(create_music(preview)?);
        self.info = info.into();
        self.illu = illu;
        self.update_chart_info()?;

        Ok(())
    }

    /// 构造用于收藏/合集比较的最小谱面引用（仅远端 id 与本地路径）。
    fn to_bare_chart_ref(&self) -> ChartRef {
        ChartRef::new_bare(self.info.id, self.local_path.as_deref())
    }

    /// 把本谱面在指定合集中加入/移除。
    ///
    /// 远端谱面会附带谱面信息快照以便离线展示；若该合集需要服务端同步，则把同步任务挂到
    /// `toggle_fav_task`，成功后由 [`SongScene::update`] 合并服务端返回结果；否则直接置空
    /// 收藏缓存并广播 [`FAV_UPDATED`]。
    fn toggle_in(&mut self, uuid: Uuid) {
        let data = get_data();
        let col = data.collection_info(&uuid).as_ref().clone();
        let mut chart_ref = self.to_bare_chart_ref();
        if self.info.id.is_some() {
            let Some(entity) = self.entity.clone() else {
                show_message(tl!("still-loading")).error();
                return;
            };
            chart_ref.info = Some(Box::new(ChartRefChartInfo::from_chart(&entity)));
        }
        let add = col.charts.iter().all(|it| it != &chart_ref);
        match col.update(uuid, &[chart_ref], add) {
            CollectionUpdate::Unchanged => {}
            CollectionUpdate::Updated { sync_task, add } => {
                if let Some(task) = sync_task {
                    self.toggle_fav_task = Some(task);
                } else {
                    self.is_fav = None;
                    FAV_UPDATED.store(true, Ordering::SeqCst);
                    if add {
                        show_message(tl!("fav-added")).duration(1.5).ok();
                    }
                }
            }
        }
    }

    /// 构建收藏夹菜单的选项列表。
    /// 已或包含该谱面的条目会以“\u2713 前缀标记。
    fn get_fav_menu_options(&mut self) -> Vec<String> {
        let data = get_data();
        let mut options = Vec::new();
        self.fav_menu_options.clear();
        let chart_ref = self.to_bare_chart_ref();
        for uuid in data.collection_uuids() {
            let col = data.collection_info(uuid);
            if !col.is_owned() {
                continue;
            }
            self.fav_menu_options.push(*uuid);
            let contains = col.charts.iter().any(|it| it == &chart_ref);
            options.push(format!("{} {}", if contains { '\u{2713}' } else { ' ' }, col.name));
        }
        options
    }
}

// `SongScene` 对 [`Scene`] 的实现。各钩子的职责：
// - `on_result`：消费内核/子场景回传的结果——成绩（[`SimpleRecord`]）则更新本地最佳、按概率
//   弹评分框并刷新排行榜；错误则提示；`Option<f32>` 是 offset 调整结果，按谱面来源写回并失效缩略图；
// - `pause` / `resume`：暂停 / 恢复预览音乐；
// - `enter`：首帧对齐淡入动画并加载排行榜，随后重置并播放预览、刷新菜单项；
// - `touch`：命中优先级见其文档注释（任务阻塞 → 下载取消 → 浮层 → 抽屉 → 主界面按钮）；
// - `update`：集中轮询所有异步任务并推进状态机（分段见函数内注释），最后统一消费各类原子信号；
// - `render`：按区块布局绘制（背景标题 → 底部成绩/名次 → 播放按钮 → 右上角图标组 → 下载对话框
//   → 右侧抽屉 → 弹出菜单 → 全屏加载态 → 对话框 → 返回过渡动画）；
// - `next_scene`：过渡动画未结束时留在本页；否则取待切场景并清理背景与预览。
impl Scene for SongScene {
    /// 处理子场景/内核回传的结果（分派规则见 trait 实现上方的钩子说明）。
    fn on_result(&mut self, tm: &mut TimeManager, res: Box<dyn Any>) -> Result<()> {
        // 类型分派 1：成绩（内核结算回传），更新本页最佳并刷新排行榜。
        let res = match res.downcast::<SimpleRecord>() {
            Err(res) => res,
            Ok(rec) => {
                self.fade_start = tm.now() as f32 + fade_in_time().unwrap_or_default();
                if self.my_rate_score == Some(0) && thread_rng().gen_ratio(2, 5) {
                    self.rate_dialog.enter(tm.real_time() as _);
                }
                if let Some(record) = &mut self.record {
                    record.update(&rec);
                } else {
                    self.record = Some(*rec);
                }
                self.load_ldb();
                return Ok(());
            }
        };
        // 类型分派 2：对局/加载错误。
        let res = match res.downcast::<anyhow::Error>() {
            Ok(error) => {
                show_error(error.context(tl!("load-chart-failed")));
                return Ok(());
            }
            Err(res) => res,
        };
        // 类型分派 3：offset 调整结果，仅 `Some` 时写盘（用户可能直接取消）。
        let _res = match res.downcast::<Option<f32>>() {
            Ok(offset) => {
                if let Some(offset) = *offset {
                    let dir = format!("{}/{}", dir::charts()?, self.local_path.as_ref().unwrap().replace(':', "_"));
                    let path = std::path::Path::new(&dir);
                    if !path.exists() {
                        std::fs::create_dir_all(path)?;
                    }
                    let dir = prpr::dir::Dir::new(dir)?;
                    // 内置谱面（合入资源包）写入独立 offset 文件并同步内存中的资源包信息；
                    // 其余谱面写回 info.yml，并删除缩略图缓存以便重建。
                    match self.chart_type {
                        ChartType::Integrated => {
                            dir.create("offset")?.write_all(&offset.to_be_bytes())?;
                            if let Ok(Some(info)) = ASSET_CHART_INFO.lock().as_deref_mut() {
                                info.offset = offset;
                            }
                        }
                        _ => {
                            let mut info: ChartInfo = serde_yaml::from_reader(&dir.open("info.yml")?)?;
                            info.offset = offset;
                            dir.create("info.yml")?.write_all(serde_yaml::to_string(&info)?.as_bytes())?;
                            let path = thumbnail_path(self.local_path.as_ref().unwrap())?;
                            if path.exists() {
                                std::fs::remove_file(path)?;
                            }
                        }
                    }
                    show_message(tl!("edit-saved")).ok();
                }
                return Ok(());
            }
            Err(res) => res,
        };
        Ok(())
    }

    /// 暂停预览音乐（页面被压栈或切到后台时调用）。
    fn pause(&mut self, _tm: &mut TimeManager) -> Result<()> {
        if let Some(preview) = &mut self.preview {
            preview.pause()?;
        }
        Ok(())
    }

    /// 恢复预览音乐播放。
    fn resume(&mut self, _tm: &mut TimeManager) -> Result<()> {
        if let Some(preview) = &mut self.preview {
            preview.play()?;
        }
        Ok(())
    }

    /// 进入本页：首次进入时把时间轴回拨以播放淡入动画并发起排行榜加载；
    /// 随后把预览音乐重置到开头重新播放，并刷新“更多操作”菜单项。
    fn enter(&mut self, tm: &mut TimeManager, _target: Option<RenderTarget>) -> Result<()> {
        if self.first_in {
            self.first_in = false;
            tm.seek_to(-fade_in_time().unwrap_or_default() as _);
            self.load_ldb();
        }
        if let Some(music) = &mut self.preview {
            music.seek_to(0.)?;
            music.play()?;
        }
        self.update_menu();
        Ok(())
    }

    /// 触摸分派。返回 `true` 表示事件已被消费。
    ///
    /// 命中优先级：进行中的任务（直接吞掉，防重入）→ 下载取消 → 标签/评分对话框与弹出菜单
    /// → 右侧抽屉（点抽屉外关闭；关闭 mods 抽屉时补存 mods）→ 主界面各按钮。
    fn touch(&mut self, tm: &mut TimeManager, touch: &Touch) -> Result<bool> {
        let t = tm.now() as f32;
        // 阶段一：只要任一异步任务在跑，就吞掉全部触摸，避免状态竞争与重复触发。
        if self.scene_task.is_some()
            || self.save_task.is_some()
            || self.upload_task.is_some()
            || self.review_task.is_some()
            || self.edit_tags_task.is_some()
            || self.rate_task.is_some()
            || self.overwrite_task.is_some()
            || self.update_cksum_task.is_some()
            || self.toggle_fav_task.is_some()
            || self.export_task.is_some()
            || self.autocomplete_task.is_some()
        {
            return Ok(true);
        }
        // 阶段二：下载对话框优先——仅取消按钮可交互，其余触摸一律吞掉。
        if let Some(dl) = &mut self.downloading {
            if dl.touch(touch, t) {
                let atomicity = dl.atomicity.clone();
                let _guard = atomicity.lock().unwrap();
                self.downloading = None;
                return Ok(true);
            }
            return Ok(false);
        }
        let rt = tm.real_time() as f32;
        // 阶段三：浮层（标签 / 评分对话框、下拉菜单）优先消费事件。
        if self.tags.touch(touch, rt) {
            return Ok(true);
        }
        if self.rate_dialog.touch(touch, rt) {
            return Ok(true);
        }
        if self.menu.showing() {
            self.menu.touch(touch, t);
            return Ok(true);
        }
        if self.fav_menu.showing() {
            self.fav_menu.touch(touch, t);
            return Ok(true);
        }
        // 阶段四：抽屉交互——点击抽屉外区域关闭；关闭 mods 抽屉时把改动落盘。
        if self.side_enter_time.is_finite() {
            if self.side_enter_time > 0. && tm.real_time() as f32 > self.side_enter_time + edit_transit().unwrap_or_default() {
                if touch.position.x < 1. - self.side_content.width() && touch.phase == TouchPhase::Started && self.save_task.is_none() {
                    if matches!(self.side_content, SideContent::Mods) {
                        if let Some(index) = get_data().find_chart_by_path(self.local_path.as_deref().unwrap()) {
                            let chart = &mut get_data_mut().charts[index];
                            if chart.mods != self.mods {
                                chart.mods = self.mods;
                                save_data()?;
                            }
                        }
                    }
                    if matches!(self.side_content, SideContent::Edit) && self.info_edit.as_ref().is_some_and(|it| it.updated) {
                        confirm_dialog(tl!("warn"), tl!("cancel-not-saved"), self.confirm_cancel_edit.clone());
                    } else {
                        self.hide_side(rt);
                    }
                    return Ok(true);
                }
                match self.side_content {
                    SideContent::Edit => {
                        if self.edit_scroll.touch(touch, t) {
                            return Ok(true);
                        }
                    }
                    SideContent::Leaderboard => {
                        if self.ldb_type_btn.touch(touch, rt) {
                            self.ldb_std ^= true;
                            self.ldb_scroll.y_scroller.offset = 0.;
                            self.load_ldb();
                            return Ok(true);
                        }
                        if self.ldb_scroll.touch(touch, t) {
                            return Ok(true);
                        }
                        if let Some((_, ldb)) = &mut self.ldb {
                            for item in ldb {
                                if item.btn.touch(touch) {
                                    button_hit();
                                    self.sf
                                        .goto(t, ProfileScene::new(item.inner.player.id, self.icons.user.clone(), self.rank_icons.clone()));
                                    return Ok(true);
                                }
                            }
                        }
                    }
                    SideContent::Info => {
                        if self.info_scroll.touch(touch, t) {
                            return Ok(true);
                        }
                        if self.uploader_btn.touch(touch) {
                            button_hit();
                            self.sf.goto(
                                t,
                                ProfileScene::new(self.info.uploader.as_ref().unwrap().id, self.icons.user.clone(), self.rank_icons.clone()),
                            );
                            return Ok(true);
                        }
                        for (id, (_, btn)) in &mut self.collaborators {
                            if btn.touch(touch) {
                                button_hit();
                                self.sf.goto(t, ProfileScene::new(*id, self.icons.user.clone(), self.rank_icons.clone()));
                                return Ok(true);
                            }
                        }
                        if self.open_web_btn.touch(touch, rt) {
                            open_url(&format!("https://phira.moe/chart/{}", self.info.id.unwrap()))?;
                            return Ok(true);
                        }
                    }
                    SideContent::Mods => {
                        if self.mod_scroll.touch(touch, t) {
                            return Ok(true);
                        }
                        let rt = tm.real_time() as _;
                        for (btn, clicked) in &mut self.mod_btns {
                            if btn.touch(touch, rt) {
                                *clicked = true;
                                return Ok(true);
                            }
                        }
                    }
                }
            }
            return Ok(false);
        }
        // 阶段五：主界面按钮——返回 / 播放（或下载）/ 菜单 / 收藏（含长按）/ 编辑 / 模组 / 榜单 / 信息。
        if self.back_btn.touch(touch) {
            button_hit();
            self.next_scene = Some(NextScene::PopWithResult(Box::new(false)));
            return Ok(true);
        }
        if self.scene_task.is_none() && self.next_scene.is_none() && self.play_btn.touch(touch, t) {
            if self.local_path.is_some() {
                self.launch(GameMode::Normal, false)?;
            } else {
                self.start_download()?;
            }
            return Ok(true);
        }
        if !self.menu_options.is_empty() && self.menu_btn.touch(touch) {
            button_hit();
            self.need_show_menu = true;
            return Ok(true);
        }
        if self.fav_btn.touch(touch) {
            self.fav_long_touch.reset();
            button_hit();
            let data = get_data();
            if let Some(uuid) = data.collection_uuids().iter().find(|uuid| data.collection_info(uuid).is_default) {
                self.toggle_in(*uuid);
            }
            return Ok(true);
        }
        if self.fav_btn.long_touch(touch, t, &mut self.fav_long_touch) {
            button_hit();
            let options = self.get_fav_menu_options();
            self.fav_menu.set_options(options);
            self.need_show_fav_menu = true;
            return Ok(true);
        }
        if let Some(path) = &self.local_path {
            if self.edit_btn.touch(touch) {
                button_hit();
                let mut info: ChartInfo = serde_yaml::from_str(&std::fs::read_to_string(format!("{}/{path}/info.yml", dir::charts()?))?)?;
                info.id = self.info.id;
                UPLOAD_NOT_SAVED.store(false, Ordering::SeqCst);
                self.info_edit = Some(ChartInfoEdit::new(info));
                self.side_content = SideContent::Edit;
                self.side_enter_time = tm.real_time() as _;
                return Ok(true);
            }
            if self.mod_btn.touch(touch) {
                button_hit();
                self.side_content = SideContent::Mods;
                self.side_enter_time = tm.real_time() as _;
                return Ok(true);
            }
        }
        if self.info.id.is_some() && self.ldb_btn.touch(touch) {
            button_hit();
            self.side_content = SideContent::Leaderboard;
            self.side_enter_time = tm.real_time() as _;
        }
        if self.info_btn.touch(touch) {
            button_hit();
            if let Some(uploader) = &self.info.uploader {
                UserManager::request(uploader.id);
            }
            self.collaborators = parse_collaborators(&self.info.intro);
            for id in self.collaborators.keys() {
                UserManager::request(*id);
            }
            self.side_content = SideContent::Info;
            self.side_enter_time = tm.real_time() as _;
            return Ok(true);
        }

        Ok(false)
    }

    /// 更新一帧：轮询所有异步任务、推进抽屉与对话框状态机，并消费各类全局原子信号。
    ///
    /// 本页任务极多，集中轮询是有意为之——所有任务完成后统一在此回填 UI 状态，
    /// 避免回调散落各处导致状态不一致。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()> {
        let t = tm.now() as f32;
        self.menu.update(t);
        self.fav_menu.update(t);
        if self.fav_btn.update_long_touch(t, &mut self.fav_long_touch) {
            button_hit();
            let options = self.get_fav_menu_options();
            self.fav_menu.set_options(options);
            self.need_show_fav_menu = true;
        }
        // 基础动画 / 浮层更新：弹出菜单、长按检测、插画渐显、标签与评分对话框。
        self.illu.settle(t);
        let rt = tm.real_time() as f32;
        self.tags.update(rt);
        self.rate_dialog.update(rt);
        // 标签确认回调：编辑态写本地表单（标记为未保存），审核态直接提交服务端。
        if self.tags.confirmed.take() == Some(true) {
            let mut tags = self.tags.tags.tags().to_vec();
            tags.push(self.tags.division.to_owned());
            if self.side_enter_time.is_finite() && matches!(self.side_content, SideContent::Edit) {
                let edit = self.info_edit.as_mut().unwrap();
                edit.info.tags = tags;
                edit.updated = true;
            } else {
                let id = self.info.id.unwrap();
                self.entity.as_mut().unwrap().tags = tags.clone();
                self.edit_tags_task = Some(Task::new(async move {
                    recv_raw(Client::post(
                        format!("/chart/{id}/edit-tags"),
                        &json!({
                            "tags": tags,
                        }),
                    ))
                    .await?;
                    Ok(())
                }));
            }
        }
        // 评分确认回调：提交 /chart/{id}/rate。
        if self.rate_dialog.confirmed.take() == Some(true) {
            if let Some(id) = self.info.id {
                let score = self.rate_dialog.rate.score;
                self.rate_task = Some(Task::new(async move {
                    recv_raw(Client::post(
                        format!("/chart/{id}/rate"),
                        &json!({
                            "score": score,
                        }),
                    ))
                    .await?;
                    Ok(())
                }));
            }
        }
        if self.side_enter_time < 0. && -tm.real_time() as f32 + edit_transit().unwrap_or_default() < self.side_enter_time {
            self.side_enter_time = f32::INFINITY;
        }
        // 自动补全对话框信号：确认则先补全再保存，跳过则直接保存。
        if CONFIRM_AUTOCOMPLETE.fetch_and(false, Ordering::SeqCst) {
            self.start_autocomplete();
        }
        if SKIP_AUTOCOMPLETE.fetch_and(false, Ordering::SeqCst) {
            self.save_edit();
        }
        if let Some(task) = &mut self.autocomplete_task {
            if let Some(res) = task.take() {
                self.autocomplete_task = None;
                match res {
                    Err(err) => {
                        show_error(err);
                    }
                    Ok(new_intro) => {
                        if let Some(edit) = self.info_edit.as_mut() {
                            edit.info.intro = new_intro;
                            edit.updated = true;
                        }
                        show_message(tl!("collab-autocomplete-done")).duration(1.).ok();
                        self.save_edit();
                    }
                }
            }
        }
        // 远端谱面实体加载结果：拉到则比对是否需要更新，404 则剥离本地残留的线上字段。
        if let Some(task) = &mut self.load_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("load-charts-failed")));
                    }
                    Ok(chart) => {
                        if let Some(chart) = chart {
                            self.entity = Some(chart.as_ref().clone());
                            if self
                                .info
                                .updated
                                .map_or(chart.updated != chart.created, |local_updated| local_updated != chart.updated)
                                && self.local_path.is_some()
                            {
                                let chart_updated = self
                                    .info
                                    .chart_updated
                                    .map_or(chart.chart_updated != chart.created, |local_updated| local_updated != chart.chart_updated);
                                confirm_dialog(
                                    tl!("need-update"),
                                    if chart_updated {
                                        tl!("need-update-content")
                                    } else {
                                        tl!("need-update-info-only-content")
                                    },
                                    Arc::clone(&self.should_update),
                                );
                            }
                        } else if let Some(local) = &self.local_path {
                            let conf = format!("{}/{}/info.yml", dir::charts()?, local);
                            let mut info: ChartInfo = serde_yaml::from_reader(File::open(&conf)?)?;
                            info.id = None;
                            info.uploader = None;
                            info.created = None;
                            info.updated = None;
                            info.chart_updated = None;
                            serde_yaml::to_writer(File::create(conf)?, &info)?;
                            self.info = info.into();
                            self.update_chart_info()?;
                        }
                        self.update_menu();
                    }
                }
                self.load_task = None;
            }
        }
        // 预览音频解码结果。
        if let Some(task) = &mut self.preview_task {
            if let Some(result) = task.take() {
                match result {
                    Err(err) => {
                        show_error(err.context(tl!("load-preview-failed")));
                    }
                    Ok(clip) => {
                        self.preview = Some(create_music(clip)?);
                    }
                }
                self.preview_task = None;
            }
        }
        // 下载任务轮询（返回语义见 `Downloading::check`）。
        if let Some(dl) = &mut self.downloading {
            if let Some(tuple) = dl.check()? {
                self.local_path = dl.local_path.take();
                self.downloading = None;
                if let Some(tuple) = tuple {
                    self.load_tuple(tuple)?;
                }
                self.update_menu();
            }
        }
        // 进入游戏任务：就绪即切换场景；出错则提示并支持一键切换到离线模式。
        if let Some(task) = &mut self.scene_task {
            if let Some(res) = poll_future(task.as_mut()) {
                match res {
                    Err(err) => {
                        error!(?err, "failed to play");
                        *self.background.lock().unwrap() = None;
                        self.tr_start = f32::NAN;
                        let error = format!("{err:?}");
                        Dialog::plain(tl!("failed-to-play"), error)
                            .buttons(vec![tl!("play-cancel").into_owned(), tl!("play-switch-to-offline").into_owned()])
                            .listener(move |_dialog, pos| {
                                if pos == 1 {
                                    get_data_mut().config.offline_mode = true;
                                    let _ = save_data();
                                    show_message(tl!("switched-to-offline")).ok();
                                }
                                false
                            })
                            .show();
                    }
                    Ok(scene) => self.next_scene = Some(scene),
                }
                self.scene_task = None;
            }
        }
        // 历史最佳成绩拉取结果。
        if let Some(task) = &mut self.fetch_best_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!(?err, "failed to fetch best record");
                    }
                    Ok(rec) => {
                        self.update_record(rec)?;
                    }
                }
                self.fetch_best_task = None;
            }
        }
        // 菜单选择分派：顺序与 `update_menu` 生成的菜单项一一对应。
        if self.menu.changed() {
            let option = self.menu_options[self.menu.selected()];
            match option {
                "delete" => {
                    confirm_delete(self.should_delete.clone());
                }
                "rate" => {
                    self.rate_dialog.enter(tm.real_time() as _);
                }
                "exercise" => {
                    // 练习模式：以 Exercise 模式进入内核（练习区间由内核/默认设定决定），不播放解锁动画。
                    self.launch(GameMode::Exercise, false)?;
                }
                "offset" => {
                    // offset 调整：以 TweakOffset 模式进入，结束时内核回传新 offset，由 `on_result` 写盘。
                    self.launch(GameMode::TweakOffset, false)?;
                }
                "unlock" => {
                    // 手动解锁入口：强制播放解锁动画（force_unlock = true）。
                    self.launch(GameMode::Normal, true)?;
                }
                "review-approve" => {
                    confirm_dialog(tl!("warn"), tl!("review-approve-confirm"), Arc::clone(&self.should_review_approve));
                }
                "review-deny" => {
                    request_input("deny-reason", InputBox::new().mode(InputMode::Multiline));
                }
                "review-del" => {
                    confirm_delete(self.chart_should_delete.clone());
                }
                "review-edit-tags" => {
                    let Some(entity) = self.entity.as_ref() else {
                        show_message(tl!("review-not-loaded")).warn();
                        return Ok(());
                    };
                    self.tags.set(entity.tags.clone());
                    self.tags.enter(tm.real_time() as _);
                }
                "stabilize" => {
                    confirm_dialog(tl!("stabilize"), tl!("stabilize-warn"), Arc::clone(&self.should_stabilize));
                }
                "stabilize-approve" => {
                    confirm_dialog(tl!("warn"), tl!("stabilize-approve-confirm"), Arc::clone(&self.should_stabilize_approve));
                }
                "stabilize-approve-ranked" => {
                    confirm_dialog(tl!("warn"), tl!("stabilize-approve-confirm"), Arc::clone(&self.should_stabilize_approve_ranked));
                }
                "stabilize-comment" => {
                    request_input("stabilize-comment", InputBox::new().mode(InputMode::Multiline));
                }
                "stabilize-deny" => {
                    request_input("stabilize-deny-reason", InputBox::new().mode(InputMode::Multiline));
                }
                "export" => {
                    request_export(format!("{}.zip", sanitize(&self.info.name)));
                }
                _ => {}
            }
        }
        // 导出就绪：在独立线程中把谱面目录打包为 zip（避免阻塞渲染），结果经通道回传。
        if let Some(config) = take_export() {
            fn export_inner(path: String, output: File) -> Result<()> {
                let charts = dir::charts()?;
                compress_folder(Path::new(&format!("{charts}/{path}")), &mut BufWriter::new(output))?;
                Ok(())
            }

            match config {
                Err(err) => show_error(err.into()),
                Ok(config) => {
                    let path = self.local_path.clone().unwrap();
                    let (tx, rx) = mpsc::sync_channel(1);
                    std::thread::spawn(move || {
                        let result = export_inner(path, config.file);
                        if result.is_err() {
                            if let Err(err) = (config.deleter)() {
                                warn!("failed to delete export file: {:?}", err);
                            }
                        }
                        let _ = tx.send(result);
                    });
                    self.export_task = Some(rx);
                }
            }
        }
        // 导出完成轮询：失败则清理半成品文件，成功则“另存为”收尾。
        if let Some(rx) = &mut self.export_task {
            match rx.try_recv() {
                Ok(Err(err)) => {
                    show_error(err);
                    self.export_task = None;
                }
                Ok(Ok(())) => {
                    resolve_export();
                    self.export_task = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    show_error(Error::msg("Export thread panicked"));
                    self.export_task = None;
                }
            }
        }
        // 各类原子信号消费：本地删除、审核删除、审核通过、稳定化审核与申请。
        if self.should_delete.fetch_and(false, Ordering::Relaxed) {
            self.next_scene = Some(NextScene::PopWithResult(Box::new(true)));
        }
        if self.fav_menu.changed() {
            let selected = self.fav_menu.selected();
            self.fav_menu.set_selected(usize::MAX);
            self.toggle_in(self.fav_menu_options[selected]);
            let _ = save_data();
            let options = self.get_fav_menu_options();
            self.fav_menu.set_options(options);
        }
        if self.chart_should_delete.fetch_and(false, Ordering::Relaxed) {
            let id = self.info.id.unwrap();
            self.review_task = Some(Task::new(async move {
                recv_raw(Client::delete(format!("/chart/{id}"))).await?;
                Ok(tl!("review-deleted").into_owned())
            }));
        }
        if self.should_review_approve.fetch_and(false, Ordering::Relaxed) {
            let id = self.info.id.unwrap();
            self.review_task = Some(Task::new(async move {
                #[derive(Deserialize)]
                struct Resp {
                    passed: bool,
                }
                let resp: Resp = recv_raw(Client::post(
                    format!("/chart/{id}/review"),
                    &json!({
                        "approve": true
                    }),
                ))
                .await?
                .json()
                .await?;
                Ok((if resp.passed { tl!("review-passed") } else { tl!("review-approved") }).into_owned())
            }));
        }
        if self.should_stabilize_approve.fetch_and(false, Ordering::Relaxed) {
            let id = self.info.id.unwrap();
            self.review_task = Some(Task::new(async move {
                let resp: StableR = recv_raw(Client::post(
                    format!("/chart/{id}/stabilize"),
                    &json!({
                        "kind": 0,
                    }),
                ))
                .await?
                .json()
                .await?;
                Ok((if resp.status == 0 {
                    tl!("stabilize-approved")
                } else {
                    tl!("stabilize-approved-passed")
                })
                .into())
            }));
        }
        if self.should_stabilize_approve_ranked.fetch_and(false, Ordering::Relaxed) {
            let id = self.info.id.unwrap();
            self.review_task = Some(Task::new(async move {
                let resp: StableR = recv_raw(Client::post(
                    format!("/chart/{id}/stabilize"),
                    &json!({
                        "kind": 1,
                    }),
                ))
                .await?
                .json()
                .await?;
                Ok((if resp.status == 0 {
                    tl!("stabilize-approved")
                } else {
                    tl!("stabilize-approved-passed")
                })
                .into())
            }));
        }
        if self.should_stabilize.fetch_and(false, Ordering::Relaxed) {
            let id = self.info.id.unwrap();
            self.stabilize_task = Some(Task::new(async move {
                recv_raw(Client::post(format!("/chart/{id}/req-stabilize"), &())).await?;
                Ok(())
            }));
        }
        // 保存编辑结果：清除“未保存”标记、热重载资源并提示成功。
        if let Some(task) = &mut self.save_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("edit-save-failed")));
                    }
                    Ok(tuple) => {
                        self.info_edit.as_mut().unwrap().updated = false;
                        self.load_tuple(tuple)?;
                        show_message(tl!("edit-saved")).duration(1.).ok();
                    }
                }
                self.save_task = None;
            }
        }
        // 上传结果：回填新的谱面信息并关闭编辑抽屉。
        if let Some(task) = &mut self.upload_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("upload-failed")));
                    }
                    Ok(info) => {
                        show_message(tl!("upload-success")).ok();
                        self.info = info;
                        self.update_chart_info()?;
                        self.side_enter_time = -tm.real_time() as _;
                    }
                }
                self.upload_task = None;
            }
        }
        // 抽屉滚动状态推进（排行榜下拉到顶时顺带刷新数据）。
        match self.side_content {
            SideContent::Edit => {
                self.edit_scroll.update(t);
            }
            SideContent::Leaderboard => {
                if self.ldb_scroll.y_scroller.pulled {
                    self.ldb_scroll.y_scroller.offset = 0.;
                    self.load_ldb();
                }
                self.ldb_scroll.update(t);
            }
            SideContent::Info => {
                self.info_scroll.update(t);
            }
            SideContent::Mods => {
                self.mod_scroll.update(t);
            }
        }
        // 上传流程起点：先核验谱面文件校验和——内容变化会使历史榜失效，需用户确认清空。
        if CONFIRM_UPLOAD.fetch_and(false, Ordering::Relaxed) {
            let local_path = self.local_path.clone().unwrap();
            let id = self.info.id;
            self.update_cksum_task = Some(Task::new(async move {
                if let Some(id) = id {
                    use hex::ToHex;
                    let mut fs = fs_from_path(&local_path)?;
                    let info = prpr::fs::load_info(fs.as_mut()).await?;
                    let chart = fs.load_file(&info.chart).await?;
                    let cksum: String = Sha256::digest(&chart).encode_hex();
                    #[derive(Deserialize)]
                    struct VerifyR {
                        ok: bool,
                    }
                    let resp: VerifyR = recv_raw(Client::get(format!("/chart/{id}/verify-cksum?checksum={cksum}")))
                        .await?
                        .json()
                        .await?;
                    Ok(resp.ok)
                } else {
                    Ok(true)
                }
            }));
        }
        if let Some(task) = &mut self.update_cksum_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("upload-failed")));
                    }
                    Ok(ok) => {
                        if ok {
                            CONFIRM_CKSUM.store(true, Ordering::Relaxed);
                        } else {
                            Dialog::simple(tl!("upload-confirm-clear-ldb"))
                                .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
                                .listener(move |_dialog, pos| {
                                    if pos == 1 {
                                        CONFIRM_CKSUM.store(true, Ordering::Relaxed);
                                    }
                                    false
                                })
                                .show();
                        }
                    }
                }
                self.update_cksum_task = None;
            }
        }
        // 校验和通过：打包上传，创建新谱面或更新已有谱面并回填元信息。
        if CONFIRM_CKSUM.fetch_and(false, Ordering::Relaxed) {
            let path = self.local_path.clone().unwrap();
            let info = self.info.clone();
            self.upload_task = Some(Task::new(async move {
                let root = format!("{}/{path}", dir::charts()?);
                let root = Path::new(&root);
                let mut chart_bytes = Vec::new();
                compress_folder(root, &mut Cursor::new(&mut chart_bytes))?;
                let file = Client::upload_file("chart.zip", chart_bytes)
                    .await
                    .with_context(|| tl!("upload-chart-failed"))?;
                if let Some(id) = info.id {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct Resp {
                        updated: DateTime<Utc>,
                        chart_updated: DateTime<Utc>,
                    }
                    let resp: Resp = recv_raw(Client::request(Method::PATCH, format!("/chart/{id}")).json(&json!({
                        "file": file,
                        "created": info.created.unwrap(),
                    })))
                    .await?
                    .json()
                    .await?;
                    let conf = root.join("info.yml");
                    let mut info: ChartInfo = serde_yaml::from_reader(File::open(&conf)?)?;
                    info.updated = Some(resp.updated);
                    info.chart_updated = Some(resp.chart_updated);
                    serde_yaml::to_writer(File::create(conf)?, &info)?;
                    Ok(info.into())
                } else {
                    #[derive(Deserialize)]
                    struct Resp {
                        id: i32,
                        created: DateTime<Utc>,
                    }
                    let resp: Resp = recv_raw(Client::post(
                        "/chart/upload",
                        &json!({
                            "file": file,
                        }),
                    ))
                    .await?
                    .json()
                    .await?;
                    let conf = root.join("info.yml");
                    let mut info: ChartInfo = serde_yaml::from_reader(File::open(&conf)?)?;
                    info.id = Some(resp.id);
                    info.created = Some(resp.created);
                    info.updated = Some(resp.created);
                    info.chart_updated = Some(resp.created);
                    info.uploader = Some(get_data().me.as_ref().unwrap().id);
                    serde_yaml::to_writer(File::create(conf)?, &info)?;
                    Ok(info.into())
                }
            }));
        }
        // 排行榜数据回填：记录本人名次、请求上榜玩家信息并触发渐显动画。
        if let Some(task) = &mut self.ldb_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("ldb-load-failed")));
                    }
                    Ok(items) => {
                        let rank = get_data()
                            .me
                            .as_ref()
                            .and_then(|me| items.iter().find(|it| it.inner.player.id == me.id).map(|it| it.rank));
                        for item in &items {
                            UserManager::request(item.inner.player.id);
                        }
                        self.ldb = Some((rank, items));
                        self.ldb_fader.sub(tm.real_time() as _);
                    }
                }
                self.ldb_task = None;
            }
        }
        // 文本输入回传（审核拒绝理由 / 稳定化相关留言）；未识别的 id 原样退回给其他场景。
        if let Some((id, text)) = take_input() {
            match id.as_str() {
                "deny-reason" => {
                    let id = self.info.id.unwrap();
                    self.review_task = Some(Task::new(async move {
                        recv_raw(Client::post(
                            format!("/chart/{id}/review"),
                            &json!({
                                "approve": false,
                                "reason": text,
                            }),
                        ))
                        .await?;
                        Ok(tl!("review-denied").into_owned())
                    }));
                }
                "stabilize-comment" => {
                    let id = self.info.id.unwrap();
                    self.review_task = Some(Task::new(async move {
                        recv_raw(Client::post(
                            format!("/chart/{id}/stabilize-comment"),
                            &json!({
                                "comment": text,
                            }),
                        ))
                        .await?;
                        Ok(tl!("stabilize-commented").into())
                    }));
                }
                "stabilize-deny-reason" => {
                    let id = self.info.id.unwrap();
                    self.review_task = Some(Task::new(async move {
                        let resp: StableR = recv_raw(Client::post(
                            format!("/chart/{id}/stabilize"),
                            &json!({
                                "kind": -1,
                                "reason": text,
                            }),
                        ))
                        .await?
                        .json()
                        .await?;
                        Ok((if resp.status == 0 {
                            tl!("stabilize-denied")
                        } else {
                            tl!("stabilize-denied-passed")
                        })
                        .into())
                    }));
                }
                _ => return_input(id, text),
            }
        }
        // 文件选择回传：`overwrite` 走覆盖确认，其余 id 原样退回。
        if let Some((id, file)) = take_file() {
            if id == "overwrite" {
                self.overwrite_from = Some(file);
                CONFIRM_OVERWRITE.store(false, Ordering::SeqCst);
                Dialog::simple(tl!("edit-overwrite-confirm"))
                    .buttons(vec![ttl!("cancel").into_owned(), ttl!("confirm").into_owned()])
                    .listener(move |_dialog, pos| {
                        if pos == 1 {
                            CONFIRM_OVERWRITE.store(true, Ordering::SeqCst);
                        }
                        false
                    })
                    .show();
            } else {
                return_file(id, file);
            }
        }
        // 覆盖确认：把用户所选文件解压到临时目录，回填 id/上传者后替换本地谱面目录。
        if CONFIRM_OVERWRITE.fetch_and(false, Ordering::Relaxed) {
            let path = self.overwrite_from.take().unwrap();
            let local_path = self.local_path.clone().unwrap();
            let def_illu = self.illu.texture.1.clone();
            let chart_id = self.info.id.unwrap();
            let owner = self.info.uploader.as_ref().unwrap().id;
            self.overwrite_task = Some(Task::new(async move {
                let (dir, id) = gen_custom_dir()?;
                let to_path = format!("{}/{}/", dir::charts()?, local_path);
                let file = File::open(path).context("cannot open file")?;
                if let Err(err) = import_chart_to(&dir, format!("custom/{id}"), file).await {
                    std::fs::remove_dir_all(dir)?;
                    return Err(err);
                }
                let mut fs = prpr::fs::fs_from_file(&dir)?;
                let mut info = prpr::fs::load_info(fs.as_mut()).await?;
                drop(fs);
                info.id = Some(chart_id);
                info.uploader = Some(owner);
                serde_yaml::to_writer(File::create(dir.join("info.yml"))?, &info)?;

                std::fs::remove_dir_all(&to_path)?;
                std::fs::rename(&dir, &to_path)?;

                load_local_tuple(&local_path, def_illu, info).await
            }));
        }
        // 覆盖任务结果：成功则热重载本地资源。
        if let Some(task) = &mut self.overwrite_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("edit-overwrite-failed")));
                    }
                    Ok(tuple) => {
                        self.load_tuple(tuple)?;
                        show_message(tl!("edit-overwrite-success")).ok();
                    }
                }
                self.overwrite_task = None;
            }
        }
        // 审核类任务结果：成功时展示服务端返回的文案。
        if let Some(task) = &mut self.review_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("review-action-failed")));
                    }
                    Ok(msg) => {
                        show_message(msg).ok();
                    }
                }
                self.review_task = None;
            }
        }
        // 稳定化申请结果。
        if let Some(task) = &mut self.stabilize_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("stabilize-failed")));
                    }
                    Ok(_) => {
                        show_message(tl!("stabilize-requested")).ok();
                    }
                }
                self.review_task = None;
            }
        }
        // 审核编辑标签的结果。
        if let Some(task) = &mut self.edit_tags_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("review-edit-tags-failed")));
                    }
                    Ok(_) => {
                        show_message(tl!("review-edit-tags-done")).ok();
                    }
                }
                self.edit_tags_task = None;
            }
        }
        // 评分提交结果：无论成败都收起评分对话框。
        if let Some(task) = &mut self.rate_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("rate-failed")));
                    }
                    Ok(_) => {
                        show_message(tl!("rate-done")).ok();
                    }
                }
                self.rate_dialog.dismiss(rt);
                self.rate_task = None;
            }
        }
        // 线上谱面有更新：重新下载覆盖本地。
        if self.should_update.fetch_and(false, Ordering::Relaxed) {
            self.start_download()?;
        }
        // 本人评分状态回填（用于判断是否引导评分）。
        if let Some(task) = &mut self.my_rating_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!(?err, "failed to fetch my rating status");
                    }
                    Ok(score) => {
                        self.rate_dialog.rate.score = score;
                        self.my_rate_score = Some(score);
                    }
                }
                self.my_rating_task = None;
            }
        }
        // 兜底再轮询一次场景任务（未就绪时保持存活，下一帧继续）。
        if let Some(task) = &mut self.scene_task {
            if let Some(res) = poll_future(task.as_mut()) {
                self.next_scene = Some(res?);
                self.scene_task = None;
            }
        }
        // 收藏同步结果：把服务端返回的合集信息合并进本地并刷新收藏缓存。
        if let Some(task) = &mut self.toggle_fav_task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        show_error(err);
                    }
                    Ok((col, added)) => {
                        let data = get_data();
                        if let Some(uuid) = data.collection_uuids().iter().find(|it| data.collection_info(it).id == Some(col.id)) {
                            let uuid = *uuid;
                            let local = data.collection_info(&uuid);
                            data.set_collection_info(&uuid, local.merge(&col))?;
                        }
                        if added {
                            show_message(tl!("fav-added")).duration(1.5).ok();
                        }
                        FAV_UPDATED.store(true, Ordering::SeqCst);
                        self.is_fav = None;
                    }
                }
                self.toggle_fav_task = None;
            }
        }
        // 放弃编辑确认：收起抽屉（未保存的改动被丢弃）。
        if self.confirm_cancel_edit.swap(false, Ordering::Relaxed) {
            self.hide_side(rt);
        }
        // 返回过渡动画：背景纹理就绪且未开启「减少动态效果」时启动。
        if self.tr_start.is_nan() && self.background.lock().unwrap().is_some() && !get_data().prefer_reduced_motion {
            self.tr_start = rt;
        }

        Ok(())
    }

    /// 渲染一帧：按区块布局绘制（区块与叠放顺序见 trait 实现上方的钩子说明）。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()> {
        set_camera(&ui.camera());
        let t = tm.now() as f32;
        ui.fill_rect(ui.screen_rect(), (*self.illu.texture.1, ui.screen_rect()));
        ui.fill_rect(ui.screen_rect(), semi_black(0.55));

        // 顶栏与标题：返回按钮 + 曲名（超宽截断）与曲师；整体随淡入进度调整透明度。
        let r = ui.back_rect();
        self.back_btn.set(ui, r);
        ui.fill_rect(r, (*self.icons.back, r, ScaleType::Fit));

        let alpha = fade_in_time().map_or(1., |tt| ((t - self.fade_start) / tt).clamp(-1., 0.) + 1.);
        ui.alpha::<Result<()>>(alpha, |ui| {
            let r = ui
                .text(&self.info.name)
                .max_width(0.57 - r.right())
                .size(1.2)
                .pos(r.right() + 0.02, r.y)
                .draw();
            ui.text(&self.info.composer)
                .size(0.5)
                .pos(r.x + 0.02, r.bottom() + 0.03)
                .color(semi_white(0.8))
                .draw();

            // bottom bar
            let s = 0.25;
            let r = Rect::new(-0.94, ui.top - s - 0.06, s, s);
            let icon = self.record.as_ref().map_or(0, |it| icon_index(it.score as _, it.full_combo));
            ui.fill_rect(r, (*self.rank_icons[icon], r, ScaleType::Fit));
            let score = self.record.as_ref().map(|it| it.score).unwrap_or_default();
            let accuracy = self.record.as_ref().map(|it| it.accuracy).unwrap_or_default();
            let r = ui
                .text(format!("{score:07}"))
                .pos(r.right() + 0.01, r.center().y)
                .anchor(0., 1.)
                .size(1.2)
                .draw();
            ui.text(format!("{:.2}%", accuracy * 100.))
                .pos(r.x, r.bottom() + 0.01)
                .anchor(0., 0.)
                .size(0.7)
                .color(semi_white(0.7))
                .draw();

            // 名次徽标：数据未到时用不定进度环占位。
            if self.info.id.is_some() {
                let h = 0.09;
                let mut r = Rect::new(r.x, r.y - h, h, h);
                ui.fill_rect(r, (*self.icons.ldb, r, ScaleType::Fit));
                if let Some((rank, _)) = &self.ldb {
                    ui.text(if let Some(rank) = rank {
                        format!("#{rank}")
                    } else {
                        tl!("ldb-no-rank").into_owned()
                    })
                    .pos(r.right() + 0.01, r.center().y)
                    .anchor(0., 0.5)
                    .no_baseline()
                    .size(0.7)
                    .draw();
                } else {
                    ui.loading(
                        r.right() + 0.04,
                        r.center().y,
                        t,
                        WHITE,
                        LoadingParams {
                            radius: 0.027,
                            width: 0.007,
                            ..Default::default()
                        },
                    );
                }
                r.w += 0.13;
                self.ldb_btn.set(ui, r);
            }

            // play button
            let w = 0.26;
            let pad = 0.08;
            let r = Rect::new(1. - pad - w, ui.top - pad - w, w, w);
            self.play_btn.render_shadow(ui, r, t, |ui, path| {
                ui.fill_path(&path, semi_white(0.3));
                let r = r.feather(-0.04);
                ui.fill_rect(
                    r,
                    (
                        if self.local_path.is_some() {
                            *self.icons.play
                        } else {
                            *self.icons.download
                        },
                        r,
                        ScaleType::Fit,
                    ),
                );
            });

            // 右上角图标组：菜单 / 信息 / 收藏 / 编辑 / 模组（按本地与内置状态决定可用性）。
            ui.scope(|ui| {
                ui.dx(1. - 0.03);
                ui.dy(-ui.top + 0.03);
                let s = 0.08;
                let r = Rect::new(-s, 0., s, s);
                let cc = semi_white(0.4);
                ui.fill_rect(r, (*self.icons.menu, r, ScaleType::Fit, if self.menu_options.is_empty() { cc } else { WHITE }));
                self.menu_btn.set(ui, r);
                if self.need_show_menu {
                    self.need_show_menu = false;
                    self.menu.set_bottom(true);
                    self.menu.set_selected(usize::MAX);
                    let d = 0.28;
                    let h = self.menu_options.len().min(5) as f32 * 0.1;
                    self.menu.show(ui, t, Rect::new(r.x - d, r.bottom() + 0.02, r.w + d, h));
                }
                ui.dx(-r.w - 0.03);
                ui.fill_rect(r, (*self.icons.info, r, ScaleType::Fit));
                self.info_btn.set(ui, r);
                ui.dx(-r.w - 0.03);

                if self.local_path.as_ref().is_none_or(|it| !it.starts_with(':')) {
                    // 收藏按钮 || Favorites button
                    // TODO cache
                    let is_fav = if let Some(fav) = self.is_fav {
                        fav
                    } else {
                        let chart_ref = self.to_bare_chart_ref();
                        let fav = get_data().collections().any(|col| col.charts.iter().any(|it| it == &chart_ref));
                        self.is_fav = Some(fav);
                        fav
                    };
                    let fav_icon = if is_fav { &self.icons.star } else { &self.icons.star_outline };
                    ui.fill_rect(r, (**fav_icon, r, ScaleType::Fit));
                    self.fav_btn.set(ui, r);
                    if self.need_show_fav_menu {
                        self.need_show_fav_menu = false;
                        self.fav_menu.set_bottom(true);
                        self.fav_menu.set_selected(usize::MAX);
                        let d = 0.28;
                        let h = self.fav_menu_options.len().min(5) as f32 * 0.1;
                        self.fav_menu.show(ui, t, Rect::new(r.x - d, r.bottom() + 0.02, r.w + d, h));
                    }
                    ui.dx(-r.w - 0.03);

                    ui.fill_rect(r, (*self.icons.edit, r, ScaleType::Fit, if self.local_path.is_some() { WHITE } else { cc }));
                    self.edit_btn.set(ui, r);
                    ui.dx(-r.w - 0.03);
                }
                ui.fill_rect(r, (*self.icons.r#mod, r, ScaleType::Fit, if self.local_path.is_some() { WHITE } else { cc }));
                self.mod_btn.set(ui, r);
            });

            // 下载进度对话框（页面最上层浮层之一）。
            if let Some(dl) = &mut self.downloading {
                dl.render(ui, t);
            }

            let rt = tm.real_time() as f32;
            // 右侧抽屉：按 `side_enter_time` 计算滑入进度与遮罩透明度，并按内容类型分发渲染。
            if self.side_enter_time.is_finite() {
                let p = edit_transit().map_or(1., |t| ((rt - self.side_enter_time.abs()) / t).min(1.));
                let p = 1. - (1. - p).powi(3);
                let p = if self.side_enter_time < 0. { 1. - p } else { p };
                ui.fill_rect(ui.screen_rect(), semi_black(p * 0.6));
                let w = self.side_content.width();
                let lf = f32::tween(&1.04, &(1. - w), p);
                ui.scope(|ui| {
                    ui.dx(lf);
                    ui.dy(-ui.top);
                    let r = Rect::new(-0.2, 0., 0.2 + w, ui.top * 2.);
                    ui.fill_rect(r, (Color::default(), (r.x, r.y), Color::new(0., 0., 0., p * 0.7), (r.right(), r.y)));

                    match self.side_content {
                        SideContent::Edit => self.side_chart_info(ui, rt),
                        SideContent::Leaderboard => {
                            self.side_ldb(ui, rt);
                            Ok(())
                        }
                        SideContent::Info => {
                            self.side_info(ui, rt);
                            Ok(())
                        }
                        SideContent::Mods => {
                            self.side_mods(ui, rt);
                            Ok(())
                        }
                    }
                })?;
            }

            Ok(())
        })?;

        // 弹出菜单（置于抽屉之上）。
        self.menu.render(ui, t, 1.);
        self.fav_menu.render(ui, t, 1.);

        // 各任务进行中的全屏加载态。
        if self.save_task.is_some() {
            ui.full_loading(tl!("edit-saving"), t);
        }
        if self.upload_task.is_some() {
            ui.full_loading(tl!("uploading"), t);
        }
        if self.review_task.is_some() {
            ui.full_loading(tl!("review-doing"), t);
        }
        if self.export_task.is_some() {
            ui.full_loading(tl!("exporting"), t);
        }
        if self.edit_tags_task.is_some()
            || self.rate_task.is_some()
            || self.overwrite_task.is_some()
            || self.update_cksum_task.is_some()
            || self.toggle_fav_task.is_some()
            || self.autocomplete_task.is_some()
        {
            ui.full_loading_simple(t);
        }
        let rt = tm.real_time() as f32;
        self.tags.render(ui, rt);
        self.rate_dialog.render(ui, rt);

        // 从游戏返回的过渡：背景图从上方落下并渐隐。
        if !self.tr_start.is_nan() {
            let p = ((rt - self.tr_start - 0.2) / 0.4).clamp(0., 1.);
            if p >= 1. {
                self.tr_start = f32::NAN;
            }
            let p = 1. - (1. - p).powi(3);
            let mut r = ui.screen_rect();
            r.y += r.h * (1. - p);
            rect_shadow(r, 0.01, 0.5);
            ui.fill_rect(r, (**self.background.lock().unwrap().as_ref().unwrap(), r));
            ui.fill_rect(r, semi_black(0.3));
        }

        self.sf.render(ui, t);

        Ok(())
    }

    /// 决定是否离开本页：过渡动画未结束时留在本页；否则取出待切场景，
    /// 并在真正切换前清理回填的背景纹理与预览音乐，避免资源泄漏与声音残留。
    fn next_scene(&mut self, tm: &mut TimeManager) -> NextScene {
        if !self.tr_start.is_nan() {
            return NextScene::None;
        }
        if let Some(scene) = self.next_scene.take().or_else(|| self.sf.next_scene(tm.now() as _)) {
            *self.background.lock().unwrap() = None;
            if let Some(music) = &mut self.preview {
                let _ = music.pause();
            }
            scene
        } else {
            NextScene::None
        }
    }
}

/// 把 `src` 目录递归打包成 zip 写入 `dst`。
///
/// 两处复用：导出本地谱面（另存为 zip）与上传谱面（打包后交给 `Client::upload_file`）。
/// 保留每个条目的修改时间（缺失时用当前时间兜底），并显式创建子目录条目以保证解压端能还原
/// 目录结构；权限统一 0o755，避免解压后因权限丢失导致谱面资源不可读。
pub fn compress_folder<W: Write + Seek>(src: &Path, dst: &mut W) -> Result<()> {
    let mut zip = ZipWriter::new(dst);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        let name = path.strip_prefix(src)?;
        let mod_time = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| DateTime::<Utc>::from(t).naive_utc())
            .unwrap_or_else(|| Utc::now().naive_utc());
        if path.is_file() {
            zip.start_file_from_path(name, options.last_modified_time(mod_time.try_into().unwrap_or_default()))?;
            let mut f = File::open(path)?;
            std::io::copy(&mut f, &mut zip)?;
        } else if !name.as_os_str().is_empty() {
            zip.add_directory_from_path(name, options.last_modified_time(mod_time.try_into().unwrap_or_default()))?;
        }
    }
    zip.finish()?;
    Ok(())
}
