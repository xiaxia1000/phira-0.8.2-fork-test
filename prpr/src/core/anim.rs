//! 关键帧动画系统：[`Anim`] 把一条“随时间变化的值”表示为一串 [`Keyframe`]。
//!
//! 求值约定：当前时间落在区间 `[kf_i, kf_{i+1})` 时，使用**左侧**关键帧 `kf_i`
//! 的 `tween` 作为缓动函数，先把局部进度
//! `t = (now - kf_i.time) / (kf_{i+1}.time - kf_i.time)` 映射为缓动后的 `y`，
//! 再在 `kf_i.value` 与 `kf_{i+1}.value` 之间插值。缓动挂在左端而非右端，
//! 是因为每个关键帧的语义是“从此刻起如何过渡到下一刻”，因此最后一个关键帧的
//! `tween` 永远不会被求值（区间右端开）。两端外插退化为常量：早于首帧取首帧值，
//! 到达或越过末帧取末帧值。
//!
//! 谱面时间会被反复 seek（回放、练习模式允许倒退），所以求值不假设时间单调递增，
//! 而是用可双向移动的游标 [`Anim::cursor`] 做摊还 O(1) 的区间定位。
use super::{StaticTween, TweenFunction, TweenId, Tweenable, Vector};
use std::rc::Rc;

/// 动画曲线上的一个关键帧：在 `time` 时刻取值恰为 `value`。
///
/// 插值目标 `T` 是 [`Tweenable`]，而缓动函数与 `T` 无关，因此缓动用
/// `Rc<dyn TweenFunction>` 共享（见 [`StaticTween::get_rc`]），避免每个关键帧
/// 各持一份动态分发对象。
#[derive(Clone)]
pub struct Keyframe<T> {
    /// 该关键帧在谱面时间轴上的绝对时刻，单位秒。
    pub time: f64,
    /// 该时刻的取值，含义由 `T` 决定（标量、向量、颜色等）。
    pub value: T,
    /// 从本关键帧过渡到下一个关键帧时使用的缓动；区间取**左端**关键帧的缓动。
    pub tween: Rc<dyn TweenFunction>,
}

// 构造关键帧：缓动以编号 `TweenId` 给出，内部换成全局共享的 `Rc` 实现。
impl<T> Keyframe<T> {
    /// 以 `(时刻, 取值, 缓动编号)` 构造关键帧。
    ///
    /// `TweenId` 即 [`TWEEN_FUNCTIONS`](super::TWEEN_FUNCTIONS) 的下标；取到的
    /// `Rc` 是缓动表里预分配的那一条，`Rc::clone` 只增加引用计数，因此大量
    /// 关键帧不会产生等量的函数对象。
    pub fn new(time: f64, value: T, tween: TweenId) -> Self {
        Self {
            time,
            value,
            tween: StaticTween::get_rc(tween),
        }
    }
}

/// 按关键帧描述的曲线，语义为 `now(t) = Σ 各段曲线(t)`。
///
/// 求值规则见模块文档；两条易踩的约定在此强调：
/// 1. 区间 `[kf_i, kf_{i+1})` 使用 **kf_i** 的 `tween`，末帧缓动永不生效；
/// 2. `time` 早于首帧时取首帧值，晚于/等于末帧时取末帧值（两端常量外插）。
#[derive(Clone)]
/// Anim Tween Function is using the `tween` value of the first keyframe of an interval `(kf1, kf2)`
pub struct Anim<T: Tweenable> {
    /// 当前绝对时间（秒），由 [`Anim::set_time`] 写入；仅用于求值，不驱动任何状态。
    pub time: f64,
    /// 按 `time` 升序排列的关键帧，至少一个；链式叠加的各段各持一份。
    pub keyframes: Box<[Keyframe<T>]>,
    /// 游标：指向最后一个 `time <= now` 的关键帧，即当前所在区间的左端。
    pub cursor: usize,
    /// Next Anim to chain
    ///
    /// e.g. `a1.next = a2` we have a1(t) = a1.keyframes(t) + a2(t)
    /// and if `a2.next = a3` we have a1(t) = a1.keyframes(t) + a2.keyframes(t) + a3(t)
    /// ...
    /// 中文：后继动画，构成单向链；求值时从链头逐级相加，
    /// 因此 `a1.next = a2` 表示 `a1(t) = a1.keyframes(t) + a2(t)`。
    /// 这是 [`Tweenable::add`] 必须存在的原因（见 [`Anim::now_opt`]）。
    pub next: Option<Box<Anim<T>>>,
}

// “空动画”的语义：无关键帧且无后继，求值时视为“不存在”（`now_opt` 返回 `None`）。
impl<T: Tweenable> Default for Anim<T> {
    /// 构造空动画；它是 [`Anim::is_default`] 判定的对象，也是 [`Anim::chain`] 的零元。
    fn default() -> Self {
        Self {
            time: 0.0,
            keyframes: [].into(),
            cursor: 0,
            next: None,
        }
    }
}

