//! UML 运行时与元素渲染。
//!
//! UML 是 Phira 的**服务端下发 UI 描述语言**（活动页专用）：活动页的排版、文案与
//! 交互随运营节奏频繁变化，若把界面写死在客户端就得跟着发版，所以服务端下发一段
//! 脚本，客户端解析成元素树后在场景里渲染并处理交互。这也决定了脚本属于**外部输入**，
//! 失败应当只降级（报错、少画一部分）而不该让整页崩掉；当前实现里仍有个别未兜住的
//! 路径（见 `parse::take_expr` 的 `panic!` 与 [`Uml::init`] 的 `unwrap`）。
//!
//! 模块分工：
//! - `lexer` / `parse`：把脚本文本变成元素树与表达式树，入口是 [`parse_uml`]；
//! - 本文件：定义元素 trait [`Element`] 与各元素类型、变量系统 [`Var`]、
//!   以及把它们驱动起来的运行时 [`Uml`]。
//!
//! 渲染模型是「按序执行 + 作用域变换」：脚本顶层是一个语句序列，逐条执行；
//! `#>rot`/`#>tr`/`#>alpha`/`#>mat` 这些变换类元素在渲染到自身时把变换压栈，
//! 从而影响**其后**的所有元素，`#>pop` 负责出栈。元素之间没有显式的父子关系，
//! 「作用域」完全由序列中的位置与压栈/出栈时机决定。
//!
//! 与 `EventScene` 的衔接：`EventScene` 负责拉取脚本、调用 [`parse_uml`] 建立
//! [`Uml`]，并在每帧依次调用 [`Uml::touch`]（转发触摸）、[`Uml::render`]（绘制并
//! 回收变量绑定）、[`Uml::render_top`]（绘制需要在最上层的元素）、
//! [`Uml::on_result`]（把子界面的选择结果回传元素）与 [`Uml::next_scene`]
//! （收集元素提出的场景切换请求）。

mod lexer;
mod parse;

pub use parse::parse_uml;

use self::parse::{constant, ButtonState, TopLevel};
use crate::{
    charts_view::{ChartDisplayItem, ChartsView},
    client::{recv_raw, Client, File},
    icons::Icons,
};
use anyhow::{anyhow, bail, Result};
use image::DynamicImage;
use macroquad::prelude::*;
use nalgebra::Vector2;
use parse::Expr;
use prpr::{
    core::Matrix,
    ext::{semi_black, semi_white, RectExt, SafeTexture, ScaleType},
    scene::NextScene,
    task::Task,
    ui::{RectButton, Ui},
};
use serde::Deserialize;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fmt::Debug,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use tracing::warn;

/// 颜色的包装类型。存在的唯一原因是给 [`Color`] 挂上自定义反序列化：
/// macroquad 的 `Color` 是外部类型，无法直接实现 `Deserialize`，而 UML 的属性里
/// 需要支持多种人类可读的颜色写法，因此用 newtype 承接转换。
#[derive(Debug)]
struct WrappedColor(Color);
// 默认颜色取白色。属性缺省时元素应当仍然可见，而不是因为 alpha 为 0 凭空消失。
impl Default for WrappedColor {
    fn default() -> Self {
        Self(WHITE)
    }
}

// 颜色的「宽容解析」反序列化。
// 活动作者不该被迫写 `[r, g, b, a]` 数组，因此这里接受若干常见写法，
// 这也是它无法用 derive 生成的原因。
impl<'de> Deserialize<'de> for WrappedColor {
    /// 依次尝试四种写法：命名色（`white`/`black`/`red`/`blue`/`yellow`/`green`/`gray`）、
    /// `#` 前缀的十六进制、`w<alpha>` 半透明白、`b<alpha>` 半透明黑；都不匹配则报错。
    ///
    /// 十六进制分支先把整串解析成一个 `u32` 再按大端拆成字节：6 位时最高字节被强制
    /// 补成 `0xff`（此时没有独立的 alpha 可用，取不透明最合理），8 位时最高字节即
    /// alpha——即 8 位写法的字节序是 `#AARRGGBB`，而不是常见的 `#RRGGBBAA`。
    /// 该实现只接受字符串形式，写成 JSON 数组不会被识别。
    ///
    /// # Errors
    /// 十六进制解析失败、半透明数值非法、或前缀不认识时返回错误，
    /// 错误信息带上原始字符串以便定位到具体属性。
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let s = String::deserialize(deserializer)?;
        Ok(WrappedColor(match s.as_str() {
            "white" => WHITE,
            "black" => BLACK,
            "red" => RED,
            "blue" => BLUE,
            "yellow" => YELLOW,
            "green" => GREEN,
            "gray" => GRAY,
            _ => {
                if let Some(d) = s.strip_prefix('#') {
                    let int = u32::from_str_radix(d, 16).map_err(D::Error::custom)?;
                    let mut v = int.to_be_bytes();
                    if d.len() == 6 {
                        v[0] = 0xff;
                    }
                    Color::from_rgba(v[1], v[2], v[3], v[0])
                } else if let Some(d) = s.strip_prefix('w') {
                    semi_white(d.parse().map_err(D::Error::custom)?)
                } else if let Some(d) = s.strip_prefix('b') {
                    semi_black(d.parse().map_err(D::Error::custom)?)
                } else {
                    return Err(D::Error::custom(format!("invalid color: {s}")));
                }
            }
        }))
    }
}

/// UML 元素的统一接口。各个方法对应渲染流水线的不同阶段，由 [`Uml`] 按固定顺序驱动：
/// - [`Element::render`]：**每帧主体**。绘制自身并返回一个值——返回值不是装饰性的，
///   它会经 `let`/`id` 机制写入变量表，供后续元素的表达式读取。
/// - [`Element::render_top`]：在同一帧所有 `render` 结束后调用，用于绘制必须压在
///   其他内容之上的部分（如谱面集合的滚动条与弹窗）。默认空实现。
/// - [`Element::touch`]：触摸分发时调用，返回值表示**是否消费该事件**——返回 `true`
///   会立即中断本轮分发，因此元素在脚本中的次序同时决定了「遮挡」关系（靠前者优先）。
///   同时可通过 `action` 向外抛出一个动作名。
/// - [`Element::on_result`]：子界面（谱面集合的选谱浮层）结束选择时回调，
///   `delete` 区分「提交选择」与「取消」。
/// - [`Element::next_scene`]：提出场景切换请求。`Uml` 按序询问并取第一个非 `None`
///   的结果，相当于向上冒泡；元素只能提出请求，是否真的切换由场景决定。
/// - [`Element::id`]：元素在变量表中的绑定名，`None` 表示不绑定。
///
/// 只有 `id` 与 `render` 没有默认实现：前者决定绑定语义，后者是元素存在的意义；
/// 其余方法对大多数元素而言无事可做，给默认实现可以避免为每个元素写空方法。
pub trait Element {
    /// 返回本元素的绑定名（脚本里 `id` 属性的值），后续表达式可直接用它引用本元素
    /// 渲染出的值；返回 `None` 表示该元素不参与变量绑定。
    fn id(&self) -> Option<&str>;
    /// 子界面选择结束时的回调。`t` 是当前时间（供动画计时用），
    /// `delete` 为真表示用户取消了选择而不是确认。默认忽略。
    fn on_result(&self, _t: f32, _delete: bool) {}
    /// 处理一次触摸，返回是否**已消费**该事件。
    /// 返回 `true` 会让 `Uml` 停止向后续元素分发（先声明的元素因此「盖住」后面的元素）；
    /// 只有按钮类元素会写入 `action`。默认不消费任何事件。
    fn touch(&self, _touch: &Touch, _uml: &Uml, _action: &mut Option<String>) -> Result<bool> {
        Ok(false)
    }
    /// 渲染本元素并返回它对外暴露的值，供变量绑定与条件判断使用。
    ///
    /// # Errors
    /// 属性表达式求值失败（变量未定义、类型不匹配等）时返回错误。错误会经
    /// [`Uml::render`] 的 `?` 直接中断本帧剩余元素的渲染，由 `EventScene` 打印并把
    /// 内容高度记 0；下一帧仍会重试，因此单条属性写错表现为「页面从该处截断并逐帧报错」。
    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var>;
    /// 在同一帧所有元素的 `render` 之后调用，用于绘制需要置顶的内容。默认什么都不做。
    fn render_top(&self, _ui: &mut Ui, _uml: &Uml) -> Result<()> {
        Ok(())
    }
    /// 返回本元素请求切换到的场景，`None` 表示没有请求。默认没有。
    fn next_scene(&self) -> Option<NextScene> {
        None
    }
}

