//! Spec regression tests for Phase 4 Part 7B:
//! - ES2025 Promise (NewPromiseCapability, SpeciesConstructor, Promise.try, Promise.allKeyed,
//!   Promise.allSettledKeyed, lazy IteratorClose)
//! - ES2024 Array.fromAsync
//! - Iterator chunking/windowing/join/includes and Joint Iteration (zip / zipKeyed)
//! - Annex B object literal `{ __proto__: proto }`

use rix_js::JsVm;

#[test]
fn promise_try_keyed_combinators_and_iterator_close() {
    let mut vm = JsVm::new();
    vm.eval_script(
        r#"
        globalThis.out = {};
        let closed = false;
        const iterable = {
            [Symbol.iterator]() {
                return {
                    next() { return { value: 1, done: false }; },
                    return() { closed = true; return { done: true }; }
                };
            }
        };
        const BadPromise = class extends Promise {
            static resolve() { throw new Error("boom"); }
        };
        BadPromise.all(iterable).catch(e => { globalThis.out.closed = closed && e.message === "boom"; });
        Promise.try((a, b) => a + b, 20, 22).then(v => { globalThis.out.tryOk = v === 42; });
        Promise.try(() => { throw new RangeError("sync"); }).catch(e => { globalThis.out.tryErr = e instanceof RangeError; });
        Promise.allKeyed({ a: Promise.resolve(1), b: 2 }).then(r => {
            globalThis.out.allKeyed = Object.getPrototypeOf(r) === null && r.a === 1 && r.b === 2;
        });
        Promise.allSettledKeyed({ ok: Promise.resolve("hi"), no: Promise.reject("err") }).then(r => {
            globalThis.out.settledKeyed = Object.getPrototypeOf(r) === null
                && r.ok.status === "fulfilled" && r.ok.value === "hi"
                && r.no.status === "rejected" && r.no.reason === "err";
        });
        "#,
    )
    .unwrap();
    vm.drain_microtasks();
    let ok = vm
        .eval_script("out.closed && out.tryOk && out.tryErr && out.allKeyed && out.settledKeyed && Promise[Symbol.species] === Promise")
        .unwrap();
    assert_eq!(ok.as_bool(), Some(true));
}

#[test]
fn array_from_async_and_iterator_proposals_and_proto_literal() {
    let mut vm = JsVm::new();
    vm.eval_script(
        r#"
        async function* gen() { yield 1; yield Promise.resolve(2); yield 3; }
        Array.fromAsync(gen(), async (x, i) => x * 10 + i).then(a => {
            globalThis.fa1 = JSON.stringify(a) === "[10,21,32]";
        });
        Array.fromAsync({ length: 2, 0: Promise.resolve("a"), 1: "b" }).then(a => {
            globalThis.fa2 = JSON.stringify(a) === '["a","b"]';
        });

        const chunks = Iterator.from([1, 2, 3, 4, 5]).chunks(2).toArray();
        const wins = Iterator.from([1, 2, 3, 4]).windows(3).toArray();
        const winsPart = Iterator.from([1, 2]).windows(5, "allow-partial").toArray();
        const joined = Iterator.from(["a", null, "b"]).join("-");
        const inc = Iterator.from([1, NaN, 3]).includes(NaN) && !Iterator.from([1, 2, 3]).includes(1, 1);
        const zipped = Iterator.zip([[1, 2], ["a", "b", "c"]], { mode: "longest", padding: [0] }).toArray();
        const zkeyed = Iterator.zipKeyed({ x: [1, 2], y: [3, 4] }).toArray();
        const protoObj = { __proto__: Iterator.prototype, next() { return { done: true }; } };

        globalThis.iterOk = JSON.stringify(chunks) === "[[1,2],[3,4],[5]]"
            && JSON.stringify(wins) === "[[1,2,3],[2,3,4]]"
            && JSON.stringify(winsPart) === "[[1,2]]"
            && joined === "a--b"
            && inc
            && JSON.stringify(zipped) === '[[1,"a"],[2,"b"],[0,"c"]]'
            && Object.getPrototypeOf(zkeyed[0]) === null
            && zkeyed[0].x === 1 && zkeyed[1].y === 4
            && Object.getPrototypeOf(protoObj) === Iterator.prototype;
        "#,
    )
    .unwrap();
    vm.drain_microtasks();
    let ok = vm.eval_script("fa1 && fa2 && iterOk").unwrap();
    assert_eq!(ok.as_bool(), Some(true));
}
