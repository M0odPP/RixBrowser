/*---
description: DataView big-endian and little-endian multi-byte reads/writes and RangeError bounds
---*/
const buf = new ArrayBuffer(8);
const dv = new DataView(buf);
dv.setInt16(0, 0x0102, false);
assert.sameValue(dv.getUint8(0), 1);
assert.sameValue(dv.getUint8(1), 2);
dv.setFloat32(4, 2.5, true);
assert.sameValue(dv.getFloat32(4, true), 2.5);
assert.throws(RangeError, () => dv.getUint32(6));
