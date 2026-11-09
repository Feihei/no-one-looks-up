//! Chunk streaming (R2): 50 m chunks spawned/despawned around the player
//! (ADR-0002, second generation phase).
//!
//! The [`ChunkManager`] resource tracks the active set; every chunk's
//! entities are grouped under one root entity (children), so despawn removes
//! the whole group recursively (Bevy 0.20 `despawn` despawns `Children`).
//! Chunk content in R2 is a ground tile per chunk (seed-tinted asphalt) for
//! streaming feedback; buildings/trees arrive with R3 placement.
//!
//! The active set is an oriented footprint around the walk direction:
//! `CHUNK_AHEAD` chunks in front, `CHUNK_BEHIND` behind, `CHUNK_SIDE` to each
//! side — about 49 chunks at peak (see `desired_chunks`).

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::camera::visibility::VisibilityRange;
use bevy::prelude::*;

use crate::plugins::facade::{FacadeMaterial, facade_material};
use crate::world::collision::CollisionIndex;
use crate::world::facade::{FacadeStyle, VARIANT_COUNT, bake_facade, texture_image};
use crate::world::layout_graph::BlockType;
use crate::world::layout_graph::LayoutGraph;
use crate::world::path_network::{WorldSeed, generate_world};
use crate::world::placement;
use crate::world::props::PropKind;
use crate::world::roof::{RoofKind, RoofProfile, bake_gable, bake_tooth};
use crate::world::tree_mesh::{TreeLod, TreeMeshKey, bake_tree_mesh_lod};
use crate::world::tree_shape::TREE_PROFILES;
use crate::world::{CHUNK_AHEAD, CHUNK_BEHIND, CHUNK_SIZE, CHUNKS_PER_FRAME, GRID_CELLS};

/// Default seed until the R4 menu provides one (dev builds).
pub const DEFAULT_SEED: u64 = 0x4845_4144; // "HEAD"

/// Chunk grid coordinates. Chunk `(x, z)` covers the world square
/// `[x * CHUNK_SIZE, (x + 1) * CHUNK_SIZE) x [z * CHUNK_SIZE, (z + 1) *
/// CHUNK_SIZE)`; 16 x 16 chunks tile the 800 m world.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChunkCoords {
    pub x: i32,
    pub z: i32,
}

impl ChunkCoords {
    /// Center of the chunk in world coordinates (ground plane, meters).
    pub fn center(self) -> Vec2 {
        Vec2::new(
            (self.x as f32 + 0.5) * CHUNK_SIZE,
            (self.z as f32 + 0.5) * CHUNK_SIZE,
        )
    }

    /// Chunk index for a world position.
    pub fn from_world(pos: Vec2) -> Self {
        Self {
            x: (pos.x / CHUNK_SIZE).floor() as i32,
            z: (pos.y / CHUNK_SIZE).floor() as i32,
        }
    }
}

/// Marker on a chunk group root: despawning the root removes every per-chunk
/// entity (ground tile, later placement content) recursively.
#[derive(Component, Debug, Default)]
pub struct ChunkRoot;

/// Tracks the active chunks and their group roots.
#[derive(Resource, Debug, Default)]
pub struct ChunkManager {
    /// Active chunk -> group root entity.
    pub active: HashMap<(i32, i32), Entity>,
    /// Chunks waiting to be spawned, nearest-first (original load queue).
    pub pending_load: VecDeque<(i32, i32)>,
    /// Chunks waiting to be despawned (original unload queue).
    pub pending_unload: VecDeque<(i32, i32)>,
}

impl ChunkManager {
    /// Chunks that should be active for a player at `pos` walking toward
    /// `forward` (world-plane direction).
    pub fn desired_chunks(pos: Vec2, forward: Vec2) -> Vec<ChunkCoords> {
        // Port of `chunk_manager.gd::_compute_needed` (user-facing behaviour:
        // a square window with a smaller radius behind).
        let forward = if forward.length_squared() > f32::EPSILON {
            forward.normalize()
        } else {
            // Looking straight down: no horizontal facing, keep the defaults.
            Vec2::new(0.0, -1.0)
        };
        let player = ChunkCoords::from_world(pos);
        let max_r = CHUNK_AHEAD.max(CHUNK_BEHIND);

        let mut out = Vec::new();
        for dx in -max_r..=max_r {
            for dz in -max_r..=max_r {
                let x = player.x + dx;
                let z = player.z + dz;
                if x < 0 || z < 0 || x >= GRID_CELLS as i32 || z >= GRID_CELLS as i32 {
                    continue;
                }
                let coords = ChunkCoords { x, z };
                if dx == 0 && dz == 0 {
                    out.push(coords); // Player's own chunk: always loaded.
                    continue;
                }
                let center = coords.center();
                let to_chunk = center - pos;
                // Ahead (or exactly abeam) uses the front radius, behind the
                // back radius. Chebyshev keeps the window square, so the side
                // extent equals the front radius ahead of the player.
                let radius = if to_chunk.dot(forward) >= 0.0 {
                    CHUNK_AHEAD
                } else {
                    CHUNK_BEHIND
                };
                if dx.abs().max(dz.abs()) <= radius {
                    out.push(coords);
                }
            }
        }
        out
    }
}

/// Expands a Chebyshev key towards `pos` for stable load ordering.
fn chunk_distance(key: (i32, i32), pos: Vec2) -> f32 {
    let center = ChunkCoords { x: key.0, z: key.1 }.center();
    (center - pos).length_squared()
}

/// Deterministic per-chunk asphalt tint (subtle luminance variation) so the
/// streaming feedback is visible in dev mode.
fn tile_color(coords: ChunkCoords) -> Color {
    let h = (coords.x as u64).rotate_left(32) ^ coords.z as u64;
    let h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let v = 0.085 + (h >> 56) as f32 / 255.0 * 0.02;
    Color::srgb(v, v, v * 1.05)
}

/// World plugin systems (ordering set, R2.8: every system chain is explicit).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorldSystems {
    /// Startup layout generation.
    Generation,
    /// Chunk streaming (spawn/despawn around the player).
    Streaming,
}

/// Generates the world layout at startup from the seed resource (or the
/// default seed until the R4 menu exists) and initializes the chunk manager.
fn generate_layout(mut commands: Commands, seed: Option<Res<WorldSeed>>) {
    let seed_value = seed.map(|s| s.0).unwrap_or(DEFAULT_SEED);
    match generate_world(seed_value) {
        Ok((grid, graph)) => {
            info!(
                "world generated: seed={seed_value:#x} blocks={} street segments={} nodes={}",
                graph.blocks.len(),
                graph.edges.len(),
                graph.nodes.len()
            );
            commands.insert_resource(grid);
            commands.insert_resource(graph);
            commands.insert_resource(ChunkManager::default());
            commands.init_resource::<CollisionIndex>();
        }
        Err(err) => error!("world generation failed: {err}"),
    }
}

