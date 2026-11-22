/*---
description: Intl.NumberFormat decimal, currency, percent, compact formatting and formatToParts
---*/
const en = new Intl.NumberFormat("en-US");
const de = new Intl.NumberFormat("de-DE");
assert.sameValue(en.format(1234567.89), "1,234,567.89");
assert.sameValue(de.format(1234567.89), "1.234.567,89");

const usd = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });
assert.sameValue(usd.format(99.5), "$99.50");
assert.sameValue(usd.formatToParts(99.5).some((p) => p.type === "currency" && p.value === "$"), true);

const pct = new Intl.NumberFormat("en-US", { style: "percent" });
assert.sameValue(pct.format(0.25), "25%");

const compact = new Intl.NumberFormat("en-US", { notation: "compact" });
assert.sameValue(compact.format(2500000), "2.5M");

assert.throws(TypeError, () => new Intl.NumberFormat("en-US", { style: "currency" }));
assert.throws(RangeError, () => new Intl.NumberFormat("en-US", { style: "currency", currency: "INVALID" }));
