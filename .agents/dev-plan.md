# 开发计划（Bevy 迁移版）

状态：草稿

日期：2026-10-19

> 计划以 checkbox 分阶段呈现。原 Godot 版 M0–M4 已完成，其成果以设计决策（ADR）形式继承；M5（声音与润色）未完成，顺延至 R5。每个阶段完成后：build + fmt + clippy + test 全绿，更新本文件状态，小步提交（Conventional Commits）。

---

## 阶段 R0: 项目骨架与依赖锁定（预计 1-2 天）

**目标**：搭建 Bevy 0.20 workspace 骨架，验证版本匹配与工具链。

- [x] 0.1 创建 Rust workspace：`Cargo.toml` 精确锁定 `bevy = "=0.20.0"`、`bevy_elements = "0.5.3"`
- [x] 0.2 `cargo check` 验证 bevy_elements 0.5.3 的 Bevy 依赖确为 0.20；不匹配则回报并回退评估（ADR-0005 方案 B）
- [x] 0.3 阅读并记录 bevy_elements 的 Cloud/Celestial/Fog/Climate 模块源码要点（无人工审计 crate，先读再信）
- [x] 0.4 项目脚手架：Plugin 结构（SkyPlugin / WorldPlugin / SeasonPlugin / SavePlugin / MenuPlugin）、`Result` 错误处理约定、CI 工作流（fmt/clippy/test 门）
- [x] 0.5 树资产链路验证：aspen_1 等 5 种树从 .blend 导出 glTF，Bevy 加载正常（⚠️ 原项目仅 3 种非备份树 .blend：aspen_1/tree_2/tree_3，已导出并通过 headless 加载测试；第 4/5 种树待 Feihei 提供 .blend）
- [x] 0.6 补丁升级 bevy 0.20.0 → 0.20.1（2026-11-05）：`cargo update -p bevy --precise 0.20.1`，只动 59 个 bevy 包 + ruzstd，45 个无关依赖不变。收益两条都对我们有效——① #25336 雾体积在相机贴近/进入近平面距离时被斜切或消失（R1 的 `VolumetricFog` 常开，黄昏雾霭会吃到）；② #25419 实体索引复用后祖先链遍历跳过活实体、导致 transform 不更新（R2 chunk 流式高频 despawn 的潜伏雷）。clippy（`--all-targets -D warnings`）/ lib test 204 项 / assets_load / `sky_screenshot --ignored` 全绿

### 边界方案（2026-10-21 用户决策，偏离原版）

- **最外圈不再是 Block**：原版 `OUTER_BAND_CELLS=1` 把最外一圈按街区生成（含 is_outer 建筑、碰撞）。改为：最外一圈 cell 不分配 Block（`SCENERY_BAND_CELL`），只放景物（建筑背面/树），由同一个 Chunk 系统流式加载；真正挡人的是一圈实心碰撞环。
- **理由**：外带仅 50m 深，扣掉半街宽剩 46m，无法吸收内部街区扰动；直线边界与扰动顶点必然相撞。改为景物带后环路可沿格点走直线。
- **几何**：环路中心线在内部第一圈格点上（`x=50`），路辐 4m → 可走 `x=46..54`；景物带 `x=0..46`，靠环路 12m 布置剪影。
- **待办**：R3 需补景物带内容（建筑背面/树）与实心碰撞环；`is_outer` 相关放置规则随之调整。


- `cargo run` 空窗口启动，无 warning；fmt/clippy/test 全绿
- glTF 树在场景中正确显示

---

## 阶段 R1: 天空系统迁移（预计 2-3 天）

**目标**：用 Bevy 0.20 Atmosphere + bevy_elements 复现原 M1 的黄昏氛围。

- [x] 1.1 集成 bevy_elements：`ElementsCamera` + Atmosphere/EnvironmentMapLight/DepthPrepass/VolumetricFog/Exposure 自动挂接
- [x] 1.2 编写 `DuskSky` Resource：时间锁定 17:30（`WorldClock` time_scale=0 + paused）、天气锁定晴朗（`WeatherDirector::force(WeatherPreset::Clear, Duration::ZERO)`）、黄昏参数接口（cloud_cover/cloud_layer 覆盖 + 曝光基准；星星可见度为保留接口——星场内置于 CelestialPlugin 无公开 API，黄金时刻被亮天空掩盖属预期）
- [x] 1.3 配置黄昏色调：物理散射 + `Exposure { ev100: 13.0 }` + ACES + Bloom（intensity 0.3）；选定 date=2026-03-21 + lon=125.0 → 17:30 太阳 3.5°/月亮 36°；实测：太阳盘+光晕+暖地平线带+云剪影暖镶边+冷蓝顶渐变（橙红饱和度/天顶紫调待 Feihei 人眼迭代，估 60%）
- [x] 1.4 裁剪 bevy_elements 未用模块（Wind/Precipitation/Lightning/Dust/Terrain/Hydrology/Water/GroundCover/Weathered 共 9 个 disable，保留 Climate/Celestial/Fog/Cloud）
- [x] 1.5 天空动态变化：世界坐标噪声（sum-of-sines，确定性）驱动 exposure 微扰 ±0.15 ev100；ScatteringMedium 密度乘子受 GPU LUT 资产限制（每帧重建不可行），记录在笔记中
- [x] 1.6 后期效果：Bloom/ACES 内置配置；CA 用 `FullscreenMaterial` 自写（延后至 R5 润色）
- [x] 1.7 headless 截图冒烟测试：`tests/sky_screenshot.rs` 黄昏天空帧断言（HDR 带分析：渐变/暖地平线/非黑；本地 GPU `--ignored` 运行，CI GPU-less runner 不能跑）

### 可验证

- 抬头看到动态黄昏天空（物理散射，非 HDRI）
- 天空中有云、星星、月亮
- 黄昏色调自然，后期效果好看；同一 seed 场景截图逐帧一致

---

## 阶段 R2: 布局图与 Chunk 系统（预计 3-5 天）

**目标**：移植原 M2/M3 生成算法到 Rust，实现 Chunk 流式加载。

> ⚠️ **状态修正（2026-10-21）**：2.1–2.8 交付的是"规整网格基线"，**未复刻原 Godot 的扰动路网**。已在原项目源码 `E:/play/Godot/heads_up/scripts/` 中逐项核对，差异列入下方 2.9–2.15。这些项完成前，R2 不算完整移植。

- [x] 2.1 `layout_graph.rs`：LayoutGraph 数据结构（节点/边/宽度、Block 标签、边界，serde）；plaza 块中心作为图节点（放置锚点）
- [x] 2.2 `world_grid.rs`：2D 网格中间结构（cell 记录 block_id/is_road/is_junction，`get_cell` 快速查询，serde）；is_road=贴街 cell，is_junction=含图节点的 cell
- [x] 2.3 `path_network.rs`：Block 优先生成。**修订（2026-10-21）：改为完整矩形剖分**——行序扫描取「区域内最大自由方块」，装不进目标类型的最小尺寸就降级为 1×1 老城区。原 carve+回退会留下 L 形残余块，其凹角在扰动+内缩下翻转朝外，实测 280 对穿插 / 最深 20.8m；剖分保证每格恰属一个矩形，无残余即无凹角。类型按面积配额抽样（见 2.12）
- [x] 2.4 种子系统：`rand_chacha::StdRng::seed_from_u64(seed)`，同 seed bit-exact 单元测试；`thread_rng` 不进入生成路径
- [x] 2.5 `chunks.rs`：50m×50m Chunk（16×16=256，与 cell 1:1 对齐），不对称半径（前方 4 / 后方 2），窗口最坏 8×8=64 格流式 spawn/despawn（2026-10-26 地图缩至 0.8km 后仍为 16×16=256 Chunk）（0.20 递归 `despawn` 组删除）+ 每 Chunk 地面 tile（seed 着色）供流式反馈 ⚠️ 原版为「切比雪夫距离 + 前后点积判定」，当前为旋转矩形窗口，略多加载；对齐见 2.15
- [x] 2.6 几何验证单元测试：半平面内缩裁剪（`inset_polygon_clip`）+ SAT；OVERLAPS=0 断言（4 个 seed 全块对）
- [x] 2.7 调试可视化：Bevy Gizmos 绘制 Chunk 边界/街道中心线/Block 轮廓（按类型着色）+ Text2d Block 标签；G 键开关（debug 构建默认开）
- [x] 2.8 系统排序显式声明（`.before()/.after()` 链）+ 调度随机化 property testing（16 种子随机注册顺序 + 歧义检测 = Error）

### 修正项（复刻原 Godot 算法，2.9–2.15）

> 对照 `scripts/path_network_generator.gd`（466 行）/ `object_placer.gd` / `chunk_manager.gd`。按依赖顺序实施，**必须在 R3.1 放置之前完成**（放置沿 Block 边缘排布，边界不扰动则建筑会是整齐直线）。

- [x] 2.9 **扰动网格**：内部格点随机偏移，边界节点两轴均固定（原版只锁单轴，上/左边界仍会滑动，导致外墙off-border）。`PERTURB_MAX` **取 4m**（非原版 12m）：50m 格距下 12m 双向扰动使相邻格点相对偏移达 24m、共享边偏角 53°，按半街宽内缩会产生 5.4m 横向误差 > 街宽 → 实测 280+ 对穿插、最深 20.8m；4m 下偏角约 22°、误差 1.6m，街道仍有可见摆动（格边 34-68m）。**修订（2026-10-24，R3.8 步行测试发现）**：第一圈内部格点（环路中线，row/col 1 与 axis-2）同样钉死——它们被扰动会让环路内侧块越过中线，自由带宽实测低至 0.3m 甚至为负。**另修**：`LayoutGraph` 节点坐标改用扰动后格点（原用基准格点，街道中线与块体内缩错位，同样挤占街道）；`street_unit_edges` 的单位中线与 `associate_edges_with_blocks` 的左右归属随之改按扰动格点/块体几何侧判定
- [x] 2.10 **沿周长构多边形**：按 上→右→下→左 取全部边界节点（合并块 `2*(rows+cols)` 顶点）。**前置条件改为矩形剖分**（见 2.3 修订）：只有块是纯矩形时，周长才是无凹陷的凸四边形；L 形残余块的凹角会让内缩翻转朝外（实测吞掉相邻块 10m）
- [x] 2.11 **内缩**：四边统一 `w/2`，按**顶点角平分线**偏移（`1/cos(θ/2)`，上限 1.5 防尖角外冲），保证扰动下仍严格内缩。原「逐边半平面裁剪 + 斜接」在扰动多边形上会把角落推出中心线，已替换
- [x] 2.12 **类型比例修正**：按**面积**配额而非块数——改前厂房仅占 8% 块数却吃掉 45-50% 面积（ADR-0001 的 50/30/20 指观感面积）。现为 广场 6% / 树林 20% 抽签，建筑类按面积偏置（老城 0.87 / 公寓 0.10 / 厂房 0.03）；实测老城 46-50% / 公寓 21-32% / 厂房 10-21% 面积占比
- [x] 2.13 **Block 元数据补齐**：`min_y/max_y` 层数范围（老城 1-3 / 公寓 4-6 / 厂房 6-10 / 广场·树林 0）、`left_block/right_block` 边->Block 关联（`_associate_edges_with_blocks`）、`world_grid` 挂到 graph（供放置阶段查询）
- [x] 2.14 **测试加固**：新增格点扰动断言（内部 95%+ 移动、边界双轴钉死、同 seed 复现）；OVERLAPS=0 判据从 SAT 深度改为**真实穿插**（顶点包含）——相邻扰动边相距毫米级时 SAT 会假阳性（实测 6-7 对假阳性、真实 0）；边界断言改为「景物带 = 最外圈、内部 cell 必属某块」
- [x] 2.15 **Chunk 窗口对齐**：改为切比雪夫半径 + 前后点积（`dot >= 0` 用 FRONT=4，否则 BACK=2），替换旋转矩形窗口；并补 `CHUNKS_PER_FRAME = 1` 分帧加载/卸载（`pending_load` / `pending_unload` 队列，就近优先），避免瞬移时单帧建 60+ Chunk。窗口随朝向改变（正对 +X 时东侧达 4 格、西侧仅 2 格），单测锁定

### 可验证

- 调试模式可见路径网络与 Block 分布
- 移动时周围物体出现/消失，无明显卡顿
- 同一 seed 每次生成的地图完全一致（bit-exact 测试）
- 几何测试 OVERLAPS=0
- **（2.9 之后）顶视图不再是笔直曼哈顿网格，街区边界有自然错位；`examples/world_map.rs` 可直接出图比对**

### 实现备注

- ~~原始 GDScript 源不可得~~ **更正（2026-10-21）**：原项目源码在 `E:/play/Godot/heads_up/scripts/`，已逐项核对。2.1–2.8 是按 ADR 不变量重实现的规整网格版，2.9–2.15 负责补齐与原版的一致性
- 无死胡同的构造性保证：完整矩形 partition 下内部格点街道度数不可能为 1，band 内部无街道使边界格点度数为 0；扰动不改变拓扑（只移动节点），该保证依然成立
- R3 碰撞可直接用 Block inset 多边形：扰动后为凸四边形（扰动矩形），半平面裁剪与 SAT 逻辑复用；**但前提是 2.10 的周长取点已落地**，否则相邻块共享边不重合，碰撞体会留缝或穿插
- 顶视图工具：`cargo run --example world_map`（CPU 光栅化，4000×4000，2px=1m，不依赖 GPU）

---

## 阶段 R3: 放置、碰撞与季节（预计 3-4 天）

**目标**：移植原 M3 的放置/碰撞/季节决策（负形街道模型最终形态）。

> **前置依赖（硬性）**：3.1 与 3.3 依赖 R2.9–R2.11（扰动网格 + 沿周长构多边形 + 逐边 inset）。建筑沿 Block 边缘排布、碰撞体取 Block 多边形，若 Block 边界仍是未扰动的直线，放置结果与碰撞围合都会偏离原版。**必须先完成 2.9–2.11 再开 R3**。

