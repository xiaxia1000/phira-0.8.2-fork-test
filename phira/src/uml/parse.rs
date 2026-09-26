//! UML 脚本的语法分析与表达式求值。
//!
//! `super::lexer` 交出 token 流后，本模块把它组装成两类产物：
//! - 顶层语句序列 [`TopLevel`]（元素定义、全局变量定义、条件块）；
//! - 表达式树 [`RawExpr`]（元素属性值与条件里的算术/比较式）。
//!
//! 与常规编译器的关键差别在于**表达式不在解析期求值**：`2 * t + 1` 这样的属性值
//! 只在解析时建成树，等每帧渲染时结合 [`Uml`] 里的变量（时间、全局变量、元素 id
//! 绑定的变量、`@btn`）重新求值。活动页的动画与条件分支正是靠「同一棵树在不同帧
//! 得到不同结果」实现的；也正因如此，脚本可以在客户端不发版的前提下改变页面行为。
//!
//! 本模块的错误类型统一用 `String`：解析发生在收到服务端脚本之后，错误需要原样
//! 反馈给作者或日志，用轻量的字符串足够，也便于跨 `Result` 边界传递。

use super::{lexer::Token, Alpha, Assign, ButtonElement, Collection, Element, Image, Mat, Pop, RectElement, Rotation, Text, Translation, Uml, Var};
use crate::icons::Icons;
use anyhow::Result;
use logos::Logos;
use macroquad::prelude::Rect;
use prpr::ext::{JoinToString, RectExt, SafeTexture};
use serde::{
    de::{value::MapDeserializer, DeserializeOwned, Visitor},
    Deserialize,
};
use std::{collections::HashMap, fmt::Display, iter::Peekable, sync::Arc};
use tap::Tap;

/// 以 `format!` 构造 `Err(String)` 并提前返回。
/// 之所以不直接用 `anyhow::bail!`，是因为本模块对外统一暴露 `String` 错误，
/// 而 `anyhow` 的错误在这里反而要在边界处再转一层字符串。
macro_rules! bail {
    ($($t:tt)*) => {
        return Err(format!($($t)*))
    }
}

/// 解析器使用的 token 流类型。
/// 外面包一层 [`Peekable`] 是必需的：解析中大量存在「先看一个 token 再决定怎么走」
/// 的场景（元素是否带配置块、标识符后面是 `.` 还是 `(`、下一个是否运算符），
/// 而 `logos` 的迭代器本身不能回退。
type Lexer<'a> = Peekable<logos::Lexer<'a, Token>>;

/// 断言下一个 token 恰为 `token`，否则报错。
/// 把期望的 token 一并写进错误信息，是因为脚本由活动作者手写，报错需要指出具体
/// 缺了哪个符号，而不是笼统的「语法错误」。
fn take(lexer: &mut Lexer, token: Token) -> Result<(), String> {
    if lexer.next().as_ref().map(|it| it.as_ref()) != Some(Ok(&token)) {
        bail!("expected {:?}", token);
    }
    Ok(())
}

/// 解析元素的属性配置块，并反序列化为具体元素的配置类型 `T`。
///
/// 设计要点：
/// - 属性块写成 `(name: value, ...)`，整体是**可选**的；没有时直接返回 `T::default()`，
///   于是作者只需写出想覆盖的属性，其余沿用元素默认值（例如 `r` 默认 0）。
/// - 解析结果不直接喂给 `T`，而是先收集成 `HashMap<String, serde_json::Value>`，
///   再交给各元素自己的 `#[derive(Deserialize)]`。这样「属性名 → Rust 字段名」
///   （`rename_all = "camelCase"`）、缺省值（`#[serde(default)]`）等映射全部由 serde
///   统一处理，解析器本身无需认识任何元素的具体属性，新增属性也不必改这里。
/// - 属性值分三种形态：`"字符串"` 原样保留为 JSON 字符串，布尔字面量保留为 JSON 布尔，
///   其余一律按表达式解析后 `to_string()` 存成 JSON 字符串。因此 [`Expr`] 的反序列化
///   只要处理字符串即可，表达式与数字两种写法自然汇合到同一条路径。
///
/// 注意：未启用 `deny_unknown_fields`，属性名拼错会被 serde 静默忽略，
/// 页面表现为「属性没生效」而不是报错。
///
/// # Panics
/// 读取属性分隔符时使用了 `lexer.next().unwrap().unwrap()`：如果此处出现词法错误
/// （例如非法数值字面量），内层 `unwrap` 会 panic 而不是返回 `Err`。
///
/// # Errors
/// 缺少属性名、缺少 `:`、分隔符不是 `,`/`)`、或属性值与目标字段类型不匹配时返回错误。
fn take_config<T: DeserializeOwned>(lexer: &mut Lexer) -> Result<T, String> {
    let mut map: HashMap<String, serde_json::Value> = HashMap::new();
    // 属性块用 `(...)` 括起（词法变体名为 `LBrace`，但实际字面量是圆括号，
    // 因为 `{}` 已被 `p` 的文本块占用）。缺少该符号表示「全部属性走默认值」。
    if lexer.peek() == Some(&Ok(Token::LBrace)) {
        lexer.next();
        loop {
            let Some(Ok(Token::Ident(name))) = lexer.next() else {
                bail!("expected attribute name");
            };
            take(lexer, Token::Colon)?;
            // 属性值只有三种形态：引号串按字面保留、布尔直接保留、
            // 其余（数字与算式）都解析成表达式后序列化为文本，
            // 由 `Expr` 的反序列化再解析回来，从而统一成同一套处理。
            let value = match lexer.peek() {
                Some(Ok(Token::Quoted(s))) => serde_json::Value::String(s.to_owned()).tap(|_| {
                    lexer.next();
                }),
                Some(Ok(Token::Bool(val))) => serde_json::Value::Bool(*val).tap(|_| {
                    lexer.next();
                }),
                _ => serde_json::Value::String(take_expr(lexer)?.to_string()),
            };
            map.insert(name, value);
            match lexer.next().unwrap().unwrap() {
                Token::Comma => continue,
                Token::RBrace => break,
                x => bail!("expected brace or comma, got {x:?}"),
            }
        }
    }
    // serde 在此完成「属性名 → Rust 字段名」的映射（camelCase）与缺省值填充；
    // 未识别的属性会被静默忽略而不是报错。
    T::deserialize(MapDeserializer::new(map.into_iter())).map_err(|it| it.to_string())
}

