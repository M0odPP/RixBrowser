/*---
description: Logical assignment operators (&&=, ||=, ??=)
---*/
let a = 0; a ||= 10; assert.sameValue(a, 10);
let b = 5; b &&= 20; assert.sameValue(b, 20);
let c = null; c ??= 30; assert.sameValue(c, 30);
let d = 0; d ??= 40; assert.sameValue(d, 0);