- [x] 3.1 `placement.rs`：沿 edge 统一放置算法 + 差异化参数表（老城区 4-6m / 公寓 25-40m×12-14m / 厂房 50×50m）+ setback（1/5/10m）+ 朝向（atan2）+ edge 中点去重；层数取 2.13 的 `min_y/max_y`
- [x] 3.2 树木放置：5 种 glTF 懒加载，行道树概率 20/60/80%，FOREST 密 8m，PLAZA 稀疏，树不带碰撞
- [x] 3.3 `collision.rs`：负形街道模型——每个实心 Block 一个不可见凸棱柱（`block.polygon`，y=0..`BLOCK_SOLID_HEIGHT`=2m），按 50m chunk 分桶索引（`CollisionIndex`，与 chunk 流式同步注册/注销）；`resolve(pos, PLAYER_RADIUS)` 沿最浅穿透轴推出，迭代 16 次 × 3×3 邻桶以处理角点连锁。规则：PLAZA 跳过、内部 FOREST 跳过、其余生成（940 块中 804 块实心）。景物带另由逐 chunk 实心环封闭。调试可视化按类型着色画出碰撞轮廓。验证：可走面积 26.2%，从街道朝 721 个实心块内部行走 0 次成功进入

> **与 ADR-0004 的差异**：ADR 描述的是「原版逐 Block 生成 `StaticBody3D` + `ConvexPolygonShape3D`」。本项目按 ADR-0005 不引入物理引擎，改为纯数学碰撞（凸多边形包含 + 最小平移推出），由 R3.4 的运动学控制器消费；语义等价（不可见、实心、不渲染、无 Draw Call）。
- [x] 3.4 自研运动学玩家控制器（`src/player/mod.rs`）：A/D 以 60°/s 转身、鼠标仅纵向控制视角并钳制仰角 22.55–60°（底边掠过地平线；鼠标水平移动不控制旋转/朝向）、W/S 沿朝向行走、E/Q 调速 1.2–2.0（0.1 步进，加速 8 m/s²）、视高 1.65m、无跳跃/跑步/FOV。运动按 ½ 半径细分子步后交 `CollisionIndex::resolve`，高速不穿墙。Esc 释放/重新捕获鼠标。替换 R2 临时调试移动；删除 R1 的 `aim_at_sun_once`（两个系统写同一 Transform 会构成隐式排序）

> **碰撞修复**：合并块内缩后的周长会出现轻微凹陷（实测顶点 3/5/7 的叉积为负），`push_out` 依赖凸性，导致 770 次高速穿墙尝试中有 13 次进入块内。改为对 `convex_hull(polygon)` 做碰撞后降为 0 次；代价是碰撞体比可见轮廓大出凹陷深度（±4m 扰动下可忽略）。已加回归测试。
- [x] 3.1 放置偏差（2026-10-22）：**完整放置**——原版 ADR footprint（公寓 25-40×12-14m / 厂房 50×50m）只有块深 ≥ `w/2 + setback + depth` 时成立，本项目剖分可给「厂房」单 cell（内缩后仅 ~39m），参数表尺寸放不下。改为：逐边采样整排建筑可用深度（`block_depth_along_pub`，从外部入射的射线扫描，非凸多边形安全），先试参数表尺寸，放不下降级为 4-6m 模块节奏，整排放不下再丢弃。`LayoutEdge` 补 `left_block/right_block`（2.13 遗留）；`street_segments` 改为**每 unit edge 一条 segment**（原合并成 1900m 长边会让放置只有 1 个锚点，整条街正面永远放不出）。三重剔除保留（走廊 / block 凸多边形 + 凸包 / 地图边界）；树资产仅 3 种，按可用集合抽样
- [x] 3.2 树木放置偏差：3 种 glTF 懒加载（`WorldAssetRoot` + `GltfAssetLabel::Scene(0)`），行道树概率 20/60/80%、FOREST 8m 密、PLAZA 稀疏（上限 2 棵）；树不带碰撞；资产未就绪时该 chunk 无树（`trees_ready` 标记，重 plan）
- [x] 3.5 `season.rs` + 树 `MaterialExtension` WESL：共享 uniform 承载 leaf_density/leaf_tint/sway_strength；保留顶点色 R 通道摆动接口（2026-10-29 完成，见下方 R3.5 小节）
- [x] 3.6 建筑立面（R3.11 步骤 11.1–11.2，2026-10-31）：程序化立面贴图 + 玻璃反射 + 室内灯光
- [x] 3.6c 屋顶轮廓（R3.11 步骤 11.3，2026-10-31）：双坡/女儿墙/锯齿
- [x] 3.6d 建筑零件（空调/雨棚/落水管/阳台/水箱/烟囱）与街道家具（路灯/电线杆/电线）——R3.11 步骤 11.4–11.5（2026-11-04，见 R3.11c 小节）
- [ ] 3.9 **程序化树形态原型（步骤 1–2）**：现状是半透明 alpha 卡片，剪影偏“纸片”。新建 `world/tree_shape.rs`（纯数据 L-system/BFS 生长模型，`StdRng` 确定性）产出「枝条骨架 + 叶簇球」；`plugins/tree_preview.rs` 用 Gizmos 在出生点前方摆 3 物种 × 4 变体画廊，F7 开关；`examples/tree_gallery.rs` headless 渲染对照图（自写 PPM→PNG，无新增依赖），`examples/tree_stats.rs` 输出高度/宽度/枝数/代理三角数。
  - [x] 步骤 1：骨架生成器 + 确定性/预算单元测试（4 个测试：同 seed 复现、跨 id/物种不同、预算区间、逐级变细）
  - [x] 步骤 2：Gizmos 画廊 + F7 开关 + headless 对照图
  - [x] 步骤 3（2026-10-27）**程序化树接入 chunk 流式**：新建 `world/tree_mesh.rs`，把 `TreeSkeleton` 烘焙成**单棵树一个 Mesh**（`ConicalFrustumMeshBuilder` 5 边锥台作枝 + 20 面 icosphere 作叶簇，`Mesh::merge` 合并），黑色剪影材质（roughness 1 / reflectance 0）。`chunks.rs` 的 `PlacementAssets` 改为持有 3 物种 × 4 变体 = **12 个共享 mesh**，替换原 glTF `WorldAssetRoot`；树随 chunk root spawn/despawn
  - [x] 步骤 4（2026-10-27）**LOD 三级 + 距离裁剪**
    - **先量后改**：叶簇占单树三角面的 **85-90%**（183 簇 × 20 面 = 3660，枝仅 930），所以分级只动叶簇，枝的影响可以忽略
    - `TreeLod::{Near, Mid, Far}`：Near 全量；Mid 叶簇每 3 取 1 且丢弃 <0.3 m 小球、枝半径下限抬到 6 cm；Far 叶簇每 14 取 1、球放大 1.55 倍补回轮廓、不要枝
    - 实测单树均值：**Near 2952 / Mid 1238（-58%）/ Far 183（-94%）** 三角面
    - 距离边界 **40 / 110 / 220 m**（`TreeLod::BOUNDARIES`）。220 m 的裁切点对应 `CHUNK_AHEAD` 的 200 m 视距，最远一级只覆盖玩家真正看得见的那段
    - 用 Bevy 0.20 的 `VisibilityRange::abrupt` 实现：每个 LOD 是**同位置的一个独立实体**，各自带可见距离区间。Near 不挂 range（`VisibilityRange` 无法表达"从 0 m 开始"的开区间）
    - 整窗 197 棵树的可见三角面估算：**无 LOD 494k → 有 LOD 141k（28%）**
    - 测试：`lod_tiers_actually_get_cheaper`（每级必须严格更省，Mid < 75% Near、Far < 50% Mid）、`lod_ranges_tile_the_distances_without_gaps_or_overlap`（任一点恰有一级可见、超出 220 m 全部裁剪）、`every_tree_spawns_all_lod_tiers_with_ranges`（每棵树三级齐全且 range 正确）。已验证**只生成 Near 时测试即失败**
    - 实测运行：36 个树网格（3 LOD × 3 物种 × 4 变体），零告警
  - **实测（seed 0x48454144，4 变体均值）**：杨 73 枝/208 簇/约 5.0k 代理三角、冠高 10.1–11.9 m；行道树 46 枝/135 簇/约 3.3k、冠高 6.6–8.9 m；宽叶 27 枝/168 簇/约 3.7k、冠高 8.1–11.2 m。均为不透明几何预算，远低于 alpha 卡片的 overdraw 成本。
  - **调参轮（2026-10-27，用户选方案 A：先定形态再做实例化）**：
    - 对照图改为**每物种一张等轴测图**（无灭点，4 变体按列对齐），列距 = 实测冠宽 + 2.5 m；此前单一透视相机把三行压成一团、宽叶冠互相粘连
    - 新增 `crown_floor`（按每棵树自身冠底切掉下部簇，消除平直“裙边”）与 `leaf_density`（按位置哈希确定性抽稀，留出透气缝）
    - 叶簇改为**沿枝轴放置**（原为绕枝尖散布，抽稀后最外圈会变成悬浮碎块）；新增 `leaf_spread` 控制离轴漂移上限
    - 抽稀引入**邻域密度加成**：孤立球最难保留（消悬浮碎块），密集处优先保留
    - **修正 `crown_width()` 不含叶簇球半径**：原只统计枝条端点，低估每侧最多一个球径；对照图按该低估值排列距，导致最宽物种仍相互接触（已加回归测试）
    - 所有物种开启 `leaf_run_spacing`（沿枝身布簇，上限 6/枝），让叶团跟随枝条深入冠内
    - 实测（4 变体均值）：杨 72 枝/190 簇/4.7k、行道树 40 枝/130 簇/3.1k、宽叶 26 枝/112 簇/2.6k 代理三角
    - 测试 84 通过（新增 `crown_width_includes_leaf_radius`、`thinning_keeps_crowns_dense_and_grounded`）
  - **第二轮（2026-10-27，Feihei 反馈：线框看不到真实效果 + 树干太长、树冠要更贴近相机）**：
    - **预览改为实体网格**：`TreePreviewPlugin` 不再用 `Gizmos`，每根枝一个低模锥台（6 边，`ConicalFrustumMeshBuilder`），每个叶簇一个低模二十面球（`ico(1)`），近黑剪影材质（roughness 1、reflectance 0）。每棵树一个实体，画廊挂在单一根实体下，关 F7 一次性 despawn
    - **画廊贴近相机**：首排 9 m（原 16 m），行距 17 m / 列距 15 m
    - **树干缩短 + 冠底下压**（Feihei 核心诉求）：杨 `trunk_length` 0.42→0.17、`leaf_min_level`→2；行道树 0.34→0.24；新增 `leaf_start`（叶从枝条多深处开始）
    - **修总预算不守恒**：`level_share` 是每层量，但深层枝数远多于浅层（行道树 L2 8 支 vs L3 25 支），按层预算分摊导致深层枝反而更长（实测 L2 0.96 m vs L3 1.02 m）。改为补偿枝数增长的平方根
    - **实测冠底**（眼高 1.65 m）：杨 1.8–2.9×、行道树 1.1–1.6×、宽叶 1.0–1.9×；原杨为 3.3×，玩家需仰角 60° 以上才能看到冠
    - 三角代理：杨 4.5k、行道树 2.0–3.9k、宽叶 1.8–2.6k（每棵）
  - **取舍记录**：杨树的「露出 Y 形分叉」与「低冠底」在本生长模型下互斥——露出分叉要占 2–3 m 高度，会把冠底推回 3× 眼高以上。**按 Feihei 的明确要求优先压低树冠**，放弃强制露分叉（宽叶因低分叉设定天然可见）
  - **第四轮（2026-10-27，Feihei 反馈：树不随 chunk 加载，走出范围就没树，要按两次 F7 才刷出来）**：
    - 根因：R3.9 步骤 2 的画廊是**一次性快照**（按 F7 当帧玩家位置建好，之后不再更新），它只是形态调试工具，本来就不该承担世界树的职责
    - 修复：把程序化树接入 `chunks.rs`，成为 chunk 内容的一部分——chunk root despawn 时树一起消失
    - **网格预算**：初版叶簇 80 面 + 枝 6 边 → 全窗口约 **262 万三角面**，超 ADR 性能预算。改为叶簇 20 面（ico 0 级）+ 枝 5 边 + 剔除半径 <3.5 cm 的细枝 → **约 72 万**，最重一棵 4392 面，13 棵树/chunk = 5.7 万面
    - **去掉 glTF 依赖**：`build_placement_assets` 不再需要 `AssetServer`（树是程序化烘焙的），顺带让 headless 测试第一次能真正验证树的流式加载
    - 新增 `procedural_trees_stream_with_their_chunks`：断言树在出生点生成、每棵只有 1 个 `Mesh3d`、网格非空、**传送到 300 m 外后树跟着走且没有一棵留在原地或脱离 chunk root**。已验证**把“树挂到 chunk root 外”的 bug 放回去该测试即失败**（报 184 trees not attached）
  - **第五轮（2026-10-27，Feihei 反馈：仍需按两次 F7 才加载新区域的树；并清理临时文件）**：
    - **真实原因：Feihei 看到的一直是 F7 画廊，不是世界里的树**。实测确认 chunk 树一直正常流式（走 100 m 后 chunk 范围 4..11 → 6..14，树 191 → 230）；而画廊是固定快照，走出范围就空、按两次才会在新位置重建——现象完全吻合
    - **处置：F7 画廊从 `GamePlugins` 注销**。它已完成使命（形态定稿），世界树由 chunk 提供；模块保留作为 `examples/tree_gallery.rs` 的形态参照
    - **顺带修掉两个每帧泄漏（严重）**：
      1. `build_placement_assets` **没有 guard，每帧重跑**——每帧重烘 12 个树 mesh、重建全部建筑材质、替换 `PlacementAssets` 资源。实测 240 帧：mesh 13 → 3242、材质 15 → 3411。加 `existing.is_some()` 早退后稳定在 64/65
      2. `spawn_chunk_content` **每个建筑都 `meshes.add(Cuboid)`**——改为共享单位立方体 + 每实例缩放，`spawn_chunk_content` 不再需要 `Assets<Mesh>`
    - 新增 `placement_assets_and_meshes_are_allocated_once`：跑 180 帧断言 mesh/材质数量不增长。已验证**去掉 guard 该测试即失败**（报 "mesh assets grow every frame"）
  - **第六轮（2026-10-27，Feihei 反馈：F7 无变化、走动旋转都看不到树，建筑盒子也不见了）——这是真正的根因**：
    - **`ChunkRoot` 实体没有 `Transform`/`Visibility`**。它是所有 chunk 内容的父节点（地面 tile / 建筑 / 树），而 Bevy 的层级传播要求父实体带 `Transform`（→`GlobalTransform`）。缺了它，**所有子节点拿不到世界变换，一个都渲染不出来**。而 gizmo 线框不走层级，所以「天空 + 彩色线框正常、网格全空」——症状完全吻合
    - 发现方式：抓真实游戏日志，看到 **1120 条 `warning[B0004]`**（"parent ... without [Transform]"），远早于这次改动就存在——**之前一直是 glTF `WorldAssetRoot` 树 + 本轮之前没人看日志**才没暴露
    - 修复：`ChunkRoot` 补 `Transform::default()` + `Visibility::default()`。实测 25 s 运行：B0004 从 1120 → **0**，无其它告警
    - 新增 `chunk_root_can_propagate_transforms_to_its_children`：断言每个 chunk root 都有 `Transform`、每个子节点都拿到 `GlobalTransform`。已验证**去掉修复该测试即失败**（报 30 chunk roots have no Transform）
    - 顺带删掉 `PlayerBody::clamp_yaw_offset` 及其测试（鼠标横向转向已移除，该函数不再被调用；`yaw_offset` 字段保留，相机旋转仍用）。至此 `cargo clippy -D warnings` 全绿、零告警
  - **仍未解决**：冠内枝干在渲染中不可见（叶簇球覆盖枝段，需实例化阶段用材质/明暗区分）；行道树 V2 树干偏长、组内一致性差
  - **第三轮（2026-10-27，Feihei 反馈：按 F7 后连线框都没有）——两个叠加 bug，都不是材质/背面剔除问题**：
    - **Bug A：空缓存提前返回**。`sync_tree_preview` 里 `if !roots.is_empty() || data.trees.is_empty() { return; }`——打开 F7 的当帧 `data.trees` 必然为空，条件成立直接返回，画廊从未被建造。删掉 `|| data.trees.is_empty()`
    - **Bug B：锚在世界原点**。`TreePreviewGalleryData::generate(Vec3::ZERO)` 固定用世界原点，而玩家出生在地图中心 400 m 处 → 树在 588 m 外。改为以玩家位置 + 水平朝向为锚（抽出 `gallery_origin()` 便于测试）；变体沿玩家右手方向横排，不再朝前堆叠
    - **为什么之前“验证过 618 个 mesh 存在”却仍是空的**：headless 测试只统计了实体数量，没检查它们在世界中的位置；而且那两个 bug 都在 `sync_tree_preview` 里，测试却只调 `generate()`，属于“测了错的层”
    - 新增 `f7_toggle_actually_builds_the_gallery_in_front_of_the_player`：**驱动 `sync_tree_preview` 本身**，断言能建成、距玩家 <80 m、在朝向前方、立于地面、变体横向排开。已验证**把任一 bug 放回去该测试即失败**（Bug A 报“built no gallery”，Bug B 报“588 m from the player”）
  - **验证缺口（重要）**：`tests/tree_preview_screenshot.rs` 的 `frame` 在本机 headless 精简插件栈下恒为清屏色，即使换成 HDR 目标 + 完整 `GoldenHourCamera` bundle 也不出画面，与 `sky_screenshot.rs` 同栈却正常，差异未定位。**因此「游戏内观感」只有 Feihei 本人 `cargo run` + F7 才是有效证据**；本文所有形态结论均来自 `examples/tree_gallery.rs`（纯 CPU 光栅化）。
  - **已知验证缺口**：`tests/tree_preview_screenshot.rs` 在精简插件栈下能跑通（F7 生效、12 棵树生成），但截图为空——HDR 相机在这套栈里没有产出画面（`sky_screenshot.rs` 的栈可正常出图，差异未定位）。**桥接测试只覆盖逻辑层，不覆盖“游戏内可见”**；游戏内观感待 Feihei 本地按 F7 人工确认。
