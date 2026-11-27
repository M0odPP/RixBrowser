//! Spec and stack-frame tests for Memory-Backed Frame Baseline JIT (`Tier-2L`)
//! covering large JS bundle functions (`> 1,024` bytecodes / `> 48` registers) and
//! deep React/framework-style recursion without overflowing the Linux 8 MiB stack.

use rix_jit::BaselineJit;
use rix_js::{compile, JsVm, SourceType};

#[test]
fn large_register_and_bytecode_function_compiles_and_matches_interpreter() {
    // Generate a JS function with > 64 live local variables and > 1,200 bytecodes so it enters
    // Memory-Backed Frame Baseline JIT (`mem_backed = true`).
    let mut src = String::from("function bigBundle(x) {\n");
    for i in 0..72 {
        src.push_str(&format!("  let v{i} = x + {i};\n"));
    }
    src.push_str("  let acc = 0;\n");
    src.push_str("  for (let k = 0; k < 20; k++) {\n");
    for i in 0..72 {
        src.push_str(&format!("    acc = acc + v{i} + k;\n"));
    }
    src.push_str("  }\n  return acc;\n}\n");
    src.push_str("let total = 0; for (let r = 0; r < 6; r++) total += bigBundle(r); total;");

    let prog1 = compile(&src, SourceType::Script).expect("compile script");
    assert!(
        prog1
            .functions
            .iter()
            .any(|f| f.code.len() > 1_024 || f.register_count > 48),
        "expected function exceeding SSA budget"
    );
    let mut vm_interp = JsVm::new();
    let expected = vm_interp.execute_program(prog1).expect("tier-1 interp");

    let prog2 = compile(&src, SourceType::Script).expect("compile script");
    let mut vm_jit = JsVm::new();
    let jit = BaselineJit::with_thresholds(1, 1);
    vm_jit.attach_jit(jit.clone());
    let actual = vm_jit.execute_program(prog2).expect("tier-2L jit");

    assert_eq!(actual.as_number(), expected.as_number());
    assert!(
        jit.stats().compiled_functions >= 1,
        "expected large function to compile in Tier-2L Baseline JIT, stats={:?}",
        jit.stats()
    );
    assert!(
        jit.stats().baseline_entries >= 1 || jit.stats().osr_entries >= 1,
        "expected native entry into Tier-2L Baseline JIT"
    );
}

#[test]
fn deeply_recursive_large_frame_function_does_not_overflow_stack() {
    // A 72-local-variable function recursing 350 frames deep would overflow the OS stack under
    // uncoalesced register spilling; Memory-Backed Frame Baseline keeps native frames O(1) (~96B).
    let mut src = String::from("function deepFiber(n) {\n  if (n <= 0) return 1;\n");
    for i in 0..68 {
        src.push_str(&format!("  let a{i} = (n + {i}) & 7;\n"));
    }
    src.push_str("  let sub = deepFiber(n - 1);\n");
    src.push_str("  return (sub + a0 + a33 + a67) % 1000003;\n}\n");
    src.push_str("deepFiber(350);");

    let prog1 = compile(&src, SourceType::Script).expect("compile");
    let mut vm_interp = JsVm::new();
    let expected = vm_interp.execute_program(prog1).expect("interp");

    let prog2 = compile(&src, SourceType::Script).expect("compile");
    let mut vm_jit = JsVm::new();
    let jit = BaselineJit::with_thresholds(1, 1);
    vm_jit.attach_jit(jit.clone());
    let actual = vm_jit.execute_program(prog2).expect("jit deep recursion");

    assert_eq!(actual.as_number(), expected.as_number());
    assert!(jit.stats().compiled_functions >= 1);
    let min_native = if cfg!(debug_assertions) { 20 } else { 100 };
    assert!(
        jit.stats().baseline_entries >= min_native,
        "expected >= {min_native} native JIT entries, got {:?}",
        jit.stats()
    );
}
