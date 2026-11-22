//! Integration tests for ES2024 Promises, microtasks, `async`/`await`,
//! `function*` generators (`yield` / `yield*`), cyclic ES Modules,
//! WHATWG Import Maps, dynamic `import()`, and top-level `await` DAG ordering.

use std::cell::RefCell;
use std::rc::Rc;

use rix_js::{ImportMap, JsVm, MemoryModuleProvider, ModuleLoader, ObjectKind, PromiseState};

#[test]
fn promises_microtask_ordering_and_combinators() {
    let mut vm = JsVm::new();
    vm.eval_script(
        r#"
        var log = [];
        log.push("sync1");
        queueMicrotask(() => log.push("qm1"));
        Promise.resolve(10)
            .then(x => {
                log.push("p1:" + x);
                queueMicrotask(() => log.push("qm_nested"));
                return x * 2;
            })
            .then(x => log.push("p2:" + x));
        log.push("sync2");

        var combResult = "";
        Promise.all([Promise.resolve(1), 2, Promise.resolve(3)]).then(arr => {
            combResult += "all:" + arr.join(",");
        });
        Promise.race([Promise.resolve("fast"), new Promise(() => {})]).then(v => {
            combResult += "|race:" + v;
        });
        Promise.allSettled([Promise.resolve("ok"), Promise.reject("bad")]).then(list => {
            combResult += "|settled:" + list[0].status + ":" + list[0].value + ":" + list[1].status + ":" + list[1].reason;
        });
        Promise.any([Promise.reject("e1"), Promise.resolve("winner")]).then(v => {
            combResult += "|any:" + v;
        });
        var { promise: wrP, resolve: wrRes } = Promise.withResolvers();
        wrP.then(v => { combResult += "|wr:" + v; });
        wrRes(77);
        "#,
    )
    .expect("promise setup");

    vm.drain_microtasks();

    let out = vm
        .eval_script("log.join('|') + '||' + combResult")
        .expect("check results");
    assert_eq!(
        vm.value_to_rust_string(out),
        "sync1|sync2|qm1|p1:10|qm_nested|p2:20|||wr:77all:1,2,3|race:fast|settled:fulfilled:ok:rejected:bad|any:winner"
    );
}

#[test]
fn async_await_try_catch_and_generators_with_yield_star() {
    let mut vm = JsVm::new();
    vm.eval_script(
        r#"
        var asyncOut = "";
        async function compute(base) {
            let acc = base;
            const inc = () => acc;
            const a = await Promise.resolve(5);
            acc += a;
            let caught = "";
            try {
                await Promise.reject("boom");
            } catch (e) {
                caught = e;
            }
            const b = await 7;
            acc += b;
            return inc() + ":" + caught;
        }
        compute(10).then(res => { asyncOut = res; });

        function* inner() {
            yield "x";
            yield "y";
            return "inner_done";
        }
        function* outer() {
            const first = yield 1;
            const delegated = yield* inner();
            yield first + ":" + delegated;
            return "fin";
        }
        const g = outer();
        const r1 = g.next();
        const r2 = g.next(42);
        const r3 = g.next();
        const r4 = g.next();
        const r5 = g.next();
        var genOut = [r1.value, r2.value, r3.value, r4.value, r5.value, r5.done].join("|");

        function* range(n) {
            for (let i = 0; i < n; i++) yield i * 2;
        }
        var spreadGen = [...range(4)].join(",");
        "#,
    )
    .expect("async & generator script");

    vm.drain_microtasks();

    let out = vm
        .eval_script("asyncOut + '||' + genOut + '||' + spreadGen")
        .expect("check async + gen");
    assert_eq!(
        vm.value_to_rust_string(out),
        "22:boom||1|x|y|42:inner_done|fin|true||0,2,4,6"
    );
}

