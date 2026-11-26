use rix_js::{
    compile, BytecodeCache, CacheStatus, CompiledProgram, ConstantEntry, IcKind, Instruction,
    Opcode, SourceType, BYTECODE_VERSION,
};
use rix_storage::StorageEngine;
use std::mem::size_of;
use tempfile::tempdir;

#[test]
fn instruction_is_fixed_32_bit_and_emits_ic_slots_for_property_and_method_ops() {
    assert_eq!(size_of::<Instruction>(), 4);

    let src = r#"
        function sumPoints(points) {
            let total = 0;
            for (let i = 0; i < points.length; i++) {
                const p = points[i];
                p.visited = true;
                total += p.x + p.compute(i);
            }
            return total;
        }
    "#;
    let prog = compile(src, SourceType::Script).expect("should compile");
    assert_eq!(prog.bytecode_version, BYTECODE_VERSION);

    let sum_fn = prog
        .functions
        .iter()
        .find(|f| f.name.as_deref() == Some("sumPoints"))
        .expect("sumPoints compiled");

    assert!(
        sum_fn
            .ic_slots
            .iter()
            .any(|ic| ic.kind == IcKind::GetProp && const_str(sum_fn, ic.key_const) == Some("x")),
        "GetProp IC slot for `.x` must exist"
    );
    assert!(
        sum_fn
            .ic_slots
            .iter()
            .any(|ic| ic.kind == IcKind::SetProp
                && const_str(sum_fn, ic.key_const) == Some("visited")),
        "SetProp IC slot for `.visited` must exist"
    );
    assert!(
        sum_fn.ic_slots.iter().any(|ic| {
            ic.kind == IcKind::CallMethod
                && ic.argc == 1
                && const_str(sum_fn, ic.key_const) == Some("compute")
        }),
        "CallMethod IC slot for `.compute(i)` with argc=1 must exist"
    );
    assert!(
        sum_fn
            .code
            .iter()
            .any(|inst| inst.opcode() == Some(Opcode::LoopBack)),
        "Loop back-edge must emit Opcode::LoopBack"
    );
}

#[test]
fn compiles_nested_upvalues_classes_destructuring_optional_chaining_async_generators_and_try() {
    let src = r#"
        function outer(a) {
            const secret = a * 2;
            function middle(b) {
                return () => secret + b;
            }
            return middle(10);
        }

        class Counter extends Base {
            static version = 1;
            #count = 0;
            static { this.ready = true; }
            constructor(start = 0) {
                super();
                this.#count = start;
            }
            inc() { return ++this.#count; }
        }

        async function* pipeline(items, { scale = 1, ...opts } = {}) {
            try {
                for await (const [k, v = 0, ...rest] of items) {
                    const out = opts?.transform?.(k, v, ...rest) ?? v * scale;
                    yield out;
                }
            } catch (err) {
                throw err;
            } finally {
                cleanup();
            }
        }
    "#;
    let prog = compile(src, SourceType::Script).expect("complex ES2024 should compile");

    // Verify 2-level nested closure captured `secret` as a forwarded upvalue!
    let middle_fn = prog
        .functions
        .iter()
        .find(|f| f.name.as_deref() == Some("middle"))
        .expect("middle compiled");
    assert_eq!(middle_fn.upvalues.len(), 1);
    assert!(middle_fn.upvalues[0].is_local);
    assert!(middle_fn.upvalues[0].is_const);

    let arrow_fn = prog
        .functions
        .iter()
        .find(|f| f.is_arrow)
        .expect("arrow compiled");
    assert_eq!(arrow_fn.upvalues.len(), 2);
    assert!(arrow_fn.upvalues.iter().any(|u| !u.is_local)); // forwarded `secret`
    assert!(arrow_fn.upvalues.iter().any(|u| u.is_local)); // direct `b`

    // Verify async generator compiled with exception handlers (`catch` + `finally`) and `Yield`/`Await`.
    let pipe_fn = prog
        .functions
        .iter()
        .find(|f| f.name.as_deref() == Some("pipeline"))
        .expect("pipeline compiled");
    assert!(pipe_fn.is_async && pipe_fn.is_generator);
    assert!(pipe_fn.exception_handlers.len() >= 2);
    assert!(pipe_fn.exception_handlers.iter().any(|h| !h.is_finally));
    assert!(pipe_fn.exception_handlers.iter().any(|h| h.is_finally));
    assert!(pipe_fn
        .code
        .iter()
        .any(|i| i.opcode() == Some(Opcode::Yield)));
    assert!(pipe_fn
        .code
        .iter()
        .any(|i| i.opcode() == Some(Opcode::Await)));
    assert!(pipe_fn
        .code
        .iter()
        .any(|i| i.opcode() == Some(Opcode::JumpIfNullish)));
}

#[test]
fn compiles_es_module_metadata_and_top_level_await() {
    let src = r#"
        import def, { foo as f } from "./dep.js";
        import * as ns from "./all.js";
        const data = await import("./lazy.js");
        export const answer = f + def + data.x;
        export { ns as namespace };
        export * from "./reexport.js";
        export default function main() { return answer; }
    "#;
    let prog = compile(src, SourceType::Module).expect("module should compile");
    let module = prog.module.as_ref().expect("module metadata");
    assert!(module.has_top_level_await);
    assert_eq!(module.imports.len(), 3);
    assert_eq!(module.exports.len(), 4);
    assert!(prog
        .entry()
        .code
        .iter()
        .any(|i| i.opcode() == Some(Opcode::DynamicImport)));
    assert!(prog
        .entry()
        .code
        .iter()
        .any(|i| i.opcode() == Some(Opcode::SetModuleExport)));
}

#[test]
fn persistent_bytecode_disk_cache_roundtrip_skips_second_compile_across_reopen() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("bytecode-cache.redb");
    let source = r#"
        export function fib(n) {
            let a = 0, b = 1;
            for (let i = 0; i < n; i++) {
                const next = a + b;
                a = b;
                b = next;
            }
            return a;
        }
    "#;

    // First process / cold cache -> Miss (compiles & persists to disk).
    let first: CompiledProgram = {
        let storage = StorageEngine::open(&db_path).expect("open db");
        let cache = BytecodeCache::new(&storage);
        let (prog, status) = cache
            .compile_or_load(source, SourceType::Module)
            .expect("cold compile");
        assert_eq!(status, CacheStatus::Miss);
        prog
    };

    // Reopen fresh StorageEngine from disk -> Hit (deserializes straight from cache!).
    {
        let storage = StorageEngine::open(&db_path).expect("reopen db");
        let cache = BytecodeCache::new(&storage);
        let (second, status) = cache
            .compile_or_load(source, SourceType::Module)
            .expect("warm load");
        assert_eq!(status, CacheStatus::Hit);
        assert_eq!(first, second);

        // Modified source produces a distinct SHA-256 key -> Miss.
        let modified = format!("{source}\nexport const extra = 1;");
        let (_third, mod_status) = cache
            .compile_or_load(&modified, SourceType::Module)
            .expect("modified compile");
        assert_eq!(mod_status, CacheStatus::Miss);

        // Corrupt cached bytes fall back cleanly to recompilation.
        let hash = BytecodeCache::source_sha256(source);
        let key = BytecodeCache::cache_key(&hash, SourceType::Module);
        storage
            .put_bytecode_cache(&key, b"corrupt-payload")
            .expect("inject corrupt payload");
        let (rebuilt, status_after_corrupt) = cache
            .compile_or_load(source, SourceType::Module)
            .expect("corrupt fallback");
        assert_eq!(status_after_corrupt, CacheStatus::Miss);
        assert_eq!(rebuilt, first);
    }
}

