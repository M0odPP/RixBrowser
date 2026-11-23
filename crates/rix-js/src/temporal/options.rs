//! Spec-compliant option parsing and JS value coercions for ECMAScript `Temporal`.

use std::str::FromStr;

use temporal_rs::fields::{CalendarFields, ZonedDateTimeFields};
use temporal_rs::options::{
    DifferenceSettings, Disambiguation, DisplayCalendar, DisplayOffset, DisplayTimeZone,
    OffsetDisambiguation, Overflow, RelativeTo, RoundingIncrement, RoundingMode, RoundingOptions,
    ToStringRoundingOptions, Unit,
};
use temporal_rs::parsers::Precision;
use temporal_rs::partial::{PartialDate, PartialDuration, PartialTime, PartialZonedDateTime};
use temporal_rs::{
    Calendar, MonthCode, PlainDate, TimeZone, TinyAsciiStr, UtcOffset, ZonedDateTime,
};

use crate::bigint::JsBigInt;
use crate::heap::TemporalValue;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, PrimHint};

use super::{get_temporal, temporal_err};

#[inline]
pub(crate) fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

/// ECMA-262 §7.1.13 `ToBigInt(argument)` (rejects Numbers, Symbols, null, undefined).
pub(crate) fn to_bigint_strict(vm: &mut JsVm, val: JsValue) -> Result<JsBigInt, JsException> {
    let prim = vm.to_primitive(val, PrimHint::Number)?;
    if let Some(bh) = prim.as_bigint() {
        return Ok(vm
            .heap
            .get_bigint(bh)
            .cloned()
            .unwrap_or_else(JsBigInt::zero));
    }
    if let Some(b) = prim.as_bool() {
        return Ok(JsBigInt::from_i64(i64::from(b)));
    }
    if let Some(sh) = prim.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        return parse_string_to_bigint(vm, &s);
    }
    Err(vm.type_error("Cannot convert value to a BigInt"))
}

fn parse_string_to_bigint(vm: &mut JsVm, s: &str) -> Result<JsBigInt, JsException> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(JsBigInt::zero());
    }
    // StringToBigInt forbids trailing 'n' and underscores.
    if trimmed.contains('_') || trimmed.ends_with('n') || trimmed.ends_with('N') {
        return Err(vm.make_error("SyntaxError", format!("Cannot convert '{s}' to a BigInt")));
    }
    if trimmed.starts_with("+0x")
        || trimmed.starts_with("-0x")
        || trimmed.starts_with("+0X")
        || trimmed.starts_with("-0X")
        || trimmed.starts_with("+0o")
        || trimmed.starts_with("-0o")
        || trimmed.starts_with("+0O")
        || trimmed.starts_with("-0O")
        || trimmed.starts_with("+0b")
        || trimmed.starts_with("-0b")
        || trimmed.starts_with("+0B")
        || trimmed.starts_with("-0B")
    {
        return Err(vm.make_error("SyntaxError", format!("Cannot convert '{s}' to a BigInt")));
    }
    JsBigInt::parse_str(trimmed)
        .ok_or_else(|| vm.make_error("SyntaxError", format!("Cannot convert '{s}' to a BigInt")))
}

/// `ToBigInt(val)` converted to `i128` (`RangeError` if outside `i128` range).
pub(crate) fn to_bigint_i128(vm: &mut JsVm, val: JsValue) -> Result<i128, JsException> {
    let bi = to_bigint_strict(vm, val)?;
    bi.to_i128_checked()
        .ok_or_else(|| vm.range_error("Epoch nanoseconds out of valid Temporal.Instant range"))
}

/// ECMA-262 `ToIntegerIfIntegral(argument)` — throws `RangeError` if non-finite or fractional.
pub(crate) fn to_integer_if_integral_f64(vm: &mut JsVm, val: JsValue) -> Result<f64, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() || n.trunc() != n {
        return Err(vm.range_error("Value must be a finite integer"));
    }
    Ok(if n == 0.0 { 0.0 } else { n })
}

pub(crate) fn to_integer_if_integral_i64(vm: &mut JsVm, val: JsValue) -> Result<i64, JsException> {
    let n = to_integer_if_integral_f64(vm, val)?;
    if n < i64::MIN as f64 || n >= 9_223_372_036_854_775_808.0 {
        return Err(vm.range_error("Integer value out of range"));
    }
    Ok(n as i64)
}

