/*---
description: Intl.RelativeTimeFormat auto/always formatting, localization, and formatToParts
---*/
const enAuto = new Intl.RelativeTimeFormat("en-US", { numeric: "auto" });
assert.sameValue(enAuto.format(-1, "day"), "yesterday");
assert.sameValue(enAuto.format(0, "day"), "today");
assert.sameValue(enAuto.format(1, "day"), "tomorrow");

const enAlways = new Intl.RelativeTimeFormat("en-US", { numeric: "always" });
assert.sameValue(enAlways.format(5, "seconds"), "in 5 seconds");
assert.sameValue(enAlways.format(-2, "hours"), "2 hours ago");

const esAuto = new Intl.RelativeTimeFormat("es-ES", { numeric: "auto" });
assert.sameValue(esAuto.format(0, "day"), "hoy");

const parts = enAlways.formatToParts(3, "day");
assert.sameValue(parts.some((p) => p.type === "integer" && p.value === "3" && p.unit === "day"), true);

assert.throws(RangeError, () => enAuto.format(1, "century"));
assert.throws(RangeError, () => enAuto.format(Infinity, "day"));
