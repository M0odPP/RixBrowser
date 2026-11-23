//! Spec regression tests for Phase 4 Part 7D:
//! ECMA-262 §10.5 Proxy Object Internal Methods & §28.1 Reflect.

use rix_js::{JsValue, JsVm};

fn eval(src: &str) -> JsValue {
    let mut vm = JsVm::new();
    vm.eval_script(src)
        .unwrap_or_else(|e| panic!("eval failed: {e:?}\nsource:\n{src}"))
}

fn eval_bool(src: &str) -> bool {
    eval(src).as_bool().expect("expected boolean result")
}

#[test]
fn proxy_constructor_and_revocable_semantics() {
    assert!(eval_bool(
        r#"
        let threw = false;
        try { Proxy({}, {}); } catch (e) { threw = e instanceof TypeError; }
        const { proxy, revoke } = Proxy.revocable({ a: 1 }, {});
        const before = proxy.a === 1;
        const nameOk = revoke.name === "" && revoke.length === 0;
        revoke();
        revoke(); // idempotent
        let revokedThrew = false;
        try { proxy.a; } catch (e) { revokedThrew = e instanceof TypeError; }
        let isArrThrew = false;
        try { Array.isArray(proxy); } catch (e) { isArrThrew = e instanceof TypeError; }
        let toStrThrew = false;
        try { Object.prototype.toString.call(proxy); } catch (e) { toStrThrew = e instanceof TypeError; }
        threw && before && nameOk && revokedThrew && isArrThrew && toStrThrew && Proxy.prototype === undefined;
        "#
    ));
}

#[test]
fn proxy_all_13_traps_and_symbol_keys() {
    assert!(eval_bool(
        r#"
        const sym = Symbol("k");
        const log = [];
        function Target(x) { this.x = x; }
        Target.prototype.p = 9;
        const target = Object.assign(Target, { a: 1, [sym]: 2 });
        const handler = {
            getPrototypeOf(t) { log.push("getPrototypeOf"); return Reflect.getPrototypeOf(t); },
            setPrototypeOf(t, p) { log.push("setPrototypeOf"); return Reflect.setPrototypeOf(t, p); },
            isExtensible(t) { log.push("isExtensible"); return Reflect.isExtensible(t); },
            preventExtensions(t) { log.push("preventExtensions"); return Reflect.preventExtensions(t); },
            getOwnPropertyDescriptor(t, k) { log.push(["gopd", k]); return Reflect.getOwnPropertyDescriptor(t, k); },
            defineProperty(t, k, d) { log.push(["define", k]); return Reflect.defineProperty(t, k, d); },
            has(t, k) { log.push(["has", k]); return Reflect.has(t, k); },
            get(t, k, r) { log.push(["get", k]); return Reflect.get(t, k, r); },
            set(t, k, v, r) { log.push(["set", k]); return Reflect.set(t, k, v, r); },
            deleteProperty(t, k) { log.push(["del", k]); return Reflect.deleteProperty(t, k); },
            ownKeys(t) { log.push("ownKeys"); return Reflect.ownKeys(t); },
            apply(t, thisArg, args) { log.push("apply"); return args[0] * 2; },
            construct(t, args, nt) { log.push("construct"); return Reflect.construct(t, args, nt); },
        };
        const p = new Proxy(target, handler);
        const r1 = Object.getPrototypeOf(p) === Function.prototype;
        const r2 = Reflect.setPrototypeOf(p, Function.prototype);
        const r3 = Object.isExtensible(p) === true;
        const r4 = Object.getOwnPropertyDescriptor(p, sym).value === 2;
        const r5 = Reflect.defineProperty(p, "b", { value: 3, configurable: true, enumerable: true, writable: true });
        const r6 = (sym in p) && (p[sym] === 2);
        p[sym] = 20;
        const r7 = target[sym] === 20;
        const r8 = delete p.b;
        const r9 = Reflect.ownKeys(p).includes(sym);
        const r10 = p(21) === 42;
        const inst = new p(7);
        const r11 = inst.x === 7;
        const r12 = Object.preventExtensions(p) === p && !Object.isExtensible(p);
        r1 && r2 && r3 && r4 && r5 && r6 && r7 && r8 && r9 && r10 && r11 && r12
            && log.some(e => Array.isArray(e) && e[0] === "get" && e[1] === sym)
            && log.some(e => Array.isArray(e) && e[0] === "set" && e[1] === sym)
            && log.some(e => Array.isArray(e) && e[0] === "has" && e[1] === sym);
        "#
    ));
}

