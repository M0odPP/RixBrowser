#![deny(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Result};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits inside workspace root")
        .to_path_buf()
}

fn print_usage() {
    eprintln!(
        "Usage:\n  \
         cargo xtask reftest [--dir <path>] [--update-refs]\n  \
         cargo xtask wpt --subset <all|css2|flexbox|grid|selectors|html> [--wpt-dir <path>] [--update-baseline]\n  \
         cargo xtask test262 [--test262-dir <path>] [--update-baseline]\n  \
         cargo xtask bench [--quick]"
    );
}

fn dispatch(args: &[String]) -> Result<()> {
    let root = workspace_root();
    let Some(cmd) = args.first().map(String::as_str) else {
        print_usage();
        bail!("missing xtask subcommand");
    };

    match cmd {
        "reftest" => {
            let mut dir = root.join("fixtures/reftests");
            let mut update_refs = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--dir" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --dir"))?;
                        dir = PathBuf::from(val);
                    }
                    "--update-refs" => update_refs = true,
                    other => bail!("unknown reftest flag: {other}"),
                }
                i += 1;
            }
            let report = xtask::reftest::run_reftest_suite(&dir, update_refs)?;
            xtask::reftest::print_report(&report);
            if report.failed > 0 {
                bail!("{} visual reftest(s) failed", report.failed);
            }
            Ok(())
        }
        "wpt" => {
            let mut subset = "all".to_string();
            let mut wpt_dir = root.join("fixtures/wpt");
            let mut update_baseline = false;
            let baseline_path = root.join("fixtures/scorecards/wpt-baseline.json");
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--subset" => {
                        i += 1;
                        subset = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --subset"))?
                            .clone();
                    }
                    "--wpt-dir" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --wpt-dir"))?;
                        wpt_dir = PathBuf::from(val);
                    }
                    "--update-baseline" => update_baseline = true,
                    other => bail!("unknown wpt flag: {other}"),
                }
                i += 1;
            }
            let report =
                xtask::wpt::run_wpt_suite(&wpt_dir, &subset, &baseline_path, update_baseline)?;
            xtask::wpt::print_report(&report);
            if !report.regressions.is_empty() {
                bail!(
                    "WPT regression(s) detected: {}",
                    report.regressions.join("; ")
                );
            }
            Ok(())
        }
        "test262" => {
            let mut test262_dir = root.join("fixtures/test262");
            let mut update_baseline = false;
            let baseline_path = root.join("fixtures/scorecards/test262-baseline.json");
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--test262-dir" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --test262-dir"))?;
                        test262_dir = PathBuf::from(val);
                    }
                    "--update-baseline" => update_baseline = true,
                    other => bail!("unknown test262 flag: {other}"),
                }
                i += 1;
            }
            let report =
                xtask::test262::run_test262_suite(&test262_dir, &baseline_path, update_baseline)?;
            xtask::test262::print_report(&report);
            if report.regressed {
                bail!(
                    "Test262 regression: passed {} < baseline {}",
                    report.passed,
                    report.baseline_passed
                );
            }
            Ok(())
        }
        "bench" => {
            let quick = args.iter().any(|a| a == "--quick");
            xtask::bench::run_benchmarks(&root, quick)
        }
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("unknown xtask subcommand: {other}");
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
