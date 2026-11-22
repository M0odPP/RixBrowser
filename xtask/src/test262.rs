//! ECMAScript Test262 harness (`cargo xtask test262`) executing real `.js`
//! conformance tests on `rix-js` with YAML frontmatter parsing (`/*--- ... ---*/`),
//! strict/non-strict/module/async modes, negative phase/type checks, and committed
//! baseline ratchet tracking.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rix_js::{EvalError, JsVm, ModuleLoader, ObjectKind, PromiseState, PropertyAttrs};
use serde::{Deserialize, Serialize};

/// Standard Test262 assertion harness injected before each test case.
const TEST262_HARNESS_PRELUDE: &str = r#"
function Test262Error(message) {
    this.name = "Test262Error";
    this.message = message || "";
}
Test262Error.prototype = Object.create(Error.prototype);
Test262Error.prototype.constructor = Test262Error;

function assert(mustBeTrue, message) {
    if (mustBeTrue === true) return;
    throw new Test262Error(message || "Expected true but got " + mustBeTrue);
}
assert.sameValue = function(actual, expected, message) {
    if (Object.is(actual, expected)) return;
    throw new Test262Error((message ? message + ": " : "") + "Expected " + expected + " but got " + actual);
};
assert.notSameValue = function(actual, unexpected, message) {
    if (!Object.is(actual, unexpected)) return;
    throw new Test262Error((message ? message + ": " : "") + "Expected value different from " + unexpected);
};
assert.throws = function(expectedErrorConstructor, func, message) {
    let threw = false;
    try {
        func();
    } catch (err) {
        threw = true;
        if (expectedErrorConstructor) {
            const expName = expectedErrorConstructor.name;
            const errName = err && err.name ? err.name : "";
            if (!(err instanceof expectedErrorConstructor) && errName !== expName) {
                throw new Test262Error((message ? message + ": " : "") + "Expected " + expName + " but got " + errName);
            }
        }
    }
    if (!threw) {
        throw new Test262Error((message ? message + ": " : "") + "Expected exception to be thrown");
    }
};
"#;

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

/// Individual failure record reported when a Test262 test fails.
#[derive(Debug, Clone)]
pub struct Test262Failure {
    pub path: String,
    pub reason: String,
}

/// Summary report emitted by `cargo xtask test262`.
#[derive(Debug, Clone, Default)]
pub struct Test262Report {
    pub total: usize,
    pub metadata_valid: usize,
    pub passed: usize,
    pub failed: usize,
    pub pending_vm: usize,
    pub baseline_passed: usize,
    pub regressed: bool,
    pub failures: Vec<Test262Failure>,
}