/// 读取紧跟属性块之后的文本块（`p` 元素的正文）。
/// 正文必须整体取出而不能走表达式解析：文本里允许出现任意字符与空格，
/// 若按 token 逐个解析，`{Hello 2024}` 里的数字会被当成数值字面量而报错。
fn take_text(lexer: &mut Lexer) -> Result<String, String> {
    let Some(Ok(Token::Text(s))) = lexer.next() else {
        bail!("expected text");
    };
    Ok(s)
}

/// 表达式在 AST 中的存放形式。
/// 表达式树是递归结构（函数实参、二元运算两侧同样是表达式），必须装箱才能确定
/// 大小；同时 `Box` 让元素配置可以低成本地在解析结果、`Uml` 与渲染代码之间传递。
pub type Expr = Box<RawExpr>;

/// `bail!` 在本文件中的重复定义（与上方那处完全一致）。
/// `macro_rules!` 同名重复定义时后定义者在后续代码中生效；此处两处展开结果相同，
/// 疑为历史遗留的冗余，为避免改变行为未做删除。
macro_rules! bail {
    ($($t:tt)*) => {
        return Err(format!($($t)*))
    }
}

/// 二元运算符。
/// 比较类运算符在这里**不返回布尔**，而是在求值时转成 0.0/1.0 的浮点值，
/// 这样才能直接参与算术、并被 `#>if` 以「非 0 即真」的方式消费。
#[derive(Debug, Clone, Copy)]
pub enum BinOp {
    /// 加法。左侧为矩形时表示按 `y` 向四周扩张。
    Add,
    /// 减法。左侧为矩形时表示按 `y` 向四周收缩（负扩张）。
    Sub,
    /// 乘法（仅浮点有定义）。
    Mul,
    /// 除法（仅浮点有定义，不做除零检查）。
    Div,
    /// 小于，结果为 0.0/1.0。
    Lt,
    /// 小于等于，结果为 0.0/1.0。
    Le,
    /// 大于，结果为 0.0/1.0。
    Gt,
    /// 大于等于，结果为 0.0/1.0。
    Ge,
    /// 相等，结果为 0.0/1.0。
    Eq,
    /// 不等，结果为 0.0/1.0。
    Neq,
}

// 运算符优先级表。
// 其存在意义是让作者手写的算式符合数学直觉：`x + w * 0.5` 必须解析为
// `x + (w * 0.5)`，`a < b + c` 必须解析为 `a < (b + c)`，否则布局与动画的计算
// 结果会与作者的预期不符。
impl BinOp {
    /// 返回该运算符的优先级，**数值越小结合越紧**：
    /// `* /` 为 1，`+ -` 为 2，关系运算（`< <= > >=`）为 3，相等运算（`== !=`）为 4。
    /// 该表与 [`take_expr`] 的调度场算法配合决定归约时机。
    pub fn precedence(&self) -> u8 {
        match self {
            Self::Mul | Self::Div => 1,
            Self::Add | Self::Sub => 2,
            Self::Lt | Self::Le | Self::Gt | Self::Ge => 3,
            Self::Eq | Self::Neq => 4,
        }
    }
}

/// 内建函数的实现签名：接收已求值的实参列表，返回单个值。
/// 用 `Box<dyn Fn>` 而不是函数指针，是因为 `max`/`min` 等需要以闭包形式携带各自的
/// 归约逻辑；函数实现会在**解析时**就绑定到 [`RawExpr::Func`] 节点上，
/// 逐帧求值不必再做名字查找。
type Function = Box<dyn Fn(&[Var]) -> Result<Var>>;

/// 按钮状态的快照，即表达式里 `@btn` 的值。
/// 按钮在渲染时把自己的状态写回变量，其他元素便能读取它来对悬停/按下做出反应
/// （例如按下时放大、或加一层高亮），这是 UML 里元素间唯一的交互反馈通道。
#[derive(Debug, Clone, Copy)]
pub struct ButtonState {
    /// 最近一次被点击的时间（秒）；从未点击过时为 -1.0。
    /// 作者可用 `@btn.last` 与当前时间比较，做出「刚点击后的一小段时间」这类效果。
    pub last: f32,
    /// 累计点击次数，只增不减（由按钮上的原子计数提供）。
    pub cnt: u32,
    /// 当前是否正被按住。
    pub touching: bool,
}

