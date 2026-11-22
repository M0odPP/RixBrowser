/*---
description: Map SameValueZero keys, insertion order, iteration, and ES2024 Map.groupBy
---*/
const m = new Map();
m.set(NaN, "nan").set(-0, "zero").set("k", 1);
assert.sameValue(m.size, 3);
assert.sameValue(m.get(NaN), "nan");
assert.sameValue(m.get(0), "zero");
assert.sameValue(m.delete("k"), true);
assert.sameValue(m.size, 2);
const g = Map.groupBy([10, 15, 20, 25], x => x % 10 === 0 ? "ten" : "five");
assert.sameValue(g.get("ten").join(","), "10,20");
