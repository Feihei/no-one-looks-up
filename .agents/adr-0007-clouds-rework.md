# ADR-0007: 自研体积云（替换 bevy_elements CloudPlugin）

状态：接受

日期：2026-11-15（R6 / C0–C5，2026-11-11 启动）

## 上下文

游戏的核心动作是抬头看天，而 vendored 的 bevy_elements 0.6.0 `CloudPlugin` 达不到质量要求：

- 基础噪声体积 4096 m / 128 texel = **32 m/texel**，细噪声 64 m/texel——云缘的团块被量化在 32–64 m，看不出翻卷（这是分辨率硬约束，不是参数问题）；
- 黄昏光照只有单次散射 + 一个固定权重的"扩散"近似，低太阳（2.7°）下整片云**发灰**，没有银镶边、没有内透橙光；
- crate 的噪声分辨率写死在 shader 里，改它要 patch 依赖：+128 MB VRAM，且每次升级 bevy_elements 都要重新合并（vendor 里已有 3 个云 shader 补丁，见 `vendor/bevy_elements/PATCHES.md`）。

ADR-0003 当初选 bevy_elements 云是"生态里唯一兼容 0.20 的完整方案"；到 R6 时结论变成：**大气/星空/月亮继续用它，云自己写**。

## 决策

自研体积云放在游戏侧 `src/plugins/clouds/`，vendor 一行不改（零升级合并风险）；`SkyPlugin` 里 `.disable::<CloudPlugin>()`，clouds 插件在它之后注册，接管 march/合成全链。

- **单一积云模型 + 5 个参数点**：`CloudKind::{ScatteredCumulus, BrokenCumulus, ToweringCumulus, Stratocumulus, Overcast}` 全部映射进同一套积云参数空间（高度梯度四元组 × dome 包络 × coverage × 对流胞 gaps × 双段 erosion × 风切变），不做第二套渲染器。层云/阴天是"梯度很平、dome 很小的积云"。
- **噪声规格**（`generate.wesl`）：基础体积 256³ / 2048 m（8 m/texel，Perlin-Worley 四通道 fBm，Worley cells 8/16/32/64/128）分 8 帧 z-slab 生成；细节体积 64³ / 512 m（cells 2…32，细胞加权）。都是可平铺的周期噪声，漂移按 tile 取模（bit-exact 单测）。
- **天气**（`weather.wesl`）：256² / 16 km，四段 GPU 计算（clear → field → 直方图 threshold → weather）。阈值法保证**云量精确等于 cover**（不是近似），dome 通道给每块云一个"胖瘦"。
- **march**（`march.wesl`）：半分辨率（每轴 1/2），每 texel 每帧全量 marching（没有 Bayer 槽位，避免棋盘伪影）；coarse/fine 双模跳步（空域 10 次细采样后回粗步）；`EMPTY_LIMIT`/`MIN_TRANSMITTANCE` 早停。
- **光照**（C2）：太阳色走 Bevy transmittance LUT（低太阳自动染橙），双瓣 HG（0.7·HG(0.8) + 0.3·HG(−0.5)）+ Beer-powder + Hillaire 三 octave 多重散射；环境光取 `GeneratedEnvironmentMapLight.environment_map`（6 层 cube，`AtmosphereEnvironmentMapLight` 每帧重算）的上半球 16 个 Fibonacci 采样，**余弦加权** `E/π = 2/N·Σ cosθ·L`；空气透视走 Bevy aerial-view LUT + 反转射线；12 km 后加 far veil 防硬边。
- **时域**（`resolve.wesl`）：history 存**未曝光**的预乘辐亮度 + 透射率，4 天邻域 clamp（3×3 fresh 框）+ 0.5 fresh 混合；壳入点世界空间重投影。所有失效条件收敛在 `CloudsRuntime::observe`（shape / quality / 传送 > 50 m / resize）。
- **档位**：march 步数 40/56/72/96、光路采样 4/6/6/8、detail 体积权重 0/1/1/1（Low 直接丢 detail 纹理与 wispy 侵蚀）。
- **可观测性**：五段 `RenderDiagnostics` pass span（noise/weather/march/resolve/composite）；主程序补了 `RenderDiagnosticsPlugin`（Bevy 不在 DefaultPlugins 里，否则 span 全是空操作）。

## 被否选项

- **patch vendor 的噪声分辨率**：最省事，但 +128 MB VRAM（256³×4 通道 HDR 或等价），且把"升级要重合并 3 个补丁"变成"要重合并 5 个"，还改不到光照——否决。
- **换生态 crate**：bevy_weather / bevy-volumetric-clouds 截至决策时都不支持 Bevy 0.20，前者还停在 0.19；bevy-volumetric-clouds 不读 depth buffer（不能飞进云里）、不与内置 atmosphere 集成——否决（与 ADR-0003 方案 C 同因，但这次时间点已晚于 0.20 发布，生态没跟上）。
- **2D 云板 / 半透明 billboard**：便宜，但没有体积光照，低太阳下做不出银镶边和内透橙光，且视角一变就穿帮——否决（游戏核心体验就是抬头看天）。
- **保守 1024³ 噪声**：显存够但生成要 64 帧、8 m/texel 的收益也没了——否决。

## 后果

- 维护：`common.wesl` 拷贝并改了 vendor 的壳层几何/大气查找（`altitude`/`shell_range`/`sample_transmittance`/`light_at`）。`common_wesl_carries_the_copied_math` 是 tripwire 单测，防止有人静默删掉这些函数；vendor 升级时对照这份清单。
- 约束（踩过的坑，写下来防复发）：
  - composite 的 blend 是 `src.rgb + dst.rgb·src.a`（`dst_factor = SrcAlpha`），**输出 alpha 必须是透射率而不是覆盖率**，否则晴朗天空会被云的不透明度乘黑；
  - 环境光必须余弦加权，均匀半球平均会被贴地的地平线高光带主导，云亮得像白天；
  - history 只能存未曝光值，曝光只由 composite 乘（`only_the_composite_applies_exposure` tripwire）。
- 测试：单测（参数表/单调性/漂移 bit-exact/reset 触发/params 镜像/tripwire）+ 5 个 GPU 截图测试（`#[ignore]`，需真 GPU）：debug 壳层全链、向阳结构、背阳变蓝、五 kind 逐一、1080p 帧预算（headless 12.6 ms DX12 / 13.8 ms Vulkan）。
- 已知限制：**Overcast/Stratocumulus 是近平坦的盖子**（stddev ≈0.01/0.07）——模型没有"基础高度噪声"，要起伏得另加一层低频扰动；Low 档丢 detail 后轮廓变软；无云影（crate 的 CloudShadows 随 CloudPlugin 一起停用）。
- 后续可做：基础高度噪声（让层云有起伏）、云影投地（用天气图低分辨率近似）、飞行穿云时的近平面处理。

## 参考

- 方案与逐阶段计划：`.trae/documents/self-built-volumetric-clouds.md`
- 进度与技术纪要：`.agents/dev-plan.md`（阶段 R6 / C0–C5）
- 拷贝来源：[vendor common.wesl](file:///e:/play/coding/heads-up-bevy/vendor/bevy_elements/src/clouds/common.wesl)、[vendor march.wesl](file:///e:/play/coding/heads-up-bevy/vendor/bevy_elements/src/clouds/march.wesl)
- 实现：[src/plugins/clouds/](file:///e:/play/coding/heads-up-bevy/src/plugins/clouds)
- 测试：[tests/clouds_screenshot.rs](file:///e:/play/coding/heads-up-bevy/tests/clouds_screenshot.rs)
