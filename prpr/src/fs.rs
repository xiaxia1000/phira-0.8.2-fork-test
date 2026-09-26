//! File system abstraction
//!
//! 为什么需要这层抽象：同一套谱面解析与资源加载逻辑必须同时服务于四种截然不同的来源——
//! 桌面上的谱面目录、打包好的 zip 谱面文件、平台内置的 assets 资源，
//! 以及运行期注入的内存补丁。若为每种来源各写一遍解析逻辑，会退化成无法维护的分支网。
//! 因此这里只抽象出两件最必要的事：“按相对路径取字节”和“列出根目录条目”，
//! 上层的元数据解析（`load_info`）与谱面加载都只依赖 `FileSystem` trait。

use crate::{ext::spawn_task, info::ChartInfo};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chardetng::EncodingDetector;
use concat_string::concat_string;
use macroquad::prelude::load_file;
use serde::Deserialize;
use serde_json::Value;
use std::{
    any::Any,
    collections::HashMap,
    fs,
    io::{Cursor, Read, Seek, Write},
    path::Path,
    sync::{Arc, Mutex},
};
use tracing::warn;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

/// 按 `patches` 重写 zip 归档并返回新的归档字节。
///
/// 语义是“替换同路径条目，其余原样保留”：
/// 先复制原归档中所有未被补丁覆盖的条目，再统一追加补丁内容，
/// 因此补丁必然得到覆盖（zip 中后者优先，且此处保证同路径条目只写一次）。
///
/// 若干细节上的取舍：
/// - 条目名统一取 `enclosed_name` 校验过的规范路径，保证重写后的键与调用方生成 `patches`
///   键的规则一致；无法解析出规范名的畸形条目直接丢弃；
/// - 压缩方式固定 Deflated、权限固定 `0o755`，让输出可预测（同输入产生同字节），
///   便于校验与缓存；
/// - 目录条目也一并保留，避免解压后的空目录结构丢失。
///
/// 该函数不修改入参归档，也不做并发控制。
pub fn update_zip<R: Read + Seek>(zip: &mut ZipArchive<R>, patches: HashMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut w = ZipWriter::new(Cursor::new(&mut buffer));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);
    // 步骤 1：搬运原归档内容；被补丁覆盖的文件在此处跳过（稍后写新内容）。
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).unwrap();
        let path = match entry.enclosed_name() {
            Some(path) => path.to_owned(),
            None => continue,
        };
        let path = path.display().to_string();
        if entry.is_dir() {
            w.add_directory(path, options)?;
        } else if !patches.contains_key(&path) {
            w.start_file(&path, options)?;
            std::io::copy(&mut entry, &mut w)?;
        }
    }
    // 步骤 2：写入补丁内容。放在最后保证同名文件一定是补丁版本。
    for (path, data) in patches.into_iter() {
        w.start_file(path, options)?;
        w.write_all(&data)?;
    }
    w.finish()?;
    Ok(buffer)
}

/// 谱面资源的读取抽象。
///
/// 方法一律接收 `&mut self`：底层实现可能需要推进游标或加锁
/// （zip 要求对归档对象有可变访问），因此 trait 不提供 `&self` 的读取入口。
/// 用 `#[async_trait]` 而非原生 async trait，是为了保持 dyn 安全——
/// 所有实现都以 `Box<dyn FileSystem>` 的形式被持有与传递。
#[async_trait]
pub trait FileSystem: Send {
    /// 读取相对路径 `path` 对应文件的全部字节。
    ///
    /// # Errors
    /// 路径不存在、归档损坏或底层 IO 失败时返回错误。调用方常把错误当作
    /// “该文件不存在”来探测（见 [`load_info`] 中依次尝试多种信息文件）。
    async fn load_file(&mut self, path: &str) -> Result<Vec<u8>>;
    /// 判断相对路径 `path` 是否存在。
    ///
    /// 与 `load_file` 并列存在的原因：zip 等来源能廉价地查询目录，
    /// 而 assets 之类没有目录概念的来源只能退化为“尝试读取”。
    async fn exists(&mut self, path: &str) -> Result<bool>;
    /// 列出根目录下的条目名（不递归），供信息文件缺失时按扩展名猜测谱面 / 音频 / 曲绘。
    fn list_root(&self) -> Result<Vec<String>>;
    /// 复制出一个拥有独立状态的读取句柄（例如各自独立的 zip 游标）。
    fn clone_box(&self) -> Box<dyn FileSystem>;
    /// 向下转型出口，让调用方取回具体实现类型。
    fn as_any(&mut self) -> &mut dyn Any;
}

