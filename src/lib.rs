//! Minimal library surface for out-of-process binaries.
//!
//! The primary artifact of this crate is the `agentic_api` server
//! binary (`src/main.rs`). Some operational tools need to reuse a
//! narrow slice of the server's logic without pulling in the full
//! startup surface (Axum routers, APNs clients, email fetchers, etc.).
//!
//! This library exposes ONLY the minimum to support those tools:
//!
//!   - `agents::anthropic_events` — the Anthropic 8-event vocabulary.
//!   - `agents::stream_event` — the internal `StreamEvent` enum that
//!     the live worker emits and that historical v1 rows carry.
//!   - `handlers::anthropic_translator` — stateful translator from
//!     `StreamEvent` → [`agents::anthropic_events::AnthropicEvent`].
//!
//! Everything else stays inside `main.rs`'s module tree.
//!
//! Consumers:
//!
//!   - `bin/backfill_v2_events.rs` — T-3B0B94B8 idempotent backfill
//!     of `event_schema_version=1` rows into the v2 Anthropic vocabulary.
//!     Reuses the exact translator that `ConversationWorker` calls in
//!     dual-write mode, so backfilled rows match live output byte-for-byte.
//!
//! ## Module mounting strategy
//!
//! `anthropic_translator.rs` imports via `crate::agents::anthropic_events`
//! and `crate::agents::StreamEvent`. To keep those paths working without
//! changing any of `main.rs`'s module layout (and without pulling the
//! heavy `mod.rs` trees that declare cc-sdk / AWS / APNs modules), we
//! declare inline `pub mod agents` and `pub mod handlers` here and mount
//! only the three source files the translator needs. `#[path]` on the
//! inner modules resolves relative to the inline module's implicit
//! directory (`src/agents/` and `src/handlers/`), so we get exactly
//! those files without triggering the mod.rs declarations.

pub mod agents {
    #[path = "anthropic_events.rs"]
    pub mod anthropic_events;
    #[path = "stream_event.rs"]
    pub mod stream_event;

    // Re-export so `crate::agents::StreamEvent` resolves the same way
    // it does inside `main.rs` (via the `pub use` in `agents/mod.rs`).
    pub use stream_event::StreamEvent;
}

pub mod handlers {
    #[path = "anthropic_translator.rs"]
    pub mod anthropic_translator;
}
