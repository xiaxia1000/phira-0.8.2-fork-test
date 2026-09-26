//! Binary serialization and deserialization for prpr data structures.
//! Currently:
//!   - [crate::core::Chart]
//!   - [crate::core::ChartSettings]
//!   - [crate::core::JudgeLine]
//!   - [crate::core::Note]
//!   - [crate::core::Object]
//!   - [crate::core::CtrlObject]
//!   - [crate::core::Anim]
//!   - [crate::core::Keyframe]
//!   - [macroquad::prelude::Color]
//!
//! 格式设计（`.pbc`，Phira 自研二进制谱面）：
//! - 时间统一以**毫秒整数**存储，并且相对上一个时间戳做**差分**，再用 uLEB128 变长编码，
//!   使密集排列的关键帧每个只占 1~2 字节；
//! - 集合一律表示为 uLEB128 长度前缀 + 连续元素；
//! - 字段按结构体声明顺序紧凑排列、不写字段名，因此**读写实现必须严格对称**，
//!   任何一侧顺序改动都会静默读出脏数据；
//! - 因为差分无法表示负增量，写入侧要求时间单调不减（见 `BinaryWriter::time` 的断言）。

use crate::{
    core::{
        Anim, AnimVector, BezierTween, BpmList, Chart, ChartExtra, ChartSettings, ClampedTween, CtrlObject, JudgeLine, JudgeLineCache, JudgeLineKind,
        Keyframe, Note, NoteKind, Object, StaticTween, Tweenable, UIElement,
    },
    judge::{HitSound, JudgeStatus},
    parse::process_lines,
};
use anyhow::{bail, Result};
use byteorder::{LittleEndian as LE, ReadBytesExt, WriteBytesExt};
use macroquad::{
    prelude::{Color, WHITE},
    texture::Texture2D,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    io::{Read, Write},
    ops::Deref,
    rc::Rc,
};

/// 二进制编解码协议：定义类型如何写入 / 读出 `.pbc` 谱面流。
///
/// 契约：`read_binary` 必须能接受 `write_binary` 产生的任意字节流，
/// 反之亦然；两侧的字段顺序、条件分支与编码方式必须逐项对应。
/// 之所以不引入派生宏或 schema，是为了让格式完全显式可见——
/// 谱面格式一旦发布就不能随意变更，实现里写清楚每一字节的含义比自动化更重要。
pub trait BinaryData: Sized {
    /// 从流中读出一个 `Self`。
    ///
    /// # Errors
    /// 数据不足、uLEB128 损坏或判别值非法时返回错误。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self>;
    /// 把 `self` 写入流。
    ///
    /// # Errors
    /// 底层写入失败，或该类型无法用本格式表示（如 GIF 贴图判定线）时返回错误。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()>;
}

/// 二进制读取器。
///
/// 字段：元组下标 `0` 是底层字节流，下标 `1` 是**当前累计时间戳**（单位毫秒）。
/// 后者是时间差分解码的基准——每读一个时间戳就在它之上累加增量，
/// 因此它的取值与“读到哪了”强绑定，跨对象读取时必须用
/// [`BinaryReader::reset_time`] 显式归零。
pub struct BinaryReader<R: Read>(pub R, u32);

// 读取器的状态管理与低层原语：差分时间、长度前缀数组、uLEB128。
impl<R: Read> BinaryReader<R> {
    /// 以时间基准 0 包装一个字节流。
    pub fn new(reader: R) -> Self {
        Self(reader, 0)
    }

    /// 把累计时间戳清零。
    ///
    /// 语义是“从这里开始重新计时”。因为时间以差分存储，读取新对象
    /// （判定线、动画链的下一节点）前必须清零，否则新对象的时间会被
    /// 上一个对象的末尾时间整体抬升。
    pub fn reset_time(&mut self) {
        self.1 = 0;
    }

    /// 读出一个时间戳并返回**秒**（内部精度为毫秒）。
    ///
    /// 差分解码：读到的 uLEB128 是相对上一个时间戳的增量（毫秒），
    /// 累加后再除以 1000 还原为秒。因此读取顺序必须与写入顺序一致，
    /// 且不能跳读任何时间字段，否则累计基准会错位。
    pub fn time(&mut self) -> Result<f32> {
        self.1 += self.uleb()? as u32;
        Ok(self.1 as f32 / 1000.)
    }