fn const_str(func: &rix_js::FunctionBytecode, idx: u32) -> Option<&str> {
    match func.constants.get(idx as usize)? {
        ConstantEntry::String(s) => Some(s.as_str()),
        _ => None,
    }
}

#[test]
fn wide_local_spill_slots_execute_and_capture_upvalues_above_250_locals() {
    // ECMA-262 imposes no 250-local limit; minified production bundles routinely exceed 300 locals.
    let mut src = String::from("(function() {\n");
    for i in 0..320 {
        src.push_str(&format!("  let v{i} = {i};\n"));
    }
    src.push_str("  const reader = () => v0 + v179 + v180 + v250 + v319;\n");
    src.push_str("  v250 += 1000;\n");
    src.push_str("  return reader() + v319;\n");
    src.push_str("})()");

    let mut vm = rix_js::JsVm::new();
    let val = vm.eval_script(&src).expect("320 locals must compile & run");
    // 0 + 179 + 180 + 1250 + 319 + 319 = 2247
    assert_eq!(val.as_number(), Some(2247.0));
}

#[test]
fn allow_in_resets_inside_function_and_nested_delimiters_in_for_init() {
    // ECMA-262 §13-15: `FunctionBody`, `(...)`, `[...]`, `{...}`, and call args restore `[+In]`
    // even when syntactically nested inside a `for (...)` initializer (`[~In]`).
    let src = r#"
        let out = 0;
        for (
            !function(obj) {
                if ("a" in obj && ("b" in obj)) out += 1;
                const fn2 = () => { return "a" in obj ? 2 : 0; };
                out += fn2();
            }({ a: 1, b: 2 }),
            [{ k: "a" in { a: 1 } }][0].k;
            false;
        ) {}
        out
    "#;
    let mut vm = rix_js::JsVm::new();
    let val = vm.eval_script(src).expect("nested +In in for-init parses");
    assert_eq!(val.as_number(), Some(3.0));
}

