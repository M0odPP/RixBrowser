/*---
description: Labeled break and continue across nested loops
---*/
let count = 0;
outer: for (let i = 0; i < 5; i++) {
    for (let j = 0; j < 5; j++) {
        if (j === 2) continue outer;
        if (i === 3) break outer;
        count++;
    }
}
assert.sameValue(count, 6);