/// Updates the active chunk set from the camera position and facing, spawning
/// missing chunk groups and despawning the ones the player walked away from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_chunk_streaming(
    cameras: Query<&Transform, With<Camera3d>>,
    mut manager: ResMut<ChunkManager>,
    mut collision: ResMut<CollisionIndex>,
    graph: Option<Res<LayoutGraph>>,
    assets: Option<Res<PlacementAssets>>,
    seed: Option<Res<WorldSeed>>,
    mut commands: Commands,
    mut meshes: Option<ResMut<Assets<Mesh>>>,
    mut materials: Option<ResMut<Assets<StandardMaterial>>>,
) {
    let Ok(camera) = cameras.single() else {
        return;
    };
    let seed_value = seed.map(|s| s.0).unwrap_or(DEFAULT_SEED);
    let pos = Vec2::new(camera.translation.x, camera.translation.z);
    let forward = Vec2::new(camera.forward().x, camera.forward().z);

    let desired = ChunkManager::desired_chunks(pos, forward);
    let desired_keys: HashSet<(i32, i32)> = desired.iter().map(|c| (c.x, c.z)).collect();

    // Rebuild the load queue: everything wanted but not active and not
    // already queued, ordered nearest-first so the player's surroundings fill
    // in before the horizon.
    let active_keys: Vec<(i32, i32)> = manager.active.keys().copied().collect();
    manager
        .pending_load
        .retain(|key| desired_keys.contains(key) && !active_keys.contains(key));
    for coords in &desired {
        let key = (coords.x, coords.z);
        if !active_keys.contains(&key) && !manager.pending_load.contains(&key) {
            manager.pending_load.push_back(key);
        }
    }
    manager.pending_load.make_contiguous().sort_by(|a, b| {
        chunk_distance(*a, pos)
            .partial_cmp(&chunk_distance(*b, pos))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Same for unloads: chunks outside the window leave, closest first.
    manager
        .pending_unload
        .retain(|key| active_keys.contains(key));
    for key in &active_keys {
        if !desired_keys.contains(key) && !manager.pending_unload.contains(key) {
            manager.pending_unload.push_back(*key);
        }
    }

    // Process at most CHUNKS_PER_FRAME of each per frame (original budget):
    // spawning builds a chunk's whole contents, so a teleport or sharp turn
    // must not pay for all of them in a single frame.
    for _ in 0..CHUNKS_PER_FRAME {
        let Some(key) = manager.pending_unload.pop_front() else {
            break;
        };
        if let Some(root) = manager.active.remove(&key) {
            collision.unregister_chunk(key);
            commands.entity(root).try_despawn();
        }
    }

    for _ in 0..CHUNKS_PER_FRAME {
        let Some(key) = manager.pending_load.pop_front() else {
            break;
        };
        if manager.active.contains_key(&key) {
            continue;
        }
        let coords = ChunkCoords { x: key.0, z: key.1 };
        let root = commands
            .spawn((
                Name::new(format!("Chunk({}, {})", coords.x, coords.z)),
                ChunkRoot,
                coords,
                // A parent WITHOUT `Transform`/`Visibility` breaks hierarchy
                // propagation for every child: the ground tile, the buildings
                // and the trees all lost their `GlobalTransform` and rendered
                // nothing (while the gizmo overlay, which does not go through
                // the hierarchy, kept drawing). Bevy warns B0004 once per
                // child; the log showed 1120 of them.
                Transform::default(),
                Visibility::default(),
            ))
            .id();
        // Ground tile: real streaming feedback; placement content follows.
        if let (Some(meshes), Some(materials)) = (meshes.as_deref_mut(), materials.as_deref_mut()) {
            let mesh = meshes.add(Plane3d::default().mesh().size(CHUNK_SIZE, CHUNK_SIZE));
            let material = materials.add(StandardMaterial {
                base_color: tile_color(coords),
                perceptual_roughness: 0.95,
                ..default()
            });
            let center = coords.center();
            commands.entity(root).with_children(|parent| {
                parent.spawn((
                    Name::new("ChunkGround"),
                    Mesh3d(mesh),
                    MeshMaterial3d(material),
                    Transform::from_xyz(center.x, 0.0, center.y),
                ));
            });
        }
        // Collision prisms travel with the chunk: R3.3 keeps the solid set in
        // step with what is actually spawned, so a walker can never slip
        // through a block whose collision was registered for another chunk.
        if let Some(graph) = graph.as_deref() {
            collision.register_chunk(graph, key);
            // R3 placement: plan this chunk's buildings/trees and spawn them
            // under the chunk root, so despawning the root removes them all.
            if assets.is_none() {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    error!(
                        "chunk {key:?} streamed with NO PlacementAssets: no buildings, no trees"
                    );
                });
            }
            if let Some(assets) = assets.as_deref() {
                let plan = placement::plan_chunk(
                    graph,
                    seed_value,
                    key,
                    // Procedural trees: the planner samples a SPECIES index.
                    TREE_PROFILES.len() as u8,
                );
                // Trees/buildings use the shared handles in `assets`, so
                // streaming needs no `Assets<Mesh>` access at all.
                spawn_chunk_content(&mut commands, assets, root, &plan);
            }
        }
        manager.active.insert(key, root);
    }
}

/// Shared placeholder materials + tree scenes, built once (Draw Call
/// control, ADR-0004).
/// The roof geometry and material of one style.
pub struct RoofAssets {
    pub material: Handle<StandardMaterial>,
    /// 双坡 gable, unit footprint with the eave overhang baked.
    pub gable: Handle<Mesh>,
    /// One sawtooth tooth, unit footprint.
    pub tooth: Handle<Mesh>,
}

#[derive(Resource)]
pub(crate) struct PlacementAssets {
    /// `facade_materials[style][variant][bucket]`: one shared material per
    /// (style, pattern variant, wear bucket), so a building's facade costs no
    /// draw state beyond the bucket switch (R3.6) and neighbours rarely share
    /// the same 12 m tile (R3.11).
    ///
    /// 3 x 2 x 4 = 24 materials for the whole city, which is the number that
    /// makes this affordable at all: one material per building would put a
    /// streaming window into the thousands of draw calls.
    facade_materials:
        [[[Handle<FacadeMaterial>; placement::WEAR_BUCKETS]; VARIANT_COUNT as usize]; 3],
    /// One roof material and mesh set per style. The gable and sawtooth tooth
    /// are unit meshes scaled per instance; the parapet is four instances of the
    /// shared unit cube, because its wall thickness must stay ~0.25 m absolute
    /// and a baked ring would scale it with the building.
    roofs: [RoofAssets; 3],
    plaza_material: Handle<StandardMaterial>,
    /// One baked mesh pair per `(lod, species, variant)` (R3.9): a whole tree
    /// in a single `Mesh3d` per part, so a chunk costs one entity per tree part
    /// per LOD tier. Indices follow `lod * TreeMeshKey::VARIANTS *
    /// TREE_PROFILES.len() + species * TreeMeshKey::VARIANTS + variant`.
    ///
    /// The wood slot is `None` where the tier drops every branch (Far keeps
    /// leaves only): baking an empty mesh there costs nothing to draw but
    /// makes the mesh allocator warn 12 times per run, which buries real
    /// warnings.
    tree_meshes: Vec<(Option<Handle<Mesh>>, Handle<Mesh>)>,
    /// Opaque silhouette material for the branches.
    tree_material: Handle<crate::plugins::season::TreeSeasonMaterial>,
    /// Alpha-tested material for the foliage cards, textured with a species
    /// leaf mask.
    tree_leaf_material: Handle<crate::plugins::season::TreeSeasonMaterial>,
    /// One unit cube reused by every building and plaza pad; each instance
    /// scales it. Buildings used to call `meshes.add(Cuboid)` per object,
    /// which allocated a new mesh every time a chunk streamed in.
    unit_cube: Handle<Mesh>,
    /// One unit cylinder (radius 0.5, height 1) for the round props:
    /// downspouts, chimneys, water tanks, roof vents, utility poles.
    unit_cylinder: Handle<Mesh>,
    /// Shared prop meshes and materials (R3.11 step 11.4).
    props: PropAssets,
}

/// Shared prop meshes and materials (R3.11 step 11.4). Built once; every
/// prop instance reuses these handles, so a streaming window's prop clutter
/// costs a handful of draw calls no matter how many boxes it hangs.
pub(crate) struct PropAssets {
    /// Fixtures: downspouts, water tanks, stair bulkheads, roof vents.
    dark: Handle<StandardMaterial>,
    /// Rusted variant for worn buildings' fixtures (锈蚀外挂件).
    rusted: Handle<StandardMaterial>,
    /// AC condenser cabinets.
    ac: Handle<StandardMaterial>,
    /// Brick chimneys.
    brick: Handle<StandardMaterial>,
    /// Awning canvases (the prop's variant bit picks the colour).
    awning_canvas: [Handle<StandardMaterial>; 2],
    /// The tilted canvas slab, wall edge at local z = 0 (real size).
    awning: Handle<Mesh>,
    /// The balcony slab + railing, wall plane at local z = 0 (real size).
    balcony: Handle<Mesh>,
    /// The street-lamp bracket reaching local +x (real size).
    lamp_arm: Handle<Mesh>,
    /// The glowing lamp head in the bracket (real size).
    lamp_head: Handle<Mesh>,
    /// The pole-top crossarm the wires meet (real size, beam along local +x).
    crossarm: Handle<Mesh>,
    /// The unit catenary wire, x from 0 to 1.
    wire: Handle<Mesh>,
    /// Cross-street drop line: the same catenary at a tighter sag (R3.13).
    cross_wire: Handle<Mesh>,
    /// The long lamp bracket reaching over the road (real size, R3.13).
    long_lamp_arm: Handle<Mesh>,
    /// The glowing lens under the long bracket's head (real size).
    long_lamp_head: Handle<Mesh>,
    /// Scaffold deck planks: weathered plywood, the one warm brown in the
    /// street furniture.
    plank: Handle<StandardMaterial>,
    /// Street-lamp head: the one emissive material in the world. The lamps
    /// are on at golden hour -- warm dots along every street, picked up by
    /// the bloom pass, no real light budget spent.
    lamp_glow: Handle<StandardMaterial>,
    /// Catenary cables: near-black, fully matte.
    wire_dark: Handle<StandardMaterial>,
}