- [x] 3.7 季节切换快捷键（1/2/3/4，开发期）+ 菜单 Tab 选择（2026-10-29，见 R3.5 小节）
- [x] 3.8 headless 验证（碰撞围合部分）：`walking_every_street_reaches_the_far_end`——沿全部路网边（2112 条 × 4 seed）按 0.5m 步长模拟步行，断言不卡死、不穿块、不出世界、单调前进。**该测试暴露并驱动修复**：环路中线格点扰动、图节点坐标错位、左右归属按基准格点判定、row_fits 与放置偏移不一致（完整链路见 2.9 修订）。修复后全城建筑 2787 → 10091（此前静默丢弃 ~90% 放置）。四季树形态部分随 3.5 顺延

### R3.5 季节系统（2026-10-29，方案 A：顶点色 + MaterialExtension）

**与原 ADR-0004 的关键差异（必须记录）**：ADR 的季节是建立在 Godot **alpha 卡片树叶**上的（`ALPHA = albedo.a * leaf_density`，冬天乘 0 让叶面透明）。R3.9 之后树是**不透明程序化网格**（枝干锥台 + 叶簇二十面球合并成单 mesh），没有贴图也没有 alpha 通道，那条路径已不存在。

差点走错的一步：直接用顶点 alpha 做密度。**Bevy 的深度预通道只按「材质基色」判 alpha 丢弃**（`pbr_prepass_functions.wesl` 的 `prepass_sample_color_and_alpha_discard`），从不读顶点色；而体积雾与云的 pass 都读这份预通道深度。用顶点 alpha 会在颜色通道抹掉叶子、却在深度缓冲里留着实体 —— 雾和云会在「看不见的叶子」周围被切出空洞。

**实际做法：密度用几何塌缩，不用 alpha。** 烘焙时把「顶点到本叶簇中心的偏移」写进 `UV_0.xy` + `UV_1.x`，顶点着色器按 `(density - 1)` 缩放该偏移：密度 1 是原样，0 时整簇收成一个点，只剩枝干骨架 —— 正好是 ADR 要的冬季观感，且塌缩发生在写顶点位置之前，主通道/预通道/阴影通道天然一致。

- `world/season.rs`：四季参数表（逐值对齐 ADR-0004 §6）+ `SeasonState` Resource。`foliage_tint()` 归一化到**单位亮度**（GPU 侧是乘子，绝对亮度由材质基色提供），`foliage_linear()` 供测试与文档读绝对值
- **修正一处 17 倍错误**：`SILHOUETTE_LUMINANCE` 最初按 sRGB 数字（0.042）写，但 `Color::srgb(0.030,0.038,0.032)` 的**线性**亮度是 0.0028；照原样会把整片树点亮 17 倍，直接违反 ADR-0001 的剪影观感。已加测试把常量与被测材质对齐
- 顶点色通道（ADR 的「艺术接口」保留）：`r` 摆动权重（叶 ~1.0 / 枝 ~0.12，随高度增益）、`g` 叶掩码（0=枝干，永不塌缩）、`b` 每簇相位（相邻叶簇不同步）
- `assets/shaders/tree_sway.wesl`：**一个文件同时服务主通道与预通道**，靠 `PREPASS_PIPELINE` def 切换 io/裁剪空间。文件唯一性就是「两通道位移逐位一致」的结构性保证——前向管线对预通道深度用 `GreaterEqual` 测试，只要主通道把顶点放到预通道后面一点，就会成片丢弃
- 片元**不接管**：继续走内置 `pbr.wesl`。`pbr_fragment.wesl` 本来就有 `base_color = in.color * material.base_color`，所以季节色作为顶点色进入光照，光照/距离雾/色调映射/延迟路径全部原样保留
- `plugins/season.rs`：`SeasonUniform`（32 B，两个 `vec4`）+ `TreeSeasonExtension`（binding 100）+ `push_uniform`。**时钟也放在这个 uniform 里**（`sway.w`）：`globals.time` 在前向是 binding 11、预通道是 binding 1，一个文件无法两处都声明
- 零开销切换：全天下的树共享**一个** `Handle<TreeSeasonMaterial>`，切季只改这一个 32 B uniform（ADR 要求「禁止逐材质实例遍历」）；代价是每帧一次 32 B 上载——省掉的是遍历，不是上载
- 菜单：主菜单显示 `Season: X [Tab]`，Tab 循环；开局把季节写进 `SaveData`；读档时 `SeasonState` 随之恢复（R3.5 前该字段恒为 `Summer`，等于读档会把秋天变回夏天）
- 开发快捷键 1/2/3/4 **只在非游戏态生效**（ADR-0001：单局季节不变）
- **替换了 `capture_current` 的七个参数**：改为 `SaveInputs`（`SystemParam`），自动存档与手动存档共用同一份采集逻辑（此前两条路径各自硬编码 seed/season，正是它们分叉的来源）

**测试（新增 20 个，总数 141）**：参数表逐值对齐 ADR；冬季唯一无叶；四季亮度都落在剪影预算内；四季色互不相同；`foliage_tint` 亮度恒为 1；uniform 32 B/16 B 对齐且字段偏移正确；时钟推进；切季确实改写共享材质；顶点色与 UV 打包覆盖每个顶点；叶/枝可区分；偏移确实以簇心为原点；摆动权重随高度增长；shader 单入口；shader 的 uniform 声明与 Rust 侧一致；shader 不收枝干；shader 时钟不来自 globals；存档往返带季节（已双向验证：改回旧行为即失败）；读档先恢复季节再流式；菜单 Tab 选季并带进 run；单局内不可改季。

**踩到并修复的两个 shader 问题（都只在运行时暴露，cargo 无能为力）**：
1. `import package::prepass::io` → 报 `module not found`。`package::` 指向「拥有该 shader 的 crate」，而本项目 shader 的 module path 是 `shaders::tree_sway`。必须写 `bevy_pbr::`（与 `cirrus.wesl` 一致）
2. `decompress_vertex` 只在预通道的 io 模块里 → 共用代码里无条件调用会找不到声明。改为按 `PREPASS_PIPELINE` 分支取 `v`

**验证缺口（重要）**：`cargo run` 已实测启动零 shader / 零管线创建错误，证明两条通道的着色器都编译通过、管线都建起来了（树在主菜单下就已流式存在，管线必然被创建）。但**四季的实际观感、摆动幅度、冬季裸枝是否好看，仍需 Feihei 本地 `cargo run` + 菜单 Tab/1-4 亲自确认**——截图冒烟栈在 R3.9 就已被证明不出画面，本项沿用该结论，不作自动化视觉断言。

### R3.10 叶片卡片（2026-10-30，Feihei 反馈：树冠是一团实心叶子）

**需求升级**：ADR-0001 的重点是「抬头看到被枝叶轮廓剪碎的天空」，且要能分辨树种（槐/榆羽状复叶、杨尖塔锯齿、柳垂枝细叶）。近黑材质可以简单，**轮廓必须真实**。

**为什么几何做不到**：一片叶 5–15 cm，一冠数万片。要做到「看得清叶片形态」，每片叶至少十几个面 → 3 万叶 × 24 面 ≈ **72 万面/棵**。中间没有可用档位：叶少了不成荫，叶多了爆预算。所以叶片尺度不该由几何承担。

**真正的根因不是细分不够，是叶簇球太大**（关键修正）：原叶簇半径 = `leaf_radius × 冠高`，实测杨 1.19 m、行道树 1.15 m、宽叶 1.58 m——一个直径 3 米的球，20 米外就是实心圆。加细分只是把同一团体积画得更圆。

**上一轮一处判断要更正**：曾说「alpha 那条路断了」。那只在**顶点 alpha** 范围内成立。回源码确认：预通道 `prepass_sample_color_and_alpha_discard` 里 `output_color` = 材质基色 **× base_color_texture 采样**，再跑裁剪——**贴图 alpha 是被尊重的**。由此：

- `AlphaMode::Mask` 天然写对深度，雾与体积云不会在叶片周围被切出空洞（上一轮的坑自动消失）
- 季节密度可直接用 `material.base_color.a = leaf_density`，预通道零改动
- 季节系统因此退回 ADR-0004 的原始设计，上一轮那套**几何塌缩成为不必要的绕路**（连同 `UV_1` 打包一并删除，`UV_0` 腾给贴图）

**实现**：

- `world/leaf_shape.rs`：物种表驱动（`LeafProfile` + `LeafShape::{Pinnate, Simple}`）。**一张蒙版 = 一个植物学单元**：槐是整片奇数羽状复叶（叶轴 + 8 对小叶 + 顶生小叶），杨/柳是单叶。互生/对生属于**排布**，归骨架管，烘进贴图会和各枝朝向打架
- 三物种：槐（羽状、小叶圆钝）、杨（三角状、26 齿/边）、柳（披针形、长宽比 1:5）。加第四种（棕榈等）只需加一行表项
- `tree_mesh.rs`：树拆成**两个网格**（`TreeParts { wood, leaves }`）。卡片要 `Mask`，合并会把 alpha test 强加给无贴图的枝干、把树干整根裁掉
- 每簇 **22 张 2 面小卡**（0.26 × 叶簇半径），初始朝向按 45° 交叉 + 每卡随机偏移与相位
- **卡片布在壳层而非体积内**：体积散布时中间那些被前面的挡住，只花面数不产生画面；真实树叶也长在受光外层。壳层同时更省且更真实——中间留空，视线穿过树冠是「叶—空—叶」
- 每棵树的三角面：**2069–4326 → 约 1000–2000**（卡片 2 面 vs 球 20 面），轮廓反而变对

**mip 链是逐级膨胀的，这一条是硬需求**：箱式滤波保持均值不变，所以细长叶在低分级里**均值不动、但每个纹素都掉到 cutoff 以下**，远处皇冠停止剪碎天空——正是卡片要防的症状。level 0 只膨胀一次是初版做法，实测第 6 级失效；现改为半径随级别增长，且 8 px 以下停止膨胀（此时已无形状可言，是否还画由 LOD 决定）。

**测试（新增 13 个，总数 154）**：各物种蒙版非空且不糊满整盒；烘焙确定性；柳是细长条、杨是宽三角（比**叶片自身宽长比**，不是占盒比例——初版断言占盒比例，对每个物种都成立，等于没测）；杨最宽处在基部（按叶基在图像**下方**换算 t，初版直接除行号，方向搞反）；锯齿确实咬边；槐有多对小叶；`the_crown_lets_sky` 量化开口率（俯视 45–62%、侧视 34–46%，球为 0）；mip 链各物种到第 3 级仍可辨；膨胀不把叶子撑成矩形；加物种只需一行；两个网格都有几何；LOD 逐级更省；UV 覆盖整个贴图；枝干不被标成叶片；卡片朝向覆盖 ≥4/8 个方位。

