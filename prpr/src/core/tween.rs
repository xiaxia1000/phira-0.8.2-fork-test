//! 缓动函数与插值原语。
//!
//! 缓动用编号 [`TweenId`]（`u8`）索引，编号规则由 [`easing_from`] 固定：
//! `major as u8 * 3 + minor as u8`，即 [`TweenMajor`] 的每种曲线占用连续 3 个编号，
//! 依次是 [`TweenMinor`] 的 In / Out / InOut。因此 [`TWEEN_FUNCTIONS`] 与
//! [`INT_TWEEN_FUNCTIONS`] 都是长度 33 的表：下标 0、1、2 是三个特殊项——常数 0
//! （无缓动，取值停在区间左端）、常数 1（立刻跳到右端）、线性 `x`；
//! 其后每 3 个一组对应一种曲线的三种变体。区间取用哪一条由关键帧的 `tween` 决定，
//! 见 [`Anim`](crate::core::Anim)。
//!
//! 表里存的是裸函数指针（无捕获、可静态初始化），再配合 `Rc` 池把
//! `Rc<dyn TweenFunction>` 在关键帧之间共享，避免每个关键帧重复构造缓动对象。
//!
//! 本模块另有几类特殊实现：整数积分表 [`INT_TWEEN_FUNCTIONS`]（需要“缓动的积分”
//! 作为动画值时使用）、区间夹取包装 [`ClampedTween`] / [`IntClampedTween`]、
//! 以及对任意缓动做数值积分的 [`GeneralIntTween`]。
use macroquad::prelude::{vec2, Color, Rect, Vec2};
use once_cell::sync::Lazy;
use std::{any::Any, ops::Range, rc::Rc};

/// 缓动编号：`u8` 足以覆盖 33 个内置缓动，且体积小、可直接内联进关键帧。
///
/// 编号到具体函数的映射见 [`easing_from`] 与 [`TWEEN_FUNCTIONS`]。
pub type TweenId = u8;

/// 圆周率的 `f32` 简写；各缓动公式里频繁出现，短名字可读性更好。
const PI: f32 = std::f32::consts::PI;

/// In 变体：不加修饰，缓动函数本身即“缓入”（起点慢、终点快）。
///
/// 这类曲线在 `x = 0` 附近导数很小，用来表现“从静止缓缓起步”。
macro_rules! f1 {
    ($fn:ident) => {
        $fn
    };
}

/// Out 变体：由 In 变体按 `f_out(x) = 1 - f_in(1 - x)` 反演得到。
///
/// 该式保证 `f_out(0) = 0`、`f_out(1) = 1`（与 In 变体共享端点），
/// 同时把“起点慢”翻转成“终点慢”，即 ease-out 的语义。
macro_rules! f2 {
    ($fn:ident) => {
        |x| (1. - $fn(1. - x))
    };
}

/// InOut 变体：把 In 变体在时间轴上压半，再前后镜像拼接。
///
/// 令 `u = 2x`：前半段 `u < 1` 取 `f_in(u) / 2`（由 0 加速到 0.5），
/// 后半段取 `1 - f_in(2 - u) / 2`（对称地减速到 1）。结果是两端导数均为 0、
/// 中段最快的曲线，整体关于中心点 `(0.5, 0.5)` 对称。
macro_rules! f3 {
    ($fn:ident) => {
        |x| {
            let x = x * 2.;
            if x < 1. {
                $fn(x) / 2.
            } else {
                1. - $fn(2. - x) / 2.
            }
        }
    };
}

/// ease-in-sine：`f(x) = 1 - cos(x·π/2)`。
///
/// 由余弦的半周期改写而来，`f(0) = 0`、`f(1) = 1`，且在 `x = 0` 处导数为 0、
/// `x = 1` 处导数为 π/2 ≈ 1.571，属于“温和”缓动中变化率最小的一档。
#[inline]
fn sine(x: f32) -> f32 {
    1. - ((x * PI) / 2.).cos()
}

/// ease-in-quad：`f(x) = x²`，二次多项式，起步加速度恒定。
#[inline]
fn quad(x: f32) -> f32 {
    x * x
}

/// ease-in-cubic：`f(x) = x³`，比 quad 更平滑，是最常用的默认缓动档位。
#[inline]
fn cubic(x: f32) -> f32 {
    x * x * x
}

/// ease-in-quart：`f(x) = x⁴`，起步更“迟滞”，中后段加速明显。
#[inline]
fn quart(x: f32) -> f32 {
    x * x * x * x
}

/// ease-in-quint：`f(x) = x⁵`，本组多项式缓动里起步最迟缓的一档。
#[inline]
fn quint(x: f32) -> f32 {
    x * x * x * x * x
}

/// ease-in-expo：`f(x) = 2^(10(x-1))`。
///
/// 指数缓动。注意在 `x = 0` 处取值为 `2^-10 ≈ 0.00098` 而非严格 0
/// （沿用 CSS 的经典近似，避免用分段函数在 0 处单独取 0），
/// 因此起点附近会有一个极小的跳变；`x = 1` 处精确为 1。
#[inline]
fn expo(x: f32) -> f32 {
    (2.0_f32).powf(10. * (x - 1.))
}

/// ease-in-circ：`f(x) = 1 - sqrt(1 - x²)`。
///
/// 即单位圆上从 `(0, 1)` 到 `(1, 0)` 的圆弧投影，`x = 1` 处导数发散，
/// 因此收尾极快，常用于表现“射出”类的运动。
#[inline]
fn circ(x: f32) -> f32 {
    1. - (1. - x * x).sqrt()
}

/// ease-in-back：`f(x) = (C3·x - C1)·x²`，`C1 = 1.70158`、`C3 = C1 + 1`。
///
/// 先向反方向回撤再前冲（“助跑”效果）。`C1 = 1.70158` 是 CSS 缓动规范中
/// 过冲约 10% 的经典系数，`C3 = C1 + 1` 用于保证 `f(1) = 1`。
/// 重要性质：**该曲线非单调**（中段会超过 1 或低于 0），所以不能假定
/// “缓动值随 x 单调递增”，`ClampedTween` 上方的 TODO 正是指这一点。
#[inline]
fn back(x: f32) -> f32 {
    // C1 决定过冲幅度，C3 = C1 + 1 令 f(1) = 1（端点归一化）。
    const C1: f32 = 1.70158;
    const C3: f32 = C1 + 1.;
    (C3 * x - C1) * x * x
}

/// ease-in-elastic：`f(x) = -(2^(10x-10) · sin((10x - 10.75)·C4))`，`C4 = 2π/3`。
///
/// 指数包络乘正弦，产生“来回振荡后归位”的弹簧效果；`C4 = 2π/3` 为 CSS 缓动
/// 规范的相位系数，使 `f(1) = 1`（振荡在终点恰好收敛到 1）。
/// 与 expo 相同，`x = 0` 处是近似 0 而非精确 0。
#[inline]
fn elastic(x: f32) -> f32 {
    // C4 为振荡相位：2π/3 使 x = 1 时正弦项归零，曲线收在 1。
    const C4: f32 = (2. * PI) / 3.;
    -((2.0_f32).powf(10. * x - 10.) * ((x * 10. - 10.75) * C4).sin())
}