/// `p` 元素的属性配置。
///
/// 容器级 `serde(default)` 表示「所有属性都可省略」，缺省值统一由 [`TextConfig::default`]
/// 提供——它是本元素默认值的唯一来源，改默认值只需要改那里。
/// `rename_all = "camelCase"` 是各配置类型的统一约定；本类型的字段恰好都是
/// 单段小写名，因此该约定在这里等价于空操作，它主要是为多词属性预留的。
#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TextConfig {
    /// 变量绑定名。渲染后可用它读取本段文字的包围盒，如 `title.w`。
    /// 另有两个特殊值会被 `render` 当作字体选择开关：`pgr` 用 PGR 字体、
    /// `bold` 用粗体字体——即这两个 id 同时承担「绑定变量」与「选字体」两种作用。
    id: Option<String>,
    /// 字号（逻辑单位，与 UI 其余部分的尺度一致）。
    size: Expr,
    /// `pos` 的横坐标（局部归一化坐标）。注意它并不一定是文字左上角，
    /// 具体含义由 `ax`/`ay` 决定。
    x: Expr,
    /// `pos` 的纵坐标。
    y: Expr,
    /// 锚点横向比例：0 表示 `x` 对应对齐文字左边界，1 表示右边界。
    ax: Expr,
    /// 锚点纵向比例：0 表示 `y` 对应对齐文字顶部（配合 `bl` 语义），1 表示底部。
    ay: Expr,
    /// 是否允许换行。关闭时文本必须在一行内放下，否则超宽会被省略处理。
    ml: bool,
    /// 多行模式下的最大宽度（`None` 表示不限）。超过该宽度时换行或截断。
    mw: Option<Expr>,
    /// 是否启用基线对齐（默认 true）：为真时 `y` 表示文字**基线**的位置，
    /// 为假时改用 `no_baseline`，让 `y` 表示行顶——做盒子对齐时后者更直观。
    bl: bool,
    /// 文字颜色，支持多种写法（见 [`WrappedColor`] 的反序列化）。
    c: WrappedColor,
}

// 各项属性的默认值：锚点 (0, 0)、坐标 (0, 0)、字号 1、白色、单行、启用基线对齐。
// 用 `constant(..)` 而不是 `Expr` 的其它构造，是因为属性默认值不依赖任何变量，
// 逐帧求值结果恒定——这同时保证了渲染代码可以无条件调用 `eval`。
impl Default for TextConfig {
    fn default() -> Self {
        Self {
            id: None,
            size: constant(1.0),
            x: constant(0.),
            y: constant(0.),
            ax: constant(0.),
            ay: constant(0.),
            ml: false,
            mw: None,
            bl: true,
            c: WrappedColor::default(),
        }
    }
}

/// `p` 元素：一段文本。
/// 正文单独作为字段而不是塞进 [`TextConfig`]，是因为两者来自不同的语法位置：
/// 属性来自 `(...)` 属性块，正文来自紧随其后的 `{...}` 文本块；分开存放正好对应
/// 解析时 `take_config` 与 `take_text` 的先后调用。
#[derive(Debug)]
pub struct Text {
    /// 属性快照，仅在解析时填充一次，逐帧渲染只读。
    config: TextConfig,
    /// 文本正文（已由 `text_block` 做过 `}}` 转义还原与缩进归一化）。
    /// 它不参与表达式求值，因此不需要每帧重新解析。
    text: String,
}

// 构造：属性与正文都由解析器传入，元素本身不做校验（校验发生在解析期）。
impl Text {
    /// 用已解析好的属性与正文建立文本元素。
    pub fn new(config: TextConfig, text: String) -> Self {
        Self { config, text }
    }
}

// `p` 元素在渲染流水线中的职责：只参与 `render`（绘制并回传包围盒），
// 不处理触摸、不置顶、不请求切换场景，因此其余方法沿用 trait 默认实现。
impl Element for Text {
    fn id(&self) -> Option<&str> {
        self.config.id.as_deref()
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let c = &self.config;
        // 基础样式：位置、锚点、字号、颜色。每个属性都是一次独立的表达式求值，
        // 因此作者可以把它们写成随 `t` 变化的算式来做出动画。
        let mut text = ui
            .text(&self.text)
            .pos(c.x.eval(uml)?.float()?, c.y.eval(uml)?.float()?)
            .anchor(c.ax.eval(uml)?.float()?, c.ay.eval(uml)?.float()?)
            .size(c.size.eval(uml)?.float()?)
            .color(c.c.0);
        // 可选修饰，按需叠加：多行 → 最大宽度 → 关闭基线对齐。
        if c.ml {
            text = text.multiline();
        }
        if let Some(w) = &c.mw {
            text = text.max_width(w.eval(uml)?.float()?);
        }
        if !c.bl {
            text = text.no_baseline();
        }
        // 字体选择借用 `id` 字段：`pgr`/`bold` 是本元素约定的特殊 id，
        // 分别走 PGR 字体与粗体字体，其余一律用默认字体。
        // 返回值是实际绘制出的矩形，供 `id` 绑定与父层计算内容尺寸使用。
        Ok(Var::Rect(match c.id.as_deref() {
            Some("pgr") => prpr::core::PGR_FONT.with(|it| text.draw_with_font(it.borrow_mut().as_mut())),
            Some("bold") => prpr::core::BOLD_FONT.with(|it| text.draw_with_font(it.borrow_mut().as_mut())),
            _ => text.draw(),
        }))
    }
}

/// `img` 元素的属性配置。
///
/// 与 [`TextConfig`] 不同，这里**没有**容器级 `serde(default)`：只有标了
/// `#[serde(default)]` 的字段可省略，`url` 与 `r` 是必填的。这是刻意的——
/// 图片没有合理的默认地址，缺 `r` 也无从决定画到哪里，与其静默画错不如解析期报错。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfig {
    /// 变量绑定名，渲染后可用它读取图片占用的矩形（常用于在其上叠加内容）。
    #[serde(default)]
    id: Option<String>,
    /// 图片文件引用。`File` 支持服务端资源路径等多种来源，由客户端按类型解析下载。
    url: File,
    /// 图片绘制到的目标矩形，同时也是本元素回传的值。
    r: Expr,
    /// 叠加色调，默认白色（即原样显示，不做染色）。
    #[serde(default)]
    c: WrappedColor,
    /// 缩放方式，默认等比填满并居中裁切（`CropCenter`），适合铺满背景的曲绘。
    #[serde(default)]
    t: ScaleType,
}

