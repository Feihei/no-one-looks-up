//! Procedural tree skeletons (silhouette study, `dev-plan` R3.9).
//!
//! The shipped trees are alpha-carded glTF scenes, which read as flat paper
//! from the shoulders-up view. Before replacing anything we need to know what
//! a *built* tree looks like: a growth skeleton plus the leaf clusters that
//! will later be instanced as opaque low-poly blobs.
//!
//! This module is **pure data**, like [`crate::world::placement`]: it turns a
//! seed and a [`TreeProfile`] into a [`TreeSkeleton`] and never touches the
//! ECS or the GPU, so the shape can be asserted headless.
//!
//! Determinism (ADR-0002): one `StdRng` seeded from `(seed, species)`, walked
//! in a fixed breadth-first order, so the same inputs produce a bit-identical
//! skeleton on every run and platform.
//!
//! Growth model (per branch, six parameters):
//! - children `n` splits the parent's length budget, so a branch's children
//!   together never exceed `child_len_ratio` of it — this is what keeps a
//!   tree from exploding into a hairball;
//! - children leave the parent at the same forward angle `spread_deg` and
//!   face a total of `whorl_deg` around the parent's axis, each turned
//!   independently by `jitter_deg`;
//! - foliage sits on branches with `level >= leaf_min_level`: one cluster of
//!   `leaf_clusters` blobs at the tip, plus a run along the branch when
//!   `leaf_run_spacing` is positive.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// Seed for tree SHAPES. Distinct from the world seed: a tree's mesh is baked
/// once and shared by every chunk, so its shape must not depend on where it
/// happens to stand.
pub const TREE_SHAPE_SEED: u64 = 0x4845_4144; // "HEAD"

/// Hard cap on clusters distributed along one branch. Higher values let the
/// leaf mass follow the limbs deep into the crown (so branches read as
/// running *through* foliage) at a small instancing cost.
pub const RUN_CLUSTER_CAP: f32 = 9.0;

/// How one species grows. Defaults come from [`TREE_PROFILES`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TreeProfile {
    /// Trunk length before the first split (m, before `height` scaling).
    pub trunk_length: f32,
    /// Trunk radius at the root (m).
    pub trunk_radius: f32,
    /// Radius multiplier per branching level (0.6-0.8 reads as a real taper).
    pub radius_ratio: f32,
    /// Length multiplier passed to the children (0.5-0.8).
    pub child_len_ratio: f32,
    /// First level that carries foliage. 0 = leaves on the trunk itself.
    pub leaf_min_level: u8,
    /// Maximum branching level; children are dropped at `level == max_level`.
    pub max_level: u8,
    /// Children per branch `[min, max]`.
    pub children: (u8, u8),
    /// Children the TRUNK splits into `[min, max]`.
    ///
    /// Kept separate from [`Self::children`] because the two numbers do
    /// different jobs. Every level below the trunk should stay at 2-3 so the
    /// branch count does not multiply out of the instancing budget, but if the
    /// trunk itself rolls a 2 the finished tree is a Y: two limbs, two leaf
    /// masses, nothing in between. That is literally the "two brushes on a
    /// stick" the player reported. A real street tree forks into 3-4 scaffolds
    /// over a short trunk, and the extra scaffolds are what let the crown read
    /// as one mass from underneath.
    pub trunk_children: (u8, u8),
    /// Total length of one branch's children (m) `[min, max]`.
    pub child_span: (f32, f32),
    /// Angle of every child off the parent's axis (degrees) `[min, max]`.
    pub spread_deg: (f32, f32),
    /// Sum of the children's rotations around the parent's axis (degrees).
    pub whorl_deg: f32,
    /// Extra random turn applied to each child (degrees, ±).
    pub jitter_deg: f32,
    /// Upward bias: the child direction is lerped toward world +Y (0..1).
    pub up_bias: f32,
    /// Leaf-cluster blobs placed at a branch tip.
    pub leaf_clusters: u8,
    /// Radius of the cluster blob at `level == leaf_min_level` (m).
    pub leaf_radius: f32,
    /// Radius multiplier per level above `leaf_min_level`.
    pub leaf_radius_ratio: f32,
    /// Spacing of extra clusters along a branch (m; <= 0 disables the run).
    pub leaf_run_spacing: f32,
    /// Minimum height a cluster must clear above the crown's lowest point, as
    /// a fraction of the crown height (0..1). Real crowns are ragged
    /// underneath and open in the middle: a hard floor here cuts the flat
    /// "skirt" a dense blob field otherwise produces.
    pub crown_floor: f32,
    /// Chance a candidate cluster is kept (0..1). Below 1.0 the leaf field
    /// gets gaps so branches show through the silhouette instead of the crown
    /// reading as one solid mass.
    pub leaf_density: f32,
    /// How far a tip cluster may drift off the branch axis, as a fraction of
    /// its own radius.
    ///
    /// This is the "does the leaf clump touch its twig" knob. 0.15-0.20 keeps
    /// every clump anchored; the 0.45-0.50 values were authored for opaque
    /// blobs and, once the card geometry started anchoring its petiole edge at
    /// the cluster centre, put whole clumps half a metre off a 1 cm twig.
    pub leaf_spread: f32,
    /// Earliest point along a leafy branch where clusters may start, as a
    /// fraction of its length. Leaving the first stretch of a limb bare is
    /// what keeps the trunk-to-limb fork visible instead of letting the crown
    /// swallow it (the aspen read as "pole with a ball" at any `leaf_min_level`
    /// because foliage started at 55% of every limb).
    pub leaf_start: f32,
    /// Longest straight run (as a fraction of `height`) a limb may keep before
    /// it is split into another segment.
    ///
    /// A limb baked as one long straight tube has two failure modes the player
    /// sees: bare spokes where the foliage does not reach, or a cylinder of
    /// leaf cards wrapped around the tube. Short segments keep every straight
    /// piece below leaf-cluster size, and the limb becomes a polyline the
    /// foliage can follow.
    pub max_segment: f32,
    /// Total bend of one limb in degrees (±). Spread across the limb's
    /// segments, so a limb is an arc instead of a set of straight spokes.
    pub limb_curve_deg: f32,
    /// How much of a limb's outward tilt it sheds along its own length (0..1).
    ///
    /// Limbs leave the trunk at an angle and, on a real tree, straighten toward
    /// vertical as they grow. Without this every scaffold climbs in a straight
    /// diagonal, so N scaffolds read as N fingers with sky between them no
    /// matter how much foliage they carry. Lifting the far end of each limb is
    /// what turns the fork into a dome.
    pub limb_lift: f32,
    /// How strongly a deeper generation turns AWAY from the crown axis instead
    /// of climbing (0 = every child keeps `up_bias`, 1 = the deepest twigs
    /// leave the crown's core radially).
    ///
    /// This is the "is the crown a ball or a column" knob: limbs climb out of
    /// the trunk, then the twigs fan outward, which rounds the envelope.
    pub crown_rounding: f32,
    /// Overall height (m). Skeleton units are multiplied by this at the end.
    pub height: f32,
    /// Overall width (m) — scales XZ only, so a 3 m x 2 m aspen keeps its
    /// height while reading narrow.
    pub width: f32,
}