// 默认值即「从未交互过」：last = -1.0、cnt = 0、未按住。
// `global x = @btn` 声明出来的按钮状态变量在首帧读到的就是它。
impl Default for ButtonState {
    fn default() -> Self {
        Self {
            last: -1.0,
            cnt: 0,
            touching: false,
        }
    }
}

/// 表达式 AST。
/// 节点本身不含时间或变量值——变量要到渲染时按名字从 [`Uml`] 的变量表里查，
/// 因此同一棵树在不同帧、不同作用域下会得到不同结果，这就是 UML 的动画模型。
pub enum RawExpr {
    /// 数字字面量。
    Literal(f32),
    /// 按钮状态值，对应写法 `@btn`。
    ButtonState(ButtonState),
    /// 矩形字面量 `[x, y, w, h]`，四个分量按顺序解析。
    Rect([Expr; 4]),
    /// 变量引用：可能是 `global` 定义的全局变量、`let` 定义的变量，或前面元素
    /// 通过 `id` 绑定的值。查不到时报错（见 [`Uml::get_var`]）。
    Var(String),
    /// 变量字段访问 `var.field`，例如 `<矩形的 id>.w`、`@btn.cnt`。
    /// 字段名按被访问变量的类型解释，未知字段名在求值时才会报错。
    VarSub(String, String),
    /// 内建函数调用：`(函数名, 已绑定的实现, 实参表达式)`。
    /// 名字在解析 [`take_atom`] 时即完成校验，未知名不会形成此节点。
    Func(&'static str, Function, Vec<Expr>),
    /// 二元运算：`(左, 右, 运算符)`。
    BinOp(Expr, Expr, BinOp),
}

// 为表达式树实现 `Display`，服务两条路径：
// 一是解析/运行出错时回显表达式，二是 `take_config` 把属性值表达式序列化成文本
// 再交给 `Expr` 的 `Deserialize`。后者要求输出能被 `parse_expr` 读回，
// 因此二元运算一律输出括号，保证结合顺序在字符串往返后不失真。
impl Display for RawExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Literal(val) => val.fmt(f),
            Self::ButtonState(_) => write!(f, "@btn"),
            Self::Rect([x, y, w, h]) => write!(f, "[{x}, {y}, {w}, {h}]"),
            Self::Var(rf) => rf.fmt(f),
            Self::VarSub(rf, field) => {
                write!(f, "{rf}.{field}")
            }
            Self::BinOp(x, y, op) => {
                write!(
                    f,
                    "({x} {} {y})",
                    match op {
                        BinOp::Add => "+",
                        BinOp::Sub => "-",
                        BinOp::Mul => "*",
                        BinOp::Div => "/",
                        BinOp::Lt => "<",
                        BinOp::Le => "<=",
                        BinOp::Gt => ">",
                        BinOp::Ge => ">=",
                        BinOp::Eq => "==",
                        BinOp::Neq => "!=",
                    }
                )
            }
            Self::Func(name, _, inner) => {
                write!(f, "{name}({})", inner.iter().map(|it| it.to_string()).join(", "))
            }
        }
    }
}
// `Debug` 直接复用 `Display` 的写法，让日志中打印的表达式与源码形式一致，
// 便于把报错信息对照脚本排查。
impl std::fmt::Debug for RawExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Self as Display>::fmt(self, f)
    }
}