**修掉的真问题（全部只在运行时暴露）**：
1. `Image::new` 断言数据长度必须等于**单个** mip 层，手工 mip 链直接 panic（编译期完全看不出来）→ 改 `new_uninit` 后挂数据
2. 卡片比叶簇还大（4 张 = 2.4 m）
3. 卡片堆在球心 → 体积散布仍不够 → 壳层
4. 预览图画的是**卡片边框**，造成「矩形直边」假象，也掩盖了「卡片尺寸过大」这个真问题 → 改为采样 alpha 贴图光栅化

**验证缺口（重要）**：
- `cargo run` 实测零 shader / 零管线错误、零 panic
- **但本轮的对照图我没能看到像素**：每次 `inspect_images` 均返回 `persistence_failed`，形态判断全部来自**视觉代理的文字描述**与上述量化数字，不是我自己看到的
- 代理判断：轮廓明显改善（不再是光滑球、无矩形硬边），但**冠内仍偏实**，未达单叶级细碎。数字与之吻合（侧视 34% 意味着仍有约 2/3 被挡）
- 再加密会逼近上限：单叶小于数像素就糊成噪点。**最终观感待 Feihei 实机确认**

**2026-10-30 实机反馈与修复（Feihei）**：
- **叶片朝向是错的**：卡片原先自由旋转，每片叶朝随机方向 → 观感是「贴图转向不对」。根因在数据结构：`Foliage` 只存了簇心和半径，**没有记录叶子该往哪长**。现补 `outward`（从枝条指向叶尖的单位向量，取自叶簇附着处的枝向；挂在枝侧的 run 簇用侧向量）。卡片局部 +y 跟随它，贴图叶尖在上缘，于是叶柄一律朝枝、叶尖朝外放射；卡片仍绕该轴均匀扭转以保留体积感，但扭转**不会再翻转叶片**
- 测试抓到两个截图看不出的问题：① `from_axis_angle(up, twist)` 与绕垂直轴的 lean 复合时会**重新正交化 `up`**，倾斜渗进生长方向，少数卡片尖端反而朝后；改为旋转两个垂直向量再反推生长向量 ② 测试最初拿叶尖边的**一个角点**比叶柄边的**一个角点**，量的是对角线而非生长轴；改为比**边中点**
- **HUD 压到左上角**：原来复用全屏居中面板，那行探索进度正好横在画面中央、挡住天空。游戏中改为左上角 14 px，菜单保持居中

**2026-10-30 R3.10 收官：三物种对齐（方案：共享图集 + 逐物种 UV cell）**：

**根因（读代码确认）**：`chunks.rs` 的 `build_placement_assets` 给全世界树共用**一张**叶片贴图，且写死 poplar 蒙版——这就是"只有宽叶一个树种"的原因。生长参数与蒙版表是两张独立的表，从没有对齐过。

**实现**：
- `leaf_shape.rs`：`bake_species_atlas`（三物种蒙版并排成**一张**图集）+ `species_cell`（各物种的 (u0,u1) 子矩形，半纹素内缩防 cell 间 bleed）+ `ATLAS_HEIGHT` 共享常量。**一张贴图 + 一个共享材质不变**（R3.5 的季节切换仍是一次 32 B uniform 写），物种区分完全走 UV 空间
- `tree_mesh.rs`：`SPECIES_LEAF` 映射表（0 aspen→poplar 杨 / 1 street→sophora 槐 / 2 broadleaf→willow 柳）+ `FoliageBake`（逐物种卡片参数）+ 卡片 UV 映射进本物种 cell
- **槐的细碎剪影（Feihei 方向）**：`species_foliage` 给槐 2.6× 卡片数 + 0.62× 卡片尺寸——行道树 profile 的簇数比杨少（~78 vs ~183），卡片数翻倍以上才让槐的剪影片数超过杨（实测 ~4400 vs ~4030 卡）
- `examples/tree_gallery.rs`：改用共享图集采样，标题标注三物种（杨/槐/柳）

**测试（新增 5 个，总数 160）**：图集 cell 不重叠且覆盖全物种；图集与单独烘焙逐字节一致；cell 有半纹素内缩；全物种 UV 都落在自己 cell 内；槐比杨卡片多且总三角面在预算内。

**对照图评估（proxy，受 persistence_failed 限制）**：三物种区分度中等偏弱——柳行明显，杨/槐靠尺寸区分；槐冠内透空仍偏少、整体偏"纸片感"。**下一步候选**：① gallery 加"单簇特写"行（真实比例，看卡片颗粒感）② 量化冠内开口率进 tree_stats 按数字调参 ③ 减小卡片覆盖率/加大间隙让天空更碎。**游戏内观感待 Feihei 实机确认**（近距视角下卡片颗粒比 gallery 缩放后明显）。

### R5.10 量化数据 + 特写行（2026-10-30，Feihei 选 B+A）

- [x] **tree_stats 加烘焙统计**：逐物种输出卡片数、平均卡片尺寸（m）、**冠内开口率**（侧视 alpha 光栅化：冠投影 bbox 内无叶覆盖占比）、烘焙三角面。实测（variant 0, Near）：

  | species | cards | card w×h (m) | opening | tris |
  |---|---|---|---|---|
  | aspen 杨 | 4026 | 0.49×0.57 | 0.415 | 5595 |
  | street 槐 | 4446 | **0.31×0.36** | **0.450** | 6155 |
  | broadleaf 柳 | 1980 | 0.80×0.92 | 0.312 | 2794 |

  **槐已是三物种中最细碎**（最高开口率 + 最小卡片），总额 14.5k 三角 < 20k 预算
- [x] **tree_gallery 加第四行 CLOSE-UP**：每物种一个叶簇的真实比例特写（span 2.6×簇半径、上限 260 px/m），单叶单元与天空缝隙肉眼可判
- **特写评估（proxy）**：杨/柳颗粒清晰可读；**槐的细碎对比成立但已接近该分辨率可辨极限**（单元偏小、近噪点；再密或再小就糊）；"环形中空"是壳层布卡的固有结构（游戏内多簇重叠时不可见）
- **待 Feihei 实机确认**：游戏内近距视角看槐的剪影；若仍嫌单元糊，候选杠杆是抬高 `ATLAS_HEIGHT`（256→512，叶片纹素密度翻倍，图集仍小）

**未落地**：柳的垂枝习性（生长模型改动，叶片本身已可区分）。

**2026-10-30 贴图素材试验结论（Feihei：程序化蒙版继续用）**：两条照片级叶片贴图路线都试过并放弃——
1. **bevy_flora 捆的 CC0 叶子扫描**（ambientCG LeafSet，512²/4 叶 JPEG）：无 alpha（需抠图）、边缘糊 + 压缩块状伪影、单片仅 ~230px，第一人称近距离发软；物种对口（poplar=杨、willow=柳）但质量不达标。若重启该方向，直接从 ambientCG 取带 alpha 的 PNG 原图，不从低配转码版抢救。
2. **ComfyUI 生成的槐叶**（品红底）：色度键控 + 分水岭拆分 + 去溢色全链路可行（`.tmp/leaf_comfyUI/processed/` 有产物），但源素材把多支复叶画成密集缠结，2D 分割对真重叠无解，处理后仍偏糊。若重启，提示词要求"单支复叶、各叶分离摆放、纯色平涂背景、无描边无阴影"。
**结论**：自研程序化蒙版（`leaf_shape.rs` 表驱动）继续作为叶片贴图来源；加工流水线的踩坑（色相键控可行、fill_holes 会把叶间缝隙填成不透明、tile 需按自身区域隔离 alpha）记录在此备查。

**未落地**：树种目前按位置映射到三个贴图，`TREE_PROFILES` 仍是原来的杨/行道树/宽叶三种生长参数，与槐/杨/柳的贴图并非一一对应。贴图形态定稿后再统一物种表（含垂枝的柳）。

### R3.11 建筑立面（2026-10-31，步骤 11.1–11.2，方案 A）

**范围**：只做「墙面 + 窗户 + 玻璃反射 + 室内灯光」这条观感主线。屋顶轮廓、零件、街道家具是 11.3–11.5，尚未开始。

**结构**：`world/facade.rs` 是纯数据 + CPU 烘焙；`plugins/facade.rs` 是材质与 GPU 侧；`assets/shaders/facade_uv.wesl` 只替换顶点阶段。

- [x] **11.1 立面烘焙**：`sample(style, variant, u, v) -> Surface{albedo, roughness, metallic, emissive}` 是唯一真源，返回三张图（albedo 1024²、metallic_roughness 512²、emissive 512²）。三风格 × 2 变体。
  - **可平铺是结构性的**：窗格索引取模、砖行数由瓦片尺寸推导、噪声用**环绕格点**值噪声（`pnoise2` 对索引取模，而不是对坐标乘系数——后者会破坏周期性并让每面墙出现接缝）
  - **三风格区分**：老城 2.0 m 开间/3 m 层高（抹灰/青砖 + 雨痕）、公寓 3.3 m（预制板缝 + 锈迹渗流）、厂房 6.0 m（波纹钢 + 砖填充 + 锈斑）
  - 玻璃 = albedo 近黑 + roughness 0.045–0.16；墙面 roughness 0.82–0.90。**反射来自掠射角的天空 IBL，不需要 SSR**
  - 亮窗按窗格哈希点亮，暖白（住宅）/冷白（厂房），emissive 乘子 `WINDOW_EMISSIVE = 3.0`
- [x] **11.2 世界→米制 UV（改为实例尺寸反推）**：`scale = length(world_from_local[i].xyz)`，再 `bays = round(scale / bay)`、`floors = round(scale / floor)`
  - **没有用世界坐标 UV**：`fract(world.xz / tile)` 会把图案锚在世界而非建筑上，每一面墙都在转角切断窗户（2 m 开间 + 4.6 m 面宽 = 两头各半扇窗）
  - 屋顶面（|normal.y|>0.7）停在墙面色带内，避免「盒子上贴窗户」；真屋顶属 11.3
  - 片元阶段保持 Bevy 内置 `pbr.wesl`（同 `tree_sway` 取舍）：光照、环境贴图、雾、色调映射全部沿用
- [x] **放置侧配套**：面宽与层高吸附到开间/层高的整数倍（`snap_to_bay`）。**吸附的是实际朝向街道的轴 `size.z`**（`yaw = atan2(dir.x, dir.z)` 把局部 +Z 转到街道方向），不是 `size.x`
- [x] **材质数量**：3 风格 × 2 变体 × 4 磨损档 = **24 个材质、18 张贴图**（磨损档共享贴图，只换 tint）
- [x] **测试（新增 22 个，总数 183）**：烘焙确定性；变体确实不同；**接缝台阶不超过普通相邻纹素**（`the_tile_seam_is_no_harder_a_step_than_a_normal_texel`，量化且与“看起来对”无关）；噪声格点环绕；玻璃占比 3–45% 且墙面 >55% 粗糙；亮窗 0.4–20%；albedo 均值落在剪影预算内且无近白；冷暖光按建筑类型；四开间不全相同；`snap_to_bay` 取整且不为零；mip 链到 4 px 且亮度守恒；三张图玻璃位置对齐；uniform 16 B 与着色器 binding 100 一致；**顶点阶段不得出现 `@fragment`**（防止有人给立面写片元着色器从而失去天空反射）；玻璃必须有镜面响应（`reflectance > 0`，照抄树的 0.0 会静默删掉反射）；面宽是开间整数倍；层高是层高整数倍**且老城至少两档高度**；变体稳定且分布 25–75%；立面材质恰好 24 个且不随帧增长
- [x] **实测**：`cargo run` 70 s 零 shader 错误、零管线错误、零 B0004，日志 `24 facade materials (3 styles x 2 variants x 4 wear buckets)`
- **对照图**：`cargo run --example facade_gallery` → `.tmp/facade_gallery/`（6 张 albedo、亮窗合成图、缩略图）。像素统计：老城均值 0.541 / 公寓 0.472 / 厂房 0.270（sRGB），无 >0.75 的过亮像素

**验证缺口（重要）**：`inspect_images` 在本会话持续返回 `persistence_failed`（与 R3.10 相同），**我没有看到任何一张图**。上述观感判断全部来自像素统计与字符画缩略图，不是目视。**玻璃反射强度、亮窗是否过亮、开间节奏是否自然，必须由 Feihei 实机 `cargo run` 确认。**

### R3.11b 建筑占位几何修正（2026-10-31，Feihei 选定优先于 11.3）

R3.11 落地时暴露的三个耦合缺陷，作为一个任务整体修复。修复前实测：公寓面宽恒为 3.3 m、进深 6 m，18 m 高——参数表里“沿 edge 长度 25–40 m”从未生效。

- [x] **yaw 指向修正**：`angle = (-dir.z).atan2(dir.x)`。`rotated_corners`/`Quat::from_rotation_y` 把局部 +X 映到 `(cos y, -sin y)`，要让长度沿街就必须 `cos y = dir.x, -sin y = dir.z`。旧公式把局部 +Z 转到街道方向，于是 `size.x`（长度）指进了对面街区，`rect_clear_of_roads` 拒掉所有表尺寸 footprint。**验证**：探针实测局部 +X 与街道方向点积 1.000（旧 0.000）。
- [x] **行放在街区实际覆盖的边内**：新增 `usable_frontage`（把 block 多边形投影到边上取 [u_lo, u_hi]）。街道边横跨整个格边，而街区多边形从中心线内缩并切角，所以边两端各约半街宽根本没有街区——这就是 `block_depth_along_pub` 在端点返回 0、进而 `budget 0.0` 拒绝整行的根源（旧的按 3 分段其实是在绕开这个坑）。现在深度探针只测行真正占据的列。
- [x] **整行判定改为逐列收缩**：扰动后的街区是斜四边形，沿边各列的弦长差好几米；旧的 `row_fits` 一列浅就否掉整行（实测一次跑出 1710 次拒绝）。现在深度取全行最浅值并整体收缩，实例级朝向判定逐个执行，落不下的列留缝——真实街道本来就有缺口。
- [x] **footprint 判定与绘制同角**：新增 `geometry::oriented_rect_inside_polygon`（四角由 `rotated_corners` 同源公式生成），`row_fits` 的逐列检查、实例级 block/hull 检查、以及 `buildings_stay_out_of_streets_and_inside_their_block` 全部改用它。轴对齐旧判定只在旧 yaw 下与绘制巧合一致。
- [x] **放置结果（3 seed）**：公寓 53–80 栋、面宽 26.4–39.6 m（参数表尺寸首次生效）；厂房 11–19 栋、42–48 × 50 m；老城 2019–3062 栋、4–6 m 模块、高 3/6 m 两档。`walking_every_street_reaches_the_far_end` 仍全绿（不卡死、不穿块、不出世界）。
- [x] **测试（新增 1 个，总数 183）**：`frontages_are_whole_bays` 改断言 `size.x`（旧断言 `size.z` 是在验证进深——恰是当初轴对应弄反时的写法）；`buildings_stay_out_of_streets_and_inside_their_block` 同步改用朝向判定。
- [x] **实测**：`cargo run` 70 s 零 shader 错误、零管线错误、零 B0004。
- **遗留**：老城数量 827→2730（密度上升，观感待验收）；`world_map` 顶视图经视觉代理确认合并大块为公寓/厂房的有意合并、外圈暗缝为景物带，未见非预期空洞。

