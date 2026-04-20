//! SSE resume cursor extraction and validation (T-2EDABDDD).
//!
//! Implements the Ably-style resume-token protocol for the durable chat
//! streaming endpoint. The client may reconnect to an existing
//! conversation stream and pick up exactly where it left off, by passing
//! the index of the last event it successfully processed.
//!
//! # Cursor sources (precedence)
//!
//! The client MAY supply a cursor via either:
//!
//! 1. `Last-Event-ID: N` HTTP header — standard WHATWG EventSource
//!    behavior. When the browser's built-in EventSource reconnects it
//!    automatically sets this header to the `id:` of the last event it
//!    saw. This is the canonical path for well-behaved clients.
//! 2. `?starting_after=N` query parameter — fallback for non-EventSource
//!    HTTP clients (native iOS `URLSession`, curl, tests) that cannot set
//!    headers transparently on reconnect but can reconstruct the URL.
//!
//! The header wins when both are present, matching Ably's documented
//! precedence rule. The query fallback exists because:
//! * `URLSessionDataTask` on iOS does not automatically attach
//!   `Last-Event-ID` — the app has to add it itself.
//! * `EventSource` implementations in some runtimes silently drop the
//!   header on HTTPS-to-HTTP redirects.
//!
//! # Cursor values
//!
//! The cursor is always a non-negative integer event index. Fresh streams
//! omit the cursor and are modeled with [`CursorSource::FreshStream`] and
//! `event_index = -1` so the query `event_index > cursor` naturally
//! yields everything from index 0 onward.
//!
//! # Error taxonomy
//!
//! * Non-numeric header or query value → [`CursorError::Malformed`] →
//!   400 Bad Request.
//! * Negative integer → [`CursorError::Malformed`].
//! * Valid cursor but below retention floor → emitted as
//!   [`CursorError::Retention`] by the handler AFTER consulting
//!   [`crate::retention::prune::earliest_retained_event_index`] → 410 Gone.
//!
//! # Reference
//!
//! Research artifact A-18DB4221 (§ "Resume Token Security + Gap
//! Detection"). Ably "Message continuity" docs. NATS JetStream
//! durable-consumer ordered delivery.

use axum::http::HeaderMap;
use serde::Deserialize;
use thiserror::Error;

/// Axum `Query` payload for the SSE reconnect endpoint. Untyped JSON
/// coercion: the only field is `starting_after`, an optional i64 cursor.
#[derive(Debug, Default, Deserialize)]
pub struct ResumeQuery {
    /// Optional i64 event-index cursor. When present and the header
    /// cursor is absent, this becomes the resume point.
    ///
    /// Deserialized as `Option<i64>` rather than `Option<String>` so
    /// malformed values (non-numeric) fail fast at the deserializer
    /// layer with Axum's standard 400 response. The `Malformed` variant
    /// of [`CursorError`] only fires for header-path parse failures and
    /// for negative values that deserialize cleanly as i64 but are
    /// semantically invalid.
    pub starting_after: Option<i64>,
}

/// Which transport supplied the resume cursor. Used by observability so
/// operators can chart which path iOS / web clients are taking on
/// reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorSource {
    /// Cursor came from the `Last-Event-ID` HTTP header. This is the
    /// WHATWG-standard path used by `EventSource` implementations.
    LastEventIdHeader,
    /// Cursor came from the `?starting_after=N` query parameter.
    /// Fallback for native HTTP clients.
    StartingAfterQuery,
    /// No cursor was supplied. The stream starts fresh from event 0.
    /// The cursor's `event_index` is set to `-1` so downstream SQL
    /// predicates of the form `event_index > cursor` work without a
    /// special case.
    FreshStream,
}

/// Parsed resume cursor plus a tag identifying where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeCursor {
    /// Event index the client claims was the last one it processed.
    /// Events strictly greater than this will be emitted. A value of
    /// `-1` paired with [`CursorSource::FreshStream`] means "start at
    /// the beginning of the retained log."
    pub event_index: i64,
    /// Which transport the cursor arrived on.
    pub source: CursorSource,
}

impl ResumeCursor {
    /// True iff the cursor was supplied by the client (either transport).
    /// False for a fresh stream with no cursor at all.
    pub fn is_resume(&self) -> bool {
        !matches!(self.source, CursorSource::FreshStream)
    }
}

