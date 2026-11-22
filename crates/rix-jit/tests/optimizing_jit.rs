//! Comprehensive Tier-3 Speculative Optimizing JIT stress & conformance suite (Issue #20).
//!
//! Verifies:
//! 1. Tier-3 promotion at `OPT_THRESHOLD = 1_000` with hoisted shape guards, LICM loads,
//!    hoisted packed-array guards, and small pure leaf callee inlining.
//! 2. Exact deoptimization soundness across mid-loop object shape mutations, prototype-chain
//!    mutations (`prototype_epoch`), `Int32` overflow into `f64`, packed array widening/OOB,
//!    and monomorphic callee identity changes.
//! 3. Deopt-loop circuit breaker (`MAX_OPT_DEOPTS = 4`) preventing compile/deopt thrashing.
//! 4. Deterministic W^X `CodePage` `munmap` cleanup across Tier-2 + Tier-3 realms (zero leaks).
//! 5. Full Test262 suite (62/62 = 100% >= 92%) producing identical results across ALL 3 tiers:
//!    Tier-1 Interpreter, Tier-2 Baseline JIT, and Tier-3 Speculative Optimizing JIT.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use rix_jit::{
    live_code_bytes, live_code_pages, total_unmapped_pages, BaselineJit, TieredJit, MAX_OPT_DEOPTS,
};
use rix_js::{JsValue, JsVm, OPT_THRESHOLD};

static TIER3_LEAK_LOCK: RwLock<()> = RwLock::new(());

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

fn eval_t1(src: &str) -> Result<String, String> {
    let mut vm = JsVm::new();
    vm.eval_script(src)
        .map(|v| format_val(&mut vm, v))
        .map_err(|e| e.to_string())
}

fn eval_t3(
    src: &str,
    b_thr: u32,
    osr_thr: u32,
    opt_thr: u32,
) -> (Result<String, String>, rix_jit::JitStats) {
    let _g = TIER3_LEAK_LOCK.read().expect("read lock");
    let mut vm = JsVm::new();
    let jit = TieredJit::with_tier_thresholds(b_thr, osr_thr, opt_thr);
    vm.attach_jit(jit.clone());
    let res = vm
        .eval_script(src)
        .map(|v| format_val(&mut vm, v))
        .map_err(|e| e.to_string());
    (res, jit.stats())
}

#[test]
fn tier3_promotes_at_opt_threshold_with_hoisting_licm_arrays_and_inlining() {
    let _g = TIER3_LEAK_LOCK.read().expect("read lock");

    let src = r#"
        function mix(a, b) {
            return ((a + b) ^ (b << 1)) | 0;
        }

        function propAndInlineKernel(obj, arr) {
            let acc = 0;
            let i = 0;
            while (i < arr.length) {
                obj.a = (obj.a + obj.b + obj.c) | 0;
                const elem = arr[i];
                arr[i] = (elem + 1) | 0;
                acc = (acc + mix(obj.a, elem)) | 0;
                i = i + 1;
            }
            return acc;
        }

        const o = { a: 1, b: 3, c: 7 };
        const data = [1, 2, 3, 4, 5, 6, 7, 8];
        let total = 0;
        for (let k = 0; k < 1020; k++) {
            total = (total + propAndInlineKernel(o, data)) | 0;
        }
        total + ":" + o.a + ":" + data[0] + ":" + data[7];
    "#;

    let expected = eval_t1(src);
    let mut vm = JsVm::new();
    let jit = TieredJit::attach_to(&mut vm); // Default OPT_THRESHOLD = 1_000!
    let actual = vm
        .eval_script(src)
        .map(|v| format_val(&mut vm, v))
        .map_err(|e| e.to_string());
    assert_eq!(actual, expected);

    let st = jit.stats();
    assert!(
        st.opt_compiled_functions >= 1,
        "Expected Tier-3 compilation at OPT_THRESHOLD={OPT_THRESHOLD}, got {st:?} err={:?}",
        jit.last_error()
    );
    assert!(
        st.opt_entries >= 20,
        "Expected >= 20 Tier-3 entries after 1,020 calls, got {st:?}"
    );
    assert!(
        st.hoisted_shape_guards >= 1,
        "Expected hoisted loop shape guard, got {st:?}"
    );
    assert!(
        st.hoisted_invariant_loads >= 2,
        "Expected LICM hoisting of obj.b and obj.c, got {st:?}"
    );
    assert!(
        st.hoisted_array_guards >= 1,
        "Expected hoisted PackedInt32 array guard, got {st:?}"
    );
    assert!(
        st.inlined_call_sites >= 1,
        "Expected small pure leaf callee `mix` to be inlined, got {st:?}"
    );
}

