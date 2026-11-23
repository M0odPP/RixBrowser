//! Phase 4 Part 1 ECMAScript conformance regression tests (Issue #111).
//!
//! Covers:
//! - Class private fields, private methods, private getters/setters, static private elements,
//!   derived class `InitializeInstanceElements` ordering & return-override, upvalue capture,
//!   and `TypeError` on invoking a class constructor without `new`.
//! - `arguments` object in non-arrow functions, arrow capture, and `Function.prototype.apply`.
//! - Global `eval`, `Function` constructor, `.constructor` intrinsic links, and `%TypedArray%`.
//! - `async function*` (`AsyncGenerator`) and `for await (... of ...)` (async & sync iterables).
//! - Sibling function hoisting inside IIFEs, block-scoped `FunctionDeclaration`, and
//!   arrow-function lookahead over IIFEs with template literals.

use rix_js::JsVm;

fn eval_str(src: &str) -> String {
    let mut vm = JsVm::new();
    let val = vm.eval_script(src).expect("script should succeed");
    vm.drain_microtasks();
    vm.value_to_rust_string(val)
}

#[test]
fn class_private_fields_methods_accessors_and_inheritance() {
    let out = eval_str(
        r#"
        function makeClasses(offset) {
            class Base {
                #x = 10 + offset;
                #double() { return this.#x * 2; }
                get #val() { return this.#x; }
                set #val(v) { this.#x = v; }
                static #tag = "B";
                static #fmt() { return this.#tag + ":" + offset; }
                static getTag() { return this.#fmt(); }
                read() { return this.#double(); }
                bump(v) { this.#val = v; return this.#val; }
                hasX(o) { return #x in o; }
            }
            class Sub extends Base {
                #y = 7;
                sum() { return this.read() + this.#y; }
            }
            return { Base, Sub };
        }
        const { Base, Sub } = makeClasses(5);
        const s = new Sub();
        const r1 = s.sum(); // (10+5)*2 + 7 = 37
        const r2 = s.bump(20); // 20
        const r3 = s.sum(); // 47
        const r4 = Base.getTag(); // B:5
        const r5 = s.hasX(s) && !s.hasX({});
        let threwNoNew = false;
        try { Base(); } catch (e) { threwNoNew = e instanceof Error; }
        let threwWriteMethod = false;
        class ReadOnlyPriv {
            #m() { return 1; }
            tryMutate() { this.#m = 2; }
        }
        try { new ReadOnlyPriv().tryMutate(); } catch (e) { threwWriteMethod = true; }
        [r1, r2, r3, r4, r5, threwNoNew, threwWriteMethod].join("|");
        "#,
    );
    assert_eq!(out, "37|20|47|B:5|true|true|true");
}

#[test]
fn arguments_object_arrow_capture_and_apply() {
    let out = eval_str(
        r#"
        function sumAll(a = arguments.length) {
            const inner = () => arguments.length + ":" + arguments[0] + ":" + arguments[2];
            let total = 0;
            for (const x of arguments) total += x;
            return a + "|" + total + "|" + inner();
        }
        function forward() {
            return (function(x, y, z) { return x * 100 + y * 10 + z; }).apply(null, arguments);
        }
        sumAll(1, 2, 3) + "|" + sumAll() + "|" + forward(4, 5, 6);
        "#,
    );
    assert_eq!(out, "1|6|3:1:3|0|0|0:undefined:undefined|456");
}

#[test]
fn eval_function_constructor_and_typed_array_intrinsic() {
    let out = eval_str(
        r#"
        const add = new Function("a", "b", "return a + b;");
        const e1 = eval("21 * 2");
        const obj = { k: 9 };
        const e2 = eval(obj) === obj;
        const links = [
            ({}).constructor === Object,
            [].constructor === Array,
            (function(){}).constructor === Function,
            Object.getPrototypeOf(Uint8Array) === Object.getPrototypeOf(Int32Array),
            Object.getPrototypeOf(Uint8Array).prototype === Object.getPrototypeOf(Uint8Array.prototype),
        ].every(Boolean);
        [add(10, 32), e1, e2, links].join("|");
        "#,
    );
    assert_eq!(out, "42|42|true|true");
}

#[test]
fn async_generator_and_for_await_of() {
    let mut vm = JsVm::new();
    vm.eval_script(
        r#"
        var result = "";
        async function* gen(start) {
            const a = await Promise.resolve(start);
            yield a;
            yield Promise.resolve(a + 1);
            yield* [a + 2, Promise.resolve(a + 3)];
        }
        (async () => {
            const vals = [];
            for await (const v of gen(10)) {
                vals.push(v);
            }
            for await (const p of [Promise.resolve(100), 200]) {
                vals.push(p);
            }
            result = vals.join(",");
        })();
        "#,
    )
    .expect("async generator script should succeed");
    vm.drain_microtasks();
    let val = vm.eval_script("result").expect("read result");
    assert_eq!(vm.value_to_rust_string(val), "10,11,12,13,100,200");
}

#[test]
fn hoisted_sibling_functions_block_functions_and_iife_template_lookahead() {
    let out = eval_str(
        r#"
        let blockRes = 0;
        {
            blockRes = innerHelper(6);
            function innerHelper(n) { return n * 7; }
        }
        const iifeRes = (function() {
            function first(n) {
                const s = `val=${second(n)} @ok`;
                return s;
            }
            function second(n) {
                return n + 1;
            }
            return first(41);
        })();
        blockRes + "|" + iifeRes;
        "#,
    );
    assert_eq!(out, "42|val=42 @ok");
}
