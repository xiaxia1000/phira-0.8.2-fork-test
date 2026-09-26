//! 插画/封面缩略图工具。
//!
//! 统一缩略图尺寸，并把「本地缓存优先、否则下载」的取图策略封装成可复用的异步函数，
//! 让上层页面在 UI 线程里只需 `spawn` 一个任务即可，不必关心磁盘/网络的细节。

use anyhow::{Context, Result};
use image::imageops::thumbnail;
use image::DynamicImage;
use prpr::ext::SafeTexture;
use std::future::Future;
use std::path::Path;

/// 缩略图的标准宽度（像素）。主要供云端封面 URL 使用：请求服务端按该尺寸裁剪/缩放，
/// 从而只下载小图（见 `client/model.rs` 的 `imageView/0/w/.../h/...` 参数）。
pub const THUMBNAIL_WIDTH: u32 = 347;
/// 缩略图的标准高度（像素）。本地 `thumbnail` 也以它为固定高度、宽度按原图比例推导，
/// 两个用途共用同一数值以保持本地与云端缩略图比例一致。
pub const THUMBNAIL_HEIGHT: u32 = 200;

/// 缩略图工具的命名空间：仅承载静态方法，无实例状态。
pub struct Images;
// 一组与图片相关的纯函数/异步工具，均不依赖 `&self`，故用无字段的 `Images` 作命名空间。
impl Images {
    /// 把 `(缩略图, 可选原图)` 统一成一对纹理。
    ///
    /// 当没有原图时，把缩略图 `clone` 成两份充当「原图」，
    /// 使调用方无需区分「是否有高清图」，后续切换小图/大图时逻辑一致。
    pub fn into_texture(tex: (DynamicImage, Option<DynamicImage>)) -> (SafeTexture, SafeTexture) {
        match tex {
            (thumb, Some(full)) => (thumb.into(), full.into()),
            (thumb, None) => {
                let tex: SafeTexture = thumb.into();
                (tex.clone(), tex)
            }
        }
    }

    /// 生成缩略图：固定高度为 `THUMBNAIL_HEIGHT`，宽度按原图宽高比换算并向上取整，
    /// 因此只做等比缩放、不做裁剪——极宽或极窄的封面仍保留完整画面（代价是宽度不定）。
    pub fn thumbnail(image: &DynamicImage) -> DynamicImage {
        let width = (image.width() as f32 / image.height() as f32 * THUMBNAIL_HEIGHT as f32).ceil() as u32;
        DynamicImage::ImageRgba8(thumbnail(image, width, THUMBNAIL_HEIGHT))
    }

    /// 本地缓存优先的取图策略：命中缓存则读盘，否则执行 `task` 获取并写回缓存。
    ///
    /// - `path` 已存在：直接异步读文件并解码（省掉一次网络请求）；
    /// - `path` 不存在：`await` 传入的 `task`（通常是从服务器下载），拿到图像后按 JPEG 写回 `path`
    ///   作为缓存，下次即可走上面的读盘分支；
    /// - 全程异步（文件 IO / 网络均 `await`），调用方应在后台任务中运行，避免阻塞 UI 帧。
    ///
    /// # Errors
    /// 读盘失败、下载失败或写入缓存失败时都会返回错误（错误信息带上具体环节的 context）。
    pub async fn local_or_else(path: impl AsRef<Path>, task: impl Future<Output = Result<DynamicImage>>) -> Result<DynamicImage> {
        let path = path.as_ref();
        Ok(if path.exists() {
            image::load_from_memory(&tokio::fs::read(path).await.context("Failed to read image")?)?
        } else {
            let image = task.await?;
            // 写缓存：JPEG 不支持 alpha 通道，`ohos` 平台编码时会因此报错，
            // 故在该平台先降级为 RGB 再保存；其它平台直接按原图保存。
            #[cfg(not(target_env = "ohos"))]
            {
                image.save_with_format(path, image::ImageFormat::Jpeg).context("Failed to save image")?;
            }
            #[cfg(target_env = "ohos")]
            {
                let rgb_image = DynamicImage::ImageRgb8(image.to_rgb8());
                rgb_image
                    .save_with_format(path, image::ImageFormat::Jpeg)
                    .context("Failed to save image")?;
            }
            image
        })
    }
}
