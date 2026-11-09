# Agents 开发指南（Bevy 迁移版）

> 本文档供自动化 Agent 开发《No One Looks Up Even Alone》（简称 No One）的 Bevy 版本时使用。所有设计决策见 `.agents/` 下的 ADR 文档，本文仅提供工程引导和关键约束。
>
> 本项目是 Godot 4.7 项目（`E:\play\Godot\heads_up`）向 Rust/Bevy 的迁移。原项目的 ADR 设计要点被最大化保留，仅做引擎映射层面改写——映射关系见 `.agents/adr-0005-engine-migration.md`。

---

## 项目基础

| 项目 | 值 |
|------|-----|
| 名称 | 中文《即使独自漫步你也很少看天》／英文 `No One Looks Up Even Alone`（简称 **No One**）；Godot 原作目录名仍为 `heads_up` |
| Rust 包名 | `no_one_looks_up`（snake_case，Cargo 要求） |
| 引擎 | **Bevy 0.20.1**（crates.io，2026-10-28 补丁发布；最低 Rust 1.97.1） |
| 天气/天空插件 | **bevy_elements 0.6.0**（vendored + 3 个云 shader 补丁，见 `vendor/bevy_elements/PATCHES.md`）。bevy_flora 0.46 试装已结束（结论：自研树为准，依赖已移除，见 dev-plan R0.6） |
| 语言 | Rust（edition 2024），所有代码 |
| 目标平台 | PC（Windows 优先，兼顾跨平台） |
| 类型 | 第一人称无尽散步体验，核心是抬头仰望天空与黄昏天色 |

## 版本锁定（强制）

`Cargo.toml` 中**精确锁定**版本，Bevy 生态半年一个 0.x 大版本、breaking change 频繁：

```toml
[dependencies]
bevy = "=0.20.1"
bevy_elements = "0.6.0"   # vendored（含 3 个云 shader 补丁，升级必须重合并，见 PATCHES.md）
# bevy_flora 0.46 试装已结束：自研树为准，依赖已移除（结论见 .agents/dev-plan.md R0.6）
rand_chacha = "0.9"       # 确定性 seed 生成
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- 动手前先 `cargo add` 试拉一遍依赖并 `cargo check`，确认 bevy_elements 0.5.3 的 Bevy 依赖确为 0.20。
- **补丁位升级（0.20.0 → 0.20.1）不算里程碑**：只改 `Cargo.toml` 的精确版本 + `cargo update -p bevy --precise 0.20.1`（实测只动 bevy 家族 59 个包 + ruzstd，45 个无关依赖不动；裸 `cargo update` 会顺带升 icu/tokio/wasm-bindgen 等几十个，禁止）。小版本（0.20 → 0.21）升级才走独立里程碑流程。
- bevy_elements 自述为 "vibe coded"（AI 辅助编写、有测试覆盖、无逐行人工审计），**引入前 Agent 须先读其源码**，尤其是 Cloud/Fog/Celestial 模块；把它当依赖而非黑盒。

## 文档索引

| 需要了解 | 读哪个文档 |
|----------|-----------|
| 游戏设计全貌（核心体验/视觉/存档/声景） | `.agents/adr-0001-game-design.md` |
| 世界生成策略（两阶段生成/Chunk 流式） | `.agents/adr-0002-world-generation.md` |
| 天空系统方案（Bevy 0.20 Atmosphere + bevy_elements） | `.agents/adr-0003-sky-system.md` |
| 放置/碰撞/季节（Block 优先 + 负形街道） | `.agents/adr-0004-placement-collision-seasons.md` |
| 跨街上部元素（电线/悬挑灯臂/棚架 + 净空契约） | `.agents/adr-0006-cross-street-overhead.md` |
| 引擎迁移映射（Godot → Bevy，含被否决选项） | `.agents/adr-0005-engine-migration.md` |
| 术语定义 | `.agents/glossary.md` |
| 开发任务与里程碑（checkbox 分阶段） | `.agents/dev-plan.md` |

---

## 架构映射速查（Godot → Bevy）

| Godot 概念 | Bevy 0.20 对应 |
|-----------|----------------|
| Node / Scene / .tscn | Entity / Component / Bundle；场景定义用 BSN（`bsn!` 宏） |
| GDScript autoload（SaveManager） | `Resource` + Plugin（`#[derive(Resource)]`），App 启动时插入 |
| signal | Bevy `Message`/Observer（0.20 的 Ready 事件适合场景就绪逻辑） |
| `global_shader_uniform` + `RenderingServer.set_shader_parameter` | 共享 uniform/storage buffer（见 adr-0004 季节系统节），**不得**每帧遍历材质实例 |
| foliage.gdshader 顶点色摆动 | `MaterialExtension` 顶点着色器（WESL），保留顶点色 R 通道控制摆动强度的艺术接口 |
| StaticBody3D + ConvexPolygonShape3D 碰撞 | 自研运动学控制器 + 半平面裁剪数学（见 adr-0005 决策），不引入物理引擎 |
| CanvasLayer 菜单 UI | Bevy UI（0.20 已有 em/rem 单位与 Feathers widget 可选） |
| sky-3D + DuskSky 管理器 | bevy_elements（Atmosphere/Celestial/Cloud）+ 自写 `DuskSky` Resource 封装 |
| BoxMesh 建筑占位 | glTF（Blender 导出）+ `StandardMaterial`/`MaterialExtension` 程序化做旧材质 |
| Chunk spawn/despawn | Bevy entity spawn/despawn + `despawn_all`（0.20 批量性能已优化） |

