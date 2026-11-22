//! Comprehensive Tier-2 Baseline Cranelift JIT & OSR test suite (Issue #19).
//!
//! Verifies:
//! 1. Strict W^X `CodePage` lifecycle and deterministic `munmap` on realm drop (zero leaks).
//! 2. Function hotness (`BASELINE_THRESHOLD = 32`) and loop back-edge OSR (`OSR_THRESHOLD = 256`).
//! 3. Exact ES2024 NaN-boxed semantics (Int32 overflow -> f64, `-0.0`, NaN, bitwise, strings,
//!    BigInt, getters/setters, dense/sparse arrays, closures, recursion, `try`/`catch`/`finally`,
//!    and cooperative loop-budget interrupts).
//! 4. 100% Test262 suite parity (every fixture in `fixtures/test262/` matches Tier-1 Bytecode VM
//!    identically under aggressive `with_thresholds(1, 1)` JIT compilation).

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use rix_jit::{live_code_bytes, live_code_pages, total_unmapped_pages, BaselineJit};
use rix_js::{JsValue, JsVm};

/// Coordinates process-wide `LIVE_CODE_PAGES` leak inspection against concurrent JIT tests.
static LEAK_LOCK: RwLock<()> = RwLock::new(());

fn eval_tier1(src: &str) -> Result<String, String> {
    let mut vm = JsVm::new();
    match vm.eval_script(src) {
        Ok(v) => Ok(format_val(&mut vm, v)),
        Err(e) => Err(e.to_string()),
    }
}

fn eval_tier2(
    src: &str,
    baseline_threshold: u32,
    osr_threshold: u32,
) -> (Result<String, String>, rix_jit::JitStats) {
    let _shared = LEAK_LOCK.read().expect("leak read lock");
    let mut vm = JsVm::new();
    let jit = BaselineJit::with_thresholds(baseline_threshold, osr_threshold);
    vm.attach_jit(jit.clone());
    let res = match vm.eval_script(src) {
        Ok(v) => Ok(format_val(&mut vm, v)),
        Err(e) => Err(e.to_string()),
    };
    let stats = jit.stats();
    (res, stats)
}

fn format_val(vm: &mut JsVm, v: JsValue) -> String {
    if let Some(n) = v.as_f64() {
        if n == 0.0 && n.is_sign_negative() {
            return "-0".to_string();
        }
        if n.is_nan() {
            return "NaN".to_string();
        }
    }
    vm.value_to_rust_string(v)
}

fn assert_parity(name: &str, src: &str) {
    let t1 = eval_tier1(src);
    let (t2, stats) = eval_tier2(src, 1, 1);
    assert_eq!(
        t1, t2,
        "Tier-1 vs Tier-2 mismatch on `{name}`:\n  Tier 1: {t1:?}\n  Tier 2: {t2:?}\n  Stats: {stats:?}"
    );
    assert!(
        stats.compiled_functions > 0,
        "Expected `{name}` to compile at least one function via Baseline JIT"
    );
}

#[test]
fn wx_code_pages_unmap_deterministically_on_realm_drop() {
    let _guard = LEAK_LOCK.write().expect("leak write lock");

    let pages_before = live_code_pages();
    let bytes_before = live_code_bytes();
    let unmapped_before = total_unmapped_pages();

    let mut total_compiled = 0u64;
    for round in 0..5 {
        let mut vm = JsVm::new();
        let jit = BaselineJit::attach_to(&mut vm);
        let src = format!(
            r#"
            function hot_{round}(n) {{
                let s = 0;
                for (let i = 0; i < n; i++) s += i;
                return s;
            }}
            let out = 0;
            for (let k = 0; k < 40; k++) out += hot_{round}(10);
            out;
            "#
        );
        let res = vm.eval_script(&src).expect("eval");
        assert_eq!(res.as_number(), Some(1800.0));
        let st = jit.stats();
        assert!(st.compiled_functions >= 1);
        assert!(st.baseline_entries >= 1);
        assert!(jit.compiled_code_bytes() > 0);
        assert!(live_code_pages() > pages_before);
        total_compiled += st.compiled_functions;
        // `vm` and `jit` both drop here -> every `CodePage` must `munmap`.
    }

    assert_eq!(
        live_code_pages(),
        pages_before,
        "All W^X CodePages must be unmapped when realms drop (zero leaks)"
    );
    assert_eq!(live_code_bytes(), bytes_before);
    assert!(
        total_unmapped_pages() >= unmapped_before + total_compiled,
        "Expected at least {total_compiled} munmap reclamations"
    );
}

