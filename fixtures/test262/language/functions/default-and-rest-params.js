/*---
description: Default parameter expressions and rest parameter collection
---*/
function combine(first = 10, second = first * 2, ...rest) {
    return first + second + rest.reduce((a, b) => a + b, 0);
}
assert.sameValue(combine(), 30);
assert.sameValue(combine(5), 15);
assert.sameValue(combine(1, 2, 3, 4, 5), 15);