// 表达式求值：把 AST 与当前 [`Uml`] 的变量环境结合成具体值。
// 这是每帧都会走一遍的热路径（每个元素的每个属性都要算一遍），因此实现只做必要的
// 递归与类型检查，不做缓存；类型不匹配在此处才暴露，由调用方决定报错还是跳过。
impl RawExpr {
    /// 在当前变量环境下求值。
    ///
    /// 求值规则可以概括为「按左值类型分派」：
    /// - 字面量直接成为对应的 [`Var`]；
    /// - 变量按名从 [`Uml`] 的变量表读取，字段访问再按变量类型投影成浮点；
    /// - 二元运算先求两侧，再按左值类型决定语义：矩形支持 `+`/`-` 做扩张/收缩，
    ///   浮点支持全部十个运算符，其余组合报错；
    /// - 函数调用先把所有实参求值，再交给解析期就绑定好的实现。
    ///
    /// # Errors
    /// 变量未定义、字段名不认识（如对浮点取 `.w`）、对不支持的左值组合做运算、
    /// 实参个数不符，都会返回错误。错误最终会中断调用者所在那一帧剩余元素的渲染
    /// （见 `Uml::render`），由场景打印并降级处理，而不是 panic。
    pub fn eval(&self, uml: &Uml) -> Result<Var> {
        Ok(match self {
            // —— 字面量：数值 / 按钮状态 / 矩形 ——
            Self::Literal(val) => Var::Float(*val),
            Self::ButtonState(state) => Var::ButtonState(*state),
            Self::Rect([x, y, w, h]) => {
                Var::Rect(Rect::new(x.eval(uml)?.float()?, y.eval(uml)?.float()?, w.eval(uml)?.float()?, h.eval(uml)?.float()?))
            }
            // —— 变量读取：查不到即报错，因此脚本必须保证「先绑定后引用」 ——
            Self::Var(rf) => *uml.get_var(rf)?,
            // —— 字段投影：把复合变量拆成单个浮点，供布局计算直接使用 ——
            Self::VarSub(rf, field) => match uml.get_var(rf)? {
                // 矩形的字段：`x`/`l` 左边界、`y`/`t` 上边界、`w` 宽、`h` 高，
                // `r`/`b` 为右/下边界，`cx`/`cy` 为中心点。
                Var::Rect(r) => Var::Float(match field.as_str() {
                    "x" | "l" => r.x,
                    "y" | "t" => r.y,
                    "w" => r.w,
                    "h" => r.h,
                    "r" => r.right(),
                    "b" => r.bottom(),
                    "cx" => r.center().x,
                    "cy" => r.center().y,
                    _ => anyhow::bail!("unknown field: {field}"),
                }),
                // 按钮状态的字段：点击时间、点击次数、是否按住；三组别名对应
                // 文档里常见的简写写法。布尔按 `as u32 as f32` 转成 0.0/1.0。
                Var::ButtonState(s) => Var::Float(match field.as_str() {
                    "l" | "last" => s.last,
                    "c" | "cnt" | "count" => s.cnt as _,
                    "t" | "touching" => s.touching as u32 as _,
                    _ => anyhow::bail!("unknown field: {field}"),
                }),
                // 浮点没有可访问的字段，取字段一律报错。
                Var::Float(_) => anyhow::bail!("cannot access float"),
            },
            // —— 二元运算：先求左值（保留其类型），右值则统一要求可转成浮点 ——
            Self::BinOp(x, y, op) => {
                let x = x.eval(uml)?;
                let y = y.eval(uml)?.float()?;
                match x {
                    // 矩形与浮点的加减表示四周扩张/收缩（`RectExt::feather`），
                    // 其余运算符对矩形无定义；负的扩张量即收缩。
                    Var::Rect(r) => match op {
                        BinOp::Add => Var::Rect(r.feather(y)),
                        BinOp::Sub => Var::Rect(r.feather(-y)),
                        x => anyhow::bail!("invalid op on rect and float: {x:?}"),
                    },
                    // 浮点支持全部运算符；比较类的结果是 0.0/1.0 而非布尔。
                    // 除法不检查除零，按 IEEE 规则得到 inf/NaN。
                    Var::Float(x) => Var::Float(match op {
                        BinOp::Add => x + y,
                        BinOp::Sub => x - y,
                        BinOp::Mul => x * y,
                        BinOp::Div => x / y,
                        BinOp::Lt => (x < y) as u32 as _,
                        BinOp::Le => (x <= y) as u32 as _,
                        BinOp::Gt => (x > y) as u32 as _,
                        BinOp::Ge => (x >= y) as u32 as _,
                        BinOp::Eq => (x == y) as u32 as _,
                        BinOp::Neq => (x != y) as u32 as _,
                    }),
                    // 按钮状态不参与二元运算，只能用字段投影取它的分量。
                    _ => anyhow::bail!("invalid op on ButtonState"),
                }
            }
            // —— 函数调用：先求全部实参（任一失败即整体失败），再交给内建实现 ——
            Self::Func(_, func, inner) => {
                let vals = inner.iter().map(|it| it.eval(uml)).collect::<Result<Vec<_>>>()?;
                func(&vals)?
            }
        })
    }
}

/// 把实参切片转成定长数组，个数不符即报错。
/// 用 `const N: usize` 泛型是为了让错误信息里能带上期望的参数个数，
/// 省去为每个元数写一遍长度断言。
fn expect<const N: usize>(s: &[Var]) -> Result<[Var; N]> {
    s.try_into().map_err(|_| anyhow::anyhow!("expected {N} arguments"))
}

/// 校验实参列表非空，供 `max`/`min` 这类参数个数不定的函数使用。
/// 空列表下它们没有可返回的值（`try_fold` 的初始值是 ±∞，直接返回会得到无意义的
/// 极值），因此在进入归约之前就拒绝。
fn non_empty(s: &[Var]) -> Result<&[Var]> {
    if s.is_empty() {
        anyhow::bail!("expected arguments");
    }
    Ok(s)
}

/// 把一元 `f32 -> f32` 的函数包装成内建函数实现，自动完成
/// 「取 1 个实参 + 转成浮点 + 结果包回 `Var::Float`」这套样板工作。
fn wrap(f: fn(f32) -> f32) -> Function {
    Box::new(move |args| {
        let [arg] = expect::<1>(args)?;
        Ok(Var::Float(f(arg.float()?)))
    })
}

/// [`wrap`] 的二元版本，用于 `atan2`/`step` 这类需要两个参数的函数。
fn wrap2(f: fn(f32, f32) -> f32) -> Function {
    Box::new(move |args| {
        let [x, y] = expect::<2>(args)?;
        Ok(Var::Float(f(x.float()?, y.float()?)))
    })
}