#[test]
fn es_modules_cycles_import_maps_dynamic_import_and_top_level_await() {
    let mut vm = JsVm::new();
    vm.eval_script("var order = [];").expect("init log");

    let import_map_json = r#"{
        "imports": {
            "pkg/": "https://app.test/vendor/pkg/",
            "lazy-mod": "https://app.test/dynamic/lazy.js"
        },
        "scopes": {
            "https://app.test/scoped/": {
                "pkg/math.js": "https://app.test/scoped/fast-math.js"
            }
        }
    }"#;
    let import_map =
        ImportMap::from_json(import_map_json, "https://app.test/index.html").expect("import map");

    let mut provider = MemoryModuleProvider::new();
    // 1. Top-level await leaf module.
    provider.insert(
        "https://app.test/vendor/pkg/async-leaf.js",
        r#"
        order.push("leaf:start");
        const val = await Promise.resolve(21);
        order.push("leaf:end");
        export const magic = val * 2;
        "#,
    );
    // 2. Cyclic pair: a.js <-> b.js with live mutable export + hoisted exported function.
    provider.insert(
        "https://app.test/a.js",
        r#"
        import { callA, count, bump } from "./b.js";
        export function greet() { return "hello-from-a"; }
        order.push("a:before:" + count);
        bump();
        order.push("a:after:" + count + ":" + callA());
        export const aSummary = count;
        "#,
    );
    provider.insert(
        "https://app.test/b.js",
        r#"
        import { greet } from "./a.js";
        export let count = 10;
        export function bump() { count = count + 5; }
        export function callA() { return greet(); }
        order.push("b:init:" + greet());
        "#,
    );
    // 3. Re-export barrel (`export * from`).
    provider.insert(
        "https://app.test/barrel.js",
        r#"
        export * from "pkg/async-leaf.js";
        export const extra = 7;
        "#,
    );
    // 4. Scoped import map override module.
    provider.insert(
        "https://app.test/scoped/fast-math.js",
        "export const factor = 100;",
    );
    provider.insert(
        "https://app.test/scoped/consumer.js",
        r#"
        import { factor } from "pkg/math.js";
        export const scaled = factor * 3;
        "#,
    );
    // 5. Dynamic import target.
    provider.insert(
        "https://app.test/dynamic/lazy.js",
        r#"
        export default "lazy-loaded";
        export const version = 2;
        "#,
    );
    // 6. Entry module combining static TLA dep, cycle, namespace, barrel, scoped map & dynamic import().
    provider.insert(
        "https://app.test/main.js",
        r#"
        import * as barrel from "./barrel.js";
        import { aSummary } from "./a.js";
        import { scaled } from "./scoped/consumer.js";
        order.push("main:magic:" + barrel.magic);
        const dyn = await import("lazy-mod");
        order.push("main:dyn:" + dyn.default + ":" + dyn.version);
        export const finalResult = [barrel.magic, barrel.extra, aSummary, scaled, dyn.default].join(",");
        "#,
    );

    let loader = Rc::new(RefCell::new(ModuleLoader::new(
        Box::new(provider),
        import_map,
    )));
    ModuleLoader::install_on_vm(&loader, &mut vm);

    let ns_promise = ModuleLoader::load_and_evaluate(&loader, &mut vm, "https://app.test/main.js")
        .expect("load entry module");
    vm.drain_microtasks();

    // Verify promise fulfilled with namespace object.
    let ns_val = match vm.heap.get_object(ns_promise).map(|o| &o.kind) {
        Some(ObjectKind::Promise(p)) => match p.state {
            PromiseState::Fulfilled(v) => v,
            PromiseState::Rejected(r) => {
                panic!("Module evaluation rejected: {}", vm.value_to_rust_string(r))
            }
            PromiseState::Pending => panic!("Module evaluation promise remained pending"),
        },
        _ => panic!("Expected Promise"),
    };
    let ns_h = ns_val.as_object().expect("namespace object");
    let k_final = vm.heap.intern_key("finalResult");
    let final_val = vm
        .get_property_by_key(ns_h, k_final)
        .expect("get finalResult");
    assert_eq!(
        vm.value_to_rust_string(final_val),
        "42,7,15,300,lazy-loaded"
    );

    let order_val = vm.eval_script("order.join('|')").expect("order log");
    let order_str = vm.value_to_rust_string(order_val);
    // Verify top-level await in `async-leaf.js` completed BEFORE `main.js` ran!
    let leaf_end_pos = order_str.find("leaf:end").expect("leaf:end present");
    let main_pos = order_str
        .find("main:magic:42")
        .expect("main:magic:42 present");
    assert!(
        leaf_end_pos < main_pos,
        "top-level await dependency must settle before parent module executes: {order_str}"
    );
    assert!(order_str.contains("b:init:hello-from-a"), "{order_str}");
    assert!(order_str.contains("a:before:10"), "{order_str}");
    assert!(order_str.contains("a:after:15:hello-from-a"), "{order_str}");
    assert!(order_str.contains("main:dyn:lazy-loaded:2"), "{order_str}");
}
