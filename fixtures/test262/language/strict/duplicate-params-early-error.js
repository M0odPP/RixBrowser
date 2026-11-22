/*---
description: Strict mode forbids duplicate formal parameters
flags: [onlyStrict]
negative:
  phase: parse
  type: SyntaxError
---*/
function f(a, a) { return a; }
