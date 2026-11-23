//! ECMAScript `Temporal.Duration` (§7) backed by `temporal_rs::Duration`.

use std::cmp::Ordering;

use temporal_rs::options::{RoundingOptions, ToStringRoundingOptions};
use temporal_rs::partial::PartialDuration;
use temporal_rs::Duration;

use crate::heap::TemporalValue;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm};

use super::options::{
    arg, get_options_object, get_relative_to_option, get_rounding_increment_option,
    get_rounding_mode_option, get_to_string_rounding_options, get_unit_option, parse_unit_str,
    to_integer_if_integral_i128, to_integer_if_integral_i64, to_partial_duration,
};
use super::{
    alloc_temporal, ctor_proto_from_new_target, define_getter, define_method, get_temporal,
    install_ctor_and_proto, temporal_err, temporal_value_of,
};

pub(crate) fn require_duration(vm: &mut JsVm, this_val: JsValue) -> Result<Duration, JsException> {
    match get_temporal(vm, this_val) {
        Some(TemporalValue::Duration(d)) => Ok(*d),
        _ => Err(vm.type_error("Receiver is not a Temporal.Duration")),
    }
}

/// ECMA-262 `ToTemporalDuration(item)`.
pub(crate) fn to_temporal_duration(vm: &mut JsVm, item: JsValue) -> Result<Duration, JsException> {
    if let Some(TemporalValue::Duration(d)) = get_temporal(vm, item) {
        return Ok(*d);
    }
    if let Some(sh) = item.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        return Duration::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e));
    }
    let Some(h) = item.as_object() else {
        return Err(
            vm.type_error("Temporal.Duration requires a Duration, object, or ISO duration string")
        );
    };
    let partial = to_partial_duration(vm, h)?;
    Duration::from_partial_duration(partial).map_err(|e| temporal_err(vm, e))
}

fn wrap_duration(vm: &mut JsVm, dur: Duration) -> JsValue {
    alloc_temporal(vm, vm.temporal_duration_proto, TemporalValue::Duration(dur))
}

fn duration_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto = ctor_proto_from_new_target(vm, vm.temporal_duration_proto, "Duration")?;
    let opt_i64 = |vm: &mut JsVm, i: usize| -> Result<i64, JsException> {
        let v = arg(args, i);
        if v.is_undefined() {
            Ok(0)
        } else {
            to_integer_if_integral_i64(vm, v)
        }
    };
    let opt_i128 = |vm: &mut JsVm, i: usize| -> Result<i128, JsException> {
        let v = arg(args, i);
        if v.is_undefined() {
            Ok(0)
        } else {
            to_integer_if_integral_i128(vm, v)
        }
    };
    let years = opt_i64(vm, 0)?;
    let months = opt_i64(vm, 1)?;
    let weeks = opt_i64(vm, 2)?;
    let days = opt_i64(vm, 3)?;
    let hours = opt_i64(vm, 4)?;
    let minutes = opt_i64(vm, 5)?;
    let seconds = opt_i64(vm, 6)?;
    let milliseconds = opt_i64(vm, 7)?;
    let microseconds = opt_i128(vm, 8)?;
    let nanoseconds = opt_i128(vm, 9)?;
    let dur = Duration::new(
        years,
        months,
        weeks,
        days,
        hours,
        minutes,
        seconds,
        milliseconds,
        microseconds,
        nanoseconds,
    )
    .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(vm, proto, TemporalValue::Duration(dur)))
}

fn duration_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    Ok(wrap_duration(vm, dur))
}

fn duration_compare(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let one = to_temporal_duration(vm, arg(args, 0))?;
    let two = to_temporal_duration(vm, arg(args, 1))?;
    let rel = match get_options_object(vm, arg(args, 2))? {
        Some(h) => get_relative_to_option(vm, h)?,
        None => None,
    };
    let ord = one.compare(&two, rel).map_err(|e| temporal_err(vm, e))?;
    let res = match ord {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(JsValue::from_i32(res))
}

macro_rules! dur_getter {
    ($fn_name:ident, $method:ident) => {
        fn $fn_name(
            vm: &mut JsVm,
            this_val: JsValue,
            _args: &[JsValue],
        ) -> Result<JsValue, JsException> {
            let d = require_duration(vm, this_val)?;
            let v = d.$method() as f64;
            Ok(JsValue::from_f64(if v == 0.0 { 0.0 } else { v }))
        }
    };
}

dur_getter!(dur_years, years);
dur_getter!(dur_months, months);
dur_getter!(dur_weeks, weeks);
dur_getter!(dur_days, days);
dur_getter!(dur_hours, hours);
dur_getter!(dur_minutes, minutes);
dur_getter!(dur_seconds, seconds);
dur_getter!(dur_milliseconds, milliseconds);
dur_getter!(dur_microseconds, microseconds);
dur_getter!(dur_nanoseconds, nanoseconds);

fn dur_sign(vm: &mut JsVm, this_val: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    Ok(JsValue::from_i32(i32::from(d.sign() as i8)))
}

fn dur_blank(vm: &mut JsVm, this_val: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    Ok(JsValue::from_bool(d.is_zero()))
}

fn dur_with(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    let Some(h) = arg(args, 0).as_object() else {
        return Err(vm.type_error("Temporal.Duration.prototype.with requires an object"));
    };
    let p = to_partial_duration(vm, h)?;
    let merged = PartialDuration {
        years: Some(p.years.unwrap_or_else(|| d.years())),
        months: Some(p.months.unwrap_or_else(|| d.months())),
        weeks: Some(p.weeks.unwrap_or_else(|| d.weeks())),
        days: Some(p.days.unwrap_or_else(|| d.days())),
        hours: Some(p.hours.unwrap_or_else(|| d.hours())),
        minutes: Some(p.minutes.unwrap_or_else(|| d.minutes())),
        seconds: Some(p.seconds.unwrap_or_else(|| d.seconds())),
        milliseconds: Some(p.milliseconds.unwrap_or_else(|| d.milliseconds())),
        microseconds: Some(p.microseconds.unwrap_or_else(|| d.microseconds())),
        nanoseconds: Some(p.nanoseconds.unwrap_or_else(|| d.nanoseconds())),
    };
    let out = Duration::from_partial_duration(merged).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_duration(vm, out))
}

