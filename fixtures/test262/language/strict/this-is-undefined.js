/*---
description: Unbound function call in strict mode receives undefined this
flags: [onlyStrict]
---*/
function getThis() { return this; }
assert.sameValue(getThis(), undefined);
