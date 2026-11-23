//! ECMA-262 §21.4 Date Objects & Annex B §B.2.4 legacy Date methods.
//!
//! Uses exact proleptic Gregorian calendar arithmetic across the full ECMAScript
//! time range `±8.64e15` ms (`-271821-04-20` .. `+275760-09-13`) and bundled IANA
//! TZDB via `jiff` for local timezone offsets, DST transitions, and zone names.

use std::time::{SystemTime, UNIX_EPOCH};

use jiff::civil::DateTime as JiffDateTime;
use jiff::tz::TimeZone as JiffTimeZone;
use jiff::Timestamp;

use crate::heap::{JsObject, ObjectKind};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr, PrimHint};
use crate::vm_builtins::{define_global, define_method_len};

const MS_PER_SECOND: f64 = 1_000.0;
const MS_PER_MINUTE: f64 = 60_000.0;
const MS_PER_HOUR: f64 = 3_600_000.0;
const MS_PER_DAY: f64 = 86_400_000.0;
const MS_PER_DAY_I64: i64 = 86_400_000;
const MAX_TIME_VALUE: f64 = 8.64e15;

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[inline]
fn arg0(args: &[JsValue]) -> JsValue {
    args.first().copied().unwrap_or(JsValue::UNDEFINED)
}

pub fn install_date(vm: &mut JsVm) {
    let dp = vm.date_proto;

    let methods: &[(&str, u32, NativeFnPtr)] = &[
        ("getTime", 0, date_get_time),
        ("valueOf", 0, date_get_time),
        ("getFullYear", 0, date_get_full_year),
        ("getUTCFullYear", 0, date_get_utc_full_year),
        ("getMonth", 0, date_get_month),
        ("getUTCMonth", 0, date_get_utc_month),
        ("getDate", 0, date_get_date),
        ("getUTCDate", 0, date_get_utc_date),
        ("getDay", 0, date_get_day),
        ("getUTCDay", 0, date_get_utc_day),
        ("getHours", 0, date_get_hours),
        ("getUTCHours", 0, date_get_utc_hours),
        ("getMinutes", 0, date_get_minutes),
        ("getUTCMinutes", 0, date_get_utc_minutes),
        ("getSeconds", 0, date_get_seconds),
        ("getUTCSeconds", 0, date_get_utc_seconds),
        ("getMilliseconds", 0, date_get_milliseconds),
        ("getUTCMilliseconds", 0, date_get_utc_milliseconds),
        ("getTimezoneOffset", 0, date_get_timezone_offset),
        ("setTime", 1, date_set_time),
        ("setMilliseconds", 1, date_set_milliseconds),
        ("setUTCMilliseconds", 1, date_set_utc_milliseconds),
        ("setSeconds", 2, date_set_seconds),
        ("setUTCSeconds", 2, date_set_utc_seconds),
        ("setMinutes", 3, date_set_minutes),
        ("setUTCMinutes", 3, date_set_utc_minutes),
        ("setHours", 4, date_set_hours),
        ("setUTCHours", 4, date_set_utc_hours),
        ("setDate", 1, date_set_date),
        ("setUTCDate", 1, date_set_utc_date),
        ("setMonth", 2, date_set_month),
        ("setUTCMonth", 2, date_set_utc_month),
        ("setFullYear", 3, date_set_full_year),
        ("setUTCFullYear", 3, date_set_utc_full_year),
        ("toString", 0, date_to_string),
        ("toDateString", 0, date_to_date_string),
        ("toTimeString", 0, date_to_time_string),
        ("toISOString", 0, date_to_iso_string),
        ("toJSON", 1, date_to_json),
        ("toLocaleString", 0, date_to_string),
        ("toLocaleDateString", 0, date_to_date_string),
        ("toLocaleTimeString", 0, date_to_time_string),
        // Annex B §B.2.4.1–B.2.4.2
        ("getYear", 0, date_get_year),
        ("setYear", 1, date_set_year),
    ];
    for &(name, len, f) in methods {
        define_method_len(vm, dp, name, len, f);
    }

    // Annex B §B.2.4.3: `Date.prototype.toGMTString === Date.prototype.toUTCString`.
    let utc_str_fn = vm.alloc_native_fn("toUTCString", date_to_utc_string);
    vm.set_fn_length(utc_str_fn, 0);
    for key_name in ["toUTCString", "toGMTString"] {
        let k = vm.heap.intern_key(key_name);
        vm.heap.object_define_data(
            dp,
            k,
            JsValue::from_object(utc_str_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    // ECMA-262 §21.4.4.45: `Date.prototype[Symbol.toPrimitive]` is non-writable,
    // non-enumerable, configurable, length 1, name "[Symbol.toPrimitive]".
    let to_prim_fn = vm.alloc_native_fn("[Symbol.toPrimitive]", date_symbol_to_primitive);
    vm.set_fn_length(to_prim_fn, 1);
    let k_to_prim = vm.key_symbol_to_primitive;
    vm.heap.object_define_data(
        dp,
        k_to_prim,
        JsValue::from_object(to_prim_fn),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    let ctor = vm.alloc_native_fn("Date", date_ctor);
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, 7);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(dp),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        dp,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_method_len(vm, ctor, "now", 0, date_now);
    define_method_len(vm, ctor, "parse", 1, date_parse);
    define_method_len(vm, ctor, "UTC", 7, date_utc);
    define_global(vm, "Date", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// Time value helpers & proleptic Gregorian calendar arithmetic (ECMA-262 §21.4.1)
// ---------------------------------------------------------------------------

pub(crate) fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| (d.as_millis() as i64) as f64)
}

/// ECMA-262 §21.4.1.32 `TimeClip(time)`.
pub(crate) fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > MAX_TIME_VALUE {
        return f64::NAN;
    }
    let truncated = t.trunc();
    if truncated == 0.0 {
        0.0
    } else {
        truncated
    }
}

/// ECMA-262 §21.4.1.33 `MakeFullYear(year)` used by `new Date(...)`, `Date.UTC(...)`, `setYear`.
fn make_full_year(year: f64) -> f64 {
    if year.is_nan() {
        return f64::NAN;
    }
    let yi = year.trunc();
    if (0.0..=99.0).contains(&yi) {
        1900.0 + yi
    } else {
        year
    }
}

/// Howard Hinnant civil-to-days algorithm (exact proleptic Gregorian calendar, 1970-01-01 = 0).
pub(crate) fn days_from_civil_i128(y: i128, m: i128, d: i128) -> i128 {
    let y = y - i128::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Howard Hinnant days-to-civil -> `(year, month 1..=12, day 1..=31)`.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = i128::from(days) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    let year = (y + i128::from(m <= 2)) as i64;
    (year, m, d)
}

/// Breaks an integer millisecond timestamp into `(year, month 1..=12, day 1..=31, h, min, s, ms)`.
pub(crate) fn civil_from_ms(total_ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let days = total_ms.div_euclid(MS_PER_DAY_I64);
    let rem = total_ms.rem_euclid(MS_PER_DAY_I64);
    let (year, m, d) = civil_from_days(days);
    let milli = (rem % 1000) as u32;
    let secs = rem / 1000;
    let s = (secs % 60) as u32;
    let min = ((secs / 60) % 60) as u32;
    let h = (secs / 3600) as u32;
    (year, m, d, h, min, s, milli)
}

fn week_day_from_ms(total_ms: i64) -> i32 {
    let days = total_ms.div_euclid(MS_PER_DAY_I64);
    (days + 4).rem_euclid(7) as i32
}

fn is_leap_year(y: i64) -> bool {
    y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0)
}

/// ECMA-262 §21.4.1.29 `MakeTime(hour, min, sec, ms)`.
fn make_time(hour: f64, min: f64, sec: f64, ms: f64) -> f64 {
    if !hour.is_finite() || !min.is_finite() || !sec.is_finite() || !ms.is_finite() {
        return f64::NAN;
    }
    let h = hour.trunc();
    let m = min.trunc();
    let s = sec.trunc();
    let milli = ms.trunc();
    h * MS_PER_HOUR + m * MS_PER_MINUTE + s * MS_PER_SECOND + milli
}

/// ECMA-262 §21.4.1.30 `MakeDay(year, month, date)`.
fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !year.is_finite() || !month.is_finite() || !date.is_finite() {
        return f64::NAN;
    }
    let y = year.trunc();
    let m = month.trunc();
    let dt = date.trunc();
    let ym = y + (m / 12.0).floor();
    if !ym.is_finite() {
        return f64::NAN;
    }
    let mn = m.rem_euclid(12.0) as i128;
    let day_of_m1 = if ym.abs() <= 1e15 {
        days_from_civil_i128(ym as i128, mn + 1, 1) as f64
    } else {
        // Beyond safe i128/f64 precision, fall back to ECMA-262 DayFromYear formula in f64.
        let day_from_year = 365.0 * (ym - 1970.0) + ((ym - 1969.0) / 4.0).floor()
            - ((ym - 1901.0) / 100.0).floor()
            + ((ym - 1601.0) / 400.0).floor();
        // Month offsets for non-leap / leap years.
        const OFFSETS: [f64; 12] = [
            0.0, 31.0, 59.0, 90.0, 120.0, 151.0, 181.0, 212.0, 243.0, 273.0, 304.0, 334.0,
        ];
        day_from_year + OFFSETS[mn as usize]
    };
    day_of_m1 + dt - 1.0
}

