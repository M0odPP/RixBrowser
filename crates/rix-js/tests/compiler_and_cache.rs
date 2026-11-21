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
    assert_eq!(pipe_fn.exception_handlers.len(), 2);
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