    /// 读出一个以 uLEB128 长度前缀开头的数组。
    ///
    /// # Errors
    /// 长度前缀损坏或任一元素解析失败时返回错误。
    pub fn array<T: BinaryData>(&mut self) -> Result<Vec<T>> {
        (0..self.uleb()?).map(|_| self.read()).collect()
    }

    /// 读出一个实现了 [`BinaryData`] 的值（转发到其 [`BinaryData::read_binary`]）。
    pub fn read<T: BinaryData>(&mut self) -> Result<T> {
        T::read_binary(self)
    }

    /// 读取一个 uLEB128 无符号整数：每字节承载低 7 位，最高位表示“后面还有字节”。
    ///
    /// 终止条件是读到最高位为 0 的字节，因此不会因数据损坏而陷入死循环。
    /// 但该实现没有限制最大字节数：若恶意数据持续给出续接位，
    /// `shift` 超过 63 后 `<<` 在 debug 构建会触发移位溢出 panic。
    pub fn uleb(&mut self) -> Result<u64> {
        let mut result = 0;
        let mut shift = 0;
        loop {
            let byte = self.read::<u8>()?;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                break Ok(result);
            }
            shift += 7;
        }
    }
}

/// 二进制写入器。
///
/// 字段：元组下标 `0` 是底层字节流，下标 `1` 是**上一次写入的时间戳**（单位毫秒），
/// 作为差分编码的基准。与读取器对称，跨对象写入前需用
/// [`BinaryWriter::reset_time`] 归零。
pub struct BinaryWriter<W: Write>(pub W, u32);

// 写入器的状态管理与低层原语：差分时间（含单调性断言）、长度前缀数组、uLEB128。
impl<W: Write> BinaryWriter<W> {
    /// 以时间基准 0 包装一个字节流。
    pub fn new(writer: W) -> Self {
        Self(writer, 0)
    }

    /// 把差分基准清零，语义与 [`BinaryReader::reset_time`] 对称。
    pub fn reset_time(&mut self) {
        self.1 = 0;
    }

    /// 写入一个时间戳（单位秒）。
    ///
    /// 先把秒乘以 1000 并四舍五入成毫秒整数，再与上一次的时间戳做差、
    /// 以 uLEB128 写出——“差分 + 变长”是密集关键帧能够塞进 1~2 字节的原因。
    ///
    /// # Panics
    /// 若本次时间戳小于上一次（也包括四舍五入恰好造成反序的情形），`assert!` 会 panic。
    /// 这是有意设计：格式本身无法表示负增量，与其写出错误数据，不如在编码端立刻失败。
    /// 因此**调用方必须按时间单调不减的顺序写入**（见 `Keyframe` / `JudgeLine`
    /// 等实现中先排序或先 reset 的做法）。
    pub fn time(&mut self, v: f32) -> Result<()> {
        let v = (v * 1000.).round() as u32;
        assert!(v >= self.1);
        self.uleb((v - self.1) as _)?;
        self.1 = v;
        Ok(())
    }

    /// 写入一个数组：先写 uLEB128 长度前缀，再依次写各元素。
    pub fn array<T: BinaryData>(&mut self, v: &[T]) -> Result<()> {
        self.uleb(v.len() as _)?;
        for element in v {
            element.write_binary(self)?;
        }
        Ok(())
    }

    /// 泛型写入便利方法，等价于 `v.write_binary(self)`。
    #[inline]
    pub fn write<T: BinaryData>(&mut self, v: &T) -> Result<()> {
        v.write_binary(self)
    }

    /// 按值写入（内部同样转发到 `write_binary`），省去调用点取引用。
    #[inline]
    pub fn write_val<T: BinaryData>(&mut self, v: T) -> Result<()> {
        v.write_binary(self)
    }

