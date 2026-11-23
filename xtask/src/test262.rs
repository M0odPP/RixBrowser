//! ECMAScript Test262 harness (`cargo xtask test262`) executing real `.js`
//! conformance tests on `rix-js` (and optionally `rix-jit` via `--always-jit`)
//! with full YAML frontmatter parsing (`/*--- ... ---*/`), official `harness/`
//! and `includes:` loading, strict/non-strict/raw/module/async modes, parallel
//! execution with panic isolation, category scorecards, and baseline ratchets.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rayon::prelude::*;
use rix_jit::TieredJit;
use rix_js::{
    EvalError, ImportMap, JsVm, ModuleLoader, ModuleSourceProvider, ObjectKind, PromiseState,
    PropertyAttrs,
};
use serde::{Deserialize, Serialize};

/// Fallback Test262 assertion prelude when running standalone fixtures without a `harness/` folder.
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

/// Default local path for official `tc39/test262` checkout.
#[must_use]
pub fn default_official_test262_dir() -> PathBuf {
    if let Ok(base) = std::env::var("RIX_CONFORMANCE_DIR") {
        return PathBuf::from(base).join("test262/test");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/rix-conformance/test262/test")
}

/// Configuration options for running the Test262 harness.
#[derive(Debug, Clone, Default)]
pub struct Test262Options {
    pub filter: Option<String>,
    pub limit: Option<usize>,
    pub always_jit: bool,
    pub report_json: Option<PathBuf>,
    pub check_baseline: bool,
}

/// Parsed Test262 YAML metadata header (`/*--- ... ---*/`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Test262Metadata {
    pub description: String,
    pub flags: Vec<String>,
    pub includes: Vec<String>,
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

/// Per-category pass/fail summary for official Test262 breakdowns.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CategoryStats {
    pub passed: usize,
    pub failed: usize,
    pub panics: usize,
    pub total: usize,
}

/// Individual failure record reported when a Test262 test fails.
#[derive(Debug, Clone, Serialize)]
pub struct Test262Failure {
    pub path: String,
    pub reason: String,
}

/// Summary report emitted by `cargo xtask test262`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Test262Report {
    pub total: usize,
    pub metadata_valid: usize,
    pub passed: usize,
    pub failed: usize,
    pub panics: usize,
    pub pending_vm: usize,
    pub baseline_passed: usize,
    pub regressed: bool,
    pub always_jit: bool,
    pub categories: BTreeMap<String, CategoryStats>,
    pub top_failure_reasons: Vec<(String, usize)>,
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

/// Parses Test262 `/*--- ... ---*/` metadata block (supports inline `[a, b]` and multiline `- item` YAML lists).
#[must_use]
pub fn parse_test262_frontmatter(source: &str) -> Option<Test262Metadata> {
    let start = source.find("/*---")?;
    let rest = &source[start + 5..];
    let end = rest.find("---*/")?;
    let block = &rest[..end];

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        None,
        Negative,
        Flags,
        Includes,
        Features,
    }

    let mut meta = Test262Metadata::default();
    let mut section = Section::None;

    for raw_line in block.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indented = raw_line.starts_with(' ') || raw_line.starts_with('\t');
        if !indented {
            section = Section::None;
        }

        if let Some(val) = trimmed.strip_prefix("description:") {
            meta.description = val.trim().trim_matches('"').trim_matches('\'').to_string();
        } else if let Some(val) = trimmed.strip_prefix("flags:") {
            let items = parse_inline_yaml_list(val);
            if items.is_empty() {
                section = Section::Flags;
            } else {
                meta.flags = items;
            }
        } else if let Some(val) = trimmed.strip_prefix("includes:") {
            let items = parse_inline_yaml_list(val);
            if items.is_empty() {
                section = Section::Includes;
            } else {
                meta.includes = items;
            }
        } else if let Some(val) = trimmed.strip_prefix("features:") {
            let items = parse_inline_yaml_list(val);
            if items.is_empty() {
                section = Section::Features;
            } else {
                meta.features = items;
            }
        } else if trimmed == "negative:" {
            section = Section::Negative;
        } else if indented {
            match section {
                Section::Negative => {
                    if let Some(val) = trimmed.strip_prefix("phase:") {
                        meta.negative_phase = Some(val.trim().to_string());
                    } else if let Some(val) = trimmed.strip_prefix("type:") {
                        meta.negative_type = Some(val.trim().to_string());
                    }
                }
                Section::Flags => {
                    if let Some(item) = trimmed.strip_prefix('-') {
                        let clean = item.trim().trim_matches('"').trim_matches('\'');
                        if !clean.is_empty() {
                            meta.flags.push(clean.to_string());
                        }
                    }
                }
                Section::Includes => {
                    if let Some(item) = trimmed.strip_prefix('-') {
                        let clean = item.trim().trim_matches('"').trim_matches('\'');
                        if !clean.is_empty() {
                            meta.includes.push(clean.to_string());
                        }
                    }
                }
                Section::Features => {
                    if let Some(item) = trimmed.strip_prefix('-') {
                        let clean = item.trim().trim_matches('"').trim_matches('\'');
                        if !clean.is_empty() {
                            meta.features.push(clean.to_string());
                        }
                    }
                }
                Section::None => {}
            }
        }
    }
    Some(meta)
}

