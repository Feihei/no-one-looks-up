//! Baking a [`TreeSkeleton`] into the two meshes a tree needs, plus the
//! per-vertex season data the foliage shader reads (R3.5, R3.10).
//!
//! This module merges a tree into **one pair of meshes per species-and-variant
//! key**, so a chunk spawns two `Mesh3d` entities per tree and every chunk
//! sharing a key reuses the same handles (and therefore the same draw state).
//!
//! # Two meshes per tree: wood and leaves
//!
//! [`TreeParts::wood`] holds the branches (opaque silhouette material) and
//! [`TreeParts::leaves`] the foliage cards (alpha-tested leaf texture). They
//! were one mesh while the foliage was opaque icosphere blobs. Cards need
//! `AlphaMode::Mask`, and a merged mesh would force that alpha mode onto the
//! branches too, where a texture-less alpha test erases the trunk.
//!
//! # Leaves are cards, not blobs
//!
//! A 1.2 m sphere is a solid ball at any distance, which is what "a clump of
//! leaves" looked like in game. The silhouette the design wants — sky cut into
//! pieces by real leaf outlines — cannot come from a sphere at any subdivision.
//! Each foliage cluster instead becomes a handful of crossed quads textured
//! with a baked leaf alpha mask ([`crate::world::leaf_shape`]).
//!
//! A card costs 2 triangles against a blob's 20, so the switch also cuts the
//! foliage triangle budget by roughly 90% while making the outline correct.
//!
//! # Season data on the vertex stream
//!
//! | Channel | Meaning |
//! |---|---|
//! | `COLOR.r` | sway weight (ADR-0004: "the vertex-colour R channel controls each vertex's sway strength") |
//! | `COLOR.g` | foliage mask, 0 = branch |
//! | `COLOR.b` | per-card sway phase, so neighbouring cards do not move in lockstep |
//! | `UV_0` | leaf texture coordinates (leaves only) |
//!
//! Leaf density no longer collapses geometry: that was the opaque-blob era,
//! when there was no alpha to work with. It is the material's base-colour alpha
//! multiplied into the texture alpha, which the depth prepass honours (see
//! `plugins::season`). `UV_1`, which used to carry the collapse offset, is gone
//! with it.
//!
//! Silhouette material (ADR-0001): near-black, fully rough, no specular, so the
//! tree reads as a shape against the dusk sky rather than as shaded geometry.
//! The foliage tint is a *multiplier* on that base colour, not a replacement,
//! so all four seasons stay inside the silhouette budget.

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::VisibilityRange;
use bevy::mesh::{ConicalFrustumMeshBuilder, Indices, Mesh, MeshBuilder, PrimitiveTopology};
use bevy::prelude::*;

use crate::world::tree_shape::{TREE_PROFILES, TreeProfile, TreeSkeleton, grow_tree};

/// Which leaf species' atlas cell each growth profile's cards sample.
///
/// The growth profiles ([`TREE_PROFILES`]) and the leaf table
/// ([`crate::world::leaf_shape`]) are two independent tables; this row aligns
/// them 1:1 (the R3.10 leftover): 0 aspen (slender, upright) samples poplar's
/// deltoid blade (杨), 1 street tree (broad crown) samples sophora's pinnate
/// compound leaf (槐), 2 broadleaf samples willow's lanceolate sliver (柳).
/// The drooping willow habit is a future growth-model change; the blade alone
/// still reads as 柳.
const SPECIES_LEAF: [&str; 3] = ["poplar", "sophora", "willow"];

/// Per-species foliage-card tuning, resolved from the leaf species.
pub struct FoliageBake {
    /// The atlas (u0, u1) this species' cards sample.
    pub cell: (f32, f32),
    /// Card aspect ratio (width / height) of this species' baked leaf mask.
    ///
    /// Drives the card quad's own proportions so the leaf texture is not
    /// stretched into a fixed-shape box. A pinnate or lanceolate leaf is far
    /// narrower than the broadleaf card the geometry used to assume, so without
    /// this every species baked into the same ~0.87 (w/h) box and the compound
    /// leaves collapsed into a solid blob. `leaf_cards` reads it to size the
    /// quad's short axis; see `LeafProfile::mask_aspect`.
    pub aspect: f32,
    /// Card-count multiplier. A pinnate crown reads finer with MORE, SMALLER
    /// cards (each compound leaf is one small unit, so the sky is cut into
    /// many more pieces); the simple blades keep the base count.
    pub cards_scale: f32,
    /// Card-reach multiplier, paired with the card count: more cards means
    /// smaller ones, so the cluster's total coverage stays put.
    pub reach_scale: f32,
}

fn species_foliage(species: u8) -> FoliageBake {
    let species = SPECIES_LEAF
        .get(species as usize)
        .copied()
        .unwrap_or(SPECIES_LEAF[0]);
    let cell =
        crate::world::leaf_shape::species_cell(species, crate::world::leaf_shape::ATLAS_HEIGHT);
    // The card quad's proportions follow the baked leaf mask, not a fixed box,
    // so a pinnate or lanceolate leaf keeps its own shape instead of being
    // smeared into the old 0.87 broadleaf card.
    let aspect = crate::world::leaf_shape::find_profile(species)
        .map(|p| p.mask_aspect())
        .unwrap_or(0.7);
    match species {
        // 槐: many small compound leaves, the finely fragmented silhouette.
        // The street profile grows fewer, larger clusters than the aspen, so
        // the card count rises well above the simple blades to keep the locust
        // the finest silhouette of the three. The reach stays close to the base
        // size: shrinking it with the card size left the crown visibly thinner
        // than the aspen's, so the extra cards buy density instead of only
        // fragmentation.
        "sophora" => FoliageBake {
            cell,
            aspect,
            // Card COUNT has to track the clump's AREA, not its count of them:
            // a compound leaf is a narrow card (4 cm of blade), so 22 of them
            // cover ~0.2 m2 while a 0.5 m-radius clump presents ~0.7 m2. 1.3x
            // keeps a run of small clumps reading as continuous foliage while
            // the whole species set stays inside the per-tree triangle budget.
            cards_scale: 1.3,
            // Just under 1.0: enough extra fragmentation without shrinking the
            // cards so far that the crown loses coverage between sprays.
            reach_scale: 0.95,
        },
        // Simple blades (杨, 柳): the base count over-covers an already dense
        // cluster once the crown carries a spray on every twig, and the species
        // set has a hard triangle budget (`the_pinnate_crown_takes_more_
        // smaller_cards`). 0.8 x 22 = 18 cards still reads as one mass because
        // the sprays overlap; the street tree keeps the finer 29.
        _ => FoliageBake {
            cell,
            aspect,
            cards_scale: 0.8,
            reach_scale: 1.0,
        },
    }
}