impl TreeProfile {
    /// Extra clusters a branch carries along its run (0 when the run is off).
    fn run_clusters(&self, length_with_scale: f32) -> u8 {
        if self.leaf_run_spacing <= 0.0 {
            return 0;
        }
        (length_with_scale / self.leaf_run_spacing)
            .floor()
            .clamp(0.0, RUN_CLUSTER_CAP) as u8
    }
}

/// Species-inspired starting points (aspen / street / broadleaf). These are
/// *subjects for review*, not final art: they exist so Feihei can look at the
/// silhouettes and tell us which knobs to turn.
pub const TREE_PROFILES: [TreeProfile; 3] = [
    // 0: aspen — slender, tall, upright branches, tight crown.
    TreeProfile {
        // Trunk runs 0.42 -> 0.22 of the height. At 0.30 the bare pole still
        // read as a third of the tree in the gallery ("mushroom on a stick").
        trunk_length: 0.17,
        trunk_radius: 0.045,
        // 0.56 keeps every radius step visibly thinner: the terminal twig of a
        // 14 m aspen lands at ~1.2 cm instead of the 3.3 cm a 0.62 ratio leaves
        // (which read as a wire, not a twig). See `MIN_BAKED_RADIUS` in
        // `tree_mesh`: nothing that carries leaves may be culled.
        radius_ratio: 0.56,
        child_len_ratio: 0.75,
        // Foliage from the FIRST fork onward, i.e. the main limbs, the
        // secondary limbs and every twig below them. Leaves only on the last
        // generations left the crown as a ring of separate clumps at the twig
        // tips; the player read "balls in the sky", not one tree. The lower
        // generations now carry sparse sprays as well, which is what ties the
        // crown together. Leaves still start at `leaf_start` so the limb bases
        // and the trunk keep the bare Y silhouette.
        leaf_min_level: 1,
        // One more split than before: trunk -> limbs -> secondaries -> twigs ->
        // leaf-bearing twigs. Three levels were what made the silhouette stop
        // at a stubby hand.
        max_level: 5,
        // 2-3 limbs with a tight budget: fixing it at 2 with a 0.96 span made
        // each limb long enough to lift the crown to 3x eye height.
        children: (2, 3),
        // The aspen keeps 2-3: a poplar's limbs come out of the pole almost
        // vertically, so a second scaffold barely changes the silhouette that
        // already reads as one column.
        trunk_children: (2, 3),
        child_span: (0.72, 0.85),
        // Narrow spread + strong upward bias = limbs that climb rather than
        // splay. This is the aspen's silhouette: tall and columnar. Widened
        // 2026-11-09 (14-26 / jitter 20) because the column read as too
        // uniform: a real aspen is loose and ragged, with a few limbs breaking
        // the envelope. It must stay a column, not become a ball.
        spread_deg: (16.0, 44.0),
        // A full turn: 200 degrees put every limb of a node inside a 200-degree
        // sector, so the crown leaned to one side like a crook. Branching has to
        // use the whole circle.
        whorl_deg: 360.0,
        jitter_deg: 30.0,
        up_bias: 0.62,
        // One clump per branch plus runs spaced along it: distributing the
        // same cards over every generation reads as a continuous crown instead
        // of a clump at each twig tip.
        leaf_clusters: 1,
        // ~0.43 m cards at 14 m tall. The old 0.085 (0.66 m cards) was sized
        // for opaque blobs; with a visible leaf mask on the card it reads as a
        // metre-wide leaf.
        leaf_radius: 0.055,
        leaf_radius_ratio: 0.88,
        // Runs every 0.62 m: enough for a continuous spray on the L1/L2 limbs
        // without paying for a clump every hand-width on the whole tree.
        leaf_run_spacing: 0.62,
        crown_floor: 0.18,
        leaf_density: 0.78,
        // Tight anchoring: a tip clump may drift at most 0.15 of its own radius
        // off the twig axis. The old 0.45 was authored for opaque blobs; with
        // cards it put petioles half a metre off a 1 cm twig.
        leaf_spread: 0.15,
        // Foliage starts low on the limbs: the crown base is what the player
        // actually reads from eye height. Demanding a fully exposed Y pushed
        // the canopy back above 3x eye height, which is the complaint this
        // was meant to fix.
        leaf_start: 0.5,
        // 0.05 of 14 m = every straight run stays under 0.7 m, so the longest
        // tube is about one leaf clump; the limb itself is an arc.
        max_segment: 0.05,
        limb_curve_deg: 22.0,
        // A poplar limb is already near vertical, so it has little to shed.
        limb_lift: 0.2,
        // Keeps the crown a narrow dome rather than a column of straight
        // spokes, without opening it into a ball.
        crown_rounding: 0.6,
        height: 14.0,
        width: 4.2,
    },
    // 1: street tree — rounder crown, wider spread, leaves start earlier.
    TreeProfile {
        // The trunk is a FRACTION of the height budget, so the budget has to
        // cover a crown that spreads sideways: at 11.5 the finished tree was
        // 6 m tall and the "trunk" was half of it, which is exactly the bare
        // "handle" the player read under each brush.
        trunk_length: 0.16,
        trunk_radius: 0.055,
        // 0.52 instead of 0.73: the old terminal limb was an 8.6 cm-radius log.
        // Two extra generations now taper to ~1.6 cm-radius twigs (3 cm across).
        radius_ratio: 0.52,
        // Long limbs with short side branches produce plumes; the ratio that
        // matters here is the one that keeps the crown compact while the twigs
        // fill the space between the scaffolds.
        // 0.8, up from 0.66: the per-level fan divides the height budget among
        // the children, so the finished crown only reached a third of `height`
        // (5.5 m from a 16.5 m budget). 0.8 keeps the tree's proportion of its
        // budget near half, which is what the budget test measures.
        child_len_ratio: 0.8,
        // Same as the aspen: leaves from the first fork down to the twigs, so
        // the main and secondary limbs carry sprays too. Terminal-only foliage
        // was the "cloud of separate clumps" the player saw.
        leaf_min_level: 1,
        // Four splits, one more than the "stubby hand" the old 3-level shape
        // ended in.
        max_level: 4,
        // Exactly 3 at every level below the trunk. Two children per limb left
        // the tree with ~20 twigs in total, so the crown was a handful of
        // strands: you could see straight through it no matter how the leaves
        // were laid out. 3-3 makes it ~120 limbs while still under the
        // instancing budget.
        children: (3, 3),
        // The trunk ALWAYS forks into 3-4. Rolling a 2 here is what produced
        // the pair of brushes in the Blender check: the whole crown was two
        // limbs, so there was no interior for the foliage to fill.
        trunk_children: (3, 4),
        child_span: (0.72, 0.88),
        // 50-64 off the parent, i.e. scaffolds that leave the fork almost
        // horizontally and then arch upward. Width has to come from the fork
        // angle -- `limb_lift` alone pulls every limb back to vertical and baked
        // a 2.5 m-wide poplar out of a street tree -- but a limb that starts
        // wide and finishes vertical curves over into a dome instead of running
        // out as a spoke.
        spread_deg: (50.0, 64.0),
        whorl_deg: 360.0,
        jitter_deg: 26.0,
        up_bias: 0.30,
        leaf_clusters: 1,
        // The clump is sized as `leaf_radius * height`, so the 0.06 authored
        // for a 11.5 m budget became a 0.99 m RADIUS (2 m ball) when the
        // budget was raised to 16.5 -- that is what made the crown a handful of
        // spheres. 0.038 bakes ~0.63 m of foliage: one compound leaf spray,
        // large enough that the sprays on two neighbouring limbs meet.
        leaf_radius: 0.038,
        leaf_radius_ratio: 0.9,
        // Runs are back on, and this time they are what TIES the crown
        // together: with a 0.45 m clump every 0.5 m, a 2.5 m limb carries a
        // continuous spray instead of one brush at its end. The earlier run
        // version read as a cylinder because the clumps were 2 m across and
        // the limbs were straight tubes; short curved segments fix both.
        leaf_run_spacing: 0.5,
        crown_floor: 0.18,
        leaf_density: 0.95,
        // Clumps stay on the twig: 0.12 put petioles 0.10 m off a 1 cm twig,
        // which the anchoring regression test rejects at branch-surface scale.
        leaf_spread: 0.05,
        // Foliage from a third of the way up each limb, not its outer half:
        // the bare stretch between the fork and the first clump is what read as
        // a "handle" on each brush.
        leaf_start: 0.25,
        max_segment: 0.055,
        limb_curve_deg: 22.0,
        // A light upward curl only: `close_crown` shapes the envelope, so the
        // limb no longer has to straighten itself to close the crown.
        limb_lift: 0.3,
        // A street tree's crown is read as a ball: the deep twigs fan outward
        // until the envelope is roughly as wide as it is tall.
        crown_rounding: 0.75,
        // Budget covers the trunk plus a crown that spreads sideways; the
        // finished tree lands around 8-9 m.
        height: 16.5,
        width: 5.5,
    },
    // 2: broadleaf — low fork, heavy limbs, big leaf masses.
    TreeProfile {
        // Low fork, but not half the finished tree: at 0.26 of 10.5 the bare
        // trunk was 2.7 m of a 5.3 m tree.
        trunk_length: 0.19,
        trunk_radius: 0.07,
        // The heaviest profile tapered slowest (0.80), so its terminal limb was
        // a 13 cm-radius log. 0.52 brings it to ~1.9 cm while the first two
        // limb generations stay visibly heavy.
        radius_ratio: 0.52,
        child_len_ratio: 0.82,
        // Foliage from the heavy limbs' first fork down through the twigs.
        leaf_min_level: 1,
        // One more split so the limbs end in twigs, not in stubs.
        max_level: 4,
        // 2-4 scaffolds for the same reason as the street tree: two heavy limbs
        // gave two blobs, not a crown.
        children: (2, 3),
        // Three or four heavy limbs over a short trunk: the broadleaf's whole
        // silhouette is its scaffolds, so a Y fork leaves it with two brushes.
        trunk_children: (3, 4),
        child_span: (0.72, 0.88),
        // 26-38, not 34-50: the wide fan is what opened the V between the two
        // arms. Limbs that climb keep the crown's mass together.
        spread_deg: (26.0, 38.0),
        // Full turn, same as the aspen: 210 degrees bunched the scaffolds into
        // one sector and the crown grew as a crescent.
        whorl_deg: 360.0,
        jitter_deg: 34.0,
        up_bias: 0.22,
        leaf_clusters: 1,
        // Same `leaf_radius * height` trap as the street tree: 0.055 against a
        // 13 m budget baked a 0.72 m radius ball per cluster. 0.045 (0.58 m) is
        // the foliar clump this crown needs: the gaps between two arms are about
        // that wide, so neighbouring sprays meet instead of leaving a V of sky.
        leaf_radius: 0.045,
        leaf_radius_ratio: 0.92,
        // A spray every 0.6 m along the heavy limbs, small enough that the
        // wood still shows between the clumps. Sized against the per-tree
        // triangle budget: the crown now holds three times the clumps it did
        // as two brushes, so the spacing has to pay for them.
        leaf_run_spacing: 0.6,
        crown_floor: 0.24,
        leaf_density: 0.95,
        leaf_spread: 0.12,
        leaf_start: 0.25,
        max_segment: 0.06,
        limb_curve_deg: 24.0,
        // Same reasoning as the street tree; the broadleaf starts even wider
        // (34-50 degrees), so it needs the lift more.
        limb_lift: 0.35,
        // 0.35, not 0.85: a strong radial push drove each scaffold's twigs
        // further out on its own side, so the broadleaf forked into two leafy
        // ARMS with a V of bare air between them. Letting the twigs keep their
        // natural spread leaves them pointing back across the crown's middle.
        crown_rounding: 0.35,
        height: 13.0,
        width: 8.4,
    },
];

