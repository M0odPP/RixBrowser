/*---
description: Block scoping of let/const inside loops creates fresh per-iteration bindings
---*/
const fns = [];
for (let i = 0; i < 4; i++) {
    fns.push(() => i);
}
assert.sameValue(fns.map(f => f()).join(","), "0,1,2,3");
