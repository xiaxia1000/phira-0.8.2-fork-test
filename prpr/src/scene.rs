//! Scene management module.
#![allow(unused_macros)]
//!
//! 场景栈驱动器（scene stack driver）。本模块是引擎与上层应用之间唯一的场景契约层。
//!
//! 凡实现 `Scene` 的类型都可以被 [`Main`] 压入场景栈成为当前活动场景。上层应用
//! （phira）只负责构造「根场景」并交给 [`Main::new`]；此后的一切——场景的入栈/出栈/替换、
//! 时间轴（`TimeManager`）推进、触摸分发、全局提示条/弹窗/全屏加载遮罩的渲染——
//! 都由本模块统一调度。
//!
//! 设计要点：场景切换被抽象成 `NextScene` 这一「意向」，而不是让场景直接操作栈。
//! 场景只在 `update` 中声明自己希望如何切换，真正的栈操作由 [`Main::update_with_mutate`]
//! 在每帧开头集中执行。这样做保证切换时序单点可控，也避免场景在渲染中途改动栈结构。
//!
//! 另外，全局叠加层（`BILLBOARD`/`DIALOG`/`FULL_LOADING`）与部分交互槽
//! （`INPUT_TEXT`/`INPUT_CANCELLED`/`CHOSEN_FILE`）都放在 thread_local/静态变量中，
//! 使异步回调（原生输入框、文件选择器）无需持有场景引用即可把结果交回主逻辑。

prpr_l10n::tl_file!("scene" ttl);

/// 结算界面：展示本局成绩、评级、成绩上传与重开/继续。
mod ending;
// 对外只暴露结算场景本身与成绩更新状态：前者用于入栈，后者是上传回调的返回类型。
pub use ending::{EndingScene, RecordUpdateState};

/// 游玩场景：谱面实际打歌流程，以及游玩模式与简化版成绩记录。
mod game;
pub use game::{GameMode, GameScene, SimpleRecord};

/// 加载场景：真正开局前预解析谱面与资源，并向玩家展示加载进度与错误。
mod loading;
// 对外暴露加载场景、玩家基础信息，以及由上层应用注入的三个回调类型：
// 上传成绩、每帧更新世界（判定/特效驱动）、保存本地成绩。
pub use loading::{BasicPlayer, LoadingScene, SaveFn, UpdateFn, UploadFn};

use crate::{
    ext::{draw_image, screen_aspect, LocalTask, SafeTexture, ScaleType},
    judge::Judge,
    time::TimeManager,
    ui::{BillBoard, Dialog, Message, MessageHandle, MessageKind, TextPainter, Ui},
};
use anyhow::{Error, Result};
use cfg_if::cfg_if;
use inputbox::{
    backend::{default_backend, Backend},
    InputBox,
};
use macroquad::prelude::*;
use std::{
    any::Any,
    borrow::Cow,
    cell::RefCell,
    sync::{Arc, Mutex},
};
use tracing::warn;

/// 场景通过 [`Scene::next_scene`] 返回的「下一步动作」意向。
///
/// 场景本身不直接操作场景栈，而是返回本枚举描述期望的切换方式，由 [`Main`] 统一执行。
/// 这样能保证栈结构始终与时间轴栈（`Main::times`）成对，不会被场景的局部逻辑破坏。
///
/// [`Default`] 实现为 [`NextScene::None`]，因此只想维持现状的场景可以直接返回默认值。
#[derive(Default)]
pub enum NextScene {
    /// 保持当前场景不变，继续本帧的更新与渲染。绝大多数帧返回的都是这个值。
    #[default]
    None,
    /// 弹出栈顶场景，回到它下面的场景；同时把时间轴回退到该场景压栈之前保存的时间点。
    Pop,
    /// 连续弹出 `num` 层场景（例如从「结算」一次性退回「选曲」上层菜单）。
    /// 执行时若栈内场景不足 `num` 个会 panic，层数需由调用方自行保证正确。
    PopN(usize),
    /// 弹出栈顶场景，并把 `result` 交给下层场景的 [`Scene::on_result`] 处理。
    /// 典型用途：子界面把用户选择、加载失败原因等回传给父界面。
    PopWithResult(Box<dyn Any>),
    /// [`PopN`](NextScene::PopN) 与 [`PopWithResult`](NextScene::PopWithResult) 的组合：
    /// 连续弹出 `num` 层后，只把结果交给最终留在栈顶的那个场景。
    /// 例如结算界面一次退回两层，并把本局成绩交给选曲界面。
    PopNWithResult(usize, Box<dyn Any>),
    /// 请求退出游戏。只置位 `Main::should_exit`，真正的进程退出由宿主（上层应用）决定。
    Exit,
    /// 把新场景压到栈顶，旧场景仍留在栈中（处于被覆盖的暂停语义下）。
    /// 适合「临时覆盖一层」的场景（设置页、确认框等），返回时用 [`Pop`](NextScene::Pop) 即可复原。
    Overlay(Box<dyn Scene>),
    /// 用新场景替换栈顶场景，旧场景被直接丢弃且不写入 `times`。
    /// 适合「不可能再返回」的切换，例如加载场景换成游玩场景：既省内存也不污染返回栈。
    Replace(Box<dyn Scene>),
}

// 以下三个全局叠加层都是 thread_local：它们只属于「执行游戏逻辑的那条线程」，
// 因此无需加锁，也天然隔离了后台异步线程的访问。渲染时由 Main::render 叠在当前场景之上，
// 构成「场景 < 提示条 < 弹窗 < 全屏加载」这一固定层级。
thread_local! {
    // 提示条（toast）队列，以及它自己的独立时间轴。
    // 独立 TimeManager 让提示动画不受场景时间轴的 pause/seek 影响，切场景时提示不会跳变。
    pub static BILLBOARD: RefCell<(BillBoard, TimeManager)> = RefCell::new((BillBoard::new(), TimeManager::default()));
    // 当前弹出的模态对话框。为 Some 时触摸会优先交给它，且不再下发给场景。
    pub static DIALOG: RefCell<Option<Dialog>> = const { RefCell::new(None) };
    // 全屏加载遮罩。是否继续显示由 FullLoadingView::keep_alive 的引用计数决定（见 Main::render）。
    pub static FULL_LOADING: RefCell<Option<FullLoadingView>> = const { RefCell::new(None) };
}

