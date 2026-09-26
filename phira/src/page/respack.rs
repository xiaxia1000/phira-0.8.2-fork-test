//! 资源包（resource pack，简称 respack）管理页。
//!
//! 资源包是玩家自定义的「皮肤」：替换音符/判定线贴图、hold 外观、打击特效、判定配色与打击音效等。
//! 每个资源包是 `data/respack/<目录名>` 下的一个目录，内部含 `info.yml` 清单
//! （字段与 [`prpr::core::ResPackInfo`] 对应）以及若干贴图与音频文件；
//! 解析后的运行期形态是 [`prpr::core::ResourcePack`]。
//!
//! 本页只做三件事：扫描并校验本地包、切换「启用」的资源包、导入/导出/删除包。
//! 「同一时间只能启用一个资源包」这一约束由 `Data::respack_id` 这**一个索引**表达，
//! 索引 0 固定为内置默认资源包（它不对应任何目录，因此不可删除、不可导出）。

prpr_l10n::tl_file!("respack");

use super::{
    library::{request_export, resolve_export, take_export},
    Page, SharedState,
};
use crate::{
    dir, get_data, get_data_mut,
    icons::Icons,
    save_data,
    scene::{compress_folder, confirm_delete, MainScene},
};
use anyhow::Result;
use macroquad::prelude::*;
use prpr::{
    core::{NoteStyle, ParticleEmitter, ResPackInfo, ResourcePack},
    ext::{create_audio_manger, poll_future, semi_black, semi_white, LocalTask, RectExt, SafeTexture, ScaleType},
    scene::{request_file, show_error, show_message},
    ui::{DRectButton, Dialog, Scroll, Ui},
};
use sasa::{AudioManager, PlaySfxParams, Sfx};
use serde_yaml::Error;
use std::{
    borrow::Cow,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
};

/// 依据资源包清单构建打击粒子发射器（用于右侧预览演示）。
///
/// 粒子基准尺寸取玩家设置的 `note_scale` 再乘 0.6：粒子只是打击的视觉点缀，
/// 若与音符同尺寸会盖住音符本体。是否显示白色方块辅粒子交给资源包的 `hide_particles` 决定，
/// 因为部分风格化的特效包只需要主特效、加上辅粒子反而破坏观感。
fn build_emitter(pack: &ResourcePack) -> Result<ParticleEmitter> {
    ParticleEmitter::new(pack, get_data().config.note_scale * 0.6, pack.info.hide_particles)
}

/// 把资源包名净化为可用作文件名的字符串，供导出 `*.zip` 使用。
///
/// 之所以逐个字符替换而不是直接剔除非法字符：包名由用户输入，可能含 `:`、`/`、`\` 等
/// 在 Windows 上非法或会被系统当作路径分隔符的字符；替换为 `_` 既保留可读性，
/// 又避免写出到目标目录之外。
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

/// 列表中的一个资源包条目。
///
/// 条目本身只持有「路径 + 展示名」这类廉价信息，真正解码好的贴图与音频放在 `loaded` 中按需填充：
/// 用户的 `data/respack` 下可能堆了很多包，构造时一次性全部解码会白白吃掉显存与启动时间。
pub struct ResPackItem {
    /// 包目录路径；`None` 表示内置默认资源包（它没有磁盘目录，因而不可删除、不可导出）
    path: Option<PathBuf>,
    /// 展示名：本地包取自 `info.yml` 的 `name`，默认包用本地化文案
    name: String,
    /// 列表行按钮，仅用于命中测试（点击即切换启用项）
    btn: DRectButton,

    /// 已解码完成的资源包；`None` 表示尚未加载或仍在加载中
    loaded: Option<ResourcePack>,
    /// 正在进行的异步加载任务；`Some` 期间界面禁止切换启用项，避免加载结果错位到别的条目
    load_task: LocalTask<Result<ResourcePack>>,
}