/// ease-in-bounce：分段抛物线拼成的“落地弹跳”曲线。
///
/// 实现方式是先做 `x -> 1 - x` 的反演，再取 `1 - (...)`，等价于 bounce-out。
/// `N1 = 7.5625`、`D1 = 2.75` 沿用 CSS 缓动规范的经典常数：断点
/// `1/D1`、`2/D1`、`2.5/D1`、`2.625/D1` 把区间切成四段，每段是一条开口相同的
/// 抛物线，顶点分别落在 0.75、0.9375、0.984375 的“反弹高度”上，
/// 于是每次弹起都比上一次矮，形成自然的衰减弹跳。
#[inline]
fn bounce(x: f32) -> f32 {
    // N1 为抛物线开口系数，D1 决定四段断点位置；二者共同保证段间连续。
    const N1: f32 = 7.5625;
    const D1: f32 = 2.75;

    // 反演自变量，使下面的分段表达式成为 bounce-out。
    let x = 1. - x;
    1. - (if x < 1. / D1 {
        N1 * x.powi(2)
    } else if x < 2. / D1 {
        N1 * (x - 1.5 / D1).powi(2) + 0.75
    } else if x < 2.5 / D1 {
        N1 * (x - 2.25 / D1).powi(2) + 0.9375
    } else {
        N1 * (x - 2.625 / D1).powi(2) + 0.984375
    })
}

/// 33 个内置缓动的查找表，下标即 [`TweenId`]（见 [`easing_from`]）。
///
/// 布局为每行三列 In / Out / InOut：
/// ```text
///  0..3 :  |_| 0.   |_| 1.   |x| x     —— 无缓动（恒取左端）/ 立即跳变 / 线性
///  3..6 :  sine    (In, Out, InOut)
///  6..9 :  quad
///  9..12:  cubic
/// 12..15:  quart
/// 15..18:  quint
/// 18..21:  expo
/// 21..24:  circ
/// 24..27:  back
/// 27..30:  elastic
/// 30..33:  bounce
/// ```
/// 下标 0 与 1 是“阶跃”语义：0 让区间内取值恒为左端关键帧的值（等价于不插值），
/// 1 让取值立刻等于右端值；两者都是常量函数，与 x 无关。
/// 表元素是裸函数指针，可静态初始化且无分配；`#[rustfmt::skip]` 仅用于保住表格排版。
#[rustfmt::skip]
pub static TWEEN_FUNCTIONS: [fn(f32) -> f32; 33] = [
	|_| 0.,			|_| 1.,			|x| x,
	/* In */		/* Out */		/* InOut */
	f1!(sine),		f2!(sine),		f3!(sine),
	f1!(quad),		f2!(quad),		f3!(quad),
	f1!(cubic),		f2!(cubic),		f3!(cubic),
	f1!(quart),		f2!(quart),		f3!(quart),
	f1!(quint),		f2!(quint),		f3!(quint),
	f1!(expo),		f2!(expo),		f3!(expo),
	f1!(circ),		f2!(circ),		f3!(circ),
	f1!(back),		f2!(back),		f3!(back),
	f1!(elastic),	f2!(elastic),	f3!(elastic),
	f1!(bounce),	f2!(bounce),	f3!(bounce),
];

// 缓动的 `Rc` 池：`StaticTween(i)` 只是“编号包装”，本身零状态，但会被成千上万个
// 关键帧以 `Rc<dyn TweenFunction>` 形式共享。这里按线程预分配 33 个 `Rc`，
// 关键帧构造时只做一次引用计数增加（见 `StaticTween::get_rc`），
// 避免逐关键帧新建装箱对象；`Lazy` 把实际分配推迟到首次使用。
// 必须用 `thread_local` 而不能用全局静态：`Rc` 非 `Send`/`Sync`，无法跨线程共享。
thread_local! {
    static TWEEN_FUNCTION_RCS: Lazy<Vec<Rc<dyn TweenFunction>>> = Lazy::new(|| {
        (0..33)
            .map(|it| -> Rc<dyn TweenFunction> { Rc::new(StaticTween(it)) })
            .collect()
    });
}

/// In 变体积分的直接引用：`I_in(x) = ∫_0^x f_in(u) du`，由各 `int_*` 函数实现。
macro_rules! i1 {
    ($fn:ident) => {
        $fn
    };
}

/// Out 变体积分：由 `f_out(x) = 1 - f_in(1 - x)` 积分可得
/// `I_out(x) = x + I_in(1 - x) - I_in(1)`。
///
/// 推导：`I_out(x) = ∫_0^x (1 - f_in(1-u)) du = x - (I_in(1) - I_in(1-x))`。
/// 注意结果依赖 `I_in(1)`（整段面积），因此 `int_*` 必须实现为“从 0 起算”的原函数。
macro_rules! i2 {
    ($fn:ident) => {
        // I(x) = x + \int_1^{1-x} f(u)du = x + I(1-x) - I(1)
        |x| x + $fn(1. - x) - $fn(1.)
    };
}

/// InOut 变体积分：分段积分并保持 `I(0) = 0`、关于中点连续。
///
/// 推导：`f_inout` 在 `u < 0.5` 段是 `f_in(2u)/2`，故
/// `I(x) = I_in(2x)/4`（`x <= 0.5`）；`x > 0.5` 时，前一半积出 `I_in(1)/4`，
/// 后一半再对 `1 - f_in(2-2u)/2` 积分并代换 `v = 2-2u`，两项合并后
/// 恰好抵消 `I_in(1)/4`，得到 `I(x) = x - 0.5 + I_in(2 - 2x)/4`。
macro_rules! i3 {
    ($fn:ident) => {
        |x| {
            let x2 = x * 2.;
            if x2 < 1. {
                $fn(x2) / 4.
            } else {
                x - 0.5 + $fn(2. - x2) / 4.
            }
        }
    };
}

/// `sine` 的积分：`I(x) = x - sin(x·π/2)·(2/π)`。
///
/// 对 `1 - cos(x·π/2)` 逐项积分得到；`I(0) = 0`，`I(1) = 1 - 2/π ≈ 0.363`。
#[inline]
fn int_sine(x: f32) -> f32 {
    // f(x) = 1 - cos(x * PI / 2)
    // I(x) = x - sin(x * PI / 2) * (2 / PI)
    x - (x * PI / 2.).sin() * (2. / PI)
}

/// `quad` 的积分：`I(x) = x³/3`（`x³/3` 求导即 `x²`）。
#[inline]
fn int_quad(x: f32) -> f32 {
    x.powi(3) / 3.
}

/// `cubic` 的积分：`I(x) = x⁴/4`。
#[inline]
fn int_cubic(x: f32) -> f32 {
    x.powi(4) / 4.
}

/// `quart` 的积分：`I(x) = x⁵/5`。
#[inline]
fn int_quart(x: f32) -> f32 {
    x.powi(5) / 5.
}

/// `quint` 的积分：`I(x) = x⁶/6`。
#[inline]
fn int_quint(x: f32) -> f32 {
    x.powi(6) / 6.
}

