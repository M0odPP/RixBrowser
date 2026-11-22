/*---
description: Proxy get, set, has, apply, construct traps and Proxy.revocable
---*/
const target = { count: 1 };
const p = new Proxy(target, {
    get(t, k) { return k in t ? t[k] * 10 : -1; },
    set(t, k, v) { t[k] = v + 1; return true; }
});
assert.sameValue(p.count, 10);
assert.sameValue(p.missing, -1);
p.count = 4;
assert.sameValue(target.count, 5);
const { proxy, revoke } = Proxy.revocable({ x: 1 }, {});
assert.sameValue(proxy.x, 1);
revoke();
assert.throws(TypeError, () => proxy.x);