### 可验证

### 可验证

- 街道行走看到建筑与树沿路排列，风格比例约 50/30/20
- 被负形街道碰撞实体限制，不能进入建筑区；可进入 PLAZA
- **环绕地图一周不被卡住、也不漏进边缘**（验证 2.11 的外侧围墙零缝）
- 切换季节树叶密度/颜色/摆动变化
- 同一 seed 每次生成完全一致；碰撞实体不渲染（无 Draw Call）

---

## 阶段 R4: 存档与菜单（预计 2-3 天）

**目标**：复现原 M4 存档/菜单系统。

- [x] 4.1 探索位图：`ExploredBitmap`（chunk 键 HashSet + 进度/完成判定 + 排序键序列化接口）+ `PlayerChunkChanged` 消息，`track_player_chunk` 挂在流式系统之后（同为 Update）
- [x] 4.2 存档序列化：`SaveData`（version/seed/season/explored 排序键/pos/yaw，serde JSON）+ `saves/slot_N.json` 落盘；season 为占位枚举（字段先占格式，R3.5 落地无需迁移）；round-trip 位精确 + 版本字段可检测试
- [x] 4.3 存档恢复：`LoadRequest` 消息 → `handle_load_request`（Update，先于 Streaming）：版本检查拒绝未来格式、seed 重建布局图（实测 <1s）、清空全部 chunk 实体/流式队列/碰撞索引、恢复位图与玩家位姿（yaw + 默认 pitch）
- [x] 4.4 多存档：`SaveGameRequest` 手动写入 + 自动存档（chunk 变化后节流 30s，停留不写盘）；`list_slots()` 元数据列表（seed/season/探索数/mtime，损坏文件跳过）；槽 0 自动 + 槽 1-3 用户
- [x] 4.5 菜单 UI（bevy_ui，键盘驱动极简风）：主菜单（seed 键入+Enter 开始/L 读档/Esc 退出）、Esc 暂停菜单（继续/S 存槽 1/M 回菜单）、HUD（探索 N/1600 %）；`MenuUiRoot` 按阶段重建面板；光标按阶段持有（Playing=Locked，菜单=None）；可点击按钮留 R5.6 润色
- [x] 4.6 菜单驱动开局 + `is_game_active` 门控 + 全探索完成消息：`GamePhase`（MainMenu/Playing/Paused）为唯一事实来源；开局复用 R4.3 恢复流（Start = 带 seed 的新鲜 LoadRequest）；玩家移动/注视/调速、流式、探索全部按 phase 门控；玩家侧 Esc 光标切换移除（菜单接管光标）；`WorldExplorationCompleted` 消息全探索时触发一次（待完成弹窗接线 R5.6）

### 可验证

- 菜单 → 散步 → Esc 保存 → 读档回到同一位置/季节/进度
- 同一 seed 地图结构完全一致；多存档互不干扰
- 全探索触发完成提示

---

## 阶段 R5.1: 天空实时调参面板（2026-10-27）

**目标**：云端与黄昏调参是迭代过程，参数互相影响（cover 改变云类型、湿度再改一次、曝光决定你能看到多少），改一次编译一次太慢。

- [x] `plugins/sky_panel.rs`：F5 开关，Up/Down 选行，Left/Right 调值（长按连续）
- [x] `DuskSky` 扩展可调参数：`relative_humidity` / `storminess` / `wind_speed_ms` / `bloom_intensity`，与已有 `cloud_cover` / `cloud_layer` / `exposure_ev100` / `exposure_noise_amplitude` 凑成 9 行
- [x] `apply_weather_overrides` 取代原 `apply_cloud_cover_override`：湿度/风暴/风速**始终生效**（只影响云的类型），cover 仍可保持 preset
- [x] `apply_bloom_intensity`：面板改 bloom 下一帧生效
- [x] 参数表用 `get`/`set` 闭包描述，新增参数只加一行；范围与步进各自声明，统一钳制
- **测试**：面板关闭时不吞方向键；F5 切换 + 方向键改选中行；**每行按到两端都不越界、不出 NaN**（NaN 曝光会整帧变黑）；云层 base 永远小于 top
- **实测**：游戏启动零告警零错误

**面板刻意不包含的参数**：云密度、噪声缩放、march 步数——它们编译在 bevy_elements 的 shader 里，改它们需要 patch 依赖，属于另一个决策。

### R5.2 云种模型（2026-10-27，Feihei 反馈：云是柱体+锥体、形状不随机）

**根因（读源码确认，非猜测）**：三层粒度限制叠加，云的轮廓被量化在 32–64 m：

| 层 | 分辨率 | 每 texel 覆盖 |
|---|---|---|
| 覆盖场（平面轮廓，`weather.wesl`） | 512² / 32 km | **62.5 m**（噪声波长仅 8/4/2 km） |
| 基础噪声（云体，`mod.rs:218`） | 128³ / 8192 m | **64 m** |
| 细节噪声（边缘侵蚀） | 32³ / 1024 m | **32 m** |

且**噪声分辨率与 `ElementsQuality` 无关**（quality 只改 march 步数和 block），所以抬 quality 无效。「柱体+锥体」正是这个粒度下噪声分布的形状。

- [x] **面板改为云种模式 + 量**：`CloudKind::{ScatteredCumulus, BrokenCumulus, ToweringCumulus, Stratocumulus, Overcast}` + `cloud_amount`。每种是一个调好的气象点（cover/humidity/storminess/wind/layer），左键循环切换，**循环末尾是「无云」**（`None`）
- [x] `resolve_clouds()` 纯函数把「云种+量」解析成 `Weather`/`CloudLayer`，便于无 GPU 断言。无云时 cover=0、humidity=0.35（防止 crate 的 `kind_from_weather` 从湿度里凭空造出层云）
- [x] 选中云种时若 amount=0 自动给 0.35（否则选了看不到）
- [x] 保留 `humidity_bias` / `storminess_bias` 做同种内的微调
- **测试 8 个**：面板关闭不吞键、F5 切换、云种循环覆盖全部并最终到无云、选云种给可见量、无云解析为零覆盖、每种云解析出的气象值都在 crate 可用范围、云层 base<top、浮点行不越界不出 NaN
- **实测**：98 测试通过、clippy 零告警、游戏启动零告警

### R5.3 云底全黑 + 形状过规则的诊断与修复（2026-10-27）

**Feihei 反馈**：stratocumulus / overcast 形状自然但**全黑**；其余云种是「底下一个圆饼上面顶着半个鸡蛋」。

- **全黑根因（实测）**：加日志抓 GPU 回读，`CloudShadows.camera_transmittance` 在 10 秒内从 0.098 衰减到 **1.5e-5**。黄昏时太阳高度仅 **2.4°**，光线要以掠射角穿过整层云才能照到云底，600 m 厚的连续云板物理上到不了底
- **修复**：`Overcast` 700→1000 m（300 m 薄板）、`Stratocumulus` 900→1500 m；`Overcast` cover 0.97→**0.90**（shader 的 deck 模式在 ~0.95 以上饱和，饱和后云层不再有结构）
- **形状根因（读 shader 确认）**：`common.wesl:89 cloud_gradient()` 有个 **dome 因子**——云的高度按「到云块边缘的距离」缩短（`DOME_WIDTH = 0.37`），核心满高、边缘变矮。这正是「圆饼 + 半个鸡蛋」。`dome` 仅在 `kind > 0` 生效，`kind = 0.5`（标准积云）时穹顶最强
- **方案 A 内的缓解**：stratus 方向（低 kind）穹顶弱，所以靠 humidity 把 kind 推离 0.5 能改善——这也解释了为什么 stratocumulus / overcast 形状更自然
- **测试**：`no_broken_or_overcast_layer_is_thick_enough_to_go_black`（层状云厚度上限；**积云豁免**——离散蓬松云块之间可透光，与连续云板物理不同，第一版测试一刀切把积云也判红，是错的）、`stormy_kinds_are_the_tall_ones`
- **实测**：100 测试通过、clippy 零告警、游戏启动零告警

### R5.4 dome 饱和带 + sky_gain 亮度反转（2026-10-27）

**Feihei 反馈**：仍偏黑；只有 overcast 像自然云，stratocumulus 仍是圆饼+鸡蛋；**天空亮度随云量反向变化**（调高变亮、调低变暗）。

- **stratocumulus 圆饼的真实原因（数学定位）**：shader 的 dome 强度是 `clamp(kind * 4, 0, 1)`，**在 `kind >= 0.25` 时就完全饱和**。原参数 cover 0.65 / rh 0.86 → `kind = 0.342` → dome 仍是满值 1.00，和普通积云一样
  - 而 `kind_from_weather` 里 `stratus = smoothstep(0.5, 0.9, cover)`，cover 0.65 只贡献 0.28。**真正能压低 kind 的是 cover，不是 humidity**（rh 被 `smoothstep(0.9, 0.98, rh)` 卡住，0.86 贡献为 0）
  - 修正：`Stratocumulus` cover 0.65 → **0.78**、rh 0.86 → 0.93、厚度 900–1500 → **900–1300**，`kind` 降到 0.108，**dome 0.43**（原 1.00）
  - 这也解释了为什么 overcast 自然：`kind = 0.000`，dome 完全消失
- **天空亮度反转的根因**：`composite.wesl` 有 `cloud.a *= mix(1.0, sky_gain, ...)`，而 `sky_gain = total / beam`。cover 越高 beam 越低、gain 越大：cover 0.90 时 gain **1.58**（天空放亮 58%）。crate 的假设是「天空在云层之上不该被下方云的遮挡影响」，但**黄昏低云时该假设不成立**
  - 这是依赖库的设计取舍，**没有公开 API 可调**（`sky_gain` 是内部函数）。「调低云量变暗」实为天空从被放亮的 1.58 掉回正常，落差明显
  - **未做补偿**：用曝光去抵消会引入「云量变化时曝光漂移」的新问题，且属观感权衡，留待 Feihei 决定
- **测试**：`sheet_kinds_do_not_sit_in_the_dome_saturation_band`（层状云的 `kind` 必须 < 0.25，否则圆饼回归）、`cumulus_kinds_keep_the_dome`（积云必须保持 dome，否则蓬松感消失）。已验证**把 stratocumulus cover 改回 0.65 测试即失败**
- **实测**：102 测试通过、clippy 零告警、游戏启动零告警

### R5.5 方案 B（部分）：vendored bevy_elements，修 dome 与 sky_gain（2026-10-27）

方案 A 的两处残留都要改 shader，而 shader 是 `load_embedded_asset!` 编译期嵌入的，**没有运行时覆盖手段**。按 Feihei 选择「只改 dome 和 sky_gain、不碰噪声」，把 crate vendored 进项目（`vendor/bevy_elements`，仅 2.5 MB 源码）通过 `[patch.crates-io]` 接入。

- [x] **dome 饱和带**：`clamp(kind * 4, 0, 1)` 在 `kind >= 0.25` 全饱和，而 `kind_from_weather` 把多数积云放在 0.34–0.50 → 散积云/碎积云/层积云拿到**同一个** dome。改为 `smoothstep(0.05, 0.55, kind)`
- [x] **dome 剖面是半椭圆**：`sqrt(dome)` 上升过陡 → 改为 `pow(dome, 0.62)`，并用基础噪声绿通道做**逐云块抖动** `[0.65, 1.35]`，相邻云不再完全一样
- [x] `DOME_WIDTH` 0.37 → **0.30**（满高度只保留在云块核心）
- [x] **sky_gain 亮度反转**：`total / beam` 在 cover 0.90 时达 **1.58**，且作用在天空像素上。crate 假设「云层之上的天空不被云层遮挡」，高日角成立、**黄昏 2.4° 不成立**。改为按太阳高度 2°–25° 用 smoothstep 淡出，25° 以上行为不变（上游自带测试仍通过）
- [x] **`Cargo.toml` 排除 workspace**：vendored crate 有自己的 lockfile 与测试，不该作为 member
- [x] **`.gitignore`**：`/vendor/*/target`（跑一次测试就 2.1 GB）与 `Cargo.lock`
- [x] **`vendor/bevy_elements/PATCHES.md`**：逐条记录改动、原因、代码位置，并注明**升级时必须重新合并**
- **测试**：`the_vendored_cloud_shader_patches_are_present` —— 同时检查 vendored 源码里三处表达式**和** root manifest 的 patch 行是否**真的启用**（不是仅作为注释存在）。已验证：注释掉该行测试即失败
- **实测**：103 测试通过、clippy 零告警、游戏运行 20 s 零告警（**同时证明两个 WGSL 文件编译通过**——shader 错误会在 pipeline 创建时暴露）

**刻意没做**：
- **噪声分辨率**（128³/32³）未动。形状与边缘是两个问题，同时改会让结果难以归因；抬高到 256³ 约 +128 MB 显存，等形状定稿后再单独评估
- **连续云板黄昏变黑**未用 shader 硬补环境光，而是靠游戏侧把层状云做薄（见 `CloudKind::layer`）

**结论与边界**：方案 A 改的是「云的种类、比例、层次」，**改不了边缘细节**——要做碎边和不规则轮廓必须改噪声分辨率。Feihei 已知悉并选择先做 A，dome/sky_gain 已通过 vendor 修复。

### R5.6 高空卷云（噪波穹顶，2026-10-27）

按 Feihei 选择「穹顶 mesh + 跟随黄昏天色」实现。**默认关闭**（`amount = 0`），体积云仍是主云，卷云是按需的廉价补充。

