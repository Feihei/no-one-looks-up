# R3.19 / ADR-0009：主干道十字切分（三级路网）

## Context

用户反馈重构后的两级路网"一级道路太多、50m 街区重复感强"。根因：tier-0 的每条街区接缝都是同级街道（6-10m），老城占 87% 且强制 1×1，导致全图约七成面积是 50m 等距、同级路网。用户决策：**先用一条 10-15m 宽的主干道十字（一条竖 lattice 线 + 一条横 lattice 线）把地图切成 4 个象限，每个象限内部继续用现有两级算法**。tier-1 巷道细分不变。地图范围不变（800m，`GRID_CELLS=16`）。

## 设计要点（已验证）

- **十字位置带抖动**：`col ∈ {7,8}`、`row ∈ {7,8}`，从 generate_world 的同一个 RNG 按固定顺序抽取（col → row → width_v → width_h），保证 bit-exact。最小象限 6 格 = 300m > 厂房 5×5 = 250m，无退化。
- **主干道线钉住不扰动**（与 ring 线同理），主干道笔直落在 lattice 线上；两端与外环 T 汇，交叉点为 4 度 Junction。
- **逐边 inset**：`inset_polygon_clip_with_insets`（[geometry.rs:70](file:///e:/play/coding/heads-up-bevy/src/world/geometry.rs#L70)）已支持 per-edge insets；临主干道的块该侧 inset = arterial 宽/2。
- **下游零结构性改动**：placement 按 curb=edge.width/2 自动缩放；collision `!= Lane` 已覆盖；`street_segments` 的 run 合并只接受同向候选，十字交叉处不会错误续接。
- 存档只存 seed（save.rs L97 从种子重建），LayoutGraph 不落盘 → EdgeKind 加变体无 serde 问题；但同 seed 世界改变，**SAVE_VERSION 1→2** 让旧档干净拒绝。

## 实施步骤

### 1. `src/world/mod.rs`
- 加 `ARTERIAL_WIDTH_MIN = 10.0`、`ARTERIAL_WIDTH_MAX = 15.0`；注释注明 overhead `SCAFFOLD_END_MARGIN` 约束（arterial 最大半宽 ≤ margin）。

### 2. `src/world/layout_graph.rs`
- `EdgeKind` 加 `Arterial` 变体（`#[default]` 仍是 `Street`，serde 兼容）。
- `StreetUnitEdge`（pub(crate)）加 `kind: EdgeKind` 字段；`from_generation` 传递 kind；修正 `street_segments` 关于交叉点的过时注释。
- 测试 L540/L556 的 `StreetUnitEdge` 字面量补字段（编译级）。

### 3. `src/world/path_network.rs`（核心）
- 新增 `ArterialCross { col, row, width_v, width_h }`；`generate_world` 开头抽取。
- `perturbed_nodes(rng, cross)`：`col == ac || row == ar` 的节点钉住（消费同样的两次 draw，保持 RNG 流对齐）。
- `partition_blocks`：按 4 象限（NW→NE→SW→SE）行主序扫描，扫描范围不跨 arterial 线；类型抽取 / merge / 退化逻辑不动；cell→block_id 写回仍在函数尾部统一做。
- `block_perimeter`：顶点旁记录 lattice 坐标，返回顶点 + `(row, col)`，供逐边 inset 判断"边两端同 col==ac → width_v/2，同 row==ar → width_h/2，否则 street_width/2"。
- `street_unit_edges`：竖线 col=ac（unit rows 1..14）、横线 row=ar（unit cols 1..14）的边宽 = arterial 宽、kind = Arterial；其余不变。
- `associate_edges_with_blocks`：不改（arterial 线笔直，几何判定天然成立）。

### 4. `src/world/subdivision.rs`
- `flanking_street_width`（L562）、`place_mouth`（L649）等 6 处 `!= EdgeKind::Street` 改为 `== EdgeKind::Lane` 才跳过（巷道口可落在主干道，形成 T）。
- `place_mouth` 的 `reach` 从 `block.street_width * 0.5 + 3.0` 改为按候选边 `edge.width * 0.5 + 3.0`（否则临主干道一侧的巷道口够不到 15m 主干道中心线）。

### 5. 下游消费点
- [placement.rs L1035](file:///e:/play/coding/heads-up-bevy/src/world/placement.rs#L1035)：`street_half_width_at` 放行 Arterial。
- [debug_viz.rs L114](file:///e:/play/coding/heads-up-bevy/src/world/debug_viz.rs#L114)：match 加 Arterial 臂（编译必需），配色区分主干道。
- [minimap.rs](file:///e:/play/coding/heads-up-bevy/src/plugins/minimap.rs#L33)：加 `ARTERIAL_THICKNESS ≈ 2.5`，主干道加粗。
- [overhead.rs](file:///e:/play/coding/heads-up-bevy/src/world/overhead.rs)：`SCAFFOLD_END_MARGIN` 6.0 → 8.0，更新注释（原依据 STREET_WIDTH_MAX=10）。

### 6. 存档与文档
- `save_data.rs`：`SAVE_VERSION` 1 → 2。
- 新增 `.agents/adr-0009-arterial-cross.md`（决策：三级路网、抖动范围、抽签顺序、逐边 inset）；`dev-plan.md` 加 R3.19；`glossary.md`/`README.md` 如有两级路网描述则同步为三级。

### 7. 测试

更新（必炸）：
- `street_widths_stay_in_range`：边宽断言按 kind 分支，arterial 查 10..15。
- `lattice_nodes_are_perturbed_but_borders_are_pinned`：denominator 排除 arterial 线节点，并加 arterial 钉住断言（否则 moved 比例跌破 95%）。

新增：
- 恰好一条竖 + 一条横 arterial，宽度在 10-15。
- arterial unit 边全部 kind 正确、四端与 ring T 汇（端点度 = 3）。
- 无块跨 arterial 线（象限不渗透）。
- 临 arterial 块该侧 inset = arterial 宽/2（多 seed 验证）。
- 既有 `streets_have_no_dead_ends` / `inset_polygons_never_overlap` 多 seed 自动覆盖 arterial。

## 验证

1. `cargo fmt && cargo clippy -D warnings && cargo test`（项目硬约束）。
2. 跑游戏实机验证：十字主干道可见、四级路口正常、巷道口 T 汇到主干道、minimap 主干道加粗、debug_viz 配色区分。
3. 旧存档加载被 VERSION=2 干净拒绝，新档存读往返正常。

## 风险

- RNG 流前移（4 次抽取）→ 所有 seed 世界改变，SAVE_VERSION 升级兜底。
- 15m 跨 cross-wire 的 sag/clearance 由 `push_overhead` 的 debug_assert 兜底，跑全 seed 测试验证。
- 出生点 (400,400) 在十字取 7 时可能落在块内——既有风险，非本次引入，不在 R3.19 处理。
