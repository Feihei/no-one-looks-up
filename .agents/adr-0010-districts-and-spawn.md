# ADR-0010: 同类街区连片 + 出生点随机路口

- 状态: 已实现（R3.20，2026-11-19）
- 关联: ADR-0002（世界生成）、ADR-0004（碰撞）、ADR-0008（街区细分）、ADR-0009（主干道十字）
- 背景反馈: R3.19 地图评审——"50m 方块感还是太强，同类地块相邻时内部道路应降级或取消，形成连片的住宅区、森林区或城中村区；城中村不能单块独立，至少像蓝色公寓区一样 2×2；蓝色区域不应该有内部路了"；"出生点可以随机指定一个路口点，避免落地块内"

## 问题

1. **50 m 节奏仍在**: ADR-0009 之后每象限内部仍是 1×1 格为主的均匀拼贴，同类地块（城中村/老城/森林）之间全部是同级街道，读不出"片区"。
2. **城中村单块散布**: `district_size` 之前城中村多为 1×1，地图上是散落的 50 m 小块。
3. **公寓区内部巷道**: 蓝色公寓楼区被 ADR-0008 细分切成内部巷道，但中高层板楼本应沿街布置、街区内部整体成片。
4. **出生点固定 (400, EYE, 400)**: 可能落在街区内部。

## 决策

### 1. 同类连片（district 增长 + 扩展）

`partition_blocks` 重写为两阶段：

- **Phase A（扫描）**: 行主序扫过象限每个格点，每格先以自身为种子长出一个矩形 district（`district_size` 按类型抽尺寸：Apartment/Factory 正方形 2–3 / 4–5；OldCity (1..=2, 1..=2)；UrbanVillage (1..=2, 2..=3)——保证 ≥2 格跨度；Forest (2..=3)²；Plaza (1,1)），再用 `largest_free_rect`（取代 largest_free_square，按面积最大、h 降序 w 降序，确定性）在剩余自由格里放块。`rects: Vec<(u32,u32,u32,u32)>` 记录每块的单元格范围（r1/c1 exclusive）。
- **扩展**（`try_extend_district`）: 扫到新格时，若左侧（或上方）邻块是可扩展类型（村庄/森林/老城）且新格恰好在其东/南边缘、整条 strip 自由，则以 `DISTRICT_EXTEND_PROB = 0.65` 概率把该格吸收进邻 district（矩形保持，双候选时 50/50 抽一个）。连片的城中村/森林/老城由此自然生长，且永不跨象限/主干道（扫描范围即边界）。
- **Phase B（多边形）**: 统一从 rect 建 inset 多边形，逐边 inset 优先级 = 主干道（ADR-0009）> 同类接缝 > 自身半宽。

### 2. 同类接缝降级（seam_width）

`street_unit_edges` 中两个非道路格之间的接缝，宽度取 `seam_width(两侧块)`：

- UrbanVillage: `min(两侧 street_width)`（村与村之间只剩 2–3.5 m 窄巷）；
- OldCity: `OLD_CITY_SEAM_WIDTH = 4.0`；
- Forest: `FOREST_PATH_WIDTH = 3.0`；
- 其余: 平均值。

同时**概率重标定**：per-roll 概率 = 目标面积占比 / 期望 district 面积（plaza 1、forest 6.25、village 3.75、old 2.25、apt 6.25、factory 20.25 格）→ `P_PLAZA_END = 0.1531`、`P_FOREST_END = 0.2102`、`P_VILLAGE_END = 0.2783`（`P_OLD_AREA = 0.87`、`P_APT_AREA = 0.10` 不变），面积配比与 R3.19 持平。

### 3. 公寓区不再细分

`subdivision.rs::params_of` 对 `Apartment` 返回 `None`——蓝色片区不切内部巷道，中高层板楼直接沿街区四周街道布置，片区整体成片（用户决策 2026-11-19）。

### 4. 出生点随机路口

`path_network.rs::spawn_point(graph, seed) -> (Vec2, yaw)`：

- 节点集合 = 全部 `NodeKind::Junction`；以 `seed` 加盐（`SPAWN_SALT`）派生独立 RNG 随机选一个（不占用生成流，布局保持位精确）。
- yaw 朝该路口第一条 tier-0 街方向（`PlayerBody` forward = `(-sin yaw, -cos yaw)`）。
- `main.rs::place_player_at_spawn`（Update 一次性，`Local<bool>`，先于 chunk streaming）+ `menu.rs::start_run` 生成 fresh `SaveData` 时同样用它算出生位姿。
- 空 graph 回退世界中心。

### 5. `NodeKind::Pass`（直行锚点）

度 = 2 且两方向都存在的格点也建节点（此前跳过 → 跨格街区中点两侧 unit 边丢失 → 环线 100 m 破洞）。`street_segments` 保持逐 unit 发射（节点对长边方案曾因弦偏离摆动走廊导致行走失败，已回滚）。

## 附带修复（被新几何暴露）