/// Branches are tapered 5-sided tubes. At silhouette distance a 5-gon is
/// indistinguishable from a 6-gon, and branches are ~40% of the triangle budget,
/// so the odd side is worth having.
const BRANCH_SIDES: u32 = 5;
/// Cards per foliage cluster near the camera.
///
/// The first version used four cards the SIZE OF THE CLUSTER, which is a 2.4 m
/// plane carrying a 10 cm leaf: at that scale the outline is a giant rectangle
/// no texture can hide. The card count and the card size have to move together
/// -- many small cards, not a few huge ones -- which is also what makes the sky
/// get cut into pieces rather than occluded in slabs.
///
/// 22 cards of ~0.26 of the cluster radius each: 44 triangles against a
/// 20-face blob, still cheaper than the sphere it replaced, and small enough
/// that the gaps between cards are leaves-sized rather than cluster-sized.
const CARDS_PER_CLUSTER: usize = 22;
/// Cards per cluster at the far tier, where individual leaves are sub-pixel and
/// only the broken-up outline matters.
const FAR_CARDS_PER_CLUSTER: usize = 6;
/// A card's half-extent as a fraction of the cluster radius.
///
/// Small enough that several cards overlap to form one cluster, large enough
/// that a leaf unit is still legible at a few metres. Raised from 0.26 when the
/// cards started growing out of the branch: a shell of cards filled the cluster
/// with their CENTRES, so each card only had to be small; radial cards only fill
/// it along their own length.
const CARD_REACH_FRACTION: f32 = 0.30;
/// How many turns the card spiral makes along one branch.
///
/// 2.5 turns over the cards of one cluster reads as alternate/spiral
/// phyllotaxis: every card leaves the twig at its own height and its own
/// azimuth, so the foliage spreads ALONG the twig instead of forming a ball
/// around one point.
const SPRAY_TURNS: f32 = 2.5;
/// Branches thinner than this (m) are dropped from the bake.
///
/// The old 3.5 cm threshold silently deleted the aspen's entire final
/// generation (~40 twigs), and those twigs were the ones carrying foliage:
/// every leaf clump on a culled twig hung in mid-air with no stem under it.
/// The threshold must therefore stay BELOW the thinnest leaf-bearing twig of
/// every profile; `leafy_branches_outlive_the_cull` asserts exactly that.
const MIN_BAKED_RADIUS: f32 = 0.010;

/// Sway weight written into `COLOR.r` for foliage vertices.
///
/// Leaves carry the motion because they are what catches the wind; the branch
/// tube they hang from is wood and barely moves. A trunk that swayed like the
/// crown would read as rubber.
pub const SWAY_WEIGHT_FOLIAGE: f32 = 1.0;
/// Sway weight for branch vertices. Not zero: a completely rigid trunk under a
/// moving crown reads as two disconnected objects at the join.
pub const SWAY_WEIGHT_BRANCH: f32 = 0.12;

/// How strongly height modulates the sway, per unit of normalised height.
///
/// Wind moves a crown more than the underside of the same crown; a uniform
/// offset would slide the whole thing sideways and read as a floating object.
pub const SWAY_HEIGHT_GAIN: f32 = 0.35;

/// How much detail a baked mesh keeps.
///
/// Foliage dominates the triangle count, so the tiers differ mostly in how many
/// leaf cards survive. A dusk silhouette is a flat dark shape, so dropping
/// cards and detail costs little visually while cutting the frame cost of the
/// far field sharply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TreeLod {
    /// Full detail: every branch and every cluster's cards.
    Near,
    /// Coarser foliage: the crown keeps its shape but fewer cards.
    Mid,
    /// A couple of big cards: at this distance the crown is a smudge.
    Far,
}

impl TreeLod {
    /// Tiers in draw order, nearest first.
    pub const ALL: [TreeLod; 3] = [TreeLod::Near, TreeLod::Mid, TreeLod::Far];

    /// Where this tier takes over from the previous one, in metres from the
    /// camera. The last entry is where trees disappear entirely.
    ///
    /// 40 m / 110 m / 220 m: `CHUNK_AHEAD` is 4 chunks (200 m), so the far tier
    /// only ever covers the last stretch the player can see. Close enough to the
    /// walk that the crown still reads, far enough that a 1.2-2.0 m/s walker
    /// sees the swap rarely.
    pub const BOUNDARIES: [f32; 4] = [0.0, 40.0, 110.0, 220.0];

    /// Distance over which neighbouring tiers crossfade (m).
    ///
    /// A tier's end margin and the next tier's start margin are the SAME range
    /// -- that is what makes `VisibilityRange` dither a *cross*fade instead of
    /// two independent vignettes. Within the band both tiers are drawn, each
    /// discarding a complementary screen-space dither pattern, so the swap
    /// dissolves instead of popping.
    pub const CROSSFADE: f32 = 8.0;

    /// The visibility range for this tier.
    ///
    /// Every tier carries one, including `Near`: `Near` has no start fade but
    /// does need an end margin to dissolve into `Mid` at 40-48 m. Leaving
    /// `Near` without a range is what made the first swap the harshest one.
    pub fn range(self) -> VisibilityRange {
        let index = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        let start = Self::BOUNDARIES[index];
        let end = Self::BOUNDARIES[index + 1];
        // The nearest tier has nothing in front of it, so its start is hard.
        let start_margin = if index == 0 {
            start..start
        } else {
            start..start + Self::CROSSFADE
        };
        VisibilityRange {
            start_margin,
            end_margin: end..end + Self::CROSSFADE,
            use_aabb: false,
        }
    }

    /// Smallest branch radius kept at this tier (m).
    fn min_branch_radius(self) -> f32 {
        match self {
            TreeLod::Near => MIN_BAKED_RADIUS,
            // Mid drops twigs entirely: at 40 m+ a 3.5 cm twig is under a pixel
            // and only carried the crown's inner structure.
            TreeLod::Mid => 0.06,
            TreeLod::Far => f32::INFINITY,
        }
    }

    /// Keep every `n`th cluster (1 = all).
    fn blob_stride(self) -> usize {
        match self {
            TreeLod::Near => 1,
            TreeLod::Mid => 2,
            TreeLod::Far => 4,
        }
    }

    /// Clusters below this radius (m) are dropped; keeps the big crown masses
    /// and discards the small filler.
    fn min_blob_radius(self) -> f32 {
        match self {
            TreeLod::Near => 0.0,
            TreeLod::Mid => 0.30,
            TreeLod::Far => 0.0,
        }
    }

    /// Fatten the surviving clusters so the crown still fills its silhouette
    /// once most of them are gone.
    fn blob_scale(self) -> f32 {
        match self {
            TreeLod::Near => 1.0,
            TreeLod::Mid => 1.18,
            TreeLod::Far => 1.55,
        }
    }

    /// Cards per cluster at this tier.
    fn cards_per_cluster(self) -> usize {
        match self {
            TreeLod::Near | TreeLod::Mid => CARDS_PER_CLUSTER,
            TreeLod::Far => FAR_CARDS_PER_CLUSTER,
        }
    }
}

/// Which skeleton to bake: species (index into [`TREE_PROFILES`]) and a
/// per-species variation id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TreeMeshKey {
    pub species: u8,
    pub variant: u8,
}

impl TreeMeshKey {
    /// Number of distinct variants baked per species.
    pub const VARIANTS: u8 = 4;

    /// Maps the placement planner's `(asset, wear)` onto a bake key.
    ///
    /// `asset` selects the species (the planner samples `0..tree_asset_count`)
    /// and `wear` — which trees also carry, unused for anything else — picks the
    /// variant. That gives neighbouring trees of one species different shapes
    /// without extra RNG plumbing through placement.
    pub fn from_placement(asset: u8, wear: f32) -> Self {
        let variant = (wear * Self::VARIANTS as f32)
            .floor()
            .clamp(0.0, (Self::VARIANTS - 1) as f32) as u8;
        Self {
            species: asset % TREE_PROFILES.len() as u8,
            variant,
        }
    }
}

/// World-scale silhouette values for a procedural tree.
///
/// Split out from the material so the numbers can be asserted headless: the
/// foliage shader multiplies the base colour by a per-season tint, and the crown
/// must stay this dark in every season.
pub const SILHOUETTE_BASE_COLOR_SRGB: [f32; 3] = [0.030, 0.038, 0.032];