/// `img` 元素：一张图片。
/// 图片需要异步下载，所以元素自带一个「加载任务 + 结果纹理」的双层状态：
/// 任务只被消费一次，纹理则长期保留供逐帧绘制使用。
pub struct Image {
    /// 属性快照，逐帧渲染只读。
    config: ImageConfig,
    /// 尚未完成的加载任务；`None` 表示任务已经取走（无论成功或失败）。
    /// 用 `RefCell` 是因为 `render` 只拿到 `&self`，却需要推进并清空这个状态。
    task: RefCell<Option<Task<Result<DynamicImage>>>>,
    /// 加载完成后的纹理；仍为 `None` 表示加载中或加载失败，此时不绘制任何内容。
    tex: RefCell<Option<SafeTexture>>,
}

// 构造即发起异步加载：脚本解析不会被网络/磁盘 IO 阻塞，页面先以文本与布局出现，
// 图片就绪后才显示。加载失败只记日志（见 `render`），不会使整页报错。
impl Image {
    /// 用已解析的属性建立图片元素，并立即开始异步加载 `url`。
    pub fn new(config: ImageConfig) -> Self {
        let url = config.url.clone();
        Self {
            config,
            task: RefCell::new(Some(Task::new(async move { url.load_image().await }))),
            tex: RefCell::new(None),
        }
    }
}

// `img` 元素在渲染流水线中的职责：每帧先尝试收割加载结果，再按当前纹理绘制；
// 不处理触摸、不置顶、不请求切换场景。
impl Element for Image {
    fn id(&self) -> Option<&str> {
        self.config.id.as_deref()
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let c = &self.config;
        // 阶段一：收割异步加载结果。`task.take()` 取出一次性的结果，
        // 成功则转换为纹理，失败只 warn（外部资源不可用不应让活动页崩溃）。
        let mut guard = self.task.borrow_mut();
        if let Some(task) = guard.as_mut() {
            if let Some(res) = task.take() {
                match res {
                    Ok(val) => *self.tex.borrow_mut() = Some(val.into()),
                    Err(err) => {
                        warn!(url = c.url.url, ?err, "failed to load image");
                    }
                }
                // 必须先释放对 `task` 的借用再写回：两处借用的是同一个 `RefCell`，
                // 不 `drop` 就会触发重复可变借用的 panic。
                drop(guard);
                *self.task.borrow_mut() = None;
            }
        }
        // 阶段二：求值目标矩形。即使图片还没加载完也要算出来，
        // 因为下面的返回值会被 `id` 绑定，后面的元素可能依赖它做布局。
        let r = c.r.eval(uml)?.rect()?;
        // 阶段三：仅在纹理就绪时绘制；参数含义为（纹理, 目标矩形, 缩放方式, 色调）。
        if let Some(tex) = self.tex.borrow().as_ref() {
            ui.fill_rect(r, (**tex, r, c.t, c.c.0));
        }
        Ok(Var::Rect(r))
    }
}

/// 一个「只接受字符串形式整数」的包装类型，用于 `cid`/`rn` 这类整型属性。
///
/// 必须自定义反序列化的原因在 `take_config` 里：它会把所有非引号、非布尔的属性值
/// 先解析成表达式再 `to_string()`，于是脚本里的 `cid: 123` 到达这里是字符串 `"123"`
/// 而不是 JSON 数字。若用 `i32` 的默认实现会因类型不符直接报错。
#[derive(Debug, Clone, Copy)]
struct I32(i32);
// 按字符串取回再 parse，与 `take_config` 的值序列化方式配套。
impl<'de> Deserialize<'de> for I32 {
    /// 把属性值当作十进制整数字符串解析。
    ///
    /// # Errors
    /// 属性值不是数字（例如写成表达式 `t + 1`）时返回解析错误。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        Ok(Self(String::deserialize(deserializer)?.parse().map_err(D::Error::custom)?))
    }
}

/// `col` 元素每行显示多少个谱面格子时的默认值。
/// 取 4 是列表页的常见排版；活动页通常希望一行多列以便横向铺开。
fn default_row_num() -> I32 {
    I32(4)
}

/// `col` 元素单行高度的默认值（归一化单位）。
fn default_chart_height() -> Expr {
    constant(0.3)
}

/// `col`（谱面集合）元素的属性配置。
/// 它把主界面那套谱面列表（[`ChartsView`]）嵌进活动页，因此属性也围绕「显示哪些谱面、
/// 怎么排版」展开。`cid` 与 `r` 必填，其余可省略。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionConfig {
    /// 变量绑定名。注意 `render` 回传的是 `Var::Float(0.)`，因此绑定它没有实际意义，
    /// 这里的 `id` 只是为了统一元素写法。
    #[serde(default)]
    id: Option<String>,
    /// 要展示的服务端谱面集合 id。
    cid: I32,
    /// 每行显示的谱面数量。
    #[serde(default = "default_row_num")]
    rn: I32,
    /// 单个谱面格子的高度。
    #[serde(default = "default_chart_height")]
    rh: Expr,
    /// 列表占用的矩形区域（必填），滚动与命中检测都基于它。
    r: Expr,
}

/// `col` 元素的内部可变状态。
/// 与 [`Image`] 类似，这里也把「一次性异步任务」与「长期保留的界面状态」分开存放。
struct CollectionState {
    /// 拉取集合数据的任务；`None` 表示已结束（成功或失败）。
    task: Option<Task<Result<crate::client::Collection>>>,
    /// 复用的谱面列表控件，持有滚动位置、选中项等全部界面状态。
    charts_view: ChartsView,
}

/// `col` 元素：内嵌一段可滚动、可点选的谱面列表。
/// 复用 [`ChartsView`] 而不是重写一套，可以保证活动页里的谱面条目在视觉与交互上
/// 与主界面完全一致。
pub struct Collection {
    /// 属性快照，逐帧渲染只读。
    config: CollectionConfig,
    /// 加载任务与列表控件；用 `RefCell` 因为渲染与触摸都只拿到 `&self`。
    state: RefCell<CollectionState>,
}

// 构造即请求 `/collection/{cid}`，并把列表控件配置成活动页想要的形态：
// 行数取自脚本、禁止多选——活动页里选谱的目的是触发一个动作，
// 多选会让「选完之后做什么」变得不明确。
impl Collection {
    /// 用已解析的属性建立谱面集合元素，并立即开始拉取集合数据。
    pub fn new(icons: Arc<Icons>, rank_icons: [SafeTexture; 8], config: CollectionConfig) -> Self {
        let cid = config.cid;
        let mut charts_view = ChartsView::new(icons, rank_icons);
        charts_view.row_num = config.rn.0 as _;
        charts_view.allow_multi_select = false;
        Self {
            config,
            state: RefCell::new(CollectionState {
                task: Some(Task::new(async move { Ok(recv_raw(Client::get(format!("/collection/{}", cid.0))).await?.json().await?) })),
                charts_view,
            }),
        }
    }
}

// `col` 元素在渲染流水线中的职责最完整：它同时参与触摸分发、主渲染、置顶渲染、
// 结果回调与场景切换——因为内嵌列表本身就是一个带滚动、浮层与跳转的完整子界面。
impl Element for Collection {
    fn id(&self) -> Option<&str> {
        self.config.id.as_deref()
    }

    // 选谱浮层结束（确认或取消）时把结果转交给列表控件。
    fn on_result(&self, t: f32, delete: bool) {
        self.state.borrow_mut().charts_view.on_result(t, delete)
    }

