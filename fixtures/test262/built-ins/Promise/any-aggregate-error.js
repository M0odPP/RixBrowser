/*---
description: Promise.any rejects with AggregateError when all inputs reject
flags: [async]
---*/
Promise.any([Promise.reject("e1"), Promise.reject("e2")]).then(
    () => $DONE("should not fulfill"),
    err => {
        assert.sameValue(err.name, "AggregateError");
        assert.sameValue(err.errors.join(","), "e1,e2");
        $DONE();
    }
);
