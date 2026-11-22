//! Criterion benchmarks comparing Tier-1 Interpreter vs Tier-2 Baseline JIT vs
//! Tier-3 Speculative Optimizing JIT across property loops, array transforms, and
//! Virtual DOM tree diffing (Spec 05 §6 & Issues #19–#20).

use criterion::{criterion_group, criterion_main, Criterion};
use rix_jit::{BaselineJit, TieredJit};
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

function arrayTransform(arr) {
    let acc = 0;
    let i = 0;
    while (i < arr.length) {
        const v = (arr[i] * 3 + 1) | 0;
        arr[i] = v;
        acc = (acc + v) | 0;
        i = i + 1;
    }
    return acc;
}

function makeArray(n) {
    const a = [];
    for (let i = 0; i < n; i++) a.push(i & 1023);
    return a;
}

function nodeHash(node) {
    return ((node.tag * 31 + node.key * 17 + node.flags) | 0);
}

function buildVdomTree(depth, seed) {
    if (depth === 0) {
        return { tag: 1, key: seed, flags: seed & 7, left: null, right: null };
    }
    return {
        tag: 2,
        key: seed,
        flags: depth,
        left: buildVdomTree(depth - 1, seed * 2 + 1),
        right: buildVdomTree(depth - 1, seed * 2 + 2)
    };
}

function diffVdom(a, b) {
    if (a === null || b === null) return 1;
    let patches = 0;
    if (a.tag !== b.tag || a.key !== b.key) patches = patches + 1;
    if (a.flags !== b.flags) patches = patches + 1;
    patches = (patches + (nodeHash(a) ^ nodeHash(b))) | 0;
    if (a.left !== null && b.left !== null) {
        patches = (patches + diffVdom(a.left, b.left)) | 0;
    }
    if (a.right !== null && b.right !== null) {
        patches = (patches + diffVdom(a.right, b.right)) | 0;
    }
    return patches;
}
"#;

fn setup_tier(tier: u8, fn_name: &str, warm_args_expr: &str, warm_calls: usize) -> (JsVm, JsValue) {
    let mut vm = JsVm::new();
    match tier {
        1 => {}
        2 => {
            let j = BaselineJit::with_tier_thresholds(32, 256, u32::MAX);
            vm.attach_jit(j);
        }
        _ => {
            let j = TieredJit::attach_to(&mut vm);
            let _ = j;
        }
    }
    vm.eval_script(BENCH_FUNCTIONS_SRC).expect("eval");
    let warm_script =
        format!("for (let __w = 0; __w < {warm_calls}; __w++) {{ {fn_name}({warm_args_expr}); }}");
    vm.eval_script(&warm_script).expect("warmup");
    let key = vm.heap.intern_key(fn_name);
    let func_val = vm.get_property_by_key(vm.global, key).expect("lookup fn");
    (vm, func_val)
}

fn bench_prop_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1_vs_tier2_vs_tier3_prop_loop");
    for (tier, label) in [
        (1, "tier1_interpreter"),
        (2, "tier2_baseline"),
        (3, "tier3_optimizing"),
    ] {
        let (mut vm, f) = setup_tier(tier, "propLoop", "10", 1_050);
        group.bench_function(label, |b| {
            b.iter(|| {
                vm.call_value(f, JsValue::UNDEFINED, &[JsValue::from_i32(100_000)])
                    .expect("call")
            });
        });
    }
    group.finish();
}

fn bench_array_transform(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1_vs_tier2_vs_tier3_array_transform");
    for (tier, label) in [
        (1, "tier1_interpreter"),
        (2, "tier2_baseline"),
        (3, "tier3_optimizing"),
    ] {
        let (mut vm, f) = setup_tier(tier, "arrayTransform", "[1, 2, 3, 4]", 1_050);
        let arr = vm.eval_script("makeArray(4096)").expect("makeArray");
        group.bench_function(label, |b| {
            b.iter(|| vm.call_value(f, JsValue::UNDEFINED, &[arr]).expect("call"));
        });
    }
    group.finish();
}

fn bench_vdom_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1_vs_tier2_vs_tier3_vdom_diff");
    for (tier, label) in [
        (1, "tier1_interpreter"),
        (2, "tier2_baseline"),
        (3, "tier3_optimizing"),
    ] {
        let mut vm = JsVm::new();
        match tier {
            1 => {}
            2 => {
                let j = BaselineJit::with_tier_thresholds(32, 256, u32::MAX);
                vm.attach_jit(j);
            }
            _ => {
                let _ = TieredJit::attach_to(&mut vm);
            }
        }
        vm.eval_script(BENCH_FUNCTIONS_SRC).expect("eval");
        vm.eval_script(
            r#"
            const __tA = buildVdomTree(9, 1);
            const __tB = buildVdomTree(9, 2);
            for (let __w = 0; __w < 10; __w++) diffVdom(__tA, __tB);
            "#,
        )
        .expect("warmup");
        let k_diff = vm.heap.intern_key("diffVdom");
        let f_diff = vm.get_property_by_key(vm.global, k_diff).expect("diffVdom");
        let k_a = vm.heap.intern_key("__tA");
        let t_a = vm.get_property_by_key(vm.global, k_a).expect("__tA");
        let k_b = vm.heap.intern_key("__tB");
        let t_b = vm.get_property_by_key(vm.global, k_b).expect("__tB");
        group.bench_function(label, |b| {
            b.iter(|| {
                vm.call_value(f_diff, JsValue::UNDEFINED, &[t_a, t_b])
                    .expect("diff")
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_prop_loop,
    bench_array_transform,
    bench_vdom_diff
);
criterion_main!(benches);
