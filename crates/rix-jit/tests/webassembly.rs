//! Comprehensive W3C WebAssembly JS-API specification test suite (Issue #21).
//!
//! Verifies:
//! - `WebAssembly.validate`, `compile`, `compileStreaming`, `instantiate`, `instantiateStreaming`
//! - `Module` (`.exports`, `.imports`, `.customSections`), `Instance` (`.exports`)
//! - `Memory` <-> JS `ArrayBuffer`/`TypedArray`/`DataView` bidirectional bridge, JS & Wasm
//!   `memory.grow`, old `ArrayBuffer` detachment, and `RangeError` on max overflow
//! - `Table` (`.length`, `.get`, `.set`, `.grow`, `call_indirect`)
//! - `Global` (`i32`, `i64` BigInt, `f32`, `f64`, mutable & immutable enforcement)
//! - `CompileError`, `LinkError`, `RuntimeError` traps & JS import exception propagation

use rix_jit::install_webassembly;
use rix_js::heap::{TypedArrayKind, TypedArrayObject};
use rix_js::{JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

fn vm_with_wasm() -> JsVm {
    let mut vm = JsVm::new();
    install_webassembly(&mut vm);
    vm
}

fn bind_wat_bytes(vm: &mut JsVm, global_name: &str, wat_src: &str) {
    let bytes = wat::parse_str(wat_src).expect("valid WAT test module");
    bind_raw_bytes(vm, global_name, bytes);
}

fn bind_raw_bytes(vm: &mut JsVm, global_name: &str, bytes: Vec<u8>) {
    let len = bytes.len() as u32;
    let ab = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::ArrayBuffer(Box::new(bytes)),
        Some(vm.array_buffer_proto),
    ));
    let ta = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::TypedArray(TypedArrayObject {
            kind: TypedArrayKind::Uint8,
            buffer: ab,
            byte_offset: 0,
            length: len,
        }),
        Some(vm.typed_array_protos[TypedArrayKind::Uint8 as usize]),
    ));
    let k = vm.heap.intern_key(global_name);
    let g = vm.global;
    vm.heap.object_define_data(
        g,
        k,
        JsValue::from_object(ta),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
}

fn eval_str(vm: &mut JsVm, src: &str) -> String {
    let val = vm.eval_script(src).expect("script should succeed");
    vm.drain_microtasks();
    vm.value_to_rust_string(val)
}

#[test]
fn validate_compile_module_reflection_and_custom_sections() {
    let mut vm = vm_with_wasm();
    bind_wat_bytes(
        &mut vm,
        "MOD_BYTES",
        r#"
        (module
            (import "env" "scale" (func $scale (param i32) (result i32)))
            (memory (export "mem") 1 4)
            (global (export "answer") i32 (i32.const 42))
            (func (export "calc") (param i32) (result i32)
                local.get 0
                call $scale)
        )
        "#,
    );

    // Append a real Wasm custom section named "rix.meta" with payload b"v0.2"
    let mut with_custom = wat::parse_str("(module (func (export \"noop\")))").expect("wat");
    let mut sec_payload = Vec::new();
    sec_payload.push(8); // name len "rix.meta"
    sec_payload.extend_from_slice(b"rix.meta");
    sec_payload.extend_from_slice(b"v0.2");
    with_custom.push(0); // custom section ID = 0
    with_custom.push(sec_payload.len() as u8);
    with_custom.extend_from_slice(&sec_payload);
    bind_raw_bytes(&mut vm, "CUSTOM_BYTES", with_custom);
    bind_raw_bytes(
        &mut vm,
        "BAD_BYTES",
        vec![0x00, 0x61, 0x73, 0x6D, 0xFF, 0x00],
    );

    let out = eval_str(
        &mut vm,
        r#"
        const validOk = WebAssembly.validate(MOD_BYTES);
        const validBad = WebAssembly.validate(BAD_BYTES);
        const validAb = WebAssembly.validate(MOD_BYTES.buffer);
        let nonBufThrew = false;
        try { WebAssembly.validate(123); } catch (e) { nonBufThrew = (e instanceof TypeError); }

        let compileErrOk = false;
        try { new WebAssembly.Module(BAD_BYTES); } catch (e) {
            compileErrOk = (e instanceof WebAssembly.CompileError) && (e instanceof Error) && e.name === "CompileError";
        }

        const mod = new WebAssembly.Module(MOD_BYTES);
        const exps = WebAssembly.Module.exports(mod).map(e => e.name + ":" + e.kind).join(",");
        const imps = WebAssembly.Module.imports(mod).map(i => i.module + "." + i.name + ":" + i.kind).join(",");

        const cmod = new WebAssembly.Module(CUSTOM_BYTES);
        const secs = WebAssembly.Module.customSections(cmod, "rix.meta");
        const secBytes = new Uint8Array(secs[0]);
        const secStr = String.fromCharCode(secBytes[0], secBytes[1], secBytes[2], secBytes[3]);
        const emptySecs = WebAssembly.Module.customSections(cmod, "missing").length;

        [validOk, validBad, validAb, nonBufThrew, compileErrOk, exps, imps, secs.length, secStr, emptySecs].join("|");
        "#,
    );

    assert_eq!(
        out,
        "true|false|true|true|true|mem:memory,answer:global,calc:function|env.scale:function|1|v0.2|0"
    );
}

