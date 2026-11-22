/*---
description: Strict mode forbids with statement
flags: [onlyStrict]
negative:
  phase: parse
  type: SyntaxError
---*/
with ({}) {}