/// `expo` 的积分：`I(x) = (2^(10x-10) - 2^-10) / (10·ln2)`。
///
/// 由 `∫2^(10u-10)du = 2^(10x-10)/(10·ln2)` 得到，再减去 `x = 0` 处的值
/// `2^-10/(10·ln2)` 使 `I(0) = 0`（原函数必须以 0 为起点，否则 `i2!` 的
/// “`I(1)` 即整段面积”这一前提不成立）。
#[inline]
fn int_expo(x: f32) -> f32 {
    // f(x) = 2^(10x - 10)
    // I(x) = (2^(10x - 10) - 2^(-10)) / (10 * ln(2))
    let ln2 = std::f32::consts::LN_2;
    ((2.0_f32).powf(10. * x - 10.) - (2.0_f32).powf(-10.)) / (10. * ln2)
}

/// `circ` 的积分：`I(x) = x - 0.5·(x·sqrt(1-x²) + arcsin x)`。
///
/// 后半项是 `∫sqrt(1-u²)du` 的标准结果，几何上等于“单位圆四分之一扇形加三角形”
/// 的面积；从 `x` 中减去它即得到对 `1 - sqrt(1-x²)` 的积分。
#[inline]
fn int_circ(x: f32) -> f32 {
    // f(x) = 1 - sqrt(1 - x^2)
    // I(x) = x - 0.5 * (x * sqrt(1 - x^2) + arcsin(x))
    x - 0.5 * (x * (1. - x * x).sqrt() + x.asin())
}

/// `back` 的积分：`I(x) = (C3/4·x - C1/3)·x³`。
///
/// 对 `(C3·x - C1)·x² = C3·x³ - C1·x²` 逐项积分，常数与 `back` 保持一致。
/// 由于 `back` 非单调，该原函数在某些区间会“倒退”（导数局部为负），
/// 这也是整数缓动在 back/elastic 上与直觉不符的原因。
#[inline]
fn int_back(x: f32) -> f32 {
    // 与 back 使用同一组过冲系数，保证 I'(x) = back(x)。
    const C1: f32 = 1.70158;
    const C3: f32 = C1 + 1.;
    // f(x) = C3 * x^3 - C1 * x^2
    // I(x) = (C3/4 * x - C1/3) * x^3
    (C3 * x / 4. - C1 / 3.) * x * x * x
}

/// `elastic` 的积分：用原函数在两端相减实现，保证 `I(0) = 0`。
///
/// 被积函数形如 `2^(10x-10)·sin(v)`，即 `e^{a·u}·sin(v)`（`a = ln2`，
/// `u = 10x - 10`，`v = (10x - 10.75)·b`，`b = C4`）。其原函数为
/// `e^{a·u}·(a·sin v - b·cos v)/(a² + b²)`，再乘上链式法则的 `1/10`
/// 与整体取负（来自 elastic 表达式的负号）。因为形如 `sin(b·u + φ)` 的相位
/// 常数项在求导后消失，直接用该原函数即可。
#[inline]
fn int_elastic(x: f32) -> f32 {
    // 内层原函数：只保证求导等于被积函数，常数项由外层相减补齐。
    #[inline]
    fn elastic_f_antideriv(x: f32) -> f32 {
        const C4: f32 = (2. * PI) / 3.;
        // a 为指数部分在对数域的斜率：2^(10x) = e^(10x·ln2)。
        let a = std::f32::consts::LN_2;
        let b = C4;
        let u = 10. * x - 10.;
        let v = (x * 10. - 10.75) * b;

        -((2.0_f32).powf(u) / (10. * (a * a + b * b))) * (a * v.sin() - b * v.cos())
    }
    // 减去 x = 0 处的取值，把积分起点归到 0。
    elastic_f_antideriv(x) - elastic_f_antideriv(0.)
}

/// `bounce` 的积分：分段原函数拼接后表达为 `x - B(1) + B(1 - x)`。
///
/// 推导：bounce 实现等价于 bounce-out，即 `1 - P(1 - x)`（`P` 为分段抛物线）。
/// 于是 `I(x) = x - ∫_{1-x}^{1} P(v)dv = x - (B(1) - B(1 - x))`，其中 `B` 是 `P` 的
/// 原函数。`B` 需要分段拼接：每段抛物线单独积分后，用 `c2/c3/c4` 修正常数，
/// 使原函数在断点 `1/D1`、`2/D1`、`2.5/D1` 处连续（`val1..val3` 即`B`的累计值）；
/// 这些常数由“段起点处新旧原函数取值相等”逐个确定。
#[inline]
fn int_bounce(x: f32) -> f32 {
    // 分母为 0.75、0.9375、0.984375 的抛物线段的原函数（每段开口同为 N1）。
    #[inline]
    fn bounce_h(u: f32) -> f32 {
        const N1: f32 = 7.5625;
        const D1: f32 = 2.75;

        // 第一段的原函数：∫N1·u² du。
        let h1 = |u: f32| N1 / 3. * u.powi(3);
        let end1 = 1. / D1;
        let val1 = h1(end1);

        // 第二段的原函数：抛物线顶点平移 1.5/D1，另有 0.75 的常数项。
        let h2 = |u: f32| N1 / 3. * (u - 1.5 / D1).powi(3) + 0.75 * u;
        let end2 = 2. / D1;
        let c2 = val1 - h2(end1);
        let val2 = h2(end2) + c2;

        // 第三段：顶点平移 2.25/D1，常数项 0.9375。
        let h3 = |u: f32| N1 / 3. * (u - 2.25 / D1).powi(3) + 0.9375 * u;
        let end3 = 2.5 / D1;
        let c3 = val2 - h3(end2);
        let val3 = h3(end3) + c3;

        // 第四段：顶点平移 2.625/D1，常数项 0.984375。
        let h4 = |u: f32| N1 / 3. * (u - 2.625 / D1).powi(3) + 0.984375 * u;
        let c4 = val3 - h4(end3);

        if u < end1 {
            h1(u)
        } else if u < end2 {
            h2(u) + c2
        } else if u < end3 {
            h3(u) + c3
        } else {
            h4(u) + c4
        }
    }

    // B(0) = 0（首段从 0 积起），故这里直接用 B 的差值表达积分。
    x - bounce_h(1.) + bounce_h(1. - x)
}

/// 33 个“缓动积分”函数的查找表，下标与 [`TWEEN_FUNCTIONS`] 一一对应。
///
/// 用途：某些动画量的物理来源是缓动的**积分**（例如“速度按缓动变化时累计的位移”），
/// 或判定线高度 `h(t) = ∫ v(t) dt` 这类场合，直接把积分结果作为动画值即可，
/// 无需在运行时做数值积分。三项特殊值也沿用同样的对应关系：
/// 0 → 恒 0，1 → 恒 `x`（单位速度的位移），2 → `x²/2`（线性加速的位移）。
///
/// 注意 Out / InOut 变体的积分并不等于 In 变体的积分，而是由 `i2!` / `i3!`
/// 的两组恒等式换算得到，保证 `I(0) = 0` 且与原曲线的物理含义一致。
#[rustfmt::skip]
pub static INT_TWEEN_FUNCTIONS:[fn(f32) -> f32; 33] =[
    |_| 0.,				|x| x,			|x| x * x / 2.,
    /* In */			/* Out */			/* InOut */
    i1!(int_sine),		i2!(int_sine),		i3!(int_sine),
    i1!(int_quad),		i2!(int_quad),		i3!(int_quad),
    i1!(int_cubic),		i2!(int_cubic),		i3!(int_cubic),
    i1!(int_quart),		i2!(int_quart),		i3!(int_quart),
    i1!(int_quint),		i2!(int_quint),		i3!(int_quint),
    i1!(int_expo),		i2!(int_expo),		i3!(int_expo),
    i1!(int_circ),		i2!(int_circ),		i3!(int_circ),
    i1!(int_back),		i2!(int_back),		i3!(int_back),
    i1!(int_elastic),	i2!(int_elastic),	i3!(int_elastic),
    i1!(int_bounce),	i2!(int_bounce),	i3!(int_bounce),
];

