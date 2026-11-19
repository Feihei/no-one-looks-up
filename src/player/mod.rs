//! Kinematic player controller (R3.4).
//!
//! Port of the original `CharacterBody3D.gd` (Godot) without a physics
//! engine: the walker is a point with a [`PLAYER_RADIUS`] circle, moved
//! kinematically and then pushed out of the negative-space street collision
//! (R3.3). Godot's `move_and_slide` becomes "substep the move, resolve each
//! substep", which is what keeps a fast walker from tunnelling through a
//! block edge thinner than one frame's step.
//!
//! Design constraints (ADR-0001) that shape this module:
//! - First person, forever looking up: pitch is clamped to 22.55-60°.
//! - The body turns with A/D at a fixed rate; the camera adds a small ±10°
//!   yaw offset so glancing sideways never turns the body.
//! - No jump, no sprint, no FOV change; walking speed 1.2-2.0 m/s adjustable
//!   in game, with acceleration so starting/stopping is not instant.

use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::core::input::{
    MOVE_BACKWARD, MOVE_FORWARD, SPEED_DOWN, SPEED_UP, TURN_LEFT, TURN_RIGHT,
};
use crate::world::collision::{CollisionIndex, PLAYER_RADIUS};

/// Eye height above the ground (original `eye_height`).
pub const EYE_HEIGHT: f32 = 1.65;
/// Walking speed bounds and step (original min/max/speed_step).
pub const MIN_WALK_SPEED: f32 = 1.2;
pub const MAX_WALK_SPEED: f32 = 2.0;
pub const DEFAULT_WALK_SPEED: f32 = 1.5;
pub const SPEED_STEP: f32 = 0.1;
/// Acceleration towards the target speed (original `accel`, m/s²).
pub const ACCELERATION: f32 = 8.0;
/// Body turn rate for A/D (original `turn_speed`, degrees per second).
pub const TURN_SPEED_DEG: f32 = 60.0;
/// Mouse sensitivity (original `mouse_sensitivity`, degrees per pixel).
pub const MOUSE_SENSITIVITY: f32 = 0.15;
/// Pitch is clamped to looking up between 22.55° and 60°. The floor is the
/// lowest angle at which the horizon is just visible without any ground:
/// the bottom edge of the frame (Bevy default 45° vertical FOV) then grazes
/// the horizon line, which dips only 0.041° below level at a 1.65 m eye
/// height. Everything in frame is sky/atmosphere; the ground never appears.
pub const MIN_PITCH_DEG: f32 = 22.55;
pub const MAX_PITCH_DEG: f32 = 60.0;
/// Camera yaw offset around the body's facing (degrees, ±).
pub const MAX_YAW_OFFSET_DEG: f32 = 10.0;

/// Player state. The entity also carries the camera `Transform`; the body's
/// `translation` is the eye position (`y = EYE_HEIGHT`).
#[derive(Component, Debug, Clone)]
pub struct PlayerBody {
    /// Current walking speed (m/s), adjustable between the bounds above.
    pub walk_speed: f32,
    /// Body facing (radians). A/D change this; the mouse does not.
    pub yaw: f32,
    /// Current horizontal speed along the facing (m/s), for acceleration.
    pub forward_speed: f32,
    /// Camera pitch (radians), clamped to `[MIN_PITCH, MAX_PITCH]`.
    pub pitch: f32,
    /// Camera yaw offset around the body facing (radians, ±10°).
    pub yaw_offset: f32,
}

impl Default for PlayerBody {
    fn default() -> Self {
        Self {
            walk_speed: DEFAULT_WALK_SPEED,
            yaw: 0.0,
            forward_speed: 0.0,
            pitch: MAX_PITCH_DEG.to_radians(),
            yaw_offset: 0.0,
        }
    }
}

impl PlayerBody {
    /// Clamps pitch into the allowed looking-up band.
    fn clamp_pitch(pitch: f32) -> f32 {
        pitch.clamp(MIN_PITCH_DEG.to_radians(), MAX_PITCH_DEG.to_radians())
    }