/// 全屏加载遮罩的状态。
///
/// 生命周期不靠显式的「结束加载」调用管理，而是靠引用计数：调用方握住
/// [`begin`](Self::begin) 返回的 `Arc<()>` 句柄，只要句柄还活着遮罩就显示；所有句柄
/// 析构后（`Arc::strong_count == 1`，只剩内部保留的那一份），[`Main::render`] 会在
/// 下一帧自动移除遮罩。这样即使异步任务中途出错提前返回，也不会出现遮罩永久卡死。
pub struct FullLoadingView {
    // 存活哨兵：外部每持有一个 clone，就代表还有一个「正在进行中的加载」。
    keep_alive: Arc<()>,
    // 遮罩上显示的文字；None 表示只显示转圈动画、不显示文案。
    text: Option<Cow<'static, str>>,
}

// 遮罩的创建入口，以「句柄存活即显示」的方式对外暴露引用计数语义。
impl FullLoadingView {
    /// 显示无文案的加载遮罩，返回的句柄必须持有到加载结束为止，否则遮罩会立即消失。
    pub fn begin() -> Arc<()> {
        Self::begin_inner(None)
    }
    /// 显示带文案的加载遮罩（例如「正在上传」），返回的句柄同样需持有到加载结束。
    pub fn begin_text(text: Cow<'static, str>) -> Arc<()> {
        Self::begin_inner(Some(text))
    }
    // 真正写入线程本地槽：内部存一份 Arc，另返回一份给调用方。
    // 因此「调用方持有的句柄数 + 1」就是判定是否仍需显示遮罩的依据。
    fn begin_inner(text: Option<Cow<'static, str>>) -> Arc<()> {
        let arc = Arc::new(());
        let ret = arc.clone();
        FULL_LOADING.replace(Some(Self { keep_alive: arc, text }));
        ret
    }
}

/// 把任意 [`Error`] 同时写入日志并以错误弹窗的形式展示给玩家。
///
/// 这是引擎各处统一的上报出口：界面层不必关心展示细节，日志则保证错误在玩家截图/复现
/// 之外也能被开发侧回收。用 [`Dialog::error`] 而非提示条，是因为错误信息通常较长，
/// 需要玩家确认后再继续。
#[inline]
pub fn show_error(error: Error) {
    warn!("show error: {error:?}");
    Dialog::error(error).show();
}

/// 提示消息的建造者，采用「Drop 即显示」的写法。
///
/// 链式配置完直接丢弃（或让临时值在语句结束时析构）即可自动显示；若后续还需要取消或
/// 更新这条消息，则调用 [`handle`](Self::handle) 交出生命周期管理权，拿到 [`MessageHandle`]。
pub struct MessageBuilder {
    // 消息正文。show 时用 `mem::take` 取走，使 `handle()` 中 `mem::forget` 之后不会被重复显示。
    content: String,
    // 消息语义类型（信息/成功/警告/错误），决定图标与配色。
    kind: MessageKind,
    // 显示时长（秒）。信息类消息默认 2 秒。
    duration: f32,
}

// 链式配置与最终投递。所有配置方法都返回 Self，便于一行内写完。
impl MessageBuilder {
    /// 以正文创建建造者，默认按「信息」类型显示 2 秒。
    pub fn new(content: String) -> Self {
        Self {
            content,
            kind: MessageKind::Info,
            duration: 2.,
        }
    }

    /// 设置消息类型（信息/成功/警告/错误）。
    #[inline]
    pub fn kind(mut self, kind: MessageKind) -> Self {
        self.kind = kind;
        self
    }

    /// 覆盖默认的显示时长（秒）。
    #[inline]
    pub fn duration(mut self, t: f32) -> Self {
        self.duration = t;
        self
    }

    /// 语义糖：标记为成功消息（绿色对勾）。
    #[inline]
    pub fn ok(self) -> Self {
        self.kind(MessageKind::Ok)
    }

    /// 语义糖：标记为警告消息（黄色）。
    #[inline]
    pub fn warn(self) -> Self {
        self.kind(MessageKind::Warn)
    }

    /// 语义糖：标记为错误消息（红色）。
    #[inline]
    pub fn error(self) -> Self {
        self.kind(MessageKind::Error)
    }

    // 把消息投入 BILLBOARD 队列，并返回可用于取消的句柄。
    // 用 BILLBOARD 自己的时间轴取 now()，保证提示动画与场景时间轴解耦。
    fn show(&mut self) -> MessageHandle {
        BILLBOARD.with(|it| {
            let mut guard = it.borrow_mut();
            let (msg, handle) = Message::new(std::mem::take(&mut self.content), guard.1.now() as _, self.duration, self.kind.clone());
            guard.0.add(msg);
            handle
        })
    }

    /// 立即投递消息并返回句柄，由调用方负责该消息的后续取消/更新。
    ///
    /// 这里 `mem::forget(self)` 是关键：`show()` 已经把正文取走，若不走 forget，
    /// 方法结束时 [`Drop`] 会再投递一条空消息。因此选择「手动投递 + 跳过析构」。
    #[inline]
    pub fn handle(mut self) -> MessageHandle {
        let handle = self.show();
        std::mem::forget(self);
        handle
    }
}

// 实现 Drop 即显示：绝大多数调用点不需要保留句柄，配置完让它自然析构即可显示消息。
impl Drop for MessageBuilder {
    fn drop(&mut self) {
        self.show();
    }
}

/// 创建提示消息的便捷入口，等价于 [`MessageBuilder::new`]。
#[inline]
pub fn show_message(msg: impl Into<String>) -> MessageBuilder {
    MessageBuilder::new(msg.into())
}

