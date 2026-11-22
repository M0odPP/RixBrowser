/*---
description: Intl.PluralRules cardinal and ordinal CLDR categories
---*/
const uk = new Intl.PluralRules("uk-UA");
assert.sameValue(uk.select(1), "one");
assert.sameValue(uk.select(2), "few");
assert.sameValue(uk.select(5), "many");
assert.sameValue(uk.select(21), "one");

const enOrd = new Intl.PluralRules("en-US", { type: "ordinal" });
assert.sameValue(enOrd.select(1), "one");
assert.sameValue(enOrd.select(2), "two");
assert.sameValue(enOrd.select(3), "few");
assert.sameValue(enOrd.select(4), "other");
assert.sameValue(enOrd.resolvedOptions().type, "ordinal");
assert.throws(RangeError, () => new Intl.PluralRules("en", { type: "invalid" }));
