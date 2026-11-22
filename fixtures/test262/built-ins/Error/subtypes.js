/*---
description: Error, TypeError, RangeError, ReferenceError, SyntaxError, and URIError constructors
---*/
for (const Ctor of [Error, TypeError, RangeError, ReferenceError, SyntaxError, URIError]) {
    const e = new Ctor("msg");
    assert.sameValue(e.name, Ctor.name);
    assert.sameValue(e.message, "msg");
}
