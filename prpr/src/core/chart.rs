//! 谱面容器与渲染编排。
//!
//! [`Chart`] 持有全部判定线、BPM 表与谱面级设置，并负责每帧固定顺序的更新与渲染。
//! 两处顺序是正确性的前提：
//!
//! - **更新**：先推进所有判定线对象的时间，再批量预计算各线的世界变换与累计旋转，
//!   最后才逐线更新。因为子线的变换由父线推出，父线必须先算好；
//! - **渲染**：先画视频（绑定判定线的视频跟随该线矩阵），再施加 flip 基准变换，
//!   然后按 [`Chart::order`] 的 z 序逐线绘制，最后一次性提交音符并叠加谱面特效。
use super::{BpmList, Effect, JudgeLine, JudgeLineKind, Matrix, Resource, UIElement, Vector};
use crate::{core::Object, fs::FileSystem, judge::JudgeStatus, ui::Ui};
use anyhow::{Context, Result};
use macroquad::prelude::*;
use nalgebra::Rotation2;
use sasa::AudioClip;
use std::{cell::RefCell, collections::HashMap};
use crate::config::ws;

/// 谱面级附加内容：特效与（可选 feature 的）视频背景。
#[derive(Default)]
pub struct ChartExtra {
    /// 谱面自带特效，在谱面坐标系中渲染，会随谱面一起翻转（见 [`Chart::render`]）。
    pub effects: Vec<Effect>,
    /// 全局特效：不随谱面翻转/缩放，直接覆盖整屏（供外部叠加层使用）。
    pub global_effects: Vec<Effect>,
    /// 视频背景与其可选的判定线绑定；绑定存在时视频跟随该线的变换绘制。
    #[cfg(feature = "video")]
    pub videos: Vec<(super::Video, Option<super::VideoAttach>)>,
}

/// 谱面级开关，决定渲染/判定细节；随谱面文件解析得到，而非玩家设置。
#[derive(Default)]
pub struct ChartSettings {
    /// 是否解释 Phira 的“负 alpha = 特效扩展”语义。
    ///
    /// 谱面把 `alpha < 0` 复用为扩展通道：取整数部分 w 表示不同的隐藏/预现策略
    /// （1 = 完全不绘制，2 = 不画线下音符，100..1000 = 提前 `(w-100)/10` 秒出现）。
    /// 关闭时负 alpha 一律视为不可见。PEC 格式默认开启该特性。
    pub pe_alpha_extension: bool,
    /// Hold 音符是否允许部分遮挡判定线。
    ///
    /// 不同编辑器对 Hold 与判定线的遮挡关系理解不一致，用该开关复刻各自观感：
    /// 关闭时 Hold 整体画在判定线之后，开启时按音符高度裁剪遮挡区域。
    pub hold_partial_cover: bool,
}

/// 打击音表：键为谱面中引用的音频文件名（如 `click.mp3`、自定义音效路径）。
///
/// 以文件名为键而非音符 id，可使同一音效在整张谱面中只解码一次、多处复用；
/// 值是已解码可播放的 [`AudioClip`]。
pub type HitSoundMap = HashMap<String, AudioClip>;

/// 一张已解析完成的谱面：判定线集合 + BPM 表 + 谱面级设置与附加内容。
pub struct Chart {
    /// 谱面时间偏移（秒），用于把音频时间轴对齐到谱面时间轴。
    pub offset: f32,
    /// 全部判定线；下标即渲染顺序表与 HUD 绑定槽中引用的 id。
    pub lines: Vec<JudgeLine>,
    /// 节拍 <-> 秒换算表。用 `RefCell` 包住是因为 [`Chart::render`] 只持有 `&self`，
    /// 而换算需要推进其内部的搜索游标。
    pub bpm_list: RefCell<BpmList>,

    /// 谱面级渲染/判定开关。
    pub settings: ChartSettings,
    /// 特效与视频等附加内容。
    pub extra: ChartExtra,

    /// Line order according to z-index, lines with attach_ui will be removed from this list
    ///
    /// Store the index of the line in z-index ascending order
    /// 线渲染顺序：元素是判定线在 [`Chart::lines`] 中的下标，按
    /// `(z_index, 原始下标)` 升序排列——先按 z_index 分层，同一层内保持谱面原始
    /// 书写顺序（依赖 `sort_by_key` 的**稳定排序**）。绑定到 HUD 的线已从本表剔除，
    /// 因为它们不参与正常谱面绘制顺序。
    pub order: Vec<usize>,
    /// TODO: docs from RPE
    /// 7 个 HUD 元素到判定线的绑定槽：下标 = `UIElement as usize - 1`
    /// （Pause = 1 … Level = 7），值为被绑定的判定线下标，`None` 表示该元素未绑定。
    /// 对应 RPE 谱面的 `attachUI`：被绑定的判定线会从 [`Chart::order`] 中移除，
    /// 由 UI 侧按该线的变换单独绘制到 HUD 上。
    pub attach_ui: [Option<usize>; 7],