#[test]
fn baseline_threshold_32_and_osr_threshold_256_triggers() {
    let _guard = LEAK_LOCK.read().expect("leak read lock");

    // 1. Default BASELINE_THRESHOLD = 32 triggers on 32nd call of a small function.
    let call_src = r#"
        function addOne(x) { return x + 1; }
        let acc = 0;
        for (let i = 0; i < 64; i++) {
            acc += addOne(i);
        }
        acc;
    "#;
    let mut vm1 = JsVm::new();
    let jit1 = BaselineJit::attach_to(&mut vm1);
    let r1 = vm1.eval_script(call_src).expect("eval");
    assert_eq!(r1.as_number(), Some(2080.0));
    let s1 = jit1.stats();
    assert!(
        s1.compiled_functions >= 1,
        "Expected compiled_functions >= 1, stats={s1:?}, last_err={:?}, call_counts={:?}",
        jit1.last_error(),
        vm1.call_counts
    );
    assert!(
        s1.baseline_entries >= 32,
        "Expected >= 32 baseline entries after 64 calls, got {s1:?}"
    );

    // 2. Default OSR_THRESHOLD = 256 triggers mid-loop on a single-call function.
    let osr_src = r#"
        function singleBigLoop(n) {
            let sum = 0;
            let i = 0;
            while (i < n) {
                sum = sum + i;
                i = i + 1;
            }
            return sum;
        }
        singleBigLoop(2000);
    "#;
    let mut vm2 = JsVm::new();
    let jit2 = BaselineJit::attach_to(&mut vm2);
    let r2 = vm2.eval_script(osr_src).expect("eval");
    assert_eq!(r2.as_number(), Some(1_999_000.0));
    let s2 = jit2.stats();
    assert!(s2.compiled_functions >= 1);
    assert!(
        s2.osr_entries >= 1,
        "Expected OSR entry inside single-call loop, got {s2:?}"
    );
}

#[test]
fn nan_boxed_arithmetic_overflow_neg_zero_and_bitwise_parity() {
    assert_parity(
        "int32_overflow_to_f64",
        r#"
        function f() {
            const a = 2147483647 + 1;
            const b = -2147483648 - 1;
            const c = 100000 * 100000;
            const d = -2147483648 * -1;
            const e = -(-2147483648);
            let x = 2147483647;
            x++;
            let y = -2147483648;
            y--;
            return [a, b, c, d, e, x, y].join(",");
        }
        f();
        "#,
    );

    assert_parity(
        "negative_zero_preservation",
        r#"
        function f() {
            const mz1 = -1 * 0;
            const mz2 = 0 * -1;
            const mz3 = -0;
            const mz4 = -5 * 0;
            const pz = 0 * 5;
            return [
                Object.is(mz1, -0),
                Object.is(mz2, -0),
                Object.is(mz3, -0),
                1 / mz4 === -Infinity,
                1 / pz === Infinity,
                mz1 === 0,
                mz1 == 0
            ].join(",");
        }
        f();
        "#,
    );

    assert_parity(
        "div_rem_nan_and_bitwise",
        r#"
        function f() {
            const a = 1 / 0;
            const b = -1 / 0;
            const c = 0 / 0;
            const d = -5 % 2;
            const e = 5.5 % 2;
            const g = (-1 >>> 0);
            const h = (0x12345678 & 0x0F0F0F0F) | (0x00FF00FF ^ 0x55555555);
            const k = (~0) + (1 << 5) + (-16 >> 2);
            return [a, b, Number.isNaN(c), d, e, g, h, k].join(",");
        }
        f();
        "#,
    );

    assert_parity(
        "strings_bigint_and_abstract_equality",
        r#"
        function f() {
            const s = "hello " + 42 + " " + true;
            const bi = (100n + 23n) * 2n;
            const eqs = [
                NaN === NaN,
                NaN !== NaN,
                null == undefined,
                null === undefined,
                "42" == 42,
                "42" === 42,
                0 == false,
                "" == 0,
                "abc" < "abd",
                "10" < "2",
                10 < "2",
                NaN < 1,
                NaN <= 1,
                NaN > 1,
                NaN >= 1
            ];
            return s + "|" + bi.toString() + "|" + eqs.join(",");
        }
        f();
        "#,
    );
}

