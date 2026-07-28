//! The target capability manifest (`conformance/manifests.toml`).
//!
//! One file declares every conformance target as a `[target.<id>]` table.
//! A target's table lists which test *tags* are unsupported (whole groups
//! skipped with a reason) and which individual test *ids* are expected to fail
//! (with a mandatory tracking reference). The runner applies these
//! declarations as policy when classifying raw adapter results. An entry-less
//! `[target.<id>]` table is still load-bearing: it registers the target, so
//! the matrix gets a row (and a visible gap when no adapter reported).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// One target's manifest: its id plus its policy entries.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// Target id, e.g. `wasmtime`, `jco-node`, `jco-browser`, `wasip3-guest`.
    pub id: String,
    /// Tags this target cannot support; matching tests become `skip-unsupported`.
    pub unsupported: Vec<Unsupported>,
    /// Test ids expected to fail on this target; a pass becomes `unexpected-pass`.
    pub expected_fail: Vec<ExpectedFail>,
}

/// The parsed manifest file: a `[target.<id>]` table per target.
#[derive(Debug, Deserialize)]
struct ManifestFile {
    /// Targets by id, in id order (`BTreeMap`), matching the matrix row order.
    #[serde(default)]
    target: BTreeMap<String, TargetEntry>,
}

/// The body of one `[target.<id>]` table.
#[derive(Debug, Deserialize)]
struct TargetEntry {
    /// `[[target.<id>.unsupported]]` entries.
    #[serde(default)]
    unsupported: Vec<Unsupported>,
    /// `[[target.<id>.expected-fail]]` entries.
    #[serde(default, rename = "expected-fail")]
    expected_fail: Vec<ExpectedFail>,
}

/// An `unsupported` entry: an unsupported tag plus a mandatory reason.
#[derive(Debug, Clone, Deserialize)]
pub struct Unsupported {
    /// Tag (from `tests.toml`) whose tests this target does not support.
    pub tag: String,
    /// Why the tag is unsupported (required; surfaced in the matrix).
    pub reason: String,
    /// Environments this entry applies to. Absent ⇒ every environment.
    /// Must be non-empty when present.
    pub environments: Option<Vec<String>>,
}

/// An `expected-fail` entry: a test id plus mandatory tracking reference.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedFail {
    /// Test id that is known to fail on this target.
    pub test: String,
    /// Why it fails (required). Part of the manifest schema for human review.
    #[allow(dead_code)]
    pub reason: String,
    /// Tracking reference (e.g. a GitHub issue URL); required to stay honest.
    pub tracking: String,
    /// Environments this entry applies to. Absent ⇒ every environment.
    /// Must be non-empty when present.
    pub environments: Option<Vec<String>>,
}

/// True if an entry's optional `environments` scope covers `environment`.
/// An absent scope covers every environment (including planning-only rows).
fn scope_applies(environments: &Option<Vec<String>>, environment: &str) -> bool {
    match environments {
        None => true,
        Some(envs) => envs.iter().any(|e| e == environment),
    }
}

impl Manifest {
    /// Parse the manifest file into per-target manifests, in target-id order.
    /// A missing file means no targets are declared.
    pub fn load_all(path: &Path) -> Result<Vec<Manifest>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest file {}", path.display()))?;
        Self::parse_all(&text).with_context(|| format!("in manifest file {}", path.display()))
    }

    /// Parse manifest-file TOML into per-target manifests, in target-id order.
    pub fn parse_all(text: &str) -> Result<Vec<Manifest>> {
        let file: ManifestFile = toml::from_str(text).context("parsing manifest file")?;
        let manifests: Vec<Manifest> = file
            .target
            .into_iter()
            .map(|(id, entry)| Manifest {
                id,
                unsupported: entry.unsupported,
                expected_fail: entry.expected_fail,
            })
            .collect();
        for manifest in &manifests {
            manifest
                .validate()
                .with_context(|| format!("validating target {:?}", manifest.id))?;
        }
        Ok(manifests)
    }

    /// Structural checks beyond what serde enforces: an `environments` list,
    /// when present, must be non-empty (an empty list would match nothing and
    /// is always a mistake).
    pub fn validate(&self) -> Result<()> {
        for u in &self.unsupported {
            if matches!(&u.environments, Some(envs) if envs.is_empty()) {
                anyhow::bail!(
                    "`unsupported` entry for tag {:?} has an empty `environments` list; \
                     omit the key to apply to every environment",
                    u.tag
                );
            }
        }
        for e in &self.expected_fail {
            if matches!(&e.environments, Some(envs) if envs.is_empty()) {
                anyhow::bail!(
                    "`expected-fail` entry for test {:?} has an empty `environments` list; \
                     omit the key to apply to every environment",
                    e.test
                );
            }
        }
        Ok(())
    }

    /// The `unsupported` entry that applies to a test with these tags in this
    /// environment, if any. Environment-scoped entries take precedence over
    /// unscoped ones when both match.
    pub fn is_unsupported(&self, tags: &[String], environment: &str) -> Option<&Unsupported> {
        let matches = |u: &&Unsupported| tags.iter().any(|t| t == &u.tag);
        self.unsupported
            .iter()
            .filter(|u| scope_applies(&u.environments, environment))
            .filter(matches)
            .max_by_key(|u| u.environments.is_some())
    }

    /// The `expected-fail` entry that applies to a test id in this environment,
    /// if any. Environment-scoped entries take precedence over unscoped ones
    /// when both match.
    pub fn expected_fail(&self, test_id: &str, environment: &str) -> Option<&ExpectedFail> {
        self.expected_fail
            .iter()
            .filter(|e| scope_applies(&e.environments, environment))
            .filter(|e| e.test == test_id)
            .max_by_key(|e| e.environments.is_some())
    }
}
