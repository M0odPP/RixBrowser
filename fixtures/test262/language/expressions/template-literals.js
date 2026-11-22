/*---
description: Template literal interpolation and nesting
---*/
const name = "Rix";
const ver = 2;
assert.sameValue(`Hello ${name} v0.${ver * 1}!`, "Hello Rix v0.2!");
assert.sameValue(`sum=${1 + 2 + 3}`, "sum=6");