// 动画的构造、时间推进与求值；所有方法都只操作已解析好的关键帧，不再做单位换算。
impl<T: Tweenable> Anim<T> {
    /// 由至少一个关键帧构造动画，并把时间游标重置到起始处。
    ///
    /// 关键帧必须按 `time` 升序给出，这是解析阶段的约定（游标算法依赖有序性）。
    ///
    /// # Panics
    /// `keyframes` 为空时 panic：空曲线没有可求值的基准值，调用方应改用
    /// [`Anim::default`]（表示无动画）或 [`Anim::fixed`]（表示常量）。
    pub fn new(keyframes: Vec<Keyframe<T>>) -> Self {
        assert!(!keyframes.is_empty());
        // assert_eq!(keyframes[0].time, 0.0);
        // assert_eq!(keyframes.last().unwrap().tween, 0);
        Self {
            keyframes: keyframes.into_boxed_slice(),
            time: 0.0,
            cursor: 0,
            next: None,
        }
    }

    /// 构造常量动画：唯一关键帧位于 `t = 0`，缓动编号取 0（恒为 0 的阶跃函数）。
    ///
    /// 只有一个关键帧意味着游标永远停在末帧，[`Anim::dead`] 立即为真，
    /// 求值结果恒等于 `value`，即“不随时间变化”。
    pub fn fixed(value: T) -> Self {
        Self {
            keyframes: Box::new([Keyframe::new(0.0, value, 0)]),
            time: 0.0,
            cursor: 0,
            next: None,
        }
    }

    /// 是否为“空动画”（既无关键帧也无 `next`）。
    ///
    /// 与 [`Anim::dead`] 的区别：`is_default` 表示“谱面从未对该属性赋值”，
    /// 用于对象级裁剪（如判断判定线是否完全静态）；`dead` 表示“时间已越过
    /// 最后一个关键帧”，二者对空动画都为真，但语义不同。
    pub fn is_default(&self) -> bool {
        self.keyframes.is_empty() && self.next.is_none()
    }

    /// 把多个动画串成 `next` 单向链（叠加语义），链头作为返回值。
    ///
    /// 结果满足 `chain([a1, a2, a3])(t) = a1(t) + a2(t) + a3(t)`；之所以要让
    /// 各段保持独立而不是合并关键帧，是因为谱面里的多次同属性事件彼此可能有
    /// 不同的时间区间与缓动，相加比重新采样更精确也更省内存。
    ///
    /// 实现：从尾部逐个弹出并挂到新的尾上（先显式把原尾的 `next` 清空以丢弃
    /// 调用方可能已有的链接），只剩一个元素时它就是链头，被移出并返回。
    /// 空输入直接返回 [`Anim::default`]，保持“无动画”语义。
    pub fn chain(elements: Vec<Anim<T>>) -> Self {
        if elements.is_empty() {
            return Self::default();
        }
        let mut elements: Vec<_> = elements.into_iter().map(Box::new).collect();
        elements.last_mut().unwrap().next = None;
        while elements.len() > 1 {
            let last = elements.pop().unwrap();
            elements.last_mut().unwrap().next = Some(last);
        }
        *elements.into_iter().next().unwrap()
    }

    /// 游标是否已停在最后一个关键帧（此后求值只剩下常量外插）。
    ///
    /// 空动画也返回 `true`：没有区间可供插值，谈不上“还在动”。
    pub fn dead(&self) -> bool {
        self.cursor + 1 >= self.keyframes.len()
    }

    /// 设置当前绝对时间，把游标移动到覆盖该时间的区间左端。
    ///
    /// 为什么需要游标而不是每次都二分：正常播放时时间单调递增，第一个循环
    /// 只需把游标向后推进，总代价摊还 O(1)；但 seek、重开、练习模式倒放都会让
    /// 时间**回退**，所以第二个循环把游标向前退，两个方向都只走必要的步数。
    /// 两个循环合起来等价于“以旧游标为起点的双向扫描”，最坏仍是 O(n)，
    /// 但实际播放中几乎不触发。
    ///
    /// 游标停在最后一个 `time <= 目标` 的关键帧；目标早于首帧时退到 0，
    /// 求值自然走左端常量外插。时间写入后会把同一时间级联到整条 `next` 链，
    /// 保证叠加的各段始终处于同一时刻。
    ///
    /// 注意：曲线为空或时间与上次完全相同时会提前返回，这两种情况下不会继续向
    /// `next` 链级联时间。
    pub fn set_time(&mut self, time: f64) {
        if self.keyframes.is_empty() || time == self.time {
            self.time = time;
            return;
        }
        while let Some(kf) = self.keyframes.get(self.cursor + 1) {
            if kf.time > time {
                break;
            }
            self.cursor += 1;
        }
        while self.cursor != 0 && self.keyframes[self.cursor].time > time {
            self.cursor -= 1;
        }
        self.time = time;
        if let Some(next) = &mut self.next {
            next.set_time(time);
        }
    }