/// Where the leaf blobs sit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Foliage {
    /// World position of the cluster centre (x, y up, z).
    pub center: [f32; 3],
    /// Blob radius (m).
    pub radius: f32,
    /// Unit vector the cluster grows ALONG: from the branch out to the leaf
    /// tips.
    ///
    /// This is what makes a leaf card read as a leaf. A card is a texture with
    /// a petiole at the bottom edge and a tip at the top, so its orientation is
    /// not free: the bottom must face the branch and the tip must fan outward.
    /// The first bake ignored this and the cards pointed in random directions,
    /// which every viewer reads as "the texture is rotated wrong".
    ///
    /// Taken from the branch direction at the point the cluster attaches, so a
    /// crown's cards fan outward along the limbs rather than all pointing the
    /// same way.
    pub outward: [f32; 3],
}

/// One straight branch segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Branch {
    /// Start / end in world units (x, y up, z); `start.y` is above ground.
    pub start: [f32; 3],
    pub end: [f32; 3],
    /// Radius at the start (m).
    pub radius: f32,
    /// Radius at the end (m). A long limb is split into several of these
    /// segments, so the taper has to run along the whole limb, not restart at
    /// every segment.
    pub end_radius: f32,
    /// Branching level (0 = trunk).
    pub level: u8,
    /// Which growth step (limb) this segment belongs to. Segments of one limb
    /// share it, so the limb can be measured as a whole.
    pub limb: u32,
}