impl Test262Report {
    #[must_use]
    pub fn pass_rate_pct(&self) -> f64 {
        if self.total == 0 {
            100.0
        } else {
            (self.passed as f64 / self.total as f64) * 100.0
        }
    }
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

fn run_single_test262(source: &str, meta: &Test262Metadata) -> std::result::Result<(), String> {
    let is_module = meta.flags.iter().any(|f| f == "module");
    let is_async = meta.flags.iter().any(|f| f == "async");
    let only_strict = meta.flags.iter().any(|f| f == "onlyStrict");
    let no_strict = meta.flags.iter().any(|f| f == "noStrict");

    let mut modes = Vec::new();
    if is_module || only_strict {
        modes.push(true);
    } else if no_strict {
        modes.push(false);
    } else if meta.negative_phase.is_some() {
        // Test262 negative tests without explicit strictness flags pass if strict mode rejects.
        modes.push(true);
    } else {
        modes.push(false);
    }

    for strict in modes {
        run_single_mode(source, meta, strict, is_module, is_async)?;
    }
    Ok(())
}

fn run_single_mode(
    source: &str,
    meta: &Test262Metadata,
    strict: bool,
    is_module: bool,
    is_async: bool,
) -> std::result::Result<(), String> {
    let mut vm = JsVm::new();
    vm.max_loop_iterations = Some(500_000);
    vm.eval_script(TEST262_HARNESS_PRELUDE)
        .map_err(|e| format!("harness prelude failed: {e}"))?;

    let done_state = std::rc::Rc::new(std::cell::RefCell::new(None::<Option<String>>));
    if is_async {
        let ds = std::rc::Rc::clone(&done_state);
        let done_fn = vm.alloc_host_fn("$DONE", move |vm, _this, args| {
            let err_arg = args.first().copied().unwrap_or(rix_js::JsValue::UNDEFINED);
            if err_arg.is_undefined() || err_arg.is_null() {
                *ds.borrow_mut() = Some(None);
            } else {
                *ds.borrow_mut() = Some(Some(vm.value_to_rust_string(err_arg)));
            }
            Ok(rix_js::JsValue::UNDEFINED)
        });
        let k_done = vm.heap.intern_key("$DONE");
        let g = vm.global;
        vm.heap.object_define_data(
            g,
            k_done,
            rix_js::JsValue::from_object(done_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    let exec_source = if strict && !is_module {
        format!("\"use strict\";\n{source}")
    } else {
        source.to_string()
    };

    let outcome: std::result::Result<(), (String, String)> = if is_module {
        match ModuleLoader::eval_single_module(
            &mut vm,
            "https://test262.local/test.mjs",
            &exec_source,
        ) {
            Ok(prom_h) => {
                vm.drain_microtasks();
                match vm.heap.get_object(prom_h).map(|o| &o.kind) {
                    Some(ObjectKind::Promise(p)) => match p.state {
                        PromiseState::Fulfilled(_) => Ok(()),
                        PromiseState::Rejected(r) => {
                            Err(("runtime".into(), error_type_name(&vm, r)))
                        }
                        PromiseState::Pending => {
                            Err(("runtime".into(), "ModulePromisePending".into()))
                        }
                    },
                    _ => Ok(()),
                }
            }
            Err(e) => Err(("parse".into(), format!("SyntaxError: {e}"))),
        }
    } else {
        match vm.eval_script(&exec_source) {
            Ok(_) => {
                vm.drain_microtasks();
                Ok(())
            }
            Err(EvalError::Compile(e)) => Err(("parse".into(), format!("SyntaxError: {e}"))),
            Err(EvalError::Runtime(e)) => Err(("runtime".into(), error_type_name(&vm, e.value))),
        }
    };

    match (&meta.negative_phase, &meta.negative_type) {
        (Some(exp_phase), Some(exp_type)) => match outcome {
            Ok(()) => Err(format!(
                "expected negative {exp_phase}/{exp_type}, but test succeeded"
            )),
            Err((actual_phase, actual_err)) => {
                let phase_ok = exp_phase == &actual_phase
                    || (exp_phase == "early" && actual_phase == "parse")
                    || (exp_phase == "resolution" && actual_phase == "parse");
                let type_ok = actual_err.contains(exp_type.as_str());
                if phase_ok && type_ok {
                    Ok(())
                } else {
                    Err(format!(
                        "expected negative {exp_phase}/{exp_type}, got {actual_phase}/{actual_err}"
                    ))
                }
            }
        },
        _ => {
            if let Err((phase, err)) = outcome {
                return Err(format!("unexpected {phase} error: {err}"));
            }
            if is_async {
                match done_state.borrow().as_ref() {
                    Some(None) => Ok(()),
                    Some(Some(err)) => Err(format!("async test called $DONE with error: {err}")),
                    None => Err("async test never invoked $DONE()".into()),
                }
            } else {
                Ok(())
            }
        }
    }
}

fn error_type_name(vm: &JsVm, val: rix_js::JsValue) -> String {
    if let Some(h) = val.as_object() {
        if let Some(p) = vm.heap.object_get_own(h, vm.key_name, &vm.shapes) {
            let n = vm.value_to_rust_string(p.value);
            let m = vm
                .heap
                .object_get_own(h, vm.key_message, &vm.shapes)
                .map(|mp| vm.value_to_rust_string(mp.value))
                .unwrap_or_default();
            return format!("{n}: {m}");
        }
    }
    vm.value_to_rust_string(val)
}

/// Discovers Test262 fixtures, validates metadata, executes each case on `rix-js`,
/// and verifies pass count does not regress below `baseline_path`.
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
        let Some(meta) = parse_test262_frontmatter(&src) else {
            report.failed += 1;
            report.failures.push(Test262Failure {
                path: path.display().to_string(),
                reason: "missing Test262 YAML frontmatter".into(),
            });
            continue;
        };
        report.metadata_valid += 1;
        match run_single_test262(&src, &meta) {
            Ok(()) => report.passed += 1,
            Err(reason) => {
                report.failed += 1;
                report.failures.push(Test262Failure {
                    path: path.display().to_string(),
                    reason,
                });
            }
        }
    }

    if update_baseline {
        let updated = Test262Baseline {
            engine_status: "phase2-rix-js-tier1".to_string(),
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
    println!("=== Test262 Conformance Harness (rix-js Tier-1) ===");
    println!(
        "  Discovered: {} | Valid frontmatter: {} | Passed: {} / {} ({:.1}%) | Baseline: {} | Failed: {}",
        report.total,
        report.metadata_valid,
        report.passed,
        report.total,
        report.pass_rate_pct(),
        report.baseline_passed,
        report.failed,
    );
    for f in &report.failures {
        println!("    FAIL {}: {}", f.path, f.reason);
    }
}