/// 文本输入框与引擎之间的全局通信槽：(请求 id, 用户输入文本)。
///
/// 之所以用全局静态而非直接返回结果，是因为各平台的原生输入框回调（Android JNI、
/// iOS 弹窗、Web 页面）都在「稍后某帧」才触发，无法在当前调用栈同步拿到结果。约定：
/// 写入 id 表示发起了请求，之后由轮询方通过 [`take_input`] 取回结果。用 id 标记请求，
/// 是为了在输入框被复用或存在多个并发请求时仍能判断结果属于哪一次请求。
pub static INPUT_TEXT: Mutex<(Option<String>, Option<String>)> = Mutex::new((None, None));
/// Holds the id of the last input request the user cancelled (clicked Cancel or
/// dismissed the dialog). Consumed via [`take_input_cancelled`]; distinct from
/// [`INPUT_TEXT`] so callers can distinguish "cancelled" from "no input yet".
///
/// 单独记录「取消」而不往 [`INPUT_TEXT`] 里塞空串，是因为「用户主动取消」与
/// 「用户输入了空字符串」在业务上必须区分：前者通常意味着要退回上一级界面。
pub static INPUT_CANCELLED: Mutex<Option<String>> = Mutex::new(None);
/// 文件选择器与引擎之间的全局通信槽：(请求 id, 选中文件的本地路径)。
///
/// 与 [`INPUT_TEXT`] 同理，原生文件选择是异步的：桌面端由 `rfd` 同步返回，Android 走
/// JNI 回调、iOS 走 `UIDocumentPickerDelegate`、OpenHarmony 走平台 request 回调，
/// 各平台统一把结果写进这里，再由上层用 [`take_file`] 轮询取走。
/// 该槽仅存在于非 wasm 目标——Web 端不使用本地文件路径，改由浏览器上传接口处理。
#[cfg(not(target_arch = "wasm32"))]
pub static CHOSEN_FILE: Mutex<(Option<String>, Option<String>)> = Mutex::new((None, None));

/// 驱动 inputbox 后端完成一次异步输入，并把结果/取消/失败分别落到全局槽或日志。
///
/// 回调可能在别的线程、任意时刻触发，因此这里只做「写全局槽」这一件事，不触碰任何场景
/// 状态；真正的状态变更留给主线程的轮询方（`take_input*`），避免跨线程数据竞争。
fn show_inputbox(config: InputBox, backend: &dyn Backend) {
    let result = config.show_with_async(backend, |result| match result {
        Ok(Some(text)) => {
            INPUT_TEXT.lock().unwrap().1 = Some(text);
        }
        Ok(None) => {
            // User cancelled; report it under the pending request's id so the
            // caller can react (e.g. return to the previous screen).
            let id = INPUT_TEXT.lock().unwrap().0.clone();
            *INPUT_CANCELLED.lock().unwrap() = id;
        }
        Err(err) => {
            warn!(?err, "failed to get input");
        }
    });
    if let Err(err) = result {
        warn!(?err, "failed to show input box");
    }
}

/// 发起一次文本输入请求。
///
/// `id` 是调用方自定义的请求标识，会随结果一起回传，用于匹配「哪次请求的结果到了」。
/// 这里会先清空上一次的取消标记，避免上一轮遗留的「已取消」被本轮流程误读。
/// `config` 中未显式设置的元素一律用内置本地化文案兜底，使各调用点保持简洁。
#[inline]
pub fn request_input(id: impl Into<String>, mut config: InputBox) {
    *INPUT_TEXT.lock().unwrap() = (Some(id.into()), None);
    *INPUT_CANCELLED.lock().unwrap() = None;
    if config.title.is_none() {
        config = config.title(ttl!("input"));
    }
    if config.prompt.is_none() {
        config = config.prompt(ttl!("input-msg"));
    }
    if config.cancel_label.is_none() {
        config = config.cancel_label(ttl!("cancel"));
    }
    if config.ok_label.is_none() {
        config = config.ok_label(ttl!("confirm"));
    }
    show_inputbox(config, &*default_backend());
}

/// 取回一次已完成的输入结果 `(请求 id, 文本)`。
///
/// 采用「取出即消费」语义：文本会被清空，因此同一结果最多被返回一次，避免同一帧内
/// 被多个系统重复处理。id 予以保留，是为了让后续的取消/错误路径仍能定位到本次请求。
pub fn take_input() -> Option<(String, String)> {
    let mut w = INPUT_TEXT.lock().unwrap();
    w.0.clone().zip(std::mem::take(&mut w.1))
}

/// Returns the id of a cancelled input request once, clearing it.
///
/// 同样是「取出即清空」的一次性语义，保证取消事件只被处理一次。
pub fn take_input_cancelled() -> Option<String> {
    INPUT_CANCELLED.lock().unwrap().take()
}

/// 由平台侧（原生回调或嵌入的 Web 输入页）把用户输入写回引擎。
///
/// 它直接构造出「已完成」的状态（id 与文本同时存在），因此下一帧 [`take_input`]
/// 即可取到结果，无需再经过输入框后端。
pub fn return_input(id: String, text: String) {
    *INPUT_TEXT.lock().unwrap() = (Some(id), Some(text));
}