pub(crate) fn to_integer_if_integral_i128(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<i128, JsException> {
    let n = to_integer_if_integral_f64(vm, val)?;
    if n.abs() >= 1.701_411_834_604_692_3e38 {
        return Err(vm.range_error("Integer value out of range"));
    }
    Ok(n as i128)
}

/// ECMA-262 `ToIntegerWithTruncation(argument)` — throws `RangeError` if non-finite.
pub(crate) fn to_integer_with_truncation_i32(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<i32, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() {
        return Err(vm.range_error("Expected finite integer"));
    }
    let t = n.trunc();
    if t < f64::from(i32::MIN) || t > f64::from(i32::MAX) {
        return Err(vm.range_error("Integer out of range"));
    }
    Ok(t as i32)
}

pub(crate) fn to_positive_integer_with_truncation_u8(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<u8, JsException> {
    let n = to_integer_with_truncation_i32(vm, val)?;
    if n < 1 {
        return Err(vm.range_error("Expected positive integer"));
    }
    u8::try_from(n).map_err(|_| vm.range_error("Integer out of range"))
}

pub(crate) fn to_integer_with_truncation_u8(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<u8, JsException> {
    let n = to_integer_with_truncation_i32(vm, val)?;
    u8::try_from(n).map_err(|_| vm.range_error("Integer out of range"))
}

pub(crate) fn to_integer_with_truncation_u16(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<u16, JsException> {
    let n = to_integer_with_truncation_i32(vm, val)?;
    u16::try_from(n).map_err(|_| vm.range_error("Integer out of range"))
}

/// ECMA-262 `GetOptionsObject(options)`.
pub(crate) fn get_options_object(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<Option<HeapHandle>, JsException> {
    if val.is_undefined() {
        return Ok(None);
    }
    val.as_object()
        .map(Some)
        .ok_or_else(|| vm.type_error("Temporal options must be an object or undefined"))
}

pub(crate) fn get_prop(vm: &mut JsVm, obj: HeapHandle, name: &str) -> Result<JsValue, JsException> {
    let k = vm.heap.intern_key(name);
    vm.get_property_by_key(obj, k)
}

pub(crate) fn get_opt_string(
    vm: &mut JsVm,
    obj: HeapHandle,
    name: &str,
) -> Result<Option<String>, JsException> {
    let v = get_prop(vm, obj, name)?;
    if v.is_undefined() {
        return Ok(None);
    }
    Ok(Some(vm.to_js_string(v)?))
}

pub(crate) fn parse_unit_str(
    vm: &mut JsVm,
    s: &str,
    allow_auto: bool,
) -> Result<Unit, JsException> {
    let u = Unit::from_str(s).map_err(|_| vm.range_error(format!("Invalid Temporal unit: {s}")))?;
    if !allow_auto && u == Unit::Auto {
        return Err(vm.range_error("Unit 'auto' is not allowed here"));
    }
    Ok(u)
}

pub(crate) fn get_unit_option(
    vm: &mut JsVm,
    obj: HeapHandle,
    name: &str,
    allow_auto: bool,
) -> Result<Option<Unit>, JsException> {
    let Some(s) = get_opt_string(vm, obj, name)? else {
        return Ok(None);
    };
    Ok(Some(parse_unit_str(vm, &s, allow_auto)?))
}

pub(crate) fn get_rounding_increment_option(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<Option<RoundingIncrement>, JsException> {
    let v = get_prop(vm, obj, "roundingIncrement")?;
    if v.is_undefined() {
        return Ok(None);
    }
    let n = vm.to_number(v)?;
    if !n.is_finite() {
        return Err(vm.range_error("roundingIncrement must be finite"));
    }
    let t = n.trunc();
    if !(1.0..=1_000_000_000.0).contains(&t) {
        return Err(vm.range_error("roundingIncrement out of range"));
    }
    let inc = RoundingIncrement::try_new(t as u32).map_err(|e| temporal_err(vm, e))?;
    Ok(Some(inc))
}

pub(crate) fn get_rounding_mode_option(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<Option<RoundingMode>, JsException> {
    let Some(s) = get_opt_string(vm, obj, "roundingMode")? else {
        return Ok(None);
    };
    RoundingMode::from_str(&s)
        .map(Some)
        .map_err(|e| temporal_err(vm, e))
}

pub(crate) fn get_overflow_option(
    vm: &mut JsVm,
    obj: Option<HeapHandle>,
) -> Result<Option<Overflow>, JsException> {
    let Some(h) = obj else {
        return Ok(None);
    };
    let Some(s) = get_opt_string(vm, h, "overflow")? else {
        return Ok(None);
    };
    Overflow::from_str(&s)
        .map(Some)
        .map_err(|_| vm.range_error(format!("Invalid overflow option: {s}")))
}

pub(crate) fn get_disambiguation_option(
    vm: &mut JsVm,
    obj: Option<HeapHandle>,
) -> Result<Option<Disambiguation>, JsException> {
    let Some(h) = obj else {
        return Ok(None);
    };
    let Some(s) = get_opt_string(vm, h, "disambiguation")? else {
        return Ok(None);
    };
    Disambiguation::from_str(&s)
        .map(Some)
        .map_err(|_| vm.range_error(format!("Invalid disambiguation option: {s}")))
}

pub(crate) fn get_offset_disambiguation_option(
    vm: &mut JsVm,
    obj: Option<HeapHandle>,
) -> Result<Option<OffsetDisambiguation>, JsException> {
    let Some(h) = obj else {
        return Ok(None);
    };
    let Some(s) = get_opt_string(vm, h, "offset")? else {
        return Ok(None);
    };
    OffsetDisambiguation::from_str(&s)
        .map(Some)
        .map_err(|_| vm.range_error(format!("Invalid offset option: {s}")))
}

pub(crate) fn get_display_calendar_option(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<DisplayCalendar, JsException> {
    let Some(s) = get_opt_string(vm, obj, "calendarName")? else {
        return Ok(DisplayCalendar::Auto);
    };
    DisplayCalendar::from_str(&s).map_err(|e| temporal_err(vm, e))
}

pub(crate) fn get_display_offset_option(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<DisplayOffset, JsException> {
    let Some(s) = get_opt_string(vm, obj, "offset")? else {
        return Ok(DisplayOffset::Auto);
    };
    DisplayOffset::from_str(&s).map_err(|e| temporal_err(vm, e))
}

pub(crate) fn get_display_time_zone_option(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<DisplayTimeZone, JsException> {
    let Some(s) = get_opt_string(vm, obj, "timeZoneName")? else {
        return Ok(DisplayTimeZone::Auto);
    };
    DisplayTimeZone::from_str(&s).map_err(|e| temporal_err(vm, e))
}

pub(crate) fn get_fractional_second_digits_option(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<Precision, JsException> {
    let v = get_prop(vm, obj, "fractionalSecondDigits")?;
    if v.is_undefined() {
        return Ok(Precision::Auto);
    }
    if let Some(n) = v.as_number() {
        if !n.is_finite() {
            return Err(vm.range_error("fractionalSecondDigits must be finite"));
        }
        let d = n.floor();
        if !(0.0..=9.0).contains(&d) {
            return Err(vm.range_error("fractionalSecondDigits must be between 0 and 9"));
        }
        return Ok(Precision::Digit(d as u8));
    }
    let s = vm.to_js_string(v)?;
    if s == "auto" {
        Ok(Precision::Auto)
    } else {
        Err(vm.range_error(format!("Invalid fractionalSecondDigits: {s}")))
    }
}

/// Reads `DifferenceSettings` in strict alphabetical order:
/// `largestUnit`, `roundingIncrement`, `roundingMode`, `smallestUnit`.
pub(crate) fn get_difference_settings(
    vm: &mut JsVm,
    options_val: JsValue,
) -> Result<DifferenceSettings, JsException> {
    let mut settings = DifferenceSettings::default();
    let Some(h) = get_options_object(vm, options_val)? else {
        return Ok(settings);
    };
    settings.largest_unit = get_unit_option(vm, h, "largestUnit", true)?;
    settings.increment = get_rounding_increment_option(vm, h)?;
    settings.rounding_mode = get_rounding_mode_option(vm, h)?;
    settings.smallest_unit = get_unit_option(vm, h, "smallestUnit", false)?;
    Ok(settings)
}

/// Reads `RoundingOptions` (for `Instant`/`PlainTime`/`PlainDateTime`/`ZonedDateTime.round`)
/// accepting either a unit string or an options object (`roundingIncrement`, `roundingMode`, `smallestUnit`).
pub(crate) fn get_simple_rounding_options(
    vm: &mut JsVm,
    round_to: JsValue,
) -> Result<RoundingOptions, JsException> {
    let mut opts = RoundingOptions::default();
    if round_to.is_undefined() {
        return Err(vm.type_error("round() requires a unit string or options object"));
    }
    if let Some(sh) = round_to.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        opts.smallest_unit = Some(parse_unit_str(vm, &s, false)?);
        return Ok(opts);
    }
    let Some(h) = round_to.as_object() else {
        return Err(vm.type_error("round() argument must be a string or object"));
    };
    opts.increment = get_rounding_increment_option(vm, h)?;
    opts.rounding_mode = get_rounding_mode_option(vm, h)?;
    opts.smallest_unit = get_unit_option(vm, h, "smallestUnit", false)?;
    Ok(opts)
}

/// Reads `ToStringRoundingOptions` in strict alphabetical order:
/// `fractionalSecondDigits`, `roundingMode`, `smallestUnit`.
pub(crate) fn get_to_string_rounding_options(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<ToStringRoundingOptions, JsException> {
    let precision = get_fractional_second_digits_option(vm, obj)?;
    let rounding_mode = get_rounding_mode_option(vm, obj)?;
    let smallest_unit = get_unit_option(vm, obj, "smallestUnit", false)?;
    Ok(ToStringRoundingOptions {
        precision,
        smallest_unit,
        rounding_mode,
    })
}

/// Converts a JS value to a `temporal_rs::TimeZone` per `ToTemporalTimeZoneIdentifier`.
pub(crate) fn to_temporal_time_zone_identifier(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<TimeZone, JsException> {
    if let Some(TemporalValue::ZonedDateTime(zdt)) = get_temporal(vm, val) {
        return Ok(*zdt.time_zone());
    }
    let Some(sh) = val.as_string() else {
        return Err(vm.type_error("Time zone must be a string or Temporal.ZonedDateTime"));
    };
    let s = vm.heap.materialize_string(sh).unwrap_or_default();
    TimeZone::try_from_str(&s).map_err(|e| temporal_err(vm, e))
}

/// Converts a JS value to a `temporal_rs::Calendar` per `ToTemporalCalendarIdentifier`.
pub(crate) fn to_temporal_calendar_identifier(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<Calendar, JsException> {
    if let Some(tv) = get_temporal(vm, val) {
        match tv {
            TemporalValue::PlainDate(d) => return Ok(d.calendar().clone()),
            TemporalValue::PlainDateTime(dt) => return Ok(dt.calendar().clone()),
            TemporalValue::PlainYearMonth(ym) => return Ok(ym.calendar().clone()),
            TemporalValue::PlainMonthDay(md) => return Ok(md.calendar().clone()),
            TemporalValue::ZonedDateTime(zdt) => return Ok(zdt.calendar().clone()),
            _ => {}
        }
    }
    let Some(sh) = val.as_string() else {
        return Err(vm.type_error("Calendar must be a string or Temporal object with a calendar"));
    };
    let s = vm.heap.materialize_string(sh).unwrap_or_default();
    Calendar::from_str(&s).map_err(|e| temporal_err(vm, e))
}

/// Reads a `PartialDuration` property bag in strict alphabetical order:
/// `days`, `hours`, `microseconds`, `milliseconds`, `minutes`, `months`,
/// `nanoseconds`, `seconds`, `weeks`, `years`.
pub(crate) fn to_partial_duration(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<PartialDuration, JsException> {
    let mut partial = PartialDuration::empty();
    let mut any = false;

    let read_i64 =
        |vm: &mut JsVm, name: &str, any: &mut bool| -> Result<Option<i64>, JsException> {
            let v = get_prop(vm, obj, name)?;
            if v.is_undefined() {
                Ok(None)
            } else {
                *any = true;
                Ok(Some(to_integer_if_integral_i64(vm, v)?))
            }
        };
    let read_i128 =
        |vm: &mut JsVm, name: &str, any: &mut bool| -> Result<Option<i128>, JsException> {
            let v = get_prop(vm, obj, name)?;
            if v.is_undefined() {
                Ok(None)
            } else {
                *any = true;
                Ok(Some(to_integer_if_integral_i128(vm, v)?))
            }
        };

    partial.days = read_i64(vm, "days", &mut any)?;
    partial.hours = read_i64(vm, "hours", &mut any)?;
    partial.microseconds = read_i128(vm, "microseconds", &mut any)?;
    partial.milliseconds = read_i64(vm, "milliseconds", &mut any)?;
    partial.minutes = read_i64(vm, "minutes", &mut any)?;
    partial.months = read_i64(vm, "months", &mut any)?;
    partial.nanoseconds = read_i128(vm, "nanoseconds", &mut any)?;
    partial.seconds = read_i64(vm, "seconds", &mut any)?;
    partial.weeks = read_i64(vm, "weeks", &mut any)?;
    partial.years = read_i64(vm, "years", &mut any)?;

    if !any {
        return Err(vm.type_error("Duration-like object must specify at least one duration field"));
    }
    Ok(partial)
}

/// Parses `relativeTo` option (`GetTemporalRelativeToOption`).
pub(crate) fn get_relative_to_option(
    vm: &mut JsVm,
    options: HeapHandle,
) -> Result<Option<RelativeTo>, JsException> {
    let v = get_prop(vm, options, "relativeTo")?;
    if v.is_undefined() {
        return Ok(None);
    }
    if let Some(tv) = get_temporal(vm, v) {
        match tv {
            TemporalValue::PlainDate(d) => return Ok(Some(RelativeTo::PlainDate(d.clone()))),
            TemporalValue::PlainDateTime(dt) => {
                return Ok(Some(RelativeTo::PlainDate(dt.to_plain_date())))
            }
            TemporalValue::ZonedDateTime(zdt) => {
                return Ok(Some(RelativeTo::ZonedDateTime(zdt.clone())))
            }
            _ => {}
        }
    }
    if let Some(sh) = v.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        return RelativeTo::try_from_str(&s)
            .map(Some)
            .map_err(|e| temporal_err(vm, e));
    }
    let Some(h) = v.as_object() else {
        return Err(vm.type_error("relativeTo must be a string or object"));
    };
    // Read calendar first (alphabetical: calendar, day, era, eraYear, hour, microsecond,
    // millisecond, minute, month, monthCode, nanosecond, offset, second, timeZone, year).
    let cal_val = get_prop(vm, h, "calendar")?;
    let calendar = if cal_val.is_undefined() {
        Calendar::ISO
    } else {
        to_temporal_calendar_identifier(vm, cal_val)?
    };
    let day_v = get_prop(vm, h, "day")?;
    let day = if day_v.is_undefined() {
        None
    } else {
        Some(to_positive_integer_with_truncation_u8(vm, day_v)?)
    };
    let era_v = get_prop(vm, h, "era")?;
    let era = if era_v.is_undefined() {
        None
    } else {
        let s = vm.to_js_string(era_v)?;
        Some(TinyAsciiStr::<19>::try_from_str(&s).map_err(|_| vm.range_error("Invalid era"))?)
    };
    let era_year_v = get_prop(vm, h, "eraYear")?;
    let era_year = if era_year_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_i32(vm, era_year_v)?)
    };
    let hour_v = get_prop(vm, h, "hour")?;
    let hour = if hour_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_u8(vm, hour_v)?)
    };
    let microsecond_v = get_prop(vm, h, "microsecond")?;
    let microsecond = if microsecond_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_u16(vm, microsecond_v)?)
    };
    let millisecond_v = get_prop(vm, h, "millisecond")?;
    let millisecond = if millisecond_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_u16(vm, millisecond_v)?)
    };
    let minute_v = get_prop(vm, h, "minute")?;
    let minute = if minute_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_u8(vm, minute_v)?)
    };
    let month_v = get_prop(vm, h, "month")?;
    let month = if month_v.is_undefined() {
        None
    } else {
        Some(to_positive_integer_with_truncation_u8(vm, month_v)?)
    };
    let month_code_v = get_prop(vm, h, "monthCode")?;
    let month_code = if month_code_v.is_undefined() {
        None
    } else {
        let s = vm.to_js_string(month_code_v)?;
        Some(MonthCode::try_from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?)
    };
    let nanosecond_v = get_prop(vm, h, "nanosecond")?;
    let nanosecond = if nanosecond_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_u16(vm, nanosecond_v)?)
    };
    let offset_v = get_prop(vm, h, "offset")?;
    let offset = if offset_v.is_undefined() {
        None
    } else {
        let prim = vm.to_primitive(offset_v, PrimHint::String)?;
        let Some(sh) = prim.as_string() else {
            return Err(vm.type_error("offset must be a string"));
        };
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        Some(UtcOffset::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?)
    };
    let second_v = get_prop(vm, h, "second")?;
    let second = if second_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_u8(vm, second_v)?)
    };
    let tz_v = get_prop(vm, h, "timeZone")?;
    let tz = if tz_v.is_undefined() {
        None
    } else {
        Some(to_temporal_time_zone_identifier(vm, tz_v)?)
    };
    let year_v = get_prop(vm, h, "year")?;
    let year = if year_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_i32(vm, year_v)?)
    };

    let cal_fields = CalendarFields {
        year,
        month,
        month_code,
        day,
        era,
        era_year,
    };

    if let Some(timezone) = tz {
        let time = PartialTime {
            hour,
            minute,
            second,
            millisecond,
            microsecond,
            nanosecond,
        };
        let zdt_partial = PartialZonedDateTime {
            fields: ZonedDateTimeFields {
                calendar_fields: cal_fields,
                time,
                offset,
            },
            timezone: Some(timezone),
            calendar,
        };
        let zdt = ZonedDateTime::from_partial(
            zdt_partial,
            Some(Overflow::Constrain),
            Some(Disambiguation::Compatible),
            Some(OffsetDisambiguation::Reject),
        )
        .map_err(|e| temporal_err(vm, e))?;
        Ok(Some(RelativeTo::ZonedDateTime(zdt)))
    } else {
        let partial_date = PartialDate {
            calendar_fields: cal_fields,
            calendar,
        };
        let pd = PlainDate::from_partial(partial_date, Some(Overflow::Constrain))
            .map_err(|e| temporal_err(vm, e))?;
        Ok(Some(RelativeTo::PlainDate(pd)))
    }
}

