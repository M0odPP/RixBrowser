/*---
description: Optional chaining on property, element, and call expressions
---*/
const obj = { a: { b: [10, 20], fn(x) { return x + 1; } } };
assert.sameValue(obj?.a?.b?.[1], 20);
assert.sameValue(obj?.a?.fn?.(9), 10);
assert.sameValue(obj?.missing?.deep?.(), undefined);
assert.sameValue((null)?.prop, undefined);