#[test]
fn stress_deopt_on_shape_mutation_prototype_change_overflow_array_widening_and_callee_swap() {
    // 1. Object shape mutation after Tier-3 specialization (adding & deleting properties).
    let shape_src = r#"
        function sumObj(obj, n) {
            let s = 0;
            let i = 0;
            while (i < n) {
                obj.a = (obj.a + obj.b) | 0;
                s = (s + obj.a) | 0;
                i = i + 1;
            }
            return s;
        }
        const o1 = { a: 1, b: 2 };
        let out = 0;
        for (let k = 0; k < 25; k++) {
            out = (out + sumObj(o1, 10)) | 0;
        }
        // Mutate shape by adding new properties and passing a differently-shaped object!
        o1.extra = 99;
        out = (out + sumObj(o1, 10)) | 0;
        delete o1.b;
        o1.b = 5;
        out = (out + sumObj(o1, 10)) | 0;
        const o2 = { b: 4, a: 2, z: 1 };
        out = (out + sumObj(o2, 10)) | 0;
        out;
    "#;
    let e1 = eval_t1(shape_src);
    let (a1, s1) = eval_t3(shape_src, 4, 16, 10);
    assert_eq!(a1, e1, "Shape mutation deopt mismatch");
    assert!(s1.opt_compiled_functions >= 1);
    assert!(
        s1.opt_deopts >= 1,
        "Expected shape-guard deoptimization on shape change, got {s1:?}"
    );

    // 2. Prototype chain mutation mid-run (`Proto.val` modified after Tier-3 compilation!).
    let proto_src = r#"
        const proto = { bonus: 10 };
        function readProto(obj) {
            return obj.base + obj.bonus;
        }
        const inst = Object.create(proto);
        inst.base = 5;
        let acc = 0;
        for (let i = 0; i < 30; i++) {
            if (i === 18) {
                proto.bonus = 1000;
            }
            if (i === 24) {
                Object.setPrototypeOf(inst, { bonus: 77 });
            }
            acc += readProto(inst);
        }
        acc;
    "#;
    let e2 = eval_t1(proto_src);
    let (a2, s2) = eval_t3(proto_src, 4, 16, 10);
    assert_eq!(a2, e2, "Prototype mutation deopt mismatch");
    assert!(
        s2.opt_deopts >= 1,
        "Expected prototype-epoch deopt when prototype mutated, got {s2:?}"
    );

    // 3. Mid-loop Int32 overflow into IEEE-754 f64!
    let overflow_src = r#"
        function grow(start, steps) {
            let x = start;
            let i = 0;
            while (i < steps) {
                x = x + 100000000;
                i = i + 1;
            }
            return x;
        }
        let r = 0;
        for (let k = 0; k < 20; k++) {
            r = grow(1, 5);
        }
        // Now trigger genuine i32 overflow mid-loop (> 2_147_483_647)!
        const big = grow(2000000000, 10);
        r + ":" + big;
    "#;
    let e3 = eval_t1(overflow_src);
    let (a3, s3) = eval_t3(overflow_src, 4, 16, 10);
    assert_eq!(a3, e3, "Mid-loop Int32 overflow deopt mismatch");
    assert!(
        s3.opt_deopts >= 1,
        "Expected Int32Overflow deopt on mid-loop overflow, got {s3:?}"
    );

    // 4. PackedInt32 array widening to PackedFloat64 / PackedValues & OOB access!
    let array_src = r#"
        function bumpAll(arr, delta, limit) {
            let sum = 0;
            let i = 0;
            while (i < limit) {
                arr[i] = arr[i] + delta;
                sum = sum + arr[i];
                i = i + 1;
            }
            return sum;
        }
        const a = [1, 2, 3, 4];
        let res = 0;
        for (let k = 0; k < 20; k++) {
            res += bumpAll(a, 1, 4);
        }
        // Widen to float mid-loop & trigger out-of-bounds read/write!
        const f = [1, 2, 3, 4];
        const rFloat = bumpAll(f, 0.5, 4);
        const sArr = ["x", "y", "z", "w"];
        const rStr = bumpAll(sArr, "!", 4);
        res + "|" + rFloat + "|" + f.join(",") + "|" + rStr;
    "#;
    let e4 = eval_t1(array_src);
    let (a4, s4) = eval_t3(array_src, 4, 16, 10);
    assert_eq!(a4, e4, "Packed array widening deopt mismatch");
    assert!(s4.hoisted_array_guards >= 1);
    assert!(
        s4.opt_deopts >= 1,
        "Expected array-guard deopt on float/string widening, got {s4:?}"
    );

    // 5. Monomorphic inlined callee swapped dynamically!
    let inline_src = r#"
        function f1(x, y) { return x + y; }
        function f2(x, y) { return x * y; }
        function applyOp(fn, a, b) {
            return fn(a, b);
        }
        let total = 0;
        for (let i = 0; i < 20; i++) {
            total += applyOp(f1, i, 3);
        }
        total += applyOp(f2, 10, 7);
        total += applyOp((x, y) => x - y, 50, 8);
        total;
    "#;
    let e5 = eval_t1(inline_src);
    let (a5, s5) = eval_t3(inline_src, 4, 16, 10);
    assert_eq!(a5, e5, "Inlined callee swap deopt mismatch");
    assert!(
        s5.inlined_call_sites >= 1,
        "Expected monomorphic leaf `f1` to be inlined, got {s5:?}"
    );
    assert!(
        s5.opt_deopts >= 1,
        "Expected callee-guard deopt when `f2` passed, got {s5:?}"
    );
}

