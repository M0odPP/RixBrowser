/*---
description: String.fromCodePoint and codePointAt handle astral Unicode symbols
---*/
const rocket = String.fromCodePoint(0x1F680);
assert.sameValue(rocket.codePointAt(0), 0x1F680);
assert.sameValue(rocket, "\u{1F680}");
