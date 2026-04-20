//! T-3B0B94B8 — Idempotent backfill of historical `conversation_events`
//! rows into the v2 Anthropic 8-event vocabulary.
//!
//! # Why this exists
//!
//! Every `conversation_events` row has an `event_schema_version` column
//! (added in v44):
//!
//!   - `1` (or `NULL`) — legacy internal `StreamEvent` vocabulary
//!     (`Text`, `ToolUse`, `ToolResult`, `Thinking`, `Status`, `Result`,
//!     router events, etc.). These rows were written by the worker
//!     before the v2 wire format shipped.
//!   - `2` — Anthropic Messages API 8-event vocabulary
//!     (`message_start`, `content_block_delta`, `message_stop`, ...),
//!     matching what Anthropic's own streaming API emits.
//!
//! The live `ConversationWorker` runs in `EventVocabMode::Dual` in
//! production, so new turns land on both wires. Historical rows written
//! before the dual-write switch are v1 only. This binary translates
//! those historical rows into v2 so readers that only understand the
//! Anthropic vocabulary (mobile clients, downstream processors) have a
//! complete record.
//!
//! # Design
//!
//! - **Translator reuse.** We call `AnthropicTranslator::translate` exactly
//!   the same way `ConversationWorker::write_event_dual` does — same enum,
//!   same stateful instance per turn. Backfilled v2 rows are byte-identical
//!   to what dual-write would have produced if the worker had run today.
//!
//! - **Cutover per conversation.** For each conversation we discover the
//!   lowest v1 event_index that does NOT yet have any v2 siblings, then
//!   translate v1 rows up to (but not including) the first v2 row. This
//!   avoids re-emitting v2 events for turns the live dual-write path
//!   already covered.
//!
//! - **Idempotency.** Two layers:
//!
//!   1. `backfill_progress(source_event_id PRIMARY KEY, ...)` records
//!      every source v1 row that has been fully translated. A `LEFT JOIN`
//!      in the batch SELECT skips rows already in this table.
//!   2. Every v2 row we emit carries
//!      `backfill_source_event_id = <source v1 rowid>`. Before translating
//!      a source row we `DELETE FROM conversation_events WHERE
//!      backfill_source_event_id = ?` so a crash mid-translation leaves
//!      no partial output behind.
//!
//!   Net effect: re-running the backfill on a fully-processed DB is a
//!   no-op (0 rows emitted, 0 rows deleted). Re-running on a
//!   partially-processed DB resumes from the first un-marked source row
//!   and re-does its v2 siblings from scratch.
//!
//! - **Concurrency safety.** Live `ConversationWorker` writes never
//!   include `backfill_source_event_id` — the column is `NULL` on every
//!   live row. The cleanup sweep targets `backfill_source_event_id = ?`,
//!   so it can never touch a live dual-write row, even if the backfill
//!   runs against a live prod DB. Event-index allocation is serialized
//!   by the `BEGIN IMMEDIATE` inside `insert_conversation_event_backfill`,
//!   so concurrent live writers interleave correctly with the backfill.
//!
//! # CLI
//!
//! ```text
//! backfill_v2_events [--batch-size N] [--dry-run]
//! ```
//!
//! - `--batch-size N`   — source rows per SELECT round (default 1000).
//! - `--dry-run`        — count what would be processed, print the
//!   per-conversation cutovers, emit zero writes.
//!
//! # Exit codes
//!
//! - `0` — success (including `--dry-run`).
//! - `1` — any non-recoverable error (argument parsing, DB, translator).

use std::time::Instant;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::Value as JsonValue;
use sqlx::{Row, SqlitePool};
use tracing::{info, warn};

use agentic_api::agents::stream_event::StreamEvent;
use agentic_api::handlers::anthropic_translator::AnthropicTranslator;

/// Rows pulled per SELECT round when `--batch-size` is not given.
const DEFAULT_BATCH_SIZE: i64 = 1000;

/// Parsed CLI arguments.
#[derive(Debug, Clone)]
struct Args {
    batch_size: i64,
    dry_run: bool,
}

