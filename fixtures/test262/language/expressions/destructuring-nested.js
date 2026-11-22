/*---
description: Nested array and object destructuring with defaults and rest elements
---*/
const { a: [x, y = 2, ...rest], b: { c = 9 } = {} } = { a: [1, undefined, 3, 4] };
assert.sameValue(x, 1);
assert.sameValue(y, 2);
assert.sameValue(rest.join(","), "3,4");
assert.sameValue(c, 9);
