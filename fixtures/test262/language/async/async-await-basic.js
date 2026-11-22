/*---
description: Async functions resolve promises and catch rejected awaits
flags: [async]
---*/
async function run() {
    const x = await Promise.resolve(20);
    let y = 0;
    try {
        await Promise.reject(22);
    } catch (e) {
        y = e;
    }
    return x + y;
}
run().then(v => {
    assert.sameValue(v, 42);
    $DONE();
}).catch($DONE);