/// Returns `true` if `calendar` uses era/eraYear fields per Temporal spec.
pub(crate) fn calendar_has_eras(calendar: &Calendar) -> bool {
    !matches!(calendar.identifier(), "iso8601" | "chinese" | "dangi")
}

/// Parses an optional `calendar` constructor argument (must be `undefined` or a string).
pub(crate) fn parse_calendar_ctor_arg(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<Calendar, JsException> {
    if val.is_undefined() {
        return Ok(Calendar::ISO);
    }
    let Some(sh) = val.as_string() else {
        return Err(vm.type_error("Calendar argument must be a string"));
    };
    let s = vm.heap.materialize_string(sh).unwrap_or_default();
    Calendar::try_from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))
}

/// Converts a value via `ToPrimitive(String)` and requires the result to be a String.
pub(crate) fn to_primitive_require_string(
    vm: &mut JsVm,
    val: JsValue,
    label: &str,
) -> Result<String, JsException> {
    let prim = if val.is_object() {
        vm.to_primitive(val, PrimHint::String)?
    } else {
        val
    };
    let Some(sh) = prim.as_string() else {
        return Err(vm.type_error(format!("{label} must be a string")));
    };
    Ok(vm.heap.materialize_string(sh).unwrap_or_default())
}

/// Positive integer clamped to `u8::MAX` so `Overflow::Constrain` can clamp high values
/// while `Overflow::Reject` still rejects them, and `<= 0` always throws `RangeError`.
pub(crate) fn to_clamped_pos_u8(vm: &mut JsVm, val: JsValue) -> Result<u8, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() {
        return Err(vm.range_error("Expected finite positive integer"));
    }
    let t = n.trunc();
    if t < 1.0 {
        return Err(vm.range_error("Expected positive integer"));
    }
    Ok(t.min(255.0) as u8)
}