#[test]
fn imports_exports_all_value_types_multivalue_and_traps() {
    let mut vm = vm_with_wasm();
    bind_wat_bytes(
        &mut vm,
        "MATH_WASM",
        r#"
        (module
            (import "env" "jsMul" (func $jsMul (param i32 i32) (result i32)))
            (import "env" "jsThrow" (func $jsThrow))
            (import "env" "base" (global $base i32))

            (func (export "compute") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                call $jsMul
                global.get $base
                i32.add)

            (func (export "addI64") (param i64 i64) (result i64)
                local.get 0
                local.get 1
                i64.add)

            (func (export "mixFloats") (param f32 f64) (result f64)
                local.get 0
                f64.promote_f32
                local.get 1
                f64.add)

            (func (export "swap") (param i32 i32) (result i32 i32)
                local.get 1
                local.get 0)

            (func (export "divByZero") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                i32.div_s)

            (func (export "trapUnreachable")
                unreachable)

            (func (export "callJsThrow")
                call $jsThrow)
        )
        "#,
    );

    let out = eval_str(
        &mut vm,
        r#"
        let linkErrOk = false;
        try {
            new WebAssembly.Instance(new WebAssembly.Module(MATH_WASM), { env: { jsMul: 42, jsThrow: () => {}, base: 1 } });
        } catch (e) {
            linkErrOk = (e instanceof WebAssembly.LinkError) && (e instanceof Error);
        }

        let calledCount = 0;
        const inst = new WebAssembly.Instance(new WebAssembly.Module(MATH_WASM), {
            env: {
                jsMul: (a, b) => { calledCount++; return a * b; },
                jsThrow: () => { throw new RangeError("boom-from-js"); },
                base: 100
            }
        });
        const e = inst.exports;
        const r1 = e.compute(6, 7);
        const r64 = e.addI64(9000000000000n, 42n);
        const rf = e.mixFloats(1.5, 2.25);
        const sw = e.swap(11, 22);

        let divTrap = false;
        try { e.divByZero(10, 0); } catch (err) {
            divTrap = (err instanceof WebAssembly.RuntimeError) && (err instanceof Error);
        }

        let unreachTrap = false;
        try { e.trapUnreachable(); } catch (err) {
            unreachTrap = (err instanceof WebAssembly.RuntimeError);
        }

        let jsExcPropagated = "";
        try { e.callJsThrow(); } catch (err) {
            jsExcPropagated = err.name + ":" + err.message;
        }

        [linkErrOk, r1, calledCount, r64.toString(), rf, sw.join(","), divTrap, unreachTrap, jsExcPropagated].join("|");
        "#,
    );

    assert_eq!(
        out,
        "true|142|1|9000000000042|3.75|22,11|true|true|RangeError:boom-from-js"
    );
}

