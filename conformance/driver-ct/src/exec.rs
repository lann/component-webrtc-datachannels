//! Child mode: run one suite instance's stream — a role, a selection, a
//! linker profile — and print the results JSONL on stdout. The
//! orchestrators spawn this mode once per instance (one solo, or one per
//! pair role) and fold/merge the streams.

use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use component_test_runner::{OutputMode, Runner};

use crate::host::{configure_linker, make_data, IceProfile, SuiteEnv};

pub struct ExecArgs {
    pub suite: PathBuf,
    pub target: String,
    pub select: Option<String>,
    pub role: Option<String>,
    pub signaling_url: Option<String>,
    pub run_id: Option<String>,
    pub composed: bool,
    pub ice: Option<IceProfile>,
    pub suite_artifact: Option<PathBuf>,
    pub jobs: usize,
    pub budget_secs: u64,
    pub case_timeout_secs: u64,
    pub jsonl: bool,
}

pub fn run(args: ExecArgs) -> Result<std::process::ExitCode> {
    if args.role.is_some() && (args.signaling_url.is_none() || args.run_id.is_none()) {
        bail!("--role needs --signaling and --run-id");
    }
    let suite_name = args
        .suite
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "suite".into());
    let env = SuiteEnv {
        role: args.role,
        signaling_url: args.signaling_url,
        run_id: args.run_id,
        composed: args.composed,
        ice: args.ice,
    };
    let composed = args.composed;
    let mut runner = Runner::with_data(
        &args.suite,
        move || make_data(&env),
        |linker| configure_linker(linker, composed),
    )?;
    if let Some(path) = &args.suite_artifact {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading --suite-artifact {}", path.display()))?;
        runner.bind_suite_artifact(&bytes);
    }
    let summary = wasmtime_wasi::runtime::in_tokio(runner.run_suite_opts(
        &suite_name,
        &args.target,
        if args.jsonl {
            OutputMode::Jsonl
        } else {
            OutputMode::Human
        },
        &[],
        1, // fresh instance per case, the incumbent isolation
        args.jobs,
        args.select.as_deref(),
        args.budget_secs,
        args.case_timeout_secs,
    ))?;
    Ok(if summary.failed > 0 {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    })
}