/// Non-negative integer clamped to `u8::MAX` (for `hour`/`minute`/`second` in partial objects).
pub(crate) fn to_clamped_u8(vm: &mut JsVm, val: JsValue) -> Result<u8, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() {
        return Err(vm.range_error("Expected finite integer"));
    }
    let t = n.trunc();
    if t < 0.0 {
        return Err(vm.range_error("Expected non-negative integer"));
    }
    Ok(t.min(255.0) as u8)
}

/// Non-negative integer clamped to `9999u16` (for subsecond fields in partial objects).
pub(crate) fn to_clamped_u16(vm: &mut JsVm, val: JsValue) -> Result<u16, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() {
        return Err(vm.range_error("Expected finite integer"));
    }
    let t = n.trunc();
    if t < 0.0 {
        return Err(vm.range_error("Expected non-negative integer"));
    }
    Ok(t.min(9999.0) as u16)
}

/// ECMA-262 `IsPartialTemporalObject(value)` check for `.with(...)` methods.
pub(crate) fn require_partial_temporal_object(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("with() requires a plain object"));
    };
    if get_temporal(vm, val).is_some() {
        return Err(vm.type_error("with() argument cannot be a Temporal object"));
    }
    if !get_prop(vm, h, "calendar")?.is_undefined() {
        return Err(vm.type_error("with() argument must not have a 'calendar' property"));
    }
    if !get_prop(vm, h, "timeZone")?.is_undefined() {
        return Err(vm.type_error("with() argument must not have a 'timeZone' property"));
    }
    Ok(h)
}