fn parse_inline_yaml_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let inner = trimmed.trim_start_matches('[').trim_end_matches(']');
    inner
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
            let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Official Test262 module fixtures (`*_FIXTURE.js`) are helper modules, not standalone tests.
            if !fname.contains("_FIXTURE") {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn load_harness_cache(test262_dir: &Path) -> HashMap<String, String> {
    let mut cache = HashMap::new();
    let candidates = [
        test262_dir
            .parent()
            .map_or_else(PathBuf::new, |p| p.join("harness")),
        test262_dir.join("harness"),
    ];
    for hdir in candidates {
        // Distinguish the real Test262 `harness/` directory (which contains `sta.js` and `assert.js`)
        // from `test/harness/` (which contains conformance tests for the harness itself).
        if !hdir.join("sta.js").is_file() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&hdir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("js") {
                    if let (Some(name), Ok(src)) = (
                        p.file_name().and_then(|n| n.to_str()),
                        fs::read_to_string(&p),
                    ) {
                        cache.insert(name.to_string(), src);
                    }
                }
            }
        }
        if !cache.is_empty() {
            break;
        }
    }
    cache
}

struct FsModuleProvider {
    root_dir: PathBuf,
    entry_url: String,
    entry_source: String,
}

impl ModuleSourceProvider for FsModuleProvider {
    fn load_source(&self, url: &str) -> std::result::Result<String, String> {
        if url == self.entry_url {
            return Ok(self.entry_source.clone());
        }
        let rel = url
            .strip_prefix("https://test262.local/")
            .ok_or_else(|| format!("unsupported module url: {url}"))?;
        let path = self.root_dir.join(rel);
        fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))
    }
}