impl Args {
    /// Parse from `std::env::args`. Unknown flags, non-numeric batch
    /// sizes, and negative values all produce hard errors — we refuse
    /// to silently fall back to defaults on malformed input.
    fn parse() -> Result<Self> {
        let mut batch_size = DEFAULT_BATCH_SIZE;
        let mut dry_run = false;

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--batch-size" => {
                    let val = raw.get(i + 1).with_context(|| {
                        "--batch-size requires a positive integer argument".to_string()
                    })?;
                    let parsed: i64 = val
                        .parse()
                        .with_context(|| format!("--batch-size '{}' is not an integer", val))?;
                    if parsed <= 0 {
                        bail!("--batch-size must be > 0 (got {})", parsed);
                    }
                    batch_size = parsed;
                    i += 2;
                }
                "--dry-run" => {
                    dry_run = true;
                    i += 1;
                }
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => {
                    bail!("unknown argument: {}", other);
                }
            }
        }

        Ok(Self {
            batch_size,
            dry_run,
        })
    }
}

fn print_usage() {
    println!("backfill_v2_events — idempotent v1 → v2 conversation_events backfill");
    println!();
    println!("USAGE:");
    println!("    backfill_v2_events [--batch-size N] [--dry-run]");
    println!();
    println!("OPTIONS:");
    println!(
        "    --batch-size N   source rows per SELECT round (default {})",
        DEFAULT_BATCH_SIZE
    );
    println!("    --dry-run        count work only, emit no writes");
    println!("    -h, --help       print this help");
}

/// Per-conversation v1 → v2 cutover: only v1 rows with
/// `event_index < first_v2_event_index` are eligible for backfill.
/// A conversation with no v2 rows yet gets `i32::MAX` (all v1 rows
/// are eligible).
#[derive(Debug, Clone, Copy)]
struct ConversationCutover {
    first_v2_event_index: i32,
}

/// One eligible v1 source row pulled from the DB.
#[derive(Debug, Clone)]
struct SourceRow {
    id: i64,
    conversation_id: String,
    event_index: i32,
    event_data: String,
}

/// Running totals for the final summary.
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    rows_processed: u64,
    v2_events_emitted: u64,
    rows_skipped_past_cutover: u64,
    cleanup_deletes: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse()?;
    info!(
        batch_size = args.batch_size,
        dry_run = args.dry_run,
        "backfill_v2_events starting"
    );

    // Reuse the main crate's DB init so the schema (incl. migration v48
    // that created `backfill_progress` and `backfill_source_event_id`)
    // is guaranteed present before we read anything.
    let pool = ticketing_system::init_db()
        .await
        .context("init_db failed")?;

    let stats = run_backfill(&pool, &args).await?;

    info!(
        rows_processed = stats.rows_processed,
        v2_events_emitted = stats.v2_events_emitted,
        rows_skipped_past_cutover = stats.rows_skipped_past_cutover,
        cleanup_deletes = stats.cleanup_deletes,
        dry_run = args.dry_run,
        "backfill_v2_events complete"
    );

    Ok(())
}