/// The shipped silhouette as a plain `StandardMaterial`.
///
/// Only used by the gallery examples; the world builds the season-aware
/// `ExtendedMaterial` in [`crate::plugins::season`].
pub fn silhouette_material() -> StandardMaterial {
    StandardMaterial {
        base_color: Color::srgb(
            SILHOUETTE_BASE_COLOR_SRGB[0],
            SILHOUETTE_BASE_COLOR_SRGB[1],
            SILHOUETTE_BASE_COLOR_SRGB[2],
        ),
        perceptual_roughness: 1.0,
        metallic: 0.0,
        // No specular at all: a rim highlight would read as wet plastic and
        // break the cut-out look the whole design rests on.
        reflectance: 0.0,
        ..default()
    }
}

/// One tree baked into the two meshes it needs (see the module header).
#[derive(Clone, Debug)]
pub struct TreeParts {
    /// Branches: opaque silhouette material.
    pub wood: Mesh,
    /// Foliage cards: alpha-masked leaf texture.
    pub leaves: Mesh,
}

impl TreeParts {
    /// Total triangles across both meshes, for the budgets the tests assert.
    pub fn triangle_count(&self) -> usize {
        triangle_count(&self.wood) + triangle_count(&self.leaves)
    }
}

/// Grows the skeleton behind `key` and bakes it.
pub fn bake_tree_mesh(key: TreeMeshKey) -> TreeParts {
    bake_tree_mesh_lod(key, TreeLod::Near)
}

/// [`bake_tree_mesh`] at a specific detail tier.
pub fn bake_tree_mesh_lod(key: TreeMeshKey, lod: TreeLod) -> TreeParts {
    let profile = TREE_PROFILES
        .get(key.species as usize)
        .copied()
        .unwrap_or(TREE_PROFILES[0]);
    // Deliberately a fixed gallery seed: a tree's SHAPE must not depend on world
    // position, or the mesh cache would miss on every chunk. It also keeps every
    // LOD of one tree the same tree.
    let skeleton = grow_tree(
        crate::world::tree_shape::TREE_SHAPE_SEED,
        key.species,
        key.variant as u64,
    );
    bake_skeleton_lod(&skeleton, &profile, lod, &species_foliage(key.species))
}

/// Bakes every branch and leaf of `skeleton`.
pub fn bake_skeleton(skeleton: &TreeSkeleton, profile: &TreeProfile, species: u8) -> TreeParts {
    bake_skeleton_lod(skeleton, profile, TreeLod::Near, &species_foliage(species))
}

/// [`bake_skeleton`] at a specific detail tier.
///
/// Besides the geometry this writes the season attributes documented at the top
/// of the module. `foliage` carries the species' atlas cell and its card
/// tuning (see [`species_foliage`]).
pub fn bake_skeleton_lod(
    skeleton: &TreeSkeleton,
    profile: &TreeProfile,
    lod: TreeLod,
    foliage_bake: &FoliageBake,
) -> TreeParts {
    let mut wood_parts: Vec<Mesh> = Vec::new();
    let mut leaf_parts: Vec<Mesh> = Vec::new();
    let min_branch = lod.min_branch_radius();
    let top = trunk_top(skeleton);

    for branch in &skeleton.branches {
        let start = Vec3::from_array(branch.start);
        let end = Vec3::from_array(branch.end);
        let delta = end - start;
        let length = delta.length();
        // Sub-millimetre twigs contribute nothing at any viewing distance.
        if length < 1e-3 {
            continue;
        }
        if branch.radius < min_branch {
            continue;
        }
        let radius = branch.radius;
        // Taper comes from the segment's own two radii, not from the profile's
        // per-level ratio: a long limb is split into segments and its taper has
        // to run along the whole limb, or the chain pinches at every joint.
        let taper = (branch.end_radius / radius).clamp(0.25, 1.0);
        let unit = ConicalFrustumMeshBuilder::new(taper, 1.0, 1.0, BRANCH_SIDES)
            .build()
            // Unit tube runs +Y from its base; move the base to the origin before
            // orienting so `start` lands where it should.
            .translated_by(Vec3::new(0.0, 0.5, 0.0));
        let rotation = Quat::from_rotation_arc(Vec3::Y, delta / length);
        let placed = unit.transformed_by(
            Transform::from_translation(start)
                .with_rotation(rotation)
                .with_scale(Vec3::new(radius, length, radius)),
        );
        wood_parts.push(with_branch_season_data(placed, top));
    }

    // Foliage: keep every `stride`th cluster and drop the small filler. The
    // stride walk is deterministic (index order), so each tier stays a stable
    // subset of the same crown rather than a re-randomised one.
    let stride = lod.blob_stride();
    let min_blob = lod.min_blob_radius();
    let blob_scale = lod.blob_scale();
    // Per-species card count: a pinnate crown takes MORE, SMALLER cards, so
    // its compound leaves cut the sky into many more pieces.
    let cards =
        ((lod.cards_per_cluster() as f32 * foliage_bake.cards_scale).round() as usize).max(1);

    // The twigs that carry the leaves, as anchor lines. Cards are laid out
    // ALONG one of these instead of around a cluster centre: a 1.2 m leaf ball
    // centred on a 2 cm twig reads as a clump floating in front of the branch,
    // while a spiral of cards anchored on the twig itself reads as leaves
    // growing out of it.
    let anchors: Vec<BranchAnchor> = skeleton
        .branches
        .iter()
        // Only leaf-bearing generations are anchors. A cluster dangling near a
        // thick limb would otherwise attach its cards to that limb (a petiole
        // offset of `0.7 * limb radius`, centimetres off the visible twig) and
        // put leaves on a branch the profile deliberately keeps bare.
        .filter(|branch| branch.level >= profile.leaf_min_level)
        .filter_map(BranchAnchor::from_branch)
        .collect();

    for (index, foliage) in skeleton.foliage.iter().enumerate() {
        if index % stride != 0 {
            continue;
        }
        if foliage.radius < min_blob {
            continue;
        }
        let center = Vec3::from_array(foliage.center);
        let Some((anchor, along)) = BranchAnchor::nearest(&anchors, center) else {
            continue;
        };
        // The cluster's radius sizes the CARDS, not a ball. The reach scale
        // pairs with the card count (more, smaller cards for the pinnate
        // species) so the total coverage of one cluster stays put.
        let cluster_radius = (foliage.radius * blob_scale * foliage_bake.reach_scale).max(0.05);
        leaf_parts.push(leaf_spray(
            anchor,
            along,
            cluster_radius,
            index,
            cards,
            top,
            foliage_bake,
        ));
    }

    TreeParts {
        wood: merge_mesh_parts(wood_parts),
        leaves: merge_mesh_parts(leaf_parts),
    }
}

/// Height of the tallest point of the tree (m), used to normalise the sway
/// height term.
fn trunk_top(skeleton: &TreeSkeleton) -> f32 {
    skeleton
        .branches
        .iter()
        .map(|b| b.end[1])
        .chain(skeleton.foliage.iter().map(|f| f.center[1] + f.radius))
        .fold(0.0_f32, f32::max)
}

/// Writes `COLOR` for one branch part.
///
/// No UVs: branches are texture-less. They used to carry an offset pack in
/// `UV_0`/`UV_1` for the density collapse, which is gone now that density is the
/// leaf texture's alpha.
fn with_branch_season_data(mut mesh: Mesh, top: f32) -> Mesh {
    let count = mesh.count_vertices();
    let Some(positions) = mesh.attribute(Mesh::ATTRIBUTE_POSITION).cloned() else {
        return mesh;
    };
    let bevy::mesh::VertexAttributeValues::Float32x3(positions) = positions else {
        return mesh;
    };

    let mut colors = Vec::with_capacity(count);
    for p in &positions {
        colors.push([
            SWAY_WEIGHT_BRANCH * (1.0 + SWAY_HEIGHT_GAIN * height_term(p[1], top)),
            0.0,
            0.0,
            1.0,
        ]);
    }
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh
}