/// 解析一个「原子」——表达式中不可再分的最小单元。
///
/// 支持的形态：
/// - 标识符：可能是函数调用（紧随 `(`）、变量字段访问（紧随 `.`），或普通变量引用；
/// - 数值字面量；
/// - `( expr )` 括号分组，只用于改变默认结合顺序，不产生新节点；
/// - `[x, y, w, h]` 矩形字面量，恰好 4 个以 `,` 分隔的表达式。
///
/// 已知局限：token 流耗尽与词法错误都会落到同一句「expected atom」上
/// （`transpose().ok().flatten()` 把 `Err` 一并吞成 `None`），报错时无法区分二者。
///
/// # Errors
/// token 流耗尽、括号/方括号不配对、函数名未定义、实参分隔符非法时返回错误字符串。
fn take_atom(lexer: &mut Lexer) -> Result<Expr, String> {
    Ok(match lexer.next().transpose().ok().flatten().ok_or_else(|| "expected atom".to_owned())? {
        // 标识符：必须先看后一个 token 才知道它是字段访问、函数调用还是变量引用，
        // 这正是 token 流需要 `Peekable` 的原因。
        Token::Ident(s) => match lexer.peek() {
            // `name.field`：变量字段访问，字段名同样来自标识符词法。
            Some(&Ok(Token::Period)) => {
                lexer.next();
                let Some(Ok(Token::Ident(f))) = lexer.next() else {
                    bail!("expected field")
                };
                RawExpr::VarSub(s, f).into()
            }
            // `name(...)`：内建函数调用。函数名在这里就完成校验并把实现绑定进 AST，
            // 于是逐帧求值不必再查函数表，拼错函数名也只会在解析期报一次错。
            Some(&Ok(Token::LBrace)) => {
                lexer.next();
                // 内建函数表。这些函数是给谱面/活动作者写动画与布局算式用的：
                // 时间驱动的位移/旋转用 `sin`/`cos`，缓动与钳制用 `step`/`clamp`，
                // 尺寸与取整用 `floor`/`ceil`/`round`/`abs`，向量求角用 `atan2`。
                // 元数约定：`wrap` = 1 个实参，`wrap2` = 2 个实参，
                // `max`/`min` 为可变参数（至少 1 个），`clamp` 固定 3 个。
                let (name, func) = match s.as_str() {
                    // 正弦，参数为弧度；常用于周期性动画 `sin(t)`。
                    "sin" => ("sin", wrap(f32::sin)),
                    // 余弦，参数为弧度；与 `sin` 配合做圆周运动。
                    "cos" => ("cos", wrap(f32::cos)),
                    // 正切，参数为弧度；接近 π/2 时会得到很大的值，慎用。
                    "tan" => ("tan", wrap(f32::tan)),
                    // 绝对值，常用于「抖动量」「偏离量」的取正。
                    "abs" => ("abs", wrap(f32::abs)),
                    // 自然指数 e^x；配 `ln` 可用于对数/指数缓动。
                    "exp" => ("exp", wrap(f32::exp)),
                    // 两参数反正切 `atan2(y, x)`：由两个分量求方向角，
                    // 参数顺序与 `f32::atan2` 一致（第一个是 y）。
                    "atan2" => ("atan2", wrap2(f32::atan2)),
                    // 自然对数；非正值会得到 -inf/NaN，调用方需自行保证输入为正。
                    "ln" => ("ln", wrap(f32::ln)),
                    // 符号函数，返回 -1.0 / 0.0 / 1.0；常用来取方向或做条件跳变。
                    "sig" => ("sig", wrap(f32::signum)),
                    // 阶跃：第一个参数小于第二个参数时返回 0.0，否则返回 1.0。
                    // 用于「达到阈值后立刻切换」的分段效果（如进度超过某点后显现元素）。
                    "step" => ("step", wrap2(|x, y| if x < y { 0.0 } else { 1.0 })),
                    // 向下取整，常用于把连续时间/坐标对齐到离散网格。
                    "floor" => ("floor", wrap(f32::floor)),
                    // 向上取整。
                    "ceil" => ("ceil", wrap(f32::ceil)),
                    // 四舍五入。
                    "round" => ("round", wrap(f32::round)),
                    // 可变参数取最大值（至少 1 个实参）；初值取 -∞ 以便逐个比较。
                    "max" => (
                        "max",
                        Box::new(|args: &[Var]| {
                            Ok(Var::Float(
                                non_empty(args)?
                                    .iter()
                                    .try_fold(f32::NEG_INFINITY, |mx, x| x.float().map(|it| it.max(mx)))
                                    .unwrap(),
                            ))
                        }) as Function,
                    ),
                    // 可变参数取最小值（至少 1 个实参）；初值取 +∞。
                    "min" => (
                        "min",
                        Box::new(|args: &[Var]| {
                            Ok(Var::Float(
                                non_empty(args)?
                                    .iter()
                                    .try_fold(f32::INFINITY, |mx, x| x.float().map(|it| it.min(mx)))
                                    .unwrap(),
                            ))
                        }) as Function,
                    ),
                    // `clamp(x, lo, hi)`：把 `x` 限制在 [lo, hi] 内。
                    // 常用于把动画进度归一化到 0..1，或防止布局越界。
                    "clamp" => (
                        "clamp",
                        Box::new(|args: &[Var]| {
                            let [x, lo, hi] = expect::<3>(args)?;
                            Ok(Var::Float(x.float()?.clamp(lo.float()?, hi.float()?)))
                        }) as Function,
                    ),
                    // 未知函数名直接拒绝：宁可解析期报错，也不要留到逐帧求值时才发现。
                    _ => bail!("unknown function: {s}"),
                };
                // 实参表至少 1 个：先无条件读掉第一个表达式，再按 `,` 续读直到 `)`。
                // 这样 `f()` 会因缺少原子而在解析期报错，而不是留到运行期变成空参数。
                let mut args = vec![take_expr(lexer)?];
                loop {
                    match lexer.next() {
                        Some(Ok(Token::Comma)) => {}
                        Some(Ok(Token::RBrace)) => break,
                        x => bail!("expected brace or comma, got {x:?}"),
                    }
                    args.push(take_expr(lexer)?);
                }
                RawExpr::Func(name, func, args).into()
            }
            // 后面既不是 `.` 也不是 `(`：普通的变量引用
            // （全局变量、`let` 变量，或前面某个元素 id 绑定的值）。
            _ => RawExpr::Var(s).into(),
        },
        // 数值字面量，直接成为常量节点。
        Token::Number(val) => RawExpr::Literal(val).into(),
        // `( expr )`：括号分组，只影响结合顺序，不产生额外节点。
        Token::LBrace => {
            let res = take_expr(lexer)?;
            let Some(Ok(Token::RBrace)) = lexer.next() else {
                bail!("expected right brace")
            };
            res
        }
        // `[x, y, w, h]`：矩形字面量。前三个分量用同一个闭包读取（每个都以 `,` 结尾），
        // 最后一个分量改由 `]` 收尾，因此这里重复了一次读取逻辑而没有复用闭包。
        Token::LBracket => {
            let mut one = || -> Result<Expr, String> {
                let res = take_expr(lexer)?;
                take(lexer, Token::Comma)?;
                Ok(res)
            };
            RawExpr::Rect([one()?, one()?, one()?, {
                let res = take_expr(lexer)?;
                take(lexer, Token::RBracket)?;
                res
            }])
            .into()
        }
        // 其余 token（运算符、逗号、方括号等）都不可能是原子：报错交由上层处理，
        // 使「表达式里出现多余符号」不会静默通过。
        x => bail!("expected atom, got {x:?}"),
    })
}