/// 以应用内置 assets 为源的实现（移动端 / wasm 的默认资源）。
///
/// 字段（元组下标 `0`）是资源路径前缀，例如 `assets/`；
/// 之所以在构造时固定前缀，是因为不同平台把资源放在不同位置，
/// 调用方只需给出相对路径，前缀拼接由本类型统一负责。
#[derive(Clone)]
pub struct AssetsFileSystem(String);

// 基于 macroquad 的资源加载器实现。资源已被打包进应用包，
// 路径前缀由本 crate 内部提供而非用户输入，因此不需要路径穿越防护。
#[async_trait]
impl FileSystem for AssetsFileSystem {
    async fn load_file(&mut self, path: &str) -> Result<Vec<u8>> {
        Ok(load_file(&concat_string!(self.0, path)).await?)
    }

    async fn exists(&mut self, path: &str) -> Result<bool> {
        // unlikely to be called
        // 平台资源接口没有目录查询能力，只能以“能否读到”代替存在性判断。
        Ok(load_file(&concat_string!(self.0, path)).await.is_ok())
    }

    fn list_root(&self) -> Result<Vec<String>> {
        // 打包后的资源清单在运行期不可枚举，故返回空表：
        // 该来源无法参与 `fix_info_with` 的“按扩展名猜文件”流程，
        // 必须依赖信息文件给出确切的谱面 / 音频 / 曲绘文件名。
        Ok(Vec::new())
    }

