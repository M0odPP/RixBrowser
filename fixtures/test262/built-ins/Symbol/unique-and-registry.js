/*---
description: Symbol uniqueness, Symbol.for registry, and Symbol.keyFor
---*/
const s1 = Symbol("tok");
const s2 = Symbol("tok");
assert.notSameValue(s1, s2);
const g1 = Symbol.for("shared.key");
const g2 = Symbol.for("shared.key");
assert.sameValue(g1, g2);
assert.sameValue(Symbol.keyFor(g1), "shared.key");
assert.sameValue(Symbol.keyFor(s1), undefined);