## 关键代码约定

1. **Rust 规范（强制）**：每个里程碑完成后必须通过 `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test`。代码、注释、CI、README 用英文；ADR/设计文档用中文（沿用原项目）。
2. **确定性（强制）**：一切程序化生成走 `rand_chacha`（`StdRng::seed_from_u64(seed)`），禁止全局 `thread_rng` 穿入生成路径。同一 seed 每次生成必须 bit-exact 一致（ADR-0002）。
3. **渲染帧路径约束**：Update/Render 系统内禁止阻塞 IO、禁止 panic 传播（0.20 的 catching panics 可兜底但不得依赖）；错误用 `Result` 显式处理。
4. **Conventional Commits**：小步增量提交，明确提交范围；spec/文档与实现拆分提交；**Git push 由 Feihei 本人执行**。
5. **决策表达**：方案讨论用 A/B/C 选项；计划以 checkbox 分阶段呈现（见 dev-plan.md）。
6. **输入**：专用 action 常量（`move_forward`/`turn_left`/`speed_up` 等），不使用魔法键位字符串散落各处。

## 核心设计不变量（迁移中不得丢失，Agent 每步自查）

- [ ] 视角限制：水平 ±10°、仰角 22.55–60°（画面底边恰好掠过地平线，看不到地面）（adr-0001）
- [ ] 移动：散步 1.2–2.0 m/s，游戏内可调；无跳跃、无跑步、无 FOV 变化
- [ ] 地图 0.8km×0.8km（16×16 格）有边界，玩家被外圈建筑碰撞实体围合，看不到空边缘（adr-0002/0004）
- [ ] Block 优先生成：Block 为正形、街道为负形；内部道路物理删除（adr-0004）
- [ ] 负形街道碰撞模型：实心不可见凸棱柱（y=0..2m），街宽 6–10m + jitter，相邻街区实体不交叠（几何验证测试 OVERLAPS=0 必须保留）
- [ ] 建筑沿 edge 放置 + BlockType 差异化参数表 + setback（1/5/10m）
- [ ] 跨街层（adr-0006）：竖直件（灯柱/棚架立柱）必须落在 curb 外的碰撞棱柱内；只有横拉电线、灯臂横杆、棚架梁/顶板允许跨在路上空，且**下沿 ≥ `OVERHEAD_MIN_CLEARANCE` (3.5m)**，只能通过 `world::overhead::overhead_lowest` 计算，不得在别处重算垂度/下垂
- [ ] 跨街层挂接既有流：`props::pole_slots` / `placement::street_tree_alongs` 只重放不新开流；本层专属 salt 13000，抽取顺序固定（电线 → 长灯臂 → 棚架）
- [ ] **自发光件是唯一有绘制距离的道具**：`LampHead` / `LongLampHead` 带 `VisibilityRange::abrupt(0, props::LAMP_GLOW_RANGE = 60m)`（相机 `Msaa::Off`，薄发光片在远处只会闪不会淡）；其余道具/杆件/棚架一律无绘制距离，`chunks::tests::only_the_lamp_lenses_have_a_draw_distance` 钉死这条边界
- [ ] 树：5 种资产懒加载，行道树概率 20/60/80%，FOREST 密 8m 间距，PLAZA 稀疏，树不带碰撞
- [ ] 季节：四季参数表（leaf_density/leaf_color/sway_strength），开局选定后单局不变；全局 uniform 零开销切换
- [ ] 存档 = seed + 地图尺寸 + 季节枚举 + 探索位图 + 玩家位姿；恢复 <1s
- [ ] 天空：固定黄昏（Golden Hour 17:30），金黄→橙红→深蓝紫；云/星/月齐备；保持晴朗不引入天气变化（adr-0001/0003）
- [ ] 建筑风格比例约 50/30/20（老城/公寓/厂房），可微调
- [ ] 菜单驱动开局（不再自动生成世界）；`is_game_active` 输入门控；全探索触发完成事件
- [ ] 性能预算：2–4GB VRAM 中低端独显流畅 30fps+