#[test]
fn properties_arrays_closures_recursion_and_exceptions_parity() {
    assert_parity(
        "monomorphic_polymorphic_and_getter_props",
        r#"
        function readX(o) { return o.x + o.y; }
        function main() {
            const a = { x: 10, y: 20 };
            const b = { y: 5, x: 7, z: 99 };
            const c = {
                _v: 1,
                get x() { this._v += 10; return this._v; },
                y: 3
            };
            let total = 0;
            for (let i = 0; i < 40; i++) {
                a.x = a.x + 1;
                total += readX(a);
            }
            total += readX(b) + readX(c) + readX(c);
            return total;
        }
        main();
        "#,
    );

    assert_parity(
        "dense_and_sparse_arrays",
        r#"
        function sumArr(arr) {
            let s = 0;
            for (let i = 0; i < arr.length; i++) {
                arr[i] = (arr[i] || 0) + 1;
                s += arr[i];
            }
            return s;
        }
        const dense = [1, 2, 3, 4, 5, 6, 7, 8];
        const sparse = [1, , 3];
        sumArr(dense) + "," + sumArr(sparse) + "," + dense.join(":");
        "#,
    );

    assert_parity(
        "closures_upvalues_and_recursion",
        r#"
        function makeCounter(start) {
            let count = start;
            const inc = (step) => {
                count = count + step;
                return count;
            };
            count += 10;
            return inc;
        }
        function fib(n) {
            if (n <= 1) return n;
            return fib(n - 1) + fib(n - 2);
        }
        const c = makeCounter(5);
        [c(1), c(2), c(3), fib(14)].join(",");
        "#,
    );

    assert_parity(
        "try_catch_finally_from_jit_frame",
        r#"
        function boom(flag) {
            let local = 10;
            let caught = "none";
            try {
                local = 42;
                if (flag === 1) {
                    throw new Error("explicit-" + local);
                } else if (flag === 2) {
                    const n = null;
                    return n.prop;
                }
            } catch (e) {
                caught = e.message + ":" + local;
                local += 8;
            } finally {
                local += 1;
            }
            return caught + "|" + local;
        }
        [boom(0), boom(1), boom(2)].join(" || ");
        "#,
    );
}

#[test]
fn cooperative_loop_budget_interrupts_jit_osr_loop() {
    let _guard = LEAK_LOCK.read().expect("leak read lock");
    let mut vm = JsVm::new();
    let jit = BaselineJit::with_thresholds(1, 16);
    vm.attach_jit(jit.clone());
    vm.max_loop_iterations = Some(200);

    let err = vm
        .eval_script(
            r#"
            function spin() {
                let i = 0;
                while (true) {
                    i = (i + 1) | 0;
                }
            }
            spin();
            "#,
        )
        .expect_err("infinite loop must be interrupted by loop budget");
    assert!(
        err.to_string().contains("loop budget exceeded"),
        "unexpected error: {err}"
    );
    let st = jit.stats();
    assert!(st.compiled_functions >= 1);
    assert!(st.exception_exits >= 1);
}