/// 尝试读取一个二元运算符。
/// 下一个 token 不是运算符时返回 `None` **且不消耗输入**——「探测但不消费」这一点是
/// [`take_expr`] 能靠 token 流自然收尾的前提：遇到 `)`、`,`、`]` 等表达式边界时，
/// 运算符循环直接结束，边界符号留给调用者处理。
///
/// 注意：这里对 `Peekable` 取出的一层结果调用 `unwrap`，若该 token 处于词法错误态
/// 会 panic（`nxt` 为 `None` 的情况已用 let-else 提前返回）。
fn take_op(lexer: &mut Lexer) -> Result<Option<BinOp>, String> {
    let Some(nxt) = lexer.peek() else { return Ok(None) };
    let res = match nxt.as_ref().unwrap() {
        Token::Add => BinOp::Add,
        Token::Sub => BinOp::Sub,
        Token::Mul => BinOp::Mul,
        Token::Div => BinOp::Div,
        Token::Lt => BinOp::Lt,
        Token::Le => BinOp::Le,
        Token::Gt => BinOp::Gt,
        Token::Ge => BinOp::Ge,
        Token::Eq => BinOp::Eq,
        Token::Neq => BinOp::Neq,
        _ => {
            return Ok(None);
        }
    };
    lexer.next();
    Ok(Some(res))
}

/// 用调度场算法（shunting-yard）把 token 序列解析成表达式树。
///
/// 为什么需要优先级：属性值与 `#>if` 条件都是作者手写的算式，
/// `x + w * 0.5` 必须按数学直觉理解为 `x + (w * 0.5)`，否则布局与动画结果会与
/// 作者预期不符。实现上维护两个栈——值栈 `vals` 与运算符栈 `ops`：
/// 遇到新运算符时先把栈上所有「优先级不低于它」的运算符归约成节点（保证同级
/// 左结合、高优先级先结合），把它压栈，再读下一个原子；循环结束后把剩余运算符
/// 自顶向下依次归约。
///
/// # Panics
/// 归约结束时值栈应恰好剩一个节点。若输入里出现两个相邻原子（例如 `x: 1 2`），
/// 运算符循环会立刻结束而值栈留下两个节点，此时触发 `panic!`。这个分支对
/// **作者可控的脚本输入**是可达的，因此畸形脚本会让此函数 panic 而非返回错误。
fn take_expr(lexer: &mut Lexer) -> Result<Expr, String> {
    // 先取一个原子作为值栈底。
    let mut vals = vec![take_atom(lexer)?];
    let mut ops: Vec<BinOp> = Vec::new();
    fn apply(vals: &mut Vec<Expr>, op: BinOp) {
        let y = vals.pop().unwrap();
        let x = vals.pop().unwrap();
        vals.push(RawExpr::BinOp(x, y, op).into());
    }
    // 主循环：每轮「归约栈顶 → 压入运算符 → 读下一个原子」。
    while let Some(op) = take_op(lexer)? {
        let pred = op.precedence();
        while let Some(last) = ops.last() {
            if last.precedence() <= pred {
                apply(&mut vals, *last);
                ops.pop();
            } else {
                break;
            }
        }
        ops.push(op);
        vals.push(take_atom(lexer)?);
    }
    // 收尾：把剩余的运算符按栈顶到栈底（即由紧到松）的顺序全部归约。
    while let Some(op) = ops.pop() {
        apply(&mut vals, op);
    }
    if vals.len() != 1 {
        panic!("invalid expression");
    }
    Ok(vals.into_iter().next().unwrap())
}

/// 把一段表达式源码解析成 AST。
/// 这是 [`Expr`] 反序列化与属性值回读的公共入口；每次调用都新建词法器，
/// 因为表达式之间彼此独立，不需要保留跨表达式的词法状态。
pub fn parse_expr(s: &str) -> Result<Expr, String> {
    take_expr(&mut Token::lexer(s).peekable())
}