/// A generated tree: branches first (trunk order), then foliage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeSkeleton {
    pub branches: Vec<Branch>,
    pub foliage: Vec<Foliage>,
    /// Species index into [`TREE_PROFILES`].
    pub species: u8,
    /// Total skeleton length (m), for stats.
    pub total_length: f32,
}

impl TreeSkeleton {
    /// Height of the tallest branch tip / foliage blob (m).
    pub fn crown_height(&self) -> f32 {
        self.branches
            .iter()
            .map(|b| b.end[1])
            .chain(self.foliage.iter().map(|f| f.center[1] + f.radius))
            .fold(0.0, f32::max)
    }

    /// Widest horizontal extent (m), **including the leaf blobs' radii**.
    ///
    /// Measuring branch endpoints alone under-reports a crown by up to one
    /// blob diameter per side; the gallery then spaced columns by that
    /// under-report and the widest species still touched its neighbour.
    pub fn crown_width(&self) -> f32 {
        let mut x_min = f32::INFINITY;
        let mut x_max = f32::NEG_INFINITY;
        let mut z_min = f32::INFINITY;
        let mut z_max = f32::NEG_INFINITY;
        for b in &self.branches {
            for p in [b.start, b.end] {
                x_min = x_min.min(p[0]);
                x_max = x_max.max(p[0]);
                z_min = z_min.min(p[2]);
                z_max = z_max.max(p[2]);
            }
        }
        for f in &self.foliage {
            x_min = x_min.min(f.center[0] - f.radius);
            x_max = x_max.max(f.center[0] + f.radius);
            z_min = z_min.min(f.center[2] - f.radius);
            z_max = z_max.max(f.center[2] + f.radius);
        }
        if x_min.is_finite() {
            (x_max - x_min).max(z_max - z_min)
        } else {
            0.0
        }
    }
}

