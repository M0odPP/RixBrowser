/*---
description: Arithmetic, bitwise, shift, exponentiation, and nullish coalescing precedence
---*/
assert.sameValue(2 + 3 * 4 ** 2, 50);
assert.sameValue((1 << 4) | 3, 19);
assert.sameValue(-8 >>> 1, 2147483644);
assert.sameValue(null ?? undefined ?? 0 ?? 5, 0);
assert.sameValue(false || "fallback", "fallback");
assert.sameValue(true && 7, 7);
