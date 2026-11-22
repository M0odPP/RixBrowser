/*---
description: WeakSet object membership and TypeError on primitive values
---*/
const ws = new WeakSet();
const o = {};
ws.add(o);
assert.sameValue(ws.has(o), true);
assert.sameValue(ws.delete(o), true);
assert.sameValue(ws.has(o), false);
assert.throws(TypeError, () => ws.add("primitive"));