/// Seed mixing for `(world seed, species)`: the same seed always grows the
/// same tree of that species, and different species do not share a stream.
fn species_rng(seed: u64, species: u8) -> StdRng {
    let mixed = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(19)
        .wrapping_add((species as u64 + 1).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    StdRng::seed_from_u64(mixed)
}

/// Drains a branch's length (`span`) into `children` children and returns the
/// per-child length. With `total = length * ratio`, D1 does not up-scale: a
/// branch never grows longer than its parent's `ratio` allows, so a wide
/// `children` count produces short twigs instead of a monster.
fn split_lengths(rng: &mut StdRng, total: f32, children: u8) -> Vec<f32> {
    if children == 0 {
        return Vec::new();
    }
    // 0.8..1.2, not 0.6..1.4: the wide span let one child be 2.3x its sibling,
    // and once one scaffold outgrew the others the crown's CENTROID moved off
    // the trunk (measured 1.7 m to the side) -- a real tree's scaffolds divide
    // their budget far more evenly than that, and an off-centre crown is exactly
    // the "two brushes on one side" look.
    let mut weights: Vec<f32> = (0..children).map(|_| rng.random_range(0.8..=1.2)).collect();
    let sum: f32 = weights.iter().sum();
    for w in &mut weights {
        *w = total * *w / sum;
    }
    weights
}

/// Unit vector helpers on plain arrays (kept dependency-free so the module
/// stays pure data).
fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1e-6 {
        [0.0, 1.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn magnitude(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    magnitude(sub(b, a))
}

fn scale(v: [f32; 3], s: f32) -> [f32; 3] {
    [v[0] * s, v[1] * s, v[2] * s]
}

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Unit vector `angle` radians away from `axis`, rotated `roll` radians
/// around it. Building children this way (rather than sampling a random
/// direction) is what makes the crown read as a *branching* structure instead
/// of a starburst.
fn tilt(axis: [f32; 3], angle: f32, roll: f32) -> [f32; 3] {
    let axis = normalize(axis);
    // Any vector not parallel to the axis works as the roll reference.
    let reference = if axis[1].abs() < 0.99 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let u = normalize(cross(axis, reference));
    let w = cross(axis, u);
    let (sin_a, cos_a) = angle.sin_cos();
    let (sin_r, cos_r) = roll.sin_cos();
    normalize([
        axis[0] * cos_a + (u[0] * cos_r + w[0] * sin_r) * sin_a,
        axis[1] * cos_a + (u[1] * cos_r + w[1] * sin_r) * sin_a,
        axis[2] * cos_a + (u[2] * cos_r + w[2] * sin_r) * sin_a,
    ])
}

/// A branch waiting to be grown (breadth-first; keeps output order stable).
struct Pending {
    start: [f32; 3],
    dir: [f32; 3],
    length: f32,
    radius: f32,
    level: u8,
}

/// Splits one pending limb into short segments along a shallow arc.
///
/// One long straight tube is what the player sees as either a bare spoke or a
/// cylinder of leaves: no foliage on the long middle, and whatever does attach
/// wraps the tube. Segmenting keeps every straight run under `max_segment` and
/// bends the limb, so the leaf clusters sit on a curve the eye reads as a
/// branch.
fn build_chain(
    rng: &mut StdRng,
    pending: &Pending,
    end_radius: f32,
    limb: u32,
    profile: &TreeProfile,
) -> Vec<Branch> {
    let max_segment = (profile.max_segment * profile.height).max(0.3);
    let count = ((pending.length / max_segment).ceil() as usize).clamp(1, 8);
    let step = pending.length / count as f32;
    // One arc per limb, in a random plane: no two limbs curve alike. The TRUNK
    // is exempt: a 22-degree lean over the first 2.5 m moved the whole crown
    // 1.5 m off the trunk's axis (measured), because every scaffold inherits the
    // leaning tangent and leans further the same way. A trunk is a plumb line.
    let bend = if pending.level == 0 {
        0.0
    } else {
        profile.limb_curve_deg.to_radians() * rng.random_range(-1.0..1.0)
    };
    let roll = rng.random_range(0.0..std::f32::consts::TAU);
    let mut segments = Vec::with_capacity(count);
    let mut cursor = pending.start;
    for index in 0..count {
        // Turn grows along the limb, so it leaves the fork pointing the way a
        // limb should and bends over its own length.
        let mut dir = tilt(
            pending.dir,
            bend * (index as f32 + 1.0) / count as f32,
            roll,
        );
        // Gravitropism: the far end of the limb sheds part of its outward
        // tilt, so limbs curve up into a dome instead of running straight out.
        //
        // Only the scaffolds (level 0-1) get the full effect. Applying it to
        // every twig straightened the deep generations too, which collapsed the
        // crown to 3 m wide: the twigs are exactly the foliage that has to fan
        // OUT to fill the envelope.
        let t = (index as f32 + 1.0) / count as f32;
        let lift = profile.limb_lift * if pending.level <= 1 { 1.0 } else { 0.3 };
        if lift > 0.0 {
            dir = normalize(lerp(dir, [0.0, 1.0, 0.0], lift * t));
        }
        let end = add(cursor, scale(dir, step));
        let from = index as f32 / count as f32;
        let to = (index as f32 + 1.0) / count as f32;
        segments.push(Branch {
            start: cursor,
            end,
            radius: pending.radius + (end_radius - pending.radius) * from,
            end_radius: pending.radius + (end_radius - pending.radius) * to,
            level: pending.level,
            limb,
        });
        cursor = end;
    }
    segments
}

/// Point and forward direction at fraction `t` along a segment chain.
fn along_chain(chain: &[Branch], t: f32) -> ([f32; 3], [f32; 3]) {
    let total: f32 = chain.iter().map(|b| distance(b.start, b.end)).sum();
    let mut target = total * t.clamp(0.0, 1.0);
    for (index, branch) in chain.iter().enumerate() {
        let len = distance(branch.start, branch.end);
        if target <= len || index + 1 == chain.len() {
            let local = if len > 1e-6 {
                (target / len).clamp(0.0, 1.0)
            } else {
                0.0
            };
            return (
                lerp(branch.start, branch.end, local),
                normalize(sub(branch.end, branch.start)),
            );
        }
        target -= len;
    }
    (chain[0].start, normalize(sub(chain[0].end, chain[0].start)))
}

/// Position of `y` inside the crown's height range (0 at the crown base, 1 at
/// the top of the height budget), clamped.
fn height_fraction(y: f32, base: f32, span: f32) -> f32 {
    ((y - base) / span).clamp(0.0, 1.0)
}

/// Grows one tree of `species` from `seed`. `id` only salts the stream so
/// two nearby trees of the same species look different.
pub fn grow_tree(seed: u64, species: u8, id: u64) -> TreeSkeleton {
    let profile = TREE_PROFILES
        .get(species as usize)
        .copied()
        .unwrap_or(TREE_PROFILES[0]);
    grow_tree_with(seed, species, id, &profile)
}

/// [`grow_tree`] with an explicit profile (the preview uses this to compare
/// parameter sets side by side).
///
/// Grows TWICE. `height` is a growth budget, and the per-level fan divides it
/// among the children, so the finished tree only reaches ~45% of it. Shaping
/// the crown envelope with that budget put every branch below 0.4 of the
/// height fraction, which permanently switched off the "twigs turn inward near
/// the top" term — the crowns then kept a notch between their scaffolds. The
/// first pass measures how tall the tree actually grows; the second shapes it
/// against that height. Both passes draw from the same seeds, so the result
/// stays deterministic.
pub fn grow_tree_with(seed: u64, species: u8, id: u64, profile: &TreeProfile) -> TreeSkeleton {
    let base = profile.trunk_length * profile.height;
    let first = grow_pass(seed, species, id, profile, (profile.height - base).max(0.1));
    let realized = (first.crown_height() - base).max(0.5);
    // 0.9 rather than 1.0: the top decile of the crown counts as "near the
    // top", so the closure starts before the last twig and the silhouette
    // rounds over instead of ending in two prongs.
    grow_pass(seed, species, id, profile, realized * 0.9)
}

/// One growth pass, with the crown's height range given explicitly.
fn grow_pass(
    seed: u64,
    species: u8,
    id: u64,
    profile: &TreeProfile,
    crown_span: f32,
) -> TreeSkeleton {
    let mut rng = species_rng(seed ^ id.wrapping_mul(0x2545_F491_4F6C_DD1D), species);
    // Foliage draws from its own stream. Cluster jitter consumes draws while
    // the queue is still being walked, so one shared stream meant retuning a
    // foliage knob (`leaf_run_spacing`) reshaped every branch of the tree.
    // The skeleton must stay put when only the leaves are retuned.
    let mut leaf_rng = species_rng(
        seed ^ id.wrapping_mul(0x2545_F491_4F6C_DD1D),
        species.wrapping_add(0x80),
    );

    let mut skeleton = TreeSkeleton {
        species,
        ..Default::default()
    };
    let mut queue: std::collections::VecDeque<Pending> = std::collections::VecDeque::new();
    // Identifier for one growth step, shared by all the segments it splits
    // into: a limb has to be measurable as a whole even after segmentation.
    let mut limb = 0u32;

    // Normalized units everywhere, scaled at the end: `height` is the trunk
    // length of a full-height tree, so a "1.0 tall" profile is `height` m.
    let root_length = profile.trunk_length * profile.height;
    let root_dir = normalize([
        rng.random_range(-0.03..0.03),
        1.0,
        rng.random_range(-0.03..0.03),
    ]);
    queue.push_back(Pending {
        start: [0.0, 0.0, 0.0],
        dir: root_dir,
        length: root_length,
        radius: profile.trunk_radius * profile.width.max(profile.height) * 0.35,
        level: 0,
    });

    while let Some(branch) = queue.pop_front() {
        let end_radius = branch.radius * profile.radius_ratio;
        let chain = build_chain(&mut rng, &branch, end_radius, limb, profile);
        limb += 1;
        skeleton.total_length += branch.length;
        skeleton.branches.extend_from_slice(&chain);

        // Foliage: clusters at the limb tip plus an optional run along it.
        //
        // Clusters are placed ON the limb (a short step back from the tip)
        // rather than orbiting it. Orbiting put the outermost blobs clear of
        // the crown, and once `leaf_density` thinned the field they hung in
        // the air as debris instead of reading as leaf mass.
        if branch.level >= profile.leaf_min_level {
            let level_above = (branch.level - profile.leaf_min_level) as f32;
            let radius =
                profile.leaf_radius * profile.leaf_radius_ratio.powf(level_above) * profile.height;
            // Run clusters cover the leafy part of the limb only; the stretch
            // from the fork to `leaf_start` stays bare so the limb's base and
            // the trunk keep their silhouette.
            let start = profile.leaf_start.clamp(0.0, 0.95);
            let clusters = profile.leaf_clusters.max(1);
            for index in 0..clusters {
                // A few clusters along the last stretch of the limb, so the
                // tip is a clump rather than one ball balanced on the end.
                let t = if clusters <= 1 {
                    (start + 1.0) * 0.5
                } else {
                    start + (1.0 - start) * index as f32 / (clusters - 1) as f32
                };
                let (center, dir) = along_chain(&chain, t);
                let drift = radius * profile.leaf_spread;
                let offset = [
                    leaf_rng.random_range(-drift..=drift),
                    leaf_rng.random_range(-drift..=drift),
                    leaf_rng.random_range(-drift..=drift),
                ];
                skeleton.foliage.push(Foliage {
                    center: add(center, offset),
                    radius: radius * leaf_rng.random_range(0.75..=1.15),
                    // Leaves at a limb tip continue along the limb, which is
                    // where a tip cluster actually points.
                    outward: dir,
                });
            }
            let runs = profile.run_clusters(branch.length);
            for run in 1..=runs {
                let t = start + (1.0 - start) * run as f32 / (runs as f32 + 1.0);
                let (center, dir) = along_chain(&chain, t);
                let side = tilt(
                    dir,
                    std::f32::consts::FRAC_PI_2,
                    leaf_rng.random_range(0.0..std::f32::consts::TAU),
                );
                skeleton.foliage.push(Foliage {
                    // Side offset capped below the blob radius: attached, not
                    // floating.
                    center: add(center, scale(side, radius * profile.leaf_spread)),
                    radius: radius * leaf_rng.random_range(0.8..=1.2),
                    // A run cluster hangs off the SIDE of the limb, so its
                    // petiole points back at the branch and its tip points away
                    // along that side vector.
                    outward: side,
                });
            }
        }

        if branch.level >= profile.max_level {
            continue;
        }

        // The trunk rolls its own scaffold count: a street tree must never end
        // up as a two-limbed Y, while every deeper level stays low enough that
        // the branch count does not blow the instancing budget.
        let (min_children, max_children) = if branch.level == 0 {
            profile.trunk_children
        } else {
            profile.children
        };
        let count = rng.random_range(min_children..=max_children);
        if count == 0 {
            continue;
        }
        // Budget comes from the TREE's scale, not the parent's: a parent only
        // 30 cm long would otherwise hand out 20 cm twigs and the crown would
        // collapse into a stick (measured: 4.7 m "8 m tree", 0.4 m wide).
        // `child_scale^level * height`, split among the children, gives every
        // level its own share of the same `height` budget.
        let level = branch.level + 1;
        // Budget comes from the TREE's scale, not the parent's: a parent only
        // 30 cm long would otherwise hand out 20 cm twigs and the crown would
        // collapse into a stick (measured: 4.7 m "8 m tree", 0.4 m wide).
        let level_share = profile.child_len_ratio.powi(level as i32) * profile.height;
        let jitter = rng.random_range(profile.child_span.0..=profile.child_span.1);
        let span = jitter * level_share;

        // **Total-budget check.** `level_share` is a per-LEVEL quantity, but a
        // level holds far more branches than the one above it (street tree:
        // 8 at L2 vs 25 at L3). Splitting a per-level budget among more
        // children produced longer mean limbs at deeper levels (measured
        // 0.96 m at L2 vs 1.02 m at L3). A real tree conserves length: every
        // generation multiplies the count and shortens each member, so the
        // per-branch length must be divided by the expected growth in count.
        // Only the *growth in branch count* is compensated, and only
        // partially (square root): compensating the full fan-out on top of
        // `child_len_ratio` decays far too fast — twigs came out 3-9 cm and
        // the crown vanished.
        let fan_out = ((profile.children.0 as f32 + profile.children.1 as f32) * 0.5).max(1.0);
        let count_growth = fan_out.powf((level.saturating_sub(1)) as f32 * 0.5);
        let span = span / count_growth.max(1.0);
        let lengths = split_lengths(&mut rng, span, count);
        let child_radius = branch.radius * profile.radius_ratio;
        let mut roll = rng.random_range(0.0..std::f32::consts::TAU);
        let step = profile.whorl_deg.to_radians() / count as f32;
        // Crown envelope: the trunk's children climb, deeper twigs leave the
        // crown's core outward. Without this every generation is pulled the
        // same way and the crown is a column.
        //
        // The outward direction is horizontal-radial with a height-dependent
        // upward tilt, NOT the normal of a sphere around a fixed centre: a
        // centre computed from the height BUDGET sits above the finished crown,
        // which points every limb downward and flattens the tree.
        let crown_base = profile.trunk_length * profile.height;
        // Constant, not `level / max_level`: the envelope is a property of the
        // crown's SHAPE, and the shape is set at every height. Scaling it by
        // level left the scaffold limbs (the ones that decide the crown's
        // width) with 0.19 of the push, so they stayed on three diverging
        // diagonals while only the short top twigs were ever turned.
        let rounding = profile.crown_rounding.clamp(0.0, 1.0);
        for (index, length) in lengths.into_iter().enumerate() {
            // The trunk's first child is a LEADER: it continues the trunk
            // nearly vertically instead of fanning out with the scaffolds.
            // Without one the fork is a pure V and the space directly above
            // the trunk stays empty no matter how the limbs are steered --
            // that void is the gap in the middle of the "two brushes".
            let leader = branch.level == 0 && index == 0;
            let spread = rng
                .random_range(profile.spread_deg.0..=profile.spread_deg.1)
                .to_radians();
            let spread = if leader { spread * 0.15 } else { spread };
            let jitter = rng
                .random_range(-profile.jitter_deg..=profile.jitter_deg)
                .to_radians();
            // Children leave from partway along the parent, not all from its
            // tip: that single detail removes the "scarecrow" look. On a
            // segmented limb the fork follows the limb's own curve.
            //
            // Scaffold limbs fork high on the trunk, but every generation below
            // them sprouts along ALMOST THE WHOLE parent. Forking only on the
            // outer third (`0.72..1.0` at every level) is what produced "two
            // long bare limbs ending in brushes": the inner 70% of each limb had
            // no side branches at all, so the crown could never be one mass.
            // Scaffolds leave from a RANGE of heights along the trunk, not all
            // from one node. A real street tree staggers its scaffolds over the
            // upper trunk, and staggered starts layer the limbs vertically: the
            // notch between two limbs that fork at the same height gets filled
            // by the limb that starts higher up.
            let fork = if branch.level == 0 {
                rng.random_range(0.45..=1.0)
            } else {
                rng.random_range(0.3..=1.0)
            };
            let (start, dir) = along_chain(&chain, fork);
            let mut dir = tilt(dir, spread, roll + jitter);
            if rounding > 0.0 {
                // Horizontal direction away from the trunk axis. Any horizontal
                // direction will do near the axis -- and "near" matters: the
                // first fork sits 5 cm off the axis, which normalised to a
                // 5 cm-long radial and pulled EVERY scaffold into the same
                // sector, so all the limbs (and therefore the whole crown)
                // leaned one way. Compare against the crown's own radius
                // instead of an epsilon.
                let mut radial = [start[0], 0.0, start[2]];
                if magnitude(radial) < (profile.width * 0.5 * 0.25).max(0.05) {
                    radial = tilt([0.0, 1.0, 0.0], 1.0, roll);
                }
                let radial = normalize(radial);
                // Children lean away from the trunk axis and climb: the crown is
                // grown as a wide fan, and `close_crown` trims whatever escapes
                // the ball afterwards. Steering alone cannot guarantee a solid
                // silhouette (N scaffolds always leave N paths), so the fan is
                // deliberately generous here.
                let frac = height_fraction(start[1], crown_base, crown_span);
                // Elevation of the fan: ~48 degrees at the crown base, climbing
                // to ~65 near the top. The old 0.5 (27 degrees) grew almost
                // horizontal limbs, so the crown came out as a flat mushroom cap
                // on a bare pole -- wide, thin, and not a ball.
                let up = 1.1 + frac * 1.4;
                let outward = normalize([radial[0], up, radial[2]]);
                // Fade the push out as the branch approaches the crown's
                // radius. A constant outward push is a runaway: a limb that is
                // already off-centre is pushed further out on every generation,
                // and the crown's centroid drifts off the trunk by over a metre
                // (measured 1.7 m) until it reads as a crescent on one side.
                let r = (start[0] * start[0] + start[2] * start[2]).sqrt();
                let inside = (1.0 - r / (profile.width * 0.5).max(0.2)).clamp(0.0, 1.0);
                let push = if leader { 0.0 } else { rounding * inside };
                dir = normalize(lerp(dir, outward, push));
            }
            // `up_bias` fades with the same depth so the two do not fight.
            dir = normalize(lerp(
                dir,
                [0.0, 1.0, 0.0],
                profile.up_bias * (1.0 - rounding),
            ));
            queue.push_back(Pending {
                start,
                dir,
                length,
                radius: child_radius,
                level,
            });
            roll += step;
        }
    }

    prune_crown(&mut skeleton, profile);
    skeleton
}

/// Two-pass crown shaping.
///
/// Pass 1 drops every cluster below `crown_floor` of the crown's own height
/// range. A dense blob field naturally ends at a flat line where the tips
/// stop; a floor cut at the real per-tree crown base removes the "skirt"
/// without guessing absolute heights.
///
/// Pass 2 thins the survivors by `leaf_density`. This is the knob that decides
/// whether the silhouette is a solid mass or a lacy one: at 1.0 every branch
/// tip carries foliage, below that some tips stay bare and read as gaps.
fn prune_crown(skeleton: &mut TreeSkeleton, profile: &TreeProfile) {
    if skeleton.foliage.is_empty() {
        return;
    }
    let (mut low, mut high) = (f32::INFINITY, f32::NEG_INFINITY);
    for f in &skeleton.foliage {
        low = low.min(f.center[1]);
        high = high.max(f.center[1]);
    }
    let span = (high - low).max(1e-3);
    let floor = low + span * profile.crown_floor;
    let keep = profile.leaf_density.clamp(0.0, 1.0);

    // Deterministic thinning keyed on the cluster position: the same skeleton
    // always thins identically, and neighbouring clusters thin independently
    // (no visible rows or patches).
    //
    // Clusters that sit in a dense neighbourhood win a density bonus. That has
    // two effects worth the extra pass: isolated blobs are the ones that read
    // as floating debris, and clusters packed against a limb keep the crown
    // from turning into a hollow shell with a bare trunk sticking out of it.
    let mut kept = Vec::with_capacity(skeleton.foliage.len());
    for f in &skeleton.foliage {
        if f.center[1] < floor {
            continue;
        }
        let neighbours = skeleton
            .foliage
            .iter()
            .filter(|g| {
                let d = [
                    g.center[0] - f.center[0],
                    g.center[1] - f.center[1],
                    g.center[2] - f.center[2],
                ];
                // Neighbourhood = one blob radius: at two radii every blob
                // in a crown qualifies, support saturates and the bonus
                // cancels out.
                let r = f.radius;
                d[0] * d[0] + d[1] * d[1] + d[2] * d[2] < r * r
            })
            .count();
        // 1 neighbour (itself) = isolated: thin hardest. 5+ = packed: keep.
        let support = (neighbours.saturating_sub(1) as f32 / 4.0).clamp(0.0, 1.0);
        let threshold = keep * (0.55 + 0.45 * support);
        let h = hash3(f.center);
        if (h & 0xFFFF) as f32 / 65535.0 > threshold {
            continue;
        }
        kept.push(*f);
    }
    skeleton.foliage = kept;
}

/// Position hash in `[0, 1)`: a cheap deterministic selector for thinning.
fn hash3(p: [f32; 3]) -> u64 {
    let (x, y, z) = (
        p[0].to_bits() as u64,
        p[1].to_bits() as u64,
        p[2].to_bits() as u64,
    );
    let mut h = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= y.wrapping_mul(0xC2B2_AE3D_27D4_EB4F).rotate_left(23);
    h ^= z.wrapping_mul(0x1656_67B1_9E37_79F9).rotate_left(47);
    h ^= h >> 31;
    h.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_grows_the_same_tree() {
        let a = grow_tree(0x4845_4144, 1, 7);
        let b = grow_tree(0x4845_4144, 1, 7);
        assert_eq!(a, b, "skeleton must be bit-identical for the same inputs");
    }

    #[test]
    fn different_ids_and_species_differ() {
        let a = grow_tree(0x4845_4144, 1, 0);
        let b = grow_tree(0x4845_4144, 1, 1);
        assert_ne!(a.branches.len(), 0);
        assert_ne!(a, b, "two trees of one species must not be clones");
        let c = grow_tree(0x4845_4144, 2, 0);
        assert_ne!(a, c, "species must change the shape");
    }

    #[test]
    fn limbs_are_split_into_short_straight_runs() {
        // Regression (2026-11-09): a limb baked as one long straight tube
        // either carried no foliage on its middle or wrapped the cards around
        // it like a cylinder. No straight run may be longer than a leaf clump.
        for (species, profile) in TREE_PROFILES.iter().enumerate() {
            let cap = (profile.max_segment * profile.height).max(0.3);
            for variant in 0..4 {
                let tree = grow_tree(0x4845_4144, species as u8, variant);
                for branch in &tree.branches {
                    let len = distance(branch.start, branch.end);
                    assert!(
                        len <= cap + 1e-3,
                        "species {species} variant {variant}: a straight run is {len:.2} m, \
                         over the {cap:.2} m cap"
                    );
                }
                let trunk = tree.branches.iter().filter(|b| b.level == 0).count();
                assert!(
                    trunk > 1,
                    "species {species} variant {variant}: the trunk was never split"
                );
            }
        }
    }

    #[test]
    fn round_crowned_species_close_into_a_ball() {
        // Species 1 (street) and 2 (broadleaf) are the round-crowned trees.
        // The aspen stays a column on purpose: it is a poplar.
        for species in [1u8, 2] {
            for variant in 0..4 {
                let tree = grow_tree(0x4845_4144, species, variant);
                let ratio = tree.crown_width() / tree.crown_height();
                assert!(
                    ratio > 0.6,
                    "species {species} variant {variant}: crown is a column \
                     ({ratio:.2} wide per metre tall)"
                );
            }
        }
    }

    #[test]
    fn every_profile_stays_within_its_budget() {
        for (species, profile) in TREE_PROFILES.iter().enumerate() {
            let tree = grow_tree(0x4845_4144, species as u8, 3);
            let h = tree.crown_height();
            // `height` is the GROWTH BUDGET, not the finished crown: the
            // per-level fan divides the budget among the children, and
            // up_bias/spread pull the tips off vertical, so the realized crown
            // lands at 0.40-0.55 of the budget (measured per species). The band
            // is therefore "a tree actually grew" and "it did not overshoot",
            // not "it is exactly `height` tall".
            assert!(
                h > profile.height * 0.35 && h < profile.height * 1.2,
                "species {species}: crown height {h:.2} m outside the budget band                  for a {:.1} m budget",
                profile.height
            );
            assert!(
                h > 4.0,
                "species {species}: crown only {h:.2} m tall, too short to read as a tree"
            );
            // Width: a slender tree must stay slender.
            let w = tree.crown_width();
            assert!(
                w <= profile.width * 1.6,
                "species {species}: crown width {w:.2} m exceeds {:.1} m",
                profile.width * 1.6
            );
            // Foliage exists and is not a single ball.
            assert!(
                tree.foliage.len() >= 12,
                "species {species}: too little foliage"
            );
            // Trunk sits on the ground, nothing goes below it.
            assert_eq!(tree.branches[0].start[1], 0.0);
            assert!(
                tree.branches.iter().all(|b| b.end[1] >= -0.05),
                "species {species}: a branch grew below ground"
            );
            // Branch count stays in an instancing-friendly range.
            assert!(
                tree.branches.len() < 400,
                "species {species}: {} branches is too many to instance",
                tree.branches.len()
            );
        }
    }

    #[test]
    fn crown_width_includes_leaf_radius() {
        // Regression: measuring branch endpoints alone under-reported a crown
        // by up to a blob diameter per side, and the gallery then spaced its
        // columns by that number — the widest species still touched its
        // neighbour.
        for (species, _) in TREE_PROFILES.iter().enumerate() {
            let tree = grow_tree(0x4845_4144, species as u8, 0);
            let branch_only = tree
                .branches
                .iter()
                .flat_map(|b| [b.start, b.end])
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), p| {
                    (lo.min(p[0]), hi.max(p[0]))
                });
            let blob_right = tree
                .foliage
                .iter()
                .map(|f| f.center[0] + f.radius)
                .fold(f32::NEG_INFINITY, f32::max);
            assert!(
                blob_right > branch_only.1,
                "species {species}: leaf blobs extend past the outermost branch                  (blob {blob_right:.2} vs branch {:.2})",
                branch_only.1
            );
            assert!(tree.crown_width() >= blob_right - branch_only.0);
        }
    }

    #[test]
    fn thinning_keeps_crowns_dense_and_grounded() {
        // `crown_floor` must actually remove the lowest clusters, and
        // `leaf_density` must not empty a crown.
        for (species, profile) in TREE_PROFILES.iter().enumerate() {
            let tree = grow_tree(0x4845_4144, species as u8, 2);
            assert!(
                !tree.foliage.is_empty(),
                "species {species}: thinning emptied the crown"
            );
            // Every surviving cluster clears the crown base.
            let low = tree
                .foliage
                .iter()
                .map(|f| f.center[1])
                .fold(f32::INFINITY, f32::min);
            let high = tree
                .foliage
                .iter()
                .map(|f| f.center[1])
                .fold(f32::NEG_INFINITY, f32::max);
            assert!(
                high - low > 1.0,
                "species {species}: crown is a single layer, not a volume"
            );
            assert!(profile.leaf_density > 0.0 && profile.leaf_density <= 1.0);
        }
    }

    #[test]
    fn branches_taper_and_levels_shorten() {
        for (species, profile) in TREE_PROFILES.iter().enumerate() {
            let tree = grow_tree(0x4845_4144, species as u8, 5);
            let trunk = &tree.branches[0];
            assert_eq!(trunk.level, 0);
            let level_one: Vec<&Branch> = tree.branches.iter().filter(|b| b.level == 1).collect();
            assert!(
                !level_one.is_empty(),
                "species {species}: no first-order limbs"
            );

            let len = |b: &Branch| {
                let d = [
                    b.end[0] - b.start[0],
                    b.end[1] - b.start[1],
                    b.end[2] - b.start[2],
                ];
                (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
            };

            for b in &level_one {
                assert!(
                    b.radius < trunk.radius,
                    "species {species}: radius must taper"
                );
            }

            // A single limb may be longer than a SHORT trunk: level lengths
            // come from the tree's height budget, not from the parent's
            // length (that was the "crown collapses to a stick" bug). What
            // must hold is that the budget shrinks per level, so the mean LIMB
            // length is monotonically decreasing.
            //
            // Mean over limbs, not over segments: every limb is split into
            // pieces of at most `max_segment`, so once two levels are both
            // hitting that cap their per-SEGMENT means are equal by
            // construction, which is not what this invariant is about.
            let mean_len_at = |level: u8| -> f32 {
                let mut per_limb: std::collections::BTreeMap<u32, f32> =
                    std::collections::BTreeMap::new();
                for b in tree.branches.iter().filter(|b| b.level == level) {
                    *per_limb.entry(b.limb).or_default() += len(b);
                }
                let total: f32 = per_limb.values().sum();
                total / per_limb.len().max(1) as f32
            };
            for level in 1..profile.max_level {
                let this = mean_len_at(level);
                let next = mean_len_at(level + 1);
                assert!(
                    next < this,
                    "species {species}: level {} limbs ({next:.2} m) are not shorter                      than level {level} ({this:.2} m)",
                    level + 1
                );
            }
            assert!(mean_len_at(1) < profile.height * 0.6);
        }
    }
}