    // 触摸交给列表控件处理；把 `uml.t`/`uml.rt` 一并传入，是为让滚动惯性、
    // 回弹等动画与页面时钟保持一致。返回值即列表是否消费了本次触摸。
    fn touch(&self, touch: &Touch, uml: &Uml, _action: &mut Option<String>) -> Result<bool> {
        self.state.borrow_mut().charts_view.touch(touch, uml.t, uml.rt)
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let mut state = self.state.borrow_mut();
        // 阶段一：收割集合数据。失败只 warn，页面仍显示空列表而不是整体报错。
        if let Some(task) = &mut state.task {
            if let Some(res) = task.take() {
                match res {
                    Err(err) => {
                        warn!(?err, "failed to fetch collection");
                    }
                    Ok(col) => {
                        state
                            .charts_view
                            .set(uml.t, col.charts.iter().map(ChartDisplayItem::from_remote).collect());
                    }
                }
                state.task = None;
            }
        }

        // 阶段二：求值列表区域与行高，更新并渲染列表。
        // 行高是逐帧求值的，因此作者可以用表达式让列表随交互缩放。
        let c = &self.config;
        let t = uml.t;
        let r = c.r.eval(uml)?.rect()?;

        state.charts_view.row_height = self.config.rh.eval(uml)?.float()?;
        state.charts_view.update(t)?;
        state.charts_view.render(ui, r, t);

        // 返回常量 0：该元素对外不提供有意义的可绑定值。
        Ok(Var::Float(0.))
    }

    // 列表的滚动条与选谱浮层必须压在后续元素之上，因此要放到置顶阶段绘制。
    fn render_top(&self, ui: &mut Ui, uml: &Uml) -> Result<()> {
        self.state.borrow_mut().charts_view.render_top(ui, uml.t);
        Ok(())
    }

    // 选中谱面后需要切到其它场景（如进入谱面详情/开始游戏），
    // 由列表控件提出请求，再由 `Uml::next_scene` 冒泡给 `EventScene`。
    fn next_scene(&self) -> Option<NextScene> {
        self.state.borrow_mut().charts_view.next_scene()
    }
}

/// `r`（矩形）元素圆角半径的默认值：0，即直角矩形。
fn default_radius() -> Expr {
    constant(0.)
}

/// `r` 元素的属性配置。矩形是最常用的布局/背景元素，属性刻意保持最少。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RectConfig {
    /// 变量绑定名；渲染后可用它读取矩形区域，在其上继续排布子内容。
    #[serde(default)]
    id: Option<String>,
    /// 矩形区域（必填）。它同时决定了填充范围与回传的 [`Var::Rect`]。
    r: Expr,
    /// 填充色，默认白色。
    #[serde(default)]
    c: WrappedColor,
    /// 圆角半径，默认 0（直角）。单位与 `r` 的坐标一致。
    #[serde(default = "default_radius")]
    rad: Expr,
}

/// `r` 元素：一个（可圆角的）填充矩形。
pub struct RectElement {
    /// 属性快照，逐帧渲染只读；本元素没有需要跨帧保留的额外状态。
    config: RectConfig,
}

// 无状态的构造，仅保存属性。
impl RectElement {
    /// 用已解析的属性建立矩形元素。
    pub fn new(config: RectConfig) -> Self {
        Self { config }
    }
}

// `r` 元素在渲染流水线中的职责：只参与 `render`（绘制并回传矩形），无交互。
impl Element for RectElement {
    fn id(&self) -> Option<&str> {
        self.config.id.as_deref()
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let c = &self.config;
        let r = c.r.eval(uml)?.rect()?;
        let rad = c.rad.eval(uml)?.float()?;
        // 圆角极小时走更廉价的轴对齐填充：半径趋近 0 时构造出的圆角路径会退化，
        // 既没必要也容易在几何库中产生退化三角形，因此用一个极小阈值分流。
        if rad > 1e-5 {
            ui.fill_path(&r.rounded(rad), c.c.0);
        } else {
            ui.fill_rect(r, c.c.0);
        }
        Ok(Var::Rect(r))
    }
}

/// `btn` 元素的属性配置。
/// 按钮只负责「命中区域 + 动作」两件事，外观完全交给作者用其它元素绘制，
/// 因此这里没有颜色、文字之类的视觉属性。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonConfig {
    /// 变量绑定名。绑定后可用「按钮状态」的字段读取它的点击时间/次数/按住状态，
    /// 例如 `<id>.cnt` 在点击后自增，据此可以做点击反馈动画。
    /// 绑定名同时也是 `@btn` 之外访问按钮状态的唯一途径。
    #[serde(default)]
    id: Option<String>,
    /// 命中区域（必填）。由于命中检测会乘上当前 UI 变换，被 `#>rot` 旋转过的
    /// 按钮依然能得到正确的（倾斜的）命中区域。
    r: Expr,
    /// 点击完成后向外抛出的动作名；`None` 表示「有交互但不产生动作」，
    /// 场景收到 `None` 会当作没有动作处理。
    action: Option<String>,
}

/// `btn` 元素：一个只有命中区域、不自带外观的按钮。
/// 视觉由作者在其上叠放矩形/文本/图片来完成，按钮本身只维护触摸状态与计数。
pub struct ButtonElement {
    /// 属性快照，逐帧渲染只读。
    config: ButtonConfig,
    /// 复用的按钮命中控件（矩形 → 屏幕四边形的投影与按下/移出判定）。
    /// 用 `RefCell` 因为 `touch`/`render` 都只拿到 `&self`。
    btn: RefCell<RectButton>,
    /// 最近一次点击成功的时间；初始为 -1.0 表示「从未点击」。
    /// 用 `Cell` 是因为它是单线程内的小标量状态，无需原子或锁。
    last_touched: Cell<f32>,
    /// 累计点击次数，只增不减。
    count: AtomicU32,
}

// 初始状态即「从未被点击」：区域未绑定、时间为 -1、计数为 0。
impl ButtonElement {
    /// 用已解析的属性建立按钮元素。命中区域要等第一次 `render` 才会被投影出来，
    /// 因此在此之前按钮不会被任何触摸命中。
    pub fn new(config: ButtonConfig) -> Self {
        Self {
            config,
            btn: RefCell::default(),
            last_touched: Cell::new(-1.),
            count: AtomicU32::new(0),
        }
    }
}

// `btn` 元素在渲染流水线中的职责：`touch` 判定点击并抛出动作、更新内部状态；
// `render` 只负责把自身状态回传给脚本，不绘制任何内容。
impl Element for ButtonElement {
    fn id(&self) -> Option<&str> {
        self.config.id.as_deref()
    }

    fn touch(&self, touch: &Touch, uml: &Uml, action: &mut Option<String>) -> Result<bool> {
        // 只有「落在区域内按下并仍在区域内抬起」才算完成点击。
        if self.btn.borrow_mut().touch(touch) {
            // 无条件覆盖 `action`：本元素已经消费了事件，此处写入的即是本次交互
            // 的最终结果（未配置 action 时写入 None，表示点击不产生动作）。
            *action = self.config.action.clone();
            self.last_touched.set(uml.t);
            self.count.fetch_add(1, Ordering::SeqCst);
            // 返回 true 表示消费事件，阻止后续元素再收到这次触摸。
            return Ok(true);
        }
        Ok(false)
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let r = self.config.r.eval(uml)?.rect()?;
        // 每帧都要重新投影命中区域：区域表达式随时可变化，且 UI 变换（`#>rot`
        // 等）也可能逐帧改变，缓存上一帧的区域会导致命中偏移。
        let mut btn = self.btn.borrow_mut();
        btn.set(ui, r);
        // 按钮不绘制自身，只把状态交给脚本；`@btn`/绑定变量正是这样被后续表达式读到的。
        Ok(Var::ButtonState(ButtonState {
            last: self.last_touched.get(),
            cnt: self.count.load(Ordering::SeqCst),
            touching: btn.touching(),
        }))
    }
}