/// ECMA-262 §21.4.1.31 `MakeDate(day, time)`.
fn make_date(day: f64, time: f64) -> f64 {
    if !day.is_finite() || !time.is_finite() {
        return f64::NAN;
    }
    let tv = day * MS_PER_DAY + time;
    if !tv.is_finite() {
        return f64::NAN;
    }
    tv
}

// ---------------------------------------------------------------------------
// Local timezone conversion (jiff IANA TZDB + equivalent-year fallback)
// ---------------------------------------------------------------------------

/// Maps any proleptic Gregorian year outside `jiff`'s `-9999..=9999` range to a
/// Gregorian year in `2000..=2027` with identical leap-year status and Jan 1 weekday.
fn equivalent_jiff_year(year: i64) -> i16 {
    if (-9999..=9999).contains(&year) {
        return year as i16;
    }
    let leap = is_leap_year(year);
    let jan1_wd = (days_from_civil_i128(i128::from(year), 1, 1) + 4).rem_euclid(7) as i32;
    for candidate in 2000i64..=2027 {
        if is_leap_year(candidate) == leap {
            let c_wd = (days_from_civil_i128(i128::from(candidate), 1, 1) + 4).rem_euclid(7) as i32;
            if c_wd == jan1_wd {
                return candidate as i16;
            }
        }
    }
    if leap {
        2000
    } else {
        2001
    }
}