/// Reads `CalendarFields` from `obj` in strict alphabetical order:
/// `day` (if `read_day`), `era` & `eraYear` (if `has_eras`), `month`, `monthCode`, `year` (if `read_year`).
pub(crate) fn read_calendar_fields(
    vm: &mut JsVm,
    obj: HeapHandle,
    has_eras: bool,
    read_day: bool,
    read_year: bool,
) -> Result<CalendarFields, JsException> {
    let day = if read_day {
        let v = get_prop(vm, obj, "day")?;
        if v.is_undefined() {
            None
        } else {
            Some(to_clamped_pos_u8(vm, v)?)
        }
    } else {
        None
    };
    let (era, era_year) = if has_eras && read_year {
        let ev = get_prop(vm, obj, "era")?;
        let era = if ev.is_undefined() {
            None
        } else {
            let s = to_primitive_require_string(vm, ev, "era")?;
            Some(TinyAsciiStr::<19>::try_from_str(&s).map_err(|_| vm.range_error("Invalid era"))?)
        };
        let eyv = get_prop(vm, obj, "eraYear")?;
        let era_year = if eyv.is_undefined() {
            None
        } else {
            Some(to_integer_with_truncation_i32(vm, eyv)?)
        };
        (era, era_year)
    } else {
        (None, None)
    };
    let mv = get_prop(vm, obj, "month")?;
    let month = if mv.is_undefined() {
        None
    } else {
        Some(to_clamped_pos_u8(vm, mv)?)
    };
    let mcv = get_prop(vm, obj, "monthCode")?;
    let month_code = if mcv.is_undefined() {
        None
    } else {
        let s = to_primitive_require_string(vm, mcv, "monthCode")?;
        Some(MonthCode::try_from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?)
    };
    let year = if read_year {
        let yv = get_prop(vm, obj, "year")?;
        if yv.is_undefined() {
            None
        } else {
            Some(to_integer_with_truncation_i32(vm, yv)?)
        }
    } else {
        None
    };
    Ok(CalendarFields {
        year,
        month,
        month_code,
        day,
        era,
        era_year,
    })
}

