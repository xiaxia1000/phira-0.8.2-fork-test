//! Chart metadata
//!
//! 本模块定义谱面包的描述文件模型：`info.yml` / `:info` 是权威元数据，
//! 而旧格式 `info.txt` / `info.csv`（以及完全缺失信息文件的谱面）由
//! [`crate::fs::load_info`] 解析或推断后同样得到本结构。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 谱面文件格式，决定由 [`crate::parse`] 中的哪条解析路径处理。
///
/// `#[repr(u8)]` 把枚举固定为单字节表示，方便后续按数值序列化 / 存储；
/// `rename_all = "lowercase"` 让 YAML 中出现的是 `rpe` / `pec` 等小写字符串。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[repr(u8)]
#[serde(rename_all = "lowercase")]
pub enum ChartFormat {
    /// RPE 编辑器导出的 JSON 谱面（社区事实标准，扩展名 `.json`）。
    Rpe = 0,
    /// PEC 文本谱面（早期 Phigros 编辑器格式，扩展名 `.pec`）。
    Pec,
    /// Phigros 官方谱面格式（扩展名 `.pgr`），通常从官方包中提取。
    Pgr,
    /// Phira 自研二进制格式（扩展名 `.pbc`），差分编码、体积小、加载快。
    Pbc,
}

/// 谱面元数据，与 `info.yml` 一一对应。
///
/// `#[serde(default)]` 让缺失字段自动取 [`Default`] 值（兼容手写 / 残缺的 info 文件），
/// `rename_all = "camelCase"` 则与前端上传统一字段命名。
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct ChartInfo {
    /// 谱面在服务端的唯一 id；本地导入（离线添加）的谱面没有 id，故为可选。
    pub id: Option<i32>,
    /// 上传者的用户 id；只有服务端下发的谱面才会带上。
    pub uploader: Option<i32>,

    /// 曲名，展示在选曲界面。
    pub name: String,
    /// 难度数值（如 `14.6`），用于排序与定数展示；与 [`ChartInfo::level`] 的文本形式区分。
    pub difficulty: f32,
    /// 难度等级文本（如 `"UK Lv.10"`、`"IN 15"`）。
    /// 其尾部的连续数字会被 [`crate::fs::fix_info`] 解析进 [`ChartInfo::difficulty`]。
    pub level: String,
    /// 谱师名。
    pub charter: String,
    /// 曲师名。
    pub composer: String,
    /// 曲绘的绘制者；注意它记录的是“人”，曲绘文件本身在 [`ChartInfo::illustration`]。
    pub illustrator: String,

    /// 谱面文件名（相对谱面根目录），是解析入口。
    pub chart: String,
    /// 谱面格式；`None` 表示交由加载器按扩展名 / 内容自动判定。
    pub format: Option<ChartFormat>,
    /// 音乐文件名。
    pub music: String,
    /// 曲绘文件名。
    pub illustration: String,
    /// 解锁后播放的背景视频文件名；`None` 表示本谱面没有视频。
    pub unlock_video: Option<String>,

    /// 试听起始时间，单位秒。
    pub preview_start: f32,
    /// 试听结束时间，单位秒；`None` 表示从 [`ChartInfo::preview_start`] 一直播到音乐结束。
    pub preview_end: Option<f32>,
    /// 谱面期望的画面宽高比（默认 16/9），用于换算判定线长度等几何量。
    pub aspect_ratio: f32,
    /// 背景压暗程度：约 `0.0` 为原图，`1.0` 为全黑；用于保证音符与判定线可辨识。
    pub background_dim: f32,
    /// 判定线基准长度（游戏内单位），会结合 [`ChartInfo::aspect_ratio`] 换算为屏幕比例。
    pub line_length: f32,
    /// 谱面级时间偏移，单位秒；会与用户在 [`crate::config::Config::offset`] 中的设置叠加。
    pub offset: f32,
    /// 谱面开场提示文本（例如剧情 / 警告语）。
    pub tip: Option<String>,
    /// 标签，用于分类与搜索。
    pub tags: Vec<String>,

    /// 曲目介绍 / 简介正文。
    pub intro: String,

    /// Hold 音符是否部分遮挡判定线。
    /// 不同编辑器对 Hold 的绘制顺序理解不一致，用该开关复刻各自的观感。
    pub hold_partial_cover: bool,
    /// 是否对所有音符使用统一缩放（忽略谱面/皮肤自带的大小差异）。
    pub note_uniform_scale: bool,
    /// 是否强制使用 [`ChartInfo::aspect_ratio`] 而忽略玩家侧的宽高比设置。
    pub force_aspect_ratio: bool,
    /// 是否按 RPE 1.7.0 变更后的流速算法渲染；`None` 表示按谱面格式取默认行为。
    pub use_rpe_170_speed: Option<bool>,
    /// 是否启用 attachUI 定位修正；`None` 表示按默认行为。
    pub use_attach_ui_fix: Option<bool>,

    /// 谱面的创建时间（服务端记录）。
    pub created: Option<DateTime<Utc>>,
    /// 元数据（本结构对应的信息文件）最后更新时间。
    pub updated: Option<DateTime<Utc>>,
    /// 谱面文件本身的最后更新时间；与 [`ChartInfo::updated`] 分开，
    /// 用于判断“改了元数据”还是“改了谱面”。
    pub chart_updated: Option<DateTime<Utc>>,
}

// 占位默认值：缺失信息文件（或 `info.txt` / `info.csv` 未提供对应字段）时的兜底。
// `"UK"` 是 unknown 的缩写，作为明显的占位符便于发现问题；
// `chart` / `music` / `illustration` 给出常见文件名，配合 fs 模块的自动探测
// （按扩展名扫描根目录）能在大多数谱面包中直接命中真实文件。
impl Default for ChartInfo {
    fn default() -> Self {
        Self {
            id: None,
            uploader: None,

            name: "UK".to_string(),
            difficulty: 10.,
            level: "UK Lv.10".to_string(),
            charter: "UK".to_string(),
            composer: "UK".to_string(),
            illustrator: "UK".to_string(),

            chart: "chart.json".to_string(),
            format: None,
            music: "song.mp3".to_string(),
            illustration: "background.png".to_string(),
            unlock_video: None,

            preview_start: 0.,
            preview_end: None,
            // Phigros 的基准画面比例。
            aspect_ratio: 16. / 9.,
            // 0.6 是观感折中：既能看清曲绘，又不会亮到干扰判定线读谱。
            background_dim: 0.6,
            line_length: 6.,
            offset: 0.,
            tip: None,
            tags: Vec::new(),

            intro: String::new(),

            hold_partial_cover: false,
            note_uniform_scale: false,
            force_aspect_ratio: false,
            use_rpe_170_speed: None,
            use_attach_ui_fix: None,

            created: None,
            updated: None,
            chart_updated: None,
        }
    }
}