// 整数积分缓动的 `Rc` 池，与上面的缓动池同构：同样是每线程 33 个共享 `Rc`，
// 供 `IntStaticTween::get_rc` 以引用计数方式复用。
thread_local! {
    static INT_TWEEN_FUNCTION_RCS: Lazy<Vec<Rc<dyn TweenFunction>>> = Lazy::new(|| {
        (0..33)
            .map(|it| -> Rc<dyn TweenFunction> { Rc::new(IntStaticTween(it)) })
            .collect()
    });
}

/// 一维缓动函数：把归一化进度 `x ∈ [0, 1]` 映射为缓动后的比例 `y`。
///
/// 内置实现都满足 `f(0) = 0`、`f(1) = 1`（back/elastic 在中段可能越出 `[0, 1]`，
/// 但端点精确），接口本身不强制这一点，调用方需自行保证。
/// 关键帧以 `Rc<dyn TweenFunction>` 持有它，以便共享与动态派发。
pub trait TweenFunction {
    /// 求 `x` 处的缓动值；`x` 一般已归一化到 `[0, 1]`，区间外的行为不作保证。
    fn y(&self, x: f32) -> f32;
    /// 向下转型为 `dyn Any`，用于从 `Rc<dyn TweenFunction>` 取回具体类型
    /// （例如序列化时要区分 [`StaticTween`] 与 [`BezierTween`]）。
    fn as_any(&self) -> &dyn Any;

    /// 数值求导，默认用**中心差分** `(y(r) - y(l)) / (r - l)` 近似。
    ///
    /// 为什么用差分而非解析导数：缓动统一以函数指针 / trait 对象持有，
    /// 接口无法要求每个实现都提供解析导数；中心差分的截断误差为 O(eps²)，
    /// 在 `eps = 1e-6` 下足以满足动画精度。
    ///
    /// `l`、`r` 分别被钳在 `[1e-7, 1 - 1e-7]`：端点处单侧取样会越界
    /// （例如 `circ` 在 `x = 1` 处导数发散），钳制后仍在区间内取样。
    /// 若钳制导致两侧重合（`x` 恰在端点）则直接返回 0，避免除零得到 NaN。
    fn derivative(&self, x: f32) -> f32 {
        let eps = 1e-6;
        let l = (x - eps).max(1e-7);
        let r = (x + eps).min(1. - 1e-7);
        if r <= l {
            return 0.;
        }
        (self.y(r) - self.y(l)) / (r - l)
    }
}

