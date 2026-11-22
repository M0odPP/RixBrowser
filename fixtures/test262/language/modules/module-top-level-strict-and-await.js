/*---
description: ES Module executes in strict mode and supports top-level await
flags: [module]
---*/
function checkThis() { return this; }
assert.sameValue(checkThis(), undefined);
const v = await Promise.resolve(123);
assert.sameValue(v, 123);
export const answer = v;
