pub mod anthropic_events;
pub mod executor;
pub mod oneshot;
pub mod openai_text;
pub mod prompts;
pub mod types;
pub mod working_dir;
// `StreamEvent` lives in its own file so the backfill binary can mount
// just that one enum without pulling in `types.rs`'s static `AgentsConfig`
// initializer (which reads `agents.json` from disk at first access).
pub mod stream_event;

pub use executor::*;
pub use oneshot::run_oneshot;
pub use types::*;
pub use working_dir::resolve_working_dir;
// Re-export so the existing `crate::agents::StreamEvent` import path
// (used throughout the worker and translator) keeps working after the
// enum moved out of `types.rs` into its own file.
pub use stream_event::StreamEvent;
