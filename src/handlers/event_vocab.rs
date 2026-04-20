//! Streaming event vocabulary mode — the dual-write toggle.
//!
//! Controls which schema_version rows the conversation_events writer emits:
//!
//! | `EVENT_VOCAB_MODE` | writes v1 (legacy) | writes v2 (Anthropic) |
//! |--------------------|--------------------|-----------------------|
//! | `legacy_only`      | yes                | no                    |
//! | `dual` (default)   | yes                | yes                   |
//! | `modern_only`      | no                 | yes                   |
//!
//! ## Runtime flippability (T-257C060D)
//!
//! The mode is held in an [`ArcSwap<EventVocabMode>`] so readers get a
//! lock-free O(1) snapshot and writers atomically swap a new value in.
//! This replaces the previous `OnceLock` so operators can flip the
//! rollout stage without a redeploy.
//!
//! Three writers touch the swap:
//!   1. [`init_global`] — once at startup from env var / DB bootstrap.
//!   2. [`store`] — when an admin PUTs `/api/admin/feature-flags/event_vocab_mode`.
//!   3. The 30-second refresher task (see `admin_feature_flags.rs`) —
//!      polls the DB and re-swaps when the value differs. This keeps
//!      multiple API instances in sync.
//!
//! Readers call [`current_mode`] — a single atomic load. The old
//! `global()` accessor remains as a thin forwarder for compatibility,
//! but the new name makes the swap semantics explicit.
//!
//! ## Failure mode
//!
//! Missing or empty `EVENT_VOCAB_MODE` → default (`dual`). Any other
//! non-canonical value at startup → fail loud (startup aborts). PUT of
//! a non-canonical value → 400. There is NO silent fallback for invalid
//! input.

use std::fmt;
use std::sync::OnceLock;

use arc_swap::ArcSwap;

/// Modes for the dual-write rollout.
///
/// `Dual` writes both v1 and v2 rows for the same logical event so both
/// old and new clients can read the stream. `LegacyOnly` and `ModernOnly`
/// are the pre- and post-rollout endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventVocabMode {
    /// Only write v1 (legacy) rows.
    LegacyOnly,
    /// Write both v1 (legacy) and v2 (Anthropic) rows — default.
    Dual,
    /// Only write v2 (Anthropic) rows.
    ModernOnly,
}

impl EventVocabMode {
    /// Name of the env var read at startup.
    pub const ENV_VAR: &'static str = "EVENT_VOCAB_MODE";

    /// Feature-flag key stored in the `feature_flags` table.
    pub const FLAG_KEY: &'static str = "event_vocab_mode";

    /// Default when `EVENT_VOCAB_MODE` is unset — dual-write is the safe
    /// default because it keeps every existing client working while v2 rolls
    /// out. Flipping to `ModernOnly` is a deliberate operator action.
    pub const DEFAULT: EventVocabMode = EventVocabMode::Dual;

    /// Returns `true` if this mode requires writing a legacy v1 row.
    pub fn writes_legacy(self) -> bool {
        matches!(self, EventVocabMode::LegacyOnly | EventVocabMode::Dual)
    }

    /// Returns `true` if this mode requires writing a modern v2 row.
    pub fn writes_modern(self) -> bool {
        matches!(self, EventVocabMode::ModernOnly | EventVocabMode::Dual)
    }

    /// Parse a case-insensitive string. Only the three canonical names are
    /// accepted — no aliases, no partial matches.
    pub fn parse(s: &str) -> Result<Self, EventVocabModeError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "legacy_only" => Ok(EventVocabMode::LegacyOnly),
            "dual" => Ok(EventVocabMode::Dual),
            "modern_only" => Ok(EventVocabMode::ModernOnly),
            other => Err(EventVocabModeError::Invalid(other.to_string())),
        }
    }

    /// Read the startup env var override. Returns `Ok(None)` when the
    /// variable is unset or whitespace-only — the caller can then fall
    /// back to the DB row or the default. Returns `Err` on non-UTF-8 or
    /// a non-canonical value so startup can fail loudly.
    pub fn from_env_optional() -> Result<Option<Self>, EventVocabModeError> {
        match std::env::var(Self::ENV_VAR) {
            Ok(v) if v.trim().is_empty() => Ok(None),
            Ok(v) => Self::parse(&v).map(Some),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(EventVocabModeError::NotUnicode)
            }
        }
    }

    /// Read from the process environment. Returns the default when the env
    /// var is unset; returns `Err` on any non-empty value that isn't one of
    /// the three canonical names.
    ///
    /// Kept for backwards compatibility with the original startup path
    /// (spec A-D50FCA98). New callers should prefer `from_env_optional`
    /// so the DB-seeded row can survive a restart with no env var.
    pub fn from_env() -> Result<Self, EventVocabModeError> {
        Ok(Self::from_env_optional()?.unwrap_or(Self::DEFAULT))
    }

    /// The canonical string form — matches what `parse` accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            EventVocabMode::LegacyOnly => "legacy_only",
            EventVocabMode::Dual => "dual",
            EventVocabMode::ModernOnly => "modern_only",
        }
    }
}

impl fmt::Display for EventVocabMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EventVocabModeError {
    #[error(
        "invalid EVENT_VOCAB_MODE {0:?} — must be one of: legacy_only, dual, modern_only"
    )]
    Invalid(String),
    #[error("EVENT_VOCAB_MODE contains non-UTF-8 bytes")]
    NotUnicode,
}

