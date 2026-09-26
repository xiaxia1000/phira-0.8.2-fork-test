//! 全局图标集合。
//!
//! 所有界面用到的图标在启动时一次性加载并由 `Icons` 集中持有，各页面通过 `Arc<Icons>` 共享。
//! 这样做的原因：一是避免每个页面各自重复加载同名纹理（浪费内存与 IO），
//! 二是把「图标文件名 -> 字段」的映射集中到一处，便于整体替换美术资源/资源包。

use crate::scene::TEX_ICON_BACK;
use anyhow::Result;
use macroquad::texture::load_texture;
use prpr::ext::SafeTexture;

/// 全部 UI 图标的集合；字段名即图标语义（`mod`/`abstract` 是关键字，用原始标识符 `r#`）。
pub struct Icons {
    /// 应用图标。
    pub icon: SafeTexture,
    /// 播放/继续。
    pub play: SafeTexture,
    /// 奖牌（排行榜/段位）。
    pub medal: SafeTexture,
    /// 资源包入口。
    pub respack: SafeTexture,
    /// 消息/通知。
    pub msg: SafeTexture,
    /// 设置。
    pub settings: SafeTexture,
    /// 返回（见 `new`，复用已加载的 `TEX_ICON_BACK`）。
    pub back: SafeTexture,
    /// 语言选择。
    pub lang: SafeTexture,
    /// 下载。
    pub download: SafeTexture,
    /// 用户/账号。
    pub user: SafeTexture,
    /// 信息详情。
    pub info: SafeTexture,
    /// 删除。
    pub delete: SafeTexture,
    /// 菜单（更多操作）。
    pub menu: SafeTexture,
    /// 编辑。
    pub edit: SafeTexture,
    /// 排行榜（leaderboard）。
    pub ldb: SafeTexture,
    /// 关闭。
    pub close: SafeTexture,
    /// 搜索。
    pub search: SafeTexture,
    /// 排序。
    pub order: SafeTexture,
    /// 筛选。
    pub filter: SafeTexture,
    /// 模组（mod）。
    pub r#mod: SafeTexture,
    /// 实心星（评分）。
    pub star: SafeTexture,
    /// 空心星（未评分）。
    pub star_outline: SafeTexture,
    /// 实心爱心（已收藏）。
    pub heart: SafeTexture,
    /// 空心爱心（未收藏）。
    pub heart_outline: SafeTexture,
    /// 未上传云端。
    pub cloud_none: SafeTexture,
    /// 已上传云端。
    pub cloud_check: SafeTexture,
    /// 新增（+）。
    pub plus: SafeTexture,
    /// 多选。
    pub select: SafeTexture,
    /// 导出。
    pub export: SafeTexture,

    /// 华为渠道专用图标，仅在启用 `hykb` feature 时编译与加载。
    #[cfg(feature = "hykb")]
    pub hykb: SafeTexture,

    /// 谱面无封面时的抽象占位图（`abstract`）。
    pub r#abstract: SafeTexture,
}

// 异步加载全部图标：文件名相对当前资源根路径；`back` 直接复用已缓存的
// `TEX_ICON_BACK`（避免重复读盘）；任一 `load_texture().await` 失败都会向上传播错误，
// 因为缺图会使依赖它的界面无法正常显示。
impl Icons {
    /// 从资源目录加载整组图标。
    ///
    /// # Errors
    /// 当任一图标文件缺失或解码失败时返回错误（`load_texture` 的 IO/解码错误向上传播）。
    pub async fn new() -> Result<Self> {
        Ok(Self {
            icon: load_texture("icon.png").await?.into(),
            play: load_texture("resume.png").await?.into(),
            medal: load_texture("medal.png").await?.into(),
            respack: load_texture("respack.png").await?.into(),
            msg: load_texture("message.png").await?.into(),
            settings: load_texture("settings.png").await?.into(),
            lang: load_texture("language.png").await?.into(),
            back: TEX_ICON_BACK.with(|it| it.borrow().clone().unwrap()),
            download: load_texture("download.png").await?.into(),
            user: load_texture("user.png").await?.into(),
            info: load_texture("info.png").await?.into(),
            delete: load_texture("delete.png").await?.into(),
            menu: load_texture("menu.png").await?.into(),
            edit: load_texture("edit.png").await?.into(),
            ldb: load_texture("leaderboard.png").await?.into(),
            close: load_texture("close.png").await?.into(),
            search: load_texture("search.png").await?.into(),
            order: load_texture("order.png").await?.into(),
            filter: load_texture("filter.png").await?.into(),
            r#mod: load_texture("mod.png").await?.into(),
            star: load_texture("star.png").await?.into(),
            star_outline: load_texture("star_outline.png").await?.into(),
            heart: load_texture("heart.png").await?.into(),
            heart_outline: load_texture("heart_outline.png").await?.into(),
            cloud_none: load_texture("cloud_none.png").await?.into(),
            cloud_check: load_texture("cloud_check.png").await?.into(),
            plus: load_texture("plus.png").await?.into(),
            select: load_texture("select.png").await?.into(),
            export: load_texture("export.png").await?.into(),

            #[cfg(feature = "hykb")]
            hykb: load_texture("hykb.png").await?.into(),

            r#abstract: load_texture("abstract.jpg").await?.into(),
        })
    }
}