fn local_offset_info_utc(t_utc_ms: i64) -> (i64, String) {
    let tz = JiffTimeZone::system();
    let ts_opt = Timestamp::from_millisecond(t_utc_ms).ok().or_else(|| {
        let (y, m, d, h, min, s, milli) = civil_from_ms(t_utc_ms);
        let eq_y = equivalent_jiff_year(y);
        let eq_days = days_from_civil_i128(i128::from(eq_y), i128::from(m), i128::from(d)) as i64;
        let eq_ms = eq_days * MS_PER_DAY_I64
            + i64::from(h) * 3_600_000
            + i64::from(min) * 60_000
            + i64::from(s) * 1_000
            + i64::from(milli);
        Timestamp::from_millisecond(eq_ms).ok()
    });
    let Some(ts) = ts_opt else {
        return (0, "Coordinated Universal Time".to_string());
    };
    let info = tz.to_offset_info(ts);
    let offset_ms = i64::from(info.offset().seconds()) * 1000;
    let abbrev = info.abbreviation();
    let zone_name = if !abbrev.is_empty() {
        abbrev.to_string()
    } else {
        tz.iana_name()
            .unwrap_or("Coordinated Universal Time")
            .to_string()
    };
    (offset_ms, zone_name)
}

/// ECMA-262 §21.4.1.27 `LocalTime(t)`.
fn local_time(t_utc: f64) -> f64 {
    if !t_utc.is_finite() {
        return f64::NAN;
    }
    let t_i = t_utc.trunc() as i64;
    let (offset_ms, _) = local_offset_info_utc(t_i);
    t_utc + offset_ms as f64
}

/// ECMA-262 §21.4.1.28 `UTC(t)` (local wall-clock ms -> UTC ms).
fn utc_from_local(t_local: f64) -> f64 {
    if !t_local.is_finite() {
        return f64::NAN;
    }
    // Far beyond valid ECMAScript Date range (+/- 8.64e15 +/- 1 day) -> let TimeClip return NaN.
    if t_local.abs() > MAX_TIME_VALUE + MS_PER_DAY {
        return t_local;
    }
    let tz = JiffTimeZone::system();
    let t_i = t_local.trunc() as i64;
    let (y, m, d, h, min, s, milli) = civil_from_ms(t_i);
    let eq_y = equivalent_jiff_year(y);
    let Ok(dt) = JiffDateTime::new(
        eq_y,
        m as i8,
        d as i8,
        h as i8,
        min as i8,
        s as i8,
        (milli as i32) * 1_000_000,
    ) else {
        return t_local;
    };
    let amb = tz.to_ambiguous_timestamp(dt);
    let Ok(ts) = amb.compatible() else {
        return t_local;
    };
    if i64::from(eq_y) == y {
        ts.as_millisecond() as f64
    } else {
        let eq_days = days_from_civil_i128(i128::from(eq_y), i128::from(m), i128::from(d)) as i64;
        let eq_local_ms = eq_days * MS_PER_DAY_I64
            + i64::from(h) * 3_600_000
            + i64::from(min) * 60_000
            + i64::from(s) * 1_000
            + i64::from(milli);
        let offset_ms = eq_local_ms - ts.as_millisecond();
        t_local - offset_ms as f64
    }
}

// ---------------------------------------------------------------------------
// Receiver validation & mutation
// ---------------------------------------------------------------------------

pub(crate) fn this_time_value(
    vm: &mut JsVm,
    this_val: JsValue,
) -> Result<(HeapHandle, f64), JsException> {
    if let Some(h) = this_val.as_object() {
        if let Some(ObjectKind::Date(ms)) = vm.heap.get_object(h).map(|o| &o.kind) {
            return Ok((h, *ms));
        }
    }
    Err(vm.type_error("this is not a Date object"))
}

fn set_date_value(vm: &mut JsVm, h: HeapHandle, tv: f64) -> JsValue {
    if let Some(o) = vm.heap.get_object_mut(h) {
        o.kind = ObjectKind::Date(tv);
    }
    JsValue::from_f64(tv)
}

fn alloc_date_with_new_target(vm: &mut JsVm, tv: f64) -> Result<JsValue, JsException> {
    let proto = if let Some(nt_h) = vm.native_new_target.as_object() {
        let p = vm.get_property_by_key(nt_h, vm.key_prototype)?;
        p.as_object().unwrap_or(vm.date_proto)
    } else {
        vm.date_proto
    };
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Date(tv), Some(proto)));
    Ok(JsValue::from_object(h))
}

// ---------------------------------------------------------------------------
// Constructor & static methods (ECMA-262 §21.4.2–§21.4.3)
// ---------------------------------------------------------------------------

fn date_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    // §21.4.2.1 step 1: If NewTarget is undefined, ignore args and return ToDateString(now).
    if vm.native_new_target.is_undefined() {
        let s = format_to_date_string(now_ms());
        return Ok(JsValue::from_string(vm.heap.alloc_string(s)));
    }

    let dv = if args.is_empty() {
        now_ms()
    } else if args.len() == 1 {
        let val = args[0];
        let tv = if let Some(h) = val.as_object() {
            if let Some(ObjectKind::Date(ms)) = vm.heap.get_object(h).map(|o| &o.kind) {
                time_clip(*ms)
            } else {
                let prim = vm.to_primitive(val, PrimHint::Default)?;
                if prim.is_string() {
                    let s = vm.value_to_rust_string(prim);
                    parse_date_string(&s)
                } else {
                    vm.to_number(prim)?
                }
            }
        } else if val.is_string() {
            let s = vm.value_to_rust_string(val);
            parse_date_string(&s)
        } else {
            vm.to_number(val)?
        };
        time_clip(tv)
    } else {
        // Strictly evaluate arguments left-to-right via ToNumber.
        let y = vm.to_number(args[0])?;
        let m = vm.to_number(args[1])?;
        let dt = if args.len() > 2 {
            vm.to_number(args[2])?
        } else {
            1.0
        };
        let h = if args.len() > 3 {
            vm.to_number(args[3])?
        } else {
            0.0
        };
        let min = if args.len() > 4 {
            vm.to_number(args[4])?
        } else {
            0.0
        };
        let s = if args.len() > 5 {
            vm.to_number(args[5])?
        } else {
            0.0
        };
        let milli = if args.len() > 6 {
            vm.to_number(args[6])?
        } else {
            0.0
        };
        let yr = make_full_year(y);
        let final_date = make_date(make_day(yr, m, dt), make_time(h, min, s, milli));
        time_clip(utc_from_local(final_date))
    };

    alloc_date_with_new_target(vm, dv)
}