/// Main loop: pull batches of unprocessed v1 rows, translate them, record
/// progress. Stops when a SELECT returns zero rows.
async fn run_backfill(pool: &SqlitePool, args: &Args) -> Result<Stats> {
    let total_eligible: i64 = count_eligible(pool).await?;
    info!(total_eligible, "scanned eligible v1 rows");

    let start = Instant::now();
    let mut stats = Stats::default();

    // Cache per-conversation cutovers to avoid re-querying for every row.
    // Conversations are long-lived but finite; in production there are
    // on the order of low thousands, so an unbounded HashMap is fine.
    let mut cutover_cache: std::collections::HashMap<String, ConversationCutover> =
        std::collections::HashMap::new();

    // Translator state is per-conversation, per-assistant-turn. We reset
    // it whenever the conversation changes (new turn under a different
    // conversation) or when a `Result`/terminal `Status` fires inside
    // the translator itself (it tracks `message_open` internally).
    let mut translator_state: Option<(String, AnthropicTranslator)> = None;

    // Cursor over source row ids. Start at 0 so the first batch picks up
    // the lowest-id unprocessed row; advance to the max id seen each
    // round so we don't re-scan rows we already filtered out.
    let mut cursor_id: i64 = 0;
    let mut last_log_at_rows: u64 = 0;

    loop {
        let batch = fetch_batch(pool, cursor_id, args.batch_size).await?;
        if batch.is_empty() {
            break;
        }
        let batch_max_id = batch
            .iter()
            .map(|r| r.id)
            .max()
            .expect("non-empty batch has a max id");

        for row in &batch {
            let cutover = match cutover_cache.get(&row.conversation_id) {
                Some(c) => *c,
                None => {
                    let c = fetch_cutover(pool, &row.conversation_id).await?;
                    cutover_cache.insert(row.conversation_id.clone(), c);
                    c
                }
            };

            // Conversations that have never seen a v2 row (first_v2_event_index == i32::MAX)
            // must NOT be backfilled — these are pre-dual-write
            // conversations whose turns were never translated live. We
            // still translate them, because that's the entire point of
            // the backfill. The "no_v2" skip only fires if we want to
            // explicitly gate on presence of dual-write output; instead
            // we treat i32::MAX as "all v1 rows eligible".
            if row.event_index >= cutover.first_v2_event_index {
                stats.rows_skipped_past_cutover += 1;
                continue;
            }

            let event = match parse_stream_event(&row.event_data) {
                Ok(ev) => ev,
                Err(e) => {
                    // Malformed historical payload — log and skip. We do
                    // NOT insert a `backfill_progress` row so a future
                    // re-run (after a fix) can re-attempt. We also do
                    // NOT abort: one bad row must not block the whole
                    // backfill.
                    warn!(
                        source_id = row.id,
                        conversation_id = %row.conversation_id,
                        event_index = row.event_index,
                        error = %e,
                        "skipping unparseable v1 event payload"
                    );
                    continue;
                }
            };

            // Ensure the translator is bound to the current conversation.
            // Swapping to a new conversation means we're starting a fresh
            // sequence of turns for a different session — initialize a
            // brand-new translator so message_ids / block indices reset.
            let same_conv = translator_state
                .as_ref()
                .map(|(cid, _)| cid == &row.conversation_id)
                .unwrap_or(false);
            if !same_conv {
                translator_state = Some((
                    row.conversation_id.clone(),
                    AnthropicTranslator::new(&row.conversation_id),
                ));
            }
            let (_cid, translator) = translator_state.as_mut().expect("translator bound above");

            // Cleanup sweep: remove any v2 rows left over from a prior
            // crashed run of this exact source id. `backfill_source_event_id`
            // only hits backfill output — live rows have it NULL — so this
            // is safe against the live dual-write writer.
            let deleted = cleanup_prior_output(pool, row.id, args.dry_run).await?;
            stats.cleanup_deletes += deleted;

            // Translate + persist.
            let anthropic_events = translator.translate(&event);
            let mut emitted = 0u64;
            for ae in &anthropic_events {
                let ae_type = ae.event_type();
                let ae_json = serde_json::to_string(ae).context("serialize AnthropicEvent")?;

                if args.dry_run {
                    emitted += 1;
                    continue;
                }

                ticketing_system::conversations::insert_conversation_event_backfill(
                    pool,
                    &row.conversation_id,
                    ae_type,
                    ae_json.as_bytes(),
                    None, // mime defaults to application/json
                    2,    // event_schema_version = 2
                    Some(ae_type),
                    row.id,
                )
                .await
                .with_context(|| {
                    format!(
                        "insert v2 row for source_id={} conversation={} index={}",
                        row.id, row.conversation_id, row.event_index
                    )
                })?;

                emitted += 1;
            }

            stats.v2_events_emitted += emitted;
            stats.rows_processed += 1;

            // Mark the source row as fully processed. Even if the
            // translator produced zero output (router/title events), we
            // record progress so a re-run won't retry this row.
            if !args.dry_run {
                record_progress(pool, row.id, emitted as i64).await?;
            }

            // Progress heartbeat every 1000 processed rows.
            if stats.rows_processed / 1000 > last_log_at_rows / 1000 {
                let elapsed = start.elapsed().as_secs_f64();
                let rate = if elapsed > 0.0 {
                    stats.rows_processed as f64 / elapsed
                } else {
                    0.0
                };
                info!(
                    rows_processed = stats.rows_processed,
                    total = total_eligible,
                    v2_events_emitted = stats.v2_events_emitted,
                    elapsed_secs = elapsed as u64,
                    rate_per_sec = rate as u64,
                    "backfill progress"
                );
                last_log_at_rows = stats.rows_processed;
            }
        }

        cursor_id = batch_max_id;
    }

    Ok(stats)
}

