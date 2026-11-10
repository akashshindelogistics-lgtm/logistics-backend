//! Prometheus metrics for the HTTP API, scraped from `GET /metrics`.
//!
//! Hand-rolled text exposition format: a request counter, a latency histogram
//! and two scan counters don't justify a metrics crate. Business numbers
//! (dispatches, stock, invoices…) are not here: Grafana reads them from MySQL
//! directly (`deploy/k8s/monitoring/`).

use actix_web::{HttpResponse, Responder, get};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Upper bounds in seconds; AI calls account for the long tail.
const BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

#[derive(Default)]
struct Histogram {
    buckets: [u64; BUCKETS.len()],
    sum: f64,
    count: u64,
}

struct Registry {
    /// (method, route pattern, status) → count
    requests: BTreeMap<(String, String, u16), u64>,
    /// (method, route pattern) → latency
    durations: BTreeMap<(String, String), Histogram>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    requests: BTreeMap::new(),
    durations: BTreeMap::new(),
});

pub static DELAY_ALERT_SCANS_OK: AtomicU64 = AtomicU64::new(0);
pub static DELAY_ALERT_SCANS_FAILED: AtomicU64 = AtomicU64::new(0);

/// Record one finished request. `route` is the matched pattern
/// (`/api/vehicles/{id}`), never the raw path, so label cardinality stays
/// bounded; unmatched paths all share one label.
pub fn observe(method: &str, route: Option<String>, status: u16, elapsed: Duration) {
    let route = route.unwrap_or_else(|| "unmatched".to_string());
    let secs = elapsed.as_secs_f64();
    let mut reg = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    *reg.requests
        .entry((method.to_string(), route.clone(), status))
        .or_default() += 1;
    let h = reg
        .durations
        .entry((method.to_string(), route))
        .or_default();
    // Buckets are stored non-cumulative and summed at render time.
    if let Some(i) = BUCKETS.iter().position(|b| secs <= *b) {
        h.buckets[i] += 1;
    }
    h.sum += secs;
    h.count += 1;
}

pub fn render() -> String {
    let reg = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    let mut out = String::new();
    out.push_str("# HELP http_requests_total HTTP requests handled by the API.\n");
    out.push_str("# TYPE http_requests_total counter\n");
    for ((method, route, status), n) in &reg.requests {
        let _ = writeln!(
            out,
            "http_requests_total{{method=\"{method}\",route=\"{route}\",status=\"{status}\"}} {n}"
        );
    }
    out.push_str(
        "# HELP http_request_duration_seconds Time from request received to response ready.\n",
    );
    out.push_str("# TYPE http_request_duration_seconds histogram\n");
    for ((method, route), h) in &reg.durations {
        let labels = format!("method=\"{method}\",route=\"{route}\"");
        let mut cumulative = 0;
        for (bound, n) in BUCKETS.iter().zip(h.buckets) {
            cumulative += n;
            let _ = writeln!(
                out,
                "http_request_duration_seconds_bucket{{{labels},le=\"{bound}\"}} {cumulative}"
            );
        }
        let _ = writeln!(
            out,
            "http_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}",
            h.count
        );
        let _ = writeln!(
            out,
            "http_request_duration_seconds_sum{{{labels}}} {}",
            h.sum
        );
        let _ = writeln!(
            out,
            "http_request_duration_seconds_count{{{labels}}} {}",
            h.count
        );
    }
    out.push_str("# HELP delay_alert_scans_total Background delay-alert scans by outcome.\n");
    out.push_str("# TYPE delay_alert_scans_total counter\n");
    let _ = writeln!(
        out,
        "delay_alert_scans_total{{result=\"ok\"}} {}",
        DELAY_ALERT_SCANS_OK.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "delay_alert_scans_total{{result=\"error\"}} {}",
        DELAY_ALERT_SCANS_FAILED.load(Ordering::Relaxed)
    );
    out
}

/// Unauthenticated, like `/api/health`; kept outside `/api` so the
/// dashboard's nginx doesn't proxy it, and blocked in `deploy/Caddyfile`.
#[get("/metrics")]
pub async fn metrics_endpoint() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(render())
}
