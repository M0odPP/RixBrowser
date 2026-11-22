/*---
description: Switch statement with fallthrough, break, and default
---*/
function classify(n) {
    let out = "";
    switch (n) {
        case 1:
        case 2:
            out += "low";
            break;
        case 3:
            out += "mid-";
        default:
            out += "other";
    }
    return out;
}
assert.sameValue(classify(1), "low");
assert.sameValue(classify(2), "low");
assert.sameValue(classify(3), "mid-other");
assert.sameValue(classify(9), "other");