    /// 写入 uLEB128 编码：每次取低 7 位，若还有剩余则把最高位置 1 表示续接，
    /// 直到高位部分为 0 结束。
    pub fn uleb(&mut self, mut v: u64) -> Result<()> {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            self.write_val(byte)?;
            if v == 0 {
                break Ok(());
            }
        }
    }
}

// 单字节原语：不压缩、直接透传，是 uLEB128 与各种判别值的基础。
impl BinaryData for u8 {
    /// 读 1 字节。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(r.0.read_u8()?)
    }

    /// 写 1 字节。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        Ok(w.0.write_u8(*self)?)
    }
}

// 固定 4 字节小端整数，用于需要与外部工具（编辑器 / 服务端）对齐的字段。
impl BinaryData for i32 {
    /// 读 4 字节小端 i32。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(r.0.read_i32::<LE>()?)
    }

    /// 写 4 字节小端 i32。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        Ok(w.0.write_i32::<LE>(*self)?)
    }
}

// 布尔以单字节 0/1 存储，保证与手写 / 旧版本产生的数据兼容。
impl BinaryData for bool {
    /// 读布尔：只有字节恰好为 `1` 才视为真，其它任何值都当作假（容错而非报错）。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(r.0.read_u8()? == 1)
    }

    /// 写布尔：真写 `1`、假写 `0`，因此读侧只需比较常量 1。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        Ok(w.0.write_u8(if *self { 1 } else { 0 })?)
    }
}

// 浮点固定 4 字节小端：不做差分是因为浮点差分收益低且会引入额外误差，
// 而谱面中的坐标、透明度本就无法用整数精确表示。
impl BinaryData for f32 {
    /// 读 4 字节小端 f32。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(r.0.read_f32::<LE>()?)
    }

    /// 写 4 字节小端 f32。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        Ok(w.0.write_f32::<LE>(*self)?)
    }
}

// 字符串复用数组编码：uLEB128 长度前缀 + 原始 UTF-8 字节。
impl BinaryData for String {
    /// 读字符串：先读长度再读字节，并对 UTF-8 合法性做校验（非法则报错）。
    ///
    /// # Errors
    /// 字节序列不是合法 UTF-8 时返回错误。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(String::from_utf8(r.array()?)?)
    }

    /// 写字符串：按 UTF-8 字节写出（长度即字节数，而非字符数）。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.array(self.as_bytes())
    }
}

// 颜色按 RGBA 各 1 字节存储。
impl BinaryData for Color {
    /// 读颜色：四个分量依次为 r/g/b/a，由 `from_rgba` 归一化回 0~1。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(Self::from_rgba(r.read()?, r.read()?, r.read()?, r.read()?))
    }

    /// 写颜色：各分量乘以 256 后截断为字节（浮点转整型是饱和转换，
    /// 因此 1.0 会得到 255 而不会回绕成 0）。
    ///
    /// 读侧按 `/255` 归一化，与写侧的 `*256` 略有不一致，
    /// 中间值往返会有极小偏差，但 0 与 1 两个端点精确。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.write_val((self.r * 256.) as u8)?;
        w.write_val((self.g * 256.) as u8)?;
        w.write_val((self.b * 256.) as u8)?;
        w.write_val((self.a * 256.) as u8)?;
        Ok(())
    }
}

// IMPLEMENTATIONS
// 以下为具体业务类型的编解码。共同约定：
// - 每个 impl 的字段顺序都与结构体声明顺序一致，便于人工核对读写对称性；
// - 运行期状态（判定结果、缓存、纹理句柄等）不落盘，读侧统一填初始值；
// - 关键帧时间一律相对“本对象的起点”差分，故每个对象开始处都要 reset_time。

