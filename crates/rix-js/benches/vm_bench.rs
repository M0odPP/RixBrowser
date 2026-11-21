use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rix_js::{compile, JsVm, SourceType};

fn bench_monomorphic_property_loop(c: &mut Criterion) {
    let source = r#"
        function runLoop(n) {
            const obj = { a: 1, b: 2, c: 3 };
            let sum = 0;
            for (let i = 0; i < n; i++) {
                obj.a = i;
                sum += obj.a + obj.b + obj.c;
            }
            return sum;
        }
        runLoop(10000);
    "#;
    let compiled = compile(source, SourceType::Script).expect("compile");

    c.bench_function("tier1_vm_monomorphic_property_loop_10k", |b| {
        b.iter(|| {
            let mut vm = JsVm::new();
            let res = vm.execute_program(compiled.clone()).expect("exec");
            black_box(res)
        });
    });
}

fn bench_vdom_tree_diffing(c: &mut Criterion) {
    let source = r#"
        function h(tag, key, text, children) {
            return { tag, key, text, children };
        }
        function buildTree(depth, seed) {
            if (depth === 0) {
                return h("span", seed, "leaf-" + seed, []);
            }
            const children = [];
            for (let i = 0; i < 3; i++) {
                children.push(buildTree(depth - 1, seed * 3 + i));
            }
            return h("div", seed, "", children);
        }
        function diff(a, b) {
            let patches = 0;
            if (a.tag !== b.tag || a.key !== b.key) return 1;
            if (a.text !== b.text) patches++;
            const len = a.children.length < b.children.length
                ? a.children.length
                : b.children.length;
            patches += a.children.length > b.children.length
                ? a.children.length - b.children.length
                : b.children.length - a.children.length;
            for (let i = 0; i < len; i++) {
                patches += diff(a.children[i], b.children[i]);
            }
            return patches;
        }
        const t1 = buildTree(4, 1);
        const t2 = buildTree(4, 2);
        diff(t1, t2);
    "#;
    let compiled = compile(source, SourceType::Script).expect("compile");

    c.bench_function("tier1_vm_vdom_tree_diffing", |b| {
        b.iter(|| {
            let mut vm = JsVm::new();
            let res = vm.execute_program(compiled.clone()).expect("exec");
            black_box(res)
        });
    });
}

criterion_group!(
    benches,
    bench_monomorphic_property_loop,
    bench_vdom_tree_diffing
);
criterion_main!(benches);
