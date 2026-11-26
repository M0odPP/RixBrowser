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
         cargo xtask wpt-official --suite <subpath> [--wpt-dir <path>] [--kind <all|testharness|reftest>] [--limit <n>] [--report-json <path>]\n  \
         cargo xtask html5lib [--dir <path>] [--filter <substr>] [--report-json <path>]\n  \
         cargo xtask test262 [--test262-dir <path>] [--official] [--filter <substr>] [--limit <n>] [--always-jit] [--report-json <path>] [--update-baseline]\n  \
         cargo xtask compare-chrome --cohort <path> [--out-dir <path>] [--rix-bin <path>] [--chrome-bin <path>] [--limit <n>] [--report-json <path>]\n  \
         cargo xtask compare-firefox --cohort <path> [--out-dir <path>] [--rix-bin <path>] [--firefox-bin <path>] [--width <px>] [--height <px>] [--dpr <f>] [--dark] [--interactive] [--warm] [--self-consistency] [--limit <n>] [--report-json <path>]\n  \
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
        "wpt-official" => {
            let mut wpt_dir = xtask::wpt_official::default_official_wpt_dir();
            let mut suite = "dom".to_string();
            let mut kind = xtask::wpt_official::OfficialWptKind::All;
            let mut limit = None;
            let mut report_json = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--wpt-dir" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --wpt-dir"))?;
                        wpt_dir = PathBuf::from(val);
                    }
                    "--suite" => {
                        i += 1;
                        suite = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --suite"))?
                            .clone();
                    }
                    "--kind" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --kind"))?;
                        kind = match val.as_str() {
                            "all" => xtask::wpt_official::OfficialWptKind::All,
                            "testharness" => xtask::wpt_official::OfficialWptKind::Testharness,
                            "reftest" => xtask::wpt_official::OfficialWptKind::Reftest,
                            other => bail!("unknown --kind value: {other}"),
                        };
                    }
                    "--limit" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --limit"))?;
                        limit = Some(val.parse::<usize>()?);
                    }
                    "--report-json" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --report-json"))?;
                        report_json = Some(PathBuf::from(val));
                    }
                    other => bail!("unknown wpt-official flag: {other}"),
                }
                i += 1;
            }
            let opts = xtask::wpt_official::OfficialWptOptions {
                wpt_dir,
                suite,
                kind,
                limit,
                report_json,
            };
            let report = xtask::wpt_official::run_official_wpt_suite(&opts)?;
            xtask::wpt_official::print_report(&report);
            Ok(())
        }
        "html5lib" => {
            let mut dir = xtask::html5lib::default_html5lib_dir();
            let mut filter: Option<String> = None;
            let mut report_json: Option<PathBuf> = None;
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
                    "--filter" => {
                        i += 1;
                        filter = Some(
                            args.get(i)
                                .ok_or_else(|| anyhow::anyhow!("missing value for --filter"))?
                                .clone(),
                        );
                    }
                    "--report-json" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --report-json"))?;
                        report_json = Some(PathBuf::from(val));
                    }
                    other => bail!("unknown html5lib flag: {other}"),
                }
                i += 1;
            }
            let report = xtask::html5lib::run_html5lib_suite(
                &dir,
                filter.as_deref(),
                report_json.as_deref(),
            )?;
            xtask::html5lib::print_report(&report);
            Ok(())
        }
        "test262" => {
            let mut test262_dir = root.join("fixtures/test262");
            let mut update_baseline = false;
            let mut official = false;
            let mut opts = xtask::test262::Test262Options {
                check_baseline: true,
                ..xtask::test262::Test262Options::default()
            };
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
                        opts.check_baseline = false;
                    }
                    "--official" => {
                        official = true;
                        test262_dir = xtask::test262::default_official_test262_dir();
                        opts.check_baseline = false;
                    }
                    "--filter" => {
                        i += 1;
                        opts.filter = Some(
                            args.get(i)
                                .ok_or_else(|| anyhow::anyhow!("missing value for --filter"))?
                                .clone(),
                        );
                        opts.check_baseline = false;
                    }
                    "--limit" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --limit"))?;
                        opts.limit = Some(val.parse::<usize>()?);
                        opts.check_baseline = false;
                    }
                    "--always-jit" => opts.always_jit = true,
                    "--report-json" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --report-json"))?;
                        opts.report_json = Some(PathBuf::from(val));
                    }
                    "--update-baseline" => update_baseline = true,
                    other => bail!("unknown test262 flag: {other}"),
                }
                i += 1;
            }
            let _ = official;
            let report = xtask::test262::run_test262_suite_with_options(
                &test262_dir,
                &baseline_path,
                update_baseline,
                &opts,
            )?;
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
        "compare-chrome" => {
            let mut cohort_path: Option<PathBuf> = None;
            let mut out_dir = PathBuf::from("/tmp/rix-compare-chrome");
            let mut rix_bin = root.join("target/release/rix-app");
            let mut chrome_bin = PathBuf::from("/usr/bin/google-chrome-stable");
            let mut limit = None;
            let mut report_json = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--cohort" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --cohort"))?;
                        cohort_path = Some(PathBuf::from(val));
                    }
                    "--out-dir" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --out-dir"))?;
                        out_dir = PathBuf::from(val);
                    }
                    "--rix-bin" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --rix-bin"))?;
                        rix_bin = PathBuf::from(val);
                    }
                    "--chrome-bin" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --chrome-bin"))?;
                        chrome_bin = PathBuf::from(val);
                    }
                    "--limit" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --limit"))?;
                        limit = Some(val.parse::<usize>()?);
                    }
                    "--report-json" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --report-json"))?;
                        report_json = Some(PathBuf::from(val));
                    }
                    other => bail!("unknown compare-chrome flag: {other}"),
                }
                i += 1;
            }
            let Some(cohort_path) = cohort_path else {
                bail!("missing required --cohort <path> argument");
            };
            let opts = xtask::compare_chrome::CompareChromeOptions {
                cohort_path,
                out_dir,
                rix_bin,
                chrome_bin,
                limit,
                report_json,
            };
            let report = xtask::compare_chrome::run_compare_chrome(&opts)?;
            xtask::compare_chrome::print_report(&report);
            Ok(())
        }
        "compare-firefox" => {
            let mut cohort_path: Option<PathBuf> = None;
            let mut out_dir = PathBuf::from("/tmp/rix-compare-firefox");
            let mut rix_bin = root.join("target/release/rix-app");
            let mut firefox_bin = xtask::compare_firefox::CompareFirefoxOptions::default_firefox();
            let mut width = 1280u32;
            let mut height = 800u32;
            let mut dpr = 1.0f64;
            let mut dark_mode = false;
            let mut interactive = false;
            let mut warm = false;
            let mut self_consistency = false;
            let mut limit = None;
            let mut report_json = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--cohort" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --cohort"))?;
                        cohort_path = Some(PathBuf::from(val));
                    }
                    "--out-dir" | "--out" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --out-dir"))?;
                        out_dir = PathBuf::from(val);
                    }
                    "--rix-bin" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --rix-bin"))?;
                        rix_bin = PathBuf::from(val);
                    }
                    "--firefox-bin" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --firefox-bin"))?;
                        firefox_bin = PathBuf::from(val);
                    }
                    "--width" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --width"))?;
                        width = val.parse::<u32>()?;
                    }
                    "--height" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --height"))?;
                        height = val.parse::<u32>()?;
                    }
                    "--dpr" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --dpr"))?;
                        dpr = val.parse::<f64>()?;
                    }
                    "--dark" => dark_mode = true,
                    "--interactive" => interactive = true,
                    "--warm" => warm = true,
                    "--self-consistency" => self_consistency = true,
                    "--limit" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --limit"))?;
                        limit = Some(val.parse::<usize>()?);
                    }
                    "--report-json" => {
                        i += 1;
                        let val = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for --report-json"))?;
                        report_json = Some(PathBuf::from(val));
                    }
                    other => bail!("unknown compare-firefox flag: {other}"),
                }
                i += 1;
            }
            let Some(cohort_path) = cohort_path else {
                bail!("missing required --cohort <path> argument");
            };
            let opts = xtask::compare_firefox::CompareFirefoxOptions {
                cohort_path,
                out_dir,
                rix_bin,
                firefox_bin,
                width,
                height,
                dpr,
                dark_mode,
                interactive,
                warm,
                self_consistency,
                limit,
                report_json,
            };
            let report = xtask::compare_firefox::run_compare_firefox(&opts)?;
            xtask::compare_firefox::print_report(&report);
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
