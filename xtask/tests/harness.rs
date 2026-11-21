use std::path::Path;

use xtask::reftest::{compare_rgba, run_reftest_suite};
use xtask::test262::{parse_test262_frontmatter, run_test262_suite};
use xtask::wpt::run_wpt_suite;

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn pixel_diff_detects_exact_match_and_mismatches() {
    let a = vec![10, 20, 30, 255, 100, 110, 120, 255];
    let b = vec![12, 20, 30, 255, 100, 110, 120, 255];
    let exact = compare_rgba(&a, &b, 2);
    assert_eq!(exact.diff_pixels, 0);
    assert_eq!(exact.max_channel_delta, 2);

    let strict = compare_rgba(&a, &b, 1);
    assert_eq!(strict.diff_pixels, 1);
}

#[test]
fn reftest_suite_passes_all_committed_fixtures() {
    let dir = repo_root().join("fixtures/reftests");
    let report = run_reftest_suite(&dir, false).expect("reftest suite runs");
    assert!(report.total >= 4);
    assert_eq!(
        report.failed, 0,
        "unexpected reftest failures: {:?}",
        report.cases
    );
}

#[test]
fn wpt_suite_passes_baseline_and_catches_regression() {
    let root = repo_root();
    let wpt_dir = root.join("fixtures/wpt");
    let baseline = root.join("fixtures/scorecards/wpt-baseline.json");

    let report = run_wpt_suite(&wpt_dir, "all", &baseline, false).expect("wpt runs");
    assert!(report.regressions.is_empty(), "{:?}", report.regressions);
    assert!(report.subsets.len() >= 4);

    // Verify regression detection triggers when baseline exceeds actual passes.
    let tmp = std::env::temp_dir().join("rix-wpt-regression-check.json");
    std::fs::write(&tmp, r#"{"subsets":{"css2":999}}"#).unwrap();
    let reg = run_wpt_suite(&wpt_dir, "css2", &tmp, false).expect("runs");
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(reg.regressions.len(), 1);
}

#[test]
fn test262_frontmatter_parser_and_suite_baseline_check() {
    let sample = "/*---\ndescription: Sample test\nflags: [onlyStrict, async]\nnegative:\n  phase: parse\n  type: SyntaxError\n---*/\nlet x = 1;\n";
    let meta = parse_test262_frontmatter(sample).expect("parses frontmatter");
    assert_eq!(meta.description, "Sample test");
    assert_eq!(meta.flags, vec!["onlyStrict", "async"]);
    assert_eq!(meta.negative_phase.as_deref(), Some("parse"));
    assert_eq!(meta.negative_type.as_deref(), Some("SyntaxError"));

    let root = repo_root();
    let report = run_test262_suite(
        &root.join("fixtures/test262"),
        &root.join("fixtures/scorecards/test262-baseline.json"),
        false,
    )
    .expect("test262 runs");
    assert_eq!(report.total, 2);
    assert_eq!(report.metadata_valid, 2);
    assert!(!report.regressed);
}
