# ADR-0003: 程序化天空系统（Bevy 迁移版）

状态：接受（2026-11-15 修订：**云改为自研**，`CloudPlugin` 停用，见 [ADR-0007](adr-0007-clouds-rework.md)；大气散射 / Celestial / Fog 仍用 bevy_elements）

日期：2026-10-19（自原 Godot 版 ADR-0003 2026-07-08 迁移改写；技术选型全面更新为 Bevy 0.20 + bevy_elements，其余设计决策保留）

## 上下文

游戏核心体验是玩家长时间抬头看天，天空的质量直接决定游戏成败。需要使用程序化方式生成动态黄昏天空，而非静态 HDRI。Bevy 0.20 已内置物理基大气散射（0.16 引入，0.18 完成遮蔽+PBR 整合与通用散射介质），bevy_elements 0.5.3 是截至 2026-10-19 唯一已确认兼容 Bevy 0.20 的完整天气/云/雾 crate。

## 决策

### 技术选型

- **主方案（A）**: Bevy 0.20 内置 `Atmosphere`（`ScatteringMedium::earth`，物理基 Rayleigh/Mie 散射）+ bevy_elements 的 Celestial/Fog 模块（云自 2026-11-15 起自研，见 ADR-0007），外封装一层 `DuskSky` Resource 管理器
- **备选（B）**: 不用 bevy_elements，只用官方内置 Atmosphere + VolumetricFog + 自写云（`FullscreenMaterial`/compute raymarch）——零外部依赖但工作量大，作为 bevy_elements 不满足需求时的退路
- **已否决（C）**: bevy_weather / bevy-volumetric-clouds——截至 2026-10-19 分别停在 Bevy 0.19 / 0.18，未跟进 0.20；且 bevy-volumetric-clouds 不读 depth buffer（不能飞进云里）、未与内置 atmosphere 集成

### 天空动态特性（原设计全部保留，实现映射如下）

| 原设计 | Bevy/bevy_elements 实现 |
|--------|----------------------|
| 黄昏色调：金黄→橙红→深蓝紫渐变 | 内置 `Atmosphere` 物理散射在低太阳仰角自动呈现橙红天色（0.18 的 Atmosphere Occlusion + PBR 整合）；固定时间见下节 |
| 云层：随机生成、缓慢飘移，每次加载不同 | 【2026-11-15 起】自研 `src/plugins/clouds/`（体积 march + 时域重投影，见 ADR-0007）；bevy_elements `CloudPlugin` 已 `.disable()`，其云影一并失去 |
| 星星：黄昏后期缓缓显现 | bevy_elements CelestialPlugin：Yale 亮星星表；0.20 的 atmosphere 合成方式使星空随天色变亮自然熄灭 |
| 月亮：天际较高位置，可选弯月/满月，位置随机 | bevy_elements 月亮盘（LROC 贴图 + 相位正确）；形态/位置参数经 DuskSky 封装暴露 |
| 不同位置天空氛围细微差异（世界坐标噪声） | 【适配】需自写：以世界坐标驱动 `ScatteringMedium` 密度乘子（`with_density_multiplier`）或 exposure 微扰，作为 DuskSky 的一部分 |

### 黄昏固定与天气锁定（保持 ADR-0001"晴朗黄昏"）

- **时间固定**: bevy_elements `WorldClock` 固定在 17:30（Golden Hour），`time_scale = 0`（时间不推进）；不启用其昼夜循环与 Markov 天气调度
- **天气锁定**: `WeatherDirector::force(晴朗)`，云量取低值（ADR-0001：保持晴朗黄昏，不引入天气变化）；不使用 bevy_elements 的雨雪/雷电/风场调度
- **自动曝光**: 启用 bevy_elements 的 `auto_exposure`（黄昏太阳高度下曝光自适应），或按官方示例手动 `Exposure { ev100: 13.0 }`——开发时二选一实测

### 相机与后期效果栈

- `ElementsCamera` 组件自动挂接：`AtmosphereSettings`、`AtmosphereEnvironmentMapLight`、`DepthPrepass`、`VolumetricFog`、`DistanceFog`、`Exposure`，并自动 spawn 地球 Atmosphere
- 官方 atmosphere 示例推荐：`Tonemapping::AcesFitted` + `Bloom::NATURAL`（太阳的自然观感主要来自 Bloom）+ TAA
- Bloom/色调映射/DoF 直接用内置；色差（CA）用 0.20 新 `FullscreenMaterial` trait 自写全屏 shader
- 大气渲染模式：常规游玩 `LookupTexture`，cinematic/精确远距切 `Raymarched`（可运行时切换）

### 可扩展性（不变）

- 接口预留以支持后期添加：飞鸟、飞机、风筝、气球等高空元素
- 高空元素作为独立 entity 挂在天空系统下（天空 entity 子级），受独立生成逻辑控制
- DuskSky Resource 提供参数接口：黄昏强度、云密度、星星可见度、月亮形态等

## 备选方案

- **纯自写程序化 Sky Shader**（bevy_elements 不满足需求时的备选）：可控性最高但开发周期长——保留为方案 B
- **多 HDRI 混合**：方便但无动态性——已否决（沿用原项目）
- **Godot 版 sky-3D 路线**：随引擎迁移放弃

## 后果

- 需要编写 `DuskSky` Resource 封装（黄昏参数接口、时间锁定、天气锁定、世界坐标噪声微扰）
- bevy_elements 的 WorldClock/Weather/Cloud/Celestial/Fog 模块需按上表逐项配置与裁剪（禁用未用模块，减小编译体积）
- 需确认 bevy_elements 云的配色与内置 `Atmosphere` 联动正常（其 CloudPlugin 设计上"lit with the sun colour from Bevy's atmosphere"）
- 需确保后期效果栈与物理大气散射的色彩搭配（Bloom/Tonemapping/DoF/CA）
- **风险提示**：bevy_elements 自述无逐行人工审计，引入前 Agent 须读其 Cloud/Celestial/Fog 源码；关键视觉参数用截图测试断言
- 系统间排序必须显式声明（`.before()/.after()`），配合 Bevy 0.20 Schedule Randomization 做 property testing
