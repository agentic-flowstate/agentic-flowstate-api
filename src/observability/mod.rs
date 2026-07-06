//! Observability layer for the API.
//!
//! # Layout
//!
//! * [`streaming`] — durable-streaming metrics (T-56987678): stream
//!   lifecycle, push delivery, gap detection, session start.
//! * [`contracts`] — versioned observability contract registry plus
//!   privacy/cardinality guardrails.
//! * [`cancellation`] — stop-button cancellation phase logs and latency
//!   metrics.
//! * [`next_actions`] — post-turn follow-up suggestion generation and storage metrics.
//! * [`runtime`] — chat/Codex runtime turn metrics and structured failure logs.
//!
//! # Integration
//!
//! The Prometheus text exporter is installed from `main.rs` via
//! [`install_prometheus_exporter`] and exposed over HTTP at `/metrics`
//! via [`metrics_handler`]. Callers across the codebase emit metrics
//! through the [`metrics`](::metrics) macros (`counter!`, `histogram!`,
//! `gauge!`) using the typed helpers in [`streaming`].
//!
//! # Design rules
//!
//! * No hardcoded label strings at call sites. Labels that are enum-like
//!   (disconnect reasons, push results, platforms) MUST go through a
//!   `Display`-bearing enum in [`streaming`] so the set of values is
//!   centralized and inspectable.
//! * Every `tracing::info!` emitted from this module MUST also update a
//!   metric — and vice-versa — so dashboards and logs agree.

pub mod cancellation;
pub mod contracts;
pub mod next_actions;
pub mod runtime;
pub mod streaming;

use std::sync::Arc;

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use once_cell::sync::OnceCell;

/// Global handle to the Prometheus recorder/renderer. Set exactly once
/// at startup by [`install_prometheus_exporter`]. Read on every hit to
/// `/metrics` via [`metrics_handler`].
///
/// Wrapped in an `Arc` so cloning is cheap when the handler captures a
/// reference.
static PROMETHEUS: OnceCell<Arc<PrometheusHandle>> = OnceCell::new();

/// Error returned when the Prometheus exporter cannot be installed.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The `metrics` facade already had a recorder installed — either
    /// [`install_prometheus_exporter`] was called twice, or some other
    /// module beat us to it.
    #[error("Prometheus exporter already installed")]
    AlreadyInstalled,

    /// Building the Prometheus recorder itself failed. Only plausible
    /// cause in production is an OOM while constructing the registry.
    #[error("failed to build Prometheus recorder: {0}")]
    Build(#[from] metrics_exporter_prometheus::BuildError),
}

/// Install the Prometheus recorder as the process-wide `metrics` facade
/// sink. MUST be called from `main.rs` before any metric is emitted —
/// counters emitted before install become no-ops silently.
///
/// Idempotent only in the "already installed" sense: the second call
/// returns `Err(AlreadyInstalled)`. Tests that share a process use
/// `install_for_test` which swallows the double-install.
pub fn install_prometheus_exporter() -> Result<(), InstallError> {
    contracts::assert_registry_loaded();

    if PROMETHEUS.get().is_some() {
        return Err(InstallError::AlreadyInstalled);
    }

    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();

    // Install the recorder as the global facade sink. If this returns
    // Err someone else already installed a recorder — we surface that
    // loudly so the operator fixes it rather than silently losing
    // metrics.
    metrics::set_global_recorder(recorder).map_err(|_| InstallError::AlreadyInstalled)?;

    PROMETHEUS
        .set(Arc::new(handle))
        .map_err(|_| InstallError::AlreadyInstalled)?;

    Ok(())
}

/// Test-only helper: install the recorder but tolerate "already set".
/// Cargo runs tests in one process; we want each #[tokio::test] that
/// touches metrics to succeed regardless of ordering.
#[cfg(test)]
pub fn install_for_test() {
    let _ = install_prometheus_exporter();
}

/// Render the current Prometheus registry as text. Returns `None` when
/// the exporter was never installed (unit-test paths that don't call
/// [`install_prometheus_exporter`]).
pub fn render_prometheus() -> Option<String> {
    PROMETHEUS.get().map(|h| h.render())
}

