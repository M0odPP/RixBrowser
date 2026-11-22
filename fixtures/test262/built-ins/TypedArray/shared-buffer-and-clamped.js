/*---
description: TypedArray subarray aliasing, Uint8ClampedArray rounding, and map/reduce/sort
---*/
const ab = new ArrayBuffer(8);
const full = new Int16Array(ab);
full.set([40, -10, 20, 5]);
const sub = full.subarray(1, 3);
sub[0] = 99;
assert.sameValue(full[1], 99);
full.sort();
assert.sameValue(full.join(","), "5,20,40,99");
const c = Uint8ClampedArray.of(-10, 2.5, 3.5, 999);
assert.sameValue(c.join(","), "0,2,4,255");