- `plugins/cirrus.rs` + `assets/shaders/cirrus.wesl`
- **为什么不用体积**：卷云在 6–12 km，视差几乎为零；其特征是**拉伸纤维**，正是 2D 噪波拉伸擅长、体积噪声最不擅长的
- **为什么走 mesh 而非全屏后期**：`FullscreenMaterial` 的 render pass **没有深度附件**，卷云会盖在街道和建筑上；走标准 mesh 管线则深度遮挡免费生效
- **形状**：两个不同频率/漂移的 fbm 叠加给出长短不一的丝缕，再用低频场做 gate 产生缺口和钩状边缘；沿一个方向压缩噪声域并加正弦剪切，得到弯曲而非直尺状的条纹
- **颜色**：取相机的 `AmbientLight`（bevy_elements 已经由天空的黄昏渐变推导出来），因此卷云不会和天空脱节
- **面板**：F5 下三行 —— `cirrus amount` / `cirrus stretch` / `cirrus scale`

**首版三个错误（均已修复并由测试锁住）**：

| 错误 | 后果 | 原因 |
|---|---|---|
| 穹顶半径 4 km | **完全不可见** | `PerspectiveProjection::default().far = 1000 m`，本项目未覆盖，整个球被裁掉 |
| shader 用 `.wgsl` 扩展名 | 编译失败 | `#import` 是 **WESL** 语法，`.wgsl` 按纯 WGSL 解析直接报错。必须 `.wesl` |
| 穹顶直接放在球内 | **完全不可见** | 相机在球心，所有三角形都是背面，Bevy 对所有 alpha mode 都剔除背面。**负缩放并不能反转绕序**（已数值验证），且 `Material` trait 没有 `cull_mode` 钩子 → 改为生成 mesh 时交换每个三角形后两个索引 |

- **`.gitignore` 变更**：`assets/` 整体被忽略，但 **shader 是源码不是资源**，缺了游戏会静默失去效果。改为 `/assets/*` + `!/assets/shaders/`，并额外忽略 `/assets/shaders/*.wgsl`（防止错误扩展名的副本被误提交）
- **测试 5 个**：默认必须关闭、参数范围钳制、穹顶在远平面内且在世界几何之外、绕序朝内（否则整个天空静默无云）、stretch 不会产生 NaN
- **实测**：108 测试通过、clippy 零告警、游戏开卷云运行零告警（证明 shader 编译通过）

### R5.7 卷云接缝修复（2026-10-27，Feihei 反馈：能看到贴图接缝）

接缝来自**两个独立原因**，都先量化再修：

1. **穹顶原本是 UV 球**。UV 球极点会收成细针：Bevy 默认 32×64 球的第一圈极区三角形**长宽比只有 0.098**。光栅化这些碎片会产生插值失真，视觉上就是向极点收敛的放射条纹
   - 改为**二十面体球**（三角形近似等边，无极点）
   - **顺带踩到一个坑**：此版本 Bevy 的 `SphereMeshBuilder::ico(n)` 生成的是 **`20(n+1)²`** 个三角形，**不是**经典细分的 `20·4^n`。`ico(3)` 只有 320 个（实测），不是 1280。现取 `ico(7)` = 1280
2. **视线方向是在穹顶顶点上插值出来的**。900 m 半径、135 m 边长下，插值 `world_position` 再减去相机位置，方向误差达 **2.8×10⁻³ rad**，且误差在每条三角形边界处突变——**这个突变就是接缝**
   - 改为 fragment 里用 `world_from_clip` **反投影自己的像素**，取近/远平面两点之差作为方向（用差值可避开投影矩阵在单点深度上的精度损失）。逐片元精确，**与三角形数量无关**

**已排除**：值噪声哈希在格点边界**连续**（ε=1e-3 时跳变 1.1e-6，ε=1e-9 时为 0），噪声本身不产生折痕。

- **测试新增**：`the_dome_has_no_polar_slivers`（遍历全部三角形限制最差长宽比 > 0.8，UV 球会以 ~0.1 失败）、`the_shader_rebuilds_the_direction_from_the_pixel`（防止退回插值写法）
- **实测**：110 测试通过、clippy 零告警、游戏开卷云运行零 shader 错误

### R5.8 卷云不可见修复 + 体积云默认关闭（2026-10-27）

**Feihei 反馈**：`cirrus amount` 调到 1 也看不到云。

- **根因**：上一轮为消接缝改为「按像素反投影方向」，但反投影的是 **深度 0.0**。Bevy 是**反向 Z + 无限远平面**：深度 1.0 是近平面，**0.0 是无穷远点**。`world_from_clip * (ndc, 0.0, 1.0)` 会除以 `w = 0`，方向变成 NaN → `dir.y < 0.02` 判定失败 → **每个片元都被 discard**，而没有任何报错
- **数值验证**（镜像 Bevy 投影矩阵）：深度 0.0 得到 `(nan, nan, -inf)`；深度 0.95 / 0.55 得到有限的朝前射线
- **修复**：取**两个有限深度**（0.95 与 0.55）做差分。方向精确、不受退化远平面影响、也不依赖近平面位置
- **测试**：`the_shader_does_not_unproject_the_degenerate_far_plane` —— 断言深度 0.0 的表达式不再出现，且两个有限深度存在。**失败模式是完全静默的**（无错误无警告，只是天空少一层），所以必须用 tripwire 而非运行期断言
- **体积云默认关闭**（Feihei 决定）：`cloud_kind: None`、`cloud_amount: 0`。理由是体积层在黄金时刻呈块状几何感且无参数可解（轮廓被固定噪声分辨率量化在 32–64 m），而晴朗黄昏也更贴近 ADR-0001。F5 仍可手动开启
- **测试**：`volume_clouds_ship_disabled` —— 锁定默认值，因为「合理默认」类重构会悄悄把它改回去
- **实测**：112 测试通过、clippy 零告警、游戏开卷云运行零 shader 错误

### R5.9 卷云随视角漂移修复（2026-10-27，Feihei 反馈：贴图随相机视角变化）

方向本身一直在世界空间算，错的是 **NDC 的 y 轴没有翻转**：

- `in.position.xy` 是**片元坐标**（原点左上、y 向下）
- NDC 是 `(-1,-1)` 左下、`(1,1)` 右上（**y 向上**）——`bevy_render/src/view.wesl` 两处都有明文
- 因此 `uv * 2 - 1` 得到的是**垂直镜像**的 NDC。镜像会让图案在俯仰时朝相反方向移动，观感就是「贴图跟着视角走」
- 修复：`ndc = vec2(uv.x * 2 - 1, 1 - uv.y * 2)`
- 同时补上**减去 viewport origin**。主相机 origin 是 (0,0) 所以实际没变化，但非零 origin 的 viewport 会让整块图案偏移，且偏移随视角变化

**排查过程记录**（避免以后重复走弯路）：
- 已验证**不是**噪声问题：值噪声哈希在格点边界连续（ε=1e-3 跳变 1.1e-6）
- 已验证**不是**穹顶跟随问题：日志确认 dome 与相机 delta = 0.000
- 已验证**不是**插值问题：`dir` 完全由像素反算，不受三角形密度影响（实测 1 个三角形边在屏幕上占 8.63°、约 368 px，但改细分不解决镜像）
- 已排除 TAA jitter（项目未启用）

- **测试**：`the_shader_flips_the_fragment_y_axis`（断言 `1.0 - uv.y * 2.0` 存在、未翻转表达式不存在）。已验证改回未翻转写法测试即失败
- **实测**：113 测试通过、clippy 零告警、游戏运行零 shader 错误

### 云的工作结论（2026-10-27 收尾）

Feihei 验收：**「好了一些，但还是不太自然。体积云就到此吧。」**

- **已解决**：天空亮度随云量反向变化（`sky_gain` 黄昏淡出）；云种之间的形状差异（dome 饱和带 + 逐块抖动）；层状云黄昏全黑（改用薄层）
- **仍未解决（已定位，未修）**：云边缘与轮廓的「几何化/块状」观感。根因是三层噪声粒度 **32–64 m**（覆盖场 512²/32 km、基础噪声 128³/8192 m、细节 32³/1024 m），与 `ElementsQuality` 无关。修它需要抬高噪声分辨率，约 **+128 MB 显存**，且会显著拉长首次编译
- **未来若要重启**：从 `vendor/bevy_elements/src/clouds/mod.rs:218-219` 的两个 `*_TEXELS` 常量入手（128→256、32→64），配合 `made` 里 `NOISE_SLAB` 分片逻辑一起看。这是唯一还能明显改善云的杠杆
- **当前状态可接受的理由**：ADR-0001 的观感重心是**抬头看黄昏天色**，云是背景元素而非主体；在金色时刻的长曝光下，低对比的云块不如白天明显

**下一步**：用面板实际试五种云，确认哪些可接受；若仍无法接受块状边缘，再走方案 B。

---

## 阶段 R5: 声音与润色（预计 2-3 天，原 M5 顺延）

**目标**：环境音、配乐与细节打磨（原 M5 任务原样继承）。

- [ ] 5.1 脚步声系统：按步行速度播放不同节奏
- [ ] 5.2 风声音效：与 sway_strength 联动
- [ ] 5.3 环境音：鸟叫、远处城市低语，随机触发
- [ ] 5.4 克制配乐：短时间片段，偶尔触发，不持续播放
- [ ] 5.5 性能优化：LOD（远处建筑低模）、Draw Call 控制，确保 2-4GB VRAM 流畅 30fps+
- [ ] 5.6 总体打磨：色调微调、速度手感、UI 美化、Bug 修复
- [ ] 5.7 跨平台验证：Windows + 至少一个其他后端（Vulkan/Metal）核验大气/云/TAA

### 可验证

- 完整体验：菜单 → 散步 → 存档 → 读档 → 再散步
- 声音自然不突兀；低端 GPU 稳定 30fps+

---

## 阶段 R6: 自研体积云（替换 bevy_elements CloudPlugin，2026-11-11 启动）

**背景**：Feihei 验收结论——crate 体积云噪声粒度 32–64 m 太粗、团块无翻卷细节，黄昏光照发灰无银边；其面向全天候动态天气的通用设计正是观感问题根源（本作锁定黄昏 17:30 + 晴朗低云量 + 仰视相机）。计划见 `.trae/documents/self-built-volumetric-clouds.md`，决策落地后登记为 adr-0007。已确认决策：游戏侧新模块 `src/plugins/clouds/`（vendor 不动，零升级合并风险）；半分辨率 march + 时域重投影（中低端独显 30fps+）；只做黄昏积云一种模型（5 种 CloudKind 全部映射进积云参数空间）。

- [x] 6.0（C0）接管与管线骨架：sky.rs disable CloudPlugin + 删 `apply_cloud_layer_override` + DepthPrepass 观察器；clouds 模块（mod/params/config/gpu.rs）注册进 GamePlugins；游戏侧 WESL import spike 通过（crate 的 `bevy_elements::noise` 可 import；游戏 crate 库名 `no_one_looks_up::plugins::clouds::*`）；自写 kernel 排队辅助（vendor 的 `pub(crate)` 不可用）；debug 均匀壳层全链（march→resolve→composite）GPU 冒烟通过；单测（params 镜像/拷贝数学 tripwire/重置触发）
- [ ] 6.1（C1）噪声与形状：generate 256³ 8-slab + detail 64³、weather 256²/16km、密度模型（高度梯度/dome/cell gaps/erosion/wind skew）、march 接真密度（coarse/fine 双模跳步）
- [ ] 6.2（C2）黄昏光照：transmittance LUT 太阳色 + Hillaire 三 octave 多重散射 + 双瓣 HG + Beer-powder、环境 cube 环境光（16 采样）、aerial perspective + far veil
- [ ] 6.3（C3）时域稳健性与遮挡：重投影 clamp 硬化（reset 全触发）、深度遮挡、曝光扰动下 history 不脏、步行+转头实机验收无拖影无闪烁
- [ ] 6.4（C4）性能与档位：`CloudsQuality::of(ElementsQuality)` 映射、RenderDiagnostics 五段 span、实机 1080p 30fps+、DX12/Vulkan 双后端核验
- [ ] 6.5（C5）验收与文档：五 kind 参数终调（F5 逐一切换人工验收）、`tests/clouds_screenshot.rs` 断言定稿、新增 `.agents/adr-0007-clouds-rework.md`、更新 adr-0003/AGENTS 状态表

### C0 技术纪要（2026-11-11）

- bevy 的 WESL import 路径 = 嵌入资产路径去掉 `src/` 前缀：`src/plugins/clouds/common.wesl` 注册后即 `no_one_looks_up::plugins::clouds::common`；crate 的噪声库是 `bevy_elements::noise`（hash31 入参为 `vec3<i32>`，u32 需显式转换）
- 历史纹理存**未曝光**辐射亮度，曝光在 composite 乘 `view.exposure`（ev100 13 → exposure ≈ 1e-4，天空辐射单位在千级——debug 壳层 luminance 6000 才是中灰，0.5 会整帧黑屏且 T=0 盖掉天空）
- resolve 无 Bayer 槽位：每 texel 每帧全量 march → 全像素 fresh 双线性上采样 + 3×3 fresh clamp + mix 0.5；壳入点世界空间重投影（`previous.clip_from_world`）
- GPU 冒烟 `tests/clouds_screenshot.rs`（#[ignore]）：断言全部 ratio 化（无 NaN / R>B 暖色 / p90-p10 抖动带宽上下限），相机用 `Tonemapping::None`（ACES 会把 1.67× 抖动压缩到 1.19×，统计须在 tonemap 前）

---

### R0.6 bevy_elements 0.6.0 迁移 + bevy_flora 试装（2026-10-30）

**背景**：Bevy 0.20 生态跟进（2026-10-30 检索）：`bevy_elements` 0.6.0（2026-10-26，Bevy 0.20，大扩张）与 `bevy_flora` 0.46（2026-10-14，Bevy 0.20，程序化树 + 季节 + LOD）均确认兼容。`bevy-volumetric-clouds`（0.2.0 仍只支持 0.18）与 `bevy_atmosphere`（停在 0.16）确认继续不可用。Feihei 选择：升 0.6.0 + bevy_flora 隔离试装对比（不动 R3.9/R3.10 自研树系统）。

