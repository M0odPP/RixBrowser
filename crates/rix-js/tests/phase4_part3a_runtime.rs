//! Phase 4 Part 3A ECMAScript conformance regression tests:
//! `ToPrimitive`/`ToNumeric`, BigInt binary/bitwise ops, boxed primitives,
//! synchronous generator parameter init, `IteratorClose`, computed class members,
//! Annex B §B.3.3 block functions, HTML comments, and direct `eval` scoping.

use rix_js::JsVm;

fn eval_str(src: &str) -> String {
    let mut vm = JsVm::new();
    let v = vm.eval_script(src).expect("script must succeed");
    vm.value_to_rust_string(v)
}

#[test]
fn to_primitive_boxed_primitives_and_bigint_binary_bitwise_ops() {
    let out = eval_str(
        r#"
        const custom = {
            [Symbol.toPrimitive](hint) {
                return hint === "string" ? "str" : 21;
            }
        };
        const d = new Date(0);
        const boxedNum = new Number(7);
        const boxedStr = new String("hi");
        const boxedBool = new Boolean(false);
        const boxedBi = Object(12n);

        const biDiv = (100n / 7n).toString();
        const biRem = (-100n % 7n).toString();
        const biPow = (2n ** 16n).toString();
        const biAnd = (0b1101n & 0b1011n).toString();
        const biOr = (0b1100n | 0b0011n).toString();
        const biXor = (-5n ^ 3n).toString();
        const biShl = (3n << 4n).toString();
        const biShr = (-10n >> 2n).toString();

        let divZeroErr = "";
        try { 1n / 0n; } catch (e) { divZeroErr = e.name; }
        let ushrErr = "";
        try { 1n >>> 1n; } catch (e) { ushrErr = e.name; }
        const powInfNan = Number.isNaN((-1) ** Infinity);

        [
            custom + 1,
            `${custom}`,
            typeof (d + 1),
            boxedNum + 3,
            boxedStr + "!",
            boxedBool.valueOf(),
            boxedBi.valueOf().toString(),
            Object.prototype.toString.call(boxedNum),
            biDiv, biRem, biPow, biAnd, biOr, biXor, biShl, biShr,
            divZeroErr, ushrErr, powInfNan,
        ].join("|");
        "#,
    );
    assert_eq!(
        out,
        "22|str|string|10|hi!|false|12|[object Number]|14|-2|65536|9|15|-8|48|-3|RangeError|TypeError|true"
    );
}

#[test]
fn generator_sync_param_init_iterator_close_and_computed_class_methods() {
    let out = eval_str(
        r#"
        let paramThrewSync = false;
        function* g(x = (() => { throw new RangeError("sync"); })()) {
            yield x;
        }
        try {
            g();
        } catch (e) {
            paramThrewSync = e instanceof RangeError;
        }

        let closed = 0;
        const iterable = {
            [Symbol.iterator]() {
                let i = 0;
                return {
                    next() { return { value: ++i, done: false }; },
                    return() { closed++; return { done: true }; }
                };
            }
        };
        const [first] = iterable;
        try {
            for (const x of iterable) {
                if (x === 1) throw new Error("abort");
            }
        } catch (_) {}

        const sym = Symbol("mySym");
        class C {
            [sym]() { return 99; }
            get ["val"]() { return 42; }
        }
        const inst = new C();
        [paramThrewSync, first, closed, inst[sym](), C.prototype[sym].name, inst.val].join("|");
        "#,
    );
    assert_eq!(out, "true|1|2|99|[mySym]|42");
}

#[test]
fn annex_b_block_functions_html_comments_and_direct_eval_scoping() {
    let out = eval_str(
        r#"
        <!-- HTML open comment in script mode
        --> HTML close comment at line start
        var before = typeof annexFn;
        {
            function annexFn() { return "inner"; }
        }
        var after = annexFn();

        const desc = Object.getOwnPropertyDescriptor(globalThis, "annexFn");

        function strictEvalTest() {
            "use strict";
            eval("var leakedStrict = 1;");
            return typeof leakedStrict;
        }

        let paramEvalCollision = false;
        const arrowParam = (p = eval("var p = 1")) => p;
        try { arrowParam(); } catch (e) { paramEvalCollision = e instanceof SyntaxError; }

        let loopCptn = eval("1; for (; false; ) {}");
        [before, after, desc.configurable, strictEvalTest(), paramEvalCollision, String(loopCptn)].join("|");
        "#,
    );
    assert_eq!(out, "undefined|inner|false|undefined|true|undefined");
}
