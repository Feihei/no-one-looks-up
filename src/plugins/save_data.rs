//! Save serialization (R4.2, ADR-0001: seed-based regeneration).
//!
//! A save is *tiny* on purpose: the layout regenerates from the seed in
//! under a second, so the file only carries what regeneration cannot
//! reproduce — the exploration bitmap and the player's pose. The season is
//! a placeholder enum until R3.5 lands (the field must exist in the format
//! so old saves stay loadable when the real system arrives).
//!
//! Files live in a `saves/` directory next to the executable for now
//! (R4.4 introduces the slot UI; a platform app-data path can be swapped in
//! behind [`save_dir`] without touching the format).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::world::explored::ExploredBitmap;

/// The season of a run (ADR-0001: fixed per run).
///
/// Re-exported from [`crate::world::season`] rather than defined here: the
/// field has been in the format since R4.2 as a placeholder with `Summer` as
/// its default and its only variant, and the real enum keeps that default and
/// serializes by variant name, so every R4-era save still parses unchanged.
pub use crate::world::season::Season;

/// One saved game, exactly what R4.3 restores from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveData {
    /// Format version, bumped on any breaking change.
    pub version: u32,
    /// World seed: regenerates the layout bit-exactly.
    pub seed: u64,
    /// Chosen season (placeholder until R3.5).
    pub season: Season,
    /// Explored chunk keys, sorted for a stable diff-friendly file.
    pub explored: Vec<(i32, i32)>,
    /// Player eye position (world XZ).
    pub player_pos: [f32; 2],
    /// Body facing (radians).
    pub player_yaw: f32,
}

/// Current save format version.
pub const SAVE_VERSION: u32 = 1;

impl SaveData {
    /// Captures the current game state.
    pub fn capture(
        seed: u64,
        season: Season,
        bitmap: &ExploredBitmap,
        player_pos: [f32; 2],
        player_yaw: f32,
    ) -> Self {
        SaveData {
            version: SAVE_VERSION,
            seed,
            season,
            explored: bitmap.sorted_keys(),
            player_pos,
            player_yaw,
        }
    }

    /// Restores the exploration bitmap from this save.
    pub fn explored_bitmap(&self) -> ExploredBitmap {
        ExploredBitmap::restore(self.explored.iter().copied())
    }
}

/// Slot numbering (ADR-0001): slot 0 is the autosave, slots 1-99 are the
/// user's manual saves. The load browser shows every slot zero-padded
/// (00-99, one hundred slots).
pub const AUTOSAVE_SLOT: u32 = 0;
pub const USER_SLOT_COUNT: u32 = 99;

/// What the slot list (R4.4/4.5) shows for one save file.
#[derive(Debug, Clone, PartialEq)]
pub struct SaveSlotInfo {
    /// Slot index the file occupies.
    pub slot: u32,
    /// Seed (regenerates the world).
    pub seed: u64,
    /// Season chosen at run start.
    pub season: Season,
    /// Explored-chunk count (progress against [`crate::world::GRID_CELLS`]²).
    pub explored_chunks: usize,
    /// File modification time (seconds since Unix epoch), for ordering.
    pub modified_secs: u64,
}

/// Lists every occupied slot, oldest-modified first.
///
/// Corrupt or unreadable files are skipped (a bad save must never break the
/// menu); an empty directory yields an empty list. With one hundred slots
/// this reads every file — the menu's load browser uses [`slot_info`] for
/// the one slot it shows instead of calling this per frame.
pub fn list_slots() -> Vec<SaveSlotInfo> {
    let mut slots: Vec<SaveSlotInfo> = (AUTOSAVE_SLOT..=USER_SLOT_COUNT)
        .filter_map(slot_info)
        .collect();
    slots.sort_by_key(|info| info.modified_secs);
    slots
}

/// Reads one slot's list info, or `None` when the slot is empty/corrupt.
pub fn slot_info(slot: u32) -> Option<SaveSlotInfo> {
    let data = read_save(slot).ok()?;
    let modified_secs = fs::metadata(save_path(slot))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(SaveSlotInfo {
        slot,
        seed: data.seed,
        season: data.season,
        explored_chunks: data.explored.len(),
        modified_secs,
    })
}

/// Directory that holds the save files.
pub fn save_dir() -> PathBuf {
    PathBuf::from("saves")
}

/// Path of one save slot (`slot 0` = autosave, per ADR-0001).
pub fn save_path(slot: u32) -> PathBuf {
    save_dir().join(format!("slot_{slot}.json"))
}

/// Writes a save to a slot (creating the directory if needed).
pub fn write_save(slot: u32, data: &SaveData) -> std::io::Result<()> {
    let path = save_path(slot);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(path, json)
}

/// Reads a save from a slot.
pub fn read_save(slot: u32) -> std::io::Result<SaveData> {
    let json = fs::read_to_string(save_path(slot))?;
    serde_json::from_str(&json).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::explored::ExploredBitmap;

    #[test]
    fn round_trip_is_bit_exact() {
        let mut bitmap = ExploredBitmap::default();
        for k in [(20_i32, 20_i32), (0, 0), (39, 39), (22, 20)] {
            bitmap.mark(k);
        }
        let original = SaveData::capture(
            0x4845_4144,
            Season::Summer,
            &bitmap,
            [1123.5, 987.25],
            1.2345,
        );
        let json = serde_json::to_string(&original).expect("serializes");
        let parsed: SaveData = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(original, parsed, "round trip preserves every field");
        assert_eq!(original.version, SAVE_VERSION);

        // The restored bitmap behaves identically.
        let restored = parsed.explored_bitmap();
        assert_eq!(restored.len(), bitmap.len());
        for key in bitmap.sorted_keys() {
            assert!(restored.is_explored(key));
        }
    }

    #[test]
    fn file_write_read_round_trips() {
        // Isolated slot name so a developer's real save never participates.
        let mut bitmap = ExploredBitmap::default();
        bitmap.mark((5, 6));
        bitmap.mark((6, 5));
        let data = SaveData::capture(42, Season::Summer, &bitmap, [50.0, 50.0], 0.0);
        write_save(998, &data).expect("writes");
        let loaded = read_save(998).expect("reads back");
        assert_eq!(data, loaded);
        // Cleanup: test save files are disposable by construction.
        let _ = fs::remove_file(save_path(998));
    }

    #[test]
    fn unknown_version_is_detectable() {
        let json = r#"{"version":999,"seed":1,"season":"Summer","explored":[],
            "player_pos":[0.0,0.0],"player_yaw":0.0}"#;
        let parsed: SaveData = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.version, 999);
        assert_ne!(parsed.version, SAVE_VERSION, "loader must version-check");
    }
}
