//! Criterion benchmark orchestrator (`cargo xtask bench [--quick]`).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

const BENCH_TARGETS: &[(&str, &str)] =
    &[("rix-css", "cascade_bench"), ("rix-layout", "layout_bench")];

/// Runs all workspace Criterion benchmarks cleanly from `workspace_root`.
pub fn run_benchmarks(workspace_root: &Path, quick: bool) -> Result<()> {
    println!("=== RixBrowser Criterion Benchmarks (quick={quick}) ===");
    for &(pkg, bench_name) in BENCH_TARGETS {
        println!("--> Running {pkg}::{bench_name}...");
        let mut cmd = Command::new("cargo");
        cmd.current_dir(workspace_root)
            .args(["bench", "-p", pkg, "--bench", bench_name]);
        if quick {
            cmd.args(["--", "--warm-up-time", "1", "--measurement-time", "2"]);
        }
        let status = cmd.status()?;
        if !status.success() {
            bail!("benchmark {pkg}::{bench_name} exited with status {status}");
        }
    }
    Ok(())
}
