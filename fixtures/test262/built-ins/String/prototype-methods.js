/*---
description: String slice, split, trim, pad, repeat, replaceAll, startsWith, includes, at
---*/
assert.sameValue("  Hello World  ".trim().toLowerCase(), "hello world");
assert.sameValue("42".padStart(5, "0"), "00042");
assert.sameValue("ab".repeat(3), "ababab");
assert.sameValue("a-b-c".replaceAll("-", "_"), "a_b_c");
assert.sameValue("abcdef".slice(1, -1), "bcde");
assert.sameValue("hello".startsWith("he") && "hello".endsWith("lo") && "hello".includes("ell"), true);
assert.sameValue("xyz".at(-1), "z");
assert.sameValue(String.fromCharCode(65, 66, 67), "ABC");