/// Total v1 source rows not yet in `backfill_progress`. Used only for
/// the progress log — the main loop doesn't depend on this number.
async fn count_eligible(pool: &SqlitePool) -> Result<i64> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) as n
        FROM conversation_events ce
        LEFT JOIN backfill_progress bp ON bp.source_event_id = ce.id
        WHERE (ce.event_schema_version IS NULL OR ce.event_schema_version = 1)
          AND bp.source_event_id IS NULL
        "#,
    )
    .fetch_one(pool)
    .await
    .context("count_eligible query failed")?;

    let n: i64 = row.try_get("n")?;
    Ok(n)
}

/// Pull the next `limit` v1 rows past `cursor_id` that are not already
/// marked processed. Ordered by id ASC so crash-resume picks up exactly
/// where we left off.
async fn fetch_batch(pool: &SqlitePool, cursor_id: i64, limit: i64) -> Result<Vec<SourceRow>> {
    let rows = sqlx::query(
        r#"
        SELECT ce.id, ce.conversation_id, ce.event_index, ce.event_data
        FROM conversation_events ce
        LEFT JOIN backfill_progress bp ON bp.source_event_id = ce.id
        WHERE ce.id > ?
          AND (ce.event_schema_version IS NULL OR ce.event_schema_version = 1)
          AND bp.source_event_id IS NULL
        ORDER BY ce.id ASC
        LIMIT ?
        "#,
    )
    .bind(cursor_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("fetch_batch query failed")?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(SourceRow {
            id: r.try_get("id")?,
            conversation_id: r.try_get("conversation_id")?,
            event_index: r.try_get("event_index")?,
            event_data: r.try_get("event_data")?,
        });
    }
    Ok(out)
}

/// Look up the lowest event_index with `event_schema_version = 2` for a
/// conversation. Returns `i32::MAX` when there are no v2 rows yet —
/// every v1 row is eligible.
async fn fetch_cutover(pool: &SqlitePool, conversation_id: &str) -> Result<ConversationCutover> {
    let row = sqlx::query(
        r#"
        SELECT MIN(event_index) AS idx
        FROM conversation_events
        WHERE conversation_id = ?
          AND event_schema_version = 2
          AND backfill_source_event_id IS NULL
        "#,
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
    .context("fetch_cutover query failed")?;

    // MIN on empty set returns NULL. `try_get::<Option<i32>, _>` maps
    // NULL → Ok(None) and a real value → Ok(Some(v)). We deliberately
    // do NOT use `.ok()` here because that would mask a genuine decode
    // error as "no v2 rows" and treat the conversation as fully
    // eligible when in fact we couldn't read the column.
    let idx: Option<i32> = row
        .try_get::<Option<i32>, _>("idx")
        .context("fetch_cutover: decode MIN(event_index)")?;
    let first_v2_event_index = idx.unwrap_or(i32::MAX);
    Ok(ConversationCutover {
        first_v2_event_index,
    })
}

/// Delete any v2 rows already tagged with this source id. Returns the
/// number of rows deleted. In `--dry-run` mode this runs a SELECT
/// COUNT(*) instead so the dry-run path shows realistic cleanup
/// numbers without mutating.
async fn cleanup_prior_output(pool: &SqlitePool, source_id: i64, dry_run: bool) -> Result<u64> {
    if dry_run {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*) AS n
            FROM conversation_events
            WHERE backfill_source_event_id = ?
            "#,
        )
        .bind(source_id)
        .fetch_one(pool)
        .await
        .context("cleanup count query failed")?;
        let n: i64 = row.try_get("n")?;
        return Ok(n as u64);
    }

    let res = sqlx::query(
        r#"
        DELETE FROM conversation_events
        WHERE backfill_source_event_id = ?
        "#,
    )
    .bind(source_id)
    .execute(pool)
    .await
    .context("cleanup delete failed")?;

    Ok(res.rows_affected())
}