    /// 打击音表，见 [`HitSoundMap`]。
    pub hitsounds: HitSoundMap,
}

// 谱面的构造、时间推进与渲染；渲染只读 `&self`，可变状态由 `RefCell` / `&mut Resource` 提供。
impl Chart {
    /// 由已解析好的各部件组装谱面，并预先算好两套索引表。
    ///
    /// # Arguments
    /// * `offset` - 谱面时间偏移（秒）
    /// * `lines` - 全部判定线，顺序即判定线 id
    /// * `bpm_list` - 节拍/秒换算表
    /// * `settings` - 谱面级开关
    /// * `extra` - 特效与视频
    /// * `hitsounds` - 打击音表
    ///
    /// 构造时同时建立：
    /// - `attach_ui`：把带 `attach_ui` 的线写进对应槽位（槽位 = 元素编号 - 1），
    ///   这些线只服务 HUD，因此被打上 `false` 从 `order` 中过滤掉；
    /// - `order`：剩余线按 `(z_index, 原下标)` 升序排列。排序键里带上原下标，
    ///   是为了让同一 z_index 的线保持谱面书写顺序（可预期的绘制次序），
    ///   避免依赖不稳定排序的偶然结果。
    pub fn new(offset: f32, lines: Vec<JudgeLine>, bpm_list: BpmList, settings: ChartSettings, extra: ChartExtra, hitsounds: HitSoundMap) -> Self {
        let mut attach_ui = [None; 7];
        let mut order = (0..lines.len())
            .filter(|it| {
                if let Some(element) = lines[*it].attach_ui {
                    attach_ui[element as usize - 1] = Some(*it);
                    false
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();
        order.sort_by_key(|it| (lines[*it].z_index, *it));
        Self {
            offset,
            lines,
            bpm_list: RefCell::new(bpm_list),
            settings,
            extra,

            order,
            attach_ui,

            hitsounds,
        }
    }

    /// 在 HUD 坐标系中执行一段绘制，并让坐标系跟随绑定的判定线。
    ///
    /// # Arguments
    /// * `ui` - 绘制上下文
    /// * `res` - 资源与全局状态
    /// * `element` - 目标 HUD 元素，其编号决定查 `attach_ui` 的槽位
    /// * `scale_point` - 缩放中心；为 `None` 时退化为 `rotation_point`
    /// * `rotation_point` - 旋转中心（HUD 自身坐标，通常取元素锚点）
    /// * `f` - 在合成好的变换与透明度下绘制元素内容，并接收绑定线的颜色
    ///
    /// # Returns
    /// 原样返回 `f` 的返回值。
    ///
    /// # Panics
    /// `element` 的编号必须落在 1..=7（由 `UIElement` 的 `repr` 保证），
    /// 否则 `element as usize - 1` 会越界访问 `attach_ui`。
    ///
    /// 未绑定判定线时不做任何变换、以白色调用 `f`，即元素停在 HUD 默认位置。
    /// 绑定后使用的矩阵是“平移 × 绕点旋转 × 绕点缩放”，其中平移的 y 与旋转角都
    /// 取了负号：谱面坐标系 y 向上，而 HUD 绘制在 y 向下的坐标系里，
    /// 两处取反正好抵消这次翻转，使挂载后的元素方向与谱面中一致。
    #[inline]
    pub fn with_element<R>(
        &self,
        ui: &mut Ui,
        res: &Resource,
        element: UIElement,
        scale_point: Option<(f32, f32)>,
        rotation_point: (f32, f32),
        f: impl FnOnce(&mut Ui, Color) -> R,
    ) -> R {
        let scale_point = scale_point.unwrap_or(rotation_point);
        if let Some(id) = self.attach_ui[element as usize - 1] {
            let lines = &self.lines;
            let line = &lines[id];
            let obj = &line.object;
            let mut tr = line.fetch_pos(res, lines);
            tr.y = -tr.y;
            let color = self.lines[id].color.now_opt().unwrap_or(WHITE);
            let scale = obj.now_scale(Vector::new(scale_point.0, scale_point.1));
            let ro =
                Object::new_rotation_wrt_point(Rotation2::new(-obj.rotation.now().to_radians()), Vector::new(rotation_point.0, rotation_point.1));
            ui.with(Matrix::new_translation(&tr) * ro * scale, |ui| ui.alpha(obj.now_alpha().max(0.), |ui| f(ui, color)))
        } else {
            f(ui, WHITE)
        }
    }

    /// 把谱面里以 `Texture` 形式保存的贴图加载为 GPU 纹理。
    ///
    /// 只处理 [`JudgeLineKind::Texture`]：这类判定线在解析阶段只记下插图路径，
    /// 需要文件系统才能真正读入（GIF 走另一条分支，在解析时已组装好帧序列）。
    ///
    /// # Errors
    /// 文件读取失败或图片解码失败时返回错误，并在上下文中带上出错路径，
    /// 便于把问题定位到具体贴图。
    pub async fn load_textures(&mut self, fs: &mut dyn FileSystem) -> Result<()> {
        for line in &mut self.lines {
            if let JudgeLineKind::Texture(tex, path) = &mut line.kind {
                *tex = image::load_from_memory(&fs.load_file(path).await.with_context(|| format!("failed to load illustration {path}"))?)?.into();
            }
        }
        Ok(())
    }

    /// 把谱面恢复到“从未游玩”的初始状态，用于重开或重试。
    ///
    /// 需要重置三类可变状态：音符的判定结果、判定线的运行期缓存（音符排序/存活表），
    /// 以及视频解码位置。动画关键帧本身是不可变的谱面数据，只需重建时间游标
    /// （由下一次 [`Chart::update`] 的 `set_time` 完成），因此不在这里处理。
    pub fn reset(&mut self) {
        // 阶段一：清空所有音符的判定结果（保留谱面原始数据，只改运行时状态）。
        self.lines
            .iter_mut()
            .flat_map(|it| it.notes.iter_mut())
            .for_each(|note| note.judge = JudgeStatus::NotJudged);
        // 阶段二：重建每条判定线的缓存，它记录着音符的更新顺序与存活情况。
        for line in &mut self.lines {
            line.cache.reset(&mut line.notes);
        }
        // 阶段三：视频复位。仅在启用 `video` feature 时编译该分支；
        // 解码器是外部资源，失败时只能上报错误而不能中断重置流程，
        // 因此用 `show_error` 把问题暴露给用户。
        #[cfg(feature = "video")]
        for (video, _) in &mut self.extra.videos {
            if let Err(err) = video.reset() {
                use crate::parse::{ptl, L10N_LOCAL};
                crate::scene::show_error(err.context(ptl!("video-load-failed", "path" => video.video_file.path().to_string_lossy())));
            }
        }
    }

    /// 按固定顺序推进整张谱面一帧。
    ///
    /// 步骤与其必要性：
    /// 1. 先把当前时间写进所有判定线对象的动画，使后续查询读到最新值；
    /// 2. **批量预计算**每线的世界变换与累计旋转，且必须在任何单线更新之前完成
    ///    —— 子线的变换由父线推导（见 `JudgeLine::fetch_pos` / `fetch_rot`），
    ///    父线未算好时子线会取到上一帧的位置；
    /// 3. 逐线 `update`，推进高度动画、音符与外观；
    /// 4. 最后推进谱面特效与视频。
    ///
    /// 之所以先收集成两个 `Vec` 再更新，是为了避开借用冲突：计算变换需要对
    /// `self.lines` 整体只读访问，而更新需要其中单条可变，先算后改即可两全。
    /// 视频解码失败只记录告警，不应影响谱面本身的更新。
    pub fn update(&mut self, res: &mut Resource) {
        // 阶段一：统一推进判定线对象的时间（它们的状态还会被子线查询）。
        for line in &mut self.lines {
            line.object.set_time(res.time);
        }
        // 阶段二：先算后改——批量求出每线的世界矩阵与累计旋转。
        // TODO optimize
        let trs = self.lines.iter().map(|it| it.now_transform(res, &self.lines)).collect::<Vec<_>>();
        let rotations = self.lines.iter().map(|it| it.fetch_rot(&self.lines)).collect::<Vec<_>>();
        // 阶段三：逐线更新（此时父线变换已就绪）。
        for ((line, tr), rot) in self.lines.iter_mut().zip(trs).zip(rotations) {
            line.update(res, tr, rot);
        }
        // 阶段四：谱面特效；它们与判定线无关，放在最后推进。
        for effect in &mut self.extra.effects {
            effect.update(res);
        }
        #[cfg(feature = "video")]
        for (video, _) in &mut self.extra.videos {
            if let Err(err) = video.update(res.time) {
                tracing::warn!("video error: {err:?}");
            }
        }
    }

    /// 渲染整张谱面一帧。
    ///
    /// # Arguments
    /// * `ui` - 绘制上下文，管理批次与裁剪状态
    /// * `res` - 资源与全局状态（时间、宽高比、音符缓冲、离屏目标、玩家 mod 等）
    ///
    /// 各步骤的存在理由：
    /// 1. **视频最先绘制**：绑定判定线的视频用 `apply_model_of` 压入该线的对象矩阵，
    ///    于是视频随判定线一起平移/旋转并被其颜色调制；未绑定的视频以白色平铺。
    ///    视频属于背景，必须早于谱面内容。
    /// 2. `apply_model_of` 施加 `(flip_x ? -WORLD_SCALE : WORLD_SCALE, -WORLD_SCALE)`：x 方向来自玩家的镜像 mod，
    ///    y 固定取 -WORLD_SCALE 把谱面“y 向上”翻成屏幕坐标。此后所有谱面内容都在这套基准
    ///    变换内绘制，因此镜像只需处理一次。应用全局缩放。
    /// 3. 按 [`Chart::order`] 的 z 序逐线渲染；期间借出 BPM 表供判定线换算节拍，
    ///    画完立即 `drop`，避免后续步骤再次借用造成 panic。
    /// 4. `note_buffer.draw_all()` 把所有音符的顶点一次性提交，让同材质批次合并、
    ///    显著减少 draw call。
    /// 5. 启用 MSAA（`sample_count > 1`）时先 `flush()` 把已压入的批次落到离屏目标，
    ///    再 `blit()` 回屏幕；否则抗锯齿不会生效。
    /// 6. 谱面自带特效最后叠加；`no_effect`（低性能模式）时整体跳过。特效绘制在
    ///    谱面坐标系里，因此 flip_x 时需要再套一次 x 镜像把它抵消，否则特效应有的
    ///    方向会相对音符左右颠倒。
    pub fn render(&self, ui: &mut Ui, res: &mut Resource) {
        // 步骤一：视频背景。仅 feature 开启时编译；绑定判定线的分支需要该线的
        // 颜色与对象矩阵，因此先取出颜色再用矩阵作用到渲染闭包上。
        #[cfg(feature = "video")]
        for (video, attach) in &self.extra.videos {
            if let Some(attach) = attach {
                let line = &self.lines[attach.line];
                let color = line.color.now_opt().unwrap_or(res.judge_line_color);
                let mat = self.lines[attach.line].object.now(res);
                res.apply_model_of(&mat, |res| {
                    video.render(res.time, res.aspect_ratio, color);
                });
            } else {
                video.render(res.time, res.aspect_ratio, WHITE);
            }
        }
        // 步骤二：施加全局基准变换 `(flip_x ? -WORLD_SCALE : WORLD_SCALE, -WORLD_SCALE)`——x 按玩家的镜像 mod，
        // y 固定翻转为屏幕方向；该变换包住整段谱面绘制，随后统一弹出。应用全局缩放。
        let ws = ws();
        res.apply_model_of(&Matrix::identity().append_nonuniform_scaling(&Vector::new(if res.config.flip_x() { -ws } else { ws }, -ws)), |res| {
            // 步骤三：按 z 序逐线渲染。BPM 表以可变借用传给判定线（换算节拍要推进游标），
            // 画完立刻释放，避免后面再次借用同一 `RefCell` 引发 panic。
            let mut guard = self.bpm_list.borrow_mut();
            for id in &self.order {
                self.lines[*id].render(ui, res, &self.lines, &mut guard, &self.settings, *id);
            }
            drop(guard);
            // 步骤四：一次性提交所有音符顶点，便于批次合并。
            res.note_buffer.borrow_mut().draw_all();
            // 步骤五：开了 MSAA 才有离屏目标，先落盘再贴回屏幕。
            if res.config.sample_count > 1 {
                // SAFETY: `get_internal_gl()` 由 macroquad 在当前 GL 线程上维护，
                // 渲染函数本身就只能在该线程调用；此处只取出上下文以 flush 未提交的批次，
                // 不改变任何所有权或生命周期，故在 unsafe 块内是安全的。
                unsafe { get_internal_gl() }.flush();
                if let Some(target) = &res.chart_target {
                    target.blit();
                }
            }
            // 步骤六：谱面自带特效；`no_effect` 为低性能模式，直接跳过。
            if !res.no_effect {
                let render = |res: &mut Resource| {
                    for effect in &self.extra.effects {
                        effect.render(res);
                    }
                };
                // 特效与谱面共用坐标系，故 flip_x 时反向镜像一次以抵消步骤二的翻转，
                // 否则特效相对音符会左右颠倒。
                if res.config.flip_x() {
                    res.apply_model_of(&Matrix::identity().append_nonuniform_scaling(&Vector::new(-1., 1.)), render);
                } else {
                    render(res);
                }
            }
        });
    }
}
