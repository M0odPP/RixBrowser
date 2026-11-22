//! Criterion benchmarks comparing Tier-1 Bytecode Interpreter vs Tier-2 Baseline Cranelift JIT.
//!
//! Reference: Spec 05 §4.3 & Issue #19 (acceptance criterion: >= 3x speedup on hot loops
//! and property-heavy code).

use criterion::{criterion_group, criterion_main, Criterion};
use rix_jit::BaselineJit;
use rix_js::{JsValue, JsVm};

const BENCH_FUNCTIONS_SRC: &str = r#"
function sumLoop(n) {
    let acc = 0;
    let i = 0;
    while (i < n) {
        acc = (acc + i * 3 - (i >> 1)) | 0;
        i = i + 1;
    }
    return acc;
}

function dotLoop(n) {
    let acc = 0.5;
    let i = 1;
    while (i <= n) {
        acc = acc + (i * 0.25) - (i * 0.125);
        i = i + 1;
    }
    return acc;
}

function propLoop(n) {
    const obj = { a: 1, b: 2, c: 3 };
    let total = 0;
    let i = 0;
    while (i < n) {
        obj.a = (obj.a + obj.b + obj.c) | 0;
        total = (total + obj.a) | 0;
        i = i + 1;
    }
    return total;
}
"#;

fn setup_vm(with_jit: bool, fn_name: &str) -> (JsVm, JsValue) {
    let mut vm = JsVm::new();
    if with_jit {
        let _jit = BaselineJit::attach_to(&mut vm);
    }
    vm.eval_script(BENCH_FUNCTIONS_SRC).expect("eval");
    let key = vm.heap.intern_key(fn_name);
    let func_val = vm.get_property_by_key(vm.global, key).expect("lookup fn");
    // Warm up past BASELINE_THRESHOLD / OSR_THRESHOLD before steady-state measurement.
    for _ in 0..40 {
        vm.call_value(func_val, JsValue::UNDEFINED, &[JsValue::from_i32(10)])
            .expect("warmup");
    }
    (vm, func_val)
}

fn bench_hot_int_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1_vs_tier2_int_loop");
    let (mut vm1, fn1) = setup_vm(false, "sumLoop");
    group.bench_function("tier1_interpreter", |b| {
        b.iter(|| {
            vm1.call_value(fn1, JsValue::UNDEFINED, &[JsValue::from_i32(100_000)])
                .expect("call")
        });
    });
    let (mut vm2, fn2) = setup_vm(true, "sumLoop");
    group.bench_function("tier2_baseline_jit", |b| {
        b.iter(|| {
            vm2.call_value(fn2, JsValue::UNDEFINED, &[JsValue::from_i32(100_000)])
                .expect("call")
        });
    });
    group.finish();
}

fn bench_hot_float_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1_vs_tier2_float_loop");
    let (mut vm1, fn1) = setup_vm(false, "dotLoop");
    group.bench_function("tier1_interpreter", |b| {
        b.iter(|| {
            vm1.call_value(fn1, JsValue::UNDEFINED, &[JsValue::from_i32(100_000)])
                .expect("call")
        });
    });
    let (mut vm2, fn2) = setup_vm(true, "dotLoop");
    group.bench_function("tier2_baseline_jit", |b| {
        b.iter(|| {
            vm2.call_value(fn2, JsValue::UNDEFINED, &[JsValue::from_i32(100_000)])
                .expect("call")
        });
    });
    group.finish();
}

fn bench_hot_prop_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1_vs_tier2_prop_loop");
    let (mut vm1, fn1) = setup_vm(false, "propLoop");
    group.bench_function("tier1_interpreter", |b| {
        b.iter(|| {
            vm1.call_value(fn1, JsValue::UNDEFINED, &[JsValue::from_i32(50_000)])
                .expect("call")
        });
    });
    let (mut vm2, fn2) = setup_vm(true, "propLoop");
    group.bench_function("tier2_baseline_jit", |b| {
        b.iter(|| {
            vm2.call_value(fn2, JsValue::UNDEFINED, &[JsValue::from_i32(50_000)])
                .expect("call")
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_hot_int_loop,
    bench_hot_float_loop,
    bench_hot_prop_loop
);
criterion_main!(benches);
