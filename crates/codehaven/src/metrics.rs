//! A few Prometheus counters, served only on the internal listener.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub started: AtomicU64,
    pub responses_2xx: AtomicU64,
    pub responses_3xx: AtomicU64,
    pub responses_4xx: AtomicU64,
    pub responses_5xx: AtomicU64,
    pub in_flight: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { started: AtomicU64::new(unix_now()), ..Default::default() })
    }

    pub fn render(&self, build: &str) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        format!(
            "# TYPE codehaven_build_info gauge\ncodehaven_build_info{{version=\"{build}\"}} 1\n\
             # TYPE codehaven_start_time_seconds gauge\ncodehaven_start_time_seconds {}\n\
             # TYPE codehaven_http_in_flight gauge\ncodehaven_http_in_flight {}\n\
             # TYPE codehaven_http_responses_total counter\n\
             codehaven_http_responses_total{{class=\"2xx\"}} {}\n\
             codehaven_http_responses_total{{class=\"3xx\"}} {}\n\
             codehaven_http_responses_total{{class=\"4xx\"}} {}\n\
             codehaven_http_responses_total{{class=\"5xx\"}} {}\n",
            g(&self.started),
            g(&self.in_flight),
            g(&self.responses_2xx),
            g(&self.responses_3xx),
            g(&self.responses_4xx),
            g(&self.responses_5xx),
        )
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub async fn count(State(m): State<Arc<Metrics>>, req: Request, next: Next) -> Response {
    m.in_flight.fetch_add(1, Ordering::Relaxed);
    let resp = next.run(req).await;
    m.in_flight.fetch_sub(1, Ordering::Relaxed);
    let counter = match resp.status().as_u16() {
        200..=299 => &m.responses_2xx,
        300..=399 => &m.responses_3xx,
        400..=499 => &m.responses_4xx,
        _ => &m.responses_5xx,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters() {
        let m = Metrics::new();
        m.responses_2xx.fetch_add(3, Ordering::Relaxed);
        let text = m.render("0.1.0");
        assert!(text.contains("codehaven_http_responses_total{class=\"2xx\"} 3"));
        assert!(text.contains("codehaven_build_info{version=\"0.1.0\"} 1"));
    }
}
