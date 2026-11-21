//! ECMAScript Test262 harness (`cargo xtask test262`) with YAML frontmatter
//! parsing (`/*--- ... ---*/`) and committed conformance baseline tracking
//! ready for Phase 2/3 JS VM (`rix-js`) integration.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Parsed Test262 YAML metadata header (`/*--- ... ---*/`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Test262Metadata {
    pub description: String,
    pub flags: Vec<String>,
    pub features: Vec<String>,
    pub negative_phase: Option<String>,
    pub negative_type: Option<String>,
}

/// Committed Test262 conformance baseline scorecard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Test262Baseline {
    pub engine_status: String,
    pub total_discovered: usize,
    pub passed: usize,
}

/// Summary report emitted by `cargo xtask test262`.
#[derive(Debug, Clone, Default)]
pub struct Test262Report {
    pub total: usize,
    pub metadata_valid: usize,
    pub passed: usize,
    pub pending_vm: usize,
    pub baseline_passed: usize,
    pub regressed: bool,
}

/// Parses Test262 `/*--- ... ---*/` metadata block from a `.js` test file.
#[must_use]
pub fn parse_test262_frontmatter(source: &str) -> Option<Test262Metadata> {
    let start = source.find("/*---")?;
    let rest = &source[start + 5..];
    let end = rest.find("---*/")?;
    let block = &rest[..end];

    let mut meta = Test262Metadata::default();
    let mut in_negative = false;
    for raw_line in block.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !raw_line.starts_with(' ') && !raw_line.starts_with('\t') {
            in_negative = false;
        }
        if let Some(val) = trimmed.strip_prefix("description:") {
            meta.description = val.trim().trim_matches('"').trim_matches('\'').to_string();
        } else if let Some(val) = trimmed.strip_prefix("flags:") {
            meta.flags = parse_inline_yaml_list(val);
        } else if let Some(val) = trimmed.strip_prefix("features:") {
            meta.features = parse_inline_yaml_list(val);
        } else if trimmed == "negative:" {
            in_negative = true;
        } else if in_negative {
            if let Some(val) = trimmed.strip_prefix("phase:") {
                meta.negative_phase = Some(val.trim().to_string());
            } else if let Some(val) = trimmed.strip_prefix("type:") {
                meta.negative_type = Some(val.trim().to_string());
            }
        }
    }
    Some(meta)
}

fn parse_inline_yaml_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn collect_js_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_js_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("js") {
            out.push(path);
        }
    }
    Ok(())
}

/// Discovers Test262 fixtures, validates metadata, runs available JS execution
/// checks, and verifies pass count does not regress below `baseline_path`.
pub fn run_test262_suite(
    test262_dir: &Path,
    baseline_path: &Path,
    update_baseline: bool,
) -> Result<Test262Report> {
    let mut files = Vec::new();
    collect_js_files(test262_dir, &mut files)?;
    files.sort();

    let baseline: Test262Baseline = if baseline_path.exists() {
        let raw = fs::read_to_string(baseline_path)?;
        serde_json::from_str(&raw).with_context(|| format!("parse {}", baseline_path.display()))?
    } else {
        Test262Baseline::default()
    };

    let mut report = Test262Report {
        total: files.len(),
        baseline_passed: baseline.passed,
        ..Test262Report::default()
    };

    for path in &files {
        let src = fs::read_to_string(path)?;
        if parse_test262_frontmatter(&src).is_some() {
            report.metadata_valid += 1;
            // Phase 2/3 (`rix-js`) will plug execution directly here and ratchet `passed`.
            report.pending_vm += 1;
        }
    }

    if update_baseline {
        let updated = Test262Baseline {
            engine_status: "phase1-harness-ready".to_string(),
            total_discovered: report.total,
            passed: report.passed,
        };
        if let Some(parent) = baseline_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            baseline_path,
            serde_json::to_string_pretty(&updated)? + "\n",
        )?;
    } else if report.passed < baseline.passed {
        report.regressed = true;
    }

    Ok(report)
}

pub fn print_report(report: &Test262Report) {
    println!("=== Test262 Conformance Harness ===");
    println!(
        "  Discovered: {} | Valid frontmatter: {} | Passed: {} (baseline: {}) | Pending JS VM: {}",
        report.total,
        report.metadata_valid,
        report.passed,
        report.baseline_passed,
        report.pending_vm
    );
}
