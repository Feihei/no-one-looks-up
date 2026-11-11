# 计划：自研体积云（替换 bevy_elements CloudPlugin）

## Context

现状体积云用 vendored bevy_elements 0.6.0 的 CloudPlugin（compute raymarch + 时域分帧累积）。Feihei 验收结论（dev-plan R1）：**形状不好看**（噪声粒度 32–64 m 太粗、团块无翻卷细节）、**光照/颜色不对**（黄昏发灰、无银边/金色云底）、整体不满意。bevy_elements 的云为全天候动态天气设计，本游戏锁定黄昏 17:30 + 晴朗低云量 + 仰视相机，通用性妥协正是观感问题根源。

已与 Feihei 确认的三项决策：
1. **代码落点**：游戏侧新模块 `src/plugins/clouds/`，vendor 不动（零升级合并风险）。
2. **质量取向**：半分辨率 march + 时域重投影上采样，中低端独显 30fps+。
3. **形态范围**：只做黄昏积云一种模型（5 种 CloudKind 全部映射进积云参数空间）。

## 核心技术事实（已核实）

- `CloudPlugin` 可用 `.disable::<CloudPlugin>()` 关闭（[sky.rs:258-270](file:///e:/play/coding/heads-up-bevy/src/plugins/sky.rs#L258-L270) 已有 13 个同模式 disable）。
- 关闭后：crate 的 celestial/fog 均 `Option<Res<CloudShadows>>`，自动回落 `1-cover^3.4` 估算压暗太阳/环境光——够用，**无需自做云影贴图**；但 crate 不再自动给相机加 `DepthPrepass`，需自己补挂；游戏 [sky.rs:355](file:///e:/play/coding/heads-up-bevy/src/plugins/sky.rs#L355) 的 `ResMut<CloudLayer>` 系统必须删除（资源随插件消失）。
- vendor 管线蓝本 [vendor/bevy_elements/src/clouds/gpu.rs](file:///e:/play/coding/heads-up-bevy/vendor/bevy_elements/src/clouds/gpu.rs)：compute march `in_set(MainPass).before(main_opaque_pass_3d)`；composite 全屏三角形 `.after(main_transparent_pass_3d)`，写 `ViewTarget::main_texture_view()`，Rgba16Float，混合 `src.rgb + dst.rgb·src.a`，`LoadOp::Load`，无 depth attachment；深度读 `ViewPrepassTextures::depth_only_view()`；大气 LUT 从 view 实体 `AtmosphereTextures`（transmittance_lut / aerial_view_lut）。
- vendor 的 Rust 辅助层（`gpu::kernel`、`CloudView`、`CloudGpu`）全是 `pub(crate)`——游戏侧须自写 kernel 排队辅助（约 60 行）；`bevy_elements::noise`（worley3/fbm3_periodic/hash31）由 crate 核心加载，关闭 CloudPlugin 后仍可 WESL import；`clouds/common.wesl` 仅由 CloudPlugin 注册，**大气查找函数（transmittance_uv/sample_transmittance/light_at/shell_range/view_ray）须拷进游戏侧 WESL（约 150 行）并用 tripwire 测试钉住**。
- 关键几何红利：相机仰角 22.55–60°、云层 0.8–3.2 km → 可见云范围 ≤10 km（vendor 为 32 km）。march 上限缩到 ~16 km，weather map 从 512²/32km 缩到 256²/16km。

## 模块结构

```
src/plugins/clouds/
├── mod.rs      # CloudsPlugin：接线、DepthPrepass 观察器、Update 系统（drift/config→uniform/重置）、CloudsQuality、CloudsView 标记
├── config.rs   # CloudsConfig Resource + CloudKind→参数表映射（纯函数，CPU 可测）
├── params.rs   # CloudsParams uniform（ShaderType，镜像 WESL struct，全 vec4）
├── gpu.rs      # RenderApp 侧：pipeline/bind group/调度
├── common.wesl     # params struct、壳几何、密度模型、大气查找（拷自 vendor）、黄昏光照
├── generate.wesl   # base 256³ 分 8 slab + detail 64³ 噪声生成（一次性）
├── weather.wesl    # 256² coverage/dome 2D 图（每帧）
├── march.wesl      # 半分辨率 march（每 texel 每帧全量，无 Bayer 槽位）
├── resolve.wesl    # 全分辨率时域重投影 + fresh 3×3 clamp
└── composite.wesl  # 全屏三角形合成
```

新 Resource：`CloudsConfig`（cover/层高/梯度四元组/erosion/dome/cell_scale，纯数据每帧重算）、`CloudsDrift`（f64 三通道漂移按 tile 周期 wrap + frame + reset_frames）、`CloudsQuality`、`CloudsView`（`ExtractComponent` 相机标记）。

## 渲染管线

| Pass | 调度 | 说明 |
|---|---|---|
| noise | RenderGraph，`.before(camera_driver)` | base 256³ rgba8unorm 分 8 帧 z-slab（4×4×4 WG）+ detail 64³ 1 帧 |
| weather | 同上（噪声完成后每帧） | 256²/16km：R=coverage 重映射、A=dome |
| march | `MainPass`，`.before(main_opaque_pass_3d)` | 半分辨率 compute，每 texel 每帧全量 march（fresh 场完备 → clamp 更准，比 vendor 的稀疏 Bayer 槽位少 ghost） |
| resolve | march 后 chain | 全分辨率：壳入点世界空间重投影（`PreviousViewData.clip_from_world`）→ bilinear history → 3×3 fresh clamp → `mix(history, fresh, 0.4~0.6)` |
| composite | `MainPass`，`.after(main_transparent_pass_3d)` | 全屏三角形，读 history + depth，曝光在此时乘 `view.exposure` |

- **重投影选世界空间壳入点**：相机平移 ≤2 m/s 在 1.5 km 云层的视差为亚像素级，旋转主导；vendor resolve.wesl:60-74 数学可整体改编。
- **history 存未曝光 luminance**（vendor 存曝光后值需补偿）；`perturb_exposure` ±0.15 ev 不再污染历史。
- **重置触发**（reset_frames=2，直接写 fresh 上采样）：kind/amount/layer/quality 变化、平移 >50 m（传送）、resize。
- 绑定：深度 `ViewPrepassTextures::depth_only_view()`（Msaa::Off 过滤照抄 gpu.rs:471）；大气 `AtmosphereTextures`；环境 cube 从相机 `GeneratedEnvironmentMapLight.environment_map` handle 镜像（Update 侧读 → render 侧资源），无 env map 回落 1×1 白 cube。

## 形状模型（解决"团块"）

- 噪声规格：**base 256³ rgba8unorm，tile 2048 m → 8 m/texel**（VRAM 67 MB；vendor 为 128³/4096 m = 32 m/texel）；**detail 64³，tile 512 m → 8 m/texel**。Worley cells 到 128，最细特征 ~16 m，粒度提升 2–4 倍——这是"翻卷碎边"的直接杠杆。
- Perlin-Worley 4 通道 fBm（Hillaire 配方，generate.wesl 重写）。
- 高度梯度四元组按 kind 参数化（积云族 `0, 0.16, 0.88, 0.98`；层状 kind 用扁平梯度 + dome≈0 表达，不做独立渲染路径）。
- 保留 vendor 已验收的形状经验：dome `pow(dome, 0.62)` + jitter ±35% + `DOME_WIDTH 0.30`（PATCH 后参数）、wispy→billowy 混合、Worley 对流胞 gaps、Nubis 式 value erosion、风切变 lean（WIND_SKEW 500 m）。
- coverage 重映射曲线放 config.rs 纯函数（CPU 可测），拉开低 cover 区间形态差异。

## 光照模型（解决"发灰"）

- 太阳色：大气 transmittance LUT（拷 `light_at`），恒白光源经 LUT 自然偏红，不手调色。
- **多重散射：Hillaire 三 octave 解析近似**（替代 vendor 的扩散平板）——τ_l 只算一次 6 采样 golden-angle 锥，octave 是零额外采样：`Σ a^o·exp(−τ_l·b^o)·phase(cosθ, g·c^o)`，黄昏默认 `a=0.4, b=0.4, c=0.7`（橙红内透光靠 a/b 调大）。
- 双瓣 HG（`0.7·HG(0.8) + 0.3·HG(−0.5)`）保逆光银镶边 + Beer-powder。
- 环境光：环境 cube 16 采样 Fibonacci 上半球平均 × 高度梯度（0.2→1.0）——黄昏橙地平线→蓝紫天顶自动给云上色。
- aerial：`aerial_view_lut` in-scatter + transmittance LUT 比值（vendor `air_transmittance` 数学）。
- far veil 保留（march 上限 16 km 处 cover 均值 veil，FAR_TAU 5），画面底边掠地平线不出现硬墙。
- 一致性保险丝：暴露 `ambient_gain`/`sky_compensation` uniform（初始 1.0），实测云与天空亮度脱节时手调。

## 性能预算（1080p）

半分辨率 0.52MP × 56 步（Medium）≈ 106M 纹理 tap/帧 ≈ 中低端独显 1.5–3 ms，33 ms 预算余量充足；不跑 vendor 的 sky probe（每帧 6 面 cube）与 shadow map，是最大省项。VRAM：噪声 68 MB + history 2×16 MB + march 4 MB + weather 0.26 MB ≈ 90 MB。

`CloudsQuality::of(ElementsQuality)`：Low 40 步/detail 降权，Medium 56（默认），High 72，Ultra 96 步 + 8 光采样。

## 分阶段计划

### C0 接管与管线骨架（最快可看）
- [ ] `src/plugins/clouds/{mod,params,gpu}.rs` 骨架 + `CloudsPlugin` 注册进 GamePlugins（SkyPlugin 后）
- [ ] sky.rs：disable `CloudPlugin`、删 `apply_cloud_layer_override`、DepthPrepass 观察器（`On<Add, ElementsCamera>`）
- [ ] WESL import spike：验证 `bevy_elements::noise` 可 import + 游戏 crate 内 `load_shader_library!` 库名解析
- [ ] 自写 kernel 排队辅助（vendor 的 pub(crate) 不可用）
- [ ] debug 模式：均匀亮色壳层全链渲染（march→resolve→composite），移动/转头验证重投影不糊不鬼影
- [ ] 单测：参数 struct 镜像、重置触发

### C1 噪声与形状
- [ ] generate.wesl（256³ 8-slab + 64³ detail）+ weather kernel（256²）
- [ ] common.wesl 密度模型（梯度/dome/cell gaps/erosion/wind skew，含 vendor patch 经验）
- [ ] march 接真密度（coarse/fine 双模跳步）
- [ ] 测试：噪声配置、coverage 重映射；截图出结构化积云

### C2 黄昏光照
- [ ] transmittance LUT 太阳色 + 三 octave 多重散射 + 双瓣 HG + powder
- [ ] 环境 cube 环境光（handle 镜像，16 采样）
- [ ] aerial perspective + far veil；黄昏默认参数表
- [ ] 截图：向阳/背向两张，验证银镶边与内透橙光

### C3 时域稳健性与遮挡
- [ ] 重投影 clamp 硬化（reset 全触发接通）
- [ ] 深度遮挡（march 块级 hidden 测试）
- [ ] 曝光扰动下 history 不脏
- [ ] 步行+转头实机验收（Feihei）：无拖影无闪烁

### C4 性能与档位
- [ ] `CloudsQuality::of(ElementsQuality)` 映射；Low 档 detail 降权
- [ ] RenderDiagnostics 五段 pass span
- [ ] 实机 1080p 30fps+；DX12/Vulkan 双后端核验

### C5 验收与文档
- [ ] 五 kind 参数终调（F5 面板逐一切换人工验收）
- [ ] `tests/clouds_screenshot.rs` 断言定稿
- [ ] 新增 `.agents/adr-0007-clouds-rework.md`（中文）：决策、噪声规格、被否选项（patch vendor 噪声——+128MB 且破坏升级路径；生态 crate 不支持 0.20；2D 云板——无体积光照）
- [ ] 更新 adr-0003（CloudPlugin 停用缘由指向 adr-0007）、dev-plan（新增 R6 里程碑）、AGENTS.md 状态表

## 测试

- 单测：config.rs 五 kind 参数表断言、coverage 重映射单调性、漂移 wrap 确定性（bit-exact）、重置触发、tripwire（游戏侧 common.wesl 含 `transmittance_uv`/`light_at`；默认无云开局不变）。
- GPU 冒烟 `tests/clouds_screenshot.rs`（#[ignore]，960×540，frame ~140）：无 NaN、云覆盖率落在 amount 推算区间 ±15%、云 mask 内亮度梯度能量下限（非团块代理指标）、地平线带 R>B 暖色、顶部梯度。
- 既有 `tests/sky_screenshot.rs` 不动，作无云回归。
- 每阶段门槛：`cargo fmt --check` + `cargo clippy -- -D warnings` + `cargo test` 全绿 → GPU 截图人工看图 → 小步 Conventional Commit → 更新 dev-plan。

## 风险

| 风险 | 缓解 |
|---|---|
| 时域拖影/鬼影 | 壳入点重投影 + 完备 fresh clamp + config 变化即 reset + fresh 权重 ≥0.4 |
| 游戏 crate WESL 库名解析未证实 | C0 首项 spike；失败退化单文件 shader |
| AtmosphereTextures/env map 时序 | 照抄 gpu.rs Query 模式，Option + 白 cube 回落 |
| storage texture ≤4/阶段 | 每 bind group ≤2 |
| 256³ 首帧卡顿 | 分 8 帧 slab；128³ 兜底开关 |
| 拷贝 vendor WESL 漂移 | tripwire + ADR 记录拷贝清单 |
| 云亮度与 celestial 压暗脱节 | ambient_gain/sky_compensation 手调保险丝 |
| DX12/Vulkan 差异 | C4 双后端核验 |

## 关键参考文件

- [vendor gpu.rs](file:///e:/play/coding/heads-up-bevy/vendor/bevy_elements/src/clouds/gpu.rs) — 管线编排/bind group/深度与大气绑定蓝本
- [vendor common.wesl](file:///e:/play/coding/heads-up-bevy/vendor/bevy_elements/src/clouds/common.wesl) — 密度模型与大气查找拷贝源
- [vendor resolve.wesl](file:///e:/play/coding/heads-up-bevy/vendor/bevy_elements/src/clouds/resolve.wesl) — 时域重投影数学
- [src/plugins/sky.rs](file:///e:/play/coding/heads-up-bevy/src/plugins/sky.rs) — disable CloudPlugin、删 CloudLayer 写入
- [tests/sky_screenshot.rs](file:///e:/play/coding/heads-up-bevy/tests/sky_screenshot.rs) — GPU 冒烟测试模板