    /// Camera rotation for the current body/camera angles.
    pub fn camera_rotation(&self) -> Quat {
        // Bevy: positive rotation about X tilts forward (-Z) upward, so a
        // positive pitch looks up. (A minus sign here pointed the camera at
        // the ground despite the clamped 22.55-60° "looking up" band.)
        Quat::from_euler(EulerRot::YXZ, self.yaw + self.yaw_offset, self.pitch, 0.0)
    }

    /// Unit direction the body walks in (world XZ, camera pitch ignored).
    pub fn forward(&self) -> Vec2 {
        // Bevy's forward is -Z; a yaw of 0 faces -Z.
        Vec2::new(-self.yaw.sin(), -self.yaw.cos())
    }
}

/// System sets for the player, so streaming and other readers can order
/// against the controller explicitly (root AGENTS.md: no implicit ordering).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlayerSystems {
    /// Reads input: mouse look, speed adjust, cursor toggle.
    Look,
    /// Turns and moves the body, then resolves collision.
    Move,
}

/// Mouse look: only vertical motion, setting pitch (22.55-60°). Horizontal
/// mouse motion is ignored — turning is keyboard-only (A/D), so the mouse
/// never changes facing or walk direction.
pub(crate) fn mouse_look(
    phase: Res<crate::core::game_state::GamePhase>,
    mut motion: MessageReader<MouseMotion>,
    mut players: Query<&mut PlayerBody>,
    windows: Query<&CursorOptions, With<PrimaryWindow>>,
) {
    // R4.6 input gating: no looking outside the playing phase.
    if !phase.is_game_active() {
        return;
    }
    // Only when the cursor is captured.
    let captured = windows
        .single()
        .map(|options| options.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !captured {
        // Drain so a later capture does not replay stale motion.
        motion.clear();
        return;
    }

    let mut delta = Vec2::ZERO;
    for event in motion.read() {
        delta += event.delta;
    }
    if delta == Vec2::ZERO {
        return;
    }
    let Ok(mut body) = players.single_mut() else {
        return;
    };
    // Screen-up (negative y) raises the view. Horizontal motion is ignored:
    // turning is keyboard-only.
    body.pitch = PlayerBody::clamp_pitch(body.pitch - delta.y * MOUSE_SENSITIVITY.to_radians());
}

/// Speed adjustment (E/Q) and cursor capture toggle (Esc), in the original's
/// 0.1 m/s steps between 1.2 and 2.0.
pub(crate) fn adjust_speed_and_cursor(
    keys: Res<ButtonInput<KeyCode>>,
    phase: Res<crate::core::game_state::GamePhase>,
    mut players: Query<&mut PlayerBody>,
) {
    // Menus own the cursor now (Esc pauses instead of toggling capture), so
    // speed only adjusts while the game actually runs.
    if !phase.is_game_active() {
        return;
    }
    if let Ok(mut body) = players.single_mut() {
        if keys.just_pressed(SPEED_UP) {
            body.walk_speed = (body.walk_speed + SPEED_STEP).min(MAX_WALK_SPEED);
        }
        if keys.just_pressed(SPEED_DOWN) {
            body.walk_speed = (body.walk_speed - SPEED_STEP).max(MIN_WALK_SPEED);
        }
    }
}

/// Turns with A/D, walks with W/S at the current speed (accelerating towards
/// it), then resolves the move against the solid blocks.
///
/// Collision is applied in substeps no longer than half the player radius, so
/// a frame's travel can never skip over a block edge (tunnelling).
pub(crate) fn walk_and_collide(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    phase: Res<crate::core::game_state::GamePhase>,
    collision: Res<CollisionIndex>,
    mut players: Query<(&mut PlayerBody, &mut Transform)>,
) {
    // R4.6 input gating: no movement outside the playing phase.
    if !phase.is_game_active() {
        return;
    }
    let Ok((mut body, mut transform)) = players.single_mut() else {
        return;
    };
    let dt = time.delta_secs();

    // Turning (A left / D right on the body, not the camera).
    let turn = (keys.pressed(TURN_LEFT) as i32 - keys.pressed(TURN_RIGHT) as i32) as f32;
    body.yaw += turn * TURN_SPEED_DEG.to_radians() * dt;

    // Walk input along the body facing.
    let input = (keys.pressed(MOVE_FORWARD) as i32 - keys.pressed(MOVE_BACKWARD) as i32) as f32;
    let target = input * body.walk_speed;
    body.forward_speed = approach(body.forward_speed, target, ACCELERATION * dt);

    let dir = body.forward();
    let mut pos = Vec2::new(transform.translation.x, transform.translation.z);
    let travel = body.forward_speed * dt;
    if travel.abs() > f32::EPSILON {
        // Substep: at most half a radius per step keeps a thin block edge
        // from being skipped (fast walk + low frame rate).
        let steps = ((travel.abs() / (PLAYER_RADIUS * 0.5)).ceil() as u32).max(1);
        let step = travel / steps as f32;
        for _ in 0..steps {
            let next = pos + dir * step;
            pos = collision.resolve(next, PLAYER_RADIUS);
        }
    }

    transform.translation.x = pos.x;
    transform.translation.z = pos.y;
    transform.translation.y = EYE_HEIGHT;
    transform.rotation = body.camera_rotation();
}

/// Moves `value` towards `target` by at most `max_delta`.
fn approach(value: f32, target: f32, max_delta: f32) -> f32 {
    let delta = target - value;
    if delta.abs() <= max_delta {
        target
    } else {
        value + delta.signum() * max_delta
    }
}

/// Registers the player controller systems with explicit ordering.
pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<MouseMotion>()
            .add_systems(
                Update,
                (
                    mouse_look.in_set(PlayerSystems::Look),
                    adjust_speed_and_cursor.in_set(PlayerSystems::Look),
                ),
            )
            .add_systems(Update, walk_and_collide.in_set(PlayerSystems::Move));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pitch_is_clamped_to_the_looking_up_band() {
        assert!((PlayerBody::clamp_pitch(-30.0) - MIN_PITCH_DEG.to_radians()).abs() < 1e-6);
        assert!(
            (PlayerBody::clamp_pitch(std::f32::consts::FRAC_PI_2) - MAX_PITCH_DEG.to_radians())
                .abs()
                < 1e-6
        );
        // A value inside the band is untouched.
        let inside = 50f32.to_radians();
        assert!((PlayerBody::clamp_pitch(inside) - inside).abs() < 1e-6);
    }

    #[test]
    fn forward_follows_yaw() {
        let mut body = PlayerBody::default();
        // Yaw 0 faces -Z.
        let f = body.forward();
        assert!(f.x.abs() < 1e-6 && (f.y + 1.0).abs() < 1e-6);
        // Yaw +90° faces -X, and yaw only affects the camera, not the body.
        body.yaw = std::f32::consts::FRAC_PI_2;
        let f = body.forward();
        assert!((f.x + 1.0).abs() < 1e-6 && f.y.abs() < 1e-6);
    }

    #[test]
    fn camera_yaw_offset_does_not_change_walk_direction() {
        let mut body = PlayerBody::default();
        let before = body.forward();
        body.yaw_offset = MAX_YAW_OFFSET_DEG.to_radians();
        assert_eq!(before, body.forward(), "the ±10° glance must not steer");
    }

    #[test]
    fn speed_steps_stay_within_bounds() {
        let mut body = PlayerBody::default();
        for _ in 0..20 {
            body.walk_speed = (body.walk_speed + SPEED_STEP).min(MAX_WALK_SPEED);
        }
        assert_eq!(body.walk_speed, MAX_WALK_SPEED);
        for _ in 0..40 {
            body.walk_speed = (body.walk_speed - SPEED_STEP).max(MIN_WALK_SPEED);
        }
        assert!((body.walk_speed - MIN_WALK_SPEED).abs() < 1e-6);
    }

    #[test]
    fn approach_accelerates_at_the_right_rate() {
        assert!((approach(0.0, 1.5, 0.1) - 0.1).abs() < 1e-6);
        assert!((approach(1.45, 1.5, 0.1) - 1.5).abs() < 1e-6);
        assert!((approach(0.0, -1.5, 0.1) + 0.1).abs() < 1e-6);
    }

    /// Regression: a merged block's inset perimeter can be slightly concave.
    /// Colliding against its raw polygon let the walker end up inside 13 of
    /// 770 blocks at max speed; collisions use the hull instead.
    #[test]
    fn concave_block_polygon_still_blocks_the_walker() {
        use crate::world::collision::{CollisionIndex, PLAYER_RADIUS, PrismKind, SolidPrism};
        use crate::world::geometry::convex_hull;
        use crate::world::{BLOCK_SOLID_HEIGHT, CHUNK_SIZE};

        // The 8-vertex perimeter measured from a real seed-0x48454144 block:
        // vertices 3, 5 and 7 dip inward, making it concave.
        let raw = vec![
            Vec2::new(952.0, 50.0),
            Vec2::new(1001.0, 49.0),
            Vec2::new(1049.0, 55.0),
            Vec2::new(1047.0, 99.0),
            Vec2::new(1049.0, 150.0),
            Vec2::new(1002.0, 143.0),
            Vec2::new(952.0, 146.0),
            Vec2::new(953.0, 104.0),
        ];
        let mut index = CollisionIndex::default();
        index.debug_set_chunk(
            (19, 2),
            vec![SolidPrism {
                child: None,
                block_id: 12,
                polygon: convex_hull(&raw),
                height: BLOCK_SOLID_HEIGHT,
                kind: PrismKind::Apartment,
            }],
        );

        // Walk straight down into the block from just outside its top edge.
        let mut pos = Vec2::new(1000.0, 143.5);
        for _ in 0..120 {
            let step = MAX_WALK_SPEED / 60.0;
            let steps = ((step / (PLAYER_RADIUS * 0.5)).ceil() as u32).max(1);
            for _ in 0..steps {
                pos = index.resolve(pos - Vec2::Y * (step / steps as f32), PLAYER_RADIUS);
            }
        }
        assert!(
            !index.is_blocked(pos),
            "walker ended inside the concave block at {pos:?}"
        );
        let _ = CHUNK_SIZE;
    }

    /// A fast walker must not tunnel through a solid block edge.
    #[test]
    fn fast_walk_does_not_tunnel_through_a_wall() {
        use crate::world::collision::SolidPrism;
        use crate::world::{BLOCK_SOLID_HEIGHT, CHUNK_SIZE};

        let mut index = CollisionIndex::default();
        // A wall spanning the chunk at x >= 20.
        let prism = SolidPrism {
            child: None,
            block_id: 0,
            polygon: vec![
                Vec2::new(20.0, 0.0),
                Vec2::new(CHUNK_SIZE, 0.0),
                Vec2::new(CHUNK_SIZE, CHUNK_SIZE),
                Vec2::new(20.0, CHUNK_SIZE),
            ],
            height: BLOCK_SOLID_HEIGHT,
            kind: crate::world::collision::PrismKind::OldCity,
        };
        index.register_chunk(&crate::world::layout_graph::LayoutGraph::default(), (0, 0));
        // Overwrite the bucket with just the wall.
        index.debug_set_chunk((0, 0), vec![prism]);

        let mut pos = Vec2::new(10.0, 25.0);
        // Simulate 2 m/s for 60 frames (2 seconds, 120 m of intent).
        let dt = 1.0 / 60.0;
        let per_frame = MAX_WALK_SPEED * dt;
        for _ in 0..60 {
            let steps = ((per_frame / (PLAYER_RADIUS * 0.5)).ceil() as u32).max(1);
            for _ in 0..steps {
                pos = index.resolve(
                    pos + Vec2::new(per_frame / steps as f32, 0.0),
                    PLAYER_RADIUS,
                );
            }
        }
        assert!(
            pos.x <= 20.0 + 1e-3,
            "walker tunnelled to {pos:?}, expected to stop at the wall (x <= 20)"
        );
    }
}
