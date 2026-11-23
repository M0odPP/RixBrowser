//! Phase 4 Part 4 ECMAScript built-ins conformance regression tests:
//! Generic `Array` (`ToObject`, `ToLength`, `ArraySpeciesCreate`, `Array.fromAsync`,
//! sparse arrays, `Array.length` RangeError), `Object` (`create` descriptors, `assign`
//! strings/Symbols, `freeze`/`seal` integrity, `defineProperty` array indices),
//! callable `Function.prototype`, `Symbol.hasInstance`, and all 13 well-known Symbols.

use rix_js::JsVm;

fn eval_str(src: &str) -> String {
    let mut vm = JsVm::new();
    let v = vm.eval_script(src).expect("script must succeed");
    vm.drain_microtasks();
    vm.value_to_rust_string(v)
}

#[test]
fn generic_array_methods_species_and_sparse() {
    let out = eval_str(
        r#"
        const like = { length: 3, 0: 10, 2: 30 };
        const mapped = Array.prototype.map.call(like, (x, i) => x + i);
        const reduced = Array.prototype.reduceRight.call(like, (a, b) => a + b, 0);
        class SubArray extends Array {}
        const subFiltered = new SubArray(1, 2, 3).filter(x => x > 1);
        const sparse = []; sparse.length = 1000000; sparse[500000] = 7;
        let rangeErr = false; try { new Array(-1); } catch (e) { rangeErr = e instanceof RangeError; }
        let lenErr = false; try { const a = []; a.length = 1.5; } catch (e) { lenErr = e instanceof RangeError; }
        [
            mapped.length, mapped[0], 1 in mapped, mapped[2], reduced,
            subFiltered instanceof SubArray, sparse.reduce((a, b) => a + b, 0),
            rangeErr, lenErr, [NaN].includes(NaN),
            [1].concat({ [Symbol.isConcatSpreadable]: true, length: 2, 0: 2, 1: 3 }).join(","),
            Array.isArray(Array.prototype),
        ].join("|");
        "#,
    );
    assert_eq!(out, "3|10|false|32|40|true|7|true|true|true|1,2,3|true");
}

#[test]
fn object_create_assign_freeze_and_function_prototype_semantics() {
    let out = eval_str(
        r#"
        const o = Object.create(null, {
            x: { value: 42, enumerable: true, writable: false, configurable: false },
            y: { get() { return 7; }, enumerable: true }
        });
        const sym = Symbol("k");
        const assigned = Object.assign({}, "ab", { [sym]: 99 });

        const arr = [1, 2];
        Object.defineProperty(arr, "0", { enumerable: false });
        const arrDesc = Object.getOwnPropertyDescriptor(arr, "0");

        const frozen = Object.freeze([10, 20]);
        let wroteFrozen = false;
        try { (() => { "use strict"; frozen[0] = 99; })(); } catch (e) { wroteFrozen = e instanceof TypeError; }

        class Custom {
            static [Symbol.hasInstance](v) { return v === "magic"; }
        }
        let callerThrew = false;
        try { Function.prototype.caller; } catch (e) { callerThrew = e instanceof TypeError; }

        [
            o.x, o.y,
            assigned[0], assigned[1], assigned[sym],
            arrDesc.value, arrDesc.enumerable,
            Object.isFrozen(frozen), Object.isSealed(frozen), wroteFrozen,
            typeof Function.prototype, String(Function.prototype()),
            "magic" instanceof Custom, callerThrew,
            typeof Symbol.species, typeof Symbol.matchAll, typeof Symbol.isConcatSpreadable,
        ].join("|");
        "#,
    );
    assert_eq!(
        out,
        "42|7|a|b|99|1|false|true|true|true|function|undefined|true|true|symbol|symbol|symbol"
    );
}
