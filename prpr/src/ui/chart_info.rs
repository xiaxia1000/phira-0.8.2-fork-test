//! 谱面元信息（`info.yml`）的编辑表单。
//!
//! 这不是展示组件，而是**编辑表单**：所有控件都直接双向绑定到
//! [`ChartInfoEdit::info`]（即 `info.yml` 的内存表示），用户一边输入一边就改掉了字段值。
//! [`ChartInfoEdit::to_patches`] 再把「改了什么」打包成一批待写入的文件提交给调用方。
//!
//! 之所以产出「补丁」而不是自己写文件：谱面的来源多种多样——可能是磁盘上的目录、
//! 可能是 zip 压缩包、也可能是编译进程序的内置资源，甚至可能是只读的安装目录。
//! 本模块刻意不关心存储细节，只负责「文件相对名 → 内容字节」的映射，
//! 由调用方决定是就地写盘、重新打包还是上传服务器。
//!
//! [`render_chart_info`] 负责把整个表单画出来，返回内容尺寸供外层容器
//! （通常是 [`super::Scroll`]）布局；表单里所有 `ui.input` / `slider` / `checkbox`
//! 的变更都会把 [`ChartInfoEdit::updated`] 置真，调用方据此决定是否需要保存。

prpr_l10n::tl_file!("chart_info");

use super::Ui;
use crate::{
    core::BOLD_FONT,
    ext::{open_url, parse_time},
    info::ChartInfo,
    scene::show_message,
    ui::InputParams,
};
use anyhow::Result;
use inputbox::InputMode;
use macroquad::math::Rect;
use std::{borrow::Cow, collections::HashMap};

/// `info.yml` 的编辑态。
///
/// 之所以要单独一个结构体而不是直接改 [`ChartInfo`]：需要额外记住「用户新选了哪些文件」
/// 以及「是否发生过任何修改」。字段命名与 `info.yml` 的键逐项对应，
/// 其中 `chart`/`music`/`illustration`/`unlock_video` 这四个 **路径** 字段
/// 与 `info` 里的**同名字段**含义不同，见各自的说明。
#[derive(Clone)]
pub struct ChartInfoEdit {
    /// 正在编辑的元信息本体，对应 `info.yml` 的全部键值。
    pub info: ChartInfo,
    /// 用户新选择的谱面文件**路径**（本地路径，尚未读取内容）。
    /// `None` 表示沿用 `info.chart` 指向的旧文件；
    /// 注意它与 `info.chart`（谱面在谱面包内的**相对文件名**）不是一回事。
    pub chart: Option<String>,
    /// 用户新选择的音频文件路径，语义同 `chart`。
    pub music: Option<String>,
    /// 用户新选择的曲绘文件路径，语义同 `chart`。
    pub illustration: Option<String>,
    /// 用户新选择的解锁动画文件路径，语义同 `chart`；仅在 `enable_unlock` 为真时生效。
    pub unlock_video: Option<String>,
    /// 是否启用「解锁动画」这一可选特性。
    ///
    /// 它在语义上等价于 `info.unlock_video.is_some()`，但必须单独存一个布尔：
    /// 关闭特性时要把 `info.unlock_video` 清成 `None`，此时「之前是否开启过」
    /// 这一信息就丢失了，无法据此恢复。
    pub enable_unlock: bool,
    /// 脏标记：任何控件发生过改动即为真，由调用方用来决定是否提示保存/上传。
    /// 一旦置真不会自动复位，保存成功后需要由调用方重建整个 `ChartInfoEdit`。
    pub updated: bool,
}

impl ChartInfoEdit {
    /// 基于现有元信息创建一个干净的编辑态：没有任何待写入的文件，`updated` 为假。
    ///
    /// `enable_unlock` 由 `info.unlock_video.is_some()` 推导，保证打开表单时
    /// 复选框状态与文件里的实际配置一致。
    pub fn new(info: ChartInfo) -> Self {
        let enable_unlock = info.unlock_video.is_some();
        Self {
            info,
            chart: None,
            music: None,
            illustration: None,
            unlock_video: None,
            enable_unlock,
            updated: false,
        }
    }

