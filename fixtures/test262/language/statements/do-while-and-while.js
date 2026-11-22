/*---
description: do-while executes at least once and while loops cleanly
---*/
let n = 0;
do { n += 5; } while (false);
assert.sameValue(n, 5);
while (n < 20) { n += 5; }
assert.sameValue(n, 20);
