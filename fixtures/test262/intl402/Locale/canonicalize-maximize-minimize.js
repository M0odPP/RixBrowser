/*---
description: Intl.getCanonicalLocales, Intl.supportedValuesOf, and Intl.Locale maximize/minimize
---*/
const canon = Intl.getCanonicalLocales(["en-us", "zh-hans-cn", "EN-US"]);
assert.sameValue(canon.length, 2);
assert.sameValue(canon[0], "en-US");
assert.sameValue(canon[1], "zh-Hans-CN");

assert.throws(RangeError, () => Intl.getCanonicalLocales("en_US"));
assert.throws(RangeError, () => Intl.supportedValuesOf("unknown-category"));

const loc = new Intl.Locale("uk", { calendar: "gregory", numeric: true });
assert.sameValue(loc.language, "uk");
assert.sameValue(loc.calendar, "gregory");
assert.sameValue(loc.numeric, true);

const maxed = loc.maximize();
assert.sameValue(maxed.baseName, "uk-Cyrl-UA");
assert.sameValue(maxed.script, "Cyrl");
assert.sameValue(maxed.region, "UA");
assert.sameValue(maxed.minimize().baseName, "uk");

const zones = Intl.supportedValuesOf("timeZone");
assert.sameValue(zones.includes("UTC") || zones.includes("Etc/UTC"), true);
assert.sameValue(zones.includes("America/New_York"), true);
assert.sameValue(zones.includes("Europe/Kyiv"), true);