/// 发起一次文件选择请求。
///
/// 与 [`request_input`] 同构：先写入请求 id 表示「已发起」，各平台再用各自的原生机制弹出
/// 选择器，结果最终写回 [`CHOSEN_FILE`]，由上层轮询 [`take_file`] 取走。
/// 之所以在入口处就写入 id，是为了让「已选中但尚未写入路径」的中间态也有据可查。
///
/// 仅存在于非 wasm 目标：Web 端直接使用浏览器的文件输入，不走本地路径这一套。
///
/// # Arguments
///
/// * `id` - 调用方自定义的请求标识，随结果回传用于匹配请求。
#[cfg(not(target_arch = "wasm32"))]
pub fn request_file(id: impl Into<String>) {
    let id: String = id.into();
    // OpenHarmony 平台需要额外告知客户端本次选择的是否为「头像」，以便其调用相册而非文件管理器。
    #[cfg(target_env = "ohos")]
    let is_photo = id == "avatar";
    *CHOSEN_FILE.lock().unwrap() = (Some(id), None);
    // 各平台实现差异极大（JNI 反射调用、Objective-C 运行时、平台 request 回调、原生对话框），
    // 用 cfg_if 按目标平台编译期分派，避免运行期分支与非目标平台的依赖被引入。
    cfg_if! {
        if #[cfg(target_os = "android")] {
            // Android 侧没有 Rust 层可用的文件选择 API，只能反射调用宿主 Activity 的
            // `chooseFile()` 方法，由 Java/Kotlin 侧弹出选择器并把结果回灌到 CHOSEN_FILE。
            unsafe {
                let env = miniquad::native::attach_jni_env();
                let ctx = ndk_context::android_context().context();
                let class = (**env).GetObjectClass.unwrap()(env, ctx);
                let method = (**env).GetMethodID.unwrap()(env, class, c"chooseFile".as_ptr() as _, c"()V".as_ptr() as _);
                (**env).CallVoidMethod.unwrap()(env, ctx, method);
            }
        } else if #[cfg(target_os = "ios")] {
            use objc2::{available, define_class, rc::Retained, runtime::ProtocolObject, MainThreadMarker, MainThreadOnly};
            use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSString, NSURL};
            use objc2_ui_kit::{UIDocumentPickerDelegate, UIDocumentPickerViewController};

            // UIDocumentPickerViewController 只弱引用 delegate，若 delegate 没有其他强引用
            // 会被立即回收、回调永不触发。因此用 thread_local 持有它直到本次选择结束。
            thread_local! {
                static DELEGATE: RefCell<Option<Retained<PickerDelegate>>> = const { RefCell::new(None) };
            }

            // 用 define_class! 在运行时声明 Objective-C 类并实现 UIDocumentPickerDelegate。
            // 之所以必须走这条路线，是因为 iOS 的文件选择回调只能以 ObjC 协议方法的形式注册，
            // 没有纯 Rust 的等价物。
            define_class! {
                // SAFETY:
                // - The superclass NSObject does not have any subclassing requirements.
                // - `Delegate` does not implement `Drop`.
                #[unsafe(super = NSObject)]
                #[thread_kind = MainThreadOnly]
                struct PickerDelegate;

                // SAFETY: `NSObjectProtocol` has no safety requirements.
                unsafe impl NSObjectProtocol for PickerDelegate {}

                // SAFETY: `UIDocumentPickerDelegate` has no safety requirements.
                unsafe impl UIDocumentPickerDelegate for PickerDelegate {
                    // SAFETY: The signature is correct.
                    #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
                    fn did_pick_documents_at_urls(&self, controller: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
                        use objc2_foundation::{NSData, NSDataReadingOptions, NSTemporaryDirectory};

                        let url = urls.firstObject().unwrap();
                        // iOS 沙盒外的文件必须先开启 security-scoped 访问权限才能读取；
                        // startAccessingSecurityScopedResource 返回 false 表示无需（也无权）关闭。
                        // 该权限是成对使用的系统资源：读取完成后必须 stop 释放，
                        // 否则会持续占用系统级的访问句柄。
                        // 注意：读取失败时提前 return 会跳过这次释放，属已知的取舍。
                        let need_close = unsafe { url.startAccessingSecurityScopedResource() };

                        let data = match NSData::dataWithContentsOfURL_options_error(&url, NSDataReadingOptions::Uncached) {
                            Ok(data) => data,
                            Err(err) => {
                                let message = err.localizedDescription().to_string();
                                show_error(Error::msg(message).context(ttl!("read-file-failed")));
                                return;
                            }
                        };
                        if need_close {
                            unsafe { url.stopAccessingSecurityScopedResource() };
                        }

                        // 原 URL 只在权限作用域内有效，作用域一结束文件就不可读。
                        // 因此把内容落盘到应用临时目录，得到一个后续任何时刻都能读取的普通路径。
                        let dir = NSTemporaryDirectory();
                        let path = format!("{}{}", dir, uuid::Uuid::new_v4());
                        data.writeToFile_atomically(&NSString::from_str(&path), true);
                        CHOSEN_FILE.lock().unwrap().1 = Some(path);
                    }
                }
            }

            // 为上面声明的 ObjC 类补一个 Rust 侧构造器：分配实例、设置 ivars 并发送 init。
            impl PickerDelegate {
                fn new(mtm: MainThreadMarker) -> Retained<Self> {
                    let this = Self::alloc(mtm).set_ivars(());
                    unsafe { objc2::msg_send![super(this), init] }
                }
            }

            // UIKit 视图控制器只能在主线程创建与呈现，MainThreadMarker 是这一约束的静态凭证；
            // 拿不到它说明当前不在主线程，属于编程错误，故直接 unwrap。
            let mtm = MainThreadMarker::new().unwrap();

            let picker = UIDocumentPickerViewController::alloc(mtm);
            // iOS 14 起推荐用 UTType 按扩展名声明可选项；更早的系统只能用已废弃的
            // 文档类型字符串（public.image/public.archive），能力更弱，故分支处理。
            let picker = if available!(ios = 14.0.0) {
                use objc2_uniform_type_identifiers::UTType;

                let ext = |e: &str| UTType::typeWithFilenameExtension(&NSString::from_str(e)).unwrap();
                let types = NSArray::from_retained_slice(&[
                    ext("zip"),
                    ext("pez"),
                    ext("jpg"),
                    ext("png"),
                    ext("jpeg"),
                    ext("json"),
                    ext("mp3"),
                    ext("ogg"),
                ]);
                UIDocumentPickerViewController::initForOpeningContentTypes(picker, &types)
            } else {
                #[allow(deprecated)]
                {
                    use objc2_ui_kit::UIDocumentPickerMode;

                    let ext = NSString::from_str;
                    let types = NSArray::from_retained_slice(&[ext("public.image"), ext("public.archive")]);
                    UIDocumentPickerViewController::initWithDocumentTypes_inMode(picker, &types, UIDocumentPickerMode::Import)
                }
            };
            // 先把 delegate 存进 thread_local 再设置，避免在 setDelegate 与保存之间出现
            // 「delegate 无人持有」的窗口期而被回收。
            let dlg_obj = PickerDelegate::new(mtm);
            picker.setDelegate(Some(ProtocolObject::from_ref(&*dlg_obj)));
            DELEGATE.with(|it| *it.borrow_mut() = Some(dlg_obj));

            // 从当前最顶层的视图控制器弹出选择器；通过 inputbox 后端的辅助函数获取，
            // 复用其「如何定位顶层 VC」的平台适配逻辑。
            inputbox::backend::IOS::get_top_view_controller(mtm)
                .unwrap()
                .presentViewController_animated_completion(&picker, true, None);
        } else if #[cfg(target_env = "ohos")] {
            // OpenHarmony 通过 miniquad 的 request 回调把选择意图转发给 ArkTS 侧，
            // isPhoto 决定调用相册还是文件管理器。
            miniquad::native::call_request_callback(format!(r#"{{"action": "chooseFile", "isPhoto": {}}}"#, is_photo));
        } else { // desktop
            // 桌面端原生对话框是同步阻塞的，因此这里可以直接把结果写回槽里；
            // 用户取消时 pick_file() 返回 None，槽里保持「只有 id、没有路径」的状态。
            CHOSEN_FILE.lock().unwrap().1 = rfd::FileDialog::new().pick_file().map(|it| it.display().to_string());
        }
    }
}