- [x] **0.6.0 迁移**：vendored 源码整体替换为 0.6.0；3 个云补丁（dome ramp/剖面抖动、DOME_WIDTH、sky_gain 黄昏淡出）核对**上游 0.6.0 全部仍未修**，重移植到新 shader 位置（stratiform/cumuliform 双路径，见 `vendor/bevy_elements/PATCHES.md`）
- [x] **代码适配**：`ElementsCamera` 扩展为 6 flag（结构字面量补 `..ElementsCamera::default()`）；裁剪名单 +4（Trails/Paths/Waves/Rocks，0.6.0 新增）；`dome_strength` 镜像更新为补丁后的 smoothstep ramp
- [x] **测试修复（既有缺口，非本次引入）**：`sky_screenshot` 测试栈缺 `InputPlugin`（R4.5 菜单读 `ButtonInput<KeyCode>`）；autosave 测试受 `saves/slot_0.json` 残留文件污染（并发测试清理后已恢复）
- [x] **验证**：fmt/clippy/test 全绿（155 lib + 集成）；GPU 冒烟 `--ignored` 通过——证明 0.6.0 shader（含补丁）编译进管线、黄昏帧断言成立
- [x] **bevy_flora 试装**：`examples/flora_gallery.rs`（4 预设 × 4 确定性 seed，静止风，窗口运行）——仅形态对比参照，不接入 chunk 流式，可随时删除
- [x] **试装结论（2026-10-30，Feihei 实机验收）**：**自研树为准，bevy_flora 依赖已移除**。实机对比：bevy_flora 的树质量不优于自研；其唯一优势是"树种特征区分度"，而这是预设表驱动 + 每物种调好参数的做法，自研 `LeafProfile`/`LeafShape` 表已有同一机制。缺的环节是 R3.10 的未落地项——`TREE_PROFILES` 生长参数（杨/行道树/宽叶）与槐/杨/柳贴图物种一一对齐。`examples/flora_gallery.rs` 因会话引用保护无法删除，已改为占位说明，待 `git rm`

## 进度总览

```
R0: 骨架与依赖锁定   [██████████] 完成（2026-10-20）
R1: 天空系统迁移     [████████░░] 完成（2026-10-20，色调饱和度待人眼迭代）
R2: 布局图与Chunk    [██████████] 完成（2.1–2.15，2026-10-21）
R3: 放置碰撞与季节   [██████████] 完成（3.1-3.8 全部 done；3.5/3.7 季节系统 2026-10-29）
R4: 存档与菜单       [██████████] 完成（4.1-4.6，2026-10-24；可点击按钮/完成弹窗接线留 R5.6）
R5: 声音与润色       [░░░░░░░░░░] 待开始（原 M5 顺延）
R3.9 程序化树         [██████████] 完成：形态 + 烘焙单 mesh + chunk 流式 + 三层 LOD（2026-10-27）
R3.10 叶片卡片        [████████░░] 贴图烘焙 + 卡片几何 + alpha 材质 + 透视率量化完成；冠内仍偏实，待实机验收（2026-10-30）
R3.11 建筑立面        [██████░░░░] 11.1-11.3 完成（立面贴图/玻璃反射/室内灯/米制 UV/占位几何修正/屋顶轮廓）；零件/街道家具待做（2026-10-31）
R3.12 观感修复补充    [██████████] 完成（叶卡贴枝/屋顶漏缝/屋顶重复/远山/横担/LOD dither，2026-11-05，待实机验收）
R3.13 跨街上部元素    [██████████] 实现完成（跨街电线/悬挑灯臂/防护棚，2026-11-09，待实机验收）
R3.14 灯头绘制距离    [██████████] 实现完成（灯头 60 m 外不绘制，2026-11-09，待实机验收）
R3.15 环路外景物带    [██████████] 实现完成（带内建筑行+角树丛/檐底封闭/长灯臂两段/棚架宽度随机，2026-11-11，待实机验收）
R5.1-5 天空调参+云    [████████░░] 面板/云种/sky_gain/dome 完成；云边缘块状属已知边界（2026-10-27 收尾）
R6: 自研体积云        [█░░░░░░░░░] 6.0 管线骨架完成（2026-11-11；crate CloudPlugin 已停用）
```

### R3.11 屋顶轮廓（2026-10-31，步骤 11.3）

**结构**：`world/roof.rs`（参数表 + 单位网格烘焙）+ `chunks.rs` 的 `spawn_roof`。与立面同一条设计主线：**屋顶是剪影，不做材质花样**。

- [x] **三种屋顶，一风格一顶**：老城双坡（屋脊沿街、单位网格烘焙 6% 檐口挑出）、公寓女儿墙（四面 0.25 m 绝对厚度的墙，用共享单位立方体实例化——烘焙环形会随建筑缩放把墙厚放大到数米）、厂房锯齿（齿沿**正面**重复，齿距 6 m 自动取整并吸收余量，保证齿刚好铺满正面）
- [x] **单元网格 + 实例化**：全城一个 gable 网格、一个 tooth 网格、一个 cube，按 `(frontage, rise, depth)` 缩放。屋面材质用树配方（roughness 1.0、reflectance 0.0）——屋脊上一点湿塑料感的高光正是树材质注释警告过的
- [x] **配色在剪影预算内**：`tint_is_darker_than_its_wall` 断言屋顶比所在风格的墙面更暗（老城 0.27 vs 0.54、公寓 0.42 vs 0.47、厂房 0.22 vs 0.27，均为 sRGB 亮度）
- [x] **绕序由测试钉死**：wgpu 默认 `FrontFace::Ccw`，`every_triangle_faces_outward` 用右手规则逐三角形重算法线并要求指向外——一个翻面的三角形就是天际线上的一个洞，别的测试看不见它。**该测试抓到两处真实 bug**：山墙三角形绕序反了（第一版 z_wall 解构也写反）、齿侧面三角形绕序反了
- [x] **齿数铺满正面**：`teeth(frontage)` 返回 `(count, width)`，count 取最近整齿数、width 吸收余量
- [x] **变体抖动**：屋脊高按立面变体 ±12%，避免一条街的山墙齐平
- [x] **测试（新增 5 个，总数 188）**：朝外性、屋脊沿街且檐口对称、齿铺满正面且宽度不漂、rise 预算、屋顶比墙暗
- [x] **顺手修掉一个既有日志污染**：树的 Far LOD 按设计去枝后 wood 网格为 0 面，网格分配器每次运行告警 12 次，把真实告警埋掉。现在空枝网格以 `None` 表示、该层只生成叶实体（名字沿用层的规范名，LOD 测试不受影响）。**实测：每次运行告警 12 → 0**
- [x] **对照图**：`cargo run --example roof_gallery` → `.tmp/roof_gallery/roof_sheet.png`（CPU 光栅化、背面剔除、画家算法）。**数值验证**：工厂顶轮廓 8 次方向变化、平滑剖面呈波状（245→232→234→224→229→214→212→218→257），与 8 齿一致
- **验证缺口**：图像通道仍然 `persistence_failed`，山墙/女儿墙/锯齿的实际观感待 Feihei 实机确认；对照图本身可用

**下一步**：R3.12 候选项见「风险与依赖」上方待验收清单；2026-11-04：立面 + 玻璃反射 + 室内灯 + 占位几何修正 + 屋顶轮廓 + 建筑零件 + 街道家具已全部落地，R3.11 完整收官，待 Feihei 实机验收。

### R3.11c 建筑零件与街道家具（2026-11-04，步骤 11.4–11.5，方案 A：全几何零件）

**结构**：`world/props.rs` 是纯数据 + 放置 + 网格烘焙；`chunks.rs` 的 `PropAssets` 是材质与 GPU 侧；挂接点在 `place_buildings`（建筑零件）与 `plan_chunk` 的 (edge, side) 循环（街道家具）。

**关键决策**：
- **零件 RNG 与主流隔离**：建筑零件从建筑自身 position 位哈希播种（同 `facade_variant` 的理由——多用一次边缘流会平移下游全部已验收的 wear 值）；街道家具用专属 salt 流 `12000+side`。11.1–11.3 的所有随机序列零位移。
- **共享网格 + 共享材质**：盒/柱类零件复用 unit cube/unit cylinder 缩放；雨棚、阳台、灯臂、灯头、电线 5 个绝对尺寸烘焙网格（倾斜/悬链不耐受非均匀缩放）。8 个材质全市共享（暗金属/锈蚀/米色外机/砖/双雨棚布/灯头 emissive/电线），锈蚀档由建筑 wear 驱动（wear>0.6 时 65% 锈）。
- **路灯点亮**：golden hour 灯头 emissive LinearRgba(4.0, 2.88, 1.68)，靠 bloom 拾取，不花真实光源预算。
- **电线悬链**：unit-span 抛物线管（菱形截面 8 段），实例按 `(span, span/30, span/30)` 缩放——垂度随跨距按真悬链比例增长，截面保持在 ~2cm。

- [x] **11.4 零件表**：老城=雨棚（每开间 45%）+外机（两层楼下窗线）+落水管 35%+穿脊灶烟囱 30%；公寓=外机网格（每开间每层 18%、上限 14）+阳台（隔开间 60%，每层）+全高落水管 50%+屋顶水箱 70%+楼梯间 80%；厂房=高烟囱 80%+齿间通风帽 2-4
- [x] **11.5 街道家具**：每侧 curb+0.45m、每 30m 一杆（±2.5m 抖动）；每杆 45% 挂灯臂+emissive 灯头（朝路心）；相邻杆间 3 层电线（5.6/6.1/6.6m，垂度 3% 跨距）。电线与墙面横向间距 ≥0.55m（立面在 setback≥1m 处），穿越老城上层窗带属预期观感，不与山墙相交
- [x] **放置约束**：零件不进街道走廊（贴立面凸出 ≤0.6m < setback 1m）；路灯臂朝向断言 dot>0.999；街道家具与建筑零件用同一 `PlacedKind::Prop` 通道流式进/出
- [x] **测试（新增 15，总数 203）**：位置键控稳定性、立面贴附（沿/法向双轴）、屋顶净空、各风格签名零件、数量预算（老城≤9/公寓≤45/厂房≤8）、box 绕向（凸体质心法）、雨棚外沿下垂、curb 线逐侧、臂朝路心、跨距几何、流稳定 + 逐 edge 键控、短线双杆、网格健全性、plan 级「零件贴建筑」与「杆在 curb 线」
- [x] **实测（seed 0x48454144 全图）**：建筑零件 8046（外机 2303/雨棚 2935/阳台 832/烟囱 864/落水管 979/楼梯间 42/水箱 61/通风帽 30）+ 街道家具 2778（杆 948/灯臂+头 408+408/线跨 1422），共 10824，平均每 chunk ~42
- [x] **实测（cargo run 85s）**：零 shader 错误、零管线错误、零 B0004；`placement assets built` 正常，autosave 正常
- **验证缺口**：`inspect_images` 本会话仍 `persistence_failed`，零件密度/锈蚀观感/灯头亮度/电线下垂全部来自数值断言，**必须由 Feihei 实机确认**；灯头 emissive 强度（4.0）与雨棚颜色是首调参数，不合口味直接改 `props.rs` 常量

### R3.12 观感修复与小件补充（2026-11-05）

**背景**：Feihei 列出的六项实机观察问题，逐项修复并各自提交（`dbd4059` 起）。全部为现有系统的修正/补充，不新增系统。

- [x] **叶卡贴枝**（`tree_mesh.rs`）：旧烘焙把卡片**中心**推到 0.72–1.0 簇半径的壳层，且偏移方向 `dir` 主要是卡片平面法线而非生长方向，叶柄边最多离枝一整个半径——近看是挂着叶子的空心绿环。改为叶柄边锚在枝点附近的小球（`ATTACH_FRACTION=0.28`）并沿 `grow` 向外生长；`CARD_REACH_FRACTION` 0.26→0.30、槐树 `reach_scale` 0.62→0.80 补回冠体积。回归测试 `every_card_grows_out_of_its_branch_attachment_point` 钉死叶柄距簇心 ≤0.40 半径。`target/tree_gallery.png` 复核对齐。
- [x] **坡屋顶漏缝**（`roof.rs` / `chunks.rs`）：锯齿齿网格基座本就在 y=0，实例化却又抬高 `rise*0.5`，檐口线出现通天空的槽；山墙端三角只到 `z_wall(0.5)` 而坡面到 `z_eave(0.56)`，两端各留一个三角缝。齿落到檐口、端三角补到 `z_eave`，新增测试 `gable_end_caps_reach_the_eave`。
- [x] **公寓屋顶重复锯齿**（`props.rs`）：水箱/楼梯间固定尺寸、固定中线偏移、生成率 70%/80%，联排同风格建筑于是排出一串相同盒子。改为逐建筑随机尺寸 + `roof_spot`（限制在 footprint 与 0.45 长度带内）随机落点，生成率降为水箱 0.5（副箱 0.3）、楼梯间 0.55；测试断言尺寸集合多样性。
- [x] **远山剪影**（新 `plugins/hills.rs`）：三层不透明 unlit 山脊带（半径 640/740/840 m、峰高 42/60/80 m），固定 seed 的周期正弦山脊，颜色随层距变浅；`BASE_Y=-200` 防止地面尽头露出天空；仅跟随相机 XZ。`sky_screenshot` 冒烟复核层叠关系。
- [x] **电线杆横担**（`props.rs` / `chunks.rs`）：每杆在最高线高度（`CROSSARM_MOUNT=6.6`）挂一根 1.6 m 横担（梁 + 两个绝缘子座 + 抱箍），默认横跨街道，绕垂直轴 ±0.17 rad 逐杆抖动；`every_pole_carries_a_crossarm_at_the_wire_height` 断言一杆一担、高度与抖动范围、yaw 多样性。
- [x] **树 LOD 渐变**（`tree_mesh.rs` / `tree_sway.wesl` / `chunks.rs`）：三层都带 `VisibilityRange`，相邻层共享 8 m（`CROSSFADE`）margin 区间触发 dither 交叉淡出（旧版 Near 无范围、40 m 硬切最刺眼）；自定义顶点着色器在 forward 与 prepass 都透传 `visibility_range_dither`。
- [x] **验证**：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全绿（210 lib + 1 集成）；游戏实跑 50 s 零 shader/管线错误；`sky_screenshot --ignored` 通过并出图（远山层叠可见）。
- **验证缺口**：槐树冠密度、远山浓淡、横担观感仍待 Feihei 实机确认（数值只能保证几何/范围正确）；远山颜色与峰高是首调参数（`plugins/hills.rs` 常量）。

### R3.13 跨街上部元素（2026-11-09）

**背景**：Feihei 指出既有生成只"沿 block 边界向内"布置，缺一类**跨过道路**的元素——横拉电线、带可穿行道路的棚架、悬挑伸进路面上空的路灯杆。这类几何有特殊约束：相机在路面高度穿行，竖直件不能进街道，悬空件不能撞相机。

**决策**：新增 `world/overhead.rs` 独立层，规则见 `adr-0006-cross-street-overhead.md`。核心是"竖直件落在 curb 外的碰撞棱柱内 + 悬空件下沿 ≥ 3.5 m"，净空计算只有 `overhead_lowest()` 一个真源。

