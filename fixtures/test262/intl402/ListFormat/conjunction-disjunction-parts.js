/*---
description: Intl.ListFormat conjunction, disjunction, locale formatting, and formatToParts
---*/
const enAnd = new Intl.ListFormat("en-US", { style: "long", type: "conjunction" });
assert.sameValue(enAnd.format(["Alpha", "Beta", "Gamma"]), "Alpha, Beta, and Gamma");

const esAnd = new Intl.ListFormat("es-ES", { style: "long", type: "conjunction" });
assert.sameValue(esAnd.format(["Uno", "Dos", "Tres"]), "Uno, Dos y Tres");

const enOr = new Intl.ListFormat("en-US", { style: "long", type: "disjunction" });
assert.sameValue(enOr.format(["Yes", "No"]), "Yes or No");

const parts = enAnd.formatToParts(["A", "B"]);
assert.sameValue(parts.map((p) => p.type).join(","), "element,literal,element");
assert.throws(TypeError, () => enAnd.format([1, 2]));
