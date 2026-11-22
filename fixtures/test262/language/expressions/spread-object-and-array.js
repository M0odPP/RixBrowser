/*---
description: Object and array spread expressions
---*/
const a = [1, 2];
const b = [...a, 3, ...[4, 5]];
assert.sameValue(b.join(","), "1,2,3,4,5");
const o1 = { x: 1, y: 2 };
const o2 = { ...o1, y: 20, z: 30 };
assert.sameValue(o2.x + o2.y + o2.z, 51);
