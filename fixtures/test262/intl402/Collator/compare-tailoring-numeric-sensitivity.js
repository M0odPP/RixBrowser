/*---
description: Intl.Collator locale tailoring, numeric ordering, sensitivity, and String.prototype.localeCompare
---*/
const de = new Intl.Collator("de-DE");
const sv = new Intl.Collator("sv-SE");
assert.sameValue(de.compare("ä", "z") < 0, true);
assert.sameValue(sv.compare("ä", "z") > 0, true);

const num = new Intl.Collator("en-US", { numeric: true });
assert.sameValue(["a10", "a2", "a1"].sort(num.compare).join(","), "a1,a2,a10");
assert.sameValue(num.resolvedOptions().numeric, true);

const base = new Intl.Collator("en-US", { sensitivity: "base" });
assert.sameValue(base.compare("E", "é"), 0);
assert.sameValue(base.compare("a", "A"), 0);

assert.sameValue("ä".localeCompare("z", "sv-SE") > 0, true);
assert.throws(RangeError, () => new Intl.Collator("en", { sensitivity: "bogus" }));
