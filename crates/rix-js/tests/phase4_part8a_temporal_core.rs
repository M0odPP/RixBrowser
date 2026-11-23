//! Spec regression tests for Phase 4 Part 8A:
//! `Temporal` namespace, `Temporal.Now`, `Temporal.Instant`, `Temporal.Duration`,
//! and `Date.prototype.toTemporalInstant`.

use rix_js::{JsValue, JsVm};

fn eval(src: &str) -> JsValue {
    let mut vm = JsVm::new();
    vm.eval_script(src)
        .unwrap_or_else(|e| panic!("eval failed: {e:?}\nsource:\n{src}"))
}

fn eval_bool(src: &str) -> bool {
    eval(src).as_bool().expect("expected boolean result")
}

#[test]
fn temporal_namespace_now_and_date_to_temporal_instant() {
    assert!(eval_bool(
        r#"
        const tagOk = Object.prototype.toString.call(Temporal) === "[object Temporal]"
            && Object.prototype.toString.call(Temporal.Now) === "[object Temporal.Now]";
        const tz = Temporal.Now.timeZoneId();
        const nowInst = Temporal.Now.instant();
        const d = new Date(1700000000123);
        const di = d.toTemporalInstant();
        let invThrew = false;
        try { new Date(NaN).toTemporalInstant(); } catch (e) { invThrew = e instanceof RangeError; }
        let nonDateThrew = false;
        try { Date.prototype.toTemporalInstant.call({}); } catch (e) { nonDateThrew = e instanceof TypeError; }
        tagOk
            && typeof tz === "string" && tz.length > 0
            && nowInst instanceof Temporal.Instant
            && di.epochMilliseconds === 1700000000123
            && di.epochNanoseconds === 1700000000123000000n
            && invThrew && nonDateThrew;
        "#
    ));
}

#[test]
fn temporal_instant_full_api() {
    assert!(eval_bool(
        r#"
        const a = Temporal.Instant.from("2024-03-15T14:30:45.123456789Z");
        const b = a.add({ hours: 1, minutes: 30, nanoseconds: 1 });
        const diff = a.until(b, { largestUnit: "hour" });
        const rounded = a.round({ smallestUnit: "second", roundingMode: "halfExpand" });
        const str = a.toString({ fractionalSecondDigits: 3, timeZone: "+02:00" });
        let valueOfThrew = false;
        try { a.valueOf(); } catch (e) { valueOfThrew = e instanceof TypeError; }
        let noNewThrew = false;
        try { Temporal.Instant(0n); } catch (e) { noNewThrew = e instanceof TypeError; }
        let numArgThrew = false;
        try { new Temporal.Instant(0); } catch (e) { numArgThrew = e instanceof TypeError; }
        let oobThrew = false;
        try { new Temporal.Instant(8640000000000000000001n); } catch (e) { oobThrew = e instanceof RangeError; }
        diff.hours === 1 && diff.minutes === 30 && diff.nanoseconds === 1
            && b.subtract({ hours: 1, minutes: 30, nanoseconds: 1 }).equals(a)
            && Temporal.Instant.compare(a, b) === -1
            && rounded.toString() === "2024-03-15T14:30:45Z"
            && str === "2024-03-15T16:30:45.123+02:00"
            && valueOfThrew && noNewThrew && numArgThrew && oobThrew;
        "#
    ));
}

#[test]
fn temporal_duration_full_api() {
    assert!(eval_bool(
        r#"
        const d1 = new Temporal.Duration(0, 0, 0, 0, 1, 30, 45, 500);
        const d2 = Temporal.Duration.from("PT45M15.5S");
        const sum = d1.add(d2);
        const neg = d1.negated();
        const abs = neg.abs();
        const rounded = sum.round({ smallestUnit: "minute", roundingMode: "ceil" });
        const totMin = sum.total("minute");
        const withMod = d1.with({ hours: 2 });
        const cmp = Temporal.Duration.compare("PT1H", "PT60M");
        const blank = new Temporal.Duration().blank;
        let valueOfThrew = false;
        try { d1.valueOf(); } catch (e) { valueOfThrew = e instanceof TypeError; }
        let mixedSignThrew = false;
        try { new Temporal.Duration(1, -1); } catch (e) { mixedSignThrew = e instanceof RangeError; }
        Temporal.Duration.length === 0
            && d1.sign === 1 && neg.sign === -1 && abs.hours === 1 && blank === true
            && sum.hours === 2 && sum.minutes === 16 && sum.seconds === 1 && sum.milliseconds === 0
            && rounded.toString() === "PT2H17M"
            && Math.abs(totMin - (136 + 1 / 60)) < 1e-12
            && withMod.hours === 2 && withMod.minutes === 30
            && cmp === 0
            && valueOfThrew && mixedSignThrew;
        "#
    ));
}
