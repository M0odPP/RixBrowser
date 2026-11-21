use rix_js::{EvalError, JsValue, JsVm};

#[test]
fn closures_open_upvalues_this_binding_and_classes_with_super() {
    let mut vm = JsVm::new();
    let res = vm
        .eval_script(
            r#"
            function makeCounter(start) {
                let n = start;
                const inc = () => ++n;
                const add = function(k) { n += k; return n; };
                return { inc, add, read: () => n };
            }
            const c = makeCounter(10);
            c.inc();
            c.add(5);
            const afterClosures = c.read(); // 16

            class Base {
                static factor = 2;
                constructor(v) {
                    this.v = v;
                }
                score() {
                    return this.v * Base.factor;
                }
            }
            class Derived extends Base {
                #bonus = 3;
                constructor(v) {
                    super(v);
                }
                score() {
                    return super.score() + this.#bonus;
                }
                hasBonus(other) {
                    return #bonus in other;
                }
            }
            const d = new Derived(7);
            const score = d.score(); // 7*2 + 3 = 17
            const inst = (d instanceof Derived) && (d instanceof Base) && d.hasBonus(d);

            // Explicit `this` binding via call / apply / bind
            function scale(a, b) { return this.base + a + b; }
            const bound = scale.bind({ base: 100 }, 5);
            const viaBind = bound(7); // 112
            const viaCall = scale.call({ base: 50 }, 1, 2); // 53

            afterClosures + score + (inst ? 1 : 0) + viaBind + viaCall;
            "#,
        )
        .expect("script should evaluate");
    assert_eq!(res.as_number(), Some(16.0 + 17.0 + 1.0 + 112.0 + 53.0));
}

#[test]
fn monomorphic_property_loop_exceeds_99_percent_ic_hit_rate_and_handles_proto_invalidation() {
    let mut vm = JsVm::new();
    let sum = vm
        .eval_script(
            r#"
            function makePoint(x, y) {
                return { x, y };
            }
            const pts = [];
            for (let i = 0; i < 50; i++) {
                pts.push(makePoint(i, i * 2));
            }
            function sumAll(arr) {
                let acc = 0;
                for (let round = 0; round < 200; round++) {
                    for (let i = 0; i < arr.length; i++) {
                        const p = arr[i];
                        acc += p.x + p.y;
                    }
                }
                return acc;
            }
            sumAll(pts);
            "#,
        )
        .expect("monomorphic loop should run");

    // 50 points: sum(i + 2i) for i=0..49 = 3 * 49 * 50 / 2 = 3675; * 200 = 735000.
    assert_eq!(sum.as_number(), Some(735_000.0));
    let hit_rate = vm.ic_stats.hit_rate();
    assert!(
        hit_rate > 0.99,
        "expected > 99% IC hit rate on monomorphic property loop, got {:.2}% ({:?})",
        hit_rate * 100.0,
        vm.ic_stats
    );

    // Prototype chain mutation must invalidate cached prototype-chain reads immediately.
    let updated = vm
        .eval_script(
            r#"
            const proto = { factor: 10 };
            const obj = Object.create(proto);
            obj.v = 3;
            function readFactor(o) { return o.v * o.factor; }
            const before = readFactor(obj); // 30 (warms proto IC)
            const before2 = readFactor(obj); // 30 (hits proto IC)
            proto.factor = 100;              // bumps prototype_epoch!
            const after = readFactor(obj);   // must see 300, not stale 30!
            before + before2 + after;
            "#,
        )
        .expect("proto invalidation test should run");
    assert_eq!(updated.as_number(), Some(360.0));
}

#[test]
fn exceptions_try_catch_finally_destructuring_optional_chaining_and_loops() {
    let mut vm = JsVm::new();
    let res = vm
        .eval_script(
            r#"
            let log = "";
            function inner(x) {
                if (x < 0) throw new RangeError("negative");
                return x * 2;
            }
            function outer(x) {
                try {
                    return inner(x);
                } catch (e) {
                    log += e.name + ":" + e.message + "|";
                    return -1;
                } finally {
                    log += "fin|";
                }
            }
            const r1 = outer(-5);
            const r2 = outer(4);

            const [head, , third = 9, ...rest] = [1, 2, undefined, 4, 5];
            const { a, b: renamed = 20, ...others } = { a: 3, c: 7 };
            const opt1 = null?.foo?.bar ?? 42;
            const opt2 = { foo: { bar: 11 } }?.foo?.bar ?? 42;

            let sum = 0;
            outerLoop: for (let i = 0; i < 5; i++) {
                for (let j = 0; j < 5; j++) {
                    if (j === 2) continue outerLoop;
                    if (i === 3) break outerLoop;
                    sum += i * 10 + j;
                }
            }
            // i=0: 0+1=1; i=1: 10+11=21; i=2: 20+21=41 -> sum = 63
            log + "|" + (r1 + r2 + head + third + rest.length + a + renamed + others.c + opt1 + opt2 + sum);
            "#,
        )
        .expect("exceptions and control flow should succeed");
    assert_eq!(
        vm.value_to_rust_string(res),
        "RangeError:negative|fin|fin||165"
    );
}

#[test]
fn proxy_traps_reflect_and_revocable_proxy() {
    let mut vm = JsVm::new();
    let res = vm
        .eval_script(
            r#"
            const target = { a: 10, b: 20 };
            let writes = 0;
            const proxy = new Proxy(target, {
                get(t, prop, recv) {
                    if (prop === "computed") return Reflect.get(t, "a", recv) + 5;
                    return Reflect.get(t, prop, recv);
                },
                set(t, prop, val, recv) {
                    writes++;
                    return Reflect.set(t, prop, val * 2, recv);
                },
                has(t, prop) {
                    return prop === "virtual" || Reflect.has(t, prop);
                }
            });
            proxy.c = 7; // target.c becomes 14
            const total = proxy.computed + proxy.c + ("virtual" in proxy ? 1 : 0) + writes;

            const rev = Proxy.revocable({ x: 9 }, {});
            const beforeRevoke = rev.proxy.x;
            rev.revoke();
            let revokedThrew = false;
            try {
                const _ = rev.proxy.x;
            } catch (e) {
                revokedThrew = e.name === "TypeError";
            }
            total + beforeRevoke + (revokedThrew ? 100 : 0);
            "#,
        )
        .expect("Proxy & Reflect should work");
    // computed=15, c=14, virtual=1, writes=1 => total=31; beforeRevoke=9; revokedThrew=100 => 140
    assert_eq!(res.as_number(), Some(140.0));
}

#[test]
fn cooperative_loop_interrupt_budget_halts_infinite_loop_cleanly() {
    let mut vm = JsVm::new();
    vm.max_loop_iterations = Some(5_000);
    let err = vm
        .eval_script("let x = 0; while (true) { x++; }")
        .expect_err("infinite loop must be interrupted by budget");
    match err {
        EvalError::Runtime(exc) => {
            assert!(
                exc.message.contains("loop budget exceeded"),
                "unexpected interrupt message: {}",
                exc.message
            );
        }
        other => panic!("expected runtime interrupt error, got {other:?}"),
    }
    assert_eq!(vm.loop_iterations, 5_001);
    assert_eq!(JsValue::UNDEFINED, JsValue::UNDEFINED);
}