- [x] **净空契约**：`OVERHEAD_MIN_CLEARANCE = 3.5`（eye 1.65 m + 1.85 m 余量）；`overhead_lowest(kind, base, size)` 按种类算实际下沿（电线垂度随跨距缩放、灯罩下垂、顶板底面），放置时 `debug_assert!`，测试对全图所有实例再断言
- [x] **竖直件不越界**：立柱/灯柱站 curb + `POLE_STANDOFF`，落在 block 碰撞棱柱内→玩家无法触及，**不加碰撞体**；测试对**每一条 edge** 校验（不是只校验自己那条）。路口实测教训：端距 3 m 时立柱探进垂直街道走廊 0.63 m，故 `SCAFFOLD_END_MARGIN` 取 6 m > 最宽街半宽 5 m
- [x] **跨街电线**：40% 杆对挂两层（6.0 / 6.5 m）；两端各接对侧杆，杆位抖动不同→略斜跨；垂度 0.45 m @10 m 标称，截面 z 不缩放（沿街线按 span/30 缩放，8-12 m 跨会把 2.5 cm 细成 6 mm）
- [x] **悬挑灯臂**：30% 的**无短灯臂**杆加装；悬挑 4.2 m（curb+0.45 起 → 伸入路面 3.75 m），挂高 7.1 m 高于横担；烘焙安装板→收细臂→下垂外段→灯罩，透镜比灯罩低 1.5 cm
- [x] **防护棚**：18% 的街；四柱（3.9 m）+ 两纵梁（3.9）+ 三横梁（4.1）+ 顶板（4.3），三层错开防 z-fighting；选址按 1 m 步长扫掠，避开杆位（pad 0.6 m）与**树冠**（pad 2.2 m，按冠幅而非树干）；找不到干净区间就不建
- [x] **零位移接入**：`pole_slots()`（props）与 `street_tree_alongs()`（placement）从既有内联循环抽出为纯函数，抽取顺序逐 draw 相同；本层专属 salt 13000，顺序固定（电线→长灯臂→棚架）。**已用 HEAD 基线对照 `placement_stats` 逐项核实**：pole 948 / crossarm 948 / lamp_arm 424 / wire 1422 / ac 2303 / awning 2935 / pipe 979 全部一致
- [x] **顺手修掉一个既有缺陷**：`bake_lamp_head` 的发光体原**完全包在**灯罩实心盒内（灯罩 y −0.15–0.0，灯头 y −0.13–−0.01），全城"唯一自发光道具"一个像素都看不到；现透镜比灯罩底面低 1.8 cm
- [x] **测试（新增 9，总数 226）**：净空全图扫描、立柱不进任何街道、电线两端落在两侧杆位线、长灯臂方向/不与短灯臂重复、棚架四柱+跨街顶板+梁高、棚架避开杆与树冠、流稳定+逐 edge 键控、本层确实产出、烘焙网格健全性（含"透镜必须在灯罩之外"）
- [x] **实测（seed 0x48454144 全图）**：跨街电线 294（147 跨）、长灯臂+灯头 130+130、棚架 17 座（立柱 68 / 横梁 85 / 顶板 17），新增实体合计 724，平均每 chunk 2.8
- [x] **验证**：`cargo fmt --check`、`clippy --all-targets -D warnings`、`cargo test`（226 lib + 集成）全绿；限时 45 s 实跑，世界生成 / placement assets / autosave 正常，零 panic、零 shader/管线错误
- **验证缺口**：跨街电线垂度与密度、灯头亮度（尤其修好后的短灯臂）、棚架 4.3 m 高度在 22.55° 俯仰下的观感、顶板胶合板配色，全部来自数值断言，**待 Feihei 实机确认**。首调参数全部集中在 `world/overhead.rs` 常量区
- **已知限制**：棚架在树密的公寓/厂房街上几乎不出现（树冠 pad 2.2 m 挤掉了干净区间），全图仅 17 座——刻意取舍；若换成"不避树"需先解决顶板切树冠

### R3.14 灯头绘制距离（2026-11-09）

**背景**：Feihei 实机反馈"远处的路灯在视口中会闪烁，应该也设定 lod，远处的灯光应该不可见"。

**根因**：灯头是一片 2 cm 厚的发光薄片（`bake_lamp_head` 只比灯罩低 1.8 cm），而主相机是 `Msaa::Off`——bevy_elements 的云 pass 读单采样 depth，这是硬前提。没有 MSAA，比一个像素行还薄的亮条不会被淡化成暗条，而是**这一帧盖住那行、下一帧漏掉**，bloom 把通断放大成闪烁。按 45° 竖直 FOV / 1080p / 眼高 1.65 m 换算：透镜只在 50 m 内能盖满一个像素行，80 m 时高 0.65 px，100 m 外只是一个点。

- [x] **绘制距离**：`props::LAMP_GLOW_RANGE = 60`，`LampHead` / `LongLampHead` 在 `spawn_prop` 里挂 `VisibilityRange::abrupt(0, 60)`（`world/chunks.rs::lamp_glow_range`）。**只有这两个灯头有绘制距离**，杆、线、雨棚、棚架梁板在 chunk 窗口内一律绘制
- [x] **用 abrupt 不用 crossfade**：`VisibilityRange` 的淡出是屏幕空间 dither，落在 2 px 的点上只是把"闪"换成"麻"，而且整个靠近过程都在麻；abrupt 走 CPU 侧 `check_visibility_ranges` 直接剔除，连 dither pipeline key 都不会打开
- [x] **灯罩保留**：只摘掉发光透镜，暗金属灯罩留在原处，远处灯杆轮廓仍然完整——"灯太远看不见灯泡"符合直觉，也不会出现半截灯臂
- [x] **测试**：`only_the_lamp_lenses_have_a_draw_distance` 全图扫描，断言两个灯头都带 `abrupt(0, LAMP_GLOW_RANGE)`、**其他任何具名实体都没被挂绘制距离**（树的三层 LOD 范围除外）
- [x] **验证**：`cargo fmt --check`、`clippy --all-targets -D warnings`、`cargo test`（227 lib + 4 集成）全绿；限时 40 s 实跑，世界生成 / placement assets / autosave 正常，零 panic
- **待实机确认**：60 m 的截止距离是否合适。调大会带回 50–60 m 段的闪烁；调小街道远端的暖点会明显减少。若以后想"既远又稳"，正确解法是 fixed-angular-size 的 billboard，不是把薄片放大
- **已知限制**：全量测试里 `plugins::save::tests::autosave_writes_only_after_the_interval` 偶发失败（依赖真实帧间隔），单跑必过，与本改动无关

### R3.15 环路外侧景物带 + 屋顶封闭 + 灯臂形状 + 棚架宽度（2026-11-11）

**背景**：Feihei 实机列出的四项观察，两项是"缺了一块"（景物带、檐底），两项是"形状不对"（长灯臂折点、棚架宽度恒定）。前两项要新增几何，后两项只改已烘焙的零件。

- [x] **最外圈环路外侧空无一物**（新 `world/band.rs`）：根因是外圈 cell 标成景物带（`SCENERY_BAND_CELL`）后**不分配 Block**，而 `plan_chunk` 只对 `edge.left_block/right_block` 有值的边放东西——带内天生无物，往外看就是空地接远山。改为新增一层**纯几何布景**（`BAND_SALT = 9000`），不复用 block 逻辑：四条边 `plan_row` 沿环路面排建筑（`SETBACK` 0.4–1.6 m、`FRONTAGE` 5–13 m、`DEPTH` 7–14 m，13% 换成 2.5–5 m 宽 9 m 深的巷道），四个角 `plan_corner_grove` 摆 3×3 抖动网格树。风格配比 `roll_style` 取 0.72 老城 / 0.88 公寓 / 其余厂房，高度上限压到公寓 21 m——"这排是从 3-5 m 外的路面看的，24 m 板楼在那个距离像悬崖而不是天际线"。建筑走 `PlacedKind::SceneryBuilding(kind)`，直接复用既有立面 / `snap_to_bay` / 屋顶烘焙，`chunks.rs` 只是把两个变体并到同一个 spawn 分支。`tree_instance` 无论有无资源都消耗三次抽样，保证有无 `tree_assets` 时行布局一致
- [x] **景物带与碰撞环内边界不一致**（`collision.rs`）：碰撞环从 `OUTER_BAND_CELLS*CELL_SIZE`（50 m）起算，而环路外缘在其外侧 `STREET_WIDTH/2`＝4 m，所以玩家其实能往带里走 4 m 撞进建筑。统一为 `band::BAND_INNER = 50 - 4 = 46`，`collision.rs` 用同一算式
- [x] **角落棱柱缺口**（`collision.rs`）：`scenery_ring_prism` 裁剪出多个矩形却**只取面积最大的一个**，四个角各留 4×46 m 的开口。改为 `scenery_ring_prisms -> Vec<SolidPrism>`，每个裁剪矩形各自成柱（允许重叠）
- [x] **坡屋顶没有底面**（`roof.rs`）：山墙棱柱与锯齿楔形都只有侧面和顶面，从下往上是背剔的内部，望天。`bake_gable` 补檐底 soffit 四边形、`bake_tooth` 补楔形底面，与墙顶共面——共面不花代价：从下看墙顶面被剔除、从上看看 soffit 被剔除，两者永不同时栅格化。新增 `roofs_are_closed_shells()`：量化 1/1024 后做有向边配对，断言每条边恰好被反向使用一次（比逐个补面更能防回归）
- [x] **长灯臂中间"折了一下"**（`overhead.rs` / `props.rs`）：旧臂是单根斜杆从 `LONG_LAMP_MOUNT` 直接落到灯头，一路下坡到悬挑末端，看起来像折断。改为「安装底托 → 斜段（`LONG_LAMP_RISER` 1.15 m 沿路爬升 `LONG_LAMP_RISE` 0.55 m，约 26°）→ 水平直段 → 灯头外壳」，斜段用新增的 `props::push_box_rot_z`（绕局部 +Z 旋转且保持绕序）烘焙。`overhead_lowest` 跟着改成按 `LONG_LAMP_RUN_UNDER` / `LONG_LAMP_LENS_BOTTOM` 逐段推导，净空仍只有一个真源
- [x] **棚架宽度恒定**（`overhead.rs`）：旧宽度死等于 `2*curb`。改为两侧**独立**随机外扩：`cap = (setback - POLE_STANDOFF - SCAFFOLD_POST_PAD).clamp(0.0, SCAFFOLD_WIDEN_MAX) + POLE_STANDOFF`，立柱落 `curb + offset`，两侧不对称时横梁/顶板按 `width = half_left + half_right`、`centre_across = (half_left - half_right) * 0.5` 跟着偏。`place_scaffold` 因此多收一个 `params: [BuildingParams; 2]`；`place_overhead_furniture` 里 `sides` 解构成 `[Some(left), Some(right)]` 后直接取值（不再 `map_or` 兜底，棚架本来只在两侧都有的街上建）
- [x] **测试（净增 6，总数 243）**：`every_side_of_the_ring_gets_a_row`、`the_row_stands_in_the_sealed_band`、`the_row_leaves_the_band_empty_behind_it`、`band_objects_are_placed_once`（band）；`roofs_are_closed_shells`（roof）；`long_bracket_rises_then_runs_level`（按三角形法线分类：恰 1 个上倾斜面 + 水平段不低于 `LONG_LAMP_RUN_UNDER`）、`gantry_width_varies_with_its_street`（宽度 > `2*curb`、≤ `2*(curb+POLE_STANDOFF+SCAFFOLD_WIDEN_MAX)+0.31`，且不同宽度 >10 种）
- [x] **实测（seed 0x48454144 全图）**：`band/old` 191 座（6–12 m）、`band/apt` 50 座（12–21 m）、`band/fac` 39 座（12 m）、`tree/band` 103 棵；`long_arm`/`long_head` 各 130、`scaf_deck` 17，与前一轮 `R3.13` 计数一致（说明新增景物层没有扰动既有流）
- [x] **验证**：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --lib`（243）全绿
- **新增开发工具** `examples/view_shot.rs`：按 `X Z YAW PITCH SEED SHOT_OUT` 环境变量渲染单帧第一人称截图（`save_to_disk`）。既有 `examples/top_view.rs` 的相机在运行中被游戏系统接管，定不住视角，所以另建一个只做拍摄的
- **验证缺口**：景物带的**观感**（黄昏色、老城占比、21 m 上限是否还压得住天际线）、檐底在 45° 仰角下的暗度、长灯臂 26° 斜段的比例、棚架最宽 1.5 m 外扩后是否有"顶板悬在没柱子的地方"的错觉——全部来自数值与截图，**待 Feihei 实机确认**。首调参数集中在 `world/band.rs` 常量区与 `world/overhead.rs` 常量区
- **已知限制**：景物带建筑只做正立面朝向环路（背面朝外不可达，也就不补配件）；带内不生成电线杆与街道家具，"带是一个布景幕布，不是第二个城市"

## 风险与依赖

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| bevy_elements 0.5.3 依赖不匹配 0.20 | R0 阻塞 | `cargo check` 先行；不匹配回退 ADR-0005 方案 B（0.19 或纯内置） |
| bevy_elements 无人工审计 | 隐藏 bug/行为不符 | R0.3 读源码；关键参数测试断言 |
| Bevy 生态版本迭代快 | 升级成本 | 精确锁版本；升级独立里程碑 |
| Blender 建筑模块制作耗时 | R3 建筑占位 | BoxMesh 占位先行（沿用原项目策略） |
| 树 .blend → glTF 导出质量 | 视觉质量 | R0.5 先行验证 |
| WESL 重写顶点色摆动 | 树动画损失 | 接口保留在 ADR-0004，R3.5 重点验证 |
| **扰动后相邻街区交叠** | 街道缝隙错位、碰撞穿插 | 原版实测过只取 4 角点导致 **7.47m 交叠**；必须沿完整周长取 `(n+1)×4` 顶点（2.10），OVERLAPS=0 测试覆盖多 seed |
| **外侧围墙带留缝** | 玩家从地图边缘走出/看到空洞 | 逐边 inset 判定（2.11）：邻居越界或外侧则 `inset=0` 直接跳过裁剪；新增共享顶点距离断言 |
| **生成算法与原版偏离** | 玩法观感不符（过整/过碎） | 已建立可复现比对路径：`cargo run --example world_map` 出顶视图，与原项目 `bake_city.tscn` 输出比对 |