    /// 求本段曲线（不含 `next` 叠加）的值；无关键帧时返回 `None`。
    ///
    /// 游标停在末帧时直接返回该帧取值（右端常量外插）；否则取区间
    /// `[cursor, cursor + 1]`，用**左端**关键帧的缓动把归一化进度 `t` 映射为
    /// `y` 后调用 [`Tweenable::tween`]。`kf2.time > kf1.time` 由关键帧有序性保证，
    /// 因此分母不会为 0。
    fn now_opt_inner(&self) -> Option<T> {
        if self.keyframes.is_empty() {
            return None;
        }
        Some(if self.cursor == self.keyframes.len() - 1 {
            self.keyframes[self.cursor].value.clone()
        } else {
            let kf1 = &self.keyframes[self.cursor];
            let kf2 = &self.keyframes[self.cursor + 1];
            let t = (self.time - kf1.time) / (kf2.time - kf1.time);
            T::tween(&kf1.value, &kf2.value, kf1.tween.y(t as f32))
        })
    }

    /// 求整条链的值：本段与所有 `next` 段逐级相加（[`Tweenable::add`]）。
    ///
    /// 返回 `None` 仅表示本段没有关键帧——这正是 [`Anim::is_default`] 的判定依据；
    /// 一旦本段非空并存在 `next`，就假定后继链也非空（`unwrap`），这是谱面解析
    /// 阶段必须维持的不变量。相加而非覆盖，使得“多个来源驱动同一属性”可以叠加。
    pub fn now_opt(&self) -> Option<T> {
        self.now_opt_inner().map(|now| {
            if let Some(next) = &self.next {
                T::add(&now, &next.now_opt().unwrap())
            } else {
                now
            }
        })
    }

    /// 对所有关键帧的取值施加同一变换 `f`，并递归作用于整条 `next` 链。
    ///
    /// 只改值、不改时间与缓动，用于解析后的整体调整（如按倍率缩放、
    /// 在不同坐标系之间换算）；递归保证链上的每一段都被同样处理。
    pub fn map_value(&mut self, mut f: impl FnMut(T) -> T) {
        self.keyframes.iter_mut().for_each(|it| it.value = f(it.value.clone()));
        if let Some(next) = &mut self.next {
            next.map_value(f);
        }
    }
}

// 需要 `T: Default` 的一族接口：无值时回落到 `T::default()`。
impl<T: Tweenable + Default> Anim<T> {
    /// 求整条链的值；本段为空动画时返回 `T::default()`。
    ///
    /// 适用于“缺省值恰好就是类型默认值”的场景；若缺省另有含义（如缩放的缺省是
    /// 1 而非 0），应改用 [`Anim::now_opt`] 或 [`AnimVector::now_with_def`]。
    pub fn now(&self) -> T {
        self.now_opt().unwrap_or_default()
    }
}

/// 标量动画的常用别名：谱面里 alpha、rotation 以及单个坐标分量都是 `f32`。
pub type AnimFloat = Anim<f32>;

/// 二维向量动画：x、y 各是一根独立的 [`AnimFloat`]。
///
/// Phigros 谱面的 `moveX`/`moveY`、`scaleX`/`scaleY` 等事件常只给出其中一个
/// 分量，另一分量必须保持“不动”，所以两个轴分开保存而不是合成为一个
/// `Anim<Vector>`；缺省处理交给 [`AnimVector::now_with_def`]。
#[derive(Default)]
pub struct AnimVector(pub AnimFloat, pub AnimFloat);

// 向量动画的构造与求值：逐分量转发给内部的两根标量动画。
impl AnimVector {
    /// 用同一个常量向量的 x、y 分别构造等值常量动画。
    pub fn fixed(v: Vector) -> Self {
        Self(AnimFloat::fixed(v.x), AnimFloat::fixed(v.y))
    }

    /// 同步推进两分量的时间与游标，使它们始终处于同一时刻。
    pub fn set_time(&mut self, time: f64) {
        self.0.set_time(time);
        self.1.set_time(time);
    }

    /// 求当前向量；某分量为空动画时该分量取 `f32::default()` 即 0。
    ///
    /// 仅当“缺省就是 0”时语义正确（如位移），缩放请用
    /// [`AnimVector::now_with_def`]。
    pub fn now(&self) -> Vector {
        Vector::new(self.0.now(), self.1.now())
    }

    /// 求当前向量，并允许为 x、y 分别指定各自的缺省值。
    ///
    /// 与 [`AnimVector::now`] 的差别在于缺省值可控：当谱面只写了 y 而没写 x 时，
    /// 缺省 0 会把缩放压扁，而缺省 1 才是“不缩放”。因此调用方按量的物理含义
    /// 传入缺省值（[`Object::now_scale`](crate::core::Object::now_scale) 传 `(1.0, 1.0)`）。
    pub fn now_with_def(&self, x: f32, y: f32) -> Vector {
        Vector::new(self.0.now_opt().unwrap_or(x), self.1.now_opt().unwrap_or(y))
    }
}
