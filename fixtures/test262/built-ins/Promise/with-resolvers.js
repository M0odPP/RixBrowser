/*---
description: ES2024 Promise.withResolvers creates externally controllable promise
flags: [async]
---*/
const { promise, resolve } = Promise.withResolvers();
resolve(777);
promise.then(v => {
    assert.sameValue(v, 777);
    $DONE();
}).catch($DONE);