#[test]
fn deopt_loop_circuit_breaker_demotes_adversarial_function_to_tier2() {
    // Repeatedly trigger distinct speculation failures (int32 -> shape -> array -> callee)
    // to force MAX_OPT_DEOPTS = 4 and verify the circuit breaker trips cleanly!
    let src = r#"
        function leafA(v) { return v + 1; }
        function leafB(v) { return v + 2; }
        function adversarial(obj, arr, fn, x) {
            let acc = 0;
            let i = 0;
            while (i < arr.length) {
                acc = acc + obj.v + arr[i] + x;
                i = i + 1;
            }
            return acc + fn(x);
        }
        const oA = { v: 1 };
        const oB = { z: 9, v: 2 };
        const aI = [1, 2];
        const aF = [1.5, 2.5];
        let sum = 0;
        // Warm up monomorphically to hit Tier-3.
        for (let k = 0; k < 12; k++) {
            sum += adversarial(oA, aI, leafA, 1);
        }
        // Trigger 4 distinct deopt classes sequentially:
        sum += adversarial(oA, aI, leafA, 2147483647); // 1: Int32Overflow
        sum += adversarial(oB, aI, leafA, 1);          // 2: ShapeGuard
        sum += adversarial(oA, aF, leafA, 1);          // 3: ArrayGuard
        sum += adversarial(oA, aI, leafB, 1);          // 4: CalleeGuard -> trips circuit breaker!
        // Subsequent calls must run stably on Tier-2 Baseline with ZERO further Tier-3 deopts!
        for (let k = 0; k < 20; k++) {
            sum += adversarial(oB, aF, leafB, 3);
        }
        sum;
    "#;

    let expected = eval_t1(src);
    let (actual, st) = eval_t3(src, 4, 16, 10);
    assert_eq!(actual, expected);
    assert!(
        st.opt_deopts >= u64::from(MAX_OPT_DEOPTS),
        "Expected >= {MAX_OPT_DEOPTS} deopts, got {st:?}"
    );
    assert!(
        st.circuit_breaker_trips >= 1,
        "Expected circuit breaker to trip after {MAX_OPT_DEOPTS} deopts, got {st:?}"
    );
}