    /// 把所有待保存的内容打包成「相对文件名 → 文件字节」的补丁。
    ///
    /// 返回的 map 里**一定**含 `info.yml`（由 `self.info` 序列化得到），
    /// 之后是按需追加的媒体文件。
    ///
    /// 键用的是 `info.*` 中的相对文件名而不是用户选择的本地路径，
    /// 因为 `info.yml` 里记录的本来就是相对名，重命名文件后新旧名称要一起生效。
    /// 这就要求调用方在调用本函数之前已经把新名字写进 `info` 了。
    ///
    /// 只有被用户显式挑选过的文件（字段为 `Some`）才会进入补丁，
    /// 未改动的大文件因此不必重复读盘/上传。
    ///
    /// # Errors
    /// - `info.yml` 序列化失败；
    /// - 读取用户选择的本地文件失败（文件被移动、无权限等）。
    ///
    /// # Platform
    /// 读文件的分支被 `#[cfg(not(target_arch = "wasm32"))]` 排除在 wasm 之外：
    /// 浏览器里没有可供 `tokio::fs` 读取的本地路径，网页端的文件选择走的是
    /// 另一条直接拿字节的通道，因此 wasm 上这里只提交 `info.yml`。
    pub async fn to_patches(&self) -> Result<HashMap<String, Vec<u8>>> {
        let mut res = HashMap::new();
        res.insert("info.yml".to_owned(), serde_yaml::to_string(&self.info)?.into_bytes());
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(chart) = &self.chart {
                res.insert(self.info.chart.clone(), tokio::fs::read(chart).await?);
            }
            if let Some(music) = &self.music {
                res.insert(self.info.music.clone(), tokio::fs::read(music).await?);
            }
            if let Some(illustration) = &self.illustration {
                res.insert(self.info.illustration.clone(), tokio::fs::read(illustration).await?);
            }
            // 即便用户选过解锁动画，只要特性被关掉就不应写入文件；
            // `unwrap_or` 兜住 `info.unlock_video` 恰好为空的异常情况。
            if self.enable_unlock {
                if let Some(unlock) = &self.unlock_video {
                    res.insert(self.info.unlock_video.clone().unwrap_or("unlock.mp4".to_string()), tokio::fs::read(unlock).await?);
                }
            }
        }
        Ok(res)
    }
}

/// 把秒数格式化成 `HH:MM:SS.ss`，用于预览时间段输入框的回填。
///
/// 显示格式特意做成定长（`{:05.2}` 让秒部分始终是「两位整数 + 两位小数」），
/// 这样用户看到的一直是 `00:00:15.00` 这种形式，改动起来不用补前导零；
/// 输出会被 [`parse_time`] 重新解析，因此格式变更必须与解析器保持同步。
///
/// 小时与分钟用截断后的整数（`it`），秒直接用带小数的 `t % 60.`，
/// 以保留 10ms 精度供用户微调预览起点；由于秒 < 60 恒成立，
/// `{:05.2}` 的输出宽度稳定为 5 个字符。唯一的显示瑕疵是 `t` 的秒部分接近 60 时
/// 会被格式化四舍五入，理论上可能出现 `60.00` 这种读数。
fn format_time(t: f32) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let it = t as u32;
    write!(&mut s, "{:02}:{:02}:{:05.2}", it / 3600, (it / 60) % 60, t % 60.).unwrap();
    s
}