impl PropAssets {
    /// Fixture material for a prop's variant bit: 0 = dark metal, 1 = rust.
    fn fixture(&self, variant: u8) -> Handle<StandardMaterial> {
        if variant % 2 == 1 {
            self.rusted.clone()
        } else {
            self.dark.clone()
        }
    }
}

/// Builds the shared prop meshes and materials (R3.11 step 11.4).
///
/// The recipe is the tree/roof one: fully matte, `reflectance = 0` -- a
/// glinting fixture reads as wet plastic against the dusk sky it is meant
/// to silhouette against. The colours stay muted; the rust is the only
/// saturated note, and it is earned by the building's wear roll.
fn build_prop_assets(
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) -> PropAssets {
    // The glow material is built before the `matte` closure takes its
    // mutable borrow of the store.
    let lamp_glow = materials.add(StandardMaterial {
        base_color: Color::srgb(0.85, 0.8, 0.65),
        emissive: LinearRgba::rgb(
            crate::world::props::LAMP_EMISSIVE,
            crate::world::props::LAMP_EMISSIVE * 0.72,
            crate::world::props::LAMP_EMISSIVE * 0.42,
        ),
        ..default()
    });
    let mut matte = |rgb: [f32; 3], rough: f32| {
        materials.add(StandardMaterial {
            base_color: Color::srgb(rgb[0], rgb[1], rgb[2]),
            perceptual_roughness: rough,
            reflectance: 0.0,
            ..default()
        })
    };
    PropAssets {
        dark: matte([0.28, 0.29, 0.31], 0.8),
        rusted: matte([0.36, 0.21, 0.12], 0.95),
        ac: matte([0.78, 0.76, 0.7], 0.85),
        brick: matte([0.38, 0.22, 0.16], 0.95),
        awning_canvas: [
            matte([0.48, 0.22, 0.18], 0.95),
            matte([0.3, 0.38, 0.28], 0.95),
        ],
        awning: meshes.add(crate::world::props::bake_awning()),
        balcony: meshes.add(crate::world::props::bake_balcony()),
        lamp_arm: meshes.add(crate::world::props::bake_lamp_arm()),
        lamp_head: meshes.add(crate::world::props::bake_lamp_head()),
        crossarm: meshes.add(crate::world::props::bake_crossarm()),
        wire: meshes.add(crate::world::props::bake_wire()),
        cross_wire: meshes.add(crate::world::overhead::bake_cross_wire()),
        long_lamp_arm: meshes.add(crate::world::overhead::bake_long_lamp_arm()),
        long_lamp_head: meshes.add(crate::world::overhead::bake_long_lamp_head()),
        plank: matte([0.34, 0.26, 0.18], 0.95),
        lamp_glow,
        wire_dark: matte([0.05, 0.05, 0.06], 1.0),
    }
}

impl PlacementAssets {
    /// Baked mesh for a placement tree instance at one LOD tier, or `None` if
    /// out of range.
    /// `(wood, leaves)` meshes for a placement tree instance at one LOD tier.
    fn tree_mesh(
        &self,
        asset: u8,
        wear: f32,
        lod: TreeLod,
    ) -> Option<&(Option<Handle<Mesh>>, Handle<Mesh>)> {
        let key = crate::world::tree_mesh::TreeMeshKey::from_placement(asset, wear);
        let per_lod = TREE_PROFILES.len() * TreeMeshKey::VARIANTS as usize;
        let tier = TreeLod::ALL.iter().position(|t| *t == lod)?;
        let index = tier * per_lod
            + key.species as usize * TreeMeshKey::VARIANTS as usize
            + key.variant as usize;
        self.tree_meshes.get(index)
    }
}

/// Builds one roof material and mesh set per style.
///
/// Split from the facade materials because the roofs are plain
/// `StandardMaterial`s: the roofline reads as a silhouette, so they use the
/// tree recipe (fully rough, no specular) instead of the facade's reflective
/// one. A wet-looking glint along the roof edge is exactly what the tree
/// material's comment warns about.
fn build_roof_assets(
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) -> [RoofAssets; 3] {
    FacadeStyle::ALL.map(|style| {
        let profile = RoofProfile::of(style);
        RoofAssets {
            material: materials.add(StandardMaterial {
                base_color: Color::srgb(profile.tint[0], profile.tint[1], profile.tint[2]),
                // The tree recipe: a rim highlight on the roofline would read as
                // wet plastic against the sky it is meant to cut.
                perceptual_roughness: 1.0,
                metallic: 0.0,
                reflectance: 0.0,
                ..default()
            }),
            gable: meshes.add(bake_gable()),
            tooth: meshes.add(bake_tooth()),
        }
    })
}

/// Builds every `(style, variant, wear bucket)` facade material.
///
/// Split out of [`build_placement_assets`] because the three nested loops there
/// made that system unreadable, and because this is the part with a decision in
/// it: the maps are shared across wear buckets (four materials per bake, not
/// four bakes), so wear costs 24 handles and 18 textures, never 24 textures.
fn build_facade_materials(
    materials: &mut Assets<FacadeMaterial>,
    mut images: Option<&mut Assets<Image>>,
    palette: &placement::StylePalette,
) -> [[[Handle<FacadeMaterial>; placement::WEAR_BUCKETS]; VARIANT_COUNT as usize]; 3] {
    let mut out: [[[Handle<FacadeMaterial>; placement::WEAR_BUCKETS]; VARIANT_COUNT as usize]; 3] =
        std::array::from_fn(|_| {
            std::array::from_fn(|_| std::array::from_fn(|_| Handle::<FacadeMaterial>::default()))
        });

    for (style_index, style) in FacadeStyle::ALL.into_iter().enumerate() {
        let tint = match style {
            FacadeStyle::OldCity => palette.old_city,
            FacadeStyle::Apartment => palette.apartment,
            FacadeStyle::Factory => palette.factory,
        };
        for (variant_index, slot) in out[style_index].iter_mut().enumerate() {
            let variant = variant_index as u8;
            // Headless tests have no `Assets<Image>`: the material then carries
            // no maps, which renders as a flat wall and is irrelevant there
            // (nothing is drawn). The material itself is still built, so the
            // material-count test covers the real pipeline in both worlds.
            let (albedo, mr, emissive) = match images.as_deref_mut() {
                Some(images) => {
                    let baked = bake_facade(style, variant);
                    (
                        images.add(texture_image(&baked.base, baked.base_size, true)),
                        images.add(texture_image(
                            &baked.metallic_roughness,
                            baked.channel_size,
                            false,
                        )),
                        images.add(texture_image(&baked.emissive, baked.channel_size, true)),
                    )
                }
                None => (
                    Handle::<Image>::default(),
                    Handle::<Image>::default(),
                    Handle::<Image>::default(),
                ),
            };
            for (bucket, handle) in slot.iter_mut().enumerate() {
                let t = bucket as f32 / (placement::WEAR_BUCKETS - 1) as f32;
                let luma = tint[0] * 0.3 + tint[1] * 0.6 + tint[2] * 0.1;
                let desat = 0.25 * t;
                let darken = 1.0 - 0.35 * t;
                let mixed = |c: f32| (luma * desat + c * (1.0 - desat)) * darken;
                *handle = materials.add(facade_material(
                    style,
                    albedo.clone(),
                    mr.clone(),
                    emissive.clone(),
                    [mixed(tint[0]), mixed(tint[1]), mixed(tint[2])],
                ));
            }
        }
    }
    out
}