fn collect_js_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect_js_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "js") {
            out.push(p);
        }
    }
}

const TEST262_PRELUDE: &str = r#"
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

#[derive(Default)]
struct MiniMeta {
    is_module: bool,
    is_async: bool,
    strict: bool,
    negative_phase: Option<String>,
    negative_type: Option<String>,
}

fn parse_meta(src: &str) -> MiniMeta {
    let mut m = MiniMeta::default();
    let Some(start) = src.find("/*---") else {
        return m;
    };
    let rest = &src[start + 5..];
    let Some(end) = rest.find("---*/") else {
        return m;
    };
    let mut in_neg = false;
    let mut flags = Vec::new();
    for line in rest[..end].lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_neg = false;
        }
        if let Some(v) = t.strip_prefix("flags:") {
            for item in v
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
            {
                let s = item.trim().trim_matches('"').trim_matches('\'');
                if !s.is_empty() {
                    flags.push(s.to_string());
                }
            }
        } else if t == "negative:" {
            in_neg = true;
        } else if in_neg {
            if let Some(v) = t.strip_prefix("phase:") {
                m.negative_phase = Some(v.trim().to_string());
            } else if let Some(v) = t.strip_prefix("type:") {
                m.negative_type = Some(v.trim().to_string());
            }
        }
    }
    m.is_module = flags.iter().any(|f| f == "module");
    m.is_async = flags.iter().any(|f| f == "async");
    let only_strict = flags.iter().any(|f| f == "onlyStrict");
    let no_strict = flags.iter().any(|f| f == "noStrict");
    m.strict = m.is_module || only_strict || (!no_strict && m.negative_phase.is_some());
    m
}