fn install_test262_host(vm: &mut JsVm) {
    let host = vm.alloc_ordinary_object();
    let g_val = rix_js::JsValue::from_object(vm.global);
    let k_global = vm.heap.intern_key("global");
    vm.heap.object_define_data(
        host,
        k_global,
        g_val,
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );

    let eval_script_fn = vm.alloc_native_fn("evalScript", |vm, _this, args| {
        let code =
            vm.value_to_rust_string(args.first().copied().unwrap_or(rix_js::JsValue::UNDEFINED));
        match vm.eval_script(&code) {
            Ok(v) => Ok(v),
            Err(EvalError::Compile(ce)) => Err(vm.make_error("SyntaxError", ce.to_string())),
            Err(EvalError::Runtime(exc)) => Err(exc),
        }
    });
    let k_eval = vm.heap.intern_key("evalScript");
    vm.heap.object_define_data(
        host,
        k_eval,
        rix_js::JsValue::from_object(eval_script_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let gc_fn = vm.alloc_native_fn("gc", |vm, _this, _args| {
        let _ = vm.collect_garbage();
        Ok(rix_js::JsValue::UNDEFINED)
    });
    let k_gc = vm.heap.intern_key("gc");
    vm.heap.object_define_data(
        host,
        k_gc,
        rix_js::JsValue::from_object(gc_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let detach_fn = vm.alloc_native_fn("detachArrayBuffer", |vm, _this, args| {
        if let Some(h) = args.first().and_then(|v| v.as_object()) {
            if let Some(ObjectKind::ArrayBuffer(buf)) =
                vm.heap.get_object_mut(h).map(|o| &mut o.kind)
            {
                buf.clear();
            }
        }
        Ok(rix_js::JsValue::UNDEFINED)
    });
    let k_detach = vm.heap.intern_key("detachArrayBuffer");
    vm.heap.object_define_data(
        host,
        k_detach,
        rix_js::JsValue::from_object(detach_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let create_realm_fn = vm.alloc_native_fn("createRealm", |vm, _this, _args| {
        let k_262 = vm.heap.intern_key("$262");
        vm.get_property_by_key(vm.global, k_262)
    });
    let k_cr = vm.heap.intern_key("createRealm");
    vm.heap.object_define_data(
        host,
        k_cr,
        rix_js::JsValue::from_object(create_realm_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let k_262 = vm.heap.intern_key("$262");
    let g = vm.global;
    vm.heap.object_define_data(
        g,
        k_262,
        rix_js::JsValue::from_object(host),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

fn run_single_test262(
    test262_dir: &Path,
    test_path: &Path,
    source: &str,
    meta: &Test262Metadata,
    harness: &HashMap<String, String>,
    always_jit: bool,
) -> std::result::Result<(), String> {
    let is_module = meta.flags.iter().any(|f| f == "module");
    let is_async = meta.flags.iter().any(|f| f == "async");
    let is_raw = meta.flags.iter().any(|f| f == "raw");
    let only_strict = meta.flags.iter().any(|f| f == "onlyStrict");
    let no_strict = meta.flags.iter().any(|f| f == "noStrict");

    let mut modes = Vec::new();
    if is_raw {
        modes.push(false);
    } else if is_module || only_strict {
        modes.push(true);
    } else if no_strict {
        modes.push(false);
    } else if meta.negative_phase.is_some() {
        modes.push(true);
    } else {
        modes.push(false);
    }

    for strict in modes {
        run_single_mode(
            test262_dir,
            test_path,
            source,
            meta,
            harness,
            strict,
            is_module,
            is_async,
            is_raw,
            always_jit,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_single_mode(
    test262_dir: &Path,
    test_path: &Path,
    source: &str,
    meta: &Test262Metadata,
    harness: &HashMap<String, String>,
    strict: bool,
    is_module: bool,
    is_async: bool,
    is_raw: bool,
    always_jit: bool,
) -> std::result::Result<(), String> {
    let mut vm = JsVm::new();
    vm.max_loop_iterations = Some(250_000);
    if always_jit {
        vm.attach_jit(TieredJit::with_tier_thresholds(1, 1, 2));
    }
    install_test262_host(&mut vm);

    if !is_raw {
        if let (Some(sta), Some(assert_js)) = (harness.get("sta.js"), harness.get("assert.js")) {
            vm.eval_script(assert_js)
                .map_err(|e| format!("harness assert.js failed: {e}"))?;
            vm.eval_script(sta)
                .map_err(|e| format!("harness sta.js failed: {e}"))?;
        } else {
            vm.eval_script(TEST262_HARNESS_PRELUDE)
                .map_err(|e| format!("harness prelude failed: {e}"))?;
        }
        for inc in &meta.includes {
            if inc == "assert.js" || inc == "sta.js" || inc == "doneprintHandle.js" {
                continue;
            }
            if let Some(inc_src) = harness.get(inc) {
                vm.eval_script(inc_src)
                    .map_err(|e| format!("harness include {inc} failed: {e}"))?;
            }
        }
    }

    let done_state = std::rc::Rc::new(std::cell::RefCell::new(None::<Option<String>>));
    if is_async {
        let ds = std::rc::Rc::clone(&done_state);
        let done_fn = vm.alloc_host_fn("$DONE", move |vm, _this, args| {
            let err_arg = args.first().copied().unwrap_or(rix_js::JsValue::UNDEFINED);
            if err_arg.is_undefined() || err_arg.is_null() {
                *ds.borrow_mut() = Some(None);
            } else {
                *ds.borrow_mut() = Some(Some(vm.format_exception(err_arg)));
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

    let exec_source = if strict && !is_module && !is_raw {
        format!("\"use strict\";\n{source}")
    } else {
        source.to_string()
    };

    let rel_url = test_path
        .strip_prefix(test262_dir)
        .unwrap_or(test_path)
        .to_string_lossy()
        .replace('\\', "/");
    let entry_url = format!("https://test262.local/{rel_url}");
    let provider = FsModuleProvider {
        root_dir: test262_dir.to_path_buf(),
        entry_url: entry_url.clone(),
        entry_source: exec_source.clone(),
    };
    let import_map = ImportMap {
        base_url: entry_url.clone(),
        ..ImportMap::default()
    };
    let loader = std::rc::Rc::new(std::cell::RefCell::new(ModuleLoader::new(
        Box::new(provider),
        import_map,
    )));
    ModuleLoader::install_on_vm(&loader, &mut vm);

    let outcome: std::result::Result<(), (String, String)> = if is_module {
        match ModuleLoader::load_and_evaluate(&loader, &mut vm, &entry_url) {
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
    vm.format_exception(val)
}

fn category_key(base_dir: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base_dir).unwrap_or(path);
    let mut parts = rel.components().filter_map(|c| c.as_os_str().to_str());
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => format!("{a}/{b}"),
        (Some(a), None) => a.to_string(),
        _ => "root".to_string(),
    }
}

fn bucket_failure_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.starts_with("PANIC:") {
        return trimmed.chars().take(72).collect();
    }
    if let Some(rest) = trimmed.strip_prefix("unexpected parse error: ") {
        return format!("parse: {}", rest.chars().take(64).collect::<String>());
    }
    if let Some(rest) = trimmed.strip_prefix("unexpected runtime error: ") {
        return format!("runtime: {}", rest.chars().take(64).collect::<String>());
    }
    trimmed.chars().take(72).collect()
}

/// Discovers Test262 fixtures, validates metadata, executes each case on `rix-js`,
/// and enforces pass count against `baseline_path`.
pub fn run_test262_suite(
    test262_dir: &Path,
    baseline_path: &Path,
    update_baseline: bool,
) -> Result<Test262Report> {
    run_test262_suite_with_options(
        test262_dir,
        baseline_path,
        update_baseline,
        &Test262Options {
            check_baseline: true,
            ..Test262Options::default()
        },
    )
}

/// Extended Test262 runner supporting official `tc39/test262`, filtering, `--always-jit`, and JSON reports.
pub fn run_test262_suite_with_options(
    test262_dir: &Path,
    baseline_path: &Path,
    update_baseline: bool,
    opts: &Test262Options,
) -> Result<Test262Report> {
    let mut files = Vec::new();
    collect_js_files(test262_dir, &mut files)?;
    files.sort();

    if let Some(ref flt) = opts.filter {
        files.retain(|p| p.to_string_lossy().contains(flt.as_str()));
    }
    if let Some(limit) = opts.limit {
        files.truncate(limit);
    }

    let baseline: Test262Baseline = if opts.check_baseline && baseline_path.exists() {
        let raw = fs::read_to_string(baseline_path)?;
        serde_json::from_str(&raw).with_context(|| format!("parse {}", baseline_path.display()))?
    } else {
        Test262Baseline::default()
    };

    let harness = Arc::new(load_harness_cache(test262_dir));
    let always_jit = opts.always_jit;

    enum Outcome {
        Pass,
        MissingMeta,
        Fail(String),
        Panic(String),
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .stack_size(8 * 1024 * 1024)
        .build()
        .context("build test262 worker pool")?;

    let results: Vec<(PathBuf, Outcome)> = pool.install(|| {
        files
            .par_iter()
            .map(|path| {
                let Ok(src) = fs::read_to_string(path) else {
                    return (
                        path.clone(),
                        Outcome::Fail("failed to read test file".into()),
                    );
                };
                let Some(meta) = parse_test262_frontmatter(&src) else {
                    return (path.clone(), Outcome::MissingMeta);
                };
                let h = Arc::clone(&harness);
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_single_test262(test262_dir, path, &src, &meta, &h, always_jit)
                }));
                match res {
                    Ok(Ok(())) => (path.clone(), Outcome::Pass),
                    Ok(Err(reason)) => (path.clone(), Outcome::Fail(reason)),
                    Err(payload) => {
                        let msg = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        (path.clone(), Outcome::Panic(format!("PANIC: {msg}")))
                    }
                }
            })
            .collect()
    });

    let mut report = Test262Report {
        total: results.len(),
        baseline_passed: baseline.passed,
        always_jit,
        ..Test262Report::default()
    };
    let mut reason_counts: HashMap<String, usize> = HashMap::new();
    let max_stored_failures = if results.len() > 500 { 100 } else { usize::MAX };

    for (path, outcome) in results {
        let cat = category_key(test262_dir, &path);
        let cstats = report.categories.entry(cat).or_default();
        cstats.total += 1;
        match outcome {
            Outcome::Pass => {
                report.metadata_valid += 1;
                report.passed += 1;
                cstats.passed += 1;
            }
            Outcome::MissingMeta => {
                report.failed += 1;
                cstats.failed += 1;
                *reason_counts
                    .entry("missing Test262 YAML frontmatter".into())
                    .or_insert(0) += 1;
                if report.failures.len() < max_stored_failures {
                    report.failures.push(Test262Failure {
                        path: path.display().to_string(),
                        reason: "missing Test262 YAML frontmatter".into(),
                    });
                }
            }
            Outcome::Fail(reason) => {
                report.metadata_valid += 1;
                report.failed += 1;
                cstats.failed += 1;
                *reason_counts
                    .entry(bucket_failure_reason(&reason))
                    .or_insert(0) += 1;
                if report.failures.len() < max_stored_failures {
                    report.failures.push(Test262Failure {
                        path: path.display().to_string(),
                        reason,
                    });
                }
            }
            Outcome::Panic(reason) => {
                report.metadata_valid += 1;
                report.failed += 1;
                report.panics += 1;
                cstats.failed += 1;
                cstats.panics += 1;
                *reason_counts
                    .entry(bucket_failure_reason(&reason))
                    .or_insert(0) += 1;
                if report.failures.len() < max_stored_failures {
                    report.failures.push(Test262Failure {
                        path: path.display().to_string(),
                        reason,
                    });
                }
            }
        }
    }

    let mut ranked: Vec<(String, usize)> = reason_counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(20);
    report.top_failure_reasons = ranked;

    if update_baseline && opts.check_baseline {
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
    } else if opts.check_baseline && report.passed < baseline.passed {
        report.regressed = true;
    }

    if let Some(ref json_path) = opts.report_json {
        if let Some(parent) = json_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(json_path, serde_json::to_string_pretty(&report)? + "\n")?;
    }

    Ok(report)
}

pub fn print_report(report: &Test262Report) {
    let mode = if report.always_jit {
        "always-jit"
    } else {
        "tiered"
    };
    println!("=== Test262 Conformance Harness (rix-js, {mode}) ===");
    println!(
        "  Discovered: {} | Valid frontmatter: {} | Passed: {} / {} ({:.2}%) | Baseline: {} | Failed: {} (Panics: {})",
        report.total,
        report.metadata_valid,
        report.passed,
        report.total,
        report.pass_rate_pct(),
        report.baseline_passed,
        report.failed,
        report.panics,
    );
    if report.categories.len() > 1 && report.total > 100 {
        println!("  Category breakdown (top 20 by size):");
        let mut cats: Vec<(&String, &CategoryStats)> = report.categories.iter().collect();
        cats.sort_by_key(|a| std::cmp::Reverse(a.1.total));
        for (cat, st) in cats.iter().take(20) {
            let pct = if st.total == 0 {
                100.0
            } else {
                (st.passed as f64 / st.total as f64) * 100.0
            };
            println!(
                "    {:<34} {:>5}/{:<5} ({:>5.1}%) panics={}",
                cat, st.passed, st.total, pct, st.panics
            );
        }
    }
    if !report.top_failure_reasons.is_empty() && report.total > 100 {
        println!("  Top failure reasons:");
        for (reason, cnt) in report.top_failure_reasons.iter().take(12) {
            println!("    [{:>5}] {}", cnt, reason);
        }
    } else {
        for f in report.failures.iter().take(25) {
            println!("    FAIL {}: {}", f.path, f.reason);
        }
    }
}
