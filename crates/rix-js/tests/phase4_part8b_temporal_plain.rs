//! Spec regression tests for Phase 4 Part 8B:
//! `Temporal.PlainDate`, `Temporal.PlainTime`, `Temporal.PlainDateTime`,
//! `Temporal.PlainYearMonth`, `Temporal.PlainMonthDay`, and `super(...spread)`.

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
fn temporal_plain_date_and_non_iso_calendars() {
    assert!(eval_bool(
        r#"
        const d = Temporal.PlainDate.from("2024-02-29");
        const next = d.add({ years: 1 }); // constrained to 2025-02-28
        let rejectThrew = false;
        try { d.add({ years: 1 }, { overflow: "reject" }); } catch (e) { rejectThrew = e instanceof RangeError; }
        const diff = d.until(next, { largestUnit: "day" });
        const jp = d.withCalendar("japanese");
        const parsedCal = Temporal.PlainDate.from({ year: 2024, month: 3, day: 15, calendar: "2024-01-01[u-ca=iso8601]" });
        d.inLeapYear === true
            && d.daysInMonth === 29 && d.daysInYear === 366
            && next.toString() === "2025-02-28"
            && rejectThrew
            && diff.days === 365
            && jp.era === "reiwa" && jp.eraYear === 6
            && parsedCal.toString() === "2024-03-15";
        "#
    ));
}

#[test]
fn temporal_plain_time_and_plain_date_time_with_super_spread() {
    assert!(eval_bool(
        r#"
        const t = new Temporal.PlainTime(12, 34, 56, 987, 654, 321);
        const t2 = t.add({ hours: 2, minutes: 30 });
        const tr = t.round({ smallestUnit: "minute", roundingMode: "halfExpand" });
        const dt = Temporal.PlainDateTime.from("2024-06-15T23:45:00");
        const dt2 = dt.add({ hours: 2 });
        const dtRound = dt.round("hour");

        // Subclass super(...args) spread forwarding
        class MyTime extends Temporal.PlainTime {
            constructor(...args) { super(...args); }
        }
        const sub = new MyTime(1, 2, 3);
        const subAdded = sub.add({ seconds: 4 });

        t2.toString() === "15:04:56.987654321"
            && tr.toString() === "12:35:00"
            && dt2.toString() === "2024-06-16T01:45:00"
            && dtRound.toString() === "2024-06-16T00:00:00"
            && sub instanceof MyTime && sub.hour === 1 && sub.second === 3
            && Object.getPrototypeOf(subAdded) === Temporal.PlainTime.prototype
            && subAdded.second === 7;
        "#
    ));
}

#[test]
fn temporal_plain_year_month_and_plain_month_day() {
    assert!(eval_bool(
        r#"
        const ym = Temporal.PlainYearMonth.from("2024-02");
        const ymNext = ym.add({ months: 1 });
        const ymDate = ym.toPlainDate({ day: 29 });
        const md = Temporal.PlainMonthDay.from("02-29");
        const mdDate = md.toPlainDate({ year: 2024 });
        const mdWith = md.with({ day: 15 });
        ym.daysInMonth === 29 && ym.inLeapYear === true
            && ymNext.toString() === "2024-03"
            && ymDate.toString() === "2024-02-29"
            && md.monthCode === "M02" && md.day === 29
            && mdDate.toString() === "2024-02-29"
            && mdWith.toString() === "02-15";
        "#
    ));
}