fn date_now(_vm: &mut JsVm, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_f64(now_ms()))
}

fn date_parse(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.to_js_string(arg0(args))?;
    Ok(JsValue::from_f64(parse_date_string(&s)))
}

fn date_utc(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let y = vm.to_number(arg0(args))?;
    let m = if args.len() > 1 {
        vm.to_number(args[1])?
    } else {
        0.0
    };
    let dt = if args.len() > 2 {
        vm.to_number(args[2])?
    } else {
        1.0
    };
    let h = if args.len() > 3 {
        vm.to_number(args[3])?
    } else {
        0.0
    };
    let min = if args.len() > 4 {
        vm.to_number(args[4])?
    } else {
        0.0
    };
    let s = if args.len() > 5 {
        vm.to_number(args[5])?
    } else {
        0.0
    };
    let milli = if args.len() > 6 {
        vm.to_number(args[6])?
    } else {
        0.0
    };
    let yr = make_full_year(y);
    let tv = time_clip(make_date(make_day(yr, m, dt), make_time(h, min, s, milli)));
    Ok(JsValue::from_f64(tv))
}

// ---------------------------------------------------------------------------
// Getters (ECMA-262 §21.4.4.2–§21.4.4.19 & Annex B getYear)
// ---------------------------------------------------------------------------

fn date_get_time(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, ms) = this_time_value(vm, this_val)?;
    Ok(JsValue::from_f64(ms))
}

fn get_field(
    vm: &mut JsVm,
    this_val: JsValue,
    local: bool,
    field: u8,
) -> Result<JsValue, JsException> {
    let (_, ms) = this_time_value(vm, this_val)?;
    if !ms.is_finite() {
        return Ok(JsValue::NAN);
    }
    let t = if local { local_time(ms) } else { ms };
    let ti = t.trunc() as i64;
    if field == 7 {
        return Ok(JsValue::from_i32(week_day_from_ms(ti)));
    }
    let (y, m, d, h, min, s, milli) = civil_from_ms(ti);
    let v = match field {
        0 => y as f64,
        1 => (m - 1) as f64,
        2 => d as f64,
        3 => h as f64,
        4 => min as f64,
        5 => s as f64,
        6 => milli as f64,
        8 => (y - 1900) as f64,
        _ => f64::NAN,
    };
    Ok(JsValue::from_f64(v))
}

fn date_get_full_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 0)
}
fn date_get_utc_full_year(
    vm: &mut JsVm,
    t: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    get_field(vm, t, false, 0)
}
fn date_get_month(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 1)
}
fn date_get_utc_month(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, false, 1)
}
fn date_get_date(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 2)
}
fn date_get_utc_date(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, false, 2)
}
fn date_get_day(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 7)
}
fn date_get_utc_day(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, false, 7)
}
fn date_get_hours(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 3)
}
fn date_get_utc_hours(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, false, 3)
}
fn date_get_minutes(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 4)
}
fn date_get_utc_minutes(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, false, 4)
}
fn date_get_seconds(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 5)
}
fn date_get_utc_seconds(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, false, 5)
}
fn date_get_milliseconds(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 6)
}
fn date_get_utc_milliseconds(
    vm: &mut JsVm,
    t: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    get_field(vm, t, false, 6)
}
fn date_get_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    get_field(vm, t, true, 8)
}

fn date_get_timezone_offset(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, ms) = this_time_value(vm, this_val)?;
    if !ms.is_finite() {
        return Ok(JsValue::NAN);
    }
    let off = (ms - local_time(ms)) / MS_PER_MINUTE;
    Ok(JsValue::from_f64(if off == 0.0 { 0.0 } else { off }))
}

// ---------------------------------------------------------------------------
// Setters (ECMA-262 §21.4.4.20–§21.4.4.34 & Annex B setYear)
// ---------------------------------------------------------------------------

fn date_set_time(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (h, _) = this_time_value(vm, this_val)?;
    let t = vm.to_number(arg0(args))?;
    let v = time_clip(t);
    Ok(set_date_value(vm, h, v))
}

