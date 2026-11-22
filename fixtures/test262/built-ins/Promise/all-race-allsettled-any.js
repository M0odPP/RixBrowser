/*---
description: Promise.all, Promise.race, Promise.allSettled, and Promise.any
flags: [async]
---*/
Promise.all([
    Promise.all([1, Promise.resolve(2)]),
    Promise.race([Promise.resolve("r"), new Promise(() => {})]),
    Promise.allSettled([Promise.resolve("a"), Promise.reject("b")]),
    Promise.any([Promise.reject(1), Promise.resolve("ok")])
]).then(([allRes, raceRes, settledRes, anyRes]) => {
    assert.sameValue(allRes.join(","), "1,2");
    assert.sameValue(raceRes, "r");
    assert.sameValue(settledRes[0].status, "fulfilled");
    assert.sameValue(settledRes[1].status, "rejected");
    assert.sameValue(anyRes, "ok");
    $DONE();
}).catch($DONE);
