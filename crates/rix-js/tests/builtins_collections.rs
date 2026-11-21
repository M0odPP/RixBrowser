//! Integration tests for ES2024 Collections (`Map`, `Set`, `WeakMap`, `WeakSet`,
//! `WeakRef`) and Binary Data (`ArrayBuffer`, `DataView`, all 9 `TypedArray`s).

use rix_js::JsVm;

#[test]
fn map_and_set_including_es2024_methods_and_iteration() {
    let mut vm = JsVm::new();
    let res = vm
        .eval_script(
            r#"
            const m = new Map([["a", 1], ["b", 2]]);
            m.set(NaN, 42).set(-0, 99);
            const hasNan = m.has(NaN) && m.get(NaN) === 42;
            const zeroNorm = m.get(0) === 99;
            const keys = Array.from(m.keys()).join(",");
            let sum = 0;
            for (const [k, v] of m) {
                sum += v;
            }
            const grouped = Map.groupBy([1, 2, 3, 4], x => x % 2 === 0 ? "even" : "odd");
            const evenLen = grouped.get("even").length;

            const s1 = new Set([1, 2, 3, 2, NaN, NaN]);
            const s2 = new Set([3, 4, 5]);
            const u = Array.from(s1.union(s2)).length;
            const inter = Array.from(s1.intersection(s2)).join(",");
            const diff = Array.from(s1.difference(s2)).length;
            const sub = new Set([1, 2]).isSubsetOf(s1);
            const disj = new Set([10, 20]).isDisjointFrom(s1);

            const custom = {
                [Symbol.iterator]() {
                    let i = 0;
                    return {
                        next() {
                            return i < 3 ? { value: ++i * 10, done: false } : { value: undefined, done: true };
                        }
                    };
                }
            };
            const customSpread = [...custom].join(",");

            [hasNan, zeroNorm, m.size, sum, evenLen, s1.size, u, inter, diff, sub, disj, customSpread, keys.startsWith("a,b")].join("|");
            "#,
        )
        .expect("map/set eval");
    assert_eq!(
        vm.value_to_rust_string(res),
        "true|true|4|144|2|4|6|3|3|true|true|10,20,30|true"
    );
}

#[test]
fn weak_map_weak_set_and_weak_ref_reclaim_on_gc() {
    let mut vm = JsVm::new();
    vm.eval_script(
        r#"
        var liveKey = { id: "live" };
        var tempKey = { id: "temp" };
        var wm = new WeakMap();
        var ws = new WeakSet();
        var liveVal = { payload: 777 };
        var tempVal = { payload: 999 };
        wm.set(liveKey, liveVal);
        wm.set(tempKey, tempVal);
        ws.add(liveKey);
        ws.add(tempKey);
        var liveRef = new WeakRef(liveKey);
        var tempRef = new WeakRef(tempKey);
        var tempValRef = new WeakRef(tempVal);
        // Drop strong references to tempKey and tempVal while keeping wm/ws/refs rooted.
        tempKey = null;
        tempVal = null;
        liveVal = null;
        "#,
    )
    .expect("weak setup");

    // Before GC, tempRef still points to allocated object; run full Major GC.
    let stats = vm.collect_garbage();
    assert!(stats.reclaimed > 0, "expected dead objects to be reclaimed");

    let check = vm
        .eval_script(
            r#"
            const liveAlive = liveRef.deref() === liveKey;
            const tempDead = tempRef.deref() === undefined;
            const tempValDead = tempValRef.deref() === undefined;
            const ephemSurvived = wm.get(liveKey).payload === 777;
            const wsHasLive = ws.has(liveKey);
            [liveAlive, tempDead, tempValDead, ephemSurvived, wsHasLive].join("|");
            "#,
        )
        .expect("weak post-gc check");
    assert_eq!(vm.value_to_rust_string(check), "true|true|true|true|true");
}

#[test]
fn array_buffer_data_view_and_all_typed_arrays() {
    let mut vm = JsVm::new();
    let res = vm
        .eval_script(
            r#"
            const buf = new ArrayBuffer(16);
            const dv = new DataView(buf);
            dv.setUint32(0, 0x12345678, false);
            dv.setUint32(4, 0x12345678, true);
            dv.setFloat64(8, 3.5, true);
            const beOk = dv.getUint8(0) === 0x12 && dv.getUint8(3) === 0x78;
            const leOk = dv.getUint8(4) === 0x78 && dv.getUint8(7) === 0x12;
            const f64Ok = dv.getFloat64(8, true) === 3.5;
            const isViewOk = ArrayBuffer.isView(dv) && !ArrayBuffer.isView(buf);

            // Shared buffer aliasing between Uint8Array and subarray + Int16Array.
            const u8 = new Uint8Array(buf, 0, 8);
            const sub = u8.subarray(2, 6);
            sub[0] = 0xAA;
            const aliasOk = u8[2] === 0xAA;

            // Clamped rounding ties-to-even + negative/overflow clamping.
            const clamped = Uint8ClampedArray.of(-5, 0.5, 1.5, 2.5, 300);
            const clampedStr = clamped.join(",");

            // Numeric typed arrays + methods + iteration.
            const i32 = Int32Array.from([30, -10, 20]);
            i32.sort();
            const f32 = Float32Array.of(1.5, 2.5, 3.5);
            const sum = f32.reduce((a, b) => a + b, 0);
            const doubled = [...i32.map(x => x * 2)].join(",");

            // Verify all 9 constructors exist and report BYTES_PER_ELEMENT.
            const bpe = [
                Int8Array.BYTES_PER_ELEMENT,
                Uint8Array.BYTES_PER_ELEMENT,
                Uint8ClampedArray.BYTES_PER_ELEMENT,
                Int16Array.BYTES_PER_ELEMENT,
                Uint16Array.BYTES_PER_ELEMENT,
                Int32Array.BYTES_PER_ELEMENT,
                Uint32Array.BYTES_PER_ELEMENT,
                Float32Array.BYTES_PER_ELEMENT,
                Float64Array.BYTES_PER_ELEMENT,
            ].join(",");

            [beOk, leOk, f64Ok, isViewOk, aliasOk, clampedStr, sum, doubled, bpe].join("|");
            "#,
        )
        .expect("binary data eval");
    assert_eq!(
        vm.value_to_rust_string(res),
        "true|true|true|true|true|0,0,2,2,255|7.5|-20,40,60|1,1,1,2,2,4,4,4,8"
    );
}
