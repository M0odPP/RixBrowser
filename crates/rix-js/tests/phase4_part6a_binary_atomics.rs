//! Phase 4 Part 6A ECMAScript conformance regression tests:
//! Resizable/Transferable/Immutable `ArrayBuffer`, multithreaded `SharedArrayBuffer`
//! with `grow`, complete 11-type `DataView` (incl. `BigInt64` & `Float16`),
//! `%TypedArray%` species & auto-length tracking, and pure-Rust `Atomics`
//! (`load`/`store`/`add`/`sub`/`and`/`or`/`xor`/`exchange`/`compareExchange`/`isLockFree`/`wait`/`waitAsync`/`notify`/`pause`).

use rix_js::JsVm;

fn eval_str(src: &str) -> String {
    let mut vm = JsVm::new();
    let v = vm.eval_script(src).expect("script must succeed");
    vm.drain_microtasks();
    vm.value_to_rust_string(v)
}

#[test]
fn resizable_transferable_immutable_ab_and_dataview_f16_bigint64() {
    let out = eval_str(
        r#"
        const rab = new ArrayBuffer(4, { maxByteLength: 16 });
        const dv = new DataView(rab);
        const ta = new Uint16Array(rab);
        dv.setUint16(0, 0x1234, true);
        const lenBefore = ta.length;
        rab.resize(8);
        const lenAfter = ta.length;
        dv.setFloat16(2, 1.5, true);
        const f16 = dv.getFloat16(2, true);

        const sab = new SharedArrayBuffer(8, { maxByteLength: 16 });
        const sdv = new DataView(sab);
        sdv.setBigInt64(0, -123456789012345n, true);
        const bi = sdv.getBigInt64(0, true);
        sab.grow(16);

        const moved = rab.transfer(10);
        const imm = moved.transferToImmutable();
        let immWriteThrew = false;
        try { new DataView(imm).setUint8(0, 1); } catch (e) { immWriteThrew = e instanceof TypeError; }

        [
            rab.resizable, rab.detached, lenBefore, lenAfter, f16,
            bi.toString(), sab.growable, sab.byteLength, sab.maxByteLength,
            moved.detached, imm.immutable, imm.byteLength, immWriteThrew,
            Object.prototype.toString.call(sab), Object.prototype.toString.call(ta),
            Uint8Array.from === Int32Array.from,
            TypedArray_toString_eq = (Uint8Array.prototype.toString === Array.prototype.toString),
        ].join("|");
        "#,
    );
    assert_eq!(
        out,
        "true|true|2|4|1.5|-123456789012345|true|16|16|true|true|10|true|[object SharedArrayBuffer]|[object Uint16Array]|true|true"
    );
}

#[test]
fn atomics_rmw_wait_wait_async_notify_and_pause() {
    let mut vm = JsVm::new();
    vm.eval_script(
        r#"
        const sab = new SharedArrayBuffer(16);
        const i32 = new Int32Array(sab);
        const bi64 = new BigInt64Array(sab);
        Atomics.store(i32, 0, 10);
        const oldAdd = Atomics.add(i32, 0, 5);
        const oldSub = Atomics.sub(i32, 0, 3);
        const oldXor = Atomics.xor(i32, 0, 0xFF);
        const oldCas = Atomics.compareExchange(i32, 0, 12 ^ 0xFF, 42);
        const loaded = Atomics.load(i32, 0);
        const notEq = Atomics.wait(i32, 0, 99, 0);
        const timedOut = Atomics.wait(i32, 0, 42, 0);
        Atomics.pause(2);

        Atomics.store(bi64, 1, 100n);
        const biOld = Atomics.add(bi64, 1, 23n);
        const biCur = Atomics.load(bi64, 1);

        var asyncOut = "";
        const wa = Atomics.waitAsync(i32, 0, 42);
        wa.value.then(v => { asyncOut = v; });
        const woken = Atomics.notify(i32, 0, 1);

        var syncOut = [
            oldAdd, oldSub, oldXor, oldCas, loaded,
            notEq, timedOut,
            Atomics.isLockFree(4), Atomics.isLockFree(3),
            biOld.toString(), biCur.toString(),
            wa.async, woken,
            Object.prototype.toString.call(Atomics),
        ].join("|");
        "#,
    )
    .expect("atomics test script");
    vm.drain_microtasks();
    let out = vm.eval_script("syncOut + '||' + asyncOut").unwrap();
    assert_eq!(
        vm.value_to_rust_string(out),
        "10|15|12|243|42|not-equal|timed-out|true|false|100|123|true|1|[object Atomics]||ok"
    );
}
