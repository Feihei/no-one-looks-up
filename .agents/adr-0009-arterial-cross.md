# ADR-0009: 主干道十字切分（三级路网）

- 状态: 已实现（R3.19，2026-11-18）
- 关联: ADR-0001（50m 格子）、ADR-0008（两级路网细分）
- 背景反馈: R3.18 实机+地形图评审——"一级道路太多，50m 街区重复感强"

## 问题

ADR-0008 解决了街区**内部**的岔口密度，但 tier-0 仍有两个结构性重复：

1. **同级路网**: 每两个街区的接缝自动成为一条 tier-0 街道（6-10 m），全部同级、无主次。真实的城市路网是少数宽的主干道 + 大量窄的支路。
2. **50 m 等距节奏**: 老城占建成区 87% 且强制 1×1，约七成地图面积是 50 m 一格的均匀网格，观感重复。

## 决策

在 tier-0 内部分级，而非再加一层间距体系（用户决策 2026-11-18："先以 10-15 米的主干道进行十字划分，变成四个地块，再分别用现有的算法进行两级路的划分"）：

1. **主干道十字**: 一条竖 lattice 线 + 一条横 lattice 线，宽 10-15 m（每轴独立抽取），把 800 m 地图切成 4 个象限。
2. **抖动 `{7, 8}`**: 十字在地图中心 ±1 格（350/400 m lattice）内抽签。最小象限 6 格 = 300 m > 厂房 5×5 = 250 m，零合并退化；更宽的抖动会在象限边缘重新引入 clamped-merge 噪声。
3. **抽签顺序固定**: `generate_world` 开头按 `col → row → width_v → width_h` 从同一 RNG 抽取（`ArterialCross`），主干道是第一个 tier-0 决策，整个流在其后保持确定性。
4. **十字线钉住**: `perturbed_nodes` 对 `col == cross.col || row == cross.row` 的节点不扰动（与 ring 线同理），主干道笔直；两端与外环 T 汇，交叉点为 4 度 Junction。
5. **象限扫描**: `partition_blocks` 按 NW→NE→SW→SE 四象限行主序扫描，扫描范围不跨十字线——街区永不跨越主干道。类型抽取、merge、退化逻辑不变。
6. **逐边 inset**: 临主干道的街区该侧退 `arterial/2`（其余边退自身 `street_width/2`）；`inset_polygon_clip_with_insets` 的逐边 inset + 顶点取相邻均值已支持。十字接缝的边宽 = 主干道绘制宽（不被两侧街区平均掉）。
7. **`EdgeKind::Arterial`** 新变体（`#[default]` 仍是 `Street`，serde 兼容）；`StreetUnitEdge` 加 `kind` 字段传递。下游语义统一为"主干道就是一条街"：建筑/树/街具照常面向它（curb = width/2 自动缩放）、巷道口可以 T 汇到主干道、行人可横穿。
8. **巷道口 reach 按候选边宽度**: `place_mouth` 的 reach 从 `block.street_width*0.5+3.0` 改为按候选边 `edge.width*0.5+3.0`——否则临 15 m 主干道一侧的巷道口够不到中心线。

## 附带修复（被新布局暴露的既有缺陷）

凸包碰撞棱柱跨过锯齿状 inset 边界的凹处：大合并块（4×N 厂房）沿强扰动街道时，`convex_hull` 的弦可越过街区自身的 inset 面凸进街道（实测 seed 0x48454144：凸包面越过 6.9 m 街道中心线 0.3 m，行走者被夹死）。修复：`prism_polygon` 把凸包再剪裁回多边形每条边的内侧半平面（棱柱 ⊆ 多边形，街道保持完整 inset 宽度），退化时回退凸包。

## 后果

- 三级路网：主干道（10-15 m）/ 街道（6-10 m，城中村 2-3.5 m）/ 巷道（2.4-6 m）。minimap 主干道 2.5 px 加粗加亮、debug_viz 品红、world_map 示例加粗。
- 同 seed 世界布局改变（RNG 流前移 4 次抽取 + 象限扫描顺序）→ `SAVE_VERSION` 1→2，旧档干净拒绝。
- `SCAFFOLD_END_MARGIN` 6→8 m（原依据 STREET_WIDTH_MAX=10；主干道最大半宽 7.5 m 必须落在脚手架立柱内收距离之内）。
- 主干道与横街交角的 mitre 顶点距主干道中心线为 `(主干道半宽 + 横街半宽)/2`——T 路口处路面瞬时收窄，可接受（仍 > 10 m）。
- 出生点 (400, 400)：十字取 8 时正好落在交叉口上，取 7 时距十字 50 m——后者与既有出生点落块风险相同，非本次引入，不在 R3.19 处理。

## 测试锚点

- `arterial_cross_splits_the_map_into_four_quadrants`（新）：恰好一竖一横、宽度正确、无块跨线、临线块退够半宽（多 seed）。
- `lattice_nodes_are_perturbed_but_borders_are_pinned`（更新）：arterial 线钉住断言 + moved 比例阈值 0.95→0.8（新钉 ~25 节点）。
- `street_widths_stay_in_range`（更新）：按 kind 分支，arterial 查 10-15。
- 既有 `streets_have_no_dead_ends` / `inset_polygons_never_overlap` / `walking_every_street...` / `walking_every_lane...` 自动覆盖主干道（多 seed）。
