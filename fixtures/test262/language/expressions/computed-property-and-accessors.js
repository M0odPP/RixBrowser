/*---
description: Computed property names and object literal getters/setters
---*/
const key = "score";
const box = {
    _v: 10,
    [key]: 5,
    get value() { return this._v + this.score; },
    set value(n) { this._v = n; }
};
assert.sameValue(box.value, 15);
box.value = 100;
assert.sameValue(box.value, 105);