#[test]
fn linear_memory_typed_array_bridge_and_growth_detachment() {
    let mut vm = vm_with_wasm();
    bind_wat_bytes(
        &mut vm,
        "MEM_WASM",
        r#"
        (module
            (import "env" "mem" (memory 1 3))
            (import "env" "inspectFromJs" (func $inspect (param i32) (result i32)))

            (func (export "sumI32") (param $ptr i32) (param $len i32) (result i32)
                (local $i i32)
                (local $acc i32)
                (block $done
                    (loop $lp
                        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
                        (local.set $acc
                            (i32.add (local.get $acc)
                                (i32.load (i32.add (local.get $ptr) (i32.shl (local.get $i) (i32.const 2))))))
                        (local.set $i (i32.add (local.get $i) (i32.const 1)))
                        (br $lp)))
                (local.get $acc))

            (func (export "writeAndCallback") (param $offset i32) (param $val i32) (result i32)
                (i32.store (local.get $offset) (local.get $val))
                (call $inspect (local.get $offset)))

            (func (export "growInsideWasm") (param $delta i32) (result i32)
                (memory.grow (local.get $delta)))

            (func (export "oobLoad") (result i32)
                (i32.load (i32.const 9999999)))
        )
        "#,
    );

    let out = eval_str(
        &mut vm,
        r#"
        const mem = new WebAssembly.Memory({ initial: 1, maximum: 3 });
        const initSameIdentity = (mem.buffer === mem.buffer);
        const initLen = mem.buffer.byteLength;

        const inst = new WebAssembly.Instance(new WebAssembly.Module(MEM_WASM), {
            env: {
                mem,
                inspectFromJs: (off) => {
                    const view = new Int32Array(mem.buffer);
                    const seen = view[off >> 2];
                    // Mutate linear memory from JS import; Wasm/JS must observe it!
                    view[(off >> 2) + 1] = seen * 3;
                    return seen + 1;
                }
            }
        });

        // Write array from JS Int32Array -> sum inside Wasm!
        const i32 = new Int32Array(mem.buffer);
        i32[0] = 10; i32[1] = 20; i32[2] = 30; i32[3] = 40;
        const sum = inst.exports.sumI32(0, 4);

        // Wasm writes at offset 64 -> JS import reads & writes at offset 68 -> JS reads offset 68!
        const cbRet = inst.exports.writeAndCallback(64, 77);
        const writtenByImport = new Int32Array(mem.buffer)[17];

        // JS memory.grow(1): old ArrayBuffer detaches to 0, new buffer is 131072 bytes & preserves data!
        const oldBuf1 = mem.buffer;
        const prevPages1 = mem.grow(1);
        const detached1 = oldBuf1.byteLength;
        const lenAfterJsGrow = mem.buffer.byteLength;
        const preserved1 = new Int32Array(mem.buffer)[0];

        // Wasm internal memory.grow(1): detaches old buffer & exposes 196608 bytes!
        const oldBuf2 = mem.buffer;
        const prevPages2 = inst.exports.growInsideWasm(1);
        const detached2 = oldBuf2.byteLength;
        const lenAfterWasmGrow = mem.buffer.byteLength;
        const preserved2 = new Int32Array(mem.buffer)[17];

        // Exceeding maximum=3 must throw RangeError!
        let maxErr = false;
        try { mem.grow(1); } catch (e) { maxErr = (e instanceof RangeError); }

        // Out-of-bounds Wasm load must throw RuntimeError!
        let oobTrap = false;
        try { inst.exports.oobLoad(); } catch (e) { oobTrap = (e instanceof WebAssembly.RuntimeError); }

        [
            initSameIdentity, initLen, sum, cbRet, writtenByImport,
            prevPages1, detached1, lenAfterJsGrow, preserved1,
            prevPages2, detached2, lenAfterWasmGrow, preserved2,
            maxErr, oobTrap
        ].join("|");
        "#,
    );

    assert_eq!(
        out,
        "true|65536|100|78|231|1|0|131072|10|2|0|196608|231|true|true"
    );
}

