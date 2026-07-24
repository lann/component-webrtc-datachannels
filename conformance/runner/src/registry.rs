//! The test registry (`conformance/tests.toml`).
//!
//! The registry is the single source of truth for which conformance tests
//! exist, what tags they carry, and a human description. The conformance guest
//! component mirrors these ids/tags via its `list-tests` export; the runner
//! uses the registry to plan each target's test list from that target's
//! manifest.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The parsed `tests.toml` document.
#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    /// Every registered conformance test, keyed by id in declaration order.
    #[serde(default)]
    pub test: Vec<TestEntry>,
}

/// One registered conformance test.
#[derive(Debug, Clone, Deserialize)]
pub struct TestEntry {
    /// Stable test id (matches the guest's `list-tests` and adapter results).
    pub id: String,
    /// Tags used by manifests to declare whole groups unsupported.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Human-readable description of what the test asserts. Part of the
    /// registry schema; surfaced in reporting by a later phase.
    #[allow(dead_code)]
    pub description: String,
}

impl Registry {
    /// Parse a `tests.toml` file, validating that ids are unique.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading test registry {}", path.display()))?;
        let registry: Registry = toml::from_str(&text)
            .with_context(|| format!("parsing test registry {}", path.display()))?;
        registry.validate()?;
        Ok(registry)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for entry in &self.test {
            anyhow::ensure!(
                !entry.id.is_empty(),
                "test registry contains an entry with an empty id"
            );
            anyhow::ensure!(
                seen.insert(entry.id.as_str()),
                "test registry contains duplicate test id `{}`",
                entry.id
            );
        }
        Ok(())
    }

    /// Look up a test by id (`None` for ids the registry does not contain).
    pub fn get(&self, id: &str) -> Option<&TestEntry> {
        self.test.iter().find(|t| t.id == id)
    }
}