/// 取回一次已完成的文件选择结果 `(请求 id, 本地文件路径)`。
///
/// 与 [`take_input`] 同样是一次性消费语义：只有路径被取走，id 保留，
/// 这样「用户取消了选择」与「选择尚未完成」都能通过路径是否为 None 来区分。
#[cfg(not(target_arch = "wasm32"))]
pub fn take_file() -> Option<(String, String)> {
    let mut w = CHOSEN_FILE.lock().unwrap();
    w.0.clone().zip(std::mem::take(&mut w.1))
}

/// 供平台侧原生代码（Android 的 Java 层、OpenHarmony 的 ArkTS 层等）回灌文件选择结果。
///
/// 一次调用即构造出完整结果状态，下一帧 [`take_file`] 就能取到。
#[cfg(not(target_arch = "wasm32"))]
pub fn return_file(id: String, file: String) {
    *CHOSEN_FILE.lock().unwrap() = (Some(id), Some(file));
}

/// 场景契约：引擎与上层应用之间唯一的接口。
///
/// 生命周期由 [`Main`] 驱动，调用顺序在「场景切换」与「稳定帧」两种情形下不同，
/// 各方法的默认实现刻意都是空实现，目的是让绝大多数场景只实现 `update`/`render`，
/// 无需为不关心的事件写样板代码。
///
/// 稳定帧（未发生切换）的调用顺序大致为：
/// `update`（内部会先 `next_scene` 再派发 `touch`）→ `render`。
/// 发生切换时，被弹出场景的 `times` 被恢复，新栈顶场景先 `on_result`（若有结果）再 `enter`。
pub trait Scene {
    /// 场景成为栈顶（首次压栈或被上层场景弹出后重新成为栈顶）时调用。
    ///
    /// `target` 是渲染目标：`None` 表示直接绘制到窗口，`Some` 表示绘制到离屏目标
    /// （用于跨场景过渡时的画面捕获）。实现方应在此重置自身动画状态。
    fn enter(&mut self, _tm: &mut TimeManager, _target: Option<RenderTarget>) -> Result<()> {
        Ok(())
    }
    /// 被新场景覆盖（上层执行了 [`NextScene::Overlay`]）或宿主主动暂停时调用。
    ///
    /// 默认空实现：大多数场景没有需要暂停的资源；有音频/BGM 的场景需在此暂停播放。
    fn pause(&mut self, _tm: &mut TimeManager) -> Result<()> {
        Ok(())
    }
    /// 覆盖它的上层场景被弹出、本场景重新成为栈顶时调用，与 [`pause`](Self::pause) 配对。
    fn resume(&mut self, _tm: &mut TimeManager) -> Result<()> {
        Ok(())
    }
    /// 上层场景通过 [`NextScene::PopWithResult`] 或 [`NextScene::PopNWithResult`] 把结果交回时调用。
    ///
    /// `result` 用 `Box<dyn Any>` 传递，调用方需自行 downcast；默认空实现意味着
    /// 「本场景不关心子场景的返回值」。
    fn on_result(&mut self, _tm: &mut TimeManager, _result: Box<dyn Any>) -> Result<()> {
        Ok(())
    }
    /// 收到一个触摸事件（本帧所有触摸会被 [`Main`] 逐个派发）。
    ///
    /// # Returns
    ///
    /// 返回 `true` 表示该触摸已被本场景消费，[`Main`] 不再把它交给更下层或后续处理；
    /// 返回 `false`（默认）表示未消费。默认 `false` 让「纯展示型」场景可以忽略触摸。
    fn touch(&mut self, _tm: &mut TimeManager, _touch: &Touch) -> Result<bool> {
        Ok(false)
    }
    /// 每帧的逻辑推进，与 [`render`](Self::render) 同为 [`Scene`] 上必须实现的方法。
    ///
    /// 实现方通常在此驱动动画、轮询异步任务；场景切换意向也常在 `update` 之后由
    /// [`next_scene`](Self::next_scene) 给出。
    fn update(&mut self, tm: &mut TimeManager) -> Result<()>;
    /// 每帧的绘制。`ui` 提供场景内的坐标与绘制原语。
    ///
    /// 渲染不应改动逻辑状态（[`Main::render`] 允许渲染阶段与逻辑阶段分离，且暂停时会被跳过）。
    fn render(&mut self, tm: &mut TimeManager, ui: &mut Ui) -> Result<()>;
    /// 询问本场景是否希望切换场景，每帧在 `update` 之后被调用一次。
    ///
    /// 默认返回 [`NextScene::None`]（保持现状），使不参与切换的场景无需实现。
    fn next_scene(&mut self, _tm: &mut TimeManager) -> NextScene {
        NextScene::None
    }
}

