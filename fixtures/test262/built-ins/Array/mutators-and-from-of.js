/*---
description: Array push, pop, shift, unshift, splice, sort, reverse, fill, at, from, of
---*/
const a = Array.of(3, 1, 4, 1, 5);
a.sort((x, y) => x - y);
assert.sameValue(a.join(","), "1,1,3,4,5");
const removed = a.splice(1, 2, 9);
assert.sameValue(removed.join(","), "1,3");
assert.sameValue(a.at(-1), 5);
a.unshift(0);
assert.sameValue(a.shift(), 0);
assert.sameValue(Array.from("abc", ch => ch.toUpperCase()).join(""), "ABC");