    fn clone_box(&self) -> Box<dyn FileSystem> {
        Box::new(self.clone())
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// 以磁盘目录为源的实现（桌面端的谱面文件夹）。
///
/// 字段（元组下标 `0`）是受 [`crate::dir::Dir`] 约束的根目录；
/// 用 `Arc` 包装是为了让克隆出的多个句柄共享同一份路径约束，克隆成本仅为一次计数递增。
#[derive(Clone)]
pub struct ExternalFileSystem(pub Arc<crate::dir::Dir>);

// 直接调用标准库 IO。读取被放到阻塞线程池中执行（见下），
// 因为大谱面包的读取可能耗时数十毫秒，不能阻塞渲染帧。
#[async_trait]
impl FileSystem for ExternalFileSystem {
    async fn load_file(&mut self, path: &str) -> Result<Vec<u8>> {
        #[cfg(target_arch = "wasm32")]
        {
            // 浏览器没有可直接访问的本地目录；走到这里说明上层错误地选择了外部目录来源。
            unimplemented!("cannot use external file system on wasm32")
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            // 先把文件句柄（可 Send）移入闭包，再在阻塞线程里读取全部内容。
            let mut file = self.0.open(path)?;
            tokio::task::spawn_blocking(move || {
                let mut res = Vec::new();
                file.read_to_end(&mut res)?;
                Ok(res)
            })
            .await?
        }
    }

    async fn exists(&mut self, path: &str) -> Result<bool> {
        self.0.exists(path)
    }

    fn list_root(&self) -> Result<Vec<String>> {
        Ok(self.0.read_dir(".")?.filter_map(|res| res.ok()?.file_name().into_string().ok()).collect())
    }

    fn clone_box(&self) -> Box<dyn FileSystem> {
        Box::new(self.clone())
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// 以 zip 归档为源的实现（单文件打包分发的谱面）。
///
/// 字段：元组下标 `0` 是加锁的内存归档 `Arc<Mutex<ZipArchive<Cursor<Vec<u8>>>>>`——
/// 整个包以字节形式驻留内存，`Mutex` 让多个克隆句柄共享同一游标并满足 `Send` 要求；
/// 下标 `1` 是构造时探测出的公共根目录前缀，会自动拼接到所有查询路径之前。
#[derive(Clone)]
pub struct ZipFileSystem(pub Arc<Mutex<ZipArchive<Cursor<Vec<u8>>>>>, String);

// 构造与根目录前缀探测。
impl ZipFileSystem {
    /// 从内存字节构造。
    ///
    /// 会探测“公共根目录”：只有当恰好存在一个形如 `xxx/` 的一级目录
    /// （以 `/` 结尾、且名字内部再无其它 `/`）时才认定它是前缀。
    /// 这是为了解决常见打包工具会多套一层目录的问题；
    /// 一级目录不唯一时保持空前缀（即不剥壳），宁可多一层也不猜错。
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        let zip = ZipArchive::new(Cursor::new(bytes))?;
        let root_dirs = zip
            .file_names()
            .filter(|it| it.ends_with('/') && it.find('/') == Some(it.len() - 1))
            .collect::<Vec<_>>();
        let root = if root_dirs.len() == 1 { root_dirs[0].to_owned() } else { String::new() };
        Ok(Self(Arc::new(Mutex::new(zip)), root))
    }
}

// 基于 zip 归档的实现：读取时加锁并推进共享游标，因此读取动作被放到阻塞线程池，
// 避免在渲染线程中做解压（Deflate 解压对单帧预算来说开销太大）。
#[async_trait]
impl FileSystem for ZipFileSystem {
    async fn load_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let arc = Arc::clone(&self.0);
        let path = concat_string!(self.1, path);
        spawn_task(move || {
            let mut zip = arc.lock().unwrap();
            let mut entry = zip.by_name(&path)?;
            let mut res = Vec::new();
            entry.read_to_end(&mut res)?;
            Ok(res)
        })
        .await
    }

    async fn exists(&mut self, path: &str) -> Result<bool> {
        // zip 的中央目录表支持 O(1) 名称查询，但打开条目仍需加锁，故只在当前线程做查表。
        Ok(self.0.lock().unwrap().by_name(&concat_string!(self.1, path)).is_ok())
    }

    fn list_root(&self) -> Result<Vec<String>> {
        // 只保留一级条目：剥掉前缀后不再含 `/` 的即为根目录下的文件 / 目录。
        Ok(self
            .0
            .lock()
            .unwrap()
            .file_names()
            .filter(|it| it.strip_prefix(&self.1).is_some_and(|it| !it.contains('/')))
            .map(str::to_owned)
            .collect())
    }

    fn clone_box(&self) -> Box<dyn FileSystem> {
        Box::new(self.clone())
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// 覆盖层：在底层 [`FileSystem`] 之上叠加一组内存中的“补丁文件”。
///
/// 典型用途是资源热更新与本地替换——补丁既覆盖同名文件，也能凭空新增底层不存在的文件。
/// 字段：下标 `0` 是被覆盖的底层来源，下标 `1` 是 `路径 -> 内容` 的补丁表。
/// 路径键必须与底层的相对路径写法一致（不带根前缀），否则不会命中。
/// 注意 [`FileSystem::clone_box`] 在本实现中未完成（会 panic），
/// 因此本类型不能出现在需要克隆来源的流程里。
pub struct PatchedFileSystem(pub Box<dyn FileSystem>, pub HashMap<String, Vec<u8>>);

// 叠加语义：读优先取补丁、命中即返回；存在性取两侧的并集；
// 列目录取两侧的合并结果。
#[async_trait]
impl FileSystem for PatchedFileSystem {
    async fn load_file(&mut self, path: &str) -> Result<Vec<u8>> {
        // 补丁内容被视为最终数据，不做任何解码 / 校验，直接克隆返回。
        if let Some(data) = self.1.get(path) {
            Ok(data.clone())
        } else {
            self.0.load_file(path).await
        }
    }

    async fn exists(&mut self, path: &str) -> Result<bool> {
        // 补丁可以新增文件，因此必须同时询问底层与补丁表。
        Ok(self.0.exists(path).await? || self.1.contains_key(path))
    }

    fn list_root(&self) -> Result<Vec<String>> {
        let mut res = self.0.list_root()?;
        res.extend(self.1.keys().cloned());
        // 注意 `dedup` 只消除相邻重复项：补丁键是直接追加在底层列表之后的，
        // 因此两侧重复的条目只有在恰好相邻时才会被去掉。
        res.dedup();
        Ok(res)
    }

    fn clone_box(&self) -> Box<dyn FileSystem> {
        unimplemented!()
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// 从难度文本末尾提取连续数字并写入 `info.difficulty`。
///
/// 之所以要这样解析：`level` 字段是给人看的自由文本（`"IN 15"`、`"UK Lv.10"` 等），
/// 格式并不统一，但排序与展示需要数值；取“末尾连续数字”能覆盖绝大多数写法。
/// 先 `rev().take_while()` 再接 `rev()` 复原顺序，是因为 `take_while` 只能从一端截取。
/// 尾部不是数字或解析失败时保持原值不变。
fn infer_diff(info: &mut ChartInfo, level: &str) {
    if let Ok(val) = level
        .chars()
        .rev()
        .take_while(|it| it.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .parse::<u32>()
    {
        info.difficulty = val as f32;
    }
}

/// 把键值序列填充进 [`ChartInfo`]（供 txt / csv 两种历史格式共用）。
///
/// # Arguments
/// * `it` - 键值迭代器
/// * `csv` - 是否来自 csv：表格导出常带空单元格，空值不应覆盖默认值，
///   故该模式下跳过值为空白的项；txt 是手写键值对，允许空值
///
/// 兼容性处理集中在此处：同一语义存在多种历史键名（`Music` / `Song` 等），
/// 以及若干已废弃的键（`NoteScale` / `GlobalAlpha` / `LastEditTime` 等），
/// 后者统一投向 `deprecate` 丢弃变量，避免未使用警告只保留一条路径。
/// 需要特殊解析的键（`Path`、`AspectRatio`、`BackgroundDim`、`Level`）在通用映射之前拦截。
fn info_from_kv<'a>(it: impl Iterator<Item = (&'a str, String)>, csv: bool) -> Result<ChartInfo> {
    let mut info = ChartInfo::default();
    for (key, value) in it {
        if csv && value.trim().is_empty() {
            continue;
        }
        // 去掉键两侧空白，容忍手写文件中的对齐空格。
        let key = key.trim();
        if key == "Path" {
            continue;
        }
        // Level 同时用于“显示文本”（写入 level）与“推断难度数值”，故先做推断再走通用映射。
        if key == "Level" {
            infer_diff(&mut info, &value);
        }
        if key == "AspectRatio" {
            info.aspect_ratio = value.parse().context("invalid aspect ratio")?;
            continue;
        }
        if key == "BackgroundDim" {
            info.background_dim = value.parse().context("invalid background dim")?;
            continue;
        }
        if key == "NoteScale" || key == "ScaleRatio" {
            // 音符缩放已改由用户配置控制，忽略谱面内的旧值。
            warn!("note scale is ignored");
            continue;
        }
        if key == "GlobalAlpha" {
            // 全局透明度已废弃（会与谱面动画叠加导致不可预期的观感）。
            warn!("global alpha is ignored");
            continue;
        }
        // 已废弃与未知的键都落到这里：写入后即丢弃，既保留“兼容解析”的行为，
        // 又不必为每个键单独写 continue。
        let mut deprecate = String::new();
        *match key {
            "Name" => &mut info.name,
            "Music" | "Song" => &mut info.music,
            "Chart" => &mut info.chart,
            "Image" | "Picture" => &mut info.illustration,
            "Level" => &mut info.level,
            "Illustrator" => &mut info.illustrator,
            "Artist" | "Composer" | "Musician" => &mut info.composer,
            "Charter" | "Designer" => &mut info.charter,
            "LastEditTime" => &mut deprecate,
            "Length" => &mut deprecate,
            "EditTime" => &mut deprecate,
            _ => &mut deprecate,
        } = value;
    }
    Ok(info)
}

/// 解析旧版 txt 信息文件。
///
/// 格式约束：首行必须是 `#`，其后每行形如 `Key: Value`（分隔符是冒号加空格）。
/// 校验首行是为了尽早识别“这根本不是 info 文件”，避免把任意文本误读成元数据；
/// 空行在迭代前就被过滤，因此文件末尾的空行不会导致解析失败。
/// 允许 `\u{feff}#`（UTF-8 BOM）是因为 Windows 记事本保存的文件常带 BOM。
fn info_from_txt(text: &str) -> Result<ChartInfo> {
    let mut it = text.lines().filter(|it| !it.is_empty()).peekable();
    let first = it.next();
    if first != Some("#") && first != Some("\u{feff}#") {
        bail!("expected the first line to be #");
    }
    let kvs = it
        .map(|line| -> Result<(&str, String)> {
            let Some((key, value)) = line.split_once(": ") else {
                bail!("expected \"Key: Value\"");
            };
            Ok((key, value.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    info_from_kv(kvs.into_iter(), false)
}

/// 解析旧版 csv 信息文件：表头为键，最后一条记录为值。
///
/// 只取最后一条记录是历史约定（这类文件由表格导出，真实数据常写在末行）；
/// `flexible(true)` 允许各行长度不一致，否则遇到参差不齐的表格会直接报错。
/// 这里把表头与记录 zip 起来后复用 [`info_from_kv`]，并启用其 csv 模式跳过空单元格。
fn info_from_csv(text: &str) -> Result<ChartInfo> {
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(Cursor::new(text));
    // shitty design
    let headers = reader.headers()?.iter().map(str::to_owned).collect::<Vec<_>>();
    let record = reader.into_records().last().ok_or_else(|| anyhow!("expected csv records"))??; // ??
    info_from_kv(headers.iter().zip(&record).map(|(key, value)| (key.as_str(), value.to_owned())), true)
}

/// 校验并补全谱面的资源文件名。是 [`fix_info_with`] 的默认入口，
/// 即不启用“从 RPE `META` 推断元数据”的宽松模式。
pub async fn fix_info(fs: &mut dyn FileSystem, info: &mut ChartInfo) -> Result<()> {
    fix_info_with(fs, info, false).await
}

/// `infer_meta`: whether RPE `META` metadata may overwrite the fields of
/// `info`. `info.yml` is the authoritative manifest and must not be clobbered
/// by META values, which chart editors frequently leave stale (e.g.
/// `level: "0"`); the legacy `info.txt` / `info.csv` formats — and charts with
/// no info file at all — still get their metadata inferred from META.
/// 校验并补全谱面的 chart / music / illustration 文件名，必要时从 RPE `META` 推断元数据。
///
/// 流程分三步，用两个局部函数组织：
/// 1. 校验 `info` 中已给出的文件名是否真的存在（`get` 成功时会就地把字符串“取走”，
///    返回 `Some` 表示确定；`None` 表示待定，而不是“不存在”）；
/// 2. 扫描根目录按扩展名兜底（`put` 负责决定候选：同名视为已确定，多个候选只取第一个并告警）；
/// 3. 读取谱面文件的 `META` 段，补齐仍缺失的曲绘与音乐，并按 `infer_meta` 决定是否覆盖元数据。
///
/// # Arguments
/// * `infer_meta` - 是否允许 `META` 覆盖 [`ChartInfo`] 字段；[`load_info`] 仅在
///   完全没有信息文件时才传 `true`
///
/// # Errors
/// 找不到任何谱面文件时返回错误——没有谱面就无法演奏，此时不应继续。
pub async fn fix_info_with(fs: &mut dyn FileSystem, info: &mut ChartInfo, infer_meta: bool) -> Result<()> {
    // 探测并就地取走路径字符串：存在则返回文件名，不存在返回 None（调用方随后才会尝试兜底）。
    async fn get(fs: &mut dyn FileSystem, path: &mut String) -> Result<Option<String>> {
        Ok(if fs.exists(path).await? { Some(std::mem::take(path)) } else { None })
    }
    let mut chart = get(fs, &mut info.chart).await?;
    let mut music = get(fs, &mut info.music).await?;
    let mut illustration = get(fs, &mut info.illustration).await?;
    // 采纳候选：已是同一个值则无需处理；已确定过别的值时只告警不覆盖，
    // 保证结果与扫描顺序无关之外还保持确定性（总是取“第一个”）。
    fn put(desc: &str, status: &mut Option<String>, value: String) {
        if status.as_ref() == Some(&value) {
            return;
        }
        if status.is_some() {
            warn!("found multiple {}, using the first one", desc);
        } else {
            *status = Some(value);
        }
    }
    // 第一轮扫描：找谱面文件（json / pec），只在 info 未给出有效谱面名时生效。
    for file in fs.list_root().context("cannot list files")? {
        if let Some((_, ext)) = file.rsplit_once('.') {
            match ext.to_ascii_lowercase().as_str() {
                "json" | "pec" => {
                    put("charts", &mut chart, file);
                }
                _ => {}
            }
        }
    }
    if let Some(chart) = &chart {
        info.chart = chart.to_owned();
        // 读取谱面并尝试解析出 RPE 的 META 段；任何一步失败都只是“推断不到”，不视为错误。
        if let Ok(s) = String::from_utf8(fs.load_file(&info.chart).await?) {
            if let Ok(mut value) = serde_json::from_str::<Value>(&s) {
                // RPE 的 META 结构；这里声明为局部类型，因为它只在此处使用，
                // 且字段全部为必需项——缺字段即视为该谱面没有可用的 META。
                #[derive(Deserialize)]
                struct RPEMeta {
                    name: String,
                    level: String,
                    background: String,
                    charter: String,
                    composer: Option<String>,
                    illustration: Option<String>,
                    song: String,
                }
                if let Ok(mut meta) = serde_json::from_value::<RPEMeta>(value["META"].take()) {
                    if infer_meta {
                        info.name = meta.name;
                        infer_diff(info, &meta.level);
                        info.level = meta.level;
                        info.charter = meta.charter;
                        // 曲师 / 曲绘作者在 META 中是可选的，缺席时保留 info 中原值。
                        if let Some(val) = meta.composer {
                            info.composer = val;
                        }
                        if let Some(val) = meta.illustration {
                            info.illustrator = val;
                        }
                    }
                    // 即使不允许覆盖元数据，META 给出的资源文件名仍是有效的兜底来源。
                    if illustration.is_none() {
                        illustration = get(fs, &mut meta.background).await?;
                    }
                    if music.is_none() {
                        music = get(fs, &mut meta.song).await?;
                    }
                }
            }
        }
    } else {
        bail!("cannot find chart");
    }
    // 第二轮扫描：按扩展名找音乐与曲绘（同样只在尚未确定时生效）。
    for file in fs.list_root().context("cannot list files")? {
        if let Some((_, ext)) = file.rsplit_once('.') {
            match ext.to_ascii_lowercase().as_str() {
                "mp3" | "ogg" | "wav" | "flac" | "aac" => {
                    put("music files", &mut music, file);
                }
                "png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "avif" | "ppm" => {
                    put("illustrations", &mut illustration, file);
                }
                _ => {}
            }
        }
    }
    if let Some(music) = music {
        info.music = music;
    }
    if let Some(illustration) = illustration {
        info.illustration = illustration;
    }
    Ok(())
}

/// 猜测字节串的字符编码并解码为字符串。
///
/// 必须做编码猜测的原因：老谱面与社区工具生成的 info 文件可能是 GBK、Shift-JIS
/// 或带 BOM 的 UTF-8，按 UTF-8 硬解会在中文曲名上直接失败。
/// [`EncodingDetector`] 用统计模型给出最可能的编码（`guess` 的第二个参数允许
/// 返回 ASCII 兼容编码，`feed` 的第二参数声明这是最后一批数据），随后做解码。
/// 注意这只是“尽力而为”：样本极短时仍可能猜错，因此本函数不返回 `Result`。
fn bytes_to_text_auto(data: &[u8]) -> String {
    let mut det = EncodingDetector::new();
    det.feed(data, true);
    let encoding = det.guess(None, true);
    let (s, _, _) = encoding.decode(data);
    s.into_owned()
}

/// 按优先级加载谱面元数据。
///
/// 查找顺序及其原因：
/// 1. `:info`——由上层注入的权威元数据（例如服务端下发的信息），优先级最高；
/// 2. `info.yml`——现代谱面包的标准格式；
/// 3. `info.txt` / `info.csv`——历史格式，仅作兼容；
/// 4. 全都没有：从默认值起步，改用 `infer_meta = true` 从谱面文件的 RPE `META` 尽力推断
///    （此时没有任何权威信息，META 反而更可靠）。
///
/// 前三种分支都先经 `bytes_to_text_auto` 处理，以容忍非 UTF-8 编码。
/// 注意自动探测（[`fix_info_with`]）只在“完全没有信息文件”的分支里执行；
/// 存在信息文件时不会用目录扫描结果去覆盖其中给出的文件名。
pub async fn load_info(fs: &mut dyn FileSystem) -> Result<ChartInfo> {
    let info = if let Ok(bytes) = fs.load_file(":info").await {
        serde_yaml::from_str(&bytes_to_text_auto(&bytes))?
    } else if let Ok(bytes) = fs.load_file("info.yml").await {
        serde_yaml::from_str(&bytes_to_text_auto(&bytes))?
    } else if let Ok(bytes) = fs.load_file("info.txt").await {
        info_from_txt(&bytes_to_text_auto(&bytes))?
    } else if let Ok(bytes) = fs.load_file("info.csv").await {
        info_from_csv(&bytes_to_text_auto(&bytes))?
    } else {
        warn!("none of info.yml, info.txt and info.csv is found, inferring");
        let mut info = ChartInfo::default();
        fix_info_with(fs, &mut info, true).await?;
        info
    };
    Ok(info)
}

/// 依据路径类型自动选择来源实现。
///
/// 判定规则是“是文件还是目录”：谱面既可能以文件夹形式分发，也可能打包成 zip，
/// 对用户而言只是路径不同，因此这里统一入口。zip 打开失败时会带上路径信息，
/// 便于定位是哪个谱面包损坏。
/// 返回 `Box<dyn FileSystem + Send + Sync>`，以便在阻塞线程中读取。
pub fn fs_from_file(path: &Path) -> Result<Box<dyn FileSystem + Send + Sync + 'static>> {
    let meta = fs::metadata(path)?;
    Ok(if meta.is_file() {
        let bytes = fs::read(path).with_context(|| format!("failed to read from {}", path.display()))?;
        Box::new(ZipFileSystem::new(bytes).with_context(|| format!("cannot open {} as zip archive", path.display()))?)
    } else {
        Box::new(ExternalFileSystem(Arc::new(crate::dir::Dir::new(path)?)))
    })
}

/// 构造读取应用内置 assets 的来源，`name` 为相对 assets 根目录的路径前缀。
pub fn fs_from_assets(name: impl Into<String>) -> Result<Box<dyn FileSystem + Send + Sync + 'static>> {
    Ok(Box::new(AssetsFileSystem(name.into())))
}
