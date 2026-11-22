/*---
description: Generator next(), send values, return(), throw(), and yield* delegation
---*/
function* sub() { yield 2; yield 3; return "done"; }
function* main() {
    const a = yield 1;
    const r = yield* sub();
    try {
        yield a + ":" + r;
    } catch (e) {
        yield "caught:" + e;
    }
}
const g = main();
assert.sameValue(g.next().value, 1);
assert.sameValue(g.next(99).value, 2);
assert.sameValue(g.next().value, 3);
assert.sameValue(g.next().value, "99:done");
assert.sameValue(g.throw("err").value, "caught:err");
assert.sameValue(g.next().done, true);
