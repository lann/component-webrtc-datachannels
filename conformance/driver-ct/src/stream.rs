//! Results-stream plumbing: parse child JSONL streams, fold the two
//! sides of a pair run into one event per case, and emit one canonical
//! stream per target.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use component_test_results::{CaseResult, Document, Envelope, Event, RunInfo, Status, SuiteInfo};

/// Parse one child stream against the case names it was selected to run
/// (missing cases fold to `not-reached`, so a crashed child still
/// yields one event per expected case).
pub fn parse_stream(stream: &str, selected: &[String]) -> Result<Document> {
    component_test_results::fold_jsonl(stream, selected).context("parsing child results stream")
}

/// Fold the two sides of a pair run: one event per case, worst status
/// wins (fail > not-reached > skipped > pass), details and diagnostics
/// role-labelled so a red cell names the side that failed.
pub fn fold_pair(offerer: Document, answerer: Document) -> Vec<CaseResult> {
    let mut answerer_by_case: BTreeMap<String, CaseResult> = answerer
        .results
        .into_iter()
        .map(|r| (r.case.clone(), r))
        .collect();
    let mut out = Vec::new();
    for off in offerer.results {
        let case = off.case.clone();
        match answerer_by_case.remove(&case) {
            Some(ans) => out.push(fold_case(off, ans)),
            None => out.push(off),
        }
    }
    // Cases only the answerer reported (offerer stream truncated before
    // fold_jsonl synthesis could cover them).
    out.extend(answerer_by_case.into_values());
    out
}

fn rank(status: Status) -> u8 {
    match status {
        Status::Fail => 3,
        Status::NotReached => 2,
        Status::Skipped => 1,
        _ => 0,
    }
}

fn fold_case(off: CaseResult, ans: CaseResult) -> CaseResult {
    let (mut winner, loser, winner_role, loser_role) = if rank(ans.status) > rank(off.status) {
        (ans, off, "answerer", "offerer")
    } else {
        (off, ans, "offerer", "answerer")
    };
    if rank(winner.status) > 0 {
        let mut detail = format!(
            "{winner_role}: {}",
            winner.detail.as_deref().unwrap_or(winner.status.word())
        );
        if rank(loser.status) > 0 {
            detail.push_str(&format!(
                "; {loser_role}: {}",
                loser.detail.as_deref().unwrap_or(loser.status.word())
            ));
        }
        winner.detail = Some(detail);
    }
    let mut diagnostics = Vec::new();
    diagnostics.extend(
        winner
            .diagnostics
            .iter()
            .map(|d| format!("{winner_role}: {d}")),
    );
    diagnostics.extend(
        loser
            .diagnostics
            .iter()
            .map(|d| format!("{loser_role}: {d}")),
    );
    winner.diagnostics = diagnostics;
    winner.diagnostics_complete = winner.diagnostics_complete && loser.diagnostics_complete;
    winner.duration_ms = match (winner.duration_ms, loser.duration_ms) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    winner
}

/// Serialize one target's merged stream: envelope, run errors, case
/// events, terminator.
pub fn emit(
    target: &str,
    suite_name: &str,
    artifact_sha256: Option<String>,
    run_id: &str,
    run_errors: &[String],
    results: &[CaseResult],
) -> Result<String> {
    let envelope = Envelope {
        version: component_test_results::RESULTS_VERSION.to_string(),
        target: target.to_string(),
        suite: SuiteInfo {
            name: suite_name.to_string(),
            artifact_sha256,
            lockfile_sha256: None,
        },
        run: RunInfo {
            id: Some(run_id.to_string()),
            started: None,
            segment: 0,
            scheduling: Some("tags".to_string()),
        },
    };
    let mut events: Vec<Event> = run_errors
        .iter()
        .map(|message| {
            Event::RunError(component_test_results::RunError {
                message: message.clone(),
            })
        })
        .collect();
    events.extend(results.iter().cloned().map(Event::Case));
    component_test_results::to_jsonl(&envelope, &events).context("serializing merged stream")
}
