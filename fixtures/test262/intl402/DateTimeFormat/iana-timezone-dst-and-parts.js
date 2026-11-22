/*---
description: Intl.DateTimeFormat IANA timezone resolution, DST handling, and formatToParts
---*/
const winter = Date.UTC(2025, 0, 15, 15, 30, 0);
const summer = Date.UTC(2025, 6, 15, 15, 30, 0);

const ny = new Intl.DateTimeFormat("en-US", {
    timeZone: "America/New_York",
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
});
assert.sameValue(ny.resolvedOptions().timeZone, "America/New_York");
assert.sameValue(ny.format(winter).includes("10:30"), true);
assert.sameValue(ny.format(summer).includes("11:30"), true);

const parts = ny.formatToParts(new Date(winter));
assert.sameValue(parts.some((p) => p.type === "year" && p.value === "2025"), true);
assert.sameValue(parts.some((p) => p.type === "hour" && p.value === "10"), true);

assert.throws(RangeError, () => new Intl.DateTimeFormat("en-US", { timeZone: "Invalid/Zone" }));
assert.throws(RangeError, () => ny.format(NaN));