fn run_test262_file(src: &str, with_jit: bool) -> (Result<(), String>, rix_jit::JitStats) {
    let meta = parse_meta(src);
    let mut vm = JsVm::new();
    vm.max_loop_iterations = Some(500_000);
    let jit_opt = if with_jit {
        let jit = BaselineJit::with_thresholds(1, 1);
        vm.attach_jit(jit.clone());
        Some(jit)
    } else {
        None
    };

    if let Err(e) = vm.eval_script(TEST262_PRELUDE) {
        return (
            Err(format!("prelude failed: {e}")),
            rix_jit::JitStats::default(),
        );
    }

    let done_state = std::rc::Rc::new(std::cell::RefCell::new(None::<Option<String>>));
    if meta.is_async {
        let ds = std::rc::Rc::clone(&done_state);
        let done_fn = vm.alloc_host_fn("$DONE", move |vm, _this, args| {
            let err_arg = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            if err_arg.is_undefined() || err_arg.is_null() {
                *ds.borrow_mut() = Some(None);
            } else {
                *ds.borrow_mut() = Some(Some(vm.value_to_rust_string(err_arg)));
            }
            Ok(JsValue::UNDEFINED)
        });
        let k_done = vm.heap.intern_key("$DONE");
        let g = vm.global;
        vm.heap.object_define_data(
            g,
            k_done,
            JsValue::from_object(done_fn),
            rix_js::PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    let exec_src = if meta.strict && !meta.is_module {
        format!("\"use strict\";\n{src}")
    } else {
        src.to_string()
    };

    let outcome: Result<(), (String, String)> = if meta.is_module {
        match rix_js::ModuleLoader::eval_single_module(
            &mut vm,
            "https://test262.local/test.mjs",
            &exec_src,
        ) {
            Ok(prom_h) => {
                vm.drain_microtasks();
                match vm.heap.get_object(prom_h).map(|o| &o.kind) {
                    Some(rix_js::ObjectKind::Promise(p)) => match p.state {
                        rix_js::PromiseState::Fulfilled(_) => Ok(()),
                        rix_js::PromiseState::Rejected(r) => {
                            Err(("runtime".into(), vm.value_to_rust_string(r)))
                        }
                        rix_js::PromiseState::Pending => {
                            Err(("runtime".into(), "ModulePromisePending".into()))
                        }
                    },
                    _ => Ok(()),
                }
            }
            Err(e) => Err(("parse".into(), format!("SyntaxError: {e}"))),
        }
    } else {
        match vm.eval_script(&exec_src) {
            Ok(_) => {
                vm.drain_microtasks();
                Ok(())
            }
            Err(rix_js::EvalError::Compile(e)) => {
                Err(("parse".into(), format!("SyntaxError: {e}")))
            }
            Err(rix_js::EvalError::Runtime(e)) => Err(("runtime".into(), e.message)),
        }
    };

    let stats = jit_opt.map(|j| j.stats()).unwrap_or_default();
    let res = match (&meta.negative_phase, &meta.negative_type) {
        (Some(exp_phase), Some(exp_type)) => match outcome {
            Ok(()) => Err(format!("expected negative {exp_phase}/{exp_type}, got Ok")),
            Err((actual_phase, actual_err)) => {
                let phase_ok = exp_phase == &actual_phase
                    || (exp_phase == "early" && actual_phase == "parse")
                    || (exp_phase == "resolution" && actual_phase == "parse");
                if phase_ok && actual_err.contains(exp_type.as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "expected {exp_phase}/{exp_type}, got {actual_phase}/{actual_err}"
                    ))
                }
            }
        },
        _ => match outcome {
            Err((phase, err)) => Err(format!("unexpected {phase} error: {err}")),
            Ok(()) if meta.is_async => match done_state.borrow().as_ref() {
                Some(None) => Ok(()),
                Some(Some(err)) => Err(format!("$DONE error: {err}")),
                None => Err("$DONE never called".into()),
            },
            Ok(()) => Ok(()),
        },
    };
    (res, stats)
}

#[test]
fn test262_full_suite_matches_tier1_vm_with_zero_miscompilations() {
    let _guard = LEAK_LOCK.read().expect("leak read lock");
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let test262_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("fixtures/test262");
    assert!(
        test262_root.is_dir(),
        "Test262 fixtures directory missing at {}",
        test262_root.display()
    );

    let mut files = Vec::new();
    collect_js_files(&test262_root, &mut files);
    assert!(
        files.len() >= 60,
        "Expected >= 60 Test262 fixtures, found {}",
        files.len()
    );

    let mut total_compiled = 0u64;
    let mut total_entries = 0u64;
    let mut passed = 0usize;

    for file in &files {
        let src = std::fs::read_to_string(file).expect("read test262 fixture");
        let rel = file.strip_prefix(&test262_root).unwrap_or(file);

        let (t1_res, _) = run_test262_file(&src, false);
        let (t2_res, stats) = run_test262_file(&src, true);

        assert_eq!(
            t1_res,
            t2_res,
            "Test262 JIT miscompilation on `{}`:\n  Tier 1: {:?}\n  Tier 2: {:?}\n  Stats: {:?}",
            rel.display(),
            t1_res,
            t2_res,
            stats
        );
        if t1_res.is_ok() {
            passed += 1;
        }
        total_compiled += stats.compiled_functions;
        total_entries += stats.baseline_entries + stats.osr_entries;
    }

    assert_eq!(
        passed,
        files.len(),
        "Expected 100% of Test262 suite to pass on both Tier-1 and Tier-2"
    );
    assert!(
        total_compiled >= files.len() as u64,
        "Expected JIT compilation across Test262 suite, got {total_compiled}"
    );
    assert!(
        total_entries >= files.len() as u64,
        "Expected JIT execution across Test262 suite, got {total_entries}"
    );
}