fn set_time_fields(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    local: bool,
    start_field: usize, // 0=hour, 1=min, 2=sec, 3=ms
) -> Result<JsValue, JsException> {
    let (h, t_raw) = this_time_value(vm, this_val)?;
    let max_args = 4 - start_field;
    // Always evaluate `ToNumber` on provided args (and at least arg0) BEFORE checking NaN.
    let mut nums = [None; 4];
    let n_eval = args.len().clamp(1, max_args);
    for i in 0..n_eval {
        let v = args.get(i).copied().unwrap_or(JsValue::UNDEFINED);
        nums[start_field + i] = Some(vm.to_number(v)?);
    }
    if !t_raw.is_finite() {
        return Ok(JsValue::NAN);
    }
    let t = if local { local_time(t_raw) } else { t_raw };
    let ti = t.trunc() as i64;
    let day = ti.div_euclid(MS_PER_DAY_I64) as f64;
    let (_, _, _, ch, cmin, cs, cms) = civil_from_ms(ti);
    let hh = nums[0].unwrap_or(f64::from(ch));
    let mm = nums[1].unwrap_or(f64::from(cmin));
    let ss = nums[2].unwrap_or(f64::from(cs));
    let ms = nums[3].unwrap_or(f64::from(cms));
    let date = make_date(day, make_time(hh, mm, ss, ms));
    let u = time_clip(if local { utc_from_local(date) } else { date });
    Ok(set_date_value(vm, h, u))
}

fn set_date_fields(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    local: bool,
    start_field: usize, // 0=year, 1=month, 2=date
) -> Result<JsValue, JsException> {
    let (h, t_raw) = this_time_value(vm, this_val)?;
    let max_args = 3 - start_field;
    let mut nums = [None; 3];
    let n_eval = args.len().clamp(1, max_args);
    for i in 0..n_eval {
        let v = args.get(i).copied().unwrap_or(JsValue::UNDEFINED);
        nums[start_field + i] = Some(vm.to_number(v)?);
    }
    let t = if !t_raw.is_finite() {
        if start_field == 0 {
            // setFullYear / setUTCFullYear treats NaN time value as +0.
            0.0
        } else {
            return Ok(JsValue::NAN);
        }
    } else if local {
        local_time(t_raw)
    } else {
        t_raw
    };
    let ti = t.trunc() as i64;
    let (cy, cm, cd, _, _, _, _) = civil_from_ms(ti);
    let time_within_day = ti.rem_euclid(MS_PER_DAY_I64) as f64;
    let yy = nums[0].unwrap_or(cy as f64);
    let mm = nums[1].unwrap_or(f64::from(cm - 1));
    let dd = nums[2].unwrap_or(f64::from(cd));
    let date = make_date(make_day(yy, mm, dd), time_within_day);
    let u = time_clip(if local { utc_from_local(date) } else { date });
    Ok(set_date_value(vm, h, u))
}

fn date_set_milliseconds(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_time_fields(vm, t, a, true, 3)
}
fn date_set_utc_milliseconds(
    vm: &mut JsVm,
    t: JsValue,
    a: &[JsValue],
) -> Result<JsValue, JsException> {
    set_time_fields(vm, t, a, false, 3)
}
fn date_set_seconds(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_time_fields(vm, t, a, true, 2)
}
fn date_set_utc_seconds(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_time_fields(vm, t, a, false, 2)
}
fn date_set_minutes(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_time_fields(vm, t, a, true, 1)
}
fn date_set_utc_minutes(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_time_fields(vm, t, a, false, 1)
}
fn date_set_hours(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_time_fields(vm, t, a, true, 0)
}
fn date_set_utc_hours(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_time_fields(vm, t, a, false, 0)
}
fn date_set_date(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_date_fields(vm, t, a, true, 2)
}
fn date_set_utc_date(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_date_fields(vm, t, a, false, 2)
}
fn date_set_month(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_date_fields(vm, t, a, true, 1)
}
fn date_set_utc_month(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_date_fields(vm, t, a, false, 1)
}
fn date_set_full_year(vm: &mut JsVm, t: JsValue, a: &[JsValue]) -> Result<JsValue, JsException> {
    set_date_fields(vm, t, a, true, 0)
}
fn date_set_utc_full_year(
    vm: &mut JsVm,
    t: JsValue,
    a: &[JsValue],
) -> Result<JsValue, JsException> {
    set_date_fields(vm, t, a, false, 0)
}

/// Annex B §B.2.4.2 `Date.prototype.setYear(year)`.
fn date_set_year(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (h, t_raw) = this_time_value(vm, this_val)?;
    let y = vm.to_number(arg0(args))?;
    if y.is_nan() {
        return Ok(set_date_value(vm, h, f64::NAN));
    }
    let t = if t_raw.is_nan() {
        0.0
    } else {
        local_time(t_raw)
    };
    let ti = t.trunc() as i64;
    let (_, cm, cd, _, _, _, _) = civil_from_ms(ti);
    let time_within_day = ti.rem_euclid(MS_PER_DAY_I64) as f64;
    let yyyy = make_full_year(y);
    let d = make_day(yyyy, f64::from(cm - 1), f64::from(cd));
    let u = time_clip(utc_from_local(make_date(d, time_within_day)));
    Ok(set_date_value(vm, h, u))
}

// ---------------------------------------------------------------------------
// Formatting methods (ECMA-262 §21.4.4.35–§21.4.4.45)
// ---------------------------------------------------------------------------

fn format_year_4(y: i64) -> String {
    if y < 0 {
        format!("-{:04}", y.unsigned_abs())
    } else {
        format!("{y:04}")
    }
}

fn format_date_string_part(t_local_i64: i64) -> String {
    let (y, m, d, _, _, _, _) = civil_from_ms(t_local_i64);
    let wd = WEEKDAYS[week_day_from_ms(t_local_i64) as usize];
    let mon = MONTHS[(m - 1) as usize];
    let y_str = format_year_4(y);
    format!("{wd} {mon} {d:02} {y_str}")
}