/// Record that a source row has been translated. UPSERT semantics so a
/// re-run of a partially-processed row (after cleanup_prior_output)
/// overwrites the previous progress row with the fresh count.
async fn record_progress(pool: &SqlitePool, source_id: i64, v2_events_emitted: i64) -> Result<()> {
    let now_ts = Utc::now().timestamp();
    sqlx::query(
        r#"
        INSERT INTO backfill_progress (source_event_id, processed_at, v2_events_emitted)
        VALUES (?, ?, ?)
        ON CONFLICT(source_event_id) DO UPDATE SET
            processed_at = excluded.processed_at,
            v2_events_emitted = excluded.v2_events_emitted
        "#,
    )
    .bind(source_id)
    .bind(now_ts)
    .bind(v2_events_emitted)
    .execute(pool)
    .await
    .context("record_progress insert failed")?;
    Ok(())
}

/// Re-hydrate a historical v1 `event_data` JSON string into a
/// `StreamEvent`. Payloads offloaded to `event_blobs` carry a sentinel
/// of shape `{"$blob": N, ...}` — we refuse to translate those because
/// the backfill binary must not hit the blob read path (that would
/// require re-implementing `load_event_payload`, which the ticket
/// explicitly forbids as a shortcut). In practice historical v1 rows
/// predate the 4096-byte blob threshold (v46), so this path is only
/// exercised by pathologically large post-v46 v1 writes — of which we
/// have none in prod.
fn parse_stream_event(event_data: &str) -> Result<StreamEvent> {
    let trimmed = event_data.trim();
    if trimmed.is_empty() {
        bail!("empty event_data");
    }

    // Detect the blob sentinel and refuse early with a clear error.
    let as_value: JsonValue = serde_json::from_str(trimmed)
        .with_context(|| format!("event_data is not valid JSON: {}", truncate(trimmed, 120)))?;
    if let Some(obj) = as_value.as_object() {
        if obj.contains_key("$blob") {
            bail!("event_data is a blob sentinel — blob-backed v1 rows are not supported by this backfill");
        }
    }

    let ev: StreamEvent = serde_json::from_value(as_value)
        .with_context(|| format!("event_data not a StreamEvent: {}", truncate(trimmed, 120)))?;
    Ok(ev)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// Build a fresh in-memory pool with exactly the tables the backfill
    /// binary reads and writes. We purposely do NOT call
    /// `ticketing_system::init_db` because that reaches for
    /// `~/.agentic-flowstate/*.db` and runs all 40+ migrations — too
    /// heavy and non-hermetic for a unit test.
    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("in-memory sqlite connect");

        // Foreign keys + synchronous behaviour match production
        // (`init_db` sets these too).
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();

        // Minimal parent table for the FK.
        sqlx::query(
            r#"
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE conversation_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                event_index INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                event_data TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                event_schema_version INTEGER NOT NULL DEFAULT 1,
                anthropic_event_type TEXT,
                backfill_source_event_id INTEGER
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE INDEX idx_conversation_events_conv ON conversation_events(conversation_id)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE INDEX idx_conversation_events_idx ON conversation_events(conversation_id, event_index)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE INDEX idx_events_backfill_src ON conversation_events(backfill_source_event_id) WHERE backfill_source_event_id IS NOT NULL",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE event_blobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id INTEGER NOT NULL REFERENCES conversation_events(id) ON DELETE CASCADE,
                mime TEXT NOT NULL,
                bytes BLOB NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE backfill_progress (
                source_event_id INTEGER PRIMARY KEY,
                processed_at INTEGER NOT NULL,
                v2_events_emitted INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    /// Seed one conversation + three v1 rows forming a complete turn:
    /// Text → ToolUse → Result. Returns (conversation_id, source row ids).
    async fn seed_v1_turn(pool: &SqlitePool) -> (String, Vec<i64>) {
        let conv_id = "conv-test-1".to_string();
        let now = 1_700_000_000i64;
        sqlx::query(
            "INSERT INTO conversations (id, user_id, status, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&conv_id)
        .bind("u-test")
        .bind("active")
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        let events = [
            (
                "text",
                serde_json::json!({"type":"text","content":"Hi there"}).to_string(),
            ),
            (
                "tool_use",
                serde_json::json!({
                    "type":"tool_use",
                    "id":"toolu_1",
                    "name":"search",
                    "input":{"q":"rust"}
                })
                .to_string(),
            ),
            (
                "result",
                serde_json::json!({
                    "type":"result",
                    "session_id":"sess-1",
                    "status":"completed",
                    "is_error":false
                })
                .to_string(),
            ),
        ];

        let mut ids = Vec::new();
        for (i, (etype, edata)) in events.iter().enumerate() {
            let row = sqlx::query(
                r#"
                INSERT INTO conversation_events
                    (conversation_id, event_index, event_type, event_data, created_at, event_schema_version, anthropic_event_type, backfill_source_event_id)
                VALUES (?, ?, ?, ?, ?, 1, NULL, NULL)
                RETURNING id
                "#,
            )
            .bind(&conv_id)
            .bind(i as i32)
            .bind(etype)
            .bind(edata)
            .bind(now)
            .fetch_one(pool)
            .await
            .unwrap();
            let id: i64 = row.try_get("id").unwrap();
            ids.push(id);
        }
        (conv_id, ids)
    }

    /// Feed the same StreamEvent sequence through a fresh translator
    /// (mirroring `ConversationWorker::write_event_dual`) and return
    /// the ordered list of (event_type, serialized JSON). The backfill
    /// output must match this byte-for-byte.
    fn live_dual_write_output(conv_id: &str) -> Vec<(&'static str, String)> {
        let mut t = AnthropicTranslator::new(conv_id);
        let v1_events = vec![
            StreamEvent::Text {
                content: "Hi there".into(),
            },
            StreamEvent::ToolUse {
                id: "toolu_1".into(),
                name: "search".into(),
                input: serde_json::json!({"q":"rust"}),
            },
            StreamEvent::Result {
                session_id: "sess-1".into(),
                status: "completed".into(),
                is_error: false,
            },
        ];
        let mut out = Vec::new();
        for ev in &v1_events {
            for ae in t.translate(ev) {
                let ae_type = ae.event_type();
                let ae_json = serde_json::to_string(&ae).unwrap();
                out.push((ae_type, ae_json));
            }
        }
        out
    }

    #[tokio::test]
    async fn backfill_matches_live_dual_write_byte_for_byte() {
        let pool = fresh_pool().await;
        let (conv_id, _src_ids) = seed_v1_turn(&pool).await;

        let args = Args {
            batch_size: 1000,
            dry_run: false,
        };
        let stats = run_backfill(&pool, &args).await.unwrap();
        assert_eq!(stats.rows_processed, 3, "all 3 v1 rows processed");

        // Read back every v2 row this conversation now has, in event_index order.
        let rows = sqlx::query(
            r#"
            SELECT anthropic_event_type, event_data
            FROM conversation_events
            WHERE conversation_id = ?
              AND event_schema_version = 2
            ORDER BY event_index ASC
            "#,
        )
        .bind(&conv_id)
        .fetch_all(&pool)
        .await
        .unwrap();

        let actual: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                let t: String = r.try_get("anthropic_event_type").unwrap();
                let d: String = r.try_get("event_data").unwrap();
                (t, d)
            })
            .collect();

        let expected = live_dual_write_output(&conv_id);
        assert_eq!(
            actual.len(),
            expected.len(),
            "backfill emitted {} v2 rows, live dual-write would have emitted {}",
            actual.len(),
            expected.len()
        );
        for (i, ((a_type, a_json), (e_type, e_json))) in
            actual.iter().zip(expected.iter()).enumerate()
        {
            assert_eq!(a_type.as_str(), *e_type, "type mismatch at v2 row {}", i);
            assert_eq!(a_json, e_json, "json mismatch at v2 row {}", i);
        }
    }

    #[tokio::test]
    async fn backfill_is_idempotent_on_rerun() {
        let pool = fresh_pool().await;
        let (conv_id, _src_ids) = seed_v1_turn(&pool).await;

        let args = Args {
            batch_size: 1000,
            dry_run: false,
        };

        // First run: should emit a non-zero number of v2 rows.
        let first = run_backfill(&pool, &args).await.unwrap();
        assert!(first.v2_events_emitted > 0, "first run must emit v2 events");
        assert_eq!(first.rows_processed, 3);

        // Snapshot row count after first run.
        let before_rerun: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM conversation_events WHERE conversation_id = ? AND event_schema_version = 2",
        )
        .bind(&conv_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();

        // Second run: must be a no-op — backfill_progress skips every row.
        let second = run_backfill(&pool, &args).await.unwrap();
        assert_eq!(
            second.rows_processed, 0,
            "second run must skip all already-processed rows"
        );
        assert_eq!(
            second.v2_events_emitted, 0,
            "second run must emit zero v2 events"
        );
        assert_eq!(
            second.cleanup_deletes, 0,
            "second run must not delete anything"
        );

        // And the v2 row count is unchanged — no duplicates.
        let after_rerun: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM conversation_events WHERE conversation_id = ? AND event_schema_version = 2",
        )
        .bind(&conv_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
        assert_eq!(before_rerun, after_rerun, "no duplicate v2 rows on re-run");
    }

    #[tokio::test]
    async fn dry_run_emits_zero_writes() {
        let pool = fresh_pool().await;
        let (conv_id, _src_ids) = seed_v1_turn(&pool).await;

        let args = Args {
            batch_size: 1000,
            dry_run: true,
        };
        let stats = run_backfill(&pool, &args).await.unwrap();
        assert_eq!(stats.rows_processed, 3);
        assert!(stats.v2_events_emitted > 0, "dry-run still counts emission");

        // But zero rows were actually written.
        let n: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM conversation_events WHERE conversation_id = ? AND event_schema_version = 2",
        )
        .bind(&conv_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
        assert_eq!(n, 0, "dry-run must not write any v2 rows");

        // And backfill_progress is empty — dry-run doesn't mark rows done,
        // so a subsequent real run picks them up.
        let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM backfill_progress")
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(n, 0, "dry-run must not mark rows as processed");
    }

    #[tokio::test]
    async fn cutover_skips_rows_already_covered_by_dual_write() {
        let pool = fresh_pool().await;
        let (conv_id, _src_ids) = seed_v1_turn(&pool).await;

        // Pretend the worker already dual-wrote a v2 row at event_index=0
        // (the earliest). That means every historical v1 row with
        // event_index >= 0 is past the cutover and must be skipped.
        let now = 1_700_000_000i64;
        sqlx::query(
            r#"
            INSERT INTO conversation_events
                (conversation_id, event_index, event_type, event_data, created_at, event_schema_version, anthropic_event_type, backfill_source_event_id)
            VALUES (?, ?, ?, ?, ?, 2, ?, NULL)
            "#,
        )
        .bind(&conv_id)
        .bind(100i32) // doesn't collide with seed rows 0/1/2
        .bind("message_start")
        .bind(r#"{"type":"message_start","message":{"id":"msg_x","type":"message","role":"assistant","content":[],"model":"","stop_reason":null,"stop_sequence":null}}"#)
        .bind(now)
        .bind("message_start")
        .execute(&pool)
        .await
        .unwrap();

        // cutover = min v2 event_index = 100; seed rows at 0/1/2 are
        // below it and should still be translated. Swap the test: make
        // the cutover lower than the seed rows.
        sqlx::query(
            r#"
            UPDATE conversation_events
            SET event_index = 0
            WHERE conversation_id = ? AND event_schema_version = 2
            "#,
        )
        .bind(&conv_id)
        .execute(&pool)
        .await
        .unwrap();

        let args = Args {
            batch_size: 1000,
            dry_run: false,
        };
        let stats = run_backfill(&pool, &args).await.unwrap();
        assert_eq!(
            stats.rows_skipped_past_cutover, 3,
            "all 3 seed rows sit at or past the cutover of 0"
        );
        assert_eq!(stats.v2_events_emitted, 0);
    }
}