fn dur_negated(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    Ok(wrap_duration(vm, d.negated()))
}

fn dur_abs(vm: &mut JsVm, this_val: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    Ok(wrap_duration(vm, d.abs()))
}

fn dur_add(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    let other = to_temporal_duration(vm, arg(args, 0))?;
    let res = d.add(&other).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_duration(vm, res))
}

fn dur_subtract(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    let other = to_temporal_duration(vm, arg(args, 0))?;
    let res = d.subtract(&other).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_duration(vm, res))
}

fn dur_round(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    let round_to = arg(args, 0);
    if round_to.is_undefined() {
        return Err(vm.type_error("Duration.prototype.round requires a unit or options object"));
    }
    let mut opts = RoundingOptions::default();
    opts.largest_unit = None;
    let rel = if let Some(sh) = round_to.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        opts.smallest_unit = Some(parse_unit_str(vm, &s, false)?);
        None
    } else if let Some(h) = round_to.as_object() {
        // Alphabetical order: largestUnit, relativeTo, roundingIncrement, roundingMode, smallestUnit
        opts.largest_unit = get_unit_option(vm, h, "largestUnit", true)?;
        let rel = get_relative_to_option(vm, h)?;
        opts.increment = get_rounding_increment_option(vm, h)?;
        opts.rounding_mode = get_rounding_mode_option(vm, h)?;
        opts.smallest_unit = get_unit_option(vm, h, "smallestUnit", false)?;
        rel
    } else {
        return Err(vm.type_error("Duration.prototype.round argument must be a string or object"));
    };
    let out = d.round(opts, rel).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_duration(vm, out))
}

fn dur_total(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    let total_of = arg(args, 0);
    if total_of.is_undefined() {
        return Err(vm.type_error("Duration.prototype.total requires a unit or options object"));
    }
    let (unit, rel) = if let Some(sh) = total_of.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        (parse_unit_str(vm, &s, false)?, None)
    } else if let Some(h) = total_of.as_object() {
        // Alphabetical order: relativeTo, unit
        let rel = get_relative_to_option(vm, h)?;
        let Some(u) = get_unit_option(vm, h, "unit", false)? else {
            return Err(vm.range_error("Duration.prototype.total requires a 'unit' option"));
        };
        (u, rel)
    } else {
        return Err(vm.type_error("Duration.prototype.total argument must be a string or object"));
    };
    let total = d.total(unit, rel).map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_f64(total.as_inner()))
}

fn dur_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    let opts = match get_options_object(vm, arg(args, 0))? {
        Some(h) => get_to_string_rounding_options(vm, h)?,
        None => ToStringRoundingOptions::default(),
    };
    let s = d
        .as_temporal_string(opts)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn dur_to_json(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    let s = d
        .as_temporal_string(ToStringRoundingOptions::default())
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn dur_to_locale_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_duration(vm, this_val)?;
    let s = d
        .as_temporal_string(ToStringRoundingOptions::default())
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

pub(crate) fn install_duration(vm: &mut JsVm, ns: HeapHandle) {
    let proto = vm.temporal_duration_proto;
    let ctor = install_ctor_and_proto(vm, ns, "Duration", 0, duration_ctor, proto);

    define_method(vm, ctor, "from", 1, duration_from);
    define_method(vm, ctor, "compare", 2, duration_compare);

    define_getter(vm, proto, "years", dur_years);
    define_getter(vm, proto, "months", dur_months);
    define_getter(vm, proto, "weeks", dur_weeks);
    define_getter(vm, proto, "days", dur_days);
    define_getter(vm, proto, "hours", dur_hours);
    define_getter(vm, proto, "minutes", dur_minutes);
    define_getter(vm, proto, "seconds", dur_seconds);
    define_getter(vm, proto, "milliseconds", dur_milliseconds);
    define_getter(vm, proto, "microseconds", dur_microseconds);
    define_getter(vm, proto, "nanoseconds", dur_nanoseconds);
    define_getter(vm, proto, "sign", dur_sign);
    define_getter(vm, proto, "blank", dur_blank);

    define_method(vm, proto, "with", 1, dur_with);
    define_method(vm, proto, "negated", 0, dur_negated);
    define_method(vm, proto, "abs", 0, dur_abs);
    define_method(vm, proto, "add", 1, dur_add);
    define_method(vm, proto, "subtract", 1, dur_subtract);
    define_method(vm, proto, "round", 1, dur_round);
    define_method(vm, proto, "total", 1, dur_total);
    define_method(vm, proto, "toString", 0, dur_to_string);
    define_method(vm, proto, "toJSON", 0, dur_to_json);
    define_method(vm, proto, "toLocaleString", 0, dur_to_locale_string);
    define_method(vm, proto, "valueOf", 0, temporal_value_of);
}