/// Error emitted by [`extract_cursor`]. The handler maps each variant to
/// a specific HTTP status code (see [`crate::handlers::resume_stream`]
/// or the docstring on this module).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CursorError {
    /// The client sent a cursor the server cannot parse: non-integer,
    /// negative, or otherwise out-of-shape. Maps to 400 Bad Request.
    #[error("malformed cursor: {0}")]
    Malformed(String),

    /// The cursor parses but points below the retention floor — the
    /// server has already pruned the events the client wants to replay.
    /// Only ever produced by the handler AFTER it consults
    /// [`crate::retention::prune::earliest_retained_event_index`];
    /// [`extract_cursor`] itself does not produce this variant. Included
    /// here so the wire-format mapping lives in one place.
    ///
    /// Maps to 410 Gone with a JSON body:
    /// ```json
    /// {
    ///   "reason": "cursor_expired",
    ///   "earliest_event_index": <N>,
    ///   "cursor": <cursor_value>,
    ///   "retention_policy": {"age_days": 90, "min_keep": 10000}
    /// }
    /// ```
    ///
    /// `#[allow(dead_code)]` — the variant is part of the public wire
    /// taxonomy and is matched exhaustively in the handler. The compiler
    /// cannot see that an exhaustive match on a never-constructed variant
    /// is load-bearing for future refactors; suppressing the lint keeps
    /// the contract visible in one place.
    #[allow(dead_code)]
    #[error("cursor {cursor} below retention floor {earliest}")]
    Retention {
        /// The client's cursor.
        cursor: i64,
        /// The earliest event index the server still retains.
        earliest: i64,
    },
}

