/*---
description: for-in enumerates prototype chain keys and for-of iterates values
---*/
const base = { a: 1 };
const child = Object.create(base);
child.b = 2;
const keys = [];
for (const k in child) keys.push(k);
assert.sameValue(keys.join(","), "b,a");
let total = 0;
for (const v of [10, 20, 30]) total += v;
assert.sameValue(total, 60);
