/*---
description: WeakRef deref returns live target and rejects primitive target
---*/
const obj = { ok: true };
const wr = new WeakRef(obj);
assert.sameValue(wr.deref(), obj);
assert.throws(TypeError, () => new WeakRef(42));
