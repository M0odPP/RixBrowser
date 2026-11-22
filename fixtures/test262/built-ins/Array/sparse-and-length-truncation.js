/*---
description: Setting array.length truncates elements and preserves remaining values
---*/
const arr = [10, 20, 30, 40, 50];
arr.length = 2;
assert.sameValue(arr.length, 2);
assert.sameValue(arr[2], undefined);
assert.sameValue(arr.join(","), "10,20");