fn format_time_and_tz_part(
    t_utc_i64: i64,
    t_local_i64: i64,
    tz_name: &str,
    offset_ms: i64,
) -> String {
    let _ = t_utc_i64;
    let (_, _, _, h, min, s, _) = civil_from_ms(t_local_i64);
    let sign = if offset_ms >= 0 { '+' } else { '-' };
    let abs_mins = offset_ms.unsigned_abs() / 60_000;
    let tz_h = abs_mins / 60;
    let tz_m = abs_mins % 60;
    format!("{h:02}:{min:02}:{s:02} GMT{sign}{tz_h:02}{tz_m:02} ({tz_name})")
}

pub(crate) fn format_to_date_string(tv: f64) -> String {
    if !tv.is_finite() {
        return "Invalid Date".to_string();
    }
    let t_utc = tv.trunc() as i64;
    let (offset_ms, tz_name) = local_offset_info_utc(t_utc);
    let t_local = t_utc + offset_ms;
    format!(
        "{} {}",
        format_date_string_part(t_local),
        format_time_and_tz_part(t_utc, t_local, &tz_name, offset_ms)
    )
}

fn date_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, tv) = this_time_value(vm, this_val)?;
    let s = format_to_date_string(tv);
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn date_to_date_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, tv) = this_time_value(vm, this_val)?;
    let s = if !tv.is_finite() {
        "Invalid Date".to_string()
    } else {
        let t_utc = tv.trunc() as i64;
        let (offset_ms, _) = local_offset_info_utc(t_utc);
        format_date_string_part(t_utc + offset_ms)
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn date_to_time_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, tv) = this_time_value(vm, this_val)?;
    let s = if !tv.is_finite() {
        "Invalid Date".to_string()
    } else {
        let t_utc = tv.trunc() as i64;
        let (offset_ms, tz_name) = local_offset_info_utc(t_utc);
        format_time_and_tz_part(t_utc, t_utc + offset_ms, &tz_name, offset_ms)
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn date_to_utc_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, tv) = this_time_value(vm, this_val)?;
    let s = if !tv.is_finite() {
        "Invalid Date".to_string()
    } else {
        let ti = tv.trunc() as i64;
        let (y, m, d, h, min, s, _) = civil_from_ms(ti);
        let wd = WEEKDAYS[week_day_from_ms(ti) as usize];
        let mon = MONTHS[(m - 1) as usize];
        let y_str = format_year_4(y);
        format!("{wd}, {d:02} {mon} {y_str} {h:02}:{min:02}:{s:02} GMT")
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

pub(crate) fn date_to_iso_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, tv) = this_time_value(vm, this_val)?;
    if !tv.is_finite() {
        return Err(vm.range_error("Invalid time value"));
    }
    let (y, m, d, h, min, s, milli) = civil_from_ms(tv.trunc() as i64);
    let y_str = if y < 0 {
        format!("-{:06}", y.unsigned_abs())
    } else if y > 9999 {
        format!("+{y:06}")
    } else {
        format!("{y:04}")
    };
    let iso = format!("{y_str}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{milli:03}Z");
    Ok(JsValue::from_string(vm.heap.alloc_string(iso)))
}

/// ECMA-262 §21.4.4.37 `Date.prototype.toJSON(key)` (generic).
fn date_to_json(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let o = vm.to_object(this_val)?;
    let tv = vm.to_primitive(JsValue::from_object(o), PrimHint::Number)?;
    if tv.is_number() && !tv.as_number().unwrap_or(0.0).is_finite() {
        return Ok(JsValue::NULL);
    }
    let k_iso = vm.heap.intern_key("toISOString");
    let to_iso = vm.get_property_by_key(o, k_iso)?;
    vm.call_value(to_iso, JsValue::from_object(o), &[])
}

/// ECMA-262 §21.4.4.45 `Date.prototype[Symbol.toPrimitive](hint)`.
fn date_symbol_to_primitive(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Date.prototype[Symbol.toPrimitive] requires an object receiver"));
    };
    let hint_val = arg0(args);
    let Some(sh) = hint_val.as_string() else {
        return Err(vm.type_error("Invalid hint passed to Date.prototype[Symbol.toPrimitive]"));
    };
    let hint = vm.heap.materialize_string(sh).unwrap_or_default();
    let order = match hint.as_str() {
        "string" | "default" => [vm.key_to_string, vm.key_value_of],
        "number" => [vm.key_value_of, vm.key_to_string],
        _ => {
            return Err(vm.type_error("Invalid hint passed to Date.prototype[Symbol.toPrimitive]"));
        }
    };
    for method_key in order {
        let m = vm.get_property_by_key(h, method_key)?;
        if let Some(mh) = m.as_object() {
            if vm.is_callable_handle(mh) {
                let res = vm.call_value(m, this_val, &[])?;
                if !res.is_object() {
                    return Ok(res);
                }
            }
        }
    }
    Err(vm.type_error("Cannot convert object to primitive value"))
}

// ---------------------------------------------------------------------------
// Date.parse (ECMA-262 §21.4.1.15 ISO-8601 + RFC 2822 / toString / toUTCString)
// ---------------------------------------------------------------------------

pub(crate) fn parse_date_string(input: &str) -> f64 {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return f64::NAN;
    }
    if let Some(ms) = parse_ecma_iso8601(trimmed) {
        return time_clip(ms);
    }
    if let Some(ms) = parse_rfc2822_or_human(trimmed) {
        return time_clip(ms);
    }
    f64::NAN
}

fn parse_exact_digits(s: &str, count: usize) -> Option<i64> {
    if s.len() != count || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok()
}