// 关键帧的编解码：时间（差分）+ 值 + 一字节紧凑描述的 Tween。
impl<T: BinaryData> BinaryData for Keyframe<T> {
    /// 读出一个关键帧。
    ///
    /// tween 用字节的高 2 位作判别：
    /// - `0` → 静态插值，低位是 tween id，经 `StaticTween::get_rc` 复用缓存实例
    ///   （避免为最常用的静止曲线反复分配）；
    /// - `0x80` → Clamped，低 7 位是 easing id，其后跟起止值；
    /// - `0xC0` → Bezier，其后跟两个控制点。
    ///
    /// # Panics
    /// 判别位为其它值（`0x40`）时 panic：这属于编码损坏，无法构造合理的插值方式，
    /// 也说明写入端与读取端版本不一致。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(Self {
            time: r.time()? as f64,
            value: r.read()?,
            tween: {
                let b = r.read::<u8>()?;
                match b & 0xC0 {
                    0 => StaticTween::get_rc(b),
                    0x80 => Rc::new(ClampedTween::new(b & 0x7f, r.read()?..r.read()?)),
                    0xC0 => Rc::new(BezierTween::new((r.read()?, r.read()?), (r.read()?, r.read()?))),
                    _ => panic!("invalid tween"),
                }
            },
        })
    }

    /// 写出一个关键帧。
    ///
    /// `Tweenable` 是 trait 对象，无法直接匹配具体变体，因此借助
    /// `as_any` + `downcast_ref` 逐类型判断；分支顺序与读取端的判别位一一对应。
    ///
    /// 注意这个 `if/else if` 链**没有兜底分支**：若将来新增第四种 Tween 实现
    /// 而忘记同步扩展此处，就会写出缺少 tween 字段的流，导致后续所有字段错位。
    /// 扩展插值类型时必须同时修改读写两侧。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.time(self.time as f32)?;
        w.write(&self.value)?;
        let tween = self.tween.as_any();
        if let Some(t) = tween.downcast_ref::<StaticTween>() {
            w.write_val(t.0)?;
        } else if let Some(t) = tween.downcast_ref::<ClampedTween>() {
            w.write_val(0x80 | t.0)?;
            w.write_val(t.1.start)?;
            w.write_val(t.1.end)?;
        } else if let Some(t) = tween.downcast_ref::<BezierTween>() {
            w.write_val(0xC0)?;
            w.write_val(t.p1.0)?;
            w.write_val(t.p1.1)?;
            w.write_val(t.p2.0)?;
            w.write_val(t.p2.1)?;
        }
        Ok(())
    }
}

/// 递归读取 `Anim` 链，返回 `None` 表示链在此结束。
///
/// 首字节是标签：
/// - `0` → 链结束；
/// - `1` → 默认动画（空关键帧集合），常见情形用它可省掉写出长度与数据；
/// - 其它（实践中为 `2`）→ 其后跟一个数组形式的动画。
///
/// 非默认分支读取前会 `reset_time()`：关键帧时间相对本动画起点差分，
/// 每个动画节点都必须重新计时。尾部 `next` 通过递归读取自然构成链。
fn read_opt<R: Read, T: BinaryData + Tweenable>(r: &mut BinaryReader<R>) -> Result<Option<Box<Anim<T>>>> {
    Ok(match r.read::<u8>()? {
        0 => None,
        x => {
            let mut res = if x == 1 {
                Anim::default()
            } else {
                r.reset_time();
                Anim::new(r.array()?)
            };
            res.next = read_opt(r)?;
            Some(Box::new(res))
        }
    })
}

// 动画链的编解码：把 `next` 链“扁平化”为一段线性字节流。
impl<T: BinaryData + Tweenable> BinaryData for Anim<T> {
    /// 读出一个动画链的头部（链的其余部分由 `read_opt` 递归组装）。
    ///
    /// 这里的 `unwrap()` 依赖写侧的保证：链首至少能解出一个节点。
    /// 若流损坏到首字节为 `0`，说明数据本身无效、无法构造合理的 `Anim`，因此直接 panic。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(*read_opt(r)?.unwrap())
    }

    /// 写出整条 `next` 链。
    ///
    /// 用循环而非递归遍历链，避免深链导致调用栈溢出；每写完一个节点就指向下一个，
    /// 遇到 `None` 时补写一个 `0` 标签收尾（与读取端的“标签 0 表示结束”对应）。
    ///
    /// 两个关键细节：
    /// - 每个节点的关键帧数组之前都要 `reset_time()`，因为关键帧时间是相对**该节点起点**
    ///   的差分，沿用上一节点的时间会让后面的关键帧整体偏移；
    /// - 空关键帧的节点只写标签 `1`，不写长度与数据（用于“该属性只有静态值”的常见情形）。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        let mut cur = self;
        loop {
            // 标签 1 = 默认（空）动画；标签 2 = 其后紧跟关键帧数组。
            if cur.keyframes.is_empty() {
                w.write_val(1_u8)?;
            } else {
                w.write_val(2_u8)?;
                w.uleb(cur.keyframes.len() as _)?;
                w.reset_time();
                for kf in cur.keyframes.iter() {
                    kf.write_binary(w)?;
                }
            }
            if let Some(next) = &cur.next {
                cur = next;
            } else {
                w.write_val(0_u8)?;
                break Ok(());
            }
        }
    }
}

