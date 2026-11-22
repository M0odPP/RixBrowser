/*---
description: Property deletion and in operator checks
---*/
const o = { a: 1, b: 2 };
assert.sameValue("a" in o, true);
assert.sameValue(delete o.a, true);
assert.sameValue("a" in o, false);
assert.sameValue("b" in o, true);