/// Builds the placement assets on first chunk spawn (lazy: `Mesh3d` handles
/// need the render `Assets` stores, which headless tests do not run).
fn build_placement_assets(
    mut commands: Commands,
    meshes: Option<ResMut<Assets<Mesh>>>,
    materials: Option<ResMut<Assets<StandardMaterial>>>,
    facade_materials: Option<ResMut<Assets<FacadeMaterial>>>,
    tree_materials: Option<ResMut<Assets<crate::plugins::season::TreeSeasonMaterial>>>,
    mut images: Option<ResMut<Assets<Image>>>,
    existing: Option<Res<PlacementAssets>>,
) {
    // Build ONCE. This system used to run every frame with no guard: it baked
    // 12 tree meshes, rebuilt every building material and replaced the
    // `PlacementAssets` resource each frame. Measured over 240 frames: meshes
    // 13 -> 3242, materials 15 -> 3411, all leaked — and every chunk kept
    // handles to a resource that was already gone.
    if existing.is_some() {
        return;
    }
    // Headless tests run without the render assets stores; placement content
    // stays disabled for them (plan_chunk still works).
    //
    // R3.9: no `AssetServer` needed any more. Trees are baked procedurally
    // from `tree_shape`, not loaded from glTF — which also means the world
    // gets trees in a headless test, so streaming is actually testable.
    //
    // The season material store is a separate requirement: it only exists once
    // `SeasonPlugin` has registered the `MaterialPlugin` (which headless tests
    // do add, so trees stay testable). Waiting for it — instead of returning
    // after the meshes have been allocated — is what keeps this system
    // idempotent; returning late would leak a fresh set of meshes every frame.
    let (
        Some(mut meshes),
        Some(mut materials),
        Some(mut facade_materials),
        Some(mut tree_materials),
    ) = (meshes, materials, facade_materials, tree_materials)
    else {
        return;
    };
    let palette = placement::StylePalette::default();
    let mut wear_color = |rgb: [f32; 3], bucket: usize| {
        // Wear grades: pristine keeps the style colour; higher buckets
        // darken + desaturate (soot/faded paint) and get slightly glossier
        // grime sheen. Same recipe per bucket so types stay comparable.
        let t = bucket as f32 / (placement::WEAR_BUCKETS - 1) as f32;
        let luma = rgb[0] * 0.3 + rgb[1] * 0.6 + rgb[2] * 0.1;
        let desat = 0.25 * t;
        let darken = 1.0 - 0.35 * t;
        let mixed = |c: f32| (luma * desat + c * (1.0 - desat)) * darken;
        materials.add(StandardMaterial {
            base_color: Color::srgb(mixed(rgb[0]), mixed(rgb[1]), mixed(rgb[2])),
            perceptual_roughness: 0.9 - 0.15 * t,
            ..default()
        })
    };
    let plaza_material = wear_color(palette.plaza, 1);

    // R3.11 step 11.3: roofs. One dark material per style (the roofline reads as
    // a silhouette; see `world::roof`), and the two unit meshes the styles need.
    let roofs = build_roof_assets(&mut meshes, &mut materials);

    // R3.11: the wall is a facade texture now. Bake three maps per (style,
    // variant) once — 6 bakes of 1024^2 + 2 x 512^2, a few tens of MB and one
    // startup hitch nobody sees, because the world is already streaming behind
    // the menu — then build one material per (style, variant, wear bucket).
    //
    // The tint keeps the R3.6 wear recipe: the maps carry the pattern and stay
    // neutral, the tint carries the style and its grime grade.
    let facade_materials_out =
        build_facade_materials(&mut facade_materials, images.as_deref_mut(), &palette);
    // R3.9: trees are procedural, baked to one mesh per (species, variant).
    // 3 species x 4 variants = 12 meshes for the whole world, reused by every
    // chunk that spawns a tree.
    // Each entry is (wood, leaves): two meshes per tree, because the branches
    // stay opaque while the foliage cards are alpha-tested. See
    // `world::tree_mesh`'s header for why they cannot share a material.
    let per_lod = TREE_PROFILES.len() * TreeMeshKey::VARIANTS as usize;
    let mut tree_meshes: Vec<(Option<Handle<Mesh>>, Handle<Mesh>)> =
        Vec::with_capacity(per_lod * TreeLod::ALL.len());
    for lod in TreeLod::ALL {
        for species in 0..TREE_PROFILES.len() as u8 {
            for variant in 0..TreeMeshKey::VARIANTS {
                let parts = bake_tree_mesh_lod(TreeMeshKey { species, variant }, lod);
                // An empty branch mesh is a tier that carries no branches (Far);
                // shipping it as None keeps the mesh allocator quiet.
                let wood = if parts.wood.count_vertices() > 0 {
                    Some(meshes.add(parts.wood))
                } else {
                    None
                };
                tree_meshes.push((wood, meshes.add(parts.leaves)));
            }
        }
    }
    // R3.5: the season-aware material replaces the plain silhouette. Every
    // tree in the world shares this one handle, which is what makes a season
    // switch a single 32-byte uniform write instead of a walk over every tree
    // entity (ADR-0004 §6 "no per-material traversal").
    let tree_material = tree_materials.add(crate::plugins::season::TreeSeasonMaterial {
        base: crate::plugins::season::tree_base_material(),
        extension: Default::default(),
    });
    // The foliage cards need a texture: ONE shared species atlas (R3.10 — the
    // single shared material is what keeps a season switch a 32-byte uniform
    // write), whose cells each species' baked mesh samples by its own UVs.
    // An `Image` store only exists once the render plugins are up; a headless
    // test has none. Falling back to a texture-less card keeps the world
    // building there, where nothing is drawn anyway -- and the test that counts
    // tree entities is about streaming, not about what the crown looks like.
    let leaf_texture = images.as_deref_mut().map(|images| {
        let atlas =
            crate::world::leaf_shape::bake_species_atlas(crate::world::leaf_shape::ATLAS_HEIGHT);
        images.add(crate::world::leaf_shape::mask_to_image(&atlas))
    });
    let tree_leaf_material =
        tree_materials.add(crate::plugins::season::tree_leaf_material(leaf_texture));
    let unit_cube = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    let unit_cylinder = meshes.add(bevy::shape::Cylinder::new(0.5, 1.0));
    // R3.11 step 11.4: shared prop meshes and materials.
    let props = build_prop_assets(&mut meshes, &mut materials);

    info!(
        "placement assets built: {} tree meshes ({} LODs), {} facade materials          ({} styles x {} variants x {} wear buckets)",
        tree_meshes.len(),
        TreeLod::ALL.len(),
        facade_materials_out.len() * VARIANT_COUNT as usize * placement::WEAR_BUCKETS,
        facade_materials_out.len(),
        VARIANT_COUNT,
        placement::WEAR_BUCKETS
    );
    commands.insert_resource(PlacementAssets {
        facade_materials: facade_materials_out,
        roofs,
        plaza_material,
        tree_meshes,
        tree_material,
        tree_leaf_material,
        unit_cube,
        unit_cylinder,
        props,
    });
}

