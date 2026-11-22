/*---
description: Promise chaining across then, catch recovery, and finally passthrough
flags: [async]
---*/
let cleaned = false;
Promise.resolve(5)
    .then(x => x * 2)
    .then(() => { throw new Error("oops"); })
    .catch(e => e.message + "!")
    .finally(() => { cleaned = true; })
    .then(res => {
        assert.sameValue(cleaned, true);
        assert.sameValue(res, "oops!");
        $DONE();
    })
    .catch($DONE);