/// `let name = expr` 的运行时实现。
///
/// 它不是声明而是一次**按序求值的绑定**：`id` 返回变量名，[`Uml::render`] 在渲染到
/// 它时会把它回传的值写入变量表。由此推出两条重要语义：
/// - 绑定只对排在它**之后**的元素可见（脚本顺序 = 作用范围）；
/// - 每帧都会重新求值，所以 `let x = t` 这类写法可以让后续表达式复用随时间变化的中间量，
///   而把它放进 `#>if` 块内还能实现条件赋值。
pub struct Assign {
    /// 变量名，直接作为 [`Element::id`] 的返回值，从而被写入变量表。
    id: String,
    /// 绑定的值表达式，每帧求值一次。
    value: Expr,
}

// 仅保存名字与表达式，求值推迟到渲染阶段。
impl Assign {
    /// 建立变量绑定元素。
    pub fn new(id: String, value: Expr) -> Self {
        Self { id, value }
    }
}

// `let` 在渲染流水线中的职责：不绘制、不交互，只在元素序列中占据一个位置，
// 借此把值写入变量表（写入动作由 `Uml::render` 完成，本元素只负责返回值）。
impl Element for Assign {
    fn id(&self) -> Option<&str> {
        Some(&self.id)
    }

    fn render(&self, _ui: &mut Ui, uml: &Uml) -> Result<Var> {
        self.value.eval(uml)
    }
}

/// 各类变换元素（旋转/平移/透明度/矩阵）属性的默认值：0，
/// 即默认不做变换。因为 0 对角度/位移/矩阵元素而言是「中性」的，
/// 只有 `#>alpha` 的 0 例外（见 [`AlphaConfig`]）。
fn default_zero() -> Expr {
    constant(0.)
}

/// `#>rot` 元素的属性配置：绕指定中心旋转其后的所有元素。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RotationConfig {
    /// 旋转角度，单位为**弧度**（不是角度），默认 0。
    #[serde(default = "default_zero")]
    angle: Expr,
    /// 旋转中心的横坐标（当前坐标系下的局部坐标），默认 0。
    #[serde(default = "default_zero")]
    cx: Expr,
    /// 旋转中心的纵坐标，默认 0。
    #[serde(default = "default_zero")]
    cy: Expr,
}
/// `#>rot` 元素：以 `(cx, cy)` 为中心旋转作用域内的后续元素。
pub struct Rotation {
    /// 属性快照，逐帧渲染只读。
    config: RotationConfig,
}
// 仅保存属性；变换在渲染时按当前帧的表达式值计算。
impl Rotation {
    /// 用已解析的属性建立旋转变换元素。
    pub fn new(config: RotationConfig) -> Self {
        Self { config }
    }
}
// `#>rot` 在渲染流水线中的职责：不绘制、不交互，渲染到自身时把旋转矩阵推入作用域栈，
// 影响其后的所有元素，直到遇到 `#>pop`。
impl Element for Rotation {
    fn id(&self) -> Option<&str> {
        None
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let angle = self.config.angle.eval(uml)?.float()?;
        let cx = self.config.cx.eval(uml)?.float()?;
        let cy = self.config.cy.eval(uml)?.float()?;
        let ct = Vector2::new(cx, cy);
        // 组合出「绕点旋转」：先把中心平移到原点，旋转，再平移回去。
        // 之所以要做成矩阵而不是直接改 `ui.alpha` 之类的标量，是因为矩阵能与
        // 已有的变换相乘，从而自然地与其他变换（含 `#>mat`）叠加。
        let mat = Matrix::new_translation(&ct) * Matrix::new_rotation(angle);
        let mat = mat.prepend_translation(&-ct);
        // 压栈：保存旧的 `ui.transform` 并把新矩阵乘进去，作用域从此刻开始。
        uml.push(ui, StackLayer::Mat(mat));
        // 变换元素没有可绑定的值，返回默认值占位（其 `id()` 恒为 `None`，不会被绑定）。
        Ok(Var::default())
    }
}

/// `#>tr` 元素的属性配置：平移其后的所有元素。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationConfig {
    /// 横向位移量（当前坐标系下的局部单位），默认 0。
    #[serde(default = "default_zero")]
    dx: Expr,
    /// 纵向位移量，默认 0。
    #[serde(default = "default_zero")]
    dy: Expr,
}
/// `#>tr` 元素：对作用域内的后续元素施加纯平移。
pub struct Translation {
    /// 属性快照，逐帧渲染只读。
    config: TranslationConfig,
}
// 仅保存属性；平移量在渲染时求值，因此可以写成随时间变化的表达式（如跑马灯）。
impl Translation {
    /// 用已解析的属性建立平移变换元素。
    pub fn new(config: TranslationConfig) -> Self {
        Self { config }
    }
}
// `#>tr` 在渲染流水线中的职责：与其它变换类元素一致——把平移矩阵压入作用域栈。
impl Element for Translation {
    fn id(&self) -> Option<&str> {
        None
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let dx = self.config.dx.eval(uml)?.float()?;
        let dy = self.config.dy.eval(uml)?.float()?;
        uml.push(ui, StackLayer::Mat(Matrix::new_translation(&Vector2::new(dx, dy))));
        Ok(Var::default())
    }
}

/// `#>alpha` 元素的属性配置：给作用域内的后续元素叠加一层透明度。
///
/// 注意默认值为 0：与其它变换元素「默认 = 不做变换」不同，省略 `a` 会把后续元素
/// 完全透明化（等于整段内容消失）。因此该属性在实践中必须显式写出。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlphaConfig {
    /// 透明度倍率，默认 0。与当前作用域已有的 alpha **相乘**，而不是覆盖，
    /// 因此嵌套使用时效果会叠加，符合「作用域内整体变淡」的直觉。
    #[serde(default = "default_zero")]
    a: Expr,
}
/// `#>alpha` 元素：调整作用域内后续元素的透明度。
pub struct Alpha {
    /// 属性快照，逐帧渲染只读。
    config: AlphaConfig,
}
// 仅保存属性；透明度在渲染时求值，可用于淡入淡出动画。
impl Alpha {
    /// 用已解析的属性建立透明度变换元素。
    pub fn new(config: AlphaConfig) -> Self {
        Self { config }
    }
}
// `#>alpha` 在渲染流水线中的职责：把当前 `ui.alpha` 压栈并乘上倍率。
// 它与矩阵类变换共用同一个栈，因此 `#>pop` 会按后进先出恢复最近一次的改动——
// 这就要求脚本里的压栈与出栈成对（见 [`Uml::push`] 的说明）。
impl Element for Alpha {
    fn id(&self) -> Option<&str> {
        None
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let alpha = self.config.a.eval(uml)?.float()?;
        uml.push(ui, StackLayer::Alpha(alpha));
        Ok(Var::default())
    }
}

