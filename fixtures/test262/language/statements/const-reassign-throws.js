/*---
description: Reassigning a const binding throws an error at runtime
---*/
assert.throws(null, () => {
    const c = 1;
    c = 2;
});