// 条目的加载生命周期：构造只登记信息，解码交给 `load` 发起的异步任务。
impl ResPackItem {
    /// 创建一个尚未加载的条目。
    ///
    /// # Arguments
    /// * `path` — 包目录；传 `None` 表示内置默认资源包
    /// * `name` — 列表展示名
    pub fn new(path: Option<PathBuf>, name: String) -> Self {
        Self {
            path,
            name,
            btn: DRectButton::new(),

            loaded: None,
            load_task: None,
        }
    }

    /// 发起（或重新发起）加载。
    ///
    /// 若已有解码结果，则把它直接包成一个「已完成的任务」而不是重新读盘：来回切换列表项很常见，
    /// 复用已解码的纹理可以避免重复的磁盘 IO 与贴图上传；只有确实还没加载过时才走 `from_path`。
    ///
    /// 采用「先写 `load_task`、由 `update` 轮询后搬运到 `loaded`」的两段式，是为了不阻塞渲染帧。
    fn load(&mut self) {
        if let Some(loaded) = self.loaded.take() {
            self.load_task = Some(Box::pin(async move { Ok(loaded) }));
        } else {
            self.load_task = Some(Box::pin(ResourcePack::from_path(self.path.clone())));
        }
    }
}

/// 资源包页面：左侧是包列表与「+」导入按钮，右侧是选中包的预览
/// （click/drag/flick 贴图预览、hold 外观、周期性的打击特效演示）。
///
/// 本页同时承担「启用」职责：被选中的条目就是当前生效的资源包，
/// 该选择通过 `Data::respack_id` 持久化，因此重启后仍是同一个包。
pub struct ResPackPage {
    /// 音频管理器，用于为预览创建打击音效（`Sfx` 必须绑定音频设备，故只能在此创建）
    audio: AudioManager,
    /// 全部可选条目；下标 0 恒为内置默认包
    items: Vec<ResPackItem>,
    /// 列表底部的「+」导入按钮
    import_btn: DRectButton,
    /// 左侧列表的滚动容器
    btns_scroll: Scroll,
    /// 当前选中（即当前启用）的条目下标，与 `Data::respack_id` 保持同步
    index: usize,

    /// 共享图标集（删除/信息/导出按钮的图案）
    icons: Arc<Icons>,

    /// 右下角「信息」按钮，仅在选中包解码成功后显示
    info_btn: DRectButton,
    /// 右下角「删除」按钮
    delete_btn: DRectButton,
    /// 右上角「导出为 zip」按钮，默认包不可用
    export_btn: DRectButton,
    /// 导出后台线程的完成通知；`Some` 期间不应重复发起新的导出
    export_task: Option<mpsc::Receiver<Result<()>>>,
    /// 待导出的源目录：点击导出时记录，等用户在系统对话框里选好目标后才真正使用
    export_path: Option<PathBuf>,

    /// 删除确认的回传标志：由确认对话框回调置位，`update` 中轮询消费
    should_delete: Arc<AtomicBool>,

    /// 预览用粒子发射器，随选中包的解码结果重建
    emitter: Option<ParticleEmitter>,
    /// 预览用打击音效（索引 0/1/2 依次对应 click/drag/flick），随选中包重建
    sfxs: Option<[Sfx; 3]>,
    /// 上一次播放特效的周期编号：用于保证同一周期只触发一次粒子与音效
    last_round: u32,
}