/// One branch treated as the line leaf cards hang from.
#[derive(Clone, Copy)]
struct BranchAnchor {
    start: Vec3,
    dir: Vec3,
    len: f32,
    radius: f32,
}

impl BranchAnchor {
    fn from_branch(branch: &crate::world::tree_shape::Branch) -> Option<Self> {
        let start = Vec3::from_array(branch.start);
        let delta = Vec3::from_array(branch.end) - start;
        let len = delta.length();
        (len >= 1e-3).then(|| Self {
            start,
            dir: delta / len,
            len,
            radius: branch.radius,
        })
    }

    /// The branch segment nearest to `p`, plus the distance along it.
    fn nearest(anchors: &[Self], p: Vec3) -> Option<(Self, f32)> {
        let mut best: Option<(f32, Self, f32)> = None;
        for anchor in anchors {
            let along = (p - anchor.start).dot(anchor.dir).clamp(0.0, anchor.len);
            let distance = p.distance_squared(anchor.start + anchor.dir * along);
            if best.as_ref().is_none_or(|(d, _, _)| distance < *d) {
                best = Some((distance, *anchor, along));
            }
        }
        best.map(|(_, anchor, along)| (anchor, along))
    }
}

/// Builds `cards` quads spiralling along one branch, each petiole on the twig.
///
/// The old layout wrapped the cards around a cluster centre up to a metre off
/// the twig. In game that read as a green clump floating beside the branch and,
/// from most angles, as "the leaves are one big bunched ball". A helix along
/// the twig uses the same card count and triangle budget but leaves (pun
/// intended) the foliage loose and divergent, because every card departs from
/// a different point of the twig at a different azimuth.
fn leaf_spray(
    anchor: BranchAnchor,
    anchor_at: f32,
    cluster_radius: f32,
    cluster: usize,
    cards: usize,
    top: f32,
    foliage: &FoliageBake,
) -> Mesh {
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(cards * 4);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(cards * 4);
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(cards * 4);
    let mut colors: Vec<[f32; 4]> = Vec::with_capacity(cards * 4);
    let mut indices: Vec<u32> = Vec::with_capacity(cards * 6);

    // Per-cluster sway phase, so neighbouring cards do not move in lockstep.
    let phase = ((cluster as f32 * 0.618_034).fract()).clamp(0.0, 1.0);
    let sway = SWAY_WEIGHT_FOLIAGE * (1.0 + SWAY_HEIGHT_GAIN * height_term(anchor.start.y, top));

    // Deterministic scatter, so the same tree always bakes the same cards.
    let mut hash = (cluster as u32).wrapping_mul(0x9E37_79B9).wrapping_add(1);
    let mut next_unit = || {
        hash ^= hash << 13;
        hash ^= hash >> 17;
        hash ^= hash << 5;
        (hash & 0xFFFF) as f32 / 65535.0
    };

    // Two axes perpendicular to the twig: the cards leave it in every direction
    // around these.
    let seed_axis = if anchor.dir.y.abs() < 0.9 {
        Vec3::Y
    } else {
        Vec3::X
    };
    let side0 = anchor.dir.cross(seed_axis).normalize();
    let side1 = anchor.dir.cross(side0).normalize();

    // Spread the cluster's cards over a SHORT stretch of the twig. When this
    // followed the whole limb, every cluster smeared its cards along a metre of
    // straight wood and the crown read as a cylinder of leaves wrapped around
    // each limb. Long limbs are also segmented now, so an anchor is already
    // short; the spray stays well inside it.
    let spray = (cluster_radius * 0.8).min(anchor.len * 0.7).max(0.08);

    // A card's half-extent. Long axis = the old cluster silhouette; short axis
    // follows the species' baked mask aspect so the leaf is not stretched.
    let half_h = cluster_radius * CARD_REACH_FRACTION * 1.15;
    let half_w = half_h * foliage.aspect;

    for card in 0..cards {
        let u = if cards <= 1 {
            0.5
        } else {
            card as f32 / (cards - 1) as f32
        };
        // Walk along the twig while rotating the departure azimuth: a spiral
        // (alternate phyllotaxis), not a rosette at one point.
        let s = (anchor_at + (u - 0.5) * spray + (next_unit() - 0.5) * spray * 0.12)
            .clamp(0.0, anchor.len);
        let angle = u * SPRAY_TURNS * std::f32::consts::TAU + phase;
        let radial = side0 * angle.cos() + side1 * angle.sin();
        // A little lean toward the branch tip, so the spray points up the twig
        // instead of forming a flat bottle-brush.
        let lean = (next_unit() * 2.0 - 1.0) * 0.5;
        let grow = (radial * lean.cos() + anchor.dir * lean.sin()).normalize();
        // The petiole sits on the bark, not inside the wood.
        let attach = anchor.start + anchor.dir * s + radial * (anchor.radius * 0.7);
        // Card width axis: any perpendicular to `grow`, rolled per card so the
        // card planes do not all contain the twig.
        let right0 = grow.cross(anchor.dir).normalize_or_zero();
        let right0 = if right0.length_squared() < 1e-6 {
            side0
        } else {
            right0
        };
        let face0 = grow.cross(right0).normalize();
        let roll = next_unit() * std::f32::consts::TAU;
        let right = right0 * roll.cos() + face0 * roll.sin();
        let face_axis = right.cross(grow).normalize();

        let card_center = attach + grow * half_h;
        let corners = [
            [-half_w, -half_h],
            [half_w, -half_h],
            [half_w, half_h],
            [-half_w, half_h],
        ];
        let base = positions.len() as u32;
        for (corner, [u, v]) in corners
            .iter()
            .zip([[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]])
        {
            // Local x is the card's width, local y the growth direction. `v = 0`
            // is the TOP of the texture (the tip), so +y is where the tip goes.
            let local = right * corner[0] + grow * corner[1];
            positions.push((card_center + local).to_array());
            // Cards are drawn double-sided, so one normal per quad is enough;
            // the PBR shader flips it for back faces.
            normals.push(face_axis.to_array());
            // Map the card's unit quad into this species' atlas cell: u is
            // rescaled into (u0, u1), v spans the shared full height.
            uvs.push([foliage.cell.0 + u * (foliage.cell.1 - foliage.cell.0), v]);
            colors.push([sway, 1.0, phase, 1.0]);
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

/// Normalised height of a vertex within the tree, clamped to 0..1.
fn height_term(y: f32, top: f32) -> f32 {
    if top <= 1e-3 {
        return 0.0;
    }
    (y / top).clamp(0.0, 1.0)
}

/// Merges parts that share an attribute set.
///
/// `Mesh::merge` requires identical attributes, and each group above does carry
/// one consistent set. A merge failure keeps what has been merged so far rather
/// than panicking — this runs inside chunk streaming, where a panic would take
/// the frame down.
fn merge_mesh_parts(parts: Vec<Mesh>) -> Mesh {
    let mut merged: Option<Mesh> = None;
    for part in parts {
        match &mut merged {
            None => merged = Some(part),
            Some(mesh) => {
                if let Err(err) = mesh.merge(&part) {
                    warn!("tree mesh merge failed: {err}");
                }
            }
        }
    }
    let mut merged = merged.unwrap_or_else(empty_mesh);
    merged.asset_usage = RenderAssetUsages::default();
    merged
}

/// Triangle count of a mesh, for the budgets the tests assert.
fn triangle_count(mesh: &Mesh) -> usize {
    match mesh.indices() {
        Some(Indices::U32(i)) => i.len() / 3,
        Some(Indices::U16(i)) => i.len() / 3,
        None => 0,
    }
}

/// An empty mesh with the attributes the merge path expects.
fn empty_mesh() -> Mesh {
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new())
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, Vec::<[f32; 3]>::new())
    .with_inserted_indices(Indices::U32(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors(m: &Mesh) -> Vec<[f32; 4]> {
        match m.attribute(Mesh::ATTRIBUTE_COLOR).expect("colour") {
            bevy::mesh::VertexAttributeValues::Float32x4(v) => v.clone(),
            other => panic!("unexpected colour layout: {other:?}"),
        }
    }

    fn positions(m: &Mesh) -> Vec<[f32; 3]> {
        match m.attribute(Mesh::ATTRIBUTE_POSITION).expect("positions") {
            bevy::mesh::VertexAttributeValues::Float32x3(v) => v.clone(),
            other => panic!("unexpected position layout: {other:?}"),
        }
    }

    /// Distance from `p` to one branch segment (m).
    fn branch_distance(branch: &crate::world::tree_shape::Branch, p: Vec3) -> f32 {
        branch_distance_squared(branch, p).sqrt()
    }

    /// Squared distance from `p` to one branch segment, computed with the same
    /// arithmetic `BranchAnchor::nearest` uses.
    ///
    /// The bake compares squared distances of points built from a NORMALISED
    /// direction; measuring with the raw segment parameterisation rounds
    /// differently, and two neighbouring twigs both ~6 cm from a cluster can
    /// then swap places. The test would measure the petiole against a segment
    /// the bake never anchored on (0.25 m off, in the broadleaf crown), so this
    /// mirrors `BranchAnchor::from_branch` + `nearest` exactly and
    /// `nearest_leafy_branch` breaks ties the same way.
    fn branch_distance_squared(branch: &crate::world::tree_shape::Branch, p: Vec3) -> f32 {
        let start = Vec3::from_array(branch.start);
        let delta = Vec3::from_array(branch.end) - start;
        let len = delta.length();
        if len < 1e-3 {
            return f32::INFINITY;
        }
        let dir = delta / len;
        let along = (p - start).dot(dir).clamp(0.0, len);
        p.distance_squared(start + dir * along)
    }

    /// Index and distance of the branch nearest to `p`.
    fn nearest_branch(
        skeleton: &crate::world::tree_shape::TreeSkeleton,
        p: Vec3,
    ) -> Option<(usize, f32)> {
        skeleton
            .branches
            .iter()
            .enumerate()
            .map(|(index, branch)| (index, branch_distance(branch, p)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// Index of the leaf-bearing branch nearest to `p`: the carrier the bake
    /// anchors a cluster on.
    ///
    /// Skips degenerate segments exactly like `BranchAnchor::from_branch` does;
    /// otherwise a zero-length stub wins the "nearest" comparison here while the
    /// bake anchors on the real twig, and the measured petiole distance is that
    /// of the wrong branch.
    fn nearest_leafy_branch(
        skeleton: &crate::world::tree_shape::TreeSkeleton,
        p: Vec3,
        min_level: u8,
    ) -> Option<usize> {
        skeleton
            .branches
            .iter()
            .enumerate()
            .filter(|(_, branch)| branch.level >= min_level)
            .filter(|(_, branch)| {
                (Vec3::from_array(branch.end) - Vec3::from_array(branch.start)).length() >= 1e-3
            })
            // Squared distance + first-minimum tie-break: the same comparison
            // `BranchAnchor::nearest` runs, so the test and the bake can never
            // disagree about which twig carries a cluster.
            .min_by(|a, b| {
                branch_distance_squared(a.1, p).total_cmp(&branch_distance_squared(b.1, p))
            })
            .map(|(index, _)| index)
    }

    #[test]
    fn leafy_branches_outlive_the_cull() {
        // Regression (2026-11-08): foliage is placed on every branch at or
        // above `leaf_min_level`, but the bake silently drops any branch
        // thinner than `MIN_BAKED_RADIUS`. The aspen's whole L4 generation
        // (3.3 cm) fell under the old 3.5 cm threshold while still carrying
        // three leaf clumps each, so half the crown's foliage hung in mid-air
        // with no twig underneath. A branch that carries leaves must be thick
        // enough to survive the Near-tier cull.
        for (species, profile) in TREE_PROFILES.iter().enumerate() {
            let tree = crate::world::tree_shape::grow_tree(
                crate::world::tree_shape::TREE_SHAPE_SEED,
                species as u8,
                0,
            );
            let thinnest_leafy = tree
                .branches
                .iter()
                .filter(|b| b.level >= profile.leaf_min_level)
                .map(|b| b.radius)
                .fold(f32::INFINITY, f32::min);
            assert!(
                thinnest_leafy >= MIN_BAKED_RADIUS,
                "species {species}: thinnest leaf-bearing twig is {thinnest_leafy:.3} m, below the                  {MIN_BAKED_RADIUS:.3} m bake threshold -- its leaves float",
            );
        }
    }

    #[test]
    fn both_meshes_have_geometry() {
        // The tree ships as two meshes; an empty either way means a silent hole
        // in the crown, or a floating crown with no trunk.
        for species in 0..TREE_PROFILES.len() as u8 {
            let parts = bake_tree_mesh(TreeMeshKey {
                species,
                variant: 0,
            });
            assert!(
                triangle_count(&parts.wood) > 0,
                "species {species}: no branches"
            );
            assert!(
                triangle_count(&parts.leaves) > 0,
                "species {species}: no leaf cards"
            );
        }
    }

    #[test]
    fn one_tree_stays_inside_the_triangle_budget() {
        // Cards are 2 triangles against a blob's 20, so the switch should keep a
        // tree cheap. If the card path regressed to spheres, this fails.
        for species in 0..TREE_PROFILES.len() as u8 {
            let parts = bake_tree_mesh(TreeMeshKey {
                species,
                variant: 0,
            });
            let total = parts.triangle_count();
            assert!(total < 20_000, "species {species}: {total} triangles");
        }
    }

    #[test]
    fn lod_tiers_actually_get_cheaper() {
        // A "LOD" that loses 5% is three copies of the same tree. Measured on
        // cards, each tier must be noticeably cheaper than the one before.
        for species in 0..TREE_PROFILES.len() as u8 {
            let key = TreeMeshKey {
                species,
                variant: 0,
            };
            let near = bake_tree_mesh_lod(key, TreeLod::Near).triangle_count();
            let mid = bake_tree_mesh_lod(key, TreeLod::Mid).triangle_count();
            let far = bake_tree_mesh_lod(key, TreeLod::Far).triangle_count();
            assert!(
                near > 0 && mid > 0 && far > 0,
                "species {species}: empty LOD"
            );
            assert!(
                mid < near * 3 / 4,
                "species {species}: mid keeps {mid}/{near}, not a real reduction"
            );
            assert!(far < mid / 2, "species {species}: far keeps {far}/{mid}");
        }
    }

    #[test]
    fn leaf_mesh_is_quads_with_uvs() {
        // The leaf material samples a texture, so every leaf vertex needs a UV.
        // Without one the crown would sample a single texel and read as a solid
        // rectangle, which is exactly the look the cards exist to remove. The
        // UVs span the species' ATLAS CELL (the atlas holds all three species;
        // a card sampling the whole texture would read its neighbours' masks).
        let parts = bake_tree_mesh(TreeMeshKey {
            species: 0,
            variant: 0,
        });
        let positions = positions(&parts.leaves);
        let uvs = match parts.leaves.attribute(Mesh::ATTRIBUTE_UV_0).expect("uv0") {
            bevy::mesh::VertexAttributeValues::Float32x2(v) => v.clone(),
            other => panic!("unexpected uv layout: {other:?}"),
        };
        assert_eq!(uvs.len(), positions.len(), "every leaf vertex needs a UV");
        assert_eq!(positions.len() % 4, 0, "the leaf mesh is not made of quads");
        let (cell_u0, cell_u1) = species_foliage(0).cell;
        let max_u = uvs.iter().map(|uv| uv[0]).fold(0.0_f32, f32::max);
        let max_v = uvs.iter().map(|uv| uv[1]).fold(0.0_f32, f32::max);
        assert!(
            max_u >= cell_u1 - 1e-4 && max_v >= 0.99,
            "UVs do not span the species' atlas cell"
        );
        // Every UV stays inside the species' own cell: one straying outside
        // samples a neighbouring species' mask.
        let min_u = uvs.iter().map(|uv| uv[0]).fold(f32::MAX, f32::min);
        assert!(
            min_u >= cell_u0 - 1e-4 && max_u <= cell_u1 + 1e-4,
            "leaf UVs leave the species' atlas cell"
        );
    }

    #[test]
    fn wood_carries_no_texture_and_no_foliage_mask() {
        // Branches use the opaque material, which has no texture. Their UV set
        // went away with the density collapse; if one came back it would need a
        // meaning.
        let parts = bake_tree_mesh(TreeMeshKey {
            species: 1,
            variant: 0,
        });
        for color in colors(&parts.wood) {
            assert_eq!(color[1], 0.0, "a branch vertex is flagged as foliage");
        }
        // The frustum builder emits its own UVs, which the opaque material
        // simply ignores. What matters is that they are not mistaken for the
        // old collapse pack: deleting that pack is what freed UV_0 for the leaf
        // texture, and a branch quad carrying leaf-like UVs would mean the two
        // paths got crossed again.
        if let Some(uvs) = parts.wood.attribute(Mesh::ATTRIBUTE_UV_0) {
            let bevy::mesh::VertexAttributeValues::Float32x2(uvs) = uvs else {
                panic!("unexpected uv layout");
            };
            assert!(
                uvs.iter().all(|uv| (0.0..=1.0).contains(&uv[0])),
                "branch UVs escaped the unit range"
            );
        }
    }

    #[test]
    fn leaves_are_flagged_so_the_shader_can_tint_them() {
        // The season tint is mixed in by the foliage mask in COLOR.g; without it
        // the crown keeps the bare silhouette in every season.
        let parts = bake_tree_mesh(TreeMeshKey {
            species: 2,
            variant: 0,
        });
        let mask = colors(&parts.leaves);
        assert!(!mask.is_empty());
        for color in &mask {
            assert_eq!(color[1], 1.0, "a leaf vertex is not flagged as foliage");
        }
        // The per-card phase must vary, or the cards sway in lockstep.
        let phases: std::collections::HashSet<u32> =
            mask.iter().map(|c| (c[2] * 1000.0) as u32).collect();
        assert!(phases.len() > 1, "every card shares one sway phase");
    }

    #[test]
    fn every_card_points_its_tip_away_from_the_branch() {
        // The texture has the petiole on its BOTTOM edge (v = 1) and the tip on
        // its TOP edge (v = 0). For every card the tip edge must therefore sit
        // further from the carrying twig than the petiole edge.
        //
        // This replaces the old "compare against the cluster's outward vector"
        // check: a spray has no single outward direction, and the invariant
        // that matters is that leaves grow OFF the branch, not which way one
        // cluster happened to face.
        for species in 0..TREE_PROFILES.len() as u8 {
            let skeleton = crate::world::tree_shape::grow_tree(
                crate::world::tree_shape::TREE_SHAPE_SEED,
                species,
                0,
            );
            let profile = TREE_PROFILES[species as usize];
            let parts = bake_skeleton(&skeleton, &profile, species);
            let positions = positions(&parts.leaves);
            let uvs = match parts.leaves.attribute(Mesh::ATTRIBUTE_UV_0).expect("uv0") {
                bevy::mesh::VertexAttributeValues::Float32x2(v) => v.clone(),
                other => panic!("unexpected uv layout: {other:?}"),
            };

            let foliage = species_foliage(species);
            let cards = ((TreeLod::Near.cards_per_cluster() as f32 * foliage.cards_scale).round()
                as usize)
                .max(1);
            let mut cursor = 0usize;
            let mut checked = 0usize;
            for cluster in &skeleton.foliage {
                if cursor + cards * 4 > positions.len() {
                    break;
                }
                // The bake anchors a cluster on the nearest LEAF-BEARING branch
                // to its centre, so measure against that same carrier. Taking
                // the nearest branch to each PETIOLE instead misfires in a dense
                // crown: an unrelated twig often crosses right against the
                // attachment point, and a card growing radially from its own
                // stem can then look "stuck" to that stranger limb.
                let carrier = nearest_leafy_branch(
                    &skeleton,
                    Vec3::from_array(cluster.center),
                    profile.leaf_min_level,
                );
                for card in 0..cards {
                    let quad = &uvs[cursor + card * 4..cursor + card * 4 + 4];
                    let pos = &positions[cursor + card * 4..cursor + card * 4 + 4];
                    // Compare the MIDPOINTS of the tip and petiole edges: taking
                    // one corner from each measures a diagonal, which mixes in
                    // the card's width and makes the check meaningless.
                    let mut tip = Vec3::ZERO;
                    let mut petiole = Vec3::ZERO;
                    let (mut n_tip, mut n_petiole) = (0u32, 0u32);
                    for (uv, p) in quad.iter().zip(pos.iter()) {
                        if uv[1] < 0.5 {
                            tip += Vec3::from_array(*p);
                            n_tip += 1;
                        } else {
                            petiole += Vec3::from_array(*p);
                            n_petiole += 1;
                        }
                    }
                    if n_tip == 0 || n_petiole == 0 {
                        continue;
                    }
                    let tip = tip / n_tip as f32;
                    let petiole = petiole / n_petiole as f32;
                    // Measure against the carrier twig this cluster was baked
                    // onto, not whichever branch happens to be nearest.
                    let Some(carrier) = carrier else {
                        continue;
                    };
                    let branch = &skeleton.branches[carrier];
                    let petiole_distance = branch_distance(branch, petiole);
                    let tip_distance = branch_distance(branch, tip);
                    // Measured against the AXIS, so the allowance has to cover
                    // the bark: a running cluster on a first-generation limb
                    // sits 0.7 x radius off the axis, which is 0.12 m on a
                    // 0.32 m-thick street-tree limb. What the check is really
                    // about is the petiole not leaving the bark.
                    assert!(
                        petiole_distance <= branch.radius + 0.10,
                        "species {species}: a petiole sits {petiole_distance:.2} m off the twig"
                    );
                    assert!(
                        tip_distance >= petiole_distance + 0.05,
                        "species {species}: a card tip ({tip_distance:.2} m) barely leaves the \
                         twig ({petiole_distance:.2} m)"
                    );
                    checked += 1;
                }
                cursor += cards * 4;
            }
            assert!(
                checked > 50,
                "species {species}: only {checked} cards were checked"
            );
        }
    }

    #[test]
    fn every_card_grows_out_of_its_branch_attachment_point() {
        // Regression history: cards sat on a shell 0.72-1.0 cluster radii from
        // the twig, then on a ball 0.28 radii from it -- both still read as
        // foliage floating beside a branch, with the twig visible in the empty
        // middle or in front of a solid leaf blob. The spray now anchors the
        // petiole EDGE on the bark, so its distance to the nearest branch
        // segment must stay at branch-surface scale.
        for species in 0..TREE_PROFILES.len() as u8 {
            let skeleton = crate::world::tree_shape::grow_tree(
                crate::world::tree_shape::TREE_SHAPE_SEED,
                species,
                0,
            );
            let profile = TREE_PROFILES[species as usize];
            let parts = bake_skeleton(&skeleton, &profile, species);
            let positions = positions(&parts.leaves);
            let uvs = match parts.leaves.attribute(Mesh::ATTRIBUTE_UV_0).expect("uv0") {
                bevy::mesh::VertexAttributeValues::Float32x2(v) => v.clone(),
                other => panic!("unexpected uv layout: {other:?}"),
            };

            let foliage = species_foliage(species);
            let cards = ((TreeLod::Near.cards_per_cluster() as f32 * foliage.cards_scale).round()
                as usize)
                .max(1);
            let mut cursor = 0usize;
            let mut checked = 0usize;
            for _cluster in &skeleton.foliage {
                if cursor + cards * 4 > positions.len() {
                    break;
                }
                for card in 0..cards {
                    let quad = &uvs[cursor + card * 4..cursor + card * 4 + 4];
                    let pos = &positions[cursor + card * 4..cursor + card * 4 + 4];
                    let mut petiole = Vec3::ZERO;
                    let mut n = 0u32;
                    for (uv, p) in quad.iter().zip(pos.iter()) {
                        if uv[1] > 0.5 {
                            petiole += Vec3::from_array(*p);
                            n += 1;
                        }
                    }
                    if n == 0 {
                        continue;
                    }
                    let petiole = petiole / n as f32;
                    let Some((nearest, distance)) = nearest_branch(&skeleton, petiole) else {
                        continue;
                    };
                    // Axis distance, so allow for the bark: a spray on a thick
                    // first-generation limb leaves the bark, not the axis.
                    let bark = skeleton.branches[nearest].radius;
                    assert!(
                        distance <= bark + 0.10,
                        "species {species}: a petiole sits {distance:.2} m from the nearest twig"
                    );
                    checked += 1;
                }
                cursor += cards * 4;
            }
            assert!(
                checked > 50,
                "species {species}: only {checked} cards were checked"
            );
        }
    }

    #[test]
    fn cards_cross_so_the_crown_has_volume() {
        // All cards at one angle would make the crown vanish edge-on.
        let parts = bake_tree_mesh(TreeMeshKey {
            species: 0,
            variant: 0,
        });
        let normals = match parts
            .leaves
            .attribute(Mesh::ATTRIBUTE_NORMAL)
            .expect("normals")
        {
            bevy::mesh::VertexAttributeValues::Float32x3(v) => v.clone(),
            other => panic!("unexpected normal layout: {other:?}"),
        };
        let mut sectors = std::collections::HashSet::new();
        for n in &normals {
            let angle = n[2].atan2(n[0]);
            let sector = ((angle / std::f32::consts::TAU) * 8.0).round() as i32;
            sectors.insert(sector.rem_euclid(8));
        }
        assert!(
            sectors.len() >= 4,
            "cards only face {} of 8 directions; the crown would disappear from some angles",
            sectors.len()
        );
    }

    #[test]
    fn sway_weight_grows_with_height() {
        // Wind moves the crown more than the trunk. A weight that ignored height
        // would slide the whole tree sideways as one rigid piece.
        let parts = bake_tree_mesh(TreeMeshKey {
            species: 1,
            variant: 0,
        });
        let all: Vec<([f32; 4], [f32; 3])> = colors(&parts.wood)
            .into_iter()
            .zip(positions(&parts.wood))
            .chain(
                colors(&parts.leaves)
                    .into_iter()
                    .zip(positions(&parts.leaves)),
            )
            .collect();
        let mut low = f32::INFINITY;
        let mut high: f32 = 0.0;
        let mut low_y = f32::INFINITY;
        let mut high_y: f32 = 0.0;
        for (color, p) in &all {
            if p[1] < low_y {
                low_y = p[1];
                low = color[0];
            }
            if p[1] > high_y {
                high_y = p[1];
                high = color[0];
            }
        }
        assert!(high_y > low_y, "tree has no vertical extent");
        assert!(
            high > low,
            "sway weight does not grow with height ({low} at {low_y} m, {high} at {high_y} m)"
        );
        for (color, _) in &all {
            assert!(
                (0.0..=2.5).contains(&color[0]),
                "sway weight {} out of range",
                color[0]
            );
        }
    }

    /// Fraction of the tree's crown bounding box that the leaf cards leave
    /// open, measured by projecting every card onto the XZ plane and testing
    /// coverage on a grid.
    ///
    /// This is the number the design actually cares about -- "sky cut into
    /// pieces by leaves" is a coverage question -- and it cannot be judged from
    /// triangle counts. A solid ball scores 0.0; a lacy crown scores high.
    fn xz_openness(parts: &TreeParts, mask: &crate::world::leaf_shape::AlphaMask) -> f32 {
        openness_along(parts, mask, true)
    }

    /// [`xz_openness`] but measured along the X axis instead: the SIDE view,
    /// which is what a player walking past actually sees. The top-down number
    /// says how much sky is visible from underneath, which answers a different
    /// question.
    fn side_openness(parts: &TreeParts, mask: &crate::world::leaf_shape::AlphaMask) -> f32 {
        openness_along(parts, mask, false)
    }

    fn openness_along(
        parts: &TreeParts,
        mask: &crate::world::leaf_shape::AlphaMask,
        top_down: bool,
    ) -> f32 {
        let positions = positions(&parts.leaves);
        let uvs = match parts.leaves.attribute(Mesh::ATTRIBUTE_UV_0).expect("uv0") {
            bevy::mesh::VertexAttributeValues::Float32x2(v) => v.clone(),
            other => panic!("unexpected uv layout: {other:?}"),
        };
        if positions.is_empty() {
            return 1.0;
        }
        let (mut min_x, mut max_x) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut min_z, mut max_z) = (f32::INFINITY, f32::NEG_INFINITY);
        // `top_down` projects onto XZ; otherwise onto YZ (a side elevation).
        let (ax, ay) = if top_down {
            (0usize, 2usize)
        } else {
            (2usize, 1usize)
        };
        for p in &positions {
            min_x = min_x.min(p[ax]);
            max_x = max_x.max(p[ax]);
            min_z = min_z.min(p[ay]);
            max_z = max_z.max(p[ay]);
        }
        const N: usize = 96;
        let mut covered = vec![false; N * N];
        for (quad, chunk) in positions.chunks(4).enumerate() {
            let quad_uv = &uvs[quad * 4..quad * 4 + 4];
            // Two triangles per quad.
            for tri in [[0usize, 1, 2], [0, 2, 3]] {
                let p0 = Vec3::from_array(chunk[tri[0]]);
                let p1 = Vec3::from_array(chunk[tri[1]]);
                let p2 = Vec3::from_array(chunk[tri[2]]);
                // Project onto the two chosen axes.
                let (p0, p1, p2) = (
                    (p0.to_array()[ax], p0.to_array()[ay]),
                    (p1.to_array()[ax], p1.to_array()[ay]),
                    (p2.to_array()[ax], p2.to_array()[ay]),
                );
                let area = (p1.0 - p0.0) * (p2.1 - p0.1) - (p1.1 - p0.1) * (p2.0 - p0.0);
                if area.abs() < 1e-9 {
                    continue;
                }
                for gz in 0..N {
                    for gx in 0..N {
                        let x = min_x + (gx as f32 + 0.5) / N as f32 * (max_x - min_x);
                        let z = min_z + (gz as f32 + 0.5) / N as f32 * (max_z - min_z);
                        let w0 = ((p1.0 - x) * (p2.1 - z) - (p1.1 - z) * (p2.0 - x)) / area;
                        let w1 = ((p2.0 - x) * (p0.1 - z) - (p2.1 - z) * (p0.0 - x)) / area;
                        let w2 = 1.0 - w0 - w1;
                        if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                            continue;
                        }
                        let u = w0 * quad_uv[tri[0]][0]
                            + w1 * quad_uv[tri[1]][0]
                            + w2 * quad_uv[tri[2]][0];
                        let v = w0 * quad_uv[tri[0]][1]
                            + w1 * quad_uv[tri[1]][1]
                            + w2 * quad_uv[tri[2]][1];
                        let tx = (u * mask.width as f32).clamp(0.0, mask.width as f32 - 1.0) as u32;
                        let ty =
                            (v * mask.height as f32).clamp(0.0, mask.height as f32 - 1.0) as u32;
                        if mask.get(tx, ty) >= 128 {
                            covered[gz * N + gx] = true;
                        }
                    }
                }
            }
        }
        let open = covered.iter().filter(|c| !**c).count();
        open as f32 / covered.len() as f32
    }

    #[test]
    fn the_crown_lets_sky_through() {
        // The design's whole reason for cards: looking up, the sky must be cut
        // into pieces rather than blocked by a mass. A sphere scores 0 here; the
        // first card version measured under 20% and read as a solid clump.
        //
        // Measured against EACH species' OWN leaf mask: openness is how much sky
        // shows through a crown of that species' actual cards, not a poplar
        // proxy. Using one poplar mask for every species hid real failures -- a
        // fixed-shape card stretches every leaf into the same blob, so the test
        // could never tell a pinnate crown from a broadleaf one.
        for species in 0..TREE_PROFILES.len() as u8 {
            let leaf = SPECIES_LEAF
                .get(species as usize)
                .copied()
                .unwrap_or(SPECIES_LEAF[0]);
            let profile = crate::world::leaf_shape::find_profile(leaf)
                .copied()
                .expect("species has a leaf profile");
            let mask = crate::world::leaf_shape::bake_leaf_alpha(&profile, 256);
            let parts = bake_tree_mesh(TreeMeshKey {
                species,
                variant: 0,
            });
            let openness = xz_openness(&parts, &mask);
            let side = side_openness(&parts, &mask);
            println!(
                "species {species} ({leaf}): top-down openness {:.1}%, side {:.1}%",
                openness * 100.0,
                side * 100.0
            );
            assert!(
                openness > 0.35,
                "species {species}: only {:.0}% of the crown leaves sky visible; it reads as a solid clump",
                openness * 100.0
            );
            // The side elevation is the view that matters on the street: a
            // crown can be open from underneath and still block the whole
            // horizon band when seen from a walking eye height.
            assert!(
                side > 0.30,
                "species {species}: only {:.0}% of the side elevation shows through",
                side * 100.0
            );
        }
    }

    #[test]
    fn lod_ranges_crossfade_without_gaps() {
        // Neighbouring tiers must share the SAME margin range; that shared
        // range is what makes the dither a crossfade instead of two separate
        // vignettes (a gap would flash sky, an offset overlap would double-draw
        // both tiers at full opacity).
        for pair in TreeLod::ALL.windows(2) {
            let (lower, upper) = (pair[0], pair[1]);
            assert_eq!(
                lower.range().end_margin,
                upper.range().start_margin,
                "{lower:?} and {upper:?} do not share their crossfade band"
            );
            assert!(
                lower.range().end_margin.end - lower.range().end_margin.start
                    >= TreeLod::CROSSFADE - 1e-5,
                "{lower:?} crossfade is shorter than {:.0} m",
                TreeLod::CROSSFADE
            );
        }
        // Only the nearest tier starts hard; everything else eases in.
        assert_eq!(TreeLod::Near.range().start_margin, 0.0..0.0);

        // Inside a shared band TWO tiers are drawn at once (each with a
        // partial dither); outside it exactly one.
        for (distance, expected) in [
            (0.0, 1),
            (39.9, 1),
            (44.0, 2),
            (47.9, 2),
            (48.0, 1),
            (109.9, 1),
            (114.0, 2),
            (118.0, 1),
            (219.9, 1),
        ] {
            let visible = TreeLod::ALL
                .iter()
                .filter(|lod| !lod.range().is_culled(distance))
                .count();
            assert_eq!(
                visible, expected,
                "{distance} m: expected {expected} tier(s), got {visible}"
            );
        }
    }

    #[test]
    fn keys_vary_by_species_and_variant() {
        let mut seen = std::collections::HashSet::new();
        for species in 0..TREE_PROFILES.len() as u8 {
            for variant in 0..TreeMeshKey::VARIANTS {
                seen.insert((species, variant));
            }
        }
        assert_eq!(
            seen.len(),
            TREE_PROFILES.len() * TreeMeshKey::VARIANTS as usize
        );
        for wear in [0.0, 0.5, 0.99, 1.0] {
            assert!(TreeMeshKey::from_placement(9, wear).variant < TreeMeshKey::VARIANTS);
        }
    }

    #[test]
    fn same_key_bakes_the_same_mesh() {
        let key = TreeMeshKey {
            species: 1,
            variant: 2,
        };
        let a = bake_tree_mesh(key);
        let b = bake_tree_mesh(key);
        assert_eq!(
            format!("{:?}", positions(&a.leaves)),
            format!("{:?}", positions(&b.leaves)),
            "bake must be deterministic"
        );
    }

    #[test]
    fn every_species_cards_sample_their_own_cell() {
        // The shared atlas holds all three species' masks; one UV straying
        // outside its species' cell samples a neighbour's mask, which is a
        // silent wrong-shape bug (no error, just the wrong crown).
        for species in 0..TREE_PROFILES.len() as u8 {
            let parts = bake_tree_mesh(TreeMeshKey {
                species,
                variant: 0,
            });
            let uvs = match parts.leaves.attribute(Mesh::ATTRIBUTE_UV_0).expect("uv0") {
                bevy::mesh::VertexAttributeValues::Float32x2(v) => v.clone(),
                other => panic!("unexpected uv layout: {other:?}"),
            };
            let (cell_u0, cell_u1) = species_foliage(species).cell;
            for (i, uv) in uvs.iter().enumerate() {
                assert!(
                    (cell_u0..=cell_u1).contains(&uv[0]),
                    "species {species}: uv[{i}] u {:.4} leaves its cell [{cell_u0:.4}, {cell_u1:.4}]",
                    uv[0]
                );
            }
        }
    }

    #[test]
    fn the_pinnate_crown_takes_more_smaller_cards() {
        // Feihei's direction (2026-10-30): the locust's compound leaves should
        // read as the finest, most fragmented silhouette. A pinnate unit is
        // one small piece, so its crown takes MORE, SMALLER cards — asserted
        // because it is a deliberate art direction a "sensible default"
        // refactor would happily undo.
        let sophora = species_foliage(1);
        assert!(
            sophora.cards_scale > 1.0 && sophora.reach_scale < 1.0,
            "the pinnate crown lost its finer fragmentation"
        );
        let sophora_cards = bake_tree_mesh(TreeMeshKey {
            species: 1,
            variant: 0,
        });
        let poplar_cards = bake_tree_mesh(TreeMeshKey {
            species: 0,
            variant: 0,
        });
        let count = |parts: &TreeParts| parts.leaves.count_vertices() / 4;
        assert!(
            count(&sophora_cards) > count(&poplar_cards),
            "species 1 must take more cards than species 0 ({} vs {})",
            count(&sophora_cards),
            count(&poplar_cards)
        );
        // The total tri budget still holds: cards are 2 triangles each, and a
        // 1.5x card count on the pinnate species stays far below the budget.
        let total: usize = (0..TREE_PROFILES.len() as u8)
            .map(|s| {
                let parts = bake_tree_mesh(TreeMeshKey {
                    species: s,
                    variant: 0,
                });
                parts.wood.count_vertices() / 3 + parts.leaves.count_vertices() / 3
            })
            .sum();
        assert!(total < 20_000, "the species set bakes {total} triangles");
    }
}