/// Strict ECMA-262 §21.4.1.15 Date Time String Format parser.
fn parse_ecma_iso8601(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    // Year: `YYYY` or `(+|-)YYYYYY`
    let (year, neg_zero_year, mut pos) =
        if bytes.first() == Some(&b'+') || bytes.first() == Some(&b'-') {
            let neg = bytes[0] == b'-';
            if bytes.len() < 7 {
                return None;
            }
            let y_abs = parse_exact_digits(&s[1..7], 6)?;
            (if neg { -y_abs } else { y_abs }, neg && y_abs == 0, 7usize)
        } else {
            if bytes.len() < 4 {
                return None;
            }
            let y = parse_exact_digits(&s[0..4], 4)?;
            (y, false, 4usize)
        };
    if neg_zero_year {
        return None;
    }

    // Optional `-MM` and `-DD`
    let mut month: i64 = 1;
    let mut day: i64 = 1;
    if pos < bytes.len() && bytes[pos] == b'-' {
        if pos + 3 > bytes.len() {
            return None;
        }
        month = parse_exact_digits(&s[pos + 1..pos + 3], 2)?;
        if !(1..=12).contains(&month) {
            return None;
        }
        pos += 3;
        if pos < bytes.len() && bytes[pos] == b'-' {
            if pos + 3 > bytes.len() {
                return None;
            }
            day = parse_exact_digits(&s[pos + 1..pos + 3], 2)?;
            if !(1..=31).contains(&day) {
                return None;
            }
            pos += 3;
        }
    }

    if pos == bytes.len() {
        // Date-only ISO-8601 forms are interpreted as UTC!
        let d = make_day(year as f64, (month - 1) as f64, day as f64);
        return Some(make_date(d, 0.0));
    }

    // Time part must begin with `T`
    if bytes[pos] != b'T' {
        return None;
    }
    pos += 1;

    // `HH:mm`
    if pos + 5 > bytes.len() || bytes[pos + 2] != b':' {
        return None;
    }
    let hour = parse_exact_digits(&s[pos..pos + 2], 2)?;
    let min = parse_exact_digits(&s[pos + 3..pos + 5], 2)?;
    if !(0..=24).contains(&hour) || !(0..=59).contains(&min) {
        return None;
    }
    pos += 5;

    let mut sec: i64 = 0;
    let mut ms: i64 = 0;
    if pos < bytes.len() && bytes[pos] == b':' {
        if pos + 3 > bytes.len() {
            return None;
        }
        sec = parse_exact_digits(&s[pos + 1..pos + 3], 2)?;
        if !(0..=59).contains(&sec) {
            return None;
        }
        pos += 3;
        if pos < bytes.len() && bytes[pos] == b'.' {
            pos += 1;
            let frac_start = pos;
            while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            let frac_len = pos - frac_start;
            if frac_len == 0 {
                return None;
            }
            let frac = &s[frac_start..pos];
            let mut m_val = 0i64;
            for i in 0..3 {
                let d = frac.as_bytes().get(i).map_or(0, |b| i64::from(b - b'0'));
                m_val = m_val * 10 + d;
            }
            ms = m_val;
        }
    }

    // Hour 24 is only valid as exact `24:00:00.000`.
    if hour == 24 && (min != 0 || sec != 0 || ms != 0) {
        return None;
    }

    // Optional timezone designator (`Z` or `+HH:mm` / `-HH:mm`)
    let tz_offset_ms: Option<f64> = if pos == bytes.len() {
        None
    } else if bytes[pos] == b'Z' && pos + 1 == bytes.len() {
        Some(0.0)
    } else if (bytes[pos] == b'+' || bytes[pos] == b'-')
        && pos + 6 == bytes.len()
        && bytes[pos + 3] == b':'
    {
        let sign = if bytes[pos] == b'+' { 1.0 } else { -1.0 };
        let tz_h = parse_exact_digits(&s[pos + 1..pos + 3], 2)?;
        let tz_m = parse_exact_digits(&s[pos + 4..pos + 6], 2)?;
        if !(0..=23).contains(&tz_h) || !(0..=59).contains(&tz_m) {
            return None;
        }
        Some(sign * ((tz_h as f64) * MS_PER_HOUR + (tz_m as f64) * MS_PER_MINUTE))
    } else {
        return None;
    };

    let d = make_day(year as f64, (month - 1) as f64, day as f64);
    let t = make_time(hour as f64, min as f64, sec as f64, ms as f64);
    let dt = make_date(d, t);
    match tz_offset_ms {
        Some(off) => Some(dt - off),
        None => Some(utc_from_local(dt)),
    }
}

fn parse_month_name(tok: &str) -> Option<i64> {
    let lower = tok.to_ascii_lowercase();
    let prefix = lower.get(..3)?;
    match prefix {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

fn is_weekday_name(tok: &str) -> bool {
    let lower = tok.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "sun"
            | "sunday"
            | "mon"
            | "monday"
            | "tue"
            | "tues"
            | "tuesday"
            | "wed"
            | "wednesday"
            | "thu"
            | "thur"
            | "thurs"
            | "thursday"
            | "fri"
            | "friday"
            | "sat"
            | "saturday"
    )
}

fn parse_tz_token(tok: &str) -> Option<f64> {
    let upper = tok.to_ascii_uppercase();
    if upper == "GMT" || upper == "UTC" || upper == "Z" || upper == "UT" {
        return Some(0.0);
    }
    let rest = upper
        .strip_prefix("GMT")
        .or_else(|| upper.strip_prefix("UTC"))
        .unwrap_or(&upper);
    if let Some(sign_char) = rest.chars().next().filter(|&c| c == '+' || c == '-') {
        let sign = if sign_char == '+' { 1.0 } else { -1.0 };
        let digits = &rest[1..];
        let (h, m) = if let Some((hs, ms)) = digits.split_once(':') {
            (hs.parse::<i64>().ok()?, ms.parse::<i64>().ok()?)
        } else if digits.len() == 4 {
            (
                digits[..2].parse::<i64>().ok()?,
                digits[2..].parse::<i64>().ok()?,
            )
        } else if digits.len() == 2 {
            (digits.parse::<i64>().ok()?, 0)
        } else {
            return None;
        };
        if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
            return None;
        }
        return Some(sign * ((h as f64) * MS_PER_HOUR + (m as f64) * MS_PER_MINUTE));
    }
    // Common US / European legacy abbreviations
    let hours = match upper.as_str() {
        "EST" | "CDT" => -5.0,
        "EDT" => -4.0,
        "CST" | "MDT" => -6.0,
        "MST" | "PDT" => -7.0,
        "PST" => -8.0,
        "CET" => 1.0,
        "CEST" => 2.0,
        _ => return None,
    };
    Some(hours * MS_PER_HOUR)
}

