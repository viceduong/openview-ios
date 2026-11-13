//! Direct-path event reporting (promotion to / demotion from the direct UDP
//! path) on the log and the status channel.

use ovapp::terminal::TerminalSession;
use ovcommon::status::{self, DirectState, StatusEvent};
use tracing::{debug, info, warn};

/// Log direct-path events (promotion, demotion, probes) and report path
/// changes on the status channel.
pub(super) fn log_direct(term: &mut TerminalSession) {
    for e in term.take_direct_events() {
        report_direct_event(&e);
    }
}

/// Classify one direct-path event string from `ovtransport`: `... active
/// ...` = promoted, `... back on relay` = demoted; anything else (probe
/// results, rebinds) is debug detail.
fn report_direct_event(e: &str) {
    let state = if e.contains("back on relay") {
        Some(DirectState::Fallback)
    } else if e.contains(" active") {
        Some(DirectState::Active)
    } else {
        None
    };
    match state {
        Some(state) => {
            info!("direct: {e}");
            status::emit(&StatusEvent::DirectPath {
                state,
                port: None,
                addr: None,
                detail: Some(e.to_owned()),
            });
        }
        None if e.starts_with("relay gone") => warn!("direct: {e}"),
        None => debug!("direct: {e}"),
    }
}
