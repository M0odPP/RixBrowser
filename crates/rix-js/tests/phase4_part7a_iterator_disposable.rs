//! Spec regression tests for Phase 4 Part 7A:
//! - ES2025 Iterator & Iterator Helpers (%IteratorHelperPrototype%, Iterator.from, Iterator.concat)
//! - ES2025 Explicit Resource Management (SuppressedError, DisposableStack, AsyncDisposableStack)

use rix_js::{JsValue, JsVm};

fn eval(src: &str) -> JsValue {
    let mut vm = JsVm::new();
    vm.eval_script(src)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\nsrc:\n{src}"))
}

#[test]
fn iterator_helpers_lazy_flatmap_concat_and_dispose() {
    let v = eval(
        r#"
        let closed = 0;
        function* naturals() {
            try {
                let i = 0;
                while (true) yield i++;
            } finally {
                closed++;
            }
        }
        const out = naturals()
            .drop(2)
            .filter(x => x % 2 === 0)
            .flatMap(x => [x, x + 1])
            .take(4)
            .toArray();
        const cat = Iterator.concat([1, 2], new Set([3, 4])).map(x => x * 10).toArray();
        let disposed = false;
        const custom = Object.assign(Object.create(Iterator.prototype), {
            next() { return { value: 1, done: false }; },
            return() { disposed = true; return { done: true }; }
        });
        custom[Symbol.dispose]();
        JSON.stringify(out) === "[2,3,4,5]"
            && JSON.stringify(cat) === "[10,20,30,40]"
            && disposed === true
            && Iterator.prototype[Symbol.toStringTag] === "Iterator";
        "#,
    );
    assert_eq!(v.as_bool(), Some(true));
}

#[test]
fn disposable_and_async_disposable_stacks_and_suppressed_error() {
    let mut vm = JsVm::new();
    vm.eval_script(
        r#"
        const log = [];
        let caught = null;
        {
            const stack = new DisposableStack();
            stack.defer(() => { log.push("first"); throw new Error("outer"); });
            stack.adopt("res", v => { log.push("adopt:" + v); });
            stack.use({ [Symbol.dispose]() { log.push("inner"); throw new Error("inner"); } });
            try { stack.dispose(); } catch (e) { caught = e; }
            globalThis.syncOk = stack.disposed
                && JSON.stringify(log) === '["inner","adopt:res","first"]'
                && caught instanceof SuppressedError
                && caught.error.message === "outer"
                && caught.suppressed.message === "inner";
        }
        const astack = new AsyncDisposableStack();
        const alog = [];
        astack.defer(async () => { alog.push("a1"); });
        astack.use({ [Symbol.asyncDispose]: async () => { alog.push("a2"); } });
        astack.disposeAsync().then(() => {
            globalThis.asyncOk = astack.disposed && JSON.stringify(alog) === '["a2","a1"]';
        });
        "#,
    )
    .unwrap();
    vm.drain_microtasks();
    let ok = vm.eval_script("syncOk && asyncOk").unwrap();
    assert_eq!(ok.as_bool(), Some(true));
}
