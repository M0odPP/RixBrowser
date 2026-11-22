/*---
description: Object.create, Object.getPrototypeOf, and Object.setPrototypeOf
---*/
const proto = { greet() { return "hi " + this.who; } };
const inst = Object.create(proto);
inst.who = "world";
assert.sameValue(Object.getPrototypeOf(inst), proto);
assert.sameValue(inst.greet(), "hi world");
const alt = { greet() { return "bye " + this.who; } };
Object.setPrototypeOf(inst, alt);
assert.sameValue(inst.greet(), "bye world");
