/*---
description: Basic numeric addition smoke case for Phase 2 JS VM tracking
flags: [onlyStrict]
---*/
const sum = 40 + 2;
if (sum !== 42) {
  throw new Error("Expected 42");
}