// 构造页面：扫描并校验本地资源包目录，恢复上次启用的资源包。
impl ResPackPage {
    /// 创建资源包页面。
    ///
    /// 构造时顺带完成一次「清理 + 自愈」：遍历 `Data::respacks` 记录的目录，目录已不存在、
    /// 缺 `info.yml`、或清单解析失败者一律从记录中剔除（后两者还会直接删除目录），
    /// 因为损坏的包既无法展示也无从修复。成功解析的目录才进入列表。
    ///
    /// # Arguments
    /// * `icons` — 共享图标集
    ///
    /// # Returns
    /// 构造完成的页面。若 `respack_id` 因条目减少而越界，会被夹到合法范围并写回 `Data`。
    ///
    /// # Errors
    /// 资源包目录解析、音频管理器创建或 `save_data` 落盘失败时返回错误。
    pub fn new(icons: Arc<Icons>) -> Result<Self> {
        // 丢弃构造前残留的导入通知：导入完成发生在文件选择回调里，这里只要干净的新列表
        MainScene::take_imported_respack();
        // 阶段一：读取资源包根目录，逐个校验 `Data::respacks` 记录的目录是否仍然可用
        let dir = dir::respacks()?;
        let mut items = vec![ResPackItem::new(None, tl!("default").into_owned())];
        let data = get_data_mut();
        data.respacks = data
            .respacks
            .clone()
            .into_iter()
            .filter(|path| -> bool {
                // `respacks` 里存的是相对 `data/respack` 的子目录名，此处拼成绝对路径
                let p = format!("{dir}/{path}");
                let p = Path::new(&p);
                // 目录被用户手动删除：静默丢弃该记录，不影响其它包
                if !p.is_dir() {
                    return false;
                }
                // 打开清单文件；打不开说明这个目录根本不是合法资源包
                let cfg = File::open(p.join("info.yml"));
                match cfg {
                    Err(_) => {
                        // 清单缺失：视为损坏目录，连同目录一起删除
                        let _ = std::fs::remove_dir_all(p);
                        false
                    }
                    Ok(cfg) => {
                        // 清单可能缺字段或类型不符，解析失败同样按损坏处理
                        let info: Result<ResPackInfo, Error> = serde_yaml::from_reader(cfg);
                        match info {
                            Err(_) => {
                                let _ = std::fs::remove_dir_all(p);
                                false
                            }
                            Ok(info) => {
                                // 校验通过：收录进列表，展示名取清单里的 name
                                items.push(ResPackItem::new(Some(p.to_owned()), info.name));
                                true
                            }
                        }
                    }
                }
            })
            .collect();

        // 阶段二：恢复上次启用项。条目数可能因上面的清理而减少，故对下标做夹取
        let respack_id = get_data().respack_id;
        let index = respack_id.min(items.len().saturating_sub(1));
        if index != respack_id {
            data.respack_id = index;
        }

        // 阶段三：先落盘修正后的索引，再发起选中包的异步加载
        save_data()?;
        items[index].load();
        let delete_btn = DRectButton::new().with_delta(-0.004).with_elevation(0.);
        Ok(Self {
            audio: create_audio_manger(&get_data().config)?,
            items,
            import_btn: DRectButton::new(),
            btns_scroll: Scroll::new(),
            index,

            icons,

            info_btn: delete_btn.clone(),
            export_btn: delete_btn.clone(),
            delete_btn,
            export_task: None,
            export_path: None,

            should_delete: Arc::new(AtomicBool::default()),

            emitter: None,
            sfxs: None,
            last_round: u32::MAX,
        })
    }
}

