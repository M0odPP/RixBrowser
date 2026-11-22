/*---
description: Function hoisting and nested mutable closure upvalues
---*/
const factor = 3;
assert.sameValue(hoisted(5), 15);
function hoisted(n) { return n * factor; }
function makeCounter(start) {
    let n = start;
    return { inc() { return ++n; }, get() { return n; } };
}
const c = makeCounter(10);
assert.sameValue(c.inc(), 11);
assert.sameValue(c.inc(), 12);
assert.sameValue(c.get(), 12);
