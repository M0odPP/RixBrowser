//! Spec regression tests for Phase 4 Part 8C:
//! `Temporal.ZonedDateTime` (IANA TZDB, DST disambiguation, transitions,
//! offset handling, non-ISO calendars, arithmetic, rounding, formatting).

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
fn temporal_zdt_iana_dst_disambiguation_and_transitions() {
    assert!(eval_bool(
        r#"
        // US spring-forward gap: 2024-03-10 02:30 does not exist in America/New_York
        const gap = Temporal.ZonedDateTime.from(
            { year: 2024, month: 3, day: 10, hour: 2, minute: 30, timeZone: "America/New_York" },
            { disambiguation: "compatible" }
        );
        let gapRejectThrew = false;
        try {
            Temporal.ZonedDateTime.from(
                { year: 2024, month: 3, day: 10, hour: 2, minute: 30, timeZone: "America/New_York" },
                { disambiguation: "reject" }
            );
        } catch (e) { gapRejectThrew = e instanceof RangeError; }

        // US fall-back repeated hour: 2024-11-03 01:30 occurs twice
        const earlier = Temporal.ZonedDateTime.from(
            { year: 2024, month: 11, day: 3, hour: 1, minute: 30, timeZone: "America/New_York" },
            { disambiguation: "earlier" }
        );
        const later = Temporal.ZonedDateTime.from(
            { year: 2024, month: 11, day: 3, hour: 1, minute: 30, timeZone: "America/New_York" },
            { disambiguation: "later" }
        );

        const nextTr = gap.getTimeZoneTransition("next");
        const prevTr = gap.getTimeZoneTransition("previous");
        const utc = Temporal.ZonedDateTime.from("2024-03-10T00:00:00Z[UTC]");

        gap.hour === 3 && gap.offset === "-04:00" && gap.hoursInDay === 23
            && gapRejectThrew
            && earlier.offset === "-04:00" && later.offset === "-05:00"
            && earlier.until(later).hours === 1
            && earlier.hoursInDay === 25
            && nextTr !== null && nextTr.month === 11 && nextTr.day === 3
            && prevTr !== null && prevTr.month === 3 && prevTr.day === 10
            && utc.getTimeZoneTransition("next") === null;
        "#
    ));
}

#[test]
fn temporal_zdt_arithmetic_rounding_conversions_and_calendars() {
    assert!(eval_bool(
        r#"
        const z = Temporal.ZonedDateTime.from("2025-03-01T11:16:10Z[America/Chicago]");
        const zurich = z.withTimeZone("Europe/Zurich");
        const sod = zurich.startOfDay();
        const jp = zurich.withCalendar("japanese");
        const rounded = zurich.round({ smallestUnit: "hour", roundingMode: "halfExpand" });
        const added = zurich.add({ days: 2, hours: 3 });
        const diff = zurich.until(added, { largestUnit: "hour" });
        const backInst = zurich.toInstant();
        const str = zurich.toString({ timeZoneName: "never", offset: "auto" });

        z.hour === 5 && zurich.hour === 12
            && z.epochNanoseconds === zurich.epochNanoseconds
            && z.toPlainDate().toString() === "2025-03-01"
            && zurich.toPlainTime().toString() === "12:16:10"
            && sod.hour === 0 && sod.minute === 0 && sod.second === 0
            && jp.era === "reiwa" && jp.eraYear === 7
            && rounded.hour === 12 && rounded.minute === 0
            && diff.hours === 51
            && backInst.equals(z.toInstant())
            && str === "2025-03-01T12:16:10+01:00";
        "#
    ));
}
