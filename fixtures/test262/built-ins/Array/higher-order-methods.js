/*---
description: Array map, filter, reduce, find, findIndex, some, every, flat, flatMap
---*/
const nums = [1, 2, 3, 4, 5];
assert.sameValue(nums.map(x => x * 2).filter(x => x > 4).reduce((a, b) => a + b, 0), 24);
assert.sameValue(nums.find(x => x > 3), 4);
assert.sameValue(nums.findIndex(x => x > 3), 3);
assert.sameValue(nums.some(x => x === 5), true);
assert.sameValue(nums.every(x => x > 0), true);
assert.sameValue([[1, 2], [3, [4]]].flat(2).join(","), "1,2,3,4");
assert.sameValue([1, 2].flatMap(x => [x, x * 10]).join(","), "1,10,2,20");
