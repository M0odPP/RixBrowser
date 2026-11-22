/*---
description: Math arithmetic, rounding, clamping, hypot, clz32, and imul
---*/
assert.sameValue(Math.abs(-9), 9);
assert.sameValue(Math.floor(3.9), 3);
assert.sameValue(Math.ceil(3.1), 4);
assert.sameValue(Math.trunc(-3.9), -3);
assert.sameValue(Math.max(1, 9, 4), 9);
assert.sameValue(Math.min(1, -2, 4), -2);
assert.sameValue(Math.hypot(3, 4), 5);
assert.sameValue(Math.clz32(1), 31);
assert.sameValue(Math.imul(0xffffffff, 5), -5);
