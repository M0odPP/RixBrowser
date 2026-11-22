/*---
description: Date.UTC, Date.parse, UTC getters, and toISOString round-trip
---*/
const ms = Date.UTC(2025, 0, 15, 12, 30, 45, 500);
const d = new Date(ms);
assert.sameValue(d.getUTCFullYear(), 2025);
assert.sameValue(d.getUTCMonth(), 0);
assert.sameValue(d.getUTCDate(), 15);
assert.sameValue(d.getUTCHours(), 12);
assert.sameValue(d.toISOString(), "2025-01-15T12:30:45.500Z");
assert.sameValue(Date.parse("2025-01-15T12:30:45.500Z"), ms);
