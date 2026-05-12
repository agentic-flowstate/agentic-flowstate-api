//! APNs (Apple Push Notification service) integration.
//!
//! Two distinct senders live here:
//!
//! * [`alert`] — user-visible alert pushes (agent-completion banners), built
//!   on `reqwest` + HTTP/2 with a hand-rolled JWT. Enabled by
//!   `APNS_ALERT_ENABLED=true`; when enabled, it uses the same required
//!   `APNS_KEY_ID` / `APNS_TEAM_ID` / `APNS_BUNDLE_ID` /
//!   `APNS_KEY_PATH` / `APNS_USE_SANDBOX` contract as silent push.
//! * [`silent`] — silent/content-available background wake pushes for the
//!   durable chat streaming pipeline, built on the `apns-h2` crate using
//!   JWT ES256 `.p8` key authentication. Payload is produced byte-for-byte
//!   to the spec `{"aps":{"content-available":1},"conv":"<id>","last":"<id>"}`.
//!
//! Top-level re-exports keep the historical `crate::apns::ApnsService`
//! call sites working unchanged.

pub mod alert;
pub mod silent;
pub mod silent_fanout;

pub use alert::ApnsService;
pub use silent::{ApnsClient, ApnsSilentError};