// `Page` 钩子行为：
// - `label` 提供侧边栏标题；
// - `touch` 只做命中分发（滚动 / 导入 / 切换包 / 信息弹窗 / 导出 / 删除），不在回调里做耗时工作；
// - `update` 推进异步加载、删除确认、导出流程，并接收在别处完成的新导入包；
// - `render` 绘制左侧列表与右侧预览（含周期性打击特效演示）。
impl Page for ResPackPage {
    /// 侧边栏显示的页面标题。
    fn label(&self) -> Cow<'static, str> {
        tl!("label")
    }

    /// 处理触摸事件，按固定优先级做命中分发。
    ///
    /// 顺序是有意为之：滚动容器覆盖整块列表区域，必须最先判断，否则后画的控件会把滚动吞掉；
    /// 选中项正在加载时禁止切换条目，避免加载回调把结果错位到新条目上。
    ///
    /// # Returns
    /// 命中任意控件并消费事件时返回 `true`。
    ///
    /// # Errors
    /// 切换启用项后 `save_data` 落盘失败会返回错误。
    fn touch(&mut self, touch: &Touch, s: &mut SharedState) -> Result<bool> {
        let t = s.t;
        if self.btns_scroll.touch(touch, t) {
            return Ok(true);
        }
        // 导入：交给平台层弹文件选择框，导入结果稍后由 `MainScene::take_imported_respack` 取回
        if self.import_btn.touch(touch, t) {
            request_file("_import_respack");
            return Ok(true);
        }
        // 切换启用项：加载中时整段跳过，保证「一次只有一个加载任务」
        if self.items[self.index].load_task.is_none() {
            for (index, item) in self.items.iter_mut().enumerate() {
                if item.btn.touch(touch, t) {
                    self.index = index;
                    // 立即持久化，否则异常退出后会丢失「当前启用哪个包」这一状态
                    get_data_mut().respack_id = index;
                    save_data()?;
                    item.load();
                    return Ok(true);
                }
            }
        }
        // 只有解码成功的包才有清单信息可展示
        if self.items[self.index].loaded.is_some() && self.info_btn.touch(touch, t) {
            let item = &self.items[self.index];
            let info = &item.loaded.as_ref().unwrap().info;
            Dialog::plain(
                tl!("info"),
                tl!("info-content", "name" => item.name.clone(), "author" => info.author.clone(), "desc" => info.description.clone()),
            )
            .listener(|_dialog, pos| pos == -2)
            .show();
            return Ok(true);
        }
        // 默认包没有对应的磁盘目录，无法打包导出
        if self.index != 0 && self.export_btn.touch(touch, t) {
            let name = sanitize_filename(&self.items[self.index].name);
            self.export_path = self.items[self.index].path.clone();
            request_export(format!("{name}.zip"));
            return Ok(true);
        }
        // 默认包是内置资源，删除它会让游戏无可用资源包，故直接拒绝并提示
        if self.delete_btn.touch(touch, t) {
            if self.index == 0 {
                show_message(tl!("cant-delete-builtin")).error();
                return Ok(true);
            }
            confirm_delete(self.should_delete.clone());
            return Ok(true);
        }
        Ok(false)
    }

    /// 推进页面上的异步状态。
    ///
    /// 涵盖四件事：选中包的解码结果、删除确认、导出目标选择与导出线程、外部导入的新包。
    /// 之所以全部收拢在 `update` 里轮询，是为了让 `touch`/`render` 里不出现任何阻塞 IO。
    ///
    /// # Errors
    /// 删除目录、写回 `Data::respacks`/`respack_id` 或 `save_data` 失败时返回错误。
    fn update(&mut self, s: &mut SharedState) -> Result<()> {
        let t = s.t;
        self.btns_scroll.update(t);
        // 阶段一：收集中选包的解码结果；成功则重建预览用的发射器与打击音效
        let item = &mut self.items[self.index];
        if let Some(task) = &mut item.load_task {
            if let Some(res) = poll_future(task.as_mut()) {
                match res {
                    Err(err) => {
                        show_error(err.context(tl!("load-failed")));
                    }
                    Ok(val) => {
                        // 预览的粒子与音效必须按新包重建：它们的贴图/音频都来自该包
                        self.emitter = Some(build_emitter(&val)?);
                        self.sfxs = Some([
                            self.audio.create_sfx(val.sfx_click.clone(), None)?,
                            self.audio.create_sfx(val.sfx_drag.clone(), None)?,
                            self.audio.create_sfx(val.sfx_flick.clone(), None)?,
                        ]);
                        item.loaded = Some(val);
                    }
                }
                item.load_task = None;
            }
        }
        // 阶段二：用户已确认删除——先删目录，再同步内存状态
        if self.should_delete.fetch_and(false, Ordering::Relaxed) {
            std::fs::remove_dir_all(self.items[self.index].path.as_ref().unwrap())?;
            self.items.remove(self.index);
            // `Data::respacks` 不含内置默认包，与 `items` 相差 1 个偏移量，故下标要减一
            get_data_mut().respacks.remove(self.index - 1);
            // 删除后下标前移一位，并把新的当前项落盘、加载
            self.index -= 1;
            get_data_mut().respack_id = self.index;
            save_data()?;
            self.items[self.index].load();
            show_message(tl!("deleted")).ok();
        }
        // 阶段三：导出目标选择完成（平台对话框回调）——在后台线程里压缩目录
        if let Some(config) = take_export() {
            match config {
                Ok(config) => {
                    let file = config.file;
                    let deleter = config.deleter;
                    if let Some(path) = self.export_path.take() {
                        let (tx, rx) = mpsc::channel();
                        self.export_task = Some(rx);
                        // 压缩可能耗时，放到独立线程；用 channel 而非 JoinHandle 是为了顺带传回错误
                        std::thread::spawn(move || {
                            let result = (|| -> Result<()> {
                                let mut writer = BufWriter::new(file);
                                compress_folder(&path, &mut writer)?;
                                writer.flush()?;
                                Ok(())
                            })();
                            // 失败时清掉半成品文件，避免留下损坏的 zip
                            if result.is_err() {
                                let _ = (deleter)();
                            }
                            let _ = tx.send(result);
                        });
                    } else {
                        // 没有待导出目录却收到回调：属于状态错乱，释放文件并报错
                        drop(file);
                        let _ = (deleter)();
                        show_error(anyhow::anyhow!("No resource pack selected for export"));
                    }
                }
                Err(err) => {
                    show_error(err.into());
                }
            }
        }
        // 阶段四：轮询导出线程
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
                    // 线程 panic 会让发送端被丢弃，此时只可能是失败
                    show_error(anyhow::anyhow!("Export thread panicked"));
                    self.export_task = None;
                }
            }
        }
        // 阶段五：接收在文件选择回调里解析好的新导入包，追加到列表末尾（不自动启用）
        if let Some(item) = MainScene::take_imported_respack() {
            self.items.push(item);
        }
        Ok(())
    }

    /// 渲染整页：左侧包列表，右侧选中包预览。
    ///
    /// # Errors
    /// 绘制本身不产生错误；保留 `Result` 仅为满足 `Page` 签名。
    fn render(&mut self, ui: &mut Ui, s: &mut SharedState) -> Result<()> {
        let t = s.t;

        // 布局：从内容区左侧让出固定宽度给列表，其余作为预览区
        let mut cr = ui.content_rect();
        let d = 0.29;
        cr.x += d;
        cr.w -= d;
        let r = Rect::new(-0.92, cr.y, 0.47, cr.h);

        // 左侧列表：顶部是各资源包行，底部固定一个「+」导入按钮
        s.render_fader(ui, |ui| {
            ui.fill_path(&r.rounded(0.005), semi_black(0.4));
            let pad = 0.02;
            self.btns_scroll.size((r.w, r.h - pad));
            ui.dx(r.x);
            ui.dy(r.y + pad);
            self.btns_scroll.render(ui, |ui| {
                let w = r.w - pad * 2.;
                let mut h = 0.;
                let r = Rect::new(pad, 0., r.w - pad * 2., 0.1);
                for (index, item) in self.items.iter_mut().enumerate() {
                    item.btn.render_text(ui, r, t, &item.name, 0.7, index == self.index);
                    ui.dy(r.h + pad);
                    h += r.h + pad;
                }
                self.import_btn.render_text(ui, r, t, "+", 0.8, false);
                ui.dy(r.h + pad);
                h += r.h + pad;
                (w, h)
            });
        });

        // 右侧预览：尚未解码完成时显示转圈
        s.render_fader(ui, |ui| {
            ui.fill_path(&cr.rounded(0.005), semi_black(0.4));
            let item = &self.items[self.index];
            if let Some(pack) = &item.loaded {
                let width = 0.16;
                let mut r = Rect::new(cr.x + 0.07, cr.y + 0.1, width, 0.);
                // 每行并排画两套贴图：左为普通音符，右为多押（MH）音符，便于对比风格一致性
                let mut draw = |mut r: Rect, tex: Texture2D, mh: Texture2D| {
                    let y = r.y;
                    r.h = tex.height() / tex.width() * r.w;
                    r.y = y - r.h / 2.;
                    ui.fill_rect(r, (tex, r, ScaleType::Fit));
                    r.x += r.w * 1.8;
                    r.w *= mh.width() / tex.width();
                    r.x -= r.w / 2.;
                    r.h = mh.height() / mh.width() * r.w;
                    r.y = y - r.h / 2.;
                    ui.fill_rect(r, (mh, r, ScaleType::Fit));
                };
                let sp = (cr.h - 0.4) / 2.;
                draw(r, *pack.note_style.click, *pack.note_style_mh.click);
                r.y += sp;
                draw(r, *pack.note_style.drag, *pack.note_style_mh.drag);
                r.y += sp;
                draw(r, *pack.note_style.flick, *pack.note_style_mh.flick);
                // 并排展示两套 hold：左侧普通、右侧多押；宽度按贴图原始宽高比缩放，
                // 避免拉伸变形（多押贴图与普通贴图宽度往往不同，故以比例换算目标宽度）
                let mut r = Rect::new(0.1, cr.y + 0.1, width, cr.h - 0.38);
                let draw = |mut r: Rect, style: &NoteStyle, width: f32| {
                    let conv = |r: Rect, tex: &SafeTexture| Rect::new(r.x * tex.width(), r.y * tex.height(), r.w * tex.width(), r.h * tex.height());
                    let tr = conv(style.hold_tail_rect(), &style.hold);
                    let factor = if pack.info.hold_compact { 0.5 } else { 1. };
                    let h = tr.h / tr.w * width;
                    let r2 = Rect::new(r.x, r.y - h * factor, width, h);
                    let r2 = ui.rect_to_global(r2);
                    draw_texture_ex(
                        *style.hold,
                        r2.x,
                        r2.y,
                        semi_white(ui.alpha),
                        DrawTextureParams {
                            source: Some(tr),
                            dest_size: Some(vec2(r2.w, r2.h)),
                            ..Default::default()
                        },
                    );
                    let tr = conv(style.hold_head_rect(), &style.hold);
                    let h = tr.h / tr.w * width;
                    let r2 = Rect::new(r.x, r.bottom() - h * (1. - factor), width, h);
                    let r2 = ui.rect_to_global(r2);
                    draw_texture_ex(
                        *style.hold,
                        r2.x,
                        r2.y,
                        semi_white(ui.alpha),
                        DrawTextureParams {
                            source: Some(tr),
                            dest_size: Some(vec2(r2.w, r2.h)),
                            ..Default::default()
                        },
                    );
                    r.w = width;
                    let r2 = ui.rect_to_global(r);
                    draw_texture_ex(
                        if pack.info.hold_repeat {
                            **style.hold_body.as_ref().unwrap()
                        } else {
                            *style.hold
                        },
                        r2.x,
                        r2.y,
                        semi_white(ui.alpha),
                        DrawTextureParams {
                            source: Some({
                                if pack.info.hold_repeat {
                                    let hold_body = style.hold_body.as_ref().unwrap();
                                    let w = hold_body.width();
                                    Rect::new(0., 0., w, r2.h / width / 2. * w)
                                } else {
                                    conv(style.hold_body_rect(), &style.hold)
                                }
                            }),
                            dest_size: Some(vec2(r2.w, r2.h)),
                            ..Default::default()
                        },
                    )
                };
                draw(r, &pack.note_style, width);
                r.x += width + 0.04;
                draw(r, &pack.note_style_mh, width * pack.note_style_mh.hold.width() / pack.note_style.hold.width());
                let x = cr.x + 0.05;
                if let Some(emitter) = &mut self.emitter {
                    emitter.draw(get_frame_time());
                };

                // 打击特效演示：每 1.5 秒一轮，依次循环 click/drag/flick，
                // 音符从上方落到判定线，越过判定线的那一帧才发射粒子并播放音效（保证每轮只触发一次）
                let inter = 1.5;
                let rnd = t.div_euclid(inter);
                let irnd = rnd as u32;
                let tex = match irnd % 3 {
                    0 => *pack.note_style.click,
                    1 => *pack.note_style.drag,
                    2 => *pack.note_style.flick,
                    _ => unreachable!(),
                };
                let st = r.y + 0.06;
                let cx = r.x + 0.43;
                let line = 0.12;
                ui.fill_rect(Rect::new(cx - 0.2, line - 0.004, 0.4, 0.008), WHITE);
                let p = (t - inter * rnd) / 0.9;
                if p <= 1. {
                    let y = st + (line - st) * p;
                    let h = tex.height() / tex.width() * width;
                    let r = Rect::new(cx - width / 2., y - h / 2., width, h);
                    ui.fill_rect(r, (tex, r, ScaleType::Fit));
                } else if irnd != self.last_round {
                    if let Some(emitter) = &mut self.emitter {
                        emitter.emit_at(vec2(cx, line), 0., pack.info.fx_perfect());
                    }
                    if let Some(sfxs) = &mut self.sfxs {
                        let _ = sfxs[(irnd % 3) as usize].play(PlaySfxParams {
                            amplifier: get_data().config.volume_sfx,
                        });
                    }
                    self.last_round = irnd;
                }
                ui.text(&item.name)
                    .pos(x, cr.bottom() - 0.05)
                    .anchor(0., 1.)
                    .max_width(cr.right() - x - 0.05)
                    .size(1.2)
                    .draw();
            } else {
                let ct = cr.center();
                ui.loading(ct.x, ct.y, t, WHITE, ());
            }
            // 右下角按钮：删除恒显示；信息按钮仅在解码成功后出现
            let s = 0.12;
            let mut tr = Rect::new(cr.right() - 0.04 - s, cr.bottom() - 0.04 - s, s, s);
            self.delete_btn.render_shadow(ui, tr, t, |ui, path| {
                ui.fill_path(&path, semi_black(0.2));
                let r = tr.feather(-0.02);
                ui.fill_rect(r, (*self.icons.delete, r, ScaleType::Fit));
            });
            if item.loaded.is_some() {
                tr.x -= tr.w + 0.02;
                self.info_btn.render_shadow(ui, tr, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.2));
                    let r = tr.feather(-0.02);
                    ui.fill_rect(r, (*self.icons.info, r, ScaleType::Fit));
                });
            }
            // 默认包没有磁盘目录，无法导出，故隐藏导出入口
            if self.index != 0 {
                let size = 0.06;
                let font_size = 0.56;
                let pad = 0.02;
                let text_width = ui.text(tl!("export")).size(font_size).measure().w;
                let mut r = Rect::new(cr.right() - pad, cr.y + pad, text_width + size + pad * 3., size + pad * 2.);
                r.x -= r.w;
                self.export_btn.render_shadow(ui, r, t, |ui, path| {
                    ui.fill_path(&path, semi_black(0.2));
                    let ir = Rect::new(r.x + pad, r.y + pad, size, size);
                    ui.fill_rect(ir, (*self.icons.export, ir, ScaleType::Fit));
                    ui.text(tl!("export"))
                        .pos(ir.right() + pad, r.y + r.h / 2.)
                        .anchor(0., 0.5)
                        .no_baseline()
                        .size(font_size)
                        .draw();
                });
            }
        });
        Ok(())
    }
}
