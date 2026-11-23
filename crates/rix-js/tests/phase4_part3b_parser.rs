//! Phase 4 Part 3B ECMAScript parser static early-error conformance tests:
//! `AllPrivateNamesValid`, parenthesized/private `delete`, strict destructuring,
//! block/switch `var` vs lexical collisions, class early errors, Annex B §B.3.4/§B.3.5,
//! and ES2025 RegExp modifier groups.

use rix_js::ast::SourceType;
use rix_js::compiler::compile;
use rix_js::parser::parse;

fn assert_syntax_err(src: &str, mode: SourceType) {
    assert!(
        compile(src, mode).is_err(),
        "expected SyntaxError for: {src}"
    );
}

fn assert_valid(src: &str, mode: SourceType) {
    compile(src, mode).unwrap_or_else(|e| panic!("expected valid JS for `{src}`, got: {e}"));
}

#[test]
fn private_names_and_delete_early_errors() {
    assert_syntax_err("{ (() => this)().#x }", SourceType::Script);
    assert_syntax_err(
        "class C { #x; m() { return this.#y; } }",
        SourceType::Script,
    );
    assert_syntax_err(
        "class C { #x; m() { return delete (this.#x); } }",
        SourceType::Script,
    );
    assert_syntax_err(
        "class C { #x; m() { return delete ((this.#x)); } }",
        SourceType::Script,
    );
    assert_syntax_err("'use strict'; var x = 1; delete (x);", SourceType::Script);

    // Forward reference to private field declared later in same class is valid.
    assert_valid(
        "class C { m() { return this.#x; } #x = 1; }",
        SourceType::Script,
    );
    assert_valid(
        "class C { #x = 1; has(o) { return #x in o; } }",
        SourceType::Script,
    );
}

#[test]
fn scoping_redeclaration_and_annex_b_grammar() {
    assert_syntax_err(
        "switch (0) { case 1: var f; default: async function f() {} }",
        SourceType::Script,
    );
    assert_syntax_err("{ var x; let x; }", SourceType::Script);
    assert_syntax_err("function f(a) { let a; }", SourceType::Script);
    assert_syntax_err("const g = (a) => { let a; };", SourceType::Script);
    assert_syntax_err("try {} catch ({ e }) { var e; }", SourceType::Script);
    assert_syntax_err("'use strict'; 0, { eval = 0 } = {};", SourceType::Script);
    assert_syntax_err("({ a = 1 });", SourceType::Script);
    assert_syntax_err(
        "({ __proto__: null, __proto__: null });",
        SourceType::Script,
    );
    assert_syntax_err("let [...a,] = [];", SourceType::Script);
    assert_syntax_err("let {...a,} = {};", SourceType::Script);

    // Annex B §B.3.4 / §B.3.5 valid sloppy forms:
    assert_valid("let f = 1; if (true) function f() {}", SourceType::Script);
    assert_valid("try {} catch (e) { var e; }", SourceType::Script);
    assert_valid(
        "({ __proto__: null, ['__proto__']: 1 });",
        SourceType::Script,
    );
}

#[test]
fn class_and_regexp_early_errors() {
    assert_syntax_err("class C { #constructor() {} }", SourceType::Script);
    assert_syntax_err("class C { static prototype() {} }", SourceType::Script);
    assert_syntax_err("class C { constructor = 1; }", SourceType::Script);
    assert_syntax_err("class C { #x; #x; }", SourceType::Script);
    assert_syntax_err("class C { x = arguments; }", SourceType::Script);
    assert_syntax_err("class C { static { arguments; } }", SourceType::Script);
    assert_syntax_err("({ get x(a) {} });", SourceType::Script);
    assert_syntax_err("({ set x() {} });", SourceType::Script);
    assert_syntax_err(
        "export const a = 1; export const a = 2;",
        SourceType::Module,
    );
    assert_syntax_err("/a/gg;", SourceType::Script);
    assert_syntax_err("/(?ii:a)/;", SourceType::Script);

    assert!(parse("/(?i-m:abc)/u;", SourceType::Script).is_ok());
}
