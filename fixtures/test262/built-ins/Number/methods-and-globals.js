/*---
description: Number static helpers, toFixed, parseInt, parseFloat, and URI encoding
---*/
assert.sameValue(Number.isInteger(42), true);
assert.sameValue(Number.isInteger(42.5), false);
assert.sameValue(Number.isNaN(NaN), true);
assert.sameValue(Number.isNaN("NaN"), false);
assert.sameValue((3.14159).toFixed(2), "3.14");
assert.sameValue(parseInt("0xff", 16), 255);
assert.sameValue(parseFloat("3.5px"), 3.5);
assert.sameValue(decodeURIComponent(encodeURIComponent("a b&c=1")), "a b&c=1");