// 变换对象（alpha / 缩放 / 旋转 / 平移）的编解码。
// 字段按结构体声明顺序写入，其中缩放与平移是二维向量，需按 (x, y) 拆开写。
impl BinaryData for Object {
    /// 按 alpha、scale(x,y)、rotation、translation(x,y) 的顺序读出。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(Self {
            alpha: r.read()?,
            scale: AnimVector(r.read()?, r.read()?),
            rotation: r.read()?,
            translation: AnimVector(r.read()?, r.read()?),
        })
    }

    /// 与 `read_binary` 完全同序地写出（顺序错位会导致读侧静默读到脏数据）。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.write(&self.alpha)?;
        w.write(&self.scale.0)?;
        w.write(&self.scale.1)?;
        w.write(&self.rotation)?;
        w.write(&self.translation.0)?;
        w.write(&self.translation.1)?;
        Ok(())
    }
}

// 手势控制对象的编解码。
// 首字节是一个固定的布局标记（8），用于区分版本 / 布局：
// 它让不兼容的数据能被立刻识别出来，而不是按错误的字段宽度继续读下去。
impl BinaryData for CtrlObject {
    /// 按 alpha、size、pos、y 的顺序读出，读之前先校验布局标记。
    ///
    /// # Panics
    /// 标记字节不等于 8 时 `assert_eq!` 失败——这表示数据损坏或版本不兼容，
    /// 此时继续解析只会得到无意义的数值。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        assert_eq!(r.read::<u8>()?, 8);
        Ok(Self {
            alpha: r.read()?,
            size: r.read()?,
            pos: r.read()?,
            y: r.read()?,
        })
    }

    /// 写出控制对象，首字节写布局标记 8，与读取端的断言对应。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.write_val(8_u8)?;
        w.write(&self.alpha)?;
        w.write(&self.size)?;
        w.write(&self.pos)?;
        w.write(&self.y)?;
        Ok(())
    }
}

// 音符的编解码。这里集中体现了“读侧补默认值”的约定：
// - `hitsound` 由 `kind` 推导，因为 `.pbc` 不存储打击音设置；
// - `multiple_hint` / `judge` / `color` / `fx_color` / `judge_area` 都是运行期状态
//   （判定结果、皮肤着色），不落盘，读出时统一置为初始值；
// - `speed` 用“先读一个 bool 标志、为真才读 f32”的可选编码，
//   默认值 1.0 时省下 4 字节，而变速音符只占少数。
impl BinaryData for Note {
    /// 读出一个音符。
    ///
    /// 顺序上 `time` 必须排在 `kind` 之后：Hold 分支自身还要读 `end_time` /
    /// `end_height` 两个字段，顺序错位会直接把后面的数据当时间读。
    ///
    /// # Errors
    /// kind 判别值不在 0~3 时返回错误——脏数据是可预见的情况，用错误码上报而非 panic。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        let object = r.read()?;
        let kind = match r.read::<u8>()? {
            0 => NoteKind::Click,
            1 => NoteKind::Hold {
                end_time: r.read::<f32>()? as f64,
                end_height: r.read::<f32>()? as f64,
            },
            2 => NoteKind::Flick,
            3 => NoteKind::Drag,
            _ => bail!("invalid note kind"),
        };
        let hitsound = HitSound::default_from_kind(&kind);
        Ok(Self {
            object,
            kind,
            hitsound,
            time: r.time()? as f64,
            height: r.read::<f32>()? as f64,
            speed: if r.read()? { r.read::<f32>()? as f64 } else { 1. },
            above: r.read()?,
            multiple_hint: false,
            fake: r.read()?,
            judge: JudgeStatus::NotJudged,
            color: WHITE,
            fx_color: None,
            judge_area: 1.,
        })
    }

    /// 写出一个音符。
    ///
    /// kind 先写成 1 字节判别值（Hold 额外带终点时间与高度），随后才是时间、高度、
    /// 可选速度、above、fake，顺序与 `read_binary` 一致。
    /// 注意运行期状态（判定结果、颜色等）不写出：它们由游玩过程重新生成。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.write(&self.object)?;
        match self.kind {
            NoteKind::Click => {
                w.write_val(0_u8)?;
            }
            NoteKind::Hold { end_time, end_height } => {
                w.write_val(1_u8)?;
                w.write_val(end_time as f32)?;
                w.write_val(end_height as f32)?;
            }
            NoteKind::Flick => w.write_val(2_u8)?,
            NoteKind::Drag => w.write_val(3_u8)?,
        }
        w.time(self.time as f32)?;
        w.write_val(self.height as f32)?;
        if self.speed == 1.0 {
            w.write_val(false)?;
        } else {
            w.write_val(true)?;
            w.write_val(self.speed as f32)?;
        }
        w.write_val(self.above)?;
        w.write_val(self.fake)?;
        Ok(())
    }
}

