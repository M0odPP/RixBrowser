/*---
description: Object.defineProperty and Object.getOwnPropertyDescriptor
---*/
const o = {};
let backing = 7;
Object.defineProperty(o, "val", {
    get() { return backing; },
    set(v) { backing = v * 2; },
    enumerable: true,
    configurable: true
});
assert.sameValue(o.val, 7);
o.val = 10;
assert.sameValue(o.val, 20);
const desc = Object.getOwnPropertyDescriptor(o, "val");
assert.sameValue(desc.enumerable, true);
assert.sameValue(typeof desc.get, "function");