/// Reads `PartialTime` from `obj` in strict alphabetical order:
/// `hour`, `microsecond`, `millisecond`, `minute`, `nanosecond`, `second`.
pub(crate) fn read_partial_time(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> Result<PartialTime, JsException> {
    let hv = get_prop(vm, obj, "hour")?;
    let hour = if hv.is_undefined() {
        None
    } else {
        Some(to_clamped_u8(vm, hv)?)
    };
    let usv = get_prop(vm, obj, "microsecond")?;
    let microsecond = if usv.is_undefined() {
        None
    } else {
        Some(to_clamped_u16(vm, usv)?)
    };
    let msv = get_prop(vm, obj, "millisecond")?;
    let millisecond = if msv.is_undefined() {
        None
    } else {
        Some(to_clamped_u16(vm, msv)?)
    };
    let minv = get_prop(vm, obj, "minute")?;
    let minute = if minv.is_undefined() {
        None
    } else {
        Some(to_clamped_u8(vm, minv)?)
    };
    let nsv = get_prop(vm, obj, "nanosecond")?;
    let nanosecond = if nsv.is_undefined() {
        None
    } else {
        Some(to_clamped_u16(vm, nsv)?)
    };
    let secv = get_prop(vm, obj, "second")?;
    let second = if secv.is_undefined() {
        None
    } else {
        Some(to_clamped_u8(vm, secv)?)
    };
    Ok(PartialTime {
        hour,
        minute,
        second,
        millisecond,
        microsecond,
        nanosecond,
    })
}

/// Reads calendar + time fields together in strict alphabetical order for `PlainDateTime`:
/// `day`, `era`, `eraYear`, `hour`, `microsecond`, `millisecond`, `minute`,
/// `month`, `monthCode`, `nanosecond`, `second`, `year`.
pub(crate) fn read_date_time_fields(
    vm: &mut JsVm,
    obj: HeapHandle,
    has_eras: bool,
) -> Result<(CalendarFields, PartialTime), JsException> {
    let dv = get_prop(vm, obj, "day")?;
    let day = if dv.is_undefined() {
        None
    } else {
        Some(to_clamped_pos_u8(vm, dv)?)
    };
    let (era, era_year) = if has_eras {
        let ev = get_prop(vm, obj, "era")?;
        let era = if ev.is_undefined() {
            None
        } else {
            let s = to_primitive_require_string(vm, ev, "era")?;
            Some(TinyAsciiStr::<19>::try_from_str(&s).map_err(|_| vm.range_error("Invalid era"))?)
        };
        let eyv = get_prop(vm, obj, "eraYear")?;
        let era_year = if eyv.is_undefined() {
            None
        } else {
            Some(to_integer_with_truncation_i32(vm, eyv)?)
        };
        (era, era_year)
    } else {
        (None, None)
    };
    let hv = get_prop(vm, obj, "hour")?;
    let hour = if hv.is_undefined() {
        None
    } else {
        Some(to_clamped_u8(vm, hv)?)
    };
    let usv = get_prop(vm, obj, "microsecond")?;
    let microsecond = if usv.is_undefined() {
        None
    } else {
        Some(to_clamped_u16(vm, usv)?)
    };
    let msv = get_prop(vm, obj, "millisecond")?;
    let millisecond = if msv.is_undefined() {
        None
    } else {
        Some(to_clamped_u16(vm, msv)?)
    };
    let minv = get_prop(vm, obj, "minute")?;
    let minute = if minv.is_undefined() {
        None
    } else {
        Some(to_clamped_u8(vm, minv)?)
    };
    let mv = get_prop(vm, obj, "month")?;
    let month = if mv.is_undefined() {
        None
    } else {
        Some(to_clamped_pos_u8(vm, mv)?)
    };
    let mcv = get_prop(vm, obj, "monthCode")?;
    let month_code = if mcv.is_undefined() {
        None
    } else {
        let s = to_primitive_require_string(vm, mcv, "monthCode")?;
        Some(MonthCode::try_from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?)
    };
    let nsv = get_prop(vm, obj, "nanosecond")?;
    let nanosecond = if nsv.is_undefined() {
        None
    } else {
        Some(to_clamped_u16(vm, nsv)?)
    };
    let secv = get_prop(vm, obj, "second")?;
    let second = if secv.is_undefined() {
        None
    } else {
        Some(to_clamped_u8(vm, secv)?)
    };
    let yv = get_prop(vm, obj, "year")?;
    let year = if yv.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_i32(vm, yv)?)
    };
    Ok((
        CalendarFields {
            year,
            month,
            month_code,
            day,
            era,
            era_year,
        },
        PartialTime {
            hour,
            minute,
            second,
            millisecond,
            microsecond,
            nanosecond,
        },
    ))
}
