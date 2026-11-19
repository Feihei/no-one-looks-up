# 术语表（Bevy 迁移版）

日期：2026-10-19（自原 Godot 版 glossary 迁移，新增 Bevy 侧术语）

## 设计术语（沿用原项目）

| 术语 | 定义 |
|------|------|
| **区块 (Chunk)** | 世界划分的网格单元（50m×50m），用于程序化生成和流式加载/卸载 |
| **路径 (Path)** | 玩家可行走的区域，通常为线性街道，有宽度 |
| **节点 (Node)** | 路径交汇处或放大空间，如小广场 |
| **环线 (Loop)** | 可闭合行走的路径回路，避免死胡同，形成类似真实街区的散步路线 |
| **主干道十字 (Arterial Cross)** | 一条竖 + 一条横的 10-15 m 主干道（ADR-0009），落在地图中部的 lattice 线上（抖动 ±1 格），把地图切成 4 个象限；街区永不跨越它，象限间的接缝即主干道本身 |
| **街区单元 (Block)** | 路径网络中被街道围合的区域块，每个 Block 分配一种用途（建筑风格或开放空间） |
| **合并规模 (Merge Size)** | Block 优先生成算法中，每种 BlockType 对应的 N×N 基础网格合并规模（老城区 1×1，公寓 2-3×2-3，厂房 4-5×4-5） |
| **布局图 (Layout Graph)** | 由 seed 在开局时一次性生成的紧凑路径拓扑数据结构（Rust 结构体，serde 可序列化），包含路径骨架和 Block 标签 |
| **世界网格 (WorldGrid)** | seed → LayoutGraph 之间的 2D 中间数据结构，记录每个 cell 的 block_id、是否为道路、是否为路口，用于快速查询和存档序列化 |
| **负形街道模型 (Negative-street Collision)** | 最终碰撞策略：街区=实心不可见凸棱柱碰撞实体（y=0..2m），街区之间的缝隙=可步行街道（负形）；取代薄墙与每物体碰撞体 |
| **退让 (Setback)** | 建筑放置时从道路红线后退的距离，按 BlockType 差异化（老城区 1m，公寓 5m，厂房 10m） |
| **半平面内缩裁剪 (Half-plane Inset Clip)** | 街区多边形内缩的自写算法，数学上保证相邻街区实体不交叠（OVERLAPS=0） |
| **天际线 (Skyline)** | 由树木和建筑顶部在天空背景下形成的剪影轮廓，是主要观赏对象 |
| **风扰动 (Wind Perturbation)** | 风力对树木枝叶的动画影响，是核心视觉效果 |
| **季节 (Season)** | 影响树叶密度的全局参数，开局选定后单局不变；经共享 storage buffer 控制树着色器（leaf_density / leaf_color / sway_strength） |
| **探索 (Exploration)** | 玩家通过行走发现新区域的过程，所有区域都可被完全探索 |
| **探索位图 (Exploration Bitmap)** | 记录哪些 Chunk 已被玩家走过的位图，存入存档 |
| **种子 (Seed)** | 确定地图唯一性的随机数种子（rand_chacha），可随机生成或由玩家指定；同 seed 生成 bit-exact |
| **持久化 (Persistence)** | 已探索区域被保存，重新加载时保持不变 |
| **程序化材质 (Procedural Material)** | 通过 shader 生成的视觉质感材质，用于建筑/树皮外观调节 |
| **程序化天空 (Procedural Sky)** | 物理基大气散射实时生成的天空，包含动态云、星星、月亮 |
| **高空元素 (Sky Element)** | 天空中的动态点缀，如飞鸟、飞机、气球等（后期扩展） |

## Bevy 侧术语（新增）

| 术语 | 定义 |
|------|------|
| **Atmosphere** | Bevy 0.20 内置物理基大气散射组件（Rayleigh/Mie），加在场景 entity 上；渲染模式 LookupTexture / Raymarched |
| **ScatteringMedium** | 0.18 起的通用散射介质资产（吸收/散射系数 + 相位函数 + 高度 falloff），`earth()` 预设为地球大气 |
| **ElementsCamera** | bevy_elements 的相机标记组件，自动挂接 AtmosphereSettings/IBL/DepthPrepass/VolumetricFog/Exposure |
| **WorldClock** | bevy_elements 的世界时间组件（本地日期+小时+time_scale），本项目锁定 17:30 黄昏（time_scale=0） |
| **WeatherDirector** | bevy_elements 的天气强制/释放接口，本项目用于锁定晴朗低云量 |
| **DuskSky** | 自写 Resource，封装黄昏参数接口（云密度/星星可见度/月亮形态/时间与天气锁定） |
| **MaterialExtension** | Bevy 的材质扩展 trait，树的顶点色摆动（WESL 顶点着色器）与季节全局参数由此实现 |
| **共享 storage buffer** | 替代 Godot global_shader_uniform 的机制：一个 storage buffer 承载季节全局参数，所有树着色器直接读取，切换零开销 |
| **WESL** | WGSL 标准化扩展，Bevy 0.20 起的着色器语言；自写 shader 一律 `.wesl` |
| **BSN** | Bevy Scene Notation，0.19 起的场景定义宏（`bsn!`） |
| **运动学控制器 (Kinematic Controller)** | 自研玩家移动实现：无物理引擎，碰撞为"点在凸棱柱内"纯数学测试 |
| **调度随机化 (Schedule Randomization)** | Bevy 0.20 的 property testing 特性：随机化系统顺序暴露隐藏顺序依赖；本系统间排序须显式声明 |

## Bevy/原引擎对照（速查）

| Godot 版概念 | Bevy 版对应 |
|--------------|-------------|
| sky-3D 插件 + DuskSky 管理器 | Atmosphere + bevy_elements + DuskSky Resource |
| global_shader_uniform + RenderingServer | 共享 storage buffer |
| StaticBody3D + ConvexPolygonShape3D | 自研运动学控制器 + 凸棱柱数学 |
| signal | Message / Observer |
| CanvasLayer | Bevy UI |
| .gdshader | .wesl（MaterialExtension） |
| .tscn / .blend | BSN / glTF |
