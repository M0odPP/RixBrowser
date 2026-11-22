/*---
description: ArrayBuffer byteLength, slice copy isolation, and isView
---*/
const buf = new ArrayBuffer(8);
const view = new Uint8Array(buf);
view[0] = 11; view[1] = 22; view[2] = 33;
const copy = buf.slice(1, 3);
assert.sameValue(copy.byteLength, 2);
const copyView = new Uint8Array(copy);
assert.sameValue(copyView[0], 22);
assert.sameValue(copyView[1], 33);
copyView[0] = 99;
assert.sameValue(view[1], 22);
assert.sameValue(ArrayBuffer.isView(view), true);
assert.sameValue(ArrayBuffer.isView(buf), false);
