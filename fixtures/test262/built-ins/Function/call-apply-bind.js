/*---
description: Function.prototype.call, apply, and bind with partial arguments
---*/
function add(a, b, c) { return this.base + a + b + c; }
assert.sameValue(add.call({ base: 100 }, 1, 2, 3), 106);
assert.sameValue(add.apply({ base: 200 }, [4, 5, 6]), 215);
const bound = add.bind({ base: 10 }, 2, 3);
assert.sameValue(bound(5), 20);