## 性能约束

- 目标硬件：中低端独显 2–4GB VRAM，30fps+；Bevy GPU-driven rendering 自动生效，Draw Call 压力小于 Godot，但仍按 Chunk 控制。
- bevy_elements 的 `ElementsCamera` 默认 `Msaa::Off`（depth prepass 依赖单采样 depth）；需要 MSAA 时在其后覆盖插入。
- 大气场景按官方示例配置 `Exposure { ev100: 13.0 }` + ACES Tonemapping + `Bloom::NATURAL`；开 `AtmosphereEnvironmentMapLight` 让天空驱动 IBL。
- 树资产用 glTF 外部引用（吸取 Godot 版教训：原 tree_3_1/tree_3_3 嵌入 .tscn 各 >17MB）。
- 大气渲染模式：常规游玩用 `LookupTexture`（LUT 高性能）；需要精确远距光照/cinematic 再切 `Raymarched`。

## 开发流程

1. **先读 `.agents/dev-plan.md`**，确认当前 checkbox 阶段；完成后勾选并更新状态。
2. **读对应 ADR** 了解设计约束；与原 Godot ADR 有出入处以本项目 `.agents/` ADR + adr-0005 映射表为准。
3. 设计取舍用 A/B/C 选项向 Feihei 提案，不擅自扩大范围（主动标记 scope creep）。
4. 每阶段完成后：build + fmt + clippy + test 全绿，再小步提交（Conventional Commits），更新 dev-plan。
5. 跨平台验证：Windows 优先，关键渲染特性（大气/云/TAA）在核验时注意不同 GPU 后端（Vulkan/DX12）差异。

## AI Agent 测试工作流

- **单元测试**：布局图/WorldGrid 生成确定性（同 seed 同结果）、半平面内缩几何验证（OVERLAPS=0）、存档 round-trip（serde）。
- **截图冒烟测试**：Bevy 支持 headless 渲染到 `Image`（`Camera` + RenderTarget，禁 WinitPlugin），CI 中对关键场景（黄昏天空、街区、季节四态）截图断言非空/亮度分布，供 Agent 视觉自查。
- **调度随机化**：Bevy 0.20 的 Schedule Randomization 用 seed 随机化系统顺序做 property testing，暴露隐藏顺序依赖——天空/季节/Chunk 系统间显式 `.before()/.after()` 排序，不得依赖隐式顺序。
- **引擎内验证项**沿用原 dev-plan 的"待用户验证"清单形式，由 Feihei 人工确认。

## 当前状态

| 阶段 | 内容 | 状态 |
|------|------|------|
| R0 | 项目骨架 + 依赖锁定 + 版本验证 | 完成（2026-10-20） |
| R1 | 天空系统迁移（Atmosphere + bevy_elements + DuskSky） | 完成（2026-10-20；黄昏渐变饱和度待人眼迭代） |
| R2 | 布局图/WorldGrid + Chunk 流式（Rust 移植） | 完成（2026-10-21） |
| R3 | 放置/碰撞/季节（负形街道模型） | 完成（3.1-3.8；3.9-3.13 见下） |
| R3.9-3.12 | 程序化树 / 叶卡 / 立面与屋顶 / 观感修复 | 完成（2026-10-27 ～ 11-05，待实机验收） |
| R3.13 | 跨街上部元素（电线/悬挑灯臂/防护棚） | 实现完成（2026-11-09，待实机验收；见 adr-0006） |
| R4 | 存档与菜单 | 完成（2026-10-24；按钮/完成弹窗接线留 R5.6） |
| R5 | 声音与润色 | 待开始 |

## 风险

| 风险 | 影响 | 缓解 |
|------|------|------|
| bevy_elements 无人工审计 | 隐藏 bug/行为不符 | 引入前读源码；关键视觉参数用测试断言 |
| Bevy 版本迭代快 | 生态 crate 落后 | 精确锁版本；升级只做独立里程碑 |
| bevy_weather/bevy-volumetric-clouds 尚不支持 0.20 | 云方案受限 | 用 bevy_elements 的 CloudPlugin；后续生态跟进再评估 |
| 建筑模块仍待 Feihei 制作 | R3 建筑占位 | 沿用 Godot 版策略：BoxMesh 占位先行 |
| DoF/色差无内置全功能对应 | 后期效果栈缺口 | DoF 用内置 DepthOfField；色差用 0.20 新 `FullscreenMaterial` 自写（方案 B，见 adr-0003） |
