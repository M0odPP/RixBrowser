//! Spec regression tests for ECMA-262 §21.4 Date & Annex B §B.2.4 Date built-ins.

use rix_js::{JsValue, JsVm};

fn eval(src: &str) -> JsValue {
    let mut vm = JsVm::new();
    vm.eval_script(src)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\nsrc:\n{src}"))
}

#[test]
fn date_constructor_and_utc_range() {
    let v = eval(
        r#"
        const s = Date(1, 2, 3);
        const okCall = typeof s === "string" && s.includes("GMT");
        const epoch = new Date(0);
        const clone = new Date(epoch);
        const maxD = new Date(8.64e15);
        const overD = new Date(8.64e15 + 1);
        const minD = new Date(-8.64e15);
        const utc1 = Date.UTC(99, 0, 1);
        const utcNeg = Date.UTC(-0.5, 0, 1);
        okCall
            && clone.getTime() === 0
            && maxD.toISOString() === "+275760-09-13T00:00:00.000Z"
            && Number.isNaN(overD.getTime())
            && minD.toISOString() === "-271821-04-20T00:00:00.000Z"
            && utc1 === Date.UTC(1999, 0, 1)
            && utcNeg === Date.UTC(1900, 0, 1);
        "#,
    );
    assert_eq!(v.as_bool(), Some(true));
}

#[test]
fn date_getters_setters_and_eval_order() {
    let v = eval(
        r#"
        const d = new Date(Date.UTC(2024, 1, 29, 12, 34, 56, 789));
        let ok = d.getUTCFullYear() === 2024
            && d.getUTCMonth() === 1
            && d.getUTCDate() === 29
            && d.getUTCDay() === 4
            && d.getUTCHours() === 12
            && d.getUTCMinutes() === 34
            && d.getUTCSeconds() === 56
            && d.getUTCMilliseconds() === 789;
        d.setUTCFullYear(2025, 2, 15);
        d.setUTCHours(23, 59, 58, 123);
        ok = ok && d.toISOString() === "2025-03-15T23:59:58.123Z";
        // Evaluation order: `this` time value is read before ToNumber(arg) mutates `this`.
        const d2 = new Date(Date.UTC(2020, 0, 1));
        d2.setUTCDate({ valueOf() { d2.setTime(NaN); return 10; } });
        ok = ok && d2.getUTCDate() === 10 && d2.getUTCFullYear() === 2020;
        // setFullYear recovers from NaN as +0.
        const inv = new Date(NaN);
        inv.setUTCFullYear(2030);
        ok = ok && inv.toISOString() === "2030-01-01T00:00:00.000Z";
        ok;
        "#,
    );
    assert_eq!(v.as_bool(), Some(true));
}

#[test]
fn date_parse_formatting_to_primitive_and_annexb() {
    let v = eval(
        r#"
        const d = new Date("2025-06-15T08:30:45.123Z");
        const utcRoundTrip = Date.parse(d.toUTCString());
        const strRoundTrip = Date.parse(d.toString());
        const isoDateOnly = Date.parse("1970-01-01");
        const isoExtNeg = Date.parse("-000001-01-01T00:00:00.000Z");
        const negZeroYear = Date.parse("-000000-01-01");
        const invalidJson = new Date(NaN).toJSON();
        const validJson = d.toJSON();
        const primDef = d[Symbol.toPrimitive]("default");
        const primNum = d[Symbol.toPrimitive]("number");
        const gmtAlias = Date.prototype.toGMTString === Date.prototype.toUTCString;
        const dYr = new Date(Date.UTC(1998, 5, 15, 12));
        const yrBefore = dYr.getYear();
        dYr.setYear(95);
        const yrAfter = dYr.getFullYear();
        utcRoundTrip === Math.floor(d.getTime() / 1000) * 1000
            && strRoundTrip === Math.floor(d.getTime() / 1000) * 1000
            && isoDateOnly === 0
            && Number.isFinite(isoExtNeg)
            && Number.isNaN(negZeroYear)
            && invalidJson === null
            && validJson === "2025-06-15T08:30:45.123Z"
            && typeof primDef === "string"
            && primNum === d.getTime()
            && gmtAlias
            && yrBefore === 98
            && yrAfter === 1995;
        "#,
    );
    assert_eq!(v.as_bool(), Some(true));
}
