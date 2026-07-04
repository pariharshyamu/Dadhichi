//! A tiny, dependency-free metrics registry.
//!
//! Counters (monotonic) and gauges (arbitrary) are keyed by name and updated
//! from any thread. A snapshot can be serialised and shipped to an OpenTelemetry
//! collector, but the registry itself imposes no exporter — offline-first,
//! measurable without a backend.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// A thread-safe registry of counters and gauges.
#[derive(Debug, Default)]
pub struct Metrics {
    counters: Mutex<BTreeMap<String, AtomicU64>>,
    gauges: Mutex<BTreeMap<String, AtomicI64>>,
}

impl Metrics {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `n` to the counter `name` (creating it at zero if new).
    pub fn incr(&self, name: &str, n: u64) {
        let map = self.counters.lock().expect("metrics poisoned");
        if let Some(c) = map.get(name) {
            c.fetch_add(n, Ordering::Relaxed);
        } else {
            drop(map);
            self.counters
                .lock()
                .expect("metrics poisoned")
                .entry(name.to_string())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The current value of counter `name` (0 if absent).
    pub fn counter(&self, name: &str) -> u64 {
        self.counters
            .lock()
            .expect("metrics poisoned")
            .get(name)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Set the gauge `name` to `value`.
    pub fn set_gauge(&self, name: &str, value: i64) {
        self.gauges
            .lock()
            .expect("metrics poisoned")
            .entry(name.to_string())
            .or_insert_with(|| AtomicI64::new(0))
            .store(value, Ordering::Relaxed);
    }

    /// The current value of gauge `name` (0 if absent).
    pub fn gauge(&self, name: &str) -> i64 {
        self.gauges
            .lock()
            .expect("metrics poisoned")
            .get(name)
            .map(|g| g.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// A JSON snapshot of every metric, suitable for export.
    pub fn snapshot(&self) -> serde_json::Value {
        let counters: BTreeMap<String, u64> = self
            .counters
            .lock()
            .expect("metrics poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
            .collect();
        let gauges: BTreeMap<String, i64> = self
            .gauges
            .lock()
            .expect("metrics poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
            .collect();
        serde_json::json!({ "counters": counters, "gauges": gauges })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let m = Metrics::new();
        m.incr("agent.runs", 1);
        m.incr("agent.runs", 2);
        assert_eq!(m.counter("agent.runs"), 3);
        assert_eq!(m.counter("never.touched"), 0);
    }

    #[test]
    fn gauges_hold_last_value() {
        let m = Metrics::new();
        m.set_gauge("memory.items", 10);
        m.set_gauge("memory.items", 4);
        assert_eq!(m.gauge("memory.items"), 4);
    }

    #[test]
    fn snapshot_serialises_all_metrics() {
        let m = Metrics::new();
        m.incr("tokens", 42);
        m.set_gauge("open.files", 3);
        let snap = m.snapshot();
        assert_eq!(snap["counters"]["tokens"], 42);
        assert_eq!(snap["gauges"]["open.files"], 3);
    }
}