1. **法线翻转**：`inset_polygon_clip_with_insets` 的质心启发式法线在细长多边形（6 m × 250 m 地块条）上符号不稳 → 法线朝外 → inset 外凸 5 m 进街道。新增 `geometry::inward_normals`（按绕向判定，CCW 取 `(-dy,dx)`），替换 geometry / collision / subdivision 共 4 处。
2. **非凸碰撞棱柱**：半平面裁剪的 kernel 可能排除多边形自身质心、不裁剪的凸包又会桥接凹口凸进街道——两难。新方案 `geometry::convex_decompose`（耳切三角剖分；凸输入直通；退化回退整块）+ `collision.rs::register_chunk` 每个凸片段一个 `SolidPrism`（child 索引保留，`prism_count` 去重不受影响）。输入先去重闭合轮廓（首尾重复顶点会使耳切产生覆盖凹口的无效三角形——实测旧代码产出的 `[A, C, E]` 片段西缘伸进街道卡死行走者，外加 `[A, D, A]` 零面积碎片）。
3. **`push_out` 角点误推**：旧"浅轴"判定把"所有边的 dist_out < radius"当成在内部，站在凸角外 2.06 m（圆与棱柱根本不相交）的行走者也被横推——窄接缝中心线旁的角点把 walker 推离中心线卡死。重写：内部点保留浅轴推出（沿墙滑动平滑），外部点改用**最近边界点径向推出**（角点唯一正确解）。

### 6. 预留 district（R3.20b/c，2026-11-19 当日评审："红色老城占了大部分啊，没有厂房吗？" → "改成整张图预留一个厂区就好" → "最新的地图又没有蓝色公寓住宅区了"）

厂房需要 4–5² 连续空格，公寓需要 2–3²——而填充扫描 + 0.65 吸收率会在轮到对应 roll 之前把象限碎片化，按契约全部降级成老城（实测 5 seed 中 3 个零厂房；公寓配额 10% 也被同一碎片化吃掉，展示 seed 整图无蓝区）。修正（最终形态）：**整张图预留一个厂区 + 一个公寓区**——各自从 RNG 抽一个宿主象限（保证不同象限：公寓象限 = 厂房象限 + 1 + rng(0..3)，确定性偏移无额外流对齐负担），在空象限中心放置（象限最小 6 格 > 厂区最大 5 格，必然放得下，且与主干道十字、环路保持距离），填充扫描绕开它们。填充扫描的 3% 厂房 roll 移除（份额归还老城，built 分割 87/10/3 → 90/10），厂区唯一来源即预留，测试断言 `factory == 1`；公寓填充 roll 保留（可在预留之外继续成片）。

已知方差：森林只在距边界 ≤4 格的林带 roll（`FOREST_EDGE_MAX_DISTANCE_CELLS`），预留区落位象限中心恰在林带内，会挤压森林——8 seed 实测 7 个有森林、1 个为 0。不做第三预留（会冻结四象限骨架），测试不断言森林。

### 7. 配比上调（R3.20d，2026-11-19 当日评审："红色老城区占比还是太高了，适当加一些橙色和蓝色的比例吧"）

沿用 p_i = 份额 × K / 期望面积 的重标定方法（K = 3.05，即 80/20 分割下 built district 期望面积）：

- **城中村面积份额 10% → 15%**：`P_VILLAGE_END` 0.2783 → 0.332（每 roll 概率 0.068 → 0.122）
- **built 分割 90/10 → 80/20**：`P_OLD_AREA` 0.90 → 0.80（公寓填充 roll 翻倍）

6 seed 实测：老城块数 36→25–32，城中村 3–7 带、公寓 1–6 片，森林 5/6 seed 存在（展示 seed 恰好为 0，同第 6 节方差，未进一步干预）。

## 后果

- 同 seed 世界布局改变 → `SAVE_VERSION` 2→3（ADR-0009 先例，旧档干净拒绝）。
- 地图肌理：同类 district 连片（城中村 ≥2 格跨度、森林 2–3²、公寓/厂房 2–3²/4–5²），50 m 独立小块基本消失；同类接缝只剩窄巷，连片区内部巷道由 ADR-0008 细分提供。
- 街道边总数下降（合并接缝消失）→ 行走测试下限从 per-cell 比例改为绝对值 `MIN_WALKED_EDGES = 64`；巷道数下限 15。
- 村 plot 数量按格数缩放（per-cell 6–30、均值 10–20），`lane_widths` 测试下限放宽到 `VILLAGE_STREET_WIDTH_MIN`（巷口合法压窄落在 2 m 同类接缝上）。
- `world_map` 示例：`Pass` 节点不画标记；公寓特写的 `densest_center` 不再要求有 plots（公寓不细分）。
- 棱柱数量上升（每块 1 → 1..N 片），碰撞桶按 chunk 分片不变。

## 测试锚点

- `same_kind_cells_coalesce_into_districts`（新）：同类格共享接缝占比 ≥ 0.5（多 seed）。
- `same_kind_district_seams_are_narrow_lanes`（新）：同类接缝宽度 = seam_width 规则表。
- `block_types_match_the_merge_table`（更新）：village 跨度 ≥ 2、OldCity/Forest 扩展链上限同为象限预算 48、**必含 Factory**（R3.20b 预留）。
- `spawn_point_is_a_deterministic_street_junction`（新）：同 seed 同点、落在 Junction 上、`point_in_convex_inclusive` 确认不在任何块内。
- `subdivision_multiplies_prisms...` / `walking_every_street...` / `walking_every_lane...` / `inset_polygons_never_overlap` / `block_centres_are_solid` 等既有测试自动覆盖新几何（lib 总数 285 → 287，全绿）。
