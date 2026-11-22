/*---
description: Set dedup and ES2024 union, intersection, difference, symmetricDifference, subset/superset
---*/
const a = new Set([1, 2, 3]);
const b = new Set([3, 4, 5]);
assert.sameValue([...a.union(b)].join(","), "1,2,3,4,5");
assert.sameValue([...a.intersection(b)].join(","), "3");
assert.sameValue([...a.difference(b)].join(","), "1,2");
assert.sameValue([...a.symmetricDifference(b)].join(","), "1,2,4,5");
assert.sameValue(new Set([1, 2]).isSubsetOf(a), true);
assert.sameValue(a.isSupersetOf(new Set([2, 3])), true);
assert.sameValue(new Set([8, 9]).isDisjointFrom(a), true);