/// Spawns one building's roof (R3.11 step 11.3).
///
/// The roof sits on the wall top and is a child of the same chunk root, so it
/// despawns with the building. All three kinds are instances of shared meshes:
/// one gable and one sawtooth tooth for the whole city, and the shared unit
/// cube for the parapet's four walls.
///
/// Rotation matters as much as height here: the roof is baked with its ridge
/// along local +X, which after the yaw fix is the street direction, so the
/// gable ends face the side streets and the sawtooth teeth cut the skyline
/// across the frontage.
fn spawn_roof(
    parent: &mut ChildSpawnerCommands,
    roof: &RoofAssets,
    unit_cube: &Handle<Mesh>,
    object: &placement::PlacedObject,
) {
    let style = match object.kind {
        placement::PlacedKind::Building(kind) => FacadeStyle::of(kind).expect("roof of a building"),
        _ => return,
    };
    let profile = RoofProfile::of(style);
    let yaw = Quat::from_rotation_y(object.yaw);
    let (x, z) = (object.position.x, object.position.y);
    let (l, h, d) = (object.size.x, object.size.y, object.size.z);
    let rise = profile.rise_for(object.variant);

    match profile.kind {
        RoofKind::Gable => {
            // One scaled instance: ridge along local +X, eaves overhanging
            // local z (the overhang is baked into the unit mesh).
            parent.spawn((
                Name::new("Roof"),
                Mesh3d(roof.gable.clone()),
                MeshMaterial3d(roof.material.clone()),
                Transform::from_translation(Vec3::new(x, h, z))
                    .with_rotation(yaw)
                    .with_scale(Vec3::new(l, rise, d)),
            ));
        }
        RoofKind::Parapet => {
            // Four walls of the shared unit cube. The wall thickness stays
            // absolute: a baked ring would scale it with the building and end
            // up metres thick on a 40 m slab.
            const WALL: f32 = 0.25;
            let walls = [
                // front / back, spanning the frontage
                ([0.0, 0.0, (d - WALL) * 0.5], [l, rise, WALL]),
                ([0.0, 0.0, -(d - WALL) * 0.5], [l, rise, WALL]),
                // sides, spanning the depth between the front and back walls
                ([(l - WALL) * 0.5, 0.0, 0.0], [WALL, rise, d - WALL * 2.0]),
                ([-(l - WALL) * 0.5, 0.0, 0.0], [WALL, rise, d - WALL * 2.0]),
            ];
            for (local, scale) in walls {
                let offset = yaw * Vec3::new(local[0], local[1], local[2]);
                parent.spawn((
                    Name::new("Roof"),
                    Mesh3d(unit_cube.clone()),
                    MeshMaterial3d(roof.material.clone()),
                    Transform::from_translation(Vec3::new(x, h, z) + offset)
                        .with_rotation(yaw)
                        .with_scale(Vec3::new(scale[0], scale[1], scale[2])),
                ));
            }
        }
        RoofKind::Sawtooth => {
            // Teeth repeat along the FRONTAGE (local +X), so the roofline cuts
            // the sky across the street-facing elevation.
            let (count, width) = profile.teeth(l);
            for i in 0..count {
                let lx = -l * 0.5 + (i as f32 + 0.5) * width;
                // The unit tooth's base is at y = 0, so it sits directly on the
                // wall top. The old code also lifted it by `rise * 0.5` as if
                // the mesh were centre-origin, which floated the whole sawtooth
                // half a rise above the wall and left a band of sky between the
                // two -- the roof's "gap to the sky".
                let offset = yaw * Vec3::new(lx, 0.0, 0.0);
                parent.spawn((
                    Name::new("RoofTooth"),
                    Mesh3d(roof.tooth.clone()),
                    MeshMaterial3d(roof.material.clone()),
                    Transform::from_translation(Vec3::new(x, h, z) + offset)
                        .with_rotation(yaw)
                        .with_scale(Vec3::new(width, rise, d)),
                ));
            }
        }
    }
}

/// Visibility range that switches a lamp's emissive lens off past
/// [`crate::world::props::LAMP_GLOW_RANGE`].
///
/// Abrupt, not crossfaded: the lens is already under a pixel tall at that
/// distance, and a dithered fade would animate that sliver instead of removing
/// it (see the constant's note). Everything else in the world is drawn at
/// every distance the chunk window reaches.
fn lamp_glow_range() -> VisibilityRange {
    VisibilityRange::abrupt(0.0, crate::world::props::LAMP_GLOW_RANGE)
}

