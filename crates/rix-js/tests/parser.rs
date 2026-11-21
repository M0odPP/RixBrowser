use rix_js::ast::*;
use rix_js::{parse, SourceType};

#[test]
fn parses_classes_private_fields_static_blocks_and_inheritance() {
    let src = r#"
        class Animal {
            static kingdom = "Animalia";
            #secret = 42;
            static {
                this.ready = true;
            }
            constructor(name) {
                this.name = name;
            }
            get label() { return this.name; }
            set label(v) { this.name = v; }
            hasSecret(other) { return #secret in other; }
            async *stream() { yield await this.name; }
        }
        class Dog extends Animal {
            constructor(name, breed) {
                super(name);
                this.breed = breed;
            }
            speak() { return super.label + "!"; }
        }
    "#;
    let prog = parse(src, SourceType::Script).expect("class syntax should parse");
    assert_eq!(prog.body.len(), 2);
    match &prog.body[0] {
        Statement::ClassDecl(cls) => {
            assert_eq!(cls.name.as_deref(), Some("Animal"));
            assert_eq!(cls.elements.len(), 8);
            assert!(matches!(cls.elements[2], ClassElement::StaticBlock { .. }));
        }
        other => panic!("expected ClassDecl, got {other:?}"),
    }
    match &prog.body[1] {
        Statement::ClassDecl(cls) => {
            assert_eq!(cls.name.as_deref(), Some("Dog"));
            assert!(cls.super_class.is_some());
        }
        other => panic!("expected ClassDecl, got {other:?}"),
    }
}

#[test]
fn parses_destructuring_rest_spread_and_arrow_functions() {
    let src = r#"
        const [a, , b = 10, ...tail] = [1, 2, 3, ...more];
        const { x, y: renamed = 20, [key]: computed, ...restObj } = { x: 1, ...base };
        ({ a, b } = { a: 5, b: 6 });
        const fn1 = x => x * 2;
        const fn2 = ({ a = 1 }, [b, ...c]) => ({ a, b, c });
        const fn3 = async (url, ...opts) => await fetch(url, ...opts);
        for (const [k, v] of entries) {
            sum += v;
        }
    "#;
    let prog = parse(src, SourceType::Script).expect("destructuring & arrows should parse");
    assert_eq!(prog.body.len(), 7);
}

#[test]
fn parses_optional_chaining_nullish_exponentiation_regex_and_nested_templates() {
    let src = r#"
        const v = a?.b?.[c]?.(1, 2) ?? fallback;
        const p = 2 ** 3 ** 2;
        const grouped = (a ?? b) || c;
        const re = /foo[a/b]+/gi;
        const div = x / 2 / y;
        const nested = `outer ${`inner ${v + 1}`} end`;
        const tagged = html`<div class="${cls}">\unicode-raw-ok</div>`;
        let z = 1
        z++
        const after = z
    "#;
    let prog = parse(src, SourceType::Script).expect("operators, regex, templates should parse");
    assert_eq!(prog.body.len(), 10);

    // Verify right-associativity of `**`: `2 ** (3 ** 2)`.
    if let Statement::VarDecl { decls, .. } = &prog.body[1] {
        let init = decls[0].init.as_ref().unwrap();
        match &init.kind {
            ExprKind::Binary {
                op: BinaryOp::Exp,
                left,
                right,
            } => {
                assert!(matches!(left.kind, ExprKind::Number(n) if n == 2.0));
                assert!(matches!(
                    right.kind,
                    ExprKind::Binary {
                        op: BinaryOp::Exp,
                        ..
                    }
                ));
            }
            other => panic!("expected right-assoc Exp, got {other:?}"),
        }
    } else {
        panic!("expected VarDecl");
    }

    // Verify regex vs division disambiguation.
    if let Statement::VarDecl { decls, .. } = &prog.body[3] {
        assert!(matches!(
            decls[0].init.as_ref().unwrap().kind,
            ExprKind::RegExp { .. }
        ));
    }
    if let Statement::VarDecl { decls, .. } = &prog.body[4] {
        assert!(matches!(
            decls[0].init.as_ref().unwrap().kind,
            ExprKind::Binary {
                op: BinaryOp::Div,
                ..
            }
        ));
    }
}

#[test]
fn parses_async_await_generators_and_es_modules_with_top_level_await() {
    let src = r#"
        import defaultExport, { alpha as a, beta } from "./mod.js";
        import * as ns from "./ns.js";
        import "./polyfill.js";

        const loaded = await import(import.meta.url);
        for await (const chunk of loaded.stream()) {
            process(chunk);
        }

        export async function* gen(items) {
            for (const item of items) {
                yield* transform(await item);
            }
        }
        export { a as exportedA, ns };
        export * as reexported from "./other.js";
        export default class Widget {
            render() { return defaultExport + beta; }
        }
    "#;
    let prog = parse(src, SourceType::Module).expect("ES module with top-level await should parse");
    assert!(prog.strict);
    assert_eq!(prog.body.len(), 9);
}

#[test]
fn enforces_strict_mode_and_es2024_early_errors() {
    let invalid_scripts = [
        // Duplicate lexical declaration in same block.
        "let x = 1; let x = 2;",
        // Const without initializer.
        "const c;",
        // Mixing `??` with `||` or `&&` without parentheses.
        "a ?? b || c;",
        "a && b ?? c;",
        // Unary on LHS of `**` without parentheses.
        "-2 ** 2;",
        // Strict mode forbids `with`, `delete ident`, octal literal, binding/assigning `eval`.
        "\"use strict\"; with (obj) {}",
        "\"use strict\"; let x = 1; delete x;",
        "\"use strict\"; const n = 077;",
        "\"use strict\"; let eval = 1;",
        "\"use strict\"; arguments = 2;",
        // Strict mode & non-simple params reject duplicate parameter names and illegal directive.
        "function f(a, a) { \"use strict\"; }",
        "function f(a = 1) { \"use strict\"; }",
        "const g = (x, x) => x;",
        // Control flow & context early errors.
        "return 1;",
        "break;",
        "continue;",
        "outer: { continue outer; }",
        "function f() { yield 1; }",
        "import x from 'y';",
        "export const x = 1;",
        "class C { constructor() {} constructor() {} }",
        "class C { async constructor() {} }",
        // Illegal newline after `throw`.
        "throw\nnew Error();",
        // Untagged template literal with invalid escape sequence.
        "`\\unicode`",
    ];

    for bad in invalid_scripts {
        let res = parse(bad, SourceType::Script);
        assert!(
            res.is_err(),
            "expected SyntaxError for invalid script: {bad}"
        );
    }

    // `return` with newline triggers ASI (returns undefined, next line is expr stmt).
    let asi = parse("function f() { return\n42; }", SourceType::Script).unwrap();
    if let Statement::FunctionDecl(f) = &asi.body[0] {
        assert_eq!(f.body.len(), 2);
        assert!(matches!(f.body[0], Statement::Return { arg: None, .. }));
    } else {
        panic!("expected FunctionDecl");
    }
}