fn strip_parenthetical_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Parses `Date.prototype.toString()`, `Date.prototype.toUTCString()`,
/// `Date.prototype.toDateString()`, RFC 2822, and common slash/named-month dates.
fn parse_rfc2822_or_human(input: &str) -> Option<f64> {
    let cleaned = strip_parenthetical_comments(input);
    let tokens: Vec<&str> = cleaned
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }

    let mut year: Option<i64> = None;
    let mut month: Option<i64> = None;
    let mut day: Option<i64> = None;
    let mut hour: i64 = 0;
    let mut min: i64 = 0;
    let mut sec: i64 = 0;
    let mut ms: i64 = 0;
    let mut tz_offset_ms: Option<f64> = None;
    let mut numbers: Vec<(i64, usize, bool)> = Vec::new(); // (val, digit_len, has_sign)

    for &tok in &tokens {
        if is_weekday_name(tok) {
            continue;
        }
        if let Some(m) = parse_month_name(tok) {
            if month.is_some() {
                return None;
            }
            month = Some(m);
            continue;
        }
        if let Some(off) = parse_tz_token(tok) {
            tz_offset_ms = Some(off);
            continue;
        }
        // Time token `HH:MM(:SS(.sss)?)?`
        if tok.contains(':') && !tok.starts_with('+') && !tok.starts_with('-') {
            let mut parts = tok.split(':');
            hour = parts.next()?.parse::<i64>().ok()?;
            min = parts.next()?.parse::<i64>().ok()?;
            if let Some(sec_s) = parts.next() {
                if let Some((si, sf)) = sec_s.split_once('.') {
                    sec = si.parse::<i64>().ok()?;
                    let mut m_val = 0i64;
                    for i in 0..3 {
                        let d = sf.as_bytes().get(i).map_or(0, |b| i64::from(b - b'0'));
                        m_val = m_val * 10 + d;
                    }
                    ms = m_val;
                } else {
                    sec = sec_s.parse::<i64>().ok()?;
                }
            }
            if parts.next().is_some()
                || !(0..=24).contains(&hour)
                || !(0..=59).contains(&min)
                || !(0..=59).contains(&sec)
            {
                return None;
            }
            continue;
        }
        // Slash/hyphen date `YYYY/MM/DD` or `MM/DD/YYYY`
        if tok.contains('/') {
            let parts: Vec<&str> = tok.split('/').collect();
            if parts.len() == 3 {
                let a = parts[0].parse::<i64>().ok()?;
                let b = parts[1].parse::<i64>().ok()?;
                let c = parts[2].parse::<i64>().ok()?;
                if parts[0].len() >= 4 || a > 31 {
                    year = Some(a);
                    month = Some(b);
                    day = Some(c);
                } else {
                    month = Some(a);
                    day = Some(b);
                    year = Some(if (0..=49).contains(&c) && parts[2].len() <= 2 {
                        2000 + c
                    } else if (50..=99).contains(&c) && parts[2].len() <= 2 {
                        1900 + c
                    } else {
                        c
                    });
                }
                continue;
            }
            return None;
        }
        // Signed or unsigned integer token (year or day)
        let has_sign = tok.starts_with('-') || tok.starts_with('+');
        let digits = if has_sign { &tok[1..] } else { tok };
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let v = tok.parse::<i64>().ok()?;
            numbers.push((v, digits.len(), has_sign));
            continue;
        }
        return None;
    }

    if month.is_some() && year.is_none() && day.is_none() {
        match numbers.as_slice() {
            [(d, _, false), (y, ylen, ys)] if *ylen >= 4 || *ys || *y > 31 => {
                day = Some(*d);
                year = Some(*y);
            }
            [(y, ylen, ys), (d, _, false)] if *ylen >= 4 || *ys || *y > 31 => {
                year = Some(*y);
                day = Some(*d);
            }
            [(d, _, false), (y, _, false)] => {
                day = Some(*d);
                year = Some(if (0..=49).contains(y) {
                    2000 + *y
                } else if (50..=99).contains(y) {
                    1900 + *y
                } else {
                    *y
                });
            }
            _ => return None,
        }
    }

    let (y, m, d) = (year?, month?, day?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let day_num = make_day(y as f64, (m - 1) as f64, d as f64);
    let time_num = make_time(hour as f64, min as f64, sec as f64, ms as f64);
    let dt = make_date(day_num, time_num);
    match tz_offset_ms {
        Some(off) => Some(dt - off),
        None => Some(utc_from_local(dt)),
    }
}
