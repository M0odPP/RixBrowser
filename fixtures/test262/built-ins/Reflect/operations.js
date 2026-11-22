/*---
description: Reflect.get, set, has, deleteProperty, ownKeys, apply, construct
---*/
const obj = { a: 10 };
assert.sameValue(Reflect.get(obj, "a"), 10);
Reflect.set(obj, "b", 20);
assert.sameValue(Reflect.has(obj, "b"), true);
assert.sameValue(Reflect.ownKeys(obj).join(","), "a,b");
assert.sameValue(Reflect.deleteProperty(obj, "a"), true);
assert.sameValue(Reflect.has(obj, "a"), false);
assert.sameValue(Reflect.apply(Math.max, null, [3, 9, 5]), 9);