// 表达式的「宽容解析」反序列化：属性值既能写成 JSON 数字（`size: 1`），
// 也能写成表达式字符串（`size: "1 + t"`），两者应当落到同一棵 AST。
// 之所以不用 serde 的 `untagged` 枚举，是因为数字与字符串都可以直接交给
// `deserialize_any`，由访问者按实际类型分派更直接，也便于把解析错误原样
// 透出成 serde 错误。
impl<'de> Deserialize<'de> for Expr {
    /// 按输入的实际形态分派：`f32`/`f64` 直接成为常量节点，字符串走 `parse_expr`。
    /// 注意经 `take_config` 链路时，数值一律先被 `to_string()` 成字符串，
    /// 因此 `visit_string`/`visit_str` 才是元素属性值的主路径。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;

        struct ExprVisitor;
        impl Visitor<'_> for ExprVisitor {
            type Value = Expr;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an expression")
            }

            fn visit_f32<E: de::Error>(self, value: f32) -> Result<Self::Value, E> {
                Ok(constant(value))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(constant(value as _))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                parse_expr(&value).map_err(E::custom)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_expr(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(ExprVisitor)
    }
}

/// 构造一个常量表达式节点，供各配置的默认值使用（如 `x` 默认 0、`rad` 默认 0）。
/// 默认值用常量节点而不是 `Option`，可以让渲染代码统一按「表达式一定存在」处理。
pub fn constant(val: f32) -> Expr {
    Box::new(RawExpr::Literal(val))
}

/// 按关键字分派并解析一个元素；token 流结束时返回 `Ok(None)`。
///
/// 关键字就是 UML 的元素类型：`p`（文本）、`img`（图片）、`col`（内嵌谱面集合）、
/// `r`（矩形）、`btn`（按钮）、`let`（变量绑定），以及 `#>` 前缀的变换类元素
/// `#>rot`/`#>tr`/`#>alpha`/`#>mat`/`#>pop`。
///
/// 两点设计说明：
/// - `let` 被做成「元素」而不是独立语法，是因为 UML 的顶层就是元素序列，
///   **变量绑定的生效位置由它在序列中的次序决定**：它只对排在自己后面的元素可见，
///   且每次渲染到该位置时才求值，因此可以被 `#>if` 包住实现条件赋值。
/// - 图片与谱面集合在构造时即发起异步加载（见 `Image::new`/`Collection::new`），
///   因此这里的解析只是建立元素对象，真正的数据要等渲染时任务完成才出现。
///
/// # Errors
/// 关键字不认识、配置块或（`p` 的）文本块缺失、词法错误时返回错误字符串。
pub fn take_element(icons: &Arc<Icons>, rank_icons: &[SafeTexture; 8], lexer: &mut Lexer) -> Result<Option<Box<dyn Element>>, String> {
    let Some(nxt) = lexer.next() else { return Ok(None) };
    let Token::Ident(ty) = nxt? else {
        bail!("expected element");
    };
    Ok(Some(match ty.as_str() {
        // 文本：先属性块，再整段文本块。
        "p" => Box::new(Text::new(take_config(lexer)?, take_text(lexer)?)),
        // 图片：`url` 必填，加载在构造时异步启动。
        "img" => Box::new(Image::new(take_config(lexer)?)),
        // 内嵌谱面集合：复用主界面的 `ChartsView`，因此外观与交互与谱面列表一致。
        "col" => Box::new(Collection::new(Arc::clone(icons), rank_icons.clone(), take_config(lexer)?)),
        // 填充矩形（可带圆角）。
        "r" => Box::new(RectElement::new(take_config(lexer)?)),
        // 可点击按钮，点击后可向场景提交一个 `action` 字符串。
        "btn" => Box::new(ButtonElement::new(take_config(lexer)?)),
        // 变量绑定：`let name = expr`。绑定的值由随后的 `render` 阶段写入变量表。
        "let" => {
            let Some(Ok(Token::Ident(id))) = lexer.next() else {
                bail!("expected variable name");
            };
            take(lexer, Token::Assign)?;
            Box::new(Assign::new(id, take_expr(lexer)?))
        }
        // 以下为变换类元素：它们在渲染时对后续元素施加作用域变换（压栈/出栈），
        // 自身不绘制任何内容。
        "#>rot" => Box::new(Rotation::new(take_config(lexer)?)),
        "#>tr" => Box::new(Translation::new(take_config(lexer)?)),
        "#>alpha" => Box::new(Alpha::new(take_config(lexer)?)),
        "#>mat" => Box::new(Mat::new(take_config(lexer)?)),
        "#>pop" => Box::new(Pop),
        _ => bail!("unknown element type: {}", ty),
    }))
}

/// UML 的顶层语句，一段脚本就是 [`TopLevel`] 的序列。
/// 渲染时按顺序逐条执行，因此**语句顺序即渲染顺序与作用范围**：变换类元素压栈后
/// 只影响排在它之后的元素，`let`/id 绑定同样只对其后的元素可见。
pub enum TopLevel {
    /// 一个可渲染、可交互的元素。
    Element(Box<dyn Element>),
    /// 全局变量定义 `global name = expr`。
    /// 它在 [`Uml::new`] 阶段被求值一次并标记为持久变量，之后跨帧保留，
    /// 不会被每帧的临时变量清理丢弃。
    GlobalDef(String, Expr),
    /// `#>if <expr>`：条件块开始。条件在**渲染时**求值（非 0 为真），
    /// 因此同一个条件块会随时间和交互在不同帧表现出不同结果。
    If(Expr),
    /// `#>else`：与最近的 `#>if`/`#>elif` 配对。
    Else,
    /// `#>elif <expr>`：链式分支，只有前序分支均未命中时才求值。
    ElseIf(Expr),
    /// `#>fi`：结束当前条件块。
    EndIf,
}