/// Extract a resume cursor from the incoming request's headers and
/// query string. Precedence: `Last-Event-ID` header > `?starting_after=N`
/// query > fresh stream.
///
/// # Errors
///
/// Returns [`CursorError::Malformed`] when either source supplies a
/// value that is not a non-negative integer. Retention-based rejection
/// is NOT done here — the caller must call
/// [`crate::retention::prune::earliest_retained_event_index`] after a
/// successful parse.
pub fn extract_cursor(
    headers: &HeaderMap,
    query: &ResumeQuery,
) -> Result<ResumeCursor, CursorError> {
    // Precedence rule: header beats query. The WHATWG spec says the
    // browser's EventSource sets Last-Event-ID on every reconnect, so a
    // client that uses both paths is most likely caching a stale query
    // URL but has a fresh header. We honor the fresher signal.
    if let Some(raw) = headers.get("last-event-id") {
        let s = raw
            .to_str()
            .map_err(|e| CursorError::Malformed(format!("Last-Event-ID not ASCII: {}", e)))?
            .trim();
        // An empty Last-Event-ID (browser sending `Last-Event-ID: ` on
        // a cold reconnect before any event was seen) is indistinguishable
        // from "no cursor" — treat as a fresh stream rather than an error.
        if s.is_empty() {
            // Fall through to the query/fresh path below. We cannot
            // early-return Ok(FreshStream) here because the client may
            // have *also* sent a query cursor that is still usable.
        } else {
            let parsed: i64 = s.parse().map_err(|_| {
                CursorError::Malformed(format!(
                    "Last-Event-ID must be a non-negative integer, got {:?}",
                    s
                ))
            })?;
            if parsed < 0 {
                return Err(CursorError::Malformed(format!(
                    "Last-Event-ID must be non-negative, got {}",
                    parsed
                )));
            }
            return Ok(ResumeCursor {
                event_index: parsed,
                source: CursorSource::LastEventIdHeader,
            });
        }
    }

    if let Some(after) = query.starting_after {
        if after < 0 {
            return Err(CursorError::Malformed(format!(
                "starting_after must be non-negative, got {}",
                after
            )));
        }
        return Ok(ResumeCursor {
            event_index: after,
            source: CursorSource::StartingAfterQuery,
        });
    }

    // No cursor from either transport. `-1` so `event_index > cursor`
    // includes event_index = 0.
    Ok(ResumeCursor {
        event_index: -1,
        source: CursorSource::FreshStream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::{HeaderMap, HeaderValue};

    fn headers_with_id(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("last-event-id", HeaderValue::from_str(v).unwrap());
        h
    }

    /// Precedence: when both `Last-Event-ID: 100` header and
    /// `?starting_after=50` query are present, the header wins.
    /// Mirrors Ably's documented behavior.
    #[test]
    fn header_precedence_wins_over_query() {
        let headers = headers_with_id("100");
        let query = ResumeQuery {
            starting_after: Some(50),
        };
        let cursor = extract_cursor(&headers, &query).unwrap();
        assert_eq!(cursor.event_index, 100);
        assert_eq!(cursor.source, CursorSource::LastEventIdHeader);
        assert!(cursor.is_resume());
    }

    /// Query fallback: when only the query cursor is present, it is
    /// used and tagged with `StartingAfterQuery`.
    #[test]
    fn query_fallback_when_header_absent() {
        let headers = HeaderMap::new();
        let query = ResumeQuery {
            starting_after: Some(50),
        };
        let cursor = extract_cursor(&headers, &query).unwrap();
        assert_eq!(cursor.event_index, 50);
        assert_eq!(cursor.source, CursorSource::StartingAfterQuery);
        assert!(cursor.is_resume());
    }

    /// Fresh stream: no cursor on either transport → `-1` + FreshStream.
    /// The `-1` makes the SQL predicate `event_index > cursor` return
    /// event 0 on the first iteration.
    #[test]
    fn fresh_stream_uses_minus_one_sentinel() {
        let headers = HeaderMap::new();
        let query = ResumeQuery::default();
        let cursor = extract_cursor(&headers, &query).unwrap();
        assert_eq!(cursor.event_index, -1);
        assert_eq!(cursor.source, CursorSource::FreshStream);
        assert!(!cursor.is_resume());
    }

    /// Malformed header: non-numeric `abc` → 400-class Malformed error.
    #[test]
    fn malformed_last_event_id_rejected() {
        let headers = headers_with_id("abc");
        let query = ResumeQuery::default();
        let err = extract_cursor(&headers, &query).unwrap_err();
        assert!(matches!(err, CursorError::Malformed(_)));
    }

    /// Malformed header: negative integer → Malformed error.
    #[test]
    fn negative_last_event_id_rejected() {
        let headers = headers_with_id("-5");
        let query = ResumeQuery::default();
        let err = extract_cursor(&headers, &query).unwrap_err();
        assert!(matches!(err, CursorError::Malformed(_)));
    }

    /// Malformed query: negative integer → Malformed error. Non-integer
    /// query values never reach this function — Axum's `Query<T>`
    /// extractor rejects them at the layer above.
    #[test]
    fn negative_starting_after_rejected() {
        let headers = HeaderMap::new();
        let query = ResumeQuery {
            starting_after: Some(-1),
        };
        let err = extract_cursor(&headers, &query).unwrap_err();
        assert!(matches!(err, CursorError::Malformed(_)));
    }

    /// Empty `Last-Event-ID: ` header (browser sent the header but with
    /// no value — happens on a cold reconnect before any event was ever
    /// seen) should fall through to the next precedence level, NOT error.
    #[test]
    fn empty_header_falls_through_to_query() {
        let headers = headers_with_id("");
        let query = ResumeQuery {
            starting_after: Some(7),
        };
        let cursor = extract_cursor(&headers, &query).unwrap();
        assert_eq!(cursor.event_index, 7);
        assert_eq!(cursor.source, CursorSource::StartingAfterQuery);
    }

    /// Empty header AND no query → fresh stream, not an error.
    #[test]
    fn empty_header_and_no_query_is_fresh_stream() {
        let headers = headers_with_id("");
        let query = ResumeQuery::default();
        let cursor = extract_cursor(&headers, &query).unwrap();
        assert_eq!(cursor.event_index, -1);
        assert_eq!(cursor.source, CursorSource::FreshStream);
    }

    /// Zero is a valid cursor (the client saw event 0 and wants
    /// everything strictly after). Must not be mistaken for "fresh".
    #[test]
    fn zero_cursor_is_valid_resume() {
        let headers = headers_with_id("0");
        let query = ResumeQuery::default();
        let cursor = extract_cursor(&headers, &query).unwrap();
        assert_eq!(cursor.event_index, 0);
        assert_eq!(cursor.source, CursorSource::LastEventIdHeader);
        assert!(cursor.is_resume());
    }
}