// 判定线的编解码。需要留意的几点：
// - 开头 `reset_time()`：本行所有音符的时间都相对**本行起点**差分，
//   若不清零会把上一行的末尾时间带进来。注意写侧没有对应的重置，
//   因此编码端必须自行保证每行开始前差分基准已归零（见 `Chart` 的写入实现）。
// - Texture 的贴图句柄用 `Texture2D::empty()` 占位，真正的纹理在后续资源加载阶段绑定；
// - Paint 的第二个字段是运行期绘制缓存（`RefCell`），读侧取默认值；
// - `JudgeLineCache` 不落盘，读完音符后由 `JudgeLineCache::new` 重建。
impl BinaryData for JudgeLine {
    /// 读出一条判定线。
    ///
    /// # Errors
    /// kind 判别值非法时返回错误。
    /// # Panics
    /// kind 为 4 时命中 `unimplemented!()`（该值被保留，未在格式中启用）。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        r.reset_time();
        let object = r.read()?;
        let kind = match r.read::<u8>()? {
            0 => JudgeLineKind::Normal,
            1 => JudgeLineKind::Texture(Texture2D::empty().into(), r.read()?),
            2 => JudgeLineKind::Text(r.read()?),
            3 => JudgeLineKind::Paint(r.read()?, RefCell::default()),
            4 => unimplemented!(),
            _ => bail!("invalid judge line kind"),
        };
        let height = r.read()?;
        let mut notes = r.array()?;
        let color = r.read()?;
        // 父级用“索引 + 1”编码、0 表示无父级：因为索引 0 是合法父级，
        // 必须留出一个哨兵值来区分“没有父级”。
        let parent = match r.uleb()? {
            0 => None,
            x => Some(x as usize - 1),
        };
        // 两个布尔开关打包进同一个字节的不同位（bit0 / bit1），省下 1~2 字节。
        let flags = r.read::<u8>()?;
        let show_below = flags & 1 != 0;
        let rot_with_parent = flags & 2 != 0;
        // 缓存依赖完整的音符列表才能建立，因此必须在读完 notes 之后再重建。
        let cache = JudgeLineCache::new(&mut notes);
        let attach_ui = UIElement::from_u8(r.read()?);
        let ctrl_obj = RefCell::new(r.read()?);
        let incline = r.read()?;
        let z_index = r.read()?;
        Ok(Self {
            object,
            kind,
            height,
            notes,
            color,
            parent,
            rot_with_parent,
            show_below,

            attach_ui,
            ctrl_obj,
            incline,
            z_index,

            cache,
        })
    }

    /// 写出一条判定线。
    ///
    /// kind 先写成 1 字节判别值，贴图 / 文本 / 绘制事件作为附加字段随后写出；
    /// 之后再写高度、音符数组、颜色、父级、打包的布尔标志、挂载的 UI、控制对象、
    /// 倾斜与 z 序，顺序与 `read_binary` 完全一致。
    ///
    /// 与读取端的非对称之处：读取时会为每条线 `reset_time()`，而这里不会——
    /// 因此**调用方必须自己保证写入本行之前差分基准已归零**，
    /// 否则本行首个音符的时间若小于上一行末尾，会触发 `BinaryWriter::time` 的断言。
    ///
    /// # Errors
    /// 纹理 GIF 判定线无法用二进制格式表示（帧序列是外部资源），返回错误而不是写出半截数据。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.write(&self.object)?;
        match &self.kind {
            JudgeLineKind::Normal => w.write_val(0_u8)?,
            JudgeLineKind::Texture(_, path) => {
                w.write_val(1_u8)?;
                w.write(path)?;
            }
            JudgeLineKind::Text(text) => {
                w.write_val(2_u8)?;
                w.write(text)?;
            }
            JudgeLineKind::Paint(events, _) => {
                w.write_val(3_u8)?;
                w.write(events)?;
            }
            JudgeLineKind::TextureGif(..) => {
                bail!("gif texture binary not supported");
            }
        }
        w.write(&self.height)?;
        w.array(&self.notes)?;
        w.write(&self.color)?;
        w.uleb(match self.parent {
            None => 0,
            Some(index) => index as u64 + 1,
        })?;
        w.write_val(self.show_below as u8 + self.rot_with_parent as u8 * 2)?;
        w.write_val(self.attach_ui.map_or(0, |it| it as u8))?;
        w.write(self.ctrl_obj.borrow().deref())?;
        w.write(&self.incline)?;
        w.write(&self.z_index)?;
        Ok(())
    }
}