/// 绘制整张谱面信息编辑表单。
///
/// # Arguments
/// * `edit` — 编辑态，所有控件直接就地修改它；发生过改动时其 `updated` 会被置真
/// * `width` — 表单可用宽度（设计坐标），用于计算输入框长度与滑块长度
///
/// # Returns
/// `(宽度, 内容总高度)`。宽度原样回传只是为了符合 `Scroll::render` 闭包的
/// `(f32, f32)` 返回约定；真正有用的是高度 `sy`——调用方拿它作为滚动内容的尺寸。
///
/// 布局采用「标签列 + 控件列」的两列结构：`rt = 0.28` 是给右对齐的标签预留的列宽，
/// 输入控件都画在 `rt` 之后，因此标签长短不一的各行也能左右对齐。
/// 控件用 `ui.dy` 逐行向下排列，`dy!` 宏在每次下移时同步累加 `sy`，
/// 于是不必手工汇总总高度。
///
/// 校验规则集中在本函数里，任一项不合法都只弹错误提示、**不写入** `info`：
/// - 预览时间段：`起始 - 结束`（半角与全角连字符都接受），时长必须落在 [1s, 20s]；
/// - 偏移：必须是合法浮点数，单位是**秒**（`info.offset` 的语义单位；
///   自动偏移面板上以毫秒展示，1ms = 0.001s）；
/// - 宽高比：接受 `16:9`、`16：9`（全角冒号）或裸浮点数，要求有限且为正。
pub fn render_chart_info(ui: &mut Ui, edit: &mut ChartInfoEdit, width: f32) -> (f32, f32) {
    // sy 累计已用高度，最后作为内容高度返回；初值 0.02 是顶部留白。
    let mut sy = 0.02;
    ui.scope(|ui| {
        let s = 0.01;
        ui.dx(0.01);
        ui.dy(sy);
        // `dy!` 同时完成「下移光标」与「记账」：前者决定绘制位置，
        // 后者让函数末尾能一步拿到内容总高度，免得每个分支都手工维护累计值。
        macro_rules! dy {
            ($dy:expr) => {{
                let dy = $dy;
                sy += dy;
                ui.dy(dy);
            }};
        }
        // 阶段一：标题。BOLD_FONT + 0.9 字号，与 0.47 的字段标签拉开层级。
        dy!(0.01);
        let r = ui.text(tl!("edit-chart")).size(0.9).draw_using(&BOLD_FONT);
        dy!(r.h + 0.04);
        // rt 为标签列宽；控件列整体右移到标签列之后，长度取剩余宽度。
        let rt = 0.28;
        ui.dx(rt);
        let len = width - rt - 0.04;
        let info = &mut edit.info;
        // 阶段二：文本类元信息。`input` 的第三个参数是 `(长度, 脏标记)`，
        // 把「内容被改过」这件事直接写进 `edit.updated`，省掉逐项比较。
        let r = ui.input(tl!("chart-name"), &mut info.name, (len, &mut edit.updated));
        dy!(r.h + s);
        let r = ui.input(tl!("author"), &mut info.charter, (len, &mut edit.updated));
        dy!(r.h + s);
        let r = ui.input(tl!("composer"), &mut info.composer, (len, &mut edit.updated));
        dy!(r.h + s);
        let r = ui.input(tl!("illustrator"), &mut info.illustrator, (len, &mut edit.updated));
        dy!(r.h + s + 0.02);

        // 定级字符串（如 "IN Lv.15"）与 0.0-20.0 的难度数值是两个独立字段，
        // 前者是给人看的标签，后者才是判定用的数值。
        let r = ui.input(tl!("level-displayed"), &mut info.level, (len, &mut edit.updated));
        dy!(r.h + s);

        // 阶段三：难度滑块。范围 0.0..20.0、步长 0.1 覆盖了 Phigros 及其社区的定数区间。
        // `ui.slider` 直接写入目标值、不返回「是否改变」，所以只能先存旧值再比较；
        // 用 1e-4 作 epsilon 是为了滤掉浮点表示误差造成的假阳性改动。
        ui.dx(-rt);
        let last = info.difficulty;
        let r = ui.slider(tl!("diff"), 0.0..20.0, 0.1, &mut info.difficulty, Some(width - 0.2));
        if (info.difficulty - last).abs() > 1e-4 {
            edit.updated = true;
        }
        dy!(r.h + s + 0.01);
        ui.dx(rt);

        // 阶段四：预览时间段。输入框内容是一整段文本，需要自行解析回两个数值，
        // 因此用局部 `changed` 而不是把 `edit.updated` 直接交给 `input`。
        // 注意代码在解析之前就把它置真了，后面解析失败也不会撤销——
        // 即「输入过非法值」同样算作已修改，处理保守但略显粗糙。
        let mut string = format!("{} - {}", format_time(info.preview_start), format_time(info.preview_end.unwrap_or(info.preview_start + 15.)));
        let mut changed = false;
        let r = ui.input(tl!("preview-time"), &mut string, (len, &mut changed));
        dy!(r.h + s);
        if changed {
            edit.updated = true;
            // 用一个立即调用的闭包承载解析与校验，好处是可以用 `?` 就近短路，
            // 并把各种失败统一收敛成 `Cow<str>` 文案交给 `show_message`。
            match || -> Result<(f32, f32), Cow<'static, str>> {
                // 连字符同时接受半角 `-` 与全角 `—`：玩家多半是从别处复制时间段
                // 或用了中文输入法，不兼容会变成莫名其妙的「非法输入」。
                let (st, en) = string.split_once(['-', '—']).ok_or_else(|| tl!("illegal-input"))?;
                let st = parse_time(st.trim()).ok_or_else(|| tl!("invalid-time"))?;
                let en = parse_time(en.trim()).ok_or_else(|| tl!("invalid-time"))?;
                // 预览片段是循环播放的短片段：太短听不出旋律，太长会挤占完整试听的定位价值；
                // 1s / 20s 这两条界限与游戏内预览播放的约束保持一致。
                if st + 1. > en {
                    return Err(tl!("preview-too-short"));
                }
                if st + 20. < en {
                    return Err(tl!("preview-too-long"));
                }
                Ok((st as f32, en as f32))
            }() {
                Err(err) => {
                    // 校验失败时只提示、不回写：保留用户原来的合法值，
                    // 避免 `preview_end` 进入「有 start 无 end」的中间状态。
                    show_message(err).error();
                }
                Ok((st, en)) => {
                    info.preview_start = st;
                    info.preview_end = Some(en);
                }
            }
        }
        // 附注行：右对齐的 "ps" 小标记 + 左对齐的说明文字。
        // 它同样通过 `dy!` 计入高度，否则后面的控件会盖在说明文字上。
        dy!(ui.scope(|ui| {
            ui.text(tl!("ps")).anchor(1., 0.).size(0.35).draw();
            ui.text(tl!("preview-hint")).pos(0.02, 0.).size(0.35).max_width(len).multiline().draw().h + 0.03
        }));

        // 阶段五：音频偏移。单位是**秒**，`{:.3}` 恰好给出毫秒级分辨率；
        // 允许负值（表示音频相对谱面提前），因此这里不做正数校验。
        let mut string = format!("{:.3}", info.offset);
        let mut changed = false;
        let r = ui.input(tl!("offset"), &mut string, (len, &mut changed));
        dy!(r.h + s);
        if changed {
            edit.updated = true;
            match string.parse::<f32>() {
                Err(_) => {
                    show_message(tl!("illegal-input")).error();
                }
                Ok(value) => {
                    info.offset = value;
                }
            }
        }

        // 阶段六：画面宽高比。存的是单个浮点数，但允许用户按「宽:高」的习惯输入，
        // 因此解析支持三种写法，且分母为 0 时得到的 `inf` 会被下面的
        // `is_finite` 检查挡掉，不需要额外判零。
        let mut string = format!("{:.5}", info.aspect_ratio);
        let mut changed = false;
        let r = ui.input(tl!("aspect-ratio"), &mut string, (len, &mut changed));
        dy!(r.h + s);
        if changed {
            edit.updated = true;
            match || -> Result<f32> {
                if let Some((w, h)) = string.split_once([':', '：']) {
                    Ok(w.trim().parse::<f32>()? / h.trim().parse::<f32>()?)
                } else {
                    Ok(string.parse()?)
                }
            }() {
                Err(_) => {
                    show_message(tl!("illegal-input")).error();
                }
                Ok(value) => {
                    // 宽高比必须有限且为正：0 或负数会让后续的投影计算出现除零/翻转。
                    if value.is_finite() && value > 0.0 {
                        info.aspect_ratio = value;
                    } else {
                        show_message(tl!("illegal-input")).error();
                    }
                }
            }
        }
        dy!(ui.scope(|ui| {
            ui.text(tl!("ps")).anchor(1., 0.).size(0.35).draw();
            ui.text(tl!("aspect-hint")).pos(0.02, 0.).size(0.35).max_width(len).multiline().draw().h + 0.03
        }));

        ui.dx(0.01);
        // `force_aspect_ratio` 是布尔值，用普通 `checkbox` 即可，无需三态处理。
        let r = ui.checkbox(tl!("force-aspect-ratio"), &mut info.force_aspect_ratio);
        dy!(r.h + s);
        ui.dx(-0.01);

        // 阶段七：背景压暗滑块。范围 0.0..1.0、步长 0.05：0 表示不压暗（曲绘原样），
        // 1 表示完全压黑（只靠谱面线条发光），步长取 0.05 是因为更细的调整肉眼分辨不出。
        ui.dx(-rt);
        let last = info.background_dim;
        let r = ui.slider(tl!("dim"), 0.0..1.0, 0.05, &mut info.background_dim, Some(width - 0.2));
        if (info.background_dim - last).abs() > 1e-4 {
            edit.updated = true;
        }
        dy!(r.h + s + 0.01);
        ui.dx(rt);

        // 阶段八：两个「三态」选项。
        //
        // 这两个字段在 `info.yml` 里是可选的（`Option<bool>`），三种含义分别是：
        // `None` = 未设置、交给播放器按默认行为处理；`Some(true)` = 强制启用；
        // `Some(false)` = 强制关闭。普通 `checkbox` 只有两态、无法表达「未设置」，
        // 因此这里用按钮轮换三态：显示为 ✓ / x / 空。
        // 轮换顺序是 None → Some(true) → Some(false) → None，
        // 即第一次点击总是「启用」，符合直觉。
        let r = ui.text(tl!("rpe-170-speed")).size(0.47).anchor(1., 0.).draw();
        let r = Rect::new(0.02, r.y - 0.01, r.h + 0.02, r.h + 0.02);
        let check_str = match info.use_rpe_170_speed {
            Some(true) => "\u{2713}",
            Some(false) => "x",
            None => "",
        };
        if ui.button("rpespeed", r, check_str.to_string()) {
            // 用 `cycle()` 把定长数组首尾相接，`skip_while` 找到当前值后取下一个；
            // 因为当前值必然在 OPTIONS 里，`unwrap()` 不会失败。
            const OPTIONS: [Option<bool>; 3] = [None, Some(true), Some(false)];
            let next = OPTIONS.iter().cycle().skip_while(|&&x| x != info.use_rpe_170_speed).nth(1).unwrap();
            info.use_rpe_170_speed = *next;
            edit.updated = true;
        }
        dy!(r.h + s);

        let r = ui.text(tl!("attach-ui-fix")).size(0.47).anchor(1., 0.).draw();
        let r = Rect::new(0.02, r.y - 0.01, r.h + 0.02, r.h + 0.02);
        let check_str = match info.use_attach_ui_fix {
            Some(true) => "\u{2713}",
            Some(false) => "x",
            None => "",
        };
        if ui.button("attachui", r, check_str.to_string()) {
            const OPTIONS: [Option<bool>; 3] = [None, Some(true), Some(false)];
            let next = OPTIONS.iter().cycle().skip_while(|&&x| x != info.use_attach_ui_fix).nth(1).unwrap();
            info.use_attach_ui_fix = *next;
            edit.updated = true;
        }
        dy!(r.h + s);

        // 阶段九（仅非 wasm 平台）：本地文件选择。
        //
        // 整块被 `#[cfg(not(target_arch = "wasm32"))]` 排除在网页端之外：
        // 浏览器里不存在「本地路径」这一概念，谱面包的更新走上传接口而非读盘，
        // 所以网页端根本不显示这些按钮，`ChartInfoEdit` 的四个路径字段恒为 `None`。
        #[cfg(not(target_arch = "wasm32"))]
        {
            use crate::scene::{request_file, return_file, take_file};

            // 解锁动画的开关。关闭时要把 `info.unlock_video` 一起清空，
            // 否则 `info.yml` 里会残留一个指向不存在文件的键；
            // 开启时预填默认文件名并同时写进 pending 路径槽，
            // 这样即使用户不重新选文件，也会按默认名去读「unlock.mp4」。
            let r = ui.text(tl!("enable-unlock")).size(0.47).anchor(1., 0.).draw();
            let r = Rect::new(0.02, r.y - 0.01, r.h + 0.02, r.h + 0.02);
            let check_str = if edit.enable_unlock { "\u{2713}" } else { "" };
            if ui.button("unlockchk", r, check_str.to_string()) {
                if edit.enable_unlock {
                    info.unlock_video = None;
                    edit.enable_unlock = false;
                } else {
                    info.unlock_video = Some("unlock.mp4".to_string());
                    edit.unlock_video = Some("unlock.mp4".to_string());
                    edit.enable_unlock = true;
                }
                edit.updated = true;
            }
            dy!(r.h + s);

            // 文件选择走「请求-应答」异步通道：`request_file` 只登记一个请求并唤起
            // 系统文件对话框，真正的结果稍后由 `take_file` 取回，因此本帧点击、后续帧才见效。
            // `id` 既作为按钮标识也作为请求标识，必须在全局范围内唯一。
            // 按钮上直接显示当前文件名，用户不用点开就能看到当前配置。
            let mut choose_file = |id: &str, label: Cow<'static, str>, value: &str| {
                let r = ui.text(label).size(0.47).anchor(1., 0.).draw();
                let r = Rect::new(0.02, r.y - 0.01, len, r.h + 0.02);
                if ui.button(id, r, value) {
                    request_file(id);
                }
                dy!(r.h + s);
            };

            choose_file("chart", tl!("chart-file"), &info.chart);
            choose_file("music", tl!("music-file"), &info.music);
            choose_file("illustration", tl!("illu-file"), &info.illustration);
            choose_file("unlock", tl!("unlock-file"), info.unlock_video.as_deref().unwrap_or("Disabled"));

            // 取回文件选择结果。可能同时有多个地方在等文件（例如曲绘选择器），
            // 因此拿到不是自己 id 的结果时必须用 `return_file` 放回队列，
            // 否则那个结果会永久丢失。
            if let Some((id, file)) = take_file() {
                match id.as_str() {
                    "chart" => {
                        // 只记录**路径**，真正的读取发生在 `to_patches` 里，
                        // 这样用户连续换几次文件也不会浪费 IO。
                        edit.chart = Some(file);
                        edit.updated = true;
                    }
                    "music" => {
                        edit.music = Some(file);
                        edit.updated = true;
                    }
                    "illustration" => {
                        edit.illustration = Some(file);
                        edit.updated = true;
                    }
                    "unlock" => {
                        // 特性被关掉时忽略选中的文件：此时该字段不参与提交，
                        // 收下反而会让用户在重新开启后拿到一个意料之外的旧文件。
                        if edit.enable_unlock {
                            edit.unlock_video = Some(file);
                            edit.updated = true;
                        }
                    }
                    _ => return_file(id, file),
                }
            }
        }

        // 阶段十：简介与提示。
        // `tip` 在 `info` 里是 `Option<String>`（不写该键表示无提示），
        // 而 `ui.input` 只能编辑 `String`，因此借一个临时串中转：
        // 输入为空时回写 `None`，避免 `info.yml` 里留下一个空的 `tip:` 键。
        let mut string = info.tip.clone().unwrap_or_default();
        let r = ui.input(tl!("tip"), &mut string, (len, &mut edit.updated));

        dy!(r.h + s);
        info.tip = if string.is_empty() { None } else { Some(string) };

        // 注意：这里的返回值没有绑定到 `r`，所以下面两处用到的 `r` 仍是上面 tip 输入框的矩形。
        // 影响有两方面：`dy!` 用的是 tip 的高度（两个单行输入框高度相同，效果等价），
        // 而下面的 `ir` 也落在 tip 那一行——若原意是让按钮占满 intro 行，
        // 这里应当写成 `let r = ui.input(...)`。仅作记录，未改动代码。
        ui.input(
            tl!("intro"),
            &mut info.intro,
            InputParams {
                changed: Some(&mut edit.updated),
                mode: InputMode::Multiline,
                length: len,
            },
        );
        dy!(r.h + s);

        // 用整行宽度的按钮充当「帮助链接」入口：文档在网页上，
        // 这里只负责唤起系统浏览器。`open_url` 的失败（例如没有默认浏览器）
        // 被 `.ok()` 吞掉——打开文档失败不应该中断用户编辑谱面信息。
        let ir = Rect::new(0., r.y, r.right(), r.h);
        if ui.button("collab", ir, tl!("how-to-add-collaborator").to_string()) {
            open_url("https://teamflos.github.io/phira-docs/chart-management/collaborator.html").ok();
        }
        dy!(r.h + s);

        // 与外层开头的 `ui.dx(0.01)` 配套；不过 `ui.scope` 退出时会整体恢复
        // transform，因此这一行对调用方而言并不产生实际效果。
        ui.dx(-0.02);
    });
    // 宽度原样返回（符合 `Scroll::render` 闭包的返回约定），高度为累计的 sy。
    (width, sy)
}
