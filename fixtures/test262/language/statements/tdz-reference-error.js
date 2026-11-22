/*---
description: Reading a let binding before initialization throws ReferenceError
negative:
  phase: runtime
  type: ReferenceError
---*/
{
    const v = x;
    let x = 1;
}
