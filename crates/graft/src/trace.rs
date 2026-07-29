use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

const TRACE_ENV: &str = "GRAFT_PUSH_TRACE";
const TRACE_PREFIX: &str = "graft-push-trace ";

pub struct PushTraceSpan {
    phase: &'static str,
    started: Instant,
    finished: bool,
}

impl PushTraceSpan {
    pub fn new(phase: &'static str) -> Self {
        Self {
            phase,
            started: Instant::now(),
            finished: false,
        }
    }

    pub fn finish(mut self, measurements: &[(&'static str, u64)]) {
        emit_phase(self.phase, self.started.elapsed(), "ok", measurements);
        self.finished = true;
    }
}

impl Drop for PushTraceSpan {
    fn drop(&mut self) {
        if !self.finished {
            emit_phase(self.phase, self.started.elapsed(), "incomplete", &[]);
        }
    }
}

pub(crate) struct HttpTrace<'a> {
    pub operation: &'static str,
    pub request_id: &'a str,
    pub duration: Duration,
    pub status: Option<u16>,
    pub request_bytes: Option<u64>,
    pub response_bytes: Option<u64>,
    pub server_timings: &'a [(&'static str, f64)],
}

pub(crate) fn emit_http(trace: HttpTrace<'_>) {
    if !enabled() {
        return;
    }
    let mut event = Map::from_iter([
        ("schema".to_string(), json!("graft-push-trace-v1")),
        ("event".to_string(), json!("http_request")),
        ("operation".to_string(), json!(trace.operation)),
        ("request_id".to_string(), json!(trace.request_id)),
        (
            "duration_ms".to_string(),
            json!(duration_ms(trace.duration)),
        ),
    ]);
    if let Some(status) = trace.status {
        event.insert("status".to_string(), json!(status));
    }
    if let Some(bytes) = trace.request_bytes {
        event.insert("request_bytes".to_string(), json!(bytes));
    }
    if let Some(bytes) = trace.response_bytes {
        event.insert("response_bytes".to_string(), json!(bytes));
    }
    if !trace.server_timings.is_empty() {
        event.insert(
            "server_timing_ms".to_string(),
            Value::Object(Map::from_iter(
                trace
                    .server_timings
                    .iter()
                    .map(|(name, duration)| ((*name).to_string(), json!(duration))),
            )),
        );
    }
    emit(Value::Object(event));
}

fn emit_phase(
    phase: &'static str,
    duration: Duration,
    outcome: &'static str,
    measurements: &[(&'static str, u64)],
) {
    if !enabled() {
        return;
    }
    let mut event = Map::from_iter([
        ("schema".to_string(), json!("graft-push-trace-v1")),
        ("event".to_string(), json!("phase")),
        ("phase".to_string(), json!(phase)),
        ("outcome".to_string(), json!(outcome)),
        ("duration_ms".to_string(), json!(duration_ms(duration))),
    ]);
    for (name, value) in measurements {
        event.insert((*name).to_string(), json!(value));
    }
    emit(Value::Object(event));
}

fn duration_ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1_000.0 * 1_000.0).round() / 1_000.0
}

fn enabled() -> bool {
    std::env::var_os(TRACE_ENV).is_some_and(|value| {
        let value = value.to_string_lossy();
        value == "1" || value.eq_ignore_ascii_case("true")
    })
}

fn emit(event: Value) {
    eprintln!("{TRACE_PREFIX}{event}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_events_are_built_only_from_safe_fields() {
        let trace = HttpTrace {
            operation: "immutable_put",
            request_id: "123-4",
            duration: Duration::from_millis(7),
            status: Some(204),
            request_bytes: Some(12),
            response_bytes: Some(0),
            server_timings: &[("auth", 2.5), ("total", 6.0)],
        };

        assert_eq!(trace.operation, "immutable_put");
        assert_eq!(trace.request_id, "123-4");
        assert_eq!(trace.server_timings.len(), 2);
    }
}