/// 从 token 流读取一个顶层语句；流结束时返回 `Ok(None)`。
///
/// 顶层只有四类：全局变量定义、条件指令、元素、旧版兼容标记 `#>if-no-v2`。
/// 条件指令在这里只被**识别与包装**，不求值——求值推迟到渲染阶段（见 [`Uml::render`]），
/// 因为条件依赖的变量（时间、`@btn`、前面元素绑定的变量）要逐帧变化。
///
/// `#>if-no-v2` 的处理是「丢掉标记本身，然后把它后面的第一个语句照常返回」，
/// 于是该标记相当于一行空操作，历史脚本不必改写成 `#>if true` 也能继续运行。
///
/// 注意这里用 `peek` 而非 `next` 预读：只有在确定分支后才消费 token，
/// 这样各分支的失败不会让 token 流错位。
///
/// # Errors
/// 变量名、`=`、表达式缺失，或遇到词法错误时返回错误字符串。
pub fn take_top_level(icons: &Arc<Icons>, rank_icons: &[SafeTexture; 8], lexer: &mut Lexer) -> Result<Option<TopLevel>, String> {
    let Some(nxt) = lexer.peek() else { return Ok(None) };
    Ok(match nxt {
        // `global name = expr`：读变量名与 `=`，再解析表达式。
        Ok(Token::Global) => {
            lexer.next();
            let Some(Ok(Token::Ident(id))) = lexer.next() else {
                bail!("expected variable name");
            };
            take(lexer, Token::Assign)?;
            // 特例：`global x = @btn` 直接构造按钮状态初值。
            // 若走通用表达式路径，`@btn` 会被当成普通变量名而查不到值，
            // 因此这里显式识别它并给出 `ButtonState::default()`。
            if let Some(Ok(Token::Ident(ident))) = lexer.peek() {
                if ident == "@btn" {
                    lexer.next();
                    return Ok(Some(TopLevel::GlobalDef(id, Box::new(RawExpr::ButtonState(ButtonState::default())))));
                }
            }
            Some(TopLevel::GlobalDef(id, take_expr(lexer)?))
        }
        // `#>if-no-v2`：跳过标记，并把它后面的一个语句照常返回。
        Ok(Token::IfNoV2) => {
            lexer.next();
            take_top_level(icons, rank_icons, lexer)?;
            return take_top_level(icons, rank_icons, lexer);
        }
        // 条件指令：只包装不求值。
        Ok(Token::If) => {
            lexer.next();
            Some(TopLevel::If(take_expr(lexer)?))
        }
        Ok(Token::Else) => {
            lexer.next();
            Some(TopLevel::Else)
        }
        Ok(Token::EndIf) => {
            lexer.next();
            Some(TopLevel::EndIf)
        }
        Ok(Token::ElseIf) => {
            lexer.next();
            Some(TopLevel::ElseIf(take_expr(lexer)?))
        }
        // 其余情况交给元素分派。
        Ok(_) => take_element(icons, rank_icons, lexer)?.map(TopLevel::Element),
        // 词法错误原样上抛为字符串。
        Err(err) => return Err(err.to_string()),
    })
}

/// UML 的总入口：源码文本 → 可渲染的 [`Uml`] 实例。
///
/// 流程分三段：
/// 1. **词法**：把整段文本交给 `logos` 词法器，并包成可回看的 `Peekable`；
/// 2. **语法**：循环调用 [`take_top_level`] 收集顶层语句，其中
///    [`TopLevel::GlobalDef`] 被单独抽出——全局变量必须在任何元素渲染之前先建立，
///    且不参与渲染顺序；
/// 3. **初始化**：用剩余语句与全局定义构造 [`Uml`]，此时全局定义被求值一次并
///    标记为持久。
///
/// # Errors
/// 返回的错误是面向脚本作者的可读字符串。调用方（`EventScene`）据此放弃渲染
/// 或退回空的 `Uml`，而不会让整个页面崩溃——服务端脚本属于外部输入，必须按
/// 不可信数据处理。
pub fn parse_uml(s: &str, icons: &Arc<Icons>, rank_icons: &[SafeTexture; 8]) -> Result<Uml, String> {
    // 1) 词法：整段文本一次词法化，包成 `Peekable` 以支持前看。
    let mut lexer = Token::lexer(s).peekable();
    // 2) 语法：逐个读出顶层语句，并把全局定义与待渲染语句分流到两个列表。
    let mut elements = Vec::new();
    let mut global_defs = Vec::new();
    while let Some(top) = take_top_level(icons, rank_icons, &mut lexer)? {
        if let TopLevel::GlobalDef(id, expr) = top {
            global_defs.push((id.clone(), expr));
        } else {
            elements.push(top);
        }
    }
    // 3) 初始化：全局定义在这一步就被求值（此时尚无任何元素绑定），
    //    因此全局变量的初值不能引用元素 id。
    Uml::new(elements, &global_defs).map_err(|it| it.to_string())
}
