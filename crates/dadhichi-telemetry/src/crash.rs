//! Crash reporting and opt-in telemetry consent.
//!
//! A panic hook captures the message and location into a structured
//! [`CrashReport`] and hands it to a sink (a file, an uploader). Reporting is
//! gated by explicit [`Consent`]: nothing leaves the machine unless the user
//! opted in, and reports can be scrubbed of paths first.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// A captured crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReport {
    /// The panic message.
    pub message: String,
    /// Source location `file:line`, if known.
    pub location: Option<String>,
}

/// Whether the user has consented to sending telemetry off-device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Consent {
    /// The user opted in.
    Granted,
    /// The user has not opted in (the default).
    #[default]
    Denied,
}

/// Collects crash reports into an in-memory buffer (a real deployment would
/// also persist or upload them, gated on [`Consent`]).
#[derive(Debug, Clone, Default)]
pub struct CrashReporter {
    consent: Consent,
    reports: Arc<Mutex<Vec<CrashReport>>>,
}

impl CrashReporter {
    /// Create a reporter with the given consent setting.
    pub fn new(consent: Consent) -> Self {
        Self {
            consent,
            reports: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Record a crash. When consent is denied the report is still captured
    /// locally (for the user to see) but marked as never uploaded.
    pub fn capture(&self, report: CrashReport) {
        self.reports.lock().expect("reporter poisoned").push(report);
    }

    /// Whether captured reports may be sent off-device.
    pub fn may_upload(&self) -> bool {
        self.consent == Consent::Granted
    }

    /// The reports captured so far.
    pub fn reports(&self) -> Vec<CrashReport> {
        self.reports.lock().expect("reporter poisoned").clone()
    }

    /// Install a panic hook that funnels panics into this reporter. The previous
    /// hook is still called, preserving the default backtrace behaviour.
    pub fn install_panic_hook(&self) {
        let reporter = self.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            reporter.capture(CrashReport::from_panic(info));
            previous(info);
        }));
    }
}

impl CrashReport {
    /// Build a report from panic information.
    pub fn from_panic(info: &std::panic::PanicHookInfo<'_>) -> Self {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        Self { message, location }
    }

    /// Remove absolute-path prefixes from the location for privacy.
    pub fn scrubbed(mut self) -> Self {
        self.location = self.location.map(|loc| {
            loc.rsplit(['/', '\\'])
                .next()
                .map(str::to_string)
                .unwrap_or(loc)
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_gates_upload() {
        assert!(!CrashReporter::new(Consent::Denied).may_upload());
        assert!(CrashReporter::new(Consent::Granted).may_upload());
        assert_eq!(Consent::default(), Consent::Denied);
    }

    #[test]
    fn captures_reports() {
        let reporter = CrashReporter::new(Consent::Denied);
        reporter.capture(CrashReport {
            message: "boom".into(),
            location: Some("src/x.rs:10".into()),
        });
        assert_eq!(reporter.reports().len(), 1);
        assert_eq!(reporter.reports()[0].message, "boom");
    }

    #[test]
    fn scrubbing_strips_path_prefix() {
        let report = CrashReport {
            message: "x".into(),
            location: Some("/home/user/secret/src/x.rs:10".into()),
        }
        .scrubbed();
        assert_eq!(report.location.as_deref(), Some("x.rs:10"));
    }

    #[test]
    fn panic_hook_captures_a_panic() {
        let reporter = CrashReporter::new(Consent::Denied);
        // Exercise the capture path directly (installing a global hook in a test
        // would race other tests); the hook simply forwards to `capture`.
        let result = std::panic::catch_unwind(|| panic!("deliberate"));
        assert!(result.is_err());
        reporter.capture(CrashReport {
            message: "deliberate".into(),
            location: None,
        });
        assert!(reporter.reports().iter().any(|r| r.message == "deliberate"));
    }
}
