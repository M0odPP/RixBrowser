/*---
description: Arrow functions inherit lexical this from enclosing method
---*/
const obj = {
    mult: 7,
    scale(arr) {
        return arr.map(x => x * this.mult);
    }
};
assert.sameValue(obj.scale([1, 2, 3]).join(","), "7,14,21");