/// Spawns one prop entity (R3.11 step 11.4).
///
/// Two mesh conventions meet here: unit meshes (cube, cylinder) scale by
/// `object.size` and centre at `base + size.y/2`; baked meshes (awning,
/// balcony) carry real size with their mount point at the origin, so `base`
/// is the spawn height directly and `size` is used as the scale as-is.
fn spawn_prop(
    parent: &mut ChildSpawnerCommands,
    assets: &PlacementAssets,
    object: &placement::PlacedObject,
) {
    let placement::PlacedKind::Prop(kind) = object.kind else {
        return;
    };
    let props = &assets.props;
    let rotation = Quat::from_rotation_y(object.yaw);
    let (name, mesh, material, y, scale) = match kind {
        PropKind::Awning => (
            "Awning",
            props.awning.clone(),
            props.awning_canvas[object.variant as usize % 2].clone(),
            object.base,
            object.size,
        ),
        PropKind::AcUnit => (
            "AcUnit",
            assets.unit_cube.clone(),
            // A fresh condenser is beige plastic; the rust variant takes
            // over on worn buildings (the same variant bit as the fixtures).
            if object.variant % 2 == 1 {
                props.rusted.clone()
            } else {
                props.ac.clone()
            },
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::Downspout => (
            "Downspout",
            assets.unit_cylinder.clone(),
            props.fixture(object.variant),
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::Balcony => (
            "Balcony",
            props.balcony.clone(),
            props.fixture(0),
            object.base,
            object.size,
        ),
        PropKind::WaterTank => (
            "WaterTank",
            assets.unit_cylinder.clone(),
            props.fixture(0),
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::Chimney => (
            "Chimney",
            assets.unit_cylinder.clone(),
            props.brick.clone(),
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::StairBox => (
            "StairBox",
            assets.unit_cube.clone(),
            props.fixture(0),
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::RoofVent => (
            "RoofVent",
            assets.unit_cylinder.clone(),
            props.fixture(0),
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::Pole => (
            "Pole",
            assets.unit_cylinder.clone(),
            props.fixture(0),
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::LampArm => (
            "LampArm",
            props.lamp_arm.clone(),
            props.fixture(0),
            object.base,
            object.size,
        ),
        PropKind::LampHead => (
            "LampHead",
            props.lamp_head.clone(),
            props.lamp_glow.clone(),
            object.base,
            object.size,
        ),
        PropKind::CrossArm => (
            "CrossArm",
            props.crossarm.clone(),
            props.fixture(0),
            object.base,
            object.size,
        ),
        PropKind::Wire => (
            "Wire",
            props.wire.clone(),
            props.wire_dark.clone(),
            object.base,
            // x spans pole to pole; y/z carry the span-proportional sag and
            // keep the cable near-round (plan side sizes it, see bake_wire).
            object.size,
        ),
        // R3.13: the cross-street layer. Cables keep their z scale at 1 so the
        // 2.5 cm section survives an 8-12 m span (the along-street run scales
        // both y and z by span/30, which would thin a short drop to 6 mm).
        PropKind::CrossWire => (
            "CrossWire",
            props.cross_wire.clone(),
            props.wire_dark.clone(),
            object.base,
            object.size,
        ),
        PropKind::LongLampArm => (
            "LongLampArm",
            props.long_lamp_arm.clone(),
            props.fixture(0),
            object.base,
            object.size,
        ),
        PropKind::LongLampHead => (
            "LongLampHead",
            props.long_lamp_head.clone(),
            props.lamp_glow.clone(),
            object.base,
            object.size,
        ),
        // Scaffold parts are planted parts: the posts stand on the ground, the
        // beams and deck sit at their own clear heights.
        PropKind::ScaffoldPost => (
            "ScaffoldPost",
            assets.unit_cylinder.clone(),
            props.fixture(0),
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::ScaffoldBeam => (
            "ScaffoldBeam",
            assets.unit_cube.clone(),
            props.fixture(object.variant),
            object.base + object.size.y * 0.5,
            object.size,
        ),
        PropKind::ScaffoldDeck => (
            "ScaffoldDeck",
            assets.unit_cube.clone(),
            props.plank.clone(),
            object.base + object.size.y * 0.5,
            object.size,
        ),
    };
    let mut spawned = parent.spawn((
        Name::new(name),
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_translation(Vec3::new(object.position.x, y, object.position.y))
            .with_rotation(rotation)
            .with_scale(scale),
    ));
    // The two emissive lenses are the only subpixel-bright surfaces in the
    // world, and the only props with a draw distance: a bright sliver that
    // small does not fade out, it blinks (see `LAMP_GLOW_RANGE`).
    if matches!(kind, PropKind::LampHead | PropKind::LongLampHead) {
        spawned.insert(lamp_glow_range());
    }
}

/// Spawns a chunk's planned content under its root: buildings as BoxMesh
/// placeholders (ADR-0004 §4; R3.6 adds wear materials), trees as glTF
/// scenes, the plaza pad as a thin box. Called inside the streaming system so
/// a chunk's contents exist in the same frame its collision is registered.
fn spawn_chunk_content(
    commands: &mut Commands,
    assets: &PlacementAssets,
    root: Entity,
    plan: &placement::ChunkPlan,
) {
    for object in &plan.objects {
        match object.kind {
            placement::PlacedKind::Building(kind) => {
                let style = match kind {
                    BlockType::OldCity => 0,
                    BlockType::Apartment => 1,
                    _ => 2,
                };
                let roof = &assets.roofs[style];
                let material = &assets.facade_materials[style]
                    [object.variant as usize % VARIANT_COUNT as usize]
                    [placement::wear_bucket(object.wear)];
                let mesh = assets.unit_cube.clone();
                let position = Vec3::new(object.position.x, object.size.y * 0.5, object.position.y);
                let rotation = Quat::from_rotation_y(object.yaw);
                commands.entity(root).with_children(|parent| {
                    parent.spawn((
                        Name::new(format!(
                            "Building@{:?} wear={:.2}",
                            object.position, object.wear
                        )),
                        Mesh3d(mesh),
                        MeshMaterial3d(material.clone()),
                        Transform::from_translation(position)
                            .with_rotation(rotation)
                            .with_scale(object.size),
                    ));

                    // R3.11 step 11.3: the roof. The silhouette is made of
                    // building tops watched from the shoulders up, and the wall
                    // texture stops at the parapet.
                    spawn_roof(parent, roof, &assets.unit_cube, object);
                });
            }
            placement::PlacedKind::Prop(_) => {
                // R3.11 step 11.4: facade / rooftop props.
                commands.entity(root).with_children(|parent| {
                    spawn_prop(parent, assets, object);
                });
            }
            placement::PlacedKind::PlazaPad => {
                let mesh = assets.unit_cube.clone();
                let position = Vec3::new(object.position.x, object.size.y * 0.5, object.position.y);
                commands.entity(root).with_children(|parent| {
                    parent.spawn((
                        Name::new("PlazaPad"),
                        Mesh3d(mesh),
                        MeshMaterial3d(assets.plaza_material.clone()),
                        Transform::from_translation(position).with_scale(object.size),
                    ));
                });
            }
            _ => {
                // Trees (street/forest/plaza): one baked mesh per LOD tier
                // (R3.9 step 4). All three sit at the same place; each carries
                // a `VisibilityRange` so exactly one is drawn at any camera
                // distance. Foliage is ~85-90% of a tree, so the tiers differ
                // mostly in how many leaf clusters survive: measured 2952
                // triangles near, 1238 mid (-58%), 183 far (-94%).
                let Some(tree) = object.tree else { continue };
                let transform = Transform::from_xyz(object.position.x, 0.0, object.position.y)
                    .with_rotation(Quat::from_rotation_y(tree.yaw))
                    .with_scale(Vec3::splat(tree.scale));

                commands.entity(root).with_children(|parent| {
                    for lod in TreeLod::ALL {
                        let Some((wood, leaves)) = assets.tree_mesh(tree.asset, object.wear, lod)
                        else {
                            continue;
                        };
                        // Two entities per tier: the branches keep the opaque
                        // silhouette material, the cards take the alpha-tested
                        // leaf material. Same transform, so they stay together.
                        let suffix = match lod {
                            TreeLod::Near => "",
                            TreeLod::Mid => "Mid",
                            TreeLod::Far => "Far",
                        };
                        // The wood entity carries the tier's canonical name
                        // (`Tree` / `TreeMid` / `TreeFar`) so the LOD and
                        // streaming tests keep working; the leaf entity is
                        // named alongside it. A tier with no branches (Far)
                        // skips the wood entity entirely and hands its name to
                        // the leaves, which is what the LOD tests match on.
                        let mut parts: Vec<(
                            String,
                            Handle<Mesh>,
                            &Handle<crate::plugins::season::TreeSeasonMaterial>,
                        )> = Vec::with_capacity(2);
                        if let Some(wood) = wood {
                            parts.push((
                                format!("Tree{suffix}"),
                                wood.clone(),
                                &assets.tree_material,
                            ));
                        }
                        parts.push((
                            if wood.is_some() {
                                format!("TreeLeaves{suffix}")
                            } else {
                                format!("Tree{suffix}")
                            },
                            leaves.clone(),
                            &assets.tree_leaf_material,
                        ));
                        for (part_name, mesh, material) in parts {
                            // Every tier carries a range, including `Near`: near
                            // has no start fade but needs an end margin to dither
                            // out across 40-48 m, which is the first and most
                            // visible swap.
                            parent.spawn((
                                Name::new(part_name),
                                Mesh3d(mesh.clone()),
                                MeshMaterial3d(material.clone()),
                                transform,
                                lod.range(),
                            ));
                        }
                    }
                });
            }
        }
    }
}

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<crate::world::explored::ExploredBitmap>()
            // The tree material store is registered here rather than by
            // `SeasonPlugin`: `Assets<T>` must exist before
            // `build_placement_assets` runs, and the headless tests build the
            // world without the render plugins. Registering the asset type
            // twice is a no-op, so the real app (which also has the
            // `MaterialPlugin` that needs it) is unaffected.
            .init_resource::<Assets<crate::plugins::season::TreeSeasonMaterial>>()
            // Same arrangement for the facade material: `MaterialPlugin` lives
            // in the render app (see `plugins::facade::FacadePlugin`), while
            // `Assets<FacadeMaterial>` must exist before
            // `build_placement_assets` runs — including in headless tests,
            // which have no renderer at all.
            .init_resource::<Assets<FacadeMaterial>>()
            .add_message::<crate::world::explored::PlayerChunkChanged>()
            .add_systems(Startup, generate_layout)
            .add_systems(PreUpdate, build_placement_assets)
            .add_systems(
                Update,
                (
                    update_chunk_streaming
                        .in_set(WorldSystems::Streaming)
                        // The controller writes the camera Transform; streaming
                        // reads it, so the window is computed from the settled
                        // position (explicit, per root AGENTS.md).
                        .after(crate::player::PlayerSystems::Move),
                    // Exploration follows the settled position too, after
                    // streaming (it reads the same camera transform).
                    crate::world::explored::track_player_chunk.after(WorldSystems::Streaming),
                ),
            );
    }
}

/// Convenience accessor for tests and the debug visualization: the layout
/// graph resource generated at startup.
pub fn layout_resource(world: &World) -> Option<&LayoutGraph> {
    world.get_resource::<LayoutGraph>()
}

/// Sanity check for tests: the world fits the chunk grid.
#[cfg(test)]
mod tests {
    use super::*;

    /// World centre, grounded on the actual constants under test.
    const CENTER: f32 = crate::world::WORLD_SIZE * 0.5;

    #[test]
    fn chunk_grid_tiles_the_world() {
        use crate::world::WORLD_SIZE;
        assert_eq!(CHUNK_SIZE, 50.0);
        assert_eq!(
            (WORLD_SIZE / CHUNK_SIZE) as u32,
            GRID_CELLS,
            "16x16 chunks tile the 800 m world"
        );
    }

    #[test]
    fn desired_chunks_follow_the_walk_direction() {
        // Player at the world center facing -Z (north): the window is the
        // original's square scan — 4 ahead, 2 behind (Chebyshev).
        let pos = Vec2::new(CENTER, CENTER);
        let forward = Vec2::new(0.0, -1.0);
        let desired = ChunkManager::desired_chunks(pos, forward);

        let player = ChunkCoords::from_world(pos);
        assert_eq!(player, ChunkCoords { x: 8, z: 8 });
        // Square scan: rows are 2 behind + own + 4 ahead. Columns whose
        // centre sits exactly abeam have dot == 0 and count as "ahead", so the
        // window is symmetric on both axes near the centre of a chunk.
        assert!(
            (49..=63).contains(&desired.len()),
            "unexpected footprint size: {}",
            desired.len()
        );

        for chunk in &desired {
            let dx = chunk.x - player.x;
            let dz = chunk.z - player.z;
            let ahead = (chunk.center() - pos).dot(forward) >= 0.0;
            let radius = if ahead { CHUNK_AHEAD } else { CHUNK_BEHIND };
            assert!(
                dx.abs().max(dz.abs()) <= radius,
                "chunk {chunk:?} outside the chebyshev radius {radius}"
            );
        }
        // The own chunk is always present.
        assert!(desired.contains(&player));
    }

    #[test]
    fn desired_chunks_shift_with_facing() {
        let pos = Vec2::new(CENTER, CENTER);
        let player = ChunkCoords::from_world(pos);
        // Facing +X (east): the window reaches CHUNK_AHEAD east, less west.
        let east = ChunkManager::desired_chunks(pos, Vec2::new(1.0, 0.0));
        assert!(east.iter().any(|c| c.x == player.x + CHUNK_AHEAD));
        assert!(!east.iter().any(|c| c.x == player.x + CHUNK_AHEAD + 1));
        assert!(!east.iter().any(|c| c.x == player.x - CHUNK_AHEAD));

        // Turning 180° mirrors it.
        let west = ChunkManager::desired_chunks(pos, Vec2::new(-1.0, 0.0));
        assert!(west.iter().any(|c| c.x == player.x - CHUNK_AHEAD));
        assert!(!west.iter().any(|c| c.x == player.x + CHUNK_AHEAD));
    }

    #[test]
    fn desired_chunks_never_leave_the_world() {
        // Player in the north-west corner: no chunk index may go negative or
        // past the 40x40 grid.
        let desired = ChunkManager::desired_chunks(Vec2::new(10.0, 10.0), Vec2::Y);
        assert!(!desired.is_empty());
        for chunk in &desired {
            assert!((0..GRID_CELLS as i32).contains(&chunk.x), "{chunk:?}");
            assert!((0..GRID_CELLS as i32).contains(&chunk.z), "{chunk:?}");
        }
    }

    #[test]
    fn tile_color_is_deterministic_per_chunk() {
        let a = tile_color(ChunkCoords { x: 3, z: 4 });
        let b = tile_color(ChunkCoords { x: 3, z: 4 });
        assert_eq!(a, b);
        assert_ne!(
            tile_color(ChunkCoords { x: 3, z: 4 }),
            tile_color(ChunkCoords { x: 4, z: 3 })
        );
    }

    /// Headless streaming test: chunk groups spawn/despawn as the camera
    /// moves (R2 verification: "移动时周围物体出现/消失").
    #[test]
    fn chunk_streaming_spawns_and_despawns() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>();

        // A camera entity (data only; no render plugins needed).
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));

        app.update(); // Startup: layout generated; Update: chunks spawned.

        // No camera movement yet: chunk set computed from the spawn position.
        app.update();
        let manager = app.world().resource::<ChunkManager>();
        assert!(!manager.active.is_empty(), "chunks spawn at startup");

        // Teleport the camera 300 m east (still 6 chunks from the east edge)
        // and update twice: the old chunks despawn, new ones spawn.
        let mut cameras = app
            .world_mut()
            .query_filtered::<&mut Transform, With<Camera3d>>();
        for mut transform in cameras.iter_mut(app.world_mut()) {
            transform.translation = Vec3::new(CENTER + 300.0, 1.6, CENTER);
        }
        app.update();
        app.update();

        let manager = app.world().resource::<ChunkManager>();
        let all_x: Vec<i32> = manager.active.keys().map(|k| k.0).collect();
        assert!(
            all_x.iter().all(|x| *x >= 10),
            "chunks behind the teleport should have despawned: {all_x:?}"
        );
        // The active set matches the footprint once the per-frame budget has
        // drained the queue (CHUNKS_PER_FRAME = 1, so give it time).
        let expected =
            ChunkManager::desired_chunks(Vec2::new(CENTER + 300.0, CENTER), Vec2::new(0.0, -1.0))
                .len();
        for _ in 0..(expected + 8) {
            app.update();
        }
        let manager = app.world().resource::<ChunkManager>();
        assert_eq!(manager.active.len(), expected, "active set should converge");
    }

    #[test]
    fn chunk_root_can_propagate_transforms_to_its_children() {
        // Regression: `ChunkRoot` was spawned with only `Name`/`ChunkRoot`/
        // `coords`, so the hierarchy had a parent without `Transform`. Every
        // child (ground tile, buildings, trees) failed to receive a
        // `GlobalTransform` and rendered nothing — while the gizmo overlay,
        // which bypasses the hierarchy, kept drawing. Bevy logged B0004 once
        // per child: 1120 of them in a 25 s run.
        //
        // The whole rendered world hangs off these roots, so assert the parent
        // can actually carry a transform rather than just checking a count.
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<crate::plugins::season::TreeSeasonMaterial>>();
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));
        for _ in 0..30 {
            app.update();
        }

        let mut roots = app
            .world_mut()
            .query_filtered::<Entity, (With<ChunkRoot>, Without<Transform>)>();
        let missing: Vec<Entity> = roots.iter(app.world()).collect();
        assert!(
            missing.is_empty(),
            "{} chunk roots have no Transform; their children cannot be placed              in the world and will not render",
            missing.len()
        );

        // And the children really do have a GlobalTransform after propagation.
        let mut children = app
            .world_mut()
            .query_filtered::<&Name, (Without<GlobalTransform>, With<ChildOf>)>();
        let unplaced: Vec<String> = children
            .iter(app.world())
            .map(|n| n.as_str().to_string())
            .collect();
        assert!(
            unplaced.is_empty(),
            "{} children never got a GlobalTransform, e.g. {:?}",
            unplaced.len(),
            &unplaced[..unplaced.len().min(5)]
        );
    }

    #[test]
    fn every_tree_spawns_all_lod_tiers_with_ranges() {
        // LOD is wired per entity, and a missing tier is invisible: the tree
        // simply vanishes between two boundaries. Assert the structure, not
        // that "a tree exists".
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<crate::plugins::season::TreeSeasonMaterial>>();
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));
        for _ in 0..30 {
            app.update();
        }

        let count = |app: &mut App, name: &'static str| -> usize {
            let mut query = app.world_mut().query::<&Name>();
            query
                .iter(app.world())
                .filter(|n| n.as_str() == name)
                .count()
        };
        let near = count(&mut app, "Tree");
        let mid = count(&mut app, "TreeMid");
        let far = count(&mut app, "TreeFar");
        assert!(near > 0, "no trees spawned");
        assert_eq!(near, mid, "every tree needs a mid tier");
        assert_eq!(near, far, "every tree needs a far tier");

        // All three tiers carry ranges, near included: near has no start fade
        // but needs an end margin to dissolve into mid across 40-48 m. Before
        // the crossfade this first swap was a hard pop.
        for (name, expected) in [
            ("Tree", TreeLod::Near),
            ("TreeMid", TreeLod::Mid),
            ("TreeFar", TreeLod::Far),
        ] {
            let mut query = app
                .world_mut()
                .query::<(&Name, Option<&bevy::camera::visibility::VisibilityRange>)>();
            let mut checked = 0usize;
            for (entity_name, range) in query.iter(app.world()) {
                if entity_name.as_str() != name {
                    continue;
                }
                let range = range.unwrap_or_else(|| panic!("{name} has no VisibilityRange"));
                // `VisibilityRange` has no Debug impl, so compare the parts.
                let want = expected.range();
                assert_eq!(range.start_margin, want.start_margin, "{name} start margin");
                assert_eq!(range.end_margin, want.end_margin, "{name} end margin");
                checked += 1;
            }
            assert_eq!(checked, near, "{name} is missing on some trees");
        }
    }

    #[test]
    fn only_the_lamp_lenses_have_a_draw_distance() {
        // A lamp's lens is a 3 cm emissive sliver: past the range it is a
        // subpixel dot whose coverage flips between frames, which reads as a
        // twinkle. Everything else in a chunk -- poles, wires, awnings, the
        // scaffold deck -- is drawn at every distance the chunk window reaches,
        // so the range has to stay on the two lenses and nowhere else.
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<crate::plugins::season::TreeSeasonMaterial>>();
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));
        for _ in 0..30 {
            app.update();
        }

        let mut query = app.world_mut().query::<(&Name, Option<&VisibilityRange>)>();
        let mut lenses = 0usize;
        for (name, range) in query.iter(app.world()) {
            let name = name.as_str();
            let is_lens = name == "LampHead" || name == "LongLampHead";
            // Tree tiers carry their own ranges (40/110/220 m) and are not
            // props; everything else must be unconditionally drawn.
            let is_tree = name.starts_with("Tree");
            match (is_lens, range) {
                (true, Some(range)) => {
                    let want = crate::world::props::LAMP_GLOW_RANGE;
                    assert_eq!(range.start_margin, 0.0..0.0, "{name} start");
                    assert_eq!(range.end_margin, want..want, "{name} end");
                    assert!(
                        range.is_abrupt(),
                        "{name} dithers its last metres: a sub-pixel lens strung out \
                         over a fade band only flickers differently"
                    );
                    lenses += 1;
                }
                (true, None) => panic!("{name} has no draw distance and will twinkle"),
                (false, Some(_)) if is_tree => {}
                (false, Some(_)) => panic!("{name} was given a draw distance"),
                (false, None) => {}
            }
        }
        assert!(lenses > 0, "no lamp lenses spawned in the shoulder chunks");
    }

    #[test]
    fn placement_assets_and_meshes_are_allocated_once() {
        // Regression: `build_placement_assets` ran every frame with no guard.
        // It re-baked 12 tree meshes, rebuilt every building material and
        // replaced the `PlacementAssets` resource each frame — measured over
        // 240 frames: meshes 13 -> 3242, materials 15 -> 3411. Buildings also
        // allocated a fresh `Cuboid` per object on every chunk load.
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<crate::plugins::season::TreeSeasonMaterial>>();
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));

        for _ in 0..60 {
            app.update();
        }
        let meshes_after_warmup = app.world().resource::<Assets<Mesh>>().len();
        let materials_after_warmup = app.world().resource::<Assets<StandardMaterial>>().len();
        let facades_after_warmup = app.world().resource::<Assets<FacadeMaterial>>().len();

        // Run a lot more frames with no world change: nothing may be
        // allocated.
        for _ in 0..120 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<Assets<Mesh>>().len(),
            meshes_after_warmup,
            "mesh assets grow every frame; the placement assets are being rebuilt"
        );
        assert_eq!(
            app.world().resource::<Assets<StandardMaterial>>().len(),
            materials_after_warmup,
            "material assets grow every frame"
        );
        assert_eq!(
            app.world().resource::<Assets<FacadeMaterial>>().len(),
            facades_after_warmup,
            "facade materials grow every frame; the facade bake ran more than once"
        );
    }

    /// The facade materials must be exactly `styles x variants x wear buckets`.
    ///
    /// A per-building material is the failure this guards: it would still look
    /// correct and would put a streaming window into thousands of draw calls,
    /// which no unit test would notice unless it counts the materials.
    #[test]
    fn facades_are_shared_not_built_per_building() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<crate::plugins::season::TreeSeasonMaterial>>();
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));
        for _ in 0..12 {
            app.update();
        }

        let assets = app.world().resource::<PlacementAssets>();
        let mut unique = std::collections::HashSet::new();
        for style in &assets.facade_materials {
            for variant in style {
                for handle in variant {
                    unique.insert(handle.id());
                }
            }
        }
        let expected = FacadeStyle::ALL.len() * VARIANT_COUNT as usize * placement::WEAR_BUCKETS;
        assert_eq!(
            unique.len(),
            expected,
            "expected {expected} distinct facade materials (styles x variants x buckets)"
        );
        // And the store agrees: the only other entries are the plaza material.
        assert_eq!(
            app.world().resource::<Assets<FacadeMaterial>>().len(),
            expected
        );
    }

    #[test]
    fn procedural_trees_stream_with_their_chunks() {
        // R3.9 regression: the first tree implementation was a one-shot debug
        // gallery pinned at a fixed spot, so walking away left the world
        // treeless until the player toggled it off and on again. Trees must
        // be part of chunk content and despawn with their chunk root.
        use bevy::mesh::Indices;
        use bevy::prelude::Mesh3d;

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<crate::plugins::season::TreeSeasonMaterial>>();
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));

        // `build_placement_assets` runs in PreUpdate; the first update brings
        // up the layout, later ones stream chunks in.
        for _ in 0..12 {
            app.update();
        }

        let trees_near = count_tree_entities(&mut app);
        assert!(
            trees_near > 0,
            "no procedural trees spawned around the spawn point"
        );

        // Every tree must be a single baked mesh, not one entity per branch.
        let single_mesh_trees = {
            let mut query = app.world_mut().query::<(&Mesh3d, &Name)>();
            query
                .iter(app.world())
                .filter(|(_, name)| name.as_str() == "Tree")
                .count()
        };
        assert_eq!(
            single_mesh_trees, trees_near,
            "each tree should be exactly one Mesh3d entity"
        );

        // Baked trees must be real geometry (a shared handle could still be
        // an empty mesh if the bake silently failed).
        {
            let handle = {
                let mut query = app.world_mut().query::<&Mesh3d>();
                query
                    .iter(app.world())
                    .next()
                    .map(|m| m.0.clone())
                    .expect("a tree mesh handle")
            };
            let meshes = app.world().resource::<Assets<Mesh>>();
            let mesh = meshes.get(&handle).expect("tree mesh exists");
            let tris = match mesh.indices() {
                Some(Indices::U32(i)) => i.len() / 3,
                Some(Indices::U16(i)) => i.len() / 3,
                None => 0,
            };
            assert!(tris > 0, "baked tree mesh is empty");
        }

        // Teleport far enough that the original chunks leave the window.
        let mut cameras = app
            .world_mut()
            .query_filtered::<&mut Transform, With<Camera3d>>();
        for mut transform in cameras.iter_mut(app.world_mut()) {
            transform.translation = Vec3::new(CENTER + 300.0, 1.6, CENTER);
        }
        for _ in 0..80 {
            app.update();
        }

        // The world stays treed at the new location, and no tree survives at
        // the old one: the tree set follows the player rather than sitting
        // still like the old gallery.
        let trees_far = count_tree_entities(&mut app);
        assert!(
            trees_far > 0,
            "no trees after walking away; trees are not streaming with chunks"
        );

        // Every tree must belong to an ACTIVE chunk root. Counting trees that
        // merely have a `ChildOf` misses orphans entirely — the earlier
        // version of this check passed even with trees spawned unparented,
        // which is exactly the "pinned gallery" failure mode.
        let manager = app.world().resource::<ChunkManager>();
        let live_roots: std::collections::HashSet<Entity> =
            manager.active.values().copied().collect();

        let mut query = app
            .world_mut()
            .query::<(&Transform, &Name, Option<&ChildOf>)>();
        let mut orphaned = 0usize;
        let mut behind = 0usize;
        for (transform, name, parent) in query.iter(app.world()) {
            if name.as_str() != "Tree" {
                continue;
            }
            match parent {
                Some(parent) if live_roots.contains(&parent.parent()) => {}
                _ => orphaned += 1,
            }
            // Trees left standing in the abandoned half of the map mean the
            // set is not following the player.
            if transform.translation.x <= CENTER {
                behind += 1;
            }
        }
        assert_eq!(
            orphaned, 0,
            "{orphaned} trees are not attached to an active chunk root"
        );
        assert_eq!(
            behind, 0,
            "{behind} trees are still standing in the abandoned area"
        );
    }

    fn count_tree_entities(app: &mut App) -> usize {
        let mut query = app.world_mut().query::<&Name>();
        query
            .iter(app.world())
            .filter(|name| name.as_str() == "Tree")
            .count()
    }

    #[test]
    fn player_chunk_changes_are_tracked_and_explored() {
        use crate::world::explored::{ExploredBitmap, PlayerChunkChanged};
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>();
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));

        app.update();
        app.update();

        let bitmap = app.world().resource::<ExploredBitmap>();
        assert!(
            bitmap.is_explored((8, 8)),
            "the starting chunk counts as explored"
        );
        assert!(!bitmap.is_empty());

        // Teleport two chunks east: the change message fires with the old
        // chunk recorded.
        let mut cameras = app
            .world_mut()
            .query_filtered::<&mut Transform, With<Camera3d>>();
        for mut transform in cameras.iter_mut(app.world_mut()) {
            transform.translation = Vec3::new(CENTER + 120.0, 1.6, CENTER);
        }
        app.update();
        app.update();

        let messages: Vec<_> = app
            .world()
            .resource::<bevy::ecs::message::Messages<PlayerChunkChanged>>()
            .iter_current_update_messages()
            .copied()
            .collect();
        let last = messages.last().expect("a chunk change was sent");
        assert_eq!(last.new_chunk, (10, 8));
        assert_eq!(last.old_chunk, Some((8, 8)));

        let bitmap = app.world().resource::<ExploredBitmap>();
        assert!(bitmap.is_explored((10, 8)), "the new chunk is explored");
        assert!(bitmap.is_explored((8, 8)), "the old chunk stays explored");
    }

    #[test]
    fn layout_resource_is_available_after_generation() {
        use crate::world::WORLD_SIZE;
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::WorldPlugin))
            .init_resource::<ButtonInput<KeyCode>>();
        app.world_mut().spawn((
            Camera3d::default(),
            Transform::from_xyz(CENTER, 1.6, CENTER),
        ));
        app.update();
        let graph = layout_resource(app.world()).expect("layout graph resource");
        assert!(!graph.blocks.is_empty());
        assert!(!graph.edges.is_empty());
        assert!((graph.bounds.x - WORLD_SIZE).abs() < 1e-3);
    }
}