/// 抽象「本帧使用哪个渲染目标」。
///
/// 之所以做成 trait，是因为调用方既可能直接给出一个固定目标（`Option<RenderTarget>`），
/// 也可能需要在每次查询时动态决定（闭包），用同一个抽象覆盖两种用法。
pub trait RenderTargetChooser {
    /// 返回当前应使用的渲染目标；`None` 表示绘制到窗口。
    fn choose(&mut self) -> Option<RenderTarget>;
}
// 固定值实现：直接返回自身携带的目标，供只用一个渲染目标的调用方使用。
impl RenderTargetChooser for Option<RenderTarget> {
    fn choose(&mut self) -> Option<RenderTarget> {
        *self
    }
}
// 闭包实现：把每次查询转发给调用方提供的闭包，支持逐帧动态变化的目标。
impl<F: FnMut() -> Option<RenderTarget>> RenderTargetChooser for F {
    fn choose(&mut self) -> Option<RenderTarget> {
        self()
    }
}

/// 场景栈驱动器：持有场景栈、时间轴与每帧输入状态，是整个游戏运行时的核心。
///
/// [`Main`] 由上层应用（phira）创建并逐帧调用 [`update`](Self::update) 与
/// [`render`](Self::render)；它本身不决定游戏内容，只负责把输入与时间分发给栈顶场景，
/// 并在场景请求切换时维护栈的一致性。
pub struct Main {
    /// 场景栈，栈顶（`last()`）即当前活动场景。与私有字段 `times` 配对使用。
    pub scenes: Vec<Box<dyn Scene>>,
    // 与 scenes 一一对应的时间轴快照：元素 i 记录 scenes[i] 被压栈时的时间。
    //
    // 不变量：`times.len() == scenes.len() - 1`。栈底（根场景）没有「压栈前的时间」，
    // 因此不占位；每当 Overlay 压入新场景时 push 当前时间，Pop/PopN 时成对 pop 并把
    // 时间轴 seek 回该快照。这保证了从覆盖层返回时，下层场景的动画时间不会凭空前进。
    times: Vec<f64>,
    // 每帧查询一次的渲染目标提供者；切换场景时会重新询问，使新场景能拿到自己的目标。
    target_chooser: Box<dyn RenderTargetChooser>,
    // 全局时间轴。触摸派发期间会被临时 seek 到各触摸的错峰时刻，随后恢复。
    tm: TimeManager,
    // 宿主暂停标记。为 true 时 update/render 直接返回，用于切后台等场景。
    paused: bool,
    // 上一帧结束时的 tm 时间，用于计算本帧经过的时间（触摸错峰时的 delta 基数）。
    last_update_time: f64,
    // 场景请求 NextScene::Exit 后被置位；由宿主查询 Main::should_exit 决定是否退出。
    should_exit: bool,
    // 是否绘制全局叠加层（提示条/弹窗/全屏加载）。上层应用在嵌套渲染（如把游戏画面
    // 叠加到播放器之上）时可临时置 false，避免叠加层被重复绘制。
    pub top_level: bool,
    // 本帧收集到的触摸集合，在 render 时交给 Ui；take() 语义使每个触摸只被消费一次。
    touches: Option<Vec<Touch>>,
    // 逻辑视口（x, y, w, h）。None 表示使用整个窗口；由宿主按需要（如画中画布局）设置。
    pub viewport: Option<(i32, i32, i32, i32)>,
}

// Main 的对外接口：初始化、逐帧驱动，以及与宿主的暂停/退出协商。
impl Main {
    /// 创建驱动器并压入初始（根）场景。
    ///
    /// 会先关闭「鼠标模拟触摸」，避免桌面端把鼠标移动误判为触摸；随后立即对根场景调用
    /// 一次 [`Scene::enter`]，保证场景从第一帧起就处于已初始化状态。最后异步预加载
    /// 提示条所需的一整套图标。
    ///
    /// # Errors
    ///
    /// 根场景 `enter` 失败或图标资源加载失败时返回错误，此时不会有可用的 Main。
    pub async fn new(mut scene: Box<dyn Scene>, mut tm: TimeManager, mut target_chooser: impl RenderTargetChooser + 'static) -> Result<Self> {
        simulate_mouse_with_touch(false);
        scene.enter(&mut tm, target_chooser.choose())?;
        let last_update_time = tm.now();
        // 把「按路径加载贴图」收敛成一个宏，避免为四张图标重复写样板代码。
        macro_rules! load_tex {
            ($path:literal) => {
                SafeTexture::from(Texture2D::from_image(&load_image($path).await?))
            };
        }
        let icons = [load_tex!("info.png"), load_tex!("warn.png"), load_tex!("ok.png"), load_tex!("error.png")];
        BILLBOARD.with(|it| it.borrow_mut().0.set_icons(icons));
        Ok(Self {
            scenes: vec![scene],
            times: Vec::new(),
            target_chooser: Box::new(target_chooser),
            tm,
            paused: false,
            last_update_time,
            should_exit: false,
            top_level: true,
            touches: None,
            viewport: None,
        })
    }

    /// 推进一帧逻辑，不对触摸做任何额外改写。
    ///
    /// 等价于 [`update_with_mutate`](Self::update_with_mutate) 传入空闭包，
    /// 供不需要修正触摸坐标/时间的宿主使用。
    pub fn update(&mut self) -> Result<()> {
        self.update_with_mutate(|_| {})
    }

