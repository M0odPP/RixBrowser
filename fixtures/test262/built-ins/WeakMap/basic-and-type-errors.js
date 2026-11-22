/*---
description: WeakMap object keys and TypeError on primitive keys
---*/
const wm = new WeakMap();
const k = {};
wm.set(k, "secret");
assert.sameValue(wm.has(k), true);
assert.sameValue(wm.get(k), "secret");
assert.sameValue(wm.delete(k), true);
assert.sameValue(wm.has(k), false);
assert.throws(TypeError, () => wm.set(123, "bad"));
