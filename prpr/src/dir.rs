//! Directory helper
//!
//! 提供“带根目录约束”的路径操作：所有便捷方法都先把相对路径交给 `Dir::join`
//! 解析，从而把路径穿越防护集中在唯一一处，而不是让每个调用点各自校验。

use anyhow::{bail, Result};
use std::{
    fs::{File, ReadDir},
    path::{Component, Path, PathBuf},
};

/// 一个已确认存在的目录，作为后续所有文件操作的根。
///
/// 持有它即意味着“所有相对路径都被限制在该目录内”，
/// 这是把用户提供的（可能来自谱面包的）路径拼接到磁盘上时的安全前提。
pub struct Dir(PathBuf);

// 根目录的构造与各种 IO 便捷封装；每个方法都会先做安全的路径解析。
impl Dir {
    /// 校验 `path` 是一个已存在的目录后构造 `Dir`。
    ///
    /// 在构造处就完成校验，可以让后续所有 `join` / IO 操作省去“是否为目录”的判断。
    ///
    /// # Errors
    /// 路径不存在或指向普通文件时返回错误。
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !path.is_dir() {
            bail!("not dir")
        }
        Ok(Self(path))
    }

    /// 把相对路径 `path` 解析为根目录下的绝对路径。
    ///
    /// **这是本模块的安全边界**：路径穿越（path traversal）防护完全由本函数承担。
    /// 规则是——`ParentDir`（`..`）只有在已经至少“下潜”过一层（`depth > 0`）时才合法，
    /// 否则立即报错。因此 `../secret` 会被拒绝，而 `a/../b` 仍能正确规约为 `b`
    /// （后者不会逃出根目录，无需禁止）。同时显式拒绝 `Prefix`
    /// （Windows 盘符如 `C:`、UNC 前缀），否则传入绝对路径会把根目录整个替换掉。
    ///
    /// # Errors
    /// 路径含前缀、或在未下潜时出现 `..`（即试图越出根目录）时返回错误。
    pub fn join(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();
        let mut res = self.0.clone();
        // depth 记录相对根目录的下潜深度：Normal 分量加一、ParentDir 减一，
        // 它是判断 `..` 是否越界的关键依据。
        let mut depth = 0;
        for comp in path.components() {
            match comp {
                Component::Prefix(_) => {
                    bail!("prefix inside dir");
                }
                Component::ParentDir => {
                    // 还没下潜就想上溯，说明要跳出根目录，直接拒绝。
                    if depth == 0 {
                        bail!("path traversal");
                    }
                    res.pop();
                    depth -= 1;
                }
                Component::Normal(name) => {
                    res.push(name);
                    depth += 1;
                }
                // 根分隔符与前缀已单独处理，它们不改变层级，忽略即可。
                Component::RootDir | Component::CurDir => {}
            }
        }
        Ok(res)
    }

    /// 递归创建子目录（自动补齐缺失的父级）。
    #[inline]
    pub fn create_dir_all(&self, p: impl AsRef<Path>) -> Result<()> {
        std::fs::create_dir_all(self.join(p)?)?;
        Ok(())
    }

    /// 递归删除子目录及其全部内容。破坏性操作，调用方需自行确认目标是缓存 /
    /// 谱面目录而不是用户数据。
    #[inline]
    pub fn remove_dir_all(&self, p: impl AsRef<Path>) -> Result<()> {
        std::fs::remove_dir_all(self.join(p)?)?;
        Ok(())
    }

    /// 打开子目录并返回以它为根的新 [`Dir`]，把后续操作的边界收窄到该子目录。
    #[inline]
    pub fn open_dir(&self, p: impl AsRef<Path>) -> Result<Self> {
        Self::new(self.join(p)?)
    }

    /// 创建（或截断已存在的）文件，返回可写句柄。
    #[inline]
    pub fn create(&self, p: impl AsRef<Path>) -> Result<File> {
        Ok(File::create(self.join(p)?)?)
    }

    /// 以只读方式打开文件。
    #[inline]
    pub fn open(&self, p: impl AsRef<Path>) -> Result<File> {
        Ok(File::open(self.join(p)?)?)
    }

    /// 判断子路径是否存在于根目录下。
    #[inline]
    pub fn exists(&self, p: impl AsRef<Path>) -> Result<bool> {
        Ok(self.join(p)?.exists())
    }

    /// 一次性读取文件的全部字节。
    #[inline]
    pub fn read(&self, p: impl AsRef<Path>) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.join(p)?)?)
    }

    /// 列出子目录的条目。
    ///
    /// 返回的 `ReadDir` 是惰性迭代器，迭代过程中的单个条目出错由调用方决定如何处理，
    /// 这样遍历大目录时不必因为一个坏条目而整批失败。
    #[inline]
    pub fn read_dir(&self, p: impl AsRef<Path>) -> Result<ReadDir> {
        Ok(std::fs::read_dir(self.join(p)?)?)
    }
}
