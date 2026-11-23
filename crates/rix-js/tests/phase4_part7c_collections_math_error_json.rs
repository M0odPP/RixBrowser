//! Spec regression tests for Phase 4 Part 7C:
//! - ES2025 Math (f16round, sumPrecise, hyperbolic/log1p/expm1, exact .length, pow/hypot edge cases)
//! - Number (toExponential, toPrecision, toFixed, Number.parseInt/parseFloat identity)
//! - BigInt & Symbol (asIntN/asUintN, toStringTag, [Symbol.toPrimitive])
//! - Error.isError & Error.prototype.stack accessor
//! - JSON.rawJSON / isRawJSON, replacer fn/array, reviver context.source
//! - Set (ES2025 GetSetRecord), Map/WeakMap (getOrInsert/getOrInsertComputed), weak Symbols, FinalizationRegistry

use rix_js::{JsValue, JsVm};

fn eval(src: &str) -> JsValue {
    let mut vm = JsVm::new();
    vm.eval_script(src)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\nsrc:\n{src}"))
}

#[test]
fn math_number_bigint_symbol_es2025() {
    let v = eval(
        r#"
        const sumExact = Math.sumPrecise([1e20, 0.1, -1e20]) === 0.1;
        const f16 = Math.f16round(1.337) === 1.3369140625;
        const powEdge = Number.isNaN(Math.pow(-1, Infinity)) && Number.isNaN(Math.pow(-1, -Infinity));
        const hyp = Math.hypot(3, 4) === 5 && Math.hypot(NaN, Infinity) === Infinity;
        const trig = Math.abs(Math.sinh(0)) === 0 && Math.cosh(0) === 1 && Math.expm1(0) === 0 && Math.log1p(0) === 0;
        const numIds = Number.parseInt === parseInt && Number.parseFloat === parseFloat;
        const numFmt = (123.456).toExponential(2) === "1.23e+2"
            && (123.456).toPrecision(4) === "123.5"
            && (1e21).toFixed(2) === "1e+21";
        const bi = BigInt.asIntN(8, 255n) === -1n && BigInt.asUintN(8, -1n) === 255n
            && BigInt.prototype[Symbol.toStringTag] === "BigInt";
        const sym = Symbol.prototype[Symbol.toStringTag] === "Symbol"
            && Symbol("x")[Symbol.toPrimitive]("default").description === "x";
        sumExact && f16 && powEdge && hyp && trig && numIds && numFmt && bi && sym
            && Math[Symbol.toStringTag] === "Math";
        "#,
    );
    assert_eq!(v.as_bool(), Some(true));
}

#[test]
fn error_is_error_stack_and_json_raw_json() {
    let v = eval(
        r#"
        const e = new TypeError("oops", { cause: 42 });
        const errOk = Error.isError(e)
            && Error.isError(new AggregateError([], "a"))
            && Error.isError(new SuppressedError(1, 2, "s"))
            && !Error.isError(Object.create(Error.prototype))
            && Object.prototype.toString.call(e) === "[object Error]"
            && typeof e.stack === "string"
            && Object.getOwnPropertyDescriptor(Error.prototype, "stack").get.call({}) === undefined;
        const raw = JSON.rawJSON("12345678901234567890");
        const rawOk = JSON.isRawJSON(raw)
            && !JSON.isRawJSON({ rawJSON: "1" })
            && JSON.stringify({ big: raw }) === '{"big":12345678901234567890}'
            && JSON.stringify({ a: 1, b: 2 }, ["b"]) === '{"b":2}';
        let seenSrc = null;
        JSON.parse("42", (k, val, ctx) => { seenSrc = ctx.source; return val; });
        errOk && rawOk && seenSrc === "42" && JSON[Symbol.toStringTag] === "JSON";
        "#,
    );
    assert_eq!(v.as_bool(), Some(true));
}

#[test]
fn set_record_map_upsert_weak_symbols_and_finalization_registry() {
    let v = eval(
        r#"
        const s = new Set([1, 2, 3]);
        const setLike = { size: 2, has: x => x === 2 || x === 4, keys: () => [2, 4].values() };
        const unionOk = JSON.stringify([...s.union(setLike)]) === "[1,2,3,4]";
        const interOk = JSON.stringify([...s.intersection(setLike)]) === "[2]";
        let arrThrew = false;
        try { s.union([1, 2]); } catch (e) { arrThrew = e instanceof TypeError; }

        const m = new Map([["a", 1]]);
        const m1 = m.getOrInsert("a", 99) === 1 && m.getOrInsert("b", 2) === 2;
        const m2 = m.getOrInsertComputed("c", k => k.length + 10) === 11;

        const sym = Symbol("weak");
        const wm = new WeakMap();
        wm.set(sym, "hello");
        const ws = new WeakSet([sym]);
        const wr = new WeakRef(sym);
        const weakOk = wm.get(sym) === "hello" && wm.getOrInsert(sym, "x") === "hello"
            && ws.has(sym) && wr.deref() === sym;

        const fr = new FinalizationRegistry(() => {});
        const tok = {};
        fr.register(sym, "held", tok);
        const frOk = fr.unregister(tok) === true && fr.unregister(tok) === false
            && FinalizationRegistry.prototype[Symbol.toStringTag] === "FinalizationRegistry";

        unionOk && interOk && arrThrew && m1 && m2 && weakOk && frOk;
        "#,
    );
    assert_eq!(v.as_bool(), Some(true));
}
