/*---
description: JSON.parse with reviver and JSON.stringify with replacer, toJSON, and cycle rejection
---*/
const parsed = JSON.parse('{"a":1,"b":[2,3]}', (k, v) => typeof v === "number" ? v * 10 : v);
assert.sameValue(parsed.a, 10);
assert.sameValue(parsed.b.join(","), "20,30");
const custom = { x: 1, toJSON() { return { serialized: true }; } };
assert.sameValue(JSON.stringify(custom), '{"serialized":true}');
const cyc = {}; cyc.self = cyc;
assert.throws(TypeError, () => JSON.stringify(cyc));