#[test]
fn proxy_invariant_enforcement() {
    assert!(eval_bool(
        r#"
        function throwsTypeError(fn) {
            try { fn(); return false; } catch (e) { return e instanceof TypeError; }
        }
        const t1 = {};
        Object.defineProperty(t1, "x", { value: 1, writable: false, configurable: false });
        const p1 = new Proxy(t1, {
            get() { return 2; },
            set() { return true; },
            has() { return false; },
            deleteProperty() { return true; },
            getOwnPropertyDescriptor() { return undefined; },
            ownKeys() { return []; },
            isExtensible() { return false; },
            preventExtensions() { return true; },
        });
        const checks = [
            throwsTypeError(() => p1.x),
            throwsTypeError(() => { p1.x = 2; }),
            throwsTypeError(() => ("x" in p1)),
            throwsTypeError(() => { delete p1.x; }),
            throwsTypeError(() => Object.getOwnPropertyDescriptor(p1, "x")),
            throwsTypeError(() => Reflect.ownKeys(p1)),
            throwsTypeError(() => Object.isExtensible(p1)),
            throwsTypeError(() => Object.preventExtensions(p1)),
        ];
        const t2 = Object.preventExtensions({ a: 1 });
        const p2 = new Proxy(t2, {
            getPrototypeOf() { return {}; },
            setPrototypeOf() { return true; },
            defineProperty() { return true; },
            ownKeys() { return ["a", "extra"]; },
        });
        checks.push(
            throwsTypeError(() => Object.getPrototypeOf(p2)),
            throwsTypeError(() => Reflect.setPrototypeOf(p2, {})),
            throwsTypeError(() => Reflect.defineProperty(p2, "b", { value: 1 })),
            throwsTypeError(() => Reflect.ownKeys(p2)),
        );
        checks.every(Boolean);
        "#
    ));
}

#[test]
fn proxy_prototype_chain_with_and_reflect_receiver() {
    let mut vm = JsVm::new();
    let res = vm.eval_script(
        r#"
        let seenReceiver = null;
        let seenKey = null;
        const p = new Proxy({}, {
            get(t, k, r) { seenReceiver = r; seenKey = k; return 99; },
            set(t, k, v, r) { seenReceiver = r; seenKey = k; return true; },
            has(t, k) { return k === "hit"; },
        });
        const child = Object.create(p);
        const g = child.abc === 99 && seenReceiver === child && seenKey === "abc";
        seenReceiver = null;
        const arr = new Array(1);
        Object.setPrototypeOf(arr, p);
        arr[0] = 5;
        const s = seenReceiver === arr && seenKey === "0";
        const h = ("hit" in child) && !("miss" in child);

        // Reflect.set receiver forwarding defines on receiver, not target
        const target = {};
        const rec = {};
        const okSet = Reflect.set(target, "k", 123, rec);
        const recOk = okSet && target.k === undefined && rec.k === 123;

        // Reflect.apply rejects null argList & Reflect has toStringTag
        let applyNullThrew = false;
        try { Reflect.apply(() => 1, null, null); } catch (e) { applyNullThrew = e instanceof TypeError; }
        const tagOk = Object.prototype.toString.call(Reflect) === "[object Reflect]";

        // Object.freeze / isFrozen through Proxy traps
        const ft = { a: 1 };
        const fp = new Proxy(ft, {});
        Object.freeze(fp);
        const frozenOk = Object.isFrozen(fp) && Object.isSealed(fp) && Object.isFrozen(ft);

        // `with (proxy)` invokes `has` trap without recursing
        const withSeen = [];
        const wp = new Proxy({ val: 77 }, {
            has(t, k) { withSeen.push(k); return k in t; }
        });
        let got = 0;
        with (wp) { got = val; }

        JSON.stringify({ g, s, h, recOk, applyNullThrew, tagOk, frozenOk, got, sawVal: withSeen.includes("val") });
        "#,
    ).unwrap();
    let json = vm
        .heap
        .materialize_string(res.as_string().unwrap())
        .unwrap();
    assert_eq!(
        json,
        r#"{"g":true,"s":true,"h":true,"recOk":true,"applyNullThrew":true,"tagOk":true,"frozenOk":true,"got":77,"sawVal":true}"#
    );
}