    /// 推进一帧逻辑：处理场景切换意向、分发触摸、更新当前场景。
    ///
    /// `f` 会在触摸派发前对每个触摸做一次原地改写，让宿主有机会修正坐标或时间戳
    /// （例如录屏回放、外部输入注入）。
    ///
    /// 各阶段的顺序是有意为之：
    /// 1. 先执行上一帧遗留的 `next_scene` 意向，保证本帧从一开始就在正确的场景上运行；
    /// 2. 再让判定系统开启新帧（`Judge::on_new_frame`），使谱面时间轴与触摸对齐；
    /// 3. 之后才分发触摸、更新场景，避免出现「触摸被旧场景消费」的错位。
    ///
    /// # Errors
    ///
    /// 场景切换、触摸处理或 `update` 返回错误时向上传播；触摸分发中的错误会延迟到
    /// 所有触摸处理完之后再返回，以免中途中断导致剩余触摸丢失。
    pub fn update_with_mutate(&mut self, f: impl Fn(&mut Touch)) -> Result<()> {
        if self.paused {
            return Ok(());
        }
        // 阶段一：执行场景切换意向。所有栈与时间轴的操作都集中在这里，
        // 保证 scenes/times 的不变量只在这一处被修改。
        match self.scenes.last_mut().unwrap().next_scene(&mut self.tm) {
            NextScene::None => {}
            NextScene::Pop => {
                self.scenes.pop();
                self.tm.seek_to(self.times.pop().unwrap());
                self.scenes.last_mut().unwrap().enter(&mut self.tm, self.target_chooser.choose())?;
            }
            NextScene::PopN(num) => {
                for _ in 0..num {
                    self.scenes.pop();
                    self.tm.seek_to(self.times.pop().unwrap());
                }
                self.scenes.last_mut().unwrap().enter(&mut self.tm, self.target_chooser.choose())?;
            }
            NextScene::PopWithResult(result) => {
                self.scenes.pop();
                self.tm.seek_to(self.times.pop().unwrap());
                self.scenes.last_mut().unwrap().on_result(&mut self.tm, result)?;
                self.scenes.last_mut().unwrap().enter(&mut self.tm, self.target_chooser.choose())?;
            }
            NextScene::PopNWithResult(num, result) => {
                for _ in 0..num {
                    self.scenes.pop();
                    self.tm.seek_to(self.times.pop().unwrap());
                }
                self.scenes.last_mut().unwrap().on_result(&mut self.tm, result)?;
                self.scenes.last_mut().unwrap().enter(&mut self.tm, self.target_chooser.choose())?;
            }
            NextScene::Exit => {
                self.should_exit = true;
            }
            NextScene::Overlay(mut scene) => {
                self.times.push(self.tm.now());
                scene.enter(&mut self.tm, self.target_chooser.choose())?;
                self.scenes.push(scene);
            }
            NextScene::Replace(mut scene) => {
                scene.enter(&mut self.tm, self.target_chooser.choose())?;
                *self.scenes.last_mut().unwrap() = scene;
            }
        }
        // 阶段二：开启判定的新帧。必须在触摸分发之前完成，判定系统才能把本帧触摸
        // 与谱面时间轴上的音符正确对齐。
        Judge::on_new_frame();
        let mut touches = Judge::get_touches();
        // 交出触摸给宿主做一次改写（坐标修正、外部输入注入等）。
        touches.iter_mut().for_each(f);
        // 两个跳过触摸分发的条件：
        // a) 本帧没有触摸，自然不用分发；
        // b) 全屏加载遮罩正在显示——遮罩期间必须"吞掉"所有输入，
        //    否则玩家会在资源未就绪时点到下层界面，造成状态错乱。
        if !(touches.is_empty() || FULL_LOADING.with(|it| it.borrow().is_some())) {
            let now = self.tm.now();
            // 把「上一帧到本帧」的时间片按触摸数均分，给同一帧内的多个触摸
            // 赋予递增的时间戳错峰处理。这样同帧多押不会因时间完全相同而被判定
            // 视为同一次输入，也保留了玩家手指落下的先后顺序。
            let delta = (now - self.last_update_time) / touches.len() as f64;
            let start_time = self.tm.start_time;
            let mut last_err = None;
            DIALOG.with(|it| -> Result<()> {
                let mut index = 1;
                // retain_mut 的返回值表示「保留该触摸」：消费掉（返回 false）的触摸会被移除，
                // 未消费的（返回 true）留在 touches 中供 Ui 做手部动画等后续使用。
                touches.retain_mut(|touch| {
                    let t = self.last_update_time + (index + 1) as f64 * delta;
                    index += 1;
                    let mut guard = it.borrow_mut();
                    // 阶段三：模态弹窗拥有最高优先级。弹窗存在时触摸一律交给它，
                    // 且无论弹窗是否消费都返回 false（不再下发给场景），避免"点穿"弹窗。
                    if let Some(dialog) = guard.as_mut() {
                        if !dialog.touch(touch, t as _) {
                            // 弹窗自行请求关闭：立刻摘除，使后续触摸直接落到场景上。
                            drop(guard);
                            *it.borrow_mut() = None;
                        }
                        false
                    } else {
                        // 阶段四：无弹窗时把触摸交给栈顶场景。为了让场景的动画时间
                        // 与本次触摸的错峰时间一致，先 seek 到 t 再调用 touch。
                        drop(guard);
                        self.tm.seek_to(t);
                        match self.scenes.last_mut().unwrap().touch(&mut self.tm, touch) {
                            // touch 返回 true 表示场景已消费该触摸，于是不再保留它。
                            Ok(val) => !val,
                            // 单次触摸失败不至于让整帧失败：记录错误继续处理剩余触摸，
                            // 待循环结束后统一向上返回，避免丢失其它输入。
                            Err(err) => {
                                warn!(?err, "failed to handle touch");
                                last_err = Some(err);
                                false
                            }
                        }
                    }
                });
                Ok(())
            })?;
            if let Some(err) = last_err {
                return Err(err);
            }
            // 恢复时间轴起点：分发过程中被反复 seek_to 过，这里把 start_time 还原，
            // 使本帧剩余逻辑（以及下一帧的 delta 计算）仍基于原本的时间基准。
            self.tm.start_time = start_time;
        }
        // 阶段五：保存本帧触摸供 render 使用，并推进各系统的状态。
        self.touches = Some(touches);
        self.last_update_time = self.tm.now();
        // 弹窗的状态更新放在场景 update 之前，使其动画（淡入/淡出）与触摸结果同帧生效。
        DIALOG.with(|it| {
            if let Some(dialog) = it.borrow_mut().as_mut() {
                dialog.update(self.last_update_time as _);
            }
        });
        self.scenes.last_mut().unwrap().update(&mut self.tm)?;
        Ok(())
    }