/// `#>mat` 元素的属性配置：直接把一个 4×4 矩阵乘进作用域。
/// 它是变换类元素里最底层的一个，其它变换（旋转/平移）都能用它表达，
/// 需要倾斜、透视、镜像等非标准变换时用它。
///
/// 字段命名沿用数学惯用的 `xIJ`，表示**第 I 行第 J 列**；`render` 会把它们重排成
/// nalgebra 需要的列优先切片，因此作者不必关心底层存储顺序。
///
/// 警告：16 个分量都有默认值 0，而全 0 矩阵会把后续元素压成一点（`w` 行也为 0，
/// 透视除法还可能产生无穷值），所以本元素的属性应当完整书写。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatConfig {
    /// 第 0 行第 0 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x00: Expr,
    /// 第 0 行第 1 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x01: Expr,
    /// 第 0 行第 2 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x02: Expr,
    /// 第 0 行第 3 列分量（默认 0）；即 `w` 行的第一项，参与透视除法。
    #[serde(default = "default_zero")]
    x03: Expr,
    /// 第 1 行第 0 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x10: Expr,
    /// 第 1 行第 1 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x11: Expr,
    /// 第 1 行第 2 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x12: Expr,
    /// 第 1 行第 3 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x13: Expr,
    /// 第 2 行第 0 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x20: Expr,
    /// 第 2 行第 1 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x21: Expr,
    /// 第 2 行第 2 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x22: Expr,
    /// 第 2 行第 3 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x23: Expr,
    /// 第 3 行第 0 列分量（默认 0）；平移分量通常写在这里。
    #[serde(default = "default_zero")]
    x30: Expr,
    /// 第 3 行第 1 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x31: Expr,
    /// 第 3 行第 2 列分量（默认 0）。
    #[serde(default = "default_zero")]
    x32: Expr,
    /// 第 3 行第 3 列分量（默认 0）；正常取 1 才表示仿射变换。
    #[serde(default = "default_zero")]
    x33: Expr,
}
/// `#>mat` 元素：把脚本给定的 4×4 矩阵乘进当前作用域。
pub struct Mat {
    /// 属性快照，逐帧渲染只读。
    config: MatConfig,
}
// 仅保存属性；矩阵各分量在渲染时求值，因此可以做出逐帧变化的任意线性变换。
impl Mat {
    /// 用已解析的属性建立矩阵变换元素。
    pub fn new(config: MatConfig) -> Self {
        Self { config }
    }
}
// `#>mat` 在渲染流水线中的职责：把所有分量求值成矩阵后压栈。
impl Element for Mat {
    fn id(&self) -> Option<&str> {
        None
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        let x00 = self.config.x00.eval(uml)?.float()?;
        let x01 = self.config.x01.eval(uml)?.float()?;
        let x02 = self.config.x02.eval(uml)?.float()?;
        let x03 = self.config.x03.eval(uml)?.float()?;
        let x10 = self.config.x10.eval(uml)?.float()?;
        let x11 = self.config.x11.eval(uml)?.float()?;
        let x12 = self.config.x12.eval(uml)?.float()?;
        let x13 = self.config.x13.eval(uml)?.float()?;
        let x20 = self.config.x20.eval(uml)?.float()?;
        let x21 = self.config.x21.eval(uml)?.float()?;
        let x22 = self.config.x22.eval(uml)?.float()?;
        let x23 = self.config.x23.eval(uml)?.float()?;
        let x30 = self.config.x30.eval(uml)?.float()?;
        let x31 = self.config.x31.eval(uml)?.float()?;
        let x32 = self.config.x32.eval(uml)?.float()?;
        let x33 = self.config.x33.eval(uml)?.float()?;
        // 列优先切片：nalgebra 按列存储，所以这里把 `xIJ`（行 I 列 J）按
        // 「先列后行」的顺序排开，与上面的字段命名约定正好互补。
        let mat = Matrix::from_column_slice(&[x00, x10, x20, x30, x01, x11, x21, x31, x02, x12, x22, x32, x03, x13, x23, x33]);
        uml.push(ui, StackLayer::Mat(mat));
        Ok(Var::default())
    }
}

/// `#>pop` 元素：结束最近一次由变换类元素开启的作用域。
/// 它是单元结构体，因为出栈不需要任何参数——栈本身记录了要恢复的内容。
pub struct Pop;
// `#>pop` 在渲染流水线中的职责：把作用域栈顶弹出并还原到 `ui` 上。
// 多余的 `pop`（栈已空）会被静默忽略而不报错，因此脚本里多写一个 `#>pop` 不会崩页，
// 但会失去「配平检查」这种错误提示。
impl Element for Pop {
    fn id(&self) -> Option<&str> {
        None
    }

    fn render(&self, ui: &mut Ui, uml: &Uml) -> Result<Var> {
        uml.pop(ui);
        Ok(Var::default())
    }
}

/// UML 的一等值类型，也是变量系统与表达式求值的公共值域。
///
/// 三个变体刚好对应脚本能引用的三类东西：元素回传的矩形（布局基础）、按钮状态
/// （交互反馈），以及纯数值。`RawExpr::eval` 的全部类型规则都是围绕这三者展开的，
/// 二元运算、字段访问、内建函数也都以「能否归约成浮点」为判据。
///
/// 值的生命周期由变量表决定：元素 `id` 绑定的值每帧重建，`global` 定义的全局变量
/// 跨帧保留（见 [`Uml::render`] 开头的变量清理）。
#[derive(Clone, Copy)]
pub enum Var {
    /// 一个矩形（位置 + 尺寸），来自文本/图片/矩形/按钮等元素的实际绘制范围。
    Rect(Rect),
    /// 按钮状态，来自按钮元素的回传值或 `global x = @btn` 的初值。
    ButtonState(ButtonState),
    /// 浮点数，表达式计算的主要形态。
    Float(f32),
}
// 默认取浮点 0：让变换类元素（它们没有真正的回传值）也能走统一的返回路径，
// 同时在数值参与运算时保持中性。
impl Default for Var {
    fn default() -> Self {
        Self::Float(0.)
    }
}

// 值的取用与类型断言。
// 这两个方法都不做隐式转换（矩形不会自动退化成宽或高），因为「取哪个分量」必须由
// 脚本显式用字段访问表达；类型不符时直接报错，避免布局静默错位。
impl Var {
    /// 取出浮点值。
    ///
    /// # Errors
    /// 值不是浮点（如把矩形直接用作字号）时返回错误。
    pub fn float(self) -> Result<f32> {
        match self {
            Self::Float(f) => Ok(f),
            _ => bail!("expected float"),
        }
    }

    /// 取出矩形值。
    ///
    /// # Errors
    /// 值不是矩形（如给 `r` 属性写了纯数字）时返回错误。
    pub fn rect(self) -> Result<Rect> {
        match self {
            Self::Rect(r) => Ok(r),
            _ => bail!("expected rect"),
        }
    }
}

/// 作用域栈里保存的一层**旧值**，供 `#>pop` 还原。
///
/// 两类改动（矩阵变换、透明度）共用同一个栈，而不是各自一个栈：只有这样，
/// `#>rot`/`#>alpha` 交叉书写时的还原顺序才严格遵循后进先出。若分成两个栈，
/// 交叉的 `#>pop` 会把某一类恢复成不属于当前层级的旧值。
enum StackLayer {
    /// 压栈前的 `ui.transform`。
    Mat(Matrix),
    /// 压栈前的 `ui.alpha`。
    Alpha(f32),
}
/// UML 运行时：把解析产物（语句序列 + 全局定义）驱动起来。
///
/// 它是脚本与场景之间的唯一桥梁，每帧被 `EventScene` 调用若干次；「一帧」的完整
/// 语义是：[`Uml::render`] 先把变量表重置为「只剩全局变量 + 本次注入的运行时变量」，
/// 然后按序执行语句——元素通过 `id`/`let` 把值写回变量表，后续元素（含同一帧内
/// 靠后的元素）即可读到，从而形成「顺序即数据流」的模型。
pub struct Uml {
    /// 顶层语句序列（解析产物）。它同时承载了渲染顺序与作用范围两重含义。
    elements: Vec<TopLevel>,

