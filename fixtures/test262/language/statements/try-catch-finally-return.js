/*---
description: Finally executes before returning from try or catch blocks
---*/
const trace = [];
function run(shouldThrow) {
    try {
        trace.push("try");
        if (shouldThrow) throw new Error("fail");
        return "ok";
    } catch (e) {
        trace.push("catch:" + e.message);
        return "recovered";
    } finally {
        trace.push("finally");
    }
}
assert.sameValue(run(false), "ok");
assert.sameValue(run(true), "recovered");
assert.sameValue(trace.join("|"), "try|finally|try|catch:fail|finally");