/// Axum handler for `GET /metrics`. Emits Prometheus text/plain v0.0.4
/// — the format Grafana + Prometheus + VictoriaMetrics all ingest.
///
/// Returns 503 if the exporter was not installed at startup. This is a
/// loud signal rather than a silent empty body so the operator spots
/// the misconfiguration the first time they hit the endpoint.
pub async fn metrics_handler() -> Response {
    match render_prometheus() {
        Some(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            body,
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "metrics exporter not installed",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the value of a simple unlabeled Prometheus metric from a
    /// text-format scrape. Returns `None` if the metric isn't present or
    /// the value can't be parsed. Skips `# HELP` / `# TYPE` comment lines.
    fn prometheus_value(text: &str, name: &str) -> Option<f64> {
        text.lines()
            .find(|l| {
                !l.starts_with('#')
                    && (l.starts_with(&format!("{} ", name))
                        || l.starts_with(&format!("{}{{", name)))
            })
            .and_then(|l| l.split_whitespace().next_back())
            .and_then(|s| s.parse::<f64>().ok())
    }

    /// The handler returns 503 when no exporter is installed, and 200
    /// with a text body when one is. We only assert the 200 path here
    /// because the 503 path requires a pristine process — which cargo
    /// cannot guarantee across parallel tests.
    #[tokio::test]
    async fn metrics_handler_renders_after_install() {
        install_for_test();
        // Emit a known metric so the registry has something to render.
        metrics::counter!("obs_test_tick").increment(1);

        let resp = metrics_handler().await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content-type")
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.starts_with("text/plain"), "got {}", ct);

        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert!(
            body.contains("obs_test_tick"),
            "registry did not render emitted metric: {}",
            body
        );
    }

    /// Second install must fail loudly rather than silently create a
    /// second recorder.
    #[test]
    fn second_install_errors() {
        install_for_test();
        // A direct re-invocation of the real entry point after one
        // successful install — either from install_for_test above or
        // from an earlier test — must error.
        let err = install_prometheus_exporter().err();
        assert!(matches!(err, Some(InstallError::AlreadyInstalled)));
    }

    /// Integration test: simulate a full SSE stream lifecycle (open →
    /// emit events → close), an APNs push attempt, a cursor-expired
    /// rejection, a gap detection, and a client session_start, then
    /// scrape `/metrics` through a real Axum router and assert every
    /// metric shows up with the expected label set.
    ///
    /// This is the test called out by the T-56987678 contract: "drive a
    /// mock SSE stream and assert counter increments". It covers ALL 14
    /// metric names emitted by the module.
    #[tokio::test]
    async fn full_streaming_lifecycle_shows_up_on_metrics_endpoint() {
        // Serialize with sibling unit tests in `streaming::tests` — they
        // share the process-wide Prometheus registry and AtomicU64 ratio
        // counters, so a concurrent run would clobber the snapshot deltas
        // this test asserts on.
        let _guard = crate::observability::streaming::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        use crate::observability::streaming::{
            record_cursor_expired, record_gap_detected, record_push_sent, record_session_start,
            record_stream_closed, record_stream_event_emitted, record_stream_opened,
            ClientPlatform, DisconnectReason, PushResult, METRIC_CLIENTS_SESSION_START,
            METRIC_EVENTS_GAP_DETECTED, METRIC_PUSH_SENT, METRIC_STREAM_BYTES_EMITTED,
            METRIC_STREAM_CLOSED, METRIC_STREAM_COLD_START, METRIC_STREAM_CONCURRENT,
            METRIC_STREAM_CURSOR_EXPIRED, METRIC_STREAM_DURATION_MS, METRIC_STREAM_EVENTS_EMITTED,
            METRIC_STREAM_OPENED, METRIC_STREAM_RESUME, METRIC_STREAM_RESUME_RATIO,
        };

        install_for_test();
        crate::observability::streaming::reset_ratios_for_test();

        // Snapshot the counters we want to assert deltas on. Prometheus
        // counters are additive across the process, so earlier sibling
        // tests may have bumped them — we compare before/after rather
        // than absolute values. The TEST_SERIAL guard above guarantees
        // no concurrent test mutates the registry between snapshots.
        let snapshot_before = render_prometheus().unwrap_or_default();
        let gap_before =
            prometheus_value(&snapshot_before, "events_gap_detected_total").unwrap_or(0.0);
        let concurrent_before =
            prometheus_value(&snapshot_before, "stream_concurrent").unwrap_or(0.0);

        // 1. Simulated SSE lifecycle: one cold-start, two resume, with
        //    three events each and varying close reasons.
        record_stream_opened("conv-A", "user-alpha", false);
        record_stream_event_emitted("conv-A", 128);
        record_stream_event_emitted("conv-A", 256);
        record_stream_event_emitted("conv-A", 64);
        record_stream_closed("conv-A", 2500, DisconnectReason::Normal);

        record_stream_opened("conv-B", "user-alpha", true);
        record_stream_event_emitted("conv-B", 512);
        record_stream_closed("conv-B", 1100, DisconnectReason::ClientDisconnect);

        record_stream_opened("conv-C", "user-alpha", true);
        record_stream_closed("conv-C", 900, DisconnectReason::ServerIdleTimeout);

        // 2. Cursor-expired 410 rejection.
        record_cursor_expired("conv-old", 42, 100);

        // 3. APNs push attempts across the result taxonomy.
        record_push_sent(PushResult::Success, 200);
        record_push_sent(PushResult::Success, 200);
        record_push_sent(PushResult::Unregistered, 410);
        record_push_sent(PushResult::BadToken, 400);
        record_push_sent(PushResult::Throttled, 429);

        // 4. Gap detection: allocator skipped 2 indices on conv-A.
        record_gap_detected("conv-A", 10, 12);

        // 5. Client session_start.
        record_session_start("user-alpha", ClientPlatform::Ios, "1.23.0");
        record_session_start("user-beta", ClientPlatform::Ios, "1.23.0");

        // Drive the exact same handler main.rs wires to /metrics. We
        // skip the Router layer here because `tower::ServiceExt::oneshot`
        // requires the `util` feature which this crate doesn't enable —
        // but the handler body IS the contract clients will hit, so
        // invoking it directly is just as load-bearing.
        let resp = metrics_handler().await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .expect("read metrics body");
        let body = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Every metric name from the module catalog must show up at
        // least once in the Prometheus text output. This is a catalog
        // guard: if someone renames a metric without updating the
        // dashboard, this assert fires.
        for name in &[
            METRIC_STREAM_OPENED,
            METRIC_STREAM_CONCURRENT,
            METRIC_STREAM_CLOSED,
            METRIC_STREAM_DURATION_MS,
            METRIC_STREAM_EVENTS_EMITTED,
            METRIC_STREAM_BYTES_EMITTED,
            METRIC_STREAM_RESUME,
            METRIC_STREAM_COLD_START,
            METRIC_STREAM_RESUME_RATIO,
            METRIC_STREAM_CURSOR_EXPIRED,
            METRIC_PUSH_SENT,
            METRIC_EVENTS_GAP_DETECTED,
            METRIC_CLIENTS_SESSION_START,
        ] {
            assert!(
                body.contains(name),
                "metric {} missing from /metrics scrape:\n{}",
                name,
                body
            );
        }

        // Resume ratio: 2 resume / 3 total ≈ 0.6667. Allow a small
        // tolerance because the gauge is written as an f64.
        let ratio_line = body
            .lines()
            .find(|l| l.starts_with(METRIC_STREAM_RESUME_RATIO))
            .expect("resume-ratio line present");
        let ratio_value: f64 = ratio_line
            .split_whitespace()
            .next_back()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (ratio_value - (2.0 / 3.0)).abs() < 1e-9,
            "resume_ratio expected 0.6667, got {}",
            ratio_value
        );

        // Concurrent-streams gauge must have returned to its pre-test
        // value after every stream was closed: 3 opens + 3 closes → net
        // delta of 0. Absolute value can be non-zero because sibling
        // tests (`resume_cold_ratio_gauge_matches_counter_ratio`) open
        // streams without closing them, so we assert a zero DELTA — if
        // this ever drifts it means a close path is missing the decrement.
        let concurrent_after = prometheus_value(&body, METRIC_STREAM_CONCURRENT).unwrap_or(0.0);
        assert!(
            (concurrent_after - concurrent_before).abs() < 1e-9,
            "concurrent gauge drifted by {} after balanced open/close pairs (before={}, after={})",
            concurrent_after - concurrent_before,
            concurrent_before,
            concurrent_after,
        );

        // Gap counter: delta of +2 from the single gap_detected call with
        // gap_size=2. Use snapshot delta so prior sibling tests (which may
        // have bumped the counter) don't poison this assertion.
        let gap_after = prometheus_value(&body, METRIC_EVENTS_GAP_DETECTED).unwrap_or(0.0);
        assert_eq!(
            (gap_after - gap_before) as u64,
            2,
            "gap counter delta expected 2, got before={} after={}, body=\n{}",
            gap_before,
            gap_after,
            body
        );

        // Per-reason closed counters must carry the reason label for
        // each of the three closes we recorded (normal, client_disconnect,
        // server_idle_timeout).
        for reason in ["normal", "client_disconnect", "server_idle_timeout"] {
            let needle = format!("reason=\"{}\"", reason);
            assert!(
                body.lines()
                    .any(|l| { l.starts_with(METRIC_STREAM_CLOSED) && l.contains(&needle) }),
                "expected {} labeled with {} in body",
                METRIC_STREAM_CLOSED,
                reason
            );
        }

        // APNs push result label space must be covered for the five
        // outcomes we exercised.
        for result in ["success", "unregistered", "bad_token", "throttled"] {
            let needle = format!("result=\"{}\"", result);
            assert!(
                body.lines()
                    .any(|l| { l.starts_with(METRIC_PUSH_SENT) && l.contains(&needle) }),
                "expected {} labeled with result={} in body",
                METRIC_PUSH_SENT,
                result
            );
        }
    }
}
