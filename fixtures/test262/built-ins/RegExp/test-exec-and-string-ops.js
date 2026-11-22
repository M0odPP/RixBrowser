/*---
description: RegExp test, exec capture groups, global lastIndex, and String match/replace/split
---*/
const re = /([a-z]+)-(\d+)/g;
const m1 = re.exec("foo-12 bar-34");
assert.sameValue(m1[0], "foo-12");
assert.sameValue(m1[1], "foo");
assert.sameValue(m1[2], "12");
const m2 = re.exec("foo-12 bar-34");
assert.sameValue(m2[0], "bar-34");
assert.sameValue("a1b2c3".replace(/\d/g, "#"), "a#b#c#");
assert.sameValue("one, two; three".split(/[,;]\s*/).join("|"), "one|two|three");