#[test]
fn member_assignment_evaluates_lhs_object_and_key_before_rhs_per_ecma262() {
    // ECMA-262 §13.15.2: LeftHandSideExpression (member base object and computed key) MUST
    // evaluate before the right-hand AssignmentExpression, and only once in compound/logical
    // assignments.
    let src = r#"
        let b, ce = {};
        (b = ce.expr = { pseudos: { eq: 42 } }).pseudos.nth = b.pseudos.eq;
        let arr = [0, 0, 0];
        let i = 0;
        arr[i++] = i; // LHS key evaluates to 0 first, then RHS evaluates to 1 -> arr[0] = 1
        let keyCalls = 0;
        let obj = { k0: 10, k1: 0 };
        function nextKey() { return "k" + (keyCalls++); }
        obj[nextKey()] += 5;  // keyCalls increments once (k0 -> 15)
        obj[nextKey()] ||= 7; // keyCalls increments once (k1 -> 7)
        b.pseudos.nth * 1000 + arr[0] * 100 + obj.k0 + obj.k1 + keyCalls * 100000
    "#;
    let mut vm = rix_js::JsVm::new();
    let val = vm
        .eval_script(src)
        .expect("ECMA-262 §13.15.2 member assignment order");
    // 2*100000 + 42*1000 + 1*100 + 15 + 7 = 242122
    assert_eq!(val.as_number(), Some(242_122.0));
}

#[test]
fn closure_captures_more_than_255_upvalues_with_16bit_indices() {
    // Production webpack/Rollup bundles routinely capture > 255 upvalues in a single IIFE.
    let mut src = String::from("(function() {\n");
    for i in 0..310 {
        src.push_str(&format!("  let u{i} = {i};\n"));
    }
    src.push_str("  const f = () => {\n    let s = 0;\n");
    for i in 0..310 {
        src.push_str(&format!("    s += u{i};\n"));
    }
    src.push_str("    u309 += 1000;\n    return s + u309;\n  };\n  return f();\n})()");

    let mut vm = rix_js::JsVm::new();
    let val = vm
        .eval_script(&src)
        .expect("310 captured upvalues must compile and execute");
    // sum(0..310) = 309 * 310 / 2 = 47895; + 1309 = 49204
    assert_eq!(val.as_number(), Some(49_204.0));
}

#[test]
fn large_argument_call_and_new_spill_cleanly_without_hitting_250_reg_limit() {
    // Calling a function or constructor with > 150 arguments inside a function with > 120 locals
    // must transparently spill arguments via CallSpread / ConstructSpread without hitting the
    // 250-register window limit.
    let mut src = String::from("(function() {\n");
    for i in 0..130 {
        src.push_str(&format!("  let x{i} = {i};\n"));
    }
    src.push_str("  function sum() { let t = 0; for (let i = 0; i < arguments.length; i++) t += arguments[i]; return t; }\n");
    src.push_str("  function Box() { this.n = arguments.length; }\n");
    let args: Vec<String> = (0..180).map(|i| format!("x{}", i % 130)).collect();
    let arg_list = args.join(", ");
    src.push_str(&format!("  const s = sum({arg_list});\n"));
    src.push_str(&format!("  const b = new Box({arg_list});\n"));
    src.push_str("  return s + b.n;\n})()");

    let mut vm = rix_js::JsVm::new();
    let val = vm
        .eval_script(&src)
        .expect("large call/new arg lists with many locals must compile and run");
    // sum(0..130) + sum(0..50) + 180 = 8385 + 1225 + 180 = 9790
    assert_eq!(val.as_number(), Some(9_790.0));
}

#[test]
fn labeled_statements_nested_labels_and_unlabeled_break_follow_ecma262() {
    // ECMA-262 §14.13, §14.8, §14.9: labeled `if`, `try`, nested labels (`a: b: ...`),
    // and unlabeled `break` inside a labeled block inside a loop must all resolve cleanly.
    let src = r#"
        let r = 0;
        // 1. Labeled `if` statement (`react.dev` / Terser pattern)
        lblIf: if (true) {
            r += 1;
            break lblIf;
            r += 1000;
        }
        // 2. Labeled `try` statement
        lblTry: try {
            r += 2;
            break lblTry;
            r += 1000;
        } catch (e) {
            r += 1000;
        }
        // 3. Nested labels on a loop (both labels valid for continue and break)
        let loopSum = 0;
        outerA: outerB: for (let i = 0; i < 5; i++) {
            if (i === 1) continue outerA;
            if (i === 3) break outerB;
            loopSum += i + 1;
        }
        // i=0 -> +1, i=1 -> skip, i=2 -> +3, i=3 -> break => loopSum = 4
        r += loopSum * 10;

        // 4. Nested labels on a block
        b1: b2: {
            r += 100;
            break b1;
            r += 10000;
        }

        // 5. Unlabeled `break` inside a labeled block inside a loop breaks the LOOP, not the block
        let iters = 0;
        while (iters < 10) {
            iters++;
            innerLbl: {
                if (iters === 2) break;
            }
            if (iters === 2) r += 10000; // must NOT execute!
        }
        r += iters * 1000;
        r
    "#;
    let mut vm = rix_js::JsVm::new();
    let val = vm
        .eval_script(src)
        .expect("ECMA-262 §14.13 labeled statements must compile and execute");
    // 1 + 2 + 40 + 100 + 2000 = 2143
    assert_eq!(val.as_number(), Some(2_143.0));

    // `continue` targeting a non-iteration labeled statement must be rejected.
    let bad = vm.eval_script("blk: { continue blk; }");
    assert!(
        bad.is_err(),
        "ECMA-262 §14.8: continue targeting a non-loop label must fail"
    );
}
