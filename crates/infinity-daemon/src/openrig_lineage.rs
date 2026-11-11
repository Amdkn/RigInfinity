//! OpenRig lineage scheduler — the native replacement for the bind's
//! `spawnHolonRig()` (D-N051 F8, pas 4, D6).
//!
//! The bind (`src/lineage_spawner.ts`, pin `adafb0d`) is a declaration, not
//! a scheduler: it builds `RIG-${areaCode}-${childHolon}-${Date.now()}` with
//! no I/O, no uniqueness guarantee beyond the millisecond, and no record of
//! what it spawned. The replacement is a real scheduler: ULID rig ids (the
//! same identifier family the daemon itself uses — the `ulid` npm package),
//! monotonic within the millisecond, with the worktree path derived from the
//! id and a spawn record the state store persists.
//!
//! The ULID generator is std-only: 48-bit millisecond timestamp +
//! 80-bit randomness from `/dev/urandom`, Crockford base32, 26 chars.
//! Two ids generated in the same millisecond sort after each other (the
//! random part is incremented, as the `ulid` package does).

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Crockford base32 alphabet, as the `ulid` package uses it.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Monotonic state: the last timestamp issued and its random part.
static LAST: Mutex<(u64, [u8; 10])> = Mutex::new((0, [0; 10]));

fn read_urandom(buf: &mut [u8; 10]) -> std::io::Result<()> {
    use std::io::Read as _;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

fn encode_time(mut value: u64) -> String {
    let mut out = [b'0'; 10];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(value % 32) as usize];
        value /= 32;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode_random(bytes: &[u8; 10]) -> String {
    let mut out = String::with_capacity(16);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((acc >> bits) & 31) as usize] as char);
        }
    }
    out
}

fn increment_random(bytes: [u8; 10]) -> [u8; 10] {
    let mut next = bytes;
    for slot in next.iter_mut().rev() {
        *slot = slot.wrapping_add(1);
        if *slot != 0 {
            break;
        }
    }
    next
}

/// Current time in milliseconds since the Unix epoch.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generate a ULID (26 chars, Crockford base32). Monotonic within the
/// millisecond: two ids generated in the same millisecond sort after each
/// other.
pub fn generate_ulid() -> String {
    let now = now_millis();
    let mut last = LAST.lock().expect("bug: lineage ULID lock poisoned");
    let rand = if now == last.0 {
        increment_random(last.1)
    } else {
        let mut bytes = [0u8; 10];
        if read_urandom(&mut bytes).is_err() {
            // /dev/urandom is expected on Linux; fall back to a
            // time/pid-derived seed rather than failing the spawn.
            let seed = now ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
            bytes[..8].copy_from_slice(&seed.to_be_bytes());
            bytes[8..].copy_from_slice(&(now >> 16).to_be_bytes()[..2]);
        }
        bytes
    };
    *last = (now, rand);
    drop(last);
    format!("{}{}", encode_time(now), encode_random(&rand))
}

/// A ULID is 26 Crockford-base32 characters.
pub fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V' | b'W' | b'X' | b'Y' | b'Z'))
}

/// The spawn record the unique daemon persists for every spawned rig.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRecord {
    pub rig_id: String,
    pub area_code: String,
    pub child_holon: String,
    pub worktree_path: String,
    /// ISO 8601 UTC instant of the spawn.
    pub spawned_at: String,
}

/// The lineage scheduler: the real ordonnanceur that replaces the bind's
/// `spawnHolonRig()`. Same rig-id shape (`RIG-<area>-<holon>-<unique>`),
/// same worktree layout (`.worktrees/<rigId>`), but the unique suffix is a
/// ULID, not `Date.now()` — sortable, collision-proof within the ms, and
/// recorded in the spawn record.
#[derive(Debug, Default, Clone)]
pub struct LineageScheduler;

impl LineageScheduler {
    pub fn new() -> Self {
        Self
    }

    /// Spawn a rig id for a child holon in an area. `area_code` and
    /// `child_holon` are the caller's words (e.g. `A0`, `clara`); they are
    /// upper-cased and stripped of anything outside `[A-Z0-9-]`.
    pub fn spawn_rig_id(&self, area_code: &str, child_holon: &str) -> String {
        format!(
            "RIG-{}-{}-{}",
            clean_part(area_code),
            clean_part(child_holon),
            generate_ulid()
        )
    }

    /// The worktree path for a spawned rig (same layout as the bind).
    pub fn worktree_path(rig_id: &str) -> String {
        format!(".worktrees/{rig_id}")
    }

    /// Parse a spawned rig id back into its parts; `None` when not ours.
    pub fn parse_rig_id(rig_id: &str) -> Option<(String, String, String)> {
        let rest = rig_id.strip_prefix("RIG-")?;
        let (area, rest) = rest.split_once('-')?;
        let (holon, ulid) = rest.rsplit_once('-')?;
        if area.is_empty() || holon.is_empty() || !is_ulid(ulid) {
            return None;
        }
        Some((area.to_string(), holon.to_string(), ulid.to_string()))
    }

    /// Spawn a rig and build its record (what the state store persists).
    pub fn spawn(&self, area_code: &str, child_holon: &str, spawned_at: String) -> SpawnRecord {
        let rig_id = self.spawn_rig_id(area_code, child_holon);
        let worktree_path = Self::worktree_path(&rig_id);
        SpawnRecord {
            rig_id,
            area_code: area_code.to_uppercase(),
            child_holon: child_holon.to_uppercase(),
            worktree_path,
            spawned_at,
        }
    }
}

fn clean_part(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in s.to_uppercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "X".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_is_26_crockford_chars() {
        let id = generate_ulid();
        assert_eq!(id.len(), 26);
        assert!(is_ulid(&id));
        assert!(!is_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAI")); // 26 chars but contains 'I', excluded from Crockford
        assert!(!is_ulid("01ARZ3NDEKTSV4RRFFQ69G5F")); // 25 chars
        assert!(!is_ulid("RIG-A0-CLARA-1234567890"));
    }

    #[test]
    fn ulids_sort_and_differ() {
        let a = generate_ulid();
        let b = generate_ulid();
        assert_ne!(a, b);
        assert!(a < b, "monotonic ULIDs sort: {a} < {b}");
    }

    #[test]
    fn spawn_round_trips() {
        let scheduler = LineageScheduler::new();
        let rig_id = scheduler.spawn_rig_id("A0", "clara");
        let (area, holon, ulid) =
            LineageScheduler::parse_rig_id(&rig_id).expect("parse spawned id");
        assert_eq!(area, "A0");
        assert_eq!(holon, "CLARA");
        assert!(is_ulid(&ulid));
        assert_eq!(
            LineageScheduler::worktree_path(&rig_id),
            format!(".worktrees/{rig_id}")
        );
        assert_eq!(
            LineageScheduler::parse_rig_id("RIG-A0-CLARA-1234567890"),
            None
        );
        assert_eq!(LineageScheduler::parse_rig_id("not-a-rig"), None);
    }

    #[test]
    fn spawn_record_carries_the_parts() {
        let scheduler = LineageScheduler::new();
        let record = scheduler.spawn("a0", "clara", "2026-11-10T00:00:00Z".to_string());
        assert!(record.rig_id.starts_with("RIG-A0-CLARA-"));
        assert_eq!(record.area_code, "A0");
        assert_eq!(record.child_holon, "CLARA");
        assert_eq!(
            record.worktree_path,
            format!(".worktrees/{}", record.rig_id)
        );
        assert_eq!(record.spawned_at, "2026-11-10T00:00:00Z");
    }
}