/// 通过 [`TweenId`] 间接调用 [`TWEEN_FUNCTIONS`] 的零状态缓动。
///
/// 内部只保存一个编号（等价于 [`TWEEN_FUNCTIONS`] 的下标），因此构造与复制都极廉价；
/// 之所以还要包成 trait 对象，是为了与 `as_any` / `derivative` 等能力统一，
/// 并配合线程局部的 `Rc` 池共享实例。
pub struct StaticTween(pub TweenId);
// 实现语义：按下标直接查表，得到该编号对应的基础缓动。
impl TweenFunction for StaticTween {
    /// 查表调用：`TWEEN_FUNCTIONS[id](x)`。编号由构造侧保证在 `0..33` 内。
    fn y(&self, x: f32) -> f32 {
        TWEEN_FUNCTIONS[self.0 as usize](x)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// 共享实例的获取入口：从线程局部池里取同一个 `Rc`。
impl StaticTween {
    /// 取得编号 `tween` 对应的共享缓动实例（只增加引用计数，不新建对象）。
    ///
    /// # Panics
    /// `tween` 越界（`>= 33`）时索引越界 panic；编号来自谱面解析结果，应已校验。
    pub fn get_rc(tween: TweenId) -> Rc<dyn TweenFunction> {
        TWEEN_FUNCTION_RCS.with(|rcs| Rc::clone(&rcs[tween as usize]))
    }
}

/// 通过 [`TweenId`] 间接调用 `INT_TWEEN_FUNCTIONS` 的零状态缓动。
///
/// 与 [`StaticTween`] 结构相同，区别只在查的是“缓动的积分”表：
/// 求值结果是累积量（如位移、面积）而非瞬时比例。
pub struct IntStaticTween(pub TweenId);
// 实现语义：按下标查积分表，得到该编号对应缓动的积分。
impl TweenFunction for IntStaticTween {
    /// 查表调用：`INT_TWEEN_FUNCTIONS[id](x)`。
    fn y(&self, x: f32) -> f32 {
        INT_TWEEN_FUNCTIONS[self.0 as usize](x)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// 与 `StaticTween::get_rc` 对应，从积分缓动的线程局部池中取共享实例。
impl IntStaticTween {
    /// 取得编号 `tween` 对应的共享积分缓动实例。
    ///
    /// # Panics
    /// `tween` 越界（`>= 33`）时索引越界 panic。
    pub fn get_rc(tween: TweenId) -> Rc<dyn TweenFunction> {
        INT_TWEEN_FUNCTION_RCS.with(|rcs| Rc::clone(&rcs[tween as usize]))
    }
}

/// 把某条缓动的**积分**限制到子区间并归一化的包装。
///
/// 用途：RPE 谱面里事件只覆盖曲线的一部分区间（例如只取 0.2~0.8 段）时，
/// 需要把该子区间的积分重新映射回 `[0, 1]`，保证动画仍从 0 单调走到 1。
/// 与 [`ClampedTween`] 的区别是本类型用积分而非瞬时值，
/// 因此结果更平滑、更适合作为“位移/高度”类动画的量。
///
/// 归一化因子取“区间长度 × 值域宽度”，即用**矩形面积**近似整段面积：
/// 对单调增缓动可保证结果恰好落在 `[0, 1]` 且两端分别取 0 与 1
/// （因为曲线的平均高度介于两端值之间），对线性缓动则完全精确。
pub struct IntClampedTween {
    /// 原始缓动编号，用于查 [`INT_TWEEN_FUNCTIONS`]。
    tween_id: TweenId,
    /// 输入侧要映射的子区间。
    x_range: Range<f32>,
    /// 子区间两端的缓动取值 `[f(start), f(end))`，构造时预计算。
    y_range: Range<f32>,
    /// `I(f, x_range.start)`，构造时预计算，用于把积分原点平移到子区间起点。
    base: f32,
}
// 实现语义：先在子区间内插值定位，再用“积分差 - 起点基线”逼近区间面积占比。
impl TweenFunction for IntClampedTween {
    /// 把 `x ∈ [0, 1]` 映射到子区间后计算归一化积分。
    ///
    /// 计算式：`x' = lerp(x_range, x)`，
    /// `∫ = I(x') - I(start) - f(start)·(x' - start)`（即对 `f(u) - f(start)` 积分），
    /// 再除以 `(x_end - x_start)·(y_end - y_start)`。
    ///
    /// 退化处理：当 `y_range` 宽度为 0 或非有限值时（缓动在该区间取值不变、
    /// 数值异常等），归一化失去意义，直接返回 `x²/2`——即“线性加速的位移”，
    /// 保证结果单调且不产生 NaN/Inf。注意该退化分支在 `x = 1` 处只到 `0.5`，
    /// 与正常路径归一化到 1 的值域并不一致。
    fn y(&self, x: f32) -> f32 {
        let denom = self.y_range.end - self.y_range.start;
        if !denom.is_finite() || denom.abs() < 1e-8 {
            return x * x / 2.;
        }

        let x = f32::tween(&self.x_range.start, &self.x_range.end, x);
        let int = INT_TWEEN_FUNCTIONS[self.tween_id as usize](x) - self.base - self.y_range.start * (x - self.x_range.start);
        let scale = (self.x_range.end - self.x_range.start) * denom;
        int / scale
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// 构造入口：把只与子区间有关的量一次性算好。
impl IntClampedTween {
    /// 由缓动编号与输入子区间构造。
    ///
    /// 预计算 `y_range` 与 `base` 的原因：它们只依赖 `x_range` 与缓动本身，
    /// 而 [`IntClampedTween::y`] 会被每个实例每帧调用，构造期算好可省掉重复求值
    /// （尤其 `INT_TWEEN_FUNCTIONS` 里弹性/弹跳两项开销较大）。
    pub fn new(tween_id: TweenId, x_range: Range<f32>) -> Self {
        let tween = TWEEN_FUNCTIONS[tween_id as usize];
        let y_range = tween(x_range.start)..tween(x_range.end);
        let base = INT_TWEEN_FUNCTIONS[tween_id as usize](x_range.start);
        Self {
            tween_id,
            x_range,
            y_range,
            base,
        }
    }
}

/// 把缓动限制到子区间并线性归一化回 `[0, 1]` 的包装。
///
/// 三个字段依次为：缓动编号、输入（x）子区间、以及该子区间端点的缓动取值
/// `[f(start), f(end)]`。求值式为
/// `y(x) = (f(lerp(start, end, x)) - f(start)) / (f(end) - f(start))`。
/// 与 [`IntClampedTween`] 的差别是这里用瞬时值而非积分，开销更小。
///
/// 两个已知限制（见下方 TODO）：
/// 1. 除以 `f(end) - f(start)`，当缓动在子区间上取值不变时除零，结果会是 ±Inf 或 NaN；
/// 2. 该归一化假定缓动单调增，但 back/elastic 并非单调，因此结果可能越出 `[0, 1]`。
// TODO assuming monotone, but actually they're not (e.g. Back tween)
pub struct ClampedTween(pub TweenId, pub Range<f32>, pub Range<f32>);
// 实现语义：把输入折到子区间后套用原缓动，再按端点值差归一化到 [0, 1]。
impl TweenFunction for ClampedTween {
    /// 先 `lerp` 到子区间，再用缓动求值并线性拉伸到 `[0, 1]`。
    ///
    /// 归一化保证端点严格映射为 0 与 1（只要 `f(end) != f(start)`），
    /// 因此子区间缓动总能当作完整过渡使用；非单调缓动在中段仍可能过冲。
    fn y(&self, x: f32) -> f32 {
        (TWEEN_FUNCTIONS[self.0 as usize](f32::tween(&self.1.start, &self.1.end, x)) - self.2.start) / (self.2.end - self.2.start)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// 构造入口：把子区间端点处的缓动取值算进 `y_range` 缓存。
impl ClampedTween {
    /// 由缓动编号与输入子区间构造，并预计算 `y_range = f(range.start)..f(range.end)`。
    ///
    /// 预计算使得每帧求值只需一次缓动调用，省掉每次重算端点值。
    pub fn new(tween: TweenId, range: Range<f32>) -> Self {
        let f = TWEEN_FUNCTIONS[tween as usize];
        let y_range = f(range.start)..f(range.end);
        Self(tween, range, y_range)
    }
}

/// 对任意 [`TweenFunction`] 做**数值积分**的适配器。
///
/// 用途：需要某条自定义缓动的积分，但它没有解析原函数（典型是 [`BezierTween`]）。
/// 内部持有共享的缓动实例（`Rc`），由调用方提供，以便与关键帧复用同一实现。
pub struct GeneralIntTween(Rc<dyn TweenFunction>);

// 构造入口：透传被积缓动，不复制其内部状态。
impl GeneralIntTween {
    /// 包裹一个已有的共享缓动实例。
    pub fn new(tween: Rc<dyn TweenFunction>) -> Self {
        Self(tween)
    }
}

// 实现语义：用 3 点 Gauss-Legendre 求积计算 ∫_0^x f(u) du。
impl TweenFunction for GeneralIntTween {
    /// 3 点 Gauss-Legendre 求积：`∫_0^x f(u)du ≈ (x/2)·Σ wᵢ·f((x/2)(vᵢ+1))`。
    ///
    /// 推导：把区间 `[0, x]` 线性映射到标准区间 `[-1, 1]`，
    /// 变换为 `u = (x/2)(v + 1)`、`du = (x/2)dv`，于是积分化为对标准区间求积的
    /// `radius = x/2` 倍。节点 `v = 0, ±sqrt(0.6)`、权重 `8/9, 5/9, 5/9` 是
    /// 3 点公式的标准取值（对 3 次以下多项式精确，实际对 5 次以内也精确）。
    ///
    /// 只用 3 点、且节点/权重在函数内以常量写死，是性能取舍：本函数会在每帧为
    /// 每个使用它的动画实例求值，节点更多会成比例放大开销，而缓动函数本身足够光滑，
    /// 3 点的误差已远小于动画可感知的量级。`sqrt(0.6)` 直接写成字面量
    /// `0.7745967` 是为了避免运行时调用 `sqrt`（同时也便于与 bezier-easing 对齐）。
    fn y(&self, x: f32) -> f32 {
        // 标准区间 [-1, 1] 上的 3 个求积节点与对应权重。
        let sqrt_06: f32 = 0.7745967;
        let nodes: [f32; 3] = [-sqrt_06, 0.0, sqrt_06];
        let weights: [f32; 3] = [5.0 / 9.0, 8.0 / 9.0, 5.0 / 9.0];

        // [0, x] -> [-1, 1] 的缩放因子，也是最终积分的雅可比系数。
        let radius = x / 2.0;

        // 逐节点取样被积缓动并加权求和；节点 v 对应的实际 x 为 radius * (v + 1)。
        let sum: f32 = nodes
            .iter()
            .zip(weights.iter())
            .map(|(&vi, &wi)| {
                let sample_x = radius * (vi + 1.0);
                wi * self.0.y(sample_x)
            })
            .sum();

        // 乘上雅可比：∫_0^x = (x/2) · ∫_{-1}^{1}。
        radius * sum
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// https://github.com/gre/bezier-easing
// 以下常数全部取自 bezier-easing 的移植版本，取值差异见各自说明；
// 它们的组合决定了 x -> t 反解的精度与最坏情况下的开销。

/// 采样表长度：在参数 `t ∈ [0, 1]` 上均匀取 21 个点，预存对应的 `x(t)`。
///
/// 原实现取 11 点，这里加密到 21（步长 0.1 -> 0.05），使初值更接近真解，
/// 从而减少 Newton 迭代次数与进入二分兜底的概率。
const SAMPLE_TABLE_SIZE: usize = 21;
/// `t` 方向采样步长 `1 / (SAMPLE_TABLE_SIZE - 1) = 0.05`。
///
/// 采样表的第 i 项对应 `t = i * SAMPLE_STEP`（t 均匀，x 不均匀）。
const SAMPLE_STEP: f32 = 1. / (SAMPLE_TABLE_SIZE - 1) as f32;
/// Newton 迭代的启用阈值：初值处斜率小于它时改用二分。
///
/// 对应 bezier-easing 的 `NEWTON_MIN_SLOPE`；斜率太小会让 `diff / slope`
/// 放大误差、使迭代振荡甚至发散。
const NEWTON_MIN_STEP: f32 = 1e-3;
/// Newton-Raphson 的最大迭代次数（与 bezier-easing 一致）。
///
/// 取 4 是精度与开销的折中：从采样表得到的初值已经很准，通常 2~3 次即收敛。
const NEWTON_ITERATIONS: usize = 4;
/// 二分法的收敛阈值：`|x(t) - x|` 小于它即停止（与 bezier-easing 一致）。
///
/// 1e-7 接近 f32 在 [0, 1] 上的可分辨极限，再细化不会提升实际精度。
const SUBDIVISION_PRECISION: f32 = 1e-7;
/// 二分法的最大迭代次数（与 bezier-easing 一致），保证最坏情况有界。
const SUBDIVISION_MAX_ITERATION: usize = 10;
/// 判定“斜率视为 0”的阈值，用于避免除零并提前退出。
///
/// 与 [`NEWTON_MIN_STEP`] 不同：这里只要斜率小到不可用（含恰好为 0 的水平段）
/// 就直接接受初值，不再做任何迭代。
const SLOPE_EPS: f32 = 1e-7;

/// 三次贝塞尔缓动（等价于 CSS 的 `cubic-bezier`）。
///
/// 曲线由两个控制点 `p1 = (x1, y1)`、`p2 = (x2, y2)` 与隐含端点 `(0, 0)`、`(1, 1)`
/// 定义，以参数 `t` 表示为
/// `x(t) = 3(1-t)²t·x1 + 3(1-t)t²·x2 + t³`（y 同理）。
/// 求值需要解“给定 x 反求 t”，没有闭式解，因此采用
/// “采样表求初值 + Newton-Raphson 精化 + 二分兜底”的混合策略，
/// 见 [`BezierTween::t_for_x`]。移植自 <https://github.com/gre/bezier-easing>。
pub struct BezierTween {
    /// x 方向采样表：第 i 项为 `t = i * SAMPLE_STEP` 处的 `x(t)`。
    ///
    /// 用表而不是每次重新求值，是因为 `t_for_x` 每帧都要用初值定位区间。
    sample_table: [f32; SAMPLE_TABLE_SIZE],
    /// 第一个控制点；`x1` 应落在 `[0, 1]` 才能保证 x(t) 单调、反解唯一。
    pub p1: (f32, f32),
    /// 第二个控制点，约束同 `p1`。
    pub p2: (f32, f32),
}

// 实现语义：先由 x 反求参数 t，再用 y 方向的同一多项式求缓动值。
impl TweenFunction for BezierTween {
    /// 先调用 [`BezierTween::t_for_x`] 解出参数 `t`，再代入 y 方向多项式。
    ///
    /// 注意 y 方向不需要反解：缓动值直接是 `y(t)`，而 `y(0) = 0`、`y(1) = 1`
    /// 由隐含端点保证。
    fn y(&self, x: f32) -> f32 {
        Self::sample(self.p1.1, self.p2.1, self.t_for_x(x))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// 三次贝塞尔的多项式运算与 x -> t 反解；x、y 两个方向共用同一套静态方法，
// 只是传入对应坐标的控制点。
impl BezierTween {
    /// 把控制点的两个坐标展开为三次多项式 `B(t) = a·t³ + b·t² + c·t` 的系数。
    ///
    /// 推导：展开 `3(1-t)²t·x1 + 3(1-t)t²·x2 + t³` 并合并同类项得
    /// `c = 3x1`、`b = 3x2 - 6x1`、`a = 1 - 3x2 + 3x1`。
    /// 常数项恒为 0（曲线过原点），故不返回。
    #[inline]
    fn coefficients(x1: f32, x2: f32) -> (f32, f32, f32) {
        ((x1 - x2) * 3. + 1., x2 * 3. - x1 * 6., x1 * 3.)
    }

    /// 用 Horner 形式求多项式值：`((a·t + b)·t + c)·t`。
    ///
    /// 比逐项乘幂少两次乘法，且数值上更稳定；同一函数既用于 x 方向（反解）
    /// 也用于 y 方向（最终取值）。
    #[inline]
    fn sample(x1: f32, x2: f32, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(x1, x2);
        ((a * t + b) * t + c) * t
    }
    /// 多项式导数 `B'(t) = 3a·t² + 2b·t + c`，Newton 迭代用它作为分母。
    #[inline]
    fn slope(x1: f32, x2: f32, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(x1, x2);
        (a * 3. * t + b * 2.) * t + c
    }

    /// 从初值 `t` 出发做固定次数的 Newton-Raphson 迭代，逼近 `x(t) = x` 的解。
    ///
    /// 迭代式为 `t ← t - (x(t) - x) / x'(t)`。斜率 `<= SLOPE_EPS` 时立即返回当前 `t`：
    /// 此时曲线局部水平，除以斜率会把误差放大导致振荡或发散。
    /// 不判断收敛而固定迭代 [`NEWTON_ITERATIONS`] 次，是因为采样表给出的初值已经很好，
    /// 多做一次乘加比每次比较收敛更划算。
    ///
    /// # Returns
    /// 近似满足 `x(t) = x` 的参数 `t`。
    fn newton_raphson_iterate(x: f32, mut t: f32, x1: f32, x2: f32) -> f32 {
        for _ in 0..NEWTON_ITERATIONS {
            let slope = Self::slope(x1, x2, t);
            if slope <= SLOPE_EPS {
                return t;
            }
            let diff = Self::sample(x1, x2, t) - x;
            t -= diff / slope;
        }
        t
    }

    /// 在 `[l, r]` 上用二分法求 `x(t) = x` 的解，作为 Newton 失效时的兜底。
    ///
    /// 每步取中点并比较 `x(t)` 与目标：`x(t) > x` 说明解在左半，否则在右半。
    /// 停止条件是残差小于 [`SUBDIVISION_PRECISION`]，或达到
    /// [`SUBDIVISION_MAX_ITERATION`] 次（保证最坏情况有界）。
    /// 前提是 `x(t)` 在区间内单调增——控制点 x 分量位于 `[0, 1]` 时成立。
    ///
    /// # Returns
    /// 区间内满足精度要求的参数 `t`。
    fn binary_subdivide(x: f32, mut l: f32, mut r: f32, x1: f32, x2: f32) -> f32 {
        let mut t = (l + r) / 2.;
        for _ in 0..SUBDIVISION_MAX_ITERATION {
            let diff = Self::sample(x1, x2, t) - x;
            if diff.abs() <= SUBDIVISION_PRECISION {
                break;
            }
            if diff > 0. {
                r = t;
            } else {
                l = t;
            }
            t = (l + r) / 2.;
        }
        t
    }

    /// 给定 `x`，反解出贝塞尔参数 `t`（即 `x` 对应的“时间进度”）。
    ///
    /// 流程：
    /// 1. `x = 0` 或 `1` 直接返回——端点精确，避免舍入把端点推离 0/1；
    /// 2. 用采样表定位 `x` 所在区间（`id`），并在区间内按 `x` 的分布线性插值出初值；
    /// 3. 按初值处斜率分流：`<= SLOPE_EPS` 说明曲线水平、`x` 对 `t` 不敏感，
    ///    直接采用初值；`>= NEWTON_MIN_STEP` 用 Newton 快速收敛；
    ///    介于两者之间则退化为在采样区间内二分。
    ///
    /// 这种分流正是 bezier-easing 的策略：Newton 在斜率充足时最快，
    /// 而接近水平段时二分更稳健。
    ///
    /// # Panics
    /// 要求 `x ∈ [0, 1]`：`x > 1` 时 `id` 会被钳到末项，随后读取
    /// `sample_table[id + 1]` 越界 panic；`x == 1` 已由开头的判断提前返回。
    pub fn t_for_x(&self, x: f32) -> f32 {
        if x == 0. || x == 1. {
            return x;
        }
        // 定位到采样表区间；min 防止 x 接近 1 时下标溢出。
        let id = (x / SAMPLE_STEP) as usize;
        let id = id.min(SAMPLE_TABLE_SIZE - 1);
        // 在区间 [id, id+1] 内按 x 的比例插值，得到比均匀取样更准的初值。
        let dist = (x - self.sample_table[id]) / (self.sample_table[id + 1] - self.sample_table[id]);
        let init_t = SAMPLE_STEP * (id as f32 + dist);
        // 按斜率分流：太小 -> 接受初值；足够大 -> Newton；居中 -> 二分兜底。
        match Self::slope(self.p1.0, self.p2.0, init_t) {
            y if y <= SLOPE_EPS => init_t,
            y if y >= NEWTON_MIN_STEP => Self::newton_raphson_iterate(x, init_t, self.p1.0, self.p2.0),
            _ => Self::binary_subdivide(x, SAMPLE_STEP * id as f32, SAMPLE_STEP * (id + 1) as f32, self.p1.0, self.p2.0),
        }
    }

    /// 由两个控制点构造，并预计算 x 方向采样表。
    ///
    /// 采样表只在构造时算一次（`SAMPLE_TABLE_SIZE` 次多项式求值），
    /// 之后每次求值只做查表与少量迭代，符合“谱面加载一次、播放中反复查询”的用法。
    pub fn new(p1: (f32, f32), p2: (f32, f32)) -> Self {
        Self {
            sample_table: std::array::from_fn(|i| Self::sample(p1.0, p2.0, i as f32 * SAMPLE_STEP)),
            p1,
            p2,
        }
    }
}

/// 缓动的主类别，即曲线的“家族”。
///
/// `#[repr(u8)]` 让它直接参与编号计算：`major as u8 * 3 + minor as u8`，
/// 因此**声明顺序即编号顺序，不可随意调整**（改动会改变既有谱面的缓动语义，
/// 也会让 [`TWEEN_FUNCTIONS`] 的排布对不上）。每一类在表中占 3 个连续下标。
#[repr(u8)]
pub enum TweenMajor {
    /// 无缓动类别：对应下标 0、1、2，即“恒 0（阶跃）”“恒 1（跳变）”“线性”。
    Plain,
    /// 正弦缓动，变化最缓和。
    Sine,
    /// 二次多项式缓动。
    Quad,
    /// 三次多项式缓动。
    Cubic,
    /// 四次多项式缓动。
    Quart,
    /// 五次多项式缓动。
    Quint,
    /// 指数缓动，起步/收尾最极端的一档。
    Expo,
    /// 圆弧缓动（单位圆投影）。
    Circ,
    /// 带回撤过冲的缓动（非单调）。
    Back,
    /// 弹簧振荡缓动（非单调）。
    Elastic,
    /// 落体弹跳缓动（分段抛物线）。
    Bounce,
}

/// 缓动的方向变体，决定曲线在两端的缓急分布。
///
/// 三个变体由 `f1!` / `f2!` / `f3!` 三个宏从同一个基础函数生成，
/// 其判别值直接参与编号计算，因此顺序必须保持 In、Out、InOut。
#[repr(u8)]
pub enum TweenMinor {
    /// 缓入：起点慢、终点快（`f1!`，直接使用基础函数，见 `sine` 等）。
    In,
    /// 缓出：起点快、终点慢（`f2!`，按 `1 - f(1 - x)` 反演）。
    Out,
    /// 两端缓动：起止都慢、中段最快（`f3!`，压半后镜像拼接）。
    InOut,
}

/// 由主类别与方向变体算出 [`TweenId`]：`major as u8 * 3 + minor as u8`。
///
/// 这是谱面缓动编号的唯一权威公式：每种曲线占 3 个连续编号（In / Out / InOut），
/// 因此缓动表长度为 11 × 3 = 33。
/// 声明为 `const fn`，可在常量上下文（如静态谱面表）中直接求值，无运行时开销。
pub const fn easing_from(major: TweenMajor, minor: TweenMinor) -> TweenId {
    major as u8 * 3 + minor as u8
}

/// 可插值的值类型：定义“两个值之间按比例 `t` 过渡”以及“两段动画如何叠加”。
///
/// 为什么需要逐类型实现：不同量的自然插值方式并不相同——标量/向量做线性插值，
/// 颜色要逐通道（含 alpha）插值，字符串要处理文本动画的特殊规则；而
/// [`Anim`](crate::core::Anim) 的 `next` 链又需要把两段曲线**相加**。
/// 把这两件事抽象成 trait 后，`Anim<T>` 才能对任意 `T` 通用。
pub trait Tweenable: Clone {
    /// 在 `x`（对应 `t = 0`）与 `y`（对应 `t = 1`）之间按比例 `t` 插值。
    ///
    /// `t` 已由缓动函数映射过，可能超出 `[0, 1]`（back/elastic 的过冲），
    /// 因此实现应按普通线性公式外插，而不是钳制 `t`。
    fn tween(x: &Self, y: &Self, t: f32) -> Self;
    /// 把两条动画的取值叠加起来，供 [`Anim`](crate::core::Anim) 的 `next` 链使用。
    ///
    /// 默认实现 `unimplemented!()`：只有需要参与链式叠加的类型才重写它。
    /// 例如 [`Color`] 不参与叠加，因此没有实现。
    ///
    /// # Panics
    /// 未重写该方法的类型被叠加时会 panic——这是刻意的“未支持”标记，
    /// 而非可恢复错误。
    fn add(_x: &Self, _y: &Self) -> Self {
        unimplemented!()
    }
}

// 实现语义：标量线性插值；叠加是普通加法，使“多条位移曲线相加”成立。
impl Tweenable for f32 {
    /// `x + (y - x)·t`，等价于 `(1-t)·x + t·y` 但少一次乘法；
    /// 且 `t = 0` / `t = 1` 时浮点上严格得到 `x` / `y`（端点无误差）。
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        x + (y - x) * t
    }

    /// 直接相加：位移、缩放增量这类需要累加的量。
    fn add(x: &Self, y: &Self) -> Self {
        x + y
    }
}

// 实现语义：与 f32 相同的线性插值，比例 t 提升到 f64。
// 时间轴与高度累积用 f64 以避免长时间播放的精度损失，而缓动比例仍是 f32。
impl Tweenable for f64 {
    /// 线性插值，`t` 由 f32 提升为 f64 后参与运算。
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        x + (y - x) * t as f64
    }

    /// 直接相加。
    fn add(x: &Self, y: &Self) -> Self {
        x + y
    }
}

// 实现语义：逐分量复用 f32 的实现，保证与标量路径采用完全相同的运算顺序，
// 避免两套公式产生细微差异而让 x/y 不同步。
impl Tweenable for Vec2 {
    /// 两个分量各自线性插值。
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        vec2(f32::tween(&x.x, &y.x, t), f32::tween(&x.y, &y.y, t))
    }

    /// 分量相加，即向量加法。
    fn add(x: &Self, y: &Self) -> Self {
        vec2(x.x + y.x, x.y + y.y)
    }
}

// 实现语义：RGBA 四通道各自线性插值（含 alpha）。
// 不实现 `add`：颜色的“相加”没有明确语义（逐通道相加会溢出且不符合调色直觉），
// 因此颜色动画不参与 `next` 链的相加。
impl Tweenable for Color {
    /// 逐通道插值。按 sRGB 分量直接线性过渡（不做线性空间的转换），
    /// 与谱面作者在编辑器里看到的渐变一致。
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        Self::new(f32::tween(&x.r, &y.r, t), f32::tween(&x.g, &y.g, t), f32::tween(&x.b, &y.b, t), f32::tween(&x.a, &y.a, t))
    }
}

// 实现语义：文本动画的过渡规则（谱面用它做“数字滚动”与“逐字显示/擦除”）。
// 不实现 `add`：文本没有“相加”的语义。
impl Tweenable for String {
    /// 按文本自身结构选择过渡方式：百分比数字插值，或逐字增删。
    ///
    /// 规则按优先级：
    /// 1. 两端都含 `%P%`：去掉标记后按数值线性插值。`%P%` 是 Phira 的“百分比数字”
    ///    占位约定，用于让分数/进度平滑滚动；端点直接返回原串，避免解析与格式化
    ///    引入舍入误差（否则 100 可能显示成 99.999）；
    /// 2. 两端都空：返回空串；
    /// 3. `y` 空而 `x` 非空：视为“擦除”，用 `1 - t` 反演后复用正向逻辑，
    ///    避免为收缩单独写一套分支；
    /// 4. `x` 空而 `y` 非空：逐字显示，取前 `round(t·len)` 个字符
    ///    （按字符而非字节计数，对中文等多字节文本安全）；
    /// 5. 互为前缀：按字符数差做逐字增长/收缩——`x` 是 `y` 的前缀时用 `floor`
    ///    保证起点严格等于 `x`；`y` 是 `x` 的前缀时用 `1 - t` 向 `y` 收缩；
    /// 6. 其它情况（含只剩 `x` 带 `%P%` 的退化情形）：直接取 `x`。
    ///
    /// # Panics
    /// 规则 4 会按 `t` 切片：`t > 1`（back/elastic 的过冲）时下标越界 panic。
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        // 规则 1：两端都是百分比数字，做数值插值。
        if x.contains("%P%") && y.contains("%P%") {
            let x = x.replace("%P%", "");
            let y = y.replace("%P%", "");
            // 端点直接取原串，绕过解析与格式化，保证 `t = 1` 精确得到 `y`。
            if t >= 1. {
                y
            } else if t <= 0. {
                x
            } else {
                let x: f32 = x.parse().unwrap_or(0.0);
                let y: f32 = y.parse().unwrap_or(0.0);
                let value = x + t * (y - x);
                // 两端都是整数时保持整数外观，否则保留 3 位小数（够用且不啰嗦）。
                if x.fract() == 0.0 && y.fract() == 0.0 {
                    format!("{:.0}", value)
                } else {
                    format!("{:.3}", value)
                }
            }
        // 规则 2：两端都空。
        } else if x.is_empty() && y.is_empty() {
            Self::new()
        // 规则 3：只有右端为空 -> 反向复用“逐字显示”，实现擦除。
        } else if y.is_empty() {
            let x = if x.contains("%P%") { x.replace("%P%", "") } else { x.to_string() };
            Self::tween(y, &x, 1. - t)
        // 规则 4：只有左端为空 -> 逐字显示，按字符数取前若干个。
        } else if x.is_empty() {
            let chars = y.chars().collect::<Vec<_>>();
            chars[..(t * chars.len() as f32).round() as usize].iter().collect()
        } else {
            let x_len = x.chars().count();
            let y_len = y.chars().count();
            // 规则 5a：x 是 y 的前缀 -> 在后缀部分逐字增加。
            if y.starts_with(x) {
                // x in y
                let take_num = ((y_len - x_len) as f32 * t).floor() as usize + x_len;
                let mut text = x.clone();
                text.push_str(&y.chars().skip(x_len).take(take_num - x_len).collect::<String>());
                text
            // 规则 5b：y 是 x 的前缀 -> 按 1-t 从 x 逐字收缩到 y。
            } else if x.starts_with(y) {
                // y in x
                let take_num = ((x_len - y_len) as f32 * (1. - t)).round() as usize + y_len;
                let mut text = y.clone();
                text.push_str(&x.chars().skip(y_len).take(take_num - y_len).collect::<String>());
                text
            // 规则 6：无可用的逐字关系；若带百分比标记则把标记剥掉，否则原样返回。
            } else if x.contains("%P%") {
                x.replace("%P%", "")
            } else {
                x.clone()
            }
        }
    }
}

// 实现语义：矩形按四个分量同步插值（左上角与宽高），
// 使矩形在移动的同时尺寸也平滑过渡。不实现 `add`：矩形叠加无明确定义。
impl Tweenable for Rect {
    /// 逐分量线性插值：`x`、`y`（左上角）与 `w`、`h`（宽高）各自 `tween`。
    fn tween(x: &Self, y: &Self, t: f32) -> Self {
        Self::new(f32::tween(&x.x, &y.x, t), f32::tween(&x.y, &y.y, t), f32::tween(&x.w, &y.w, t), f32::tween(&x.h, &y.h, t))
    }
}