    /// 变量表：脚本中所有可被表达式引用的名字到值的映射。
    /// 其中元素 `id` 绑定的条目每帧重建，只有 `persistent_vars` 里的条目会跨帧保留。
    var_map: HashMap<String, Var>,
    /// 需要跨帧保留的变量名（`global` 定义的那些），是每帧清理变量表时的白名单。
    persistent_vars: Vec<String>,

    /// 变换/透明度的作用域栈，`#>rot`/`#>tr`/`#>alpha`/`#>mat` 压栈、`#>pop` 出栈。
    /// 用 `RefCell` 是因为元素是以 `&Uml` 被渲染的（见 [`Uml::render`]），
    /// 而压栈/出栈需要修改自身状态。
    ///
    /// 不变量：脚本中的压栈与出栈应当成对。栈本身在帧之间**不清空**，因此持续不配平
    /// 会让它不断增长，并在出栈多于压栈时还原出上一帧的过期变换。
    stack: RefCell<Vec<StackLayer>>,

    /// 本帧的谱面时间（秒）。元素的表达式通常以它为动画自变量。
    t: f32,
    /// 本帧的真实时间（秒），不受暂停/变速影响，用于与谱面进度无关的动画。
    rt: f32,

    /// 是否为首帧。当前仅被赋值、未被读取（见 [`Uml::render`] 末尾），
    /// 保留字段不删除以免改变结构布局。
    first_time: bool,
}

// 空脚本对应的默认运行时：没有元素、没有全局变量。
// `new` 在这里可以安全 `unwrap`，因为空定义列表不会触发任何求值。
impl Default for Uml {
    fn default() -> Self {
        Self::new(Vec::new(), &[]).unwrap()
    }
}

impl Uml {
    /// 建立运行时。
    ///
    /// 初始化流程分两步：
    /// 1. 先构造出「空变量表 + 空栈 + 时间归零」的骨架；
    /// 2. 再调用 [`Uml::init`] 逐个求值全局定义并登记为持久变量。
    ///
    /// 之所以要分两步，是因为全局定义的初值本身是表达式，求值需要一个已存在的
    /// `Uml`（`eval` 的签名要求 `&Uml`）；这也是 [`Uml::init`] 单独成函数的原因。
    ///
    /// # Panics
    /// 全局定义的初值若引用了尚未定义的变量（包括任何元素 `id`——此刻它们都还没有
    /// 被绑定），[`Uml::init`] 内的 `unwrap` 会 panic。畸形脚本因此可能使客户端崩溃。
    pub fn new(elements: Vec<TopLevel>, global_defs: &[(String, Expr)]) -> Result<Self> {
        // 阶段一：构造空骨架。时间与栈都从零开始，`first_time` 标记首帧。
        let mut res = Self {
            elements,

            var_map: HashMap::new(),
            persistent_vars: Vec::new(),

            stack: RefCell::new(Vec::new()),

            t: 0.,
            rt: 0.,

            first_time: true,
        };
        // 阶段二：求值并登记全局变量。
        res.init(global_defs);
        Ok(res)
    }

    /// 求值全局定义并写入变量表，同时把它们登记为持久变量。
    /// 求值顺序即脚本中的声明顺序，因此后面的全局变量可以引用前面的——
    /// 这也是唯一允许「引用尚未出现的名字」的场合（元素 id 在此阶段还都不存在）。
    fn init(&mut self, global_defs: &[(String, Expr)]) {
        for (name, initial) in global_defs {
            // 未定义的引用会在此 panic；调用方（`parse_uml`）不会捕获它。
            self.var_map.insert(name.clone(), initial.eval(self).unwrap());
            self.persistent_vars.push(name.clone());
        }
    }

    /// 压入一层作用域：保存当前值，再把新变换**乘**到 `ui` 上。
    /// 注意保存的是「旧值」而不是新值——这样 `#>pop` 只需按栈顶恢复即可，
    /// 不需要重新计算；同时新变换与已有变换相乘（而非覆盖），使嵌套变换可以叠加。
    fn push(&self, ui: &mut Ui, layer: StackLayer) {
        match layer {
            StackLayer::Mat(mat) => {
                self.stack.borrow_mut().push(StackLayer::Mat(ui.transform));
                ui.transform *= mat;
            }
            StackLayer::Alpha(alpha) => {
                self.stack.borrow_mut().push(StackLayer::Alpha(ui.alpha));
                ui.alpha *= alpha;
            }
        }
    }
    /// 弹出栈顶并恢复到 `ui`。栈为空时静默忽略——多余 `#>pop` 不是致命错误，
    /// 但这意味着脚本的作用域已经失衡：此后多余出栈会依次还原更早的（可能是上一帧的）值。
    fn pop(&self, ui: &mut Ui) {
        match self.stack.borrow_mut().pop() {
            Some(StackLayer::Mat(mat)) => ui.transform = mat,
            Some(StackLayer::Alpha(alpha)) => ui.alpha = alpha,
            None => {}
        }
    }

    /// 按名读取变量，供表达式求值使用。
    ///
    /// # Errors
    /// 名字未定义（拼写错误、引用尚未绑定的元素，或引用了当前 `#>if` 分支之外
    /// 才建立的变量）时返回错误。
    pub(crate) fn get_var(&self, id: &str) -> Result<&Var> {
        self.var_map.get(id).ok_or_else(|| anyhow!("variable not found: {id}"))
    }