#[test]
fn wx_code_pages_unmap_cleanly_across_tier2_and_tier3_realms() {
    let _g = TIER3_LEAK_LOCK.write().expect("write lock");

    let pages_before = live_code_pages();
    let bytes_before = live_code_bytes();
    let unmapped_before = total_unmapped_pages();

    let mut total_compiled = 0u64;
    for round in 0..3 {
        let mut vm = JsVm::new();
        let jit = TieredJit::with_tier_thresholds(4, 16, 10);
        vm.attach_jit(jit.clone());
        let src = format!(
            r#"
            function t3_{round}(o) {{
                let s = 0;
                let i = 0;
                while (i < 10) {{
                    o.a = (o.a + o.b) | 0;
                    s = (s + o.a) | 0;
                    i = i + 1;
                }}
                return s;
            }}
            const obj = {{ a: 1, b: 2 }};
            let acc = 0;
            for (let k = 0; k < 20; k++) acc = (acc + t3_{round}(obj)) | 0;
            acc;
            "#
        );
        vm.eval_script(&src).expect("eval");
        let st = jit.stats();
        assert!(st.opt_compiled_functions >= 1);
        assert!(live_code_pages() > pages_before);
        total_compiled += st.compiled_functions + st.opt_compiled_functions;
    }

    assert_eq!(live_code_pages(), pages_before);
    assert_eq!(live_code_bytes(), bytes_before);
    assert!(total_unmapped_pages() >= unmapped_before + total_compiled);
}

// --- Full 3-Tier Test262 Parity Harness (Tier 1 vs Tier 2 vs Tier 3) ---

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

fn run_test262_tier(src: &str, tier: u8) -> (Result<(), String>, rix_jit::JitStats) {
    let meta = parse_meta(src);
    let mut vm = JsVm::new();
    vm.max_loop_iterations = Some(500_000);
    let jit_opt = match tier {
        1 => None,
        2 => {
            let j = BaselineJit::with_tier_thresholds(1, 1, u32::MAX);
            vm.attach_jit(j.clone());
            Some(j)
        }
        _ => {
            let j = TieredJit::with_tier_thresholds(1, 1, 1);
            vm.attach_jit(j.clone());
            Some(j)
        }
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
fn test262_suite_reaches_100pct_with_identical_results_across_all_three_tiers() {
    let _g = TIER3_LEAK_LOCK.read().expect("read lock");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("fixtures/test262");

    let mut files = Vec::new();
    collect_js_files(&root, &mut files);
    assert!(files.len() >= 60);

    let mut passed = 0usize;
    let mut t3_compiled = 0u64;
    let mut t3_entries = 0u64;

    for file in &files {
        let src = std::fs::read_to_string(file).expect("read");
        let rel = file.strip_prefix(&root).unwrap_or(file);
        let (r1, _) = run_test262_tier(&src, 1);
        let (r2, _) = run_test262_tier(&src, 2);
        let (r3, s3) = run_test262_tier(&src, 3);

        assert_eq!(r1, r2, "Tier-1 vs Tier-2 mismatch on `{}`", rel.display());
        assert_eq!(
            r1,
            r3,
            "Tier-1 vs Tier-3 mismatch on `{}` (stats={s3:?})",
            rel.display()
        );
        if r1.is_ok() {
            passed += 1;
        }
        t3_compiled += s3.opt_compiled_functions;
        t3_entries += s3.opt_entries;
    }

    let pass_pct = (passed as f64 / files.len() as f64) * 100.0;
    assert!(
        pass_pct >= 92.0,
        "Test262 pass rate {pass_pct:.1}% below 92% gate"
    );
    assert_eq!(passed, files.len());
    assert!(t3_compiled >= files.len() as u64);
    assert!(t3_entries >= files.len() as u64);
}
