//! Phase 4 Part 2 ECMAScript conformance regression tests (Issue #111).

use rix_js::JsVm;

fn eval_str(src: &str) -> String {
    let mut vm = JsVm::new();
    let val = vm.eval_script(src).expect("script should succeed");
    vm.drain_microtasks();
    vm.value_to_rust_string(val)
}

#[test]
fn error_subclasses_prototypes_and_prototype_ic_isolation() {
    let out = eval_str(
        r#"
        const ctors = [Error, TypeError, ReferenceError, RangeError, SyntaxError, URIError, EvalError];
        const ok = ctors.every((C) => {
            const e = new C("boom");
            return e instanceof C && e instanceof Error && e.constructor === C && e.name === C.name && e.message === "boom";
        });
        const agg = new AggregateError([1, 2], "multi", { cause: "root" });
        const aggOk = agg instanceof AggregateError && agg instanceof Error && agg.errors.length === 2 && agg.cause === "root";
        ok + "|" + aggOk + "|" + (new TypeError("x").toString());
        "#,
    );
    assert_eq!(out, "true|true|TypeError: x");
}

#[test]
fn function_descriptors_name_inference_and_constructability() {
    let out = eval_str(
        r#"
        const arrow = (a, b = 1, c) => a + b;
        const cls = class { constructor(x, y) {} m() {} };
        const obj = { fn: function() {}, method() {}, get prop() { return 1; } };
        const nonCtors = [arrow, obj.method, cls.prototype.m, Math.max, BigInt, Symbol];
        const allReject = nonCtors.every((f) => {
            try { Reflect.construct(function(){}, [], f); return false; } catch (e) { return e instanceof TypeError; }
        });
        [arrow.name, arrow.length, cls.name, cls.length, obj.fn.name, obj.method.name, allReject].join("|");
        "#,
    );
    assert_eq!(out, "arrow|1|cls|2|fn|method|true");
}

#[test]
fn property_descriptors_strict_mode_and_object_reflect_methods() {
    let out = eval_str(
        r#"
        "use strict";
        const o = {};
        Object.defineProperty(o, "x", { value: 10, writable: false, enumerable: true, configurable: false });
        let threwWrite = false;
        try { o.x = 20; } catch (e) { threwWrite = e instanceof TypeError; }
        let threwDelete = false;
        try { delete o.x; } catch (e) { threwDelete = e instanceof TypeError; }
        const d = Object.getOwnPropertyDescriptor(o, "x");
        Object.preventExtensions(o);
        let threwAdd = false;
        try { o.y = 1; } catch (e) { threwAdd = e instanceof TypeError; }
        const arrDesc = Object.getOwnPropertyDescriptor([7, 8], "length");
        const grouped = Object.groupBy([1, 2, 3, 4], (n) => n % 2 === 0 ? "even" : "odd");
        [threwWrite, threwDelete, threwAdd, d.value, d.writable, Object.isExtensible(o), arrDesc.value, grouped.even.join(",")].join("|");
        "#,
    );
    assert_eq!(out, "true|true|true|10|false|false|2|2,4");
}

#[test]
fn bigint_typed_arrays_shared_array_buffer_and_iterator_helpers() {
    let out = eval_str(
        r#"
        const b64 = new BigInt64Array([1n, -2n, 3n]);
        const bu64 = new BigUint64Array([10n, 20n]);
        const f16 = new Float16Array([1.5, -2.25]);
        const sab = new SharedArrayBuffer(16);
        const sliced = sab.slice(4, 12);
        const iterRes = [1, 2, 3, 4, 5].values().filter((x) => x % 2 === 1).map((x) => x * 10).take(2).toArray();
        const arrMethods = [1, 2, 3].toReversed().with(1, 99).join(",");
        [b64[1], bu64[1], f16[0], f16[1], sliced.byteLength, iterRes.join(","), arrMethods, BigInt.asIntN(8, 255n)].join("|");
        "#,
    );
    assert_eq!(out, "-2|20|1.5|-2.25|8|10,30|3,99,1|-1");
}
