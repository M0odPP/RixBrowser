/*---
description: Object.assign, keys, values, entries, fromEntries, freeze, and isFrozen
---*/
const merged = Object.assign({ a: 1 }, { b: 2, c: 3 });
assert.sameValue(Object.keys(merged).join(","), "a,b,c");
assert.sameValue(Object.values(merged).join(","), "1,2,3");
const rebuilt = Object.fromEntries(Object.entries(merged));
assert.sameValue(rebuilt.b, 2);
Object.freeze(rebuilt);
assert.sameValue(Object.isFrozen(rebuilt), true);
