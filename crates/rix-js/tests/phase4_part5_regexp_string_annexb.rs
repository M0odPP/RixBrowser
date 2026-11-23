//! Phase 4 Part 5 ECMAScript conformance regression tests:
//! ES2025 `RegExp` (lookbehind, backrefs, named groups, `/d` indices, `/y` sticky,
//! `/u`/`/v` unicodeSets, inline modifiers, `RegExp.escape`, Symbol.{match,matchAll,replace,search,split}),
//! `String` (UTF-16 semantics, `matchAll`, `raw`, `normalize`, `trimLeft`/`trimRight`,
//! `isWellFormed`/`toWellFormed`, `GetSubstitution`), and Annex B built-ins
//! (`escape`/`unescape`, `RegExp.prototype.compile`, HTML wrappers, `Object.prototype.__proto__` & `__defineGetter__`).

use rix_js::JsVm;

fn eval_str(src: &str) -> String {
    let mut vm = JsVm::new();
    let v = vm.eval_script(src).expect("script must succeed");
    vm.drain_microtasks();
    vm.value_to_rust_string(v)
}

#[test]
fn regexp_es2025_engine_features_and_symbols() {
    let out = eval_str(
        r#"
        const m = /(?<=@)(?<user>[a-z]+)\1/d.exec("hi @booboo!");
        const modMatch = /(?i:abc)def/.test("ABCdef") && !/(?i:abc)def/.test("ABCDEF");
        const vMatch = /^[\p{ASCII_Hex_Digit}--[a-f]]+$/v.test("09AF");
        const dup = /(?<x>a)|(?<x>b)/.exec("b").groups.x;
        const sticky = /a/y; sticky.lastIndex = 1;
        const s1 = sticky.test("ba");
        const s2 = sticky.lastIndex;
        const ma = [...("a1b2".matchAll(/([a-z])(\d)/g))].map(r => r[1] + ":" + r[2]).join(",");
        const rep = "2026-11-23".replace(/(?<y>\d{4})-(?<m>\d{2})-(?<d>\d{2})/, "$<d>/$<m>/$<y>");
        const esc = RegExp.escape("a.b+c?");
        const re = /foo/i;
        const same = RegExp(re) === re && new RegExp(re) !== re;
        [
            m[0], m.groups.user, m.indices[0].join("-"), m.indices.groups.user.join("-"),
            modMatch, vMatch, dup, s1, s2, ma, rep, esc, same,
            RegExp.prototype.source, /a\/b/gm.flags,
            RegExp.$1, RegExp.lastMatch,
        ].join("|");
        "#,
    );
    assert_eq!(
        out,
        "booboo|boo|4-10|4-7|true|true|b|true|2|a:1,b:2|23/11/2026|\\x61\\.b\\+c\\?|true|(?:)|gm|2026|2026-11-23"
    );
}

#[test]
fn string_utf16_normalize_and_annexb_builtins() {
    let out = eval_str(
        r#"
        const nfc = "\u0065\u0301".normalize("NFC");
        const nfd = "\u00e9".normalize("NFD");
        const raw = String.raw({ raw: ["a", "b", "c"] }, 1, 2);
        const trimBom = "\uFEFF  hi \u2028".trim();
        const sameTrim = String.prototype.trimLeft === String.prototype.trimStart
            && String.prototype.trimRight === String.prototype.trimEnd;
        const html = "hi".anchor('a"b').bold();
        const esc = escape("a b+c\u0100");
        const unesc = unescape(esc);
        const obj = {};
        obj.__defineGetter__("x", () => 77);
        const protoTest = {};
        const parent = { inherited: 9 };
        protoTest.__proto__ = parent;
        const re = /a/;
        re.compile("b", "i");
        [
            nfc.length, nfd.length, raw, trimBom, sameTrim,
            html, esc, unesc,
            obj.x, typeof obj.__lookupGetter__("x"), protoTest.inherited,
            re.test("B"),
            String(1e21), String(1e-7),
            "😀".length, "😀".codePointAt(0),
            "abc".replace("b", "[$&|$`|$'|$$]"),
        ].join("|");
        "#,
    );
    assert_eq!(
        out,
        "1|2|a1b2c|hi|true|<b><a name=\"a&quot;b\">hi</a></b>|a%20b+c%u0100|a b+c\u{0100}|77|function|9|true|1e+21|1e-7|2|128512|a[b|a|c|$]c"
    );
}