    /// 绘制一帧：先画栈顶场景，再在顶层模式叠加三个全局层。
    ///
    /// 渲染与逻辑分离：本方法不推进时间、不改动场景状态，只消费 [`update`](Self::update)
    /// 阶段留下的输入快照（`touches` 被 `take` 走，保证一个触摸只被渲染逻辑消费一次）。
    ///
    /// `top_level` 为 false 时只画场景本身——嵌套渲染（例如把游戏画面作为素材渲染到
    /// 另一个画布）不应重复叠加提示条/弹窗/加载遮罩，否则会看到两套 UI。
    pub fn render(&mut self, painter: &mut TextPainter) -> Result<()> {
        if self.paused {
            return Ok(());
        }
        let mut ui = Ui::new(painter, self.viewport);
        ui.set_touches(self.touches.take());
        // scope 负责在渲染结束时恢复 UI 的裁剪/变换栈，避免场景内部改动泄漏到叠加层。
        ui.scope(|ui| self.scenes.last_mut().unwrap().render(&mut self.tm, ui))?;
        if self.top_level {
            // 叠加层需要覆盖整个逻辑屏幕，因此先保存相机状态、切到 UI 相机再绘制，
            // 结束后还原，保证不影响宿主的后续绘制。
            push_camera_state();
            set_camera(&ui.camera());
            // SAFETY: get_internal_gl 要求必须在渲染线程内、且处于相机状态成对使用的作用域中调用；
            // 此处位于 push_camera_state 之后，且 flush 后不再依赖被 flush 的几何数据。
            let mut gl = unsafe { get_internal_gl() };
            // 先冲刷场景已提交但未绘制的几何，保证叠加层绘制在场景之上。
            gl.flush();
            // gl.quad_gl.render_pass(None);
            // gl.quad_gl.viewport(None);
            // 叠加顺序：提示条 → 弹窗 → 全屏加载，与触摸优先级相反（加载遮罩吞输入、
            // 弹窗次之、场景最后），视觉上后画的永远盖在前面。
            BILLBOARD.with(|it| {
                let mut guard = it.borrow_mut();
                let t = guard.1.now() as f32;
                guard.0.render(&mut ui, t);
            });
            DIALOG.with(|it| {
                if let Some(dialog) = it.borrow_mut().as_mut() {
                    dialog.render(&mut ui, self.tm.now() as _);
                }
            });
            // FULL_LOADING 的「句柄存活即显示」策略：keep_alive 是内部保留的那一份，
            // strong_count > 1 说明外部仍有加载者在持有句柄，于是继续显示并返回 false；
            // 一旦降到 1，说明所有加载者都已释放，本帧返回 true 触发移除。
            let remove = FULL_LOADING.with(|it| {
                if let Some(loading) = it.borrow_mut().as_mut() {
                    if Arc::strong_count(&loading.keep_alive) > 1 {
                        if let Some(text) = loading.text.as_ref() {
                            ui.full_loading(text.clone(), self.tm.now() as _);
                        } else {
                            ui.full_loading_simple(self.tm.now() as _);
                        }
                        return false;
                    } else {
                        return true;
                    }
                }
                false
            });
            // 延迟到借用结束再移除：避免在持有 RefCell 借用时修改同一个槽导致 panic。
            if remove {
                FULL_LOADING.take();
            }
            pop_camera_state();
        }
        Ok(())
    }

    /// 暂停整个驱动器：置位暂停标记，并把暂停事件转发给栈顶场景。
    ///
    /// 暂停后 [`update`](Self::update)/[`render`](Self::render) 会直接返回（画面冻结），
    /// 因此需要场景自行处理音频等外部资源的暂停。
    pub fn pause(&mut self) -> Result<()> {
        self.paused = true;
        self.scenes.last_mut().unwrap().pause(&mut self.tm)
    }

    /// 恢复运行：清除暂停标记并把恢复事件转发给栈顶场景。
    ///
    /// 时间轴由场景在自己的 `resume` 中决定如何续接，避免引擎强行推进时间造成音符跳帧。
    pub fn resume(&mut self) -> Result<()> {
        self.paused = false;
        self.scenes.last_mut().unwrap().resume(&mut self.tm)
    }

    /// 当前是否处于暂停状态（宿主可据此决定是否继续投递帧驱动）。
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// 场景是否已请求退出游戏。宿主每帧检查，为 true 时执行实际的退出流程。
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }
}

// 绘制场景通用背景：按窗口宽高比裁剪填充，并在其上压一层 30% 黑，
// 保证前景文字无论背景图明暗都能保持可读性。
fn draw_background(tex: Texture2D) {
    let asp = screen_aspect();
    let top = 1. / asp;
    draw_image(tex, Rect::new(-1., -top, 2., top * 2.), ScaleType::CropCenter);
    draw_rectangle(-1., -top, 2., top * 2., Color::new(0., 0., 0., 0.3));
}

/// 场景本地异步任务的类型别名：持有一个未完成的 Future，完成后产出下一个场景意向。
///
/// 之所以用 `LocalTask`（非 Send 的 `Pin<Box<dyn Future>>`）而非 spawn 出去的任务，
/// 是因为加载任务只在游戏主线程上被轮询（见 `LoadingScene::update`），
/// 这样可以直接触碰非 Send 的图形资源。
pub type LocalSceneTask = LocalTask<Result<NextScene>>;