#[test]
fn table_indirect_calls_and_mutable_immutable_globals() {
    let mut vm = vm_with_wasm();
    bind_wat_bytes(
        &mut vm,
        "TBL_GLB_WASM",
        r#"
        (module
            (import "env" "tbl" (table 2 5 funcref))
            (import "env" "counter" (global $counter (mut i32)))
            (type $fn_i32 (func (param i32) (result i32)))

            (func $double (export "double") (param i32) (result i32)
                (i32.mul (local.get 0) (i32.const 2)))

            (func $triple (export "triple") (param i32) (result i32)
                (i32.mul (local.get 0) (i32.const 3)))

            (func (export "dispatch") (param $idx i32) (param $x i32) (result i32)
                (call_indirect (type $fn_i32) (local.get $x) (local.get $idx)))

            (func (export "bumpCounter") (result i32)
                (global.set $counter (i32.add (global.get $counter) (i32.const 1)))
                (global.get $counter))
        )
        "#,
    );

    let out = eval_str(
        &mut vm,
        r#"
        const tbl = new WebAssembly.Table({ element: "anyfunc", initial: 2, maximum: 5 });
        const counter = new WebAssembly.Global({ value: "i32", mutable: true }, 10);
        const fixed = new WebAssembly.Global({ value: "i64", mutable: false }, 999n);

        let immutThrew = false;
        try { fixed.value = 1000n; } catch (e) { immutThrew = (e instanceof TypeError); }

        const inst = new WebAssembly.Instance(new WebAssembly.Module(TBL_GLB_WASM), {
            env: { tbl, counter }
        });
        tbl.set(0, inst.exports.double);
        tbl.set(1, inst.exports.triple);
        const r0 = inst.exports.dispatch(0, 9);
        const r1 = inst.exports.dispatch(1, 9);
        const fromGet = tbl.get(1)(5);

        const prevLen = tbl.grow(2);
        const newLen = tbl.length;
        const emptySlot = tbl.get(3);

        const c1 = inst.exports.bumpCounter();
        counter.value = 50;
        const c2 = inst.exports.bumpCounter();

        [immutThrew, fixed.valueOf().toString(), r0, r1, fromGet, prevLen, newLen, emptySlot === null, c1, c2].join("|");
        "#,
    );

    assert_eq!(out, "true|999|18|27|15|2|4|true|11|51");
}

#[test]
fn async_compile_instantiate_and_streaming_apis() {
    let mut vm = vm_with_wasm();
    bind_wat_bytes(
        &mut vm,
        "STREAM_WASM",
        r#"
        (module
            (func (export "fib") (param $n i32) (result i32)
                (if (result i32) (i32.le_s (local.get $n) (i32.const 1))
                    (then (local.get $n))
                    (else
                        (i32.add
                            (call 0 (i32.sub (local.get $n) (i32.const 1)))
                            (call 0 (i32.sub (local.get $n) (i32.const 2))))))))
        "#,
    );

    let res_val = vm
        .eval_script(
            r#"
            globalThis.__results = {};

            function makeMockResponse(bytes, ok, mime) {
                return {
                    ok,
                    headers: {
                        get(name) {
                            return name.toLowerCase() === "content-type" ? mime : null;
                        }
                    },
                    arrayBuffer() {
                        return Promise.resolve(bytes.buffer.slice(0));
                    }
                };
            }

            // 1. WebAssembly.compile + WebAssembly.instantiate(Module)
            WebAssembly.compile(STREAM_WASM)
                .then(m => WebAssembly.instantiate(m))
                .then(inst => { __results.fromMod = inst.exports.fib(10); });

            // 2. WebAssembly.instantiate(bytes)
            WebAssembly.instantiate(STREAM_WASM)
                .then(res => { __results.fromBytes = res.instance.exports.fib(12); });

            // 3. WebAssembly.instantiateStreaming(Promise<Response>) with valid application/wasm MIME
            WebAssembly.instantiateStreaming(
                Promise.resolve(makeMockResponse(STREAM_WASM, true, "application/wasm; charset=utf-8"))
            ).then(res => { __results.streaming = res.instance.exports.fib(15); });

            // 4. WebAssembly.compileStreaming rejects on wrong MIME type!
            WebAssembly.compileStreaming(
                makeMockResponse(STREAM_WASM, true, "text/plain")
            ).then(
                () => { __results.badMime = "unexpected-ok"; },
                (err) => { __results.badMime = err.name; }
            );

            // 5. WebAssembly.compileStreaming rejects on non-OK status!
            WebAssembly.compileStreaming(
                makeMockResponse(STREAM_WASM, false, "application/wasm")
            ).then(
                () => { __results.badStatus = "unexpected-ok"; },
                (err) => { __results.badStatus = err.name; }
            );
            "#,
        )
        .expect("eval async wasm");
    let _ = res_val;
    vm.drain_microtasks();

    let summary = eval_str(
        &mut vm,
        r#"[__results.fromMod, __results.fromBytes, __results.streaming, __results.badMime, __results.badStatus].join("|")"#,
    );
    assert_eq!(summary, "55|144|610|TypeError|TypeError");
}

