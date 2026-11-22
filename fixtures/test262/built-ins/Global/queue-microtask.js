/*---
description: queueMicrotask executes strictly in FIFO order after synchronous code
flags: [async]
---*/
const seq = [];
seq.push(1);
queueMicrotask(() => seq.push(3));
queueMicrotask(() => {
    seq.push(4);
    assert.sameValue(seq.join(","), "1,2,3,4");
    $DONE();
});
seq.push(2);
