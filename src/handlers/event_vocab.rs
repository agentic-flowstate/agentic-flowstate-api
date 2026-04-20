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
//! This is the single source of truth for the rollout. T-257C060D formalizes
//! operational management on top of this enum.
//!
//! ## Failure mode
//!
//! Missing or invalid `EVENT_VOCAB_MODE` — the binary fails at startup via
//! [`EventVocabMode::from_env`] returning `Err`. There is no silent fallback.
//! Absent env var is treated as "operator didn't say → pick the safe
//! default (`dual`)"; any other value is a loud error.

use std::fmt;
use std::sync::OnceLock;

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

    /// Read from the process environment. Returns the default when the env
    /// var is unset; returns `Err` on any non-empty value that isn't one of
    /// the three canonical names.
    pub fn from_env() -> Result<Self, EventVocabModeError> {
        match std::env::var(Self::ENV_VAR) {
            Ok(v) if v.trim().is_empty() => Ok(Self::DEFAULT),
            Ok(v) => Self::parse(&v),
            Err(std::env::VarError::NotPresent) => Ok(Self::DEFAULT),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(EventVocabModeError::NotUnicode)
            }
        }
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

/// Process-wide event-vocabulary mode, initialized once from
/// `EVENT_VOCAB_MODE` at startup. Read by the conversation worker on every
/// emitted event.
///
/// Populated by `main.rs` via [`init_global`] immediately after env parsing.
/// Before init, [`global`] returns [`EventVocabMode::DEFAULT`] — no panics,
/// no silent fallbacks beyond the documented default. Tests can skip init
/// and rely on the default, or call `init_global` explicitly.
static GLOBAL_MODE: OnceLock<EventVocabMode> = OnceLock::new();

/// Set the global mode. Idempotent only in the sense that subsequent calls
/// are ignored (OnceLock::set errors after the first). Intended to be
/// called exactly once, from main.rs, after parsing env.
pub fn init_global(mode: EventVocabMode) {
    let _ = GLOBAL_MODE.set(mode);
}

/// Read the global mode. Returns [`EventVocabMode::DEFAULT`] if
/// [`init_global`] hasn't been called yet.
pub fn global() -> EventVocabMode {
    GLOBAL_MODE.get().copied().unwrap_or(EventVocabMode::DEFAULT)
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

    /// `from_env` is exercised indirectly by the unit tests on `parse`;
    /// we don't mutate process env here because Rust test runners may
    /// execute tests in parallel. The startup-failure behavior is covered
    /// by the error path `parse` returns.
    #[test]
    fn default_is_dual() {
        assert_eq!(EventVocabMode::DEFAULT, EventVocabMode::Dual);
    }
}
