//! # dadhichi-telemetry
//!
//! **Observability**, offline-first. A dependency-free [`Metrics`] registry
//! (counters and gauges) measures the running IDE, and a [`CrashReporter`]
//! captures panics into structured [`CrashReport`]s. A snapshot serialises to
//! JSON for an OpenTelemetry exporter, but nothing leaves the machine without
//! explicit [`Consent`].

pub mod crash;
pub mod metrics;

pub use crash::{Consent, CrashReport, CrashReporter};
pub use metrics::Metrics;