#[test]
fn phase5_wasm2_memory64_shared_exceptions_function_gc_and_reentrancy() {
    let mut vm = vm_with_wasm();

    // Re-entrant JS -> Wasm -> JS -> Wasm module + StackOverflow module.
    bind_wat_bytes(
        &mut vm,
        "REENTRANT_WASM",
        r#"
        (module
            (import "env" "callback" (func $cb (param i32) (result i32)))
            (func (export "outer") (param i32) (result i32)
                (call $cb (i32.add (local.get 0) (i32.const 1))))
            (func (export "inner") (param i32) (result i32)
                (i32.mul (local.get 0) (i32.const 3)))
            (func $inf (export "overflow") (param i64) (result i64)
                (i64.add (i64.const 1) (call $inf (local.get 0)))))
        "#,
    );

    // Wasm GC struct + i31 module.
    bind_wat_bytes(
        &mut vm,
        "GC_WASM",
        r#"
        (module
            (type $box (struct (field i32)))
            (func (export "makeStruct") (result (ref $box))
                (struct.new $box (i32.const 77)))
            (func (export "getI31") (param (ref i31)) (result i32)
                (i31.get_s (local.get 0)))
            (func (export "acceptAny") (param anyref) (result anyref)
                (local.get 0)))
        "#,
    );

    // W3C JS String Builtins + importedStringConstants module.
    bind_wat_bytes(
        &mut vm,
        "JS_STR_WASM",
        r#"
        (module
            (import "'" "hello" (global $hello (ref extern)))
            (import "wasm:js-string" "length" (func $str_len (param externref) (result i32)))
            (import "wasm:js-string" "concat" (func $str_cat (param externref externref) (result (ref extern))))
            (func (export "greet") (param externref) (result (ref extern))
                (call $str_cat (global.get $hello) (local.get 0)))
            (func (export "len") (param externref) (result i32)
                (call $str_len (local.get 0))))
        "#,
    );

    let out = eval_str(
        &mut vm,
        r#"
        // 1. Memory64 + Table64 + SharedMemory fixed/resizable buffers
        const m64 = new WebAssembly.Memory({ initial: 1n, maximum: 4n, address: "i64" });
        const m64Prev = m64.grow(1n);
        const m64Type = m64.type();
        const t64 = new WebAssembly.Table({ initial: 2n, maximum: 5n, element: "funcref", address: "i64" });
        const t64Prev = t64.grow(1n);
        const t64Type = t64.type();

        const sharedMem = new WebAssembly.Memory({ initial: 1, maximum: 3, shared: true });
        const oldSab = sharedMem.buffer;
        sharedMem.grow(1);
        const newSab = sharedMem.buffer;
        const sabFixedOk = oldSab !== newSab && oldSab.byteLength === 65536 && newSab.byteLength === 131072;
        const resSab = sharedMem.toResizableBuffer();
        let badSabGrowThrew = false;
        try { resSab.grow(131072 + 1); } catch (e) { badSabGrowThrew = e instanceof RangeError; }

        // 2. WebAssembly.Tag & WebAssembly.Exception
        const tag = new WebAssembly.Tag({ parameters: ["i32", "i64"] });
        const exn = new WebAssembly.Exception(tag, [42, 900n], { traceStack: true });
        let exnRangeThrew = false;
        try { exn.getArg(tag, -1); } catch (e) { exnRangeThrew = e instanceof RangeError; }
        const exnOk = exn.is(tag) && exn.getArg(tag, 0) === 42 && exn.getArg(tag, 1) === 900n && typeof exn.stack === "string" && exnRangeThrew;

        // 3. WebAssembly.Function + AbstractModuleSource
        const wf = new WebAssembly.Function({ parameters: ["i32", "i32"], results: ["i32"] }, (a, b) => a + b);
        const wfOk = (wf instanceof WebAssembly.Function) && wf(10, 25) === 35 && wf.type().parameters.length === 2;
        const tRef = new WebAssembly.Table({ initial: 1, element: "funcref" });
        tRef.set(0, wf);
        const tableIdentityOk = tRef.get(0) === wf;

        const ams = Object.getPrototypeOf(WebAssembly.Module);
        const mod = new WebAssembly.Module(REENTRANT_WASM);
        const tagGetter = Object.getOwnPropertyDescriptor(ams.prototype, Symbol.toStringTag).get;
        const amsOk = typeof AbstractModuleSource === "undefined" && ams.name === "AbstractModuleSource" && tagGetter.call(mod) === "WebAssembly.Module" && tagGetter.call({}) === undefined;

        // 4. Re-entrant JS -> Wasm -> JS -> Wasm + StackOverflow -> RangeError
        let inst;
        inst = new WebAssembly.Instance(mod, {
            env: {
                callback(x) { return inst.exports.inner(x); }
            }
        });
        const reentrantVal = inst.exports.outer(13); // (13 + 1) * 3 = 42
        let overflowRange = false;
        try { inst.exports.overflow(1n); } catch (e) { overflowRange = e instanceof RangeError; }

        // 5. Wasm GC i31 + exotic non-strict object traps + anyref host roundtrip
        const gcInst = new WebAssembly.Instance(new WebAssembly.Module(GC_WASM));
        const st = gcInst.exports.makeStruct();
        let nonStrictSetThrew = false;
        try { st.foo = 1; } catch (e) { nonStrictSetThrew = e instanceof TypeError; }
        let preventExtThrew = false;
        try { Object.preventExtensions(st); } catch (e) { preventExtThrew = e instanceof TypeError; }
        const hostObj = { k: 1 };
        const gcOk = gcInst.exports.getI31(-42) === -42 && gcInst.exports.acceptAny(hostObj) === hostObj && nonStrictSetThrew && preventExtThrew;

        // 6. JS String Builtins + importedStringConstants
        const strMod = new WebAssembly.Module(JS_STR_WASM, {
            builtins: ["js-string"],
            importedStringConstants: "'"
        });
        const strImportsSuppressed = WebAssembly.Module.imports(strMod).length === 0;
        const strInst = new WebAssembly.Instance(strMod);
        const strOk = strImportsSuppressed && strInst.exports.greet(" world") === "hello world" && strInst.exports.len("rix") === 3;

        [
            m64Prev === 1n && m64Type.address === "i64",
            t64Prev === 2n && t64Type.address === "i64",
            sabFixedOk && badSabGrowThrew,
            exnOk,
            wfOk && tableIdentityOk,
            amsOk,
            reentrantVal === 42 && overflowRange,
            gcOk,
            strOk,
        ].join("|");
        "#,
    );

    assert_eq!(out, "true|true|true|true|true|true|true|true|true");
}
