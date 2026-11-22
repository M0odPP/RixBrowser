/*---
description: Boolean truthiness and falsiness conversions
---*/
assert.sameValue(Boolean(0), false);
assert.sameValue(Boolean(""), false);
assert.sameValue(Boolean(null), false);
assert.sameValue(Boolean(undefined), false);
assert.sameValue(Boolean(NaN), false);
assert.sameValue(Boolean("0"), true);
assert.sameValue(Boolean([]), true);
assert.sameValue(Boolean({}), true);