// 谱面级设置的编解码：两个布尔开关各占一个字节（显式写成 0/1），
// 以便与手写或旧版本产生的数据保持兼容；读侧只认恰好等于 1 为真。
impl BinaryData for ChartSettings {
    /// 依次读出 `pe_alpha_extension` 与 `hold_partial_cover`。
    /// 读取用 `== 1` 而不是解析为 bool，是因为这两项在历史数据中可能被写成其它数值。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        Ok(Self {
            pe_alpha_extension: r.read::<u8>()? == 1,
            hold_partial_cover: r.read::<u8>()? == 1,
        })
    }

    /// 把两个开关写成 0/1 单字节，顺序与读取端一致。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.write_val(self.pe_alpha_extension as u8)?;
        w.write_val(self.hold_partial_cover as u8)?;
        Ok(())
    }
}

// 谱面顶层的编解码：offset、判定线数组、谱面设置。
impl BinaryData for Chart {
    /// 读出一份完整谱面。
    ///
    /// 读完判定线后立刻调用 `process_lines`：父子关系、继承属性等只有在全部判定线
    /// 都就位之后才能建立，因此这一步不能推迟到调用方。
    ///
    /// 注意 `BpmList` 被固定成 `[(0., 60.)]`（第 0 拍起 60 BPM），
    /// `ChartExtra` 与命中音表也为空——说明 `.pbc` 只承载谱面的几何与时间信息，
    /// 节拍换算、资源映射等由调用方在后续流程中另行设置。
    fn read_binary<R: Read>(r: &mut BinaryReader<R>) -> Result<Self> {
        let offset = r.read()?;
        let mut lines = r.array()?;
        process_lines(&mut lines);
        let settings = r.read()?;
        Ok(Chart::new(offset, lines, BpmList::new(vec![(0., 60.)]), settings, ChartExtra::default(), HashMap::new()))
    }

    /// 写出谱面：字段顺序必须与 `read_binary` 完全一致。
    ///
    /// `lines` 以数组形式写出，各判定线内部的时间差分基准由 [`BinaryData`]
    /// 的判定线实现负责（见其 `write_binary` 的说明），此处不再额外重置。
    fn write_binary<W: Write>(&self, w: &mut BinaryWriter<W>) -> Result<()> {
        w.write_val(self.offset)?;
        w.array(&self.lines)?;
        w.write(&self.settings)?;
        Ok(())
    }
}