/// Process-wide event-vocabulary mode, swap-able at runtime. Held in an
/// `ArcSwap<EventVocabMode>` so readers get a lock-free atomic load and
/// writers atomically publish the new value.
///
/// Initialized lazily on first access: if no one has called
/// [`init_global`] yet, [`current_mode`] will create the cell with
/// [`EventVocabMode::DEFAULT`]. Production always calls `init_global`
/// from `main.rs` before any worker starts.
static GLOBAL_MODE: OnceLock<ArcSwap<EventVocabMode>> = OnceLock::new();

fn cell() -> &'static ArcSwap<EventVocabMode> {
    GLOBAL_MODE.get_or_init(|| ArcSwap::from_pointee(EventVocabMode::DEFAULT))
}

/// Set the global mode. Can be called more than once — subsequent calls
/// atomically swap in the new value. Called from `main.rs` at startup
/// (once) and from the admin PUT handler / refresher task (possibly
/// many times).
pub fn init_global(mode: EventVocabMode) {
    cell().store(std::sync::Arc::new(mode));
}

/// Atomic swap primitive used by the admin PUT handler and the
/// refresher. Alias for [`init_global`] — kept as a distinct name so
/// call sites document intent ("I'm *storing* a new value", not
/// "initializing").
pub fn store(mode: EventVocabMode) {
    init_global(mode);
}

/// Read the current mode. Returns an owned `EventVocabMode` — cheap,
/// since the enum is `Copy`. Lock-free.
pub fn current_mode() -> EventVocabMode {
    *cell().load_full()
}

/// Process-wide mutex used by tests that mutate the global `ArcSwap`
/// and/or `EVENT_VOCAB_MODE` env var. Exposed so sibling modules
/// (`admin_feature_flags`) can serialize with each other — cargo runs
/// tests in parallel by default, and both modules share the same
/// process-wide global.
#[cfg(test)]
pub(crate) static TEST_STATE_LOCK: std::sync::Mutex<()> =
    std::sync::Mutex::new(());

/// Legacy accessor name. Forwards to [`current_mode`]. Kept so the
/// conversation worker and tests don't need simultaneous edits.
pub fn global() -> EventVocabMode {
    current_mode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_canonical_values() {
        assert_eq!(
            EventVocabMode::parse("legacy_only").unwrap(),
            EventVocabMode::LegacyOnly
        );
        assert_eq!(EventVocabMode::parse("dual").unwrap(), EventVocabMode::Dual);
        assert_eq!(
            EventVocabMode::parse("modern_only").unwrap(),
            EventVocabMode::ModernOnly
        );
    }

    #[test]
    fn parse_is_case_insensitive_and_trims() {
        assert_eq!(
            EventVocabMode::parse("  LEGACY_ONLY  ").unwrap(),
            EventVocabMode::LegacyOnly
        );
        assert_eq!(
            EventVocabMode::parse("Dual").unwrap(),
            EventVocabMode::Dual
        );
    }

    #[test]
    fn parse_rejects_unknown_values() {
        assert!(matches!(
            EventVocabMode::parse("both"),
            Err(EventVocabModeError::Invalid(_))
        ));
        assert!(matches!(
            EventVocabMode::parse(""),
            Err(EventVocabModeError::Invalid(_))
        ));
        assert!(matches!(
            EventVocabMode::parse("legacy"),
            Err(EventVocabModeError::Invalid(_))
        ));
    }

    #[test]
    fn writes_legacy_and_modern_flags() {
        assert!(EventVocabMode::LegacyOnly.writes_legacy());
        assert!(!EventVocabMode::LegacyOnly.writes_modern());

        assert!(EventVocabMode::Dual.writes_legacy());
        assert!(EventVocabMode::Dual.writes_modern());

        assert!(!EventVocabMode::ModernOnly.writes_legacy());
        assert!(EventVocabMode::ModernOnly.writes_modern());
    }

    #[test]
    fn roundtrip_display_and_parse() {
        for mode in [
            EventVocabMode::LegacyOnly,
            EventVocabMode::Dual,
            EventVocabMode::ModernOnly,
        ] {
            let rendered = mode.to_string();
            assert_eq!(EventVocabMode::parse(&rendered).unwrap(), mode);
        }
    }

    #[test]
    fn default_is_dual() {
        assert_eq!(EventVocabMode::DEFAULT, EventVocabMode::Dual);
    }

    /// Runtime swap: writing a new value via `store()` makes the next
    /// `current_mode()` read return it. This is the whole point of the
    /// ArcSwap — validate that the primitive actually publishes.
    ///
    /// Serializes with `admin_feature_flags` tests via `TEST_STATE_LOCK`
    /// so concurrent writers from the other module don't race this one.
    #[test]
    fn runtime_store_publishes_new_value() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        // Start from a known baseline.
        store(EventVocabMode::Dual);
        assert_eq!(current_mode(), EventVocabMode::Dual);

        store(EventVocabMode::ModernOnly);
        assert_eq!(current_mode(), EventVocabMode::ModernOnly);

        store(EventVocabMode::LegacyOnly);
        assert_eq!(current_mode(), EventVocabMode::LegacyOnly);

        // Reset for anyone reading after us.
        store(EventVocabMode::Dual);
    }
}
