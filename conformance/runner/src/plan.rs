//! Planning and classification: combine the registry, the manifests, and the
//! adapter reports into a per-(target, environment, test) final status, and
//! render the markdown conformance matrix.
//!
//! A target can run in more than one *environment* — the loopback path the
//! default adapters use, plus the netns-lab scenarios (`lan`, `stun-srflx`,
//! `turn-relay`; see the netns lab in `conformance/README.md`) that route each peer through
//! its own network namespace. Each `(target, environment)` pair reported by an
//! adapter is its own matrix row, so a scenario's outcome is classified against
//! the same manifest policy as its target's loopback run without being merged
//! into it. Manifest entries may carry an optional `environments` list scoping
//! them to specific environments; unscoped entries apply everywhere.

use std::collections::BTreeMap;

use crate::manifest::Manifest;
use crate::registry::Registry;
use crate::results::{AdapterReport, RawStatus, Status};

/// One classified cell of the matrix.
#[derive(Debug, Clone)]
pub struct Cell {
    pub status: Status,
    /// Reason/detail behind the status; surfaced in the end-of-run failure
    /// summary (the compact matrix shows only the status symbol).
    pub detail: Option<String>,
}

/// One matrix row: a target running in a particular environment. An empty
/// `environment` marks a planning-only row (a manifest with no adapter report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub target: String,
    pub environment: String,
}

impl Row {
    /// The cell-map key for a test in this row.
    fn key(&self, test: &str) -> (String, String, String) {
        (
            self.target.clone(),
            self.environment.clone(),
            test.to_string(),
        )
    }
}

/// The classified matrix: rows are (target, environment) pairs, columns tests.
pub struct Matrix {
    /// Rows in the order their reports/manifests were supplied.
    pub rows: Vec<Row>,
    /// Test ids in registry order.
    pub tests: Vec<String>,
    /// (target, environment, test) -> classified cell.
    pub cells: BTreeMap<(String, String, String), Cell>,
}

impl Matrix {
    /// Classify every (target, environment, test) triple from the registry, the
    /// per-target manifests, and the collected adapter reports.
    ///
    /// Each environment an adapter reports for a target becomes its own row,
    /// classified against that target's manifest. A target with no adapter
    /// report is still planned from its manifest as a single planning-only row
    /// (empty environment): its tests appear as `skip-unsupported` (where a tag
    /// is declared unsupported) or `missing` (otherwise).
    pub fn classify(
        registry: &Registry,
        manifests: &[Manifest],
        reports: &[AdapterReport],
    ) -> Self {
        let tests: Vec<String> = registry.test.iter().map(|t| t.id.clone()).collect();
        let mut rows = Vec::new();
        let mut cells = BTreeMap::new();

        for manifest in manifests {
            let target = &manifest.id;
            // Group this target's raw results by the environment they ran in.
            let mut by_env: BTreeMap<String, BTreeMap<&str, (RawStatus, Option<String>)>> =
                BTreeMap::new();
            for report in reports.iter().filter(|r| &r.target == target) {
                let env = by_env.entry(report.environment.clone()).or_default();
                for result in &report.results {
                    env.insert(
                        result.test_id.as_str(),
                        (result.status, result.detail.clone()),
                    );
                }
            }

            if by_env.is_empty() {
                // Planning-only: no adapter ran this target.
                by_env.insert(String::new(), BTreeMap::new());
            }

            for (environment, raw) in by_env {
                let row = Row {
                    target: target.clone(),
                    environment,
                };
                for entry in &registry.test {
                    let cell = classify_one(
                        manifest,
                        &row.environment,
                        &entry.id,
                        &entry.tags,
                        raw.get(entry.id.as_str()),
                    );
                    cells.insert(row.key(&entry.id), cell);
                }
                rows.push(row);
            }
        }

        Matrix { rows, tests, cells }
    }

    /// True if any classified cell is a failure (fail or unexpected-pass).
    pub fn has_failures(&self) -> bool {
        self.cells.values().any(|c| c.status.is_failure())
    }

    /// Every failing cell as `(row, test, cell)`, in row order then registry
    /// test order.
    pub fn failures(&self) -> Vec<(&Row, &str, &Cell)> {
        let mut failures = Vec::new();
        for row in &self.rows {
            for test in &self.tests {
                if let Some(cell) = self.cells.get(&row.key(test)) {
                    if cell.status.is_failure() {
                        failures.push((row, test.as_str(), cell));
                    }
                }
            }
        }
        failures
    }

    /// Render the matrix as a markdown table (rows × test columns), with a
    /// `target` and an `environment` column identifying each row.
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Conformance matrix\n\n");

        if self.rows.is_empty() {
            out.push_str("_No targets enabled._\n");
            return out;
        }
        if self.tests.is_empty() {
            out.push_str("_No tests registered._\n");
            return out;
        }

        out.push_str("| target | environment |");
        for test in &self.tests {
            out.push_str(&format!(" {test} |"));
        }
        out.push('\n');
        out.push_str("| --- | --- |");
        for _ in &self.tests {
            out.push_str(" --- |");
        }
        out.push('\n');

        for row in &self.rows {
            let environment = if row.environment.is_empty() {
                "—"
            } else {
                &row.environment
            };
            out.push_str(&format!("| {} | {environment} |", row.target));
            for test in &self.tests {
                let symbol = self
                    .cells
                    .get(&row.key(test))
                    .map(|c| c.status.symbol())
                    .unwrap_or("—");
                out.push_str(&format!(" {symbol} |"));
            }
            out.push('\n');
        }

        out.push_str("\nLegend: pass · FAIL · skip (unsupported) · xfail (expected-fail) · UNEXPECTED-PASS · — (missing/not run)\n");
        out
    }
}

fn classify_one(
    manifest: &Manifest,
    environment: &str,
    test_id: &str,
    tags: &[String],
    raw: Option<&(RawStatus, Option<String>)>,
) -> Cell {
    // Unsupported tags win regardless of whether a result was reported.
    if let Some(unsupported) = manifest.is_unsupported(tags, environment) {
        return Cell {
            status: Status::SkipUnsupported,
            detail: Some(unsupported.reason.clone()),
        };
    }

    let expected_fail = manifest.expected_fail(test_id, environment);

    match raw {
        None => Cell {
            status: Status::Missing,
            detail: None,
        },
        Some((RawStatus::Skip, detail)) => Cell {
            status: Status::SkipUnsupported,
            detail: detail.clone(),
        },
        Some((RawStatus::Pass, detail)) => {
            if let Some(xf) = expected_fail {
                Cell {
                    status: Status::UnexpectedPass,
                    detail: Some(format!(
                        "expected-fail passed (tracking {}); update the manifest",
                        xf.tracking
                    )),
                }
            } else {
                Cell {
                    status: Status::Pass,
                    detail: detail.clone(),
                }
            }
        }
        Some((RawStatus::Fail, detail)) => {
            if expected_fail.is_some() {
                Cell {
                    status: Status::ExpectedFail,
                    detail: detail.clone(),
                }
            } else {
                Cell {
                    status: Status::Fail,
                    detail: detail.clone(),
                }
            }
        }
    }
}