    /// 把一次触摸分发给元素。
    ///
    /// 先更新帧时间再分发，是因为元素的命中判定与反馈动画都要用当前时间。
    /// 按声明顺序遍历并**在第一个消费事件的元素处终止**，因此脚本中越靠前的元素
    /// 越「在上面」；`action` 若被某个元素写入即随返回值一起交给场景。
    ///
    /// # Returns
    /// `true` 表示已有元素消费本次事件；`false` 表示无人处理。
    pub fn touch(&mut self, touch: &Touch, t: f32, rt: f32, action: &mut Option<String>) -> Result<bool> {
        self.t = t;
        self.rt = rt;
        for el in &self.elements {
            if let TopLevel::Element(el) = el {
                if el.touch(touch, self, action)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// 渲染一帧：重置变量表 → 按序执行语句 → 返回内容包围盒。
    ///
    /// `vars` 是场景每帧注入的运行时变量（`t` 谱面时间、`o` 判定偏移、`top` 可视高度、
    /// `joined` 是否已加入活动），它们是脚本可用的「外部输入」。另外恒有一个
    /// `version = 2.0`：它是**脚本语言版本号**（供作者做新旧脚本兼容分支），
    /// 与请求 UML 时携带的客户端包版本无关。
    ///
    /// 条件指令（`#>if`/`#>elif`/`#>else`/`#>fi`）在这里才被求值，且状态保存在一个
    /// 三态栈里，因此条件可以嵌套并逐帧变化——这正是 UML「同一个脚本既能排布又能
    /// 随交互变化」的关键。
    ///
    /// # Returns
    /// `(right, bottom)`：本帧所有元素回传矩形的最大右下坐标，即内容的包围盒；
    /// 若脚本定义了 `$w`/`$h` 浮点变量则以它们为准。`EventScene` 用它决定滚动区高度。
    /// 在没有任何元素回传矩形时返回 `(0, 0)`。
    ///
    /// # Errors
    /// 任一元素的属性表达式求值失败都会立即中断**本帧剩余元素**的渲染并把错误上抛；
    /// 调用方打印错误并把高度记 0，下一帧仍会重新尝试，因此表现为「页面从出错处
    /// 被截断且逐帧报错」，而不是整页不可用。
    pub fn render(&mut self, ui: &mut Ui, t: f32, rt: f32, vars: &[(&str, f32)]) -> Result<(f32, f32)> {
        // 步骤一：变量表重置。只保留持久变量（`global` 定义的），其余（上一帧由元素
        // `id` 或 `let` 建立的绑定）全部丢弃，避免把过期值留给本帧的表达式。
        // 这里先用 `mem::take` 把 map 整体换出，是为了能「移动」它去构造新 map
        // （直接移动会有「不能从借用内容中移出」的限制），同时 filter 里还能只读
        // 借用 `self.persistent_vars`。
        self.var_map = std::mem::take(&mut self.var_map)
            .into_iter()
            .filter(|(key, _)| self.persistent_vars.contains(key))
            .collect::<HashMap<_, _>>();
        // 步骤二：注入场景提供的运行时变量，并固定补上脚本语言版本号。
        for (name, value) in vars.iter().copied().chain(std::iter::once(("version", 2.))) {
            self.var_map.insert(name.to_owned(), Var::Float(value));
        }

        // 条件块的求值状态。用三态而不是布尔，是为了区分两种「不生效」：
        // `IfFailed` 表示尚无分支命中，后续 `#>else`/`#>elif` 仍可尝试；
        // `Nopped` 表示已有分支命中，后续分支必须全部跳过。
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum IfState {
            IfPassed,
            IfFailed,
            Nopped,
        }

        // 栈底放一个哨兵（恒为 `IfPassed`），使栈永不为空——
        // `#>else` 等分支会直接 `ifs.last_mut().unwrap()`，空栈会 panic。
        let mut ifs = vec![IfState::IfPassed];

        // 内容包围盒的累积量，逐帧从零开始测量。
        let mut right = 0f32;
        let mut bottom = 0f32;
        self.t = t;
        self.rt = rt;
        // 外层 `scope` 保证帧末 `transform` 被还原（`alpha` 不在其保护范围内，
        // 只能靠脚本里的 `#>pop` 配平），避免变换泄漏到场景其它 UI 上。
        ui.scope::<Result<()>>(|ui| {
            // 步骤三：按脚本顺序执行顶层语句。
            for el in &self.elements {
                match el {
                    // 元素：只有在最内层条件为「通过」时才渲染；否则整条语句只是被跳过
                    // （注意不渲染也就不绑定变量，因此被条件挡住的元素其 `id` 不可引用）。
                    TopLevel::Element(el) => {
                        if let Some(IfState::IfPassed) = ifs.last() {
                            let r = el.render(ui, self)?;
                            // 只有矩形参与内容尺寸统计：浮点/按钮状态无法表达占用范围。
                            if let Var::Rect(r) = &r {
                                right = right.max(r.right());
                                bottom = bottom.max(r.bottom());
                            }
                            // 把回传值以元素 id 写入变量表，供其后的表达式引用。
                            if let Some(id) = el.id() {
                                self.var_map.insert(id.to_owned(), r);
                            }
                        }
                    }
                    // `#>if`：外层通过时才压下新状态；非 0 视为真。
                    TopLevel::If(cond) => {
                        if let Some(IfState::IfPassed) = ifs.last() {
                            ifs.push(if cond.eval(self)?.float()? > 0. {
                                IfState::IfPassed
                            } else {
                                IfState::IfFailed
                            });
                        }
                    }
                    // `#>else`：仅在「尚无分支命中」时接管，否则标记为 Nopped 以屏蔽后续分支。
                    TopLevel::Else => {
                        if let Some(IfState::IfFailed) = ifs.last() {
                            *ifs.last_mut().unwrap() = IfState::IfPassed;
                        } else {
                            *ifs.last_mut().unwrap() = IfState::Nopped;
                        }
                    }
                    // `#>elif`：语义与 `#>else` 相同，条件通过才成为生效分支。
                    TopLevel::ElseIf(cond) => {
                        if let Some(IfState::IfFailed) = ifs.last() {
                            *ifs.last_mut().unwrap() = if cond.eval(self)?.float()? > 0. {
                                IfState::IfPassed
                            } else {
                                IfState::IfFailed
                            };
                        } else {
                            *ifs.last_mut().unwrap() = IfState::Nopped;
                        }
                    }
                    // `#>fi`：无条件出栈。配合上面「外层不通过时 `#>if` 不压栈」的行为，
                    // 在**未通过**的分支内部再嵌套条件块会造成括号失配（内层没压栈却仍被
                    // 弹出），从而误开/误关外层分支；嵌套应写在外层条件成立的分支里。
                    TopLevel::EndIf => {
                        ifs.pop();
                    }
                    // 全局定义在 `Uml::new` 阶段就已处理，渲染阶段无需再做任何事。
                    TopLevel::GlobalDef(..) => {}
                }
            }

            Ok(())
        })?;
        // 步骤四：允许脚本用 `$w`/`$h` 覆盖实测尺寸（例如为底部预留空间）。
        // 只认浮点值，其它类型视为未定义——否则会把布局尺寸算成无意义的数。
        if let Some(Var::Float(w)) = self.var_map.get("$w") {
            right = *w;
        }
        if let Some(Var::Float(h)) = self.var_map.get("$h") {
            bottom = *h;
        }
        // 标记已过首帧（当前无读取方，保留以免改变行为）。
        self.first_time = false;

        Ok((right, bottom))
    }

    /// 置顶渲染：在同一帧的 [`Uml::render`] 之后调用，用于绘制必须压在其它内容
    /// 之上的部分（实现上是把调用转发给每个元素的 `render_top`）。
    ///
    /// 这里重新写入 `t`/`rt`，是因为它是一个独立调用点，可能与 `render` 之间隔了
    /// 其它绘制；元素不该假设自己缓存的帧时间仍然有效。
    ///
    /// 注意：条件块（`#>if`）只在 `render` 阶段决定元素是否渲染，本方法对**所有**元素
    /// 无差别调用，因此被条件挡住的元素仍可能在置顶阶段绘制自己的叠加层。
    ///
    /// # Errors
    /// 任一元素的置顶绘制失败即中断返回，与 `render` 的错误处理一致。
    pub fn render_top(&mut self, ui: &mut Ui, t: f32, rt: f32) -> Result<()> {
        self.t = t;
        self.rt = rt;
        for el in &self.elements {
            if let TopLevel::Element(el) = el {
                el.render_top(ui, self)?;
            }
        }
        Ok(())
    }

    /// 把「子界面选择结束」的通知转发给所有元素。
    /// `t` 为当前时间，`delete` 为真表示取消选择。与 [`Uml::render_top`] 一样，
    /// 本方法不受条件块约束（它不属于绘制流程）。
    pub fn on_result(&self, t: f32, delete: bool) {
        for el in &self.elements {
            if let TopLevel::Element(el) = el {
                el.on_result(t, delete);
            }
        }
    }

    /// 收集元素提出的场景切换请求。
    ///
    /// 取**第一个**非 `None` 的结果并立即返回，因此同一帧至多有一个切换生效，
    /// 脚本中靠前的元素优先级更高，其余请求被忽略。返回 `None` 表示本帧没有元素
    /// 要求切换场景（`EventScene` 会保持当前页）。
    pub fn next_scene(&self) -> Option<NextScene> {
        for el in &self.elements {
            if let TopLevel::Element(el) = el {
                if let Some(next) = el.next_scene() {
                    return Some(next);
                }
            }
        }
        None
    }
}
