/*---
description: BigInt literals, arithmetic, and comparisons
---*/
const big = 100000000000000000000n;
assert.sameValue(big + 23n === 100000000000000000023n, true);
assert.sameValue(big * 2n === 200000000000000000000n, true);
assert.sameValue(typeof big, "bigint");
