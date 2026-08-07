//! Results-stream plumbing: parse child JSONL streams, fold the two
//! sides of a pair run into one event per case, and emit one canonical
//! stream per target.

use anyhow::{Context as _, Result};
use component_test_results::{CaseResult, Document, Envelope, Event};

/// Parse one child stream against the case names it was selected to run
/// (missing cases fold to `not-reached`, so a crashed child still
/// yields one event per expected case).
pub fn parse_stream(stream: &str, selected: &[String]) -> Result<Document> {
    component_test_results::fold_jsonl(stream, selected).context("parsing child results stream")
}

/// Fold the two sides of a pair run: one event per case, worst status
/// wins, details and diagnostics role-labelled so a red cell names the
/// side that failed (the upstream role fold, with this suite's role
/// names).
pub fn fold_pair(offerer: Document, answerer: Document) -> Vec<CaseResult> {
    component_test_results::fold_roles(&[
        ("offerer", offerer.results),
        ("answerer", answerer.results),
    ])
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
    let mut envelope = Envelope::new(target, suite_name);
    envelope.suite.artifact_sha256 = artifact_sha256;
    envelope.run.id = Some(run_id.to_string());
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
