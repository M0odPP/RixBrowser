/*---
description: Strict and abstract equality edge cases
---*/
assert.sameValue(NaN === NaN, false);
assert.sameValue(+0 === -0, true);
assert.sameValue(Object.is(+0, -0), false);
assert.sameValue(Object.is(NaN, NaN), true);
assert.sameValue(null == undefined, true);
assert.sameValue(null === undefined, false);
assert.sameValue("42" == 42, true);
assert.sameValue("42" === 42, false);
