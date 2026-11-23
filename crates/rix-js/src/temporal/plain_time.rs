//! ECMAScript `Temporal.PlainTime` (§4) and `Temporal.PlainDateTime` (§5)
//! backed by `temporal_rs`.

use std::cmp::Ordering;

use temporal_rs::fields::DateTimeFields;
use temporal_rs::options::{Disambiguation, DisplayCalendar, ToStringRoundingOptions};
use temporal_rs::partial::PartialDateTime;
use temporal_rs::{Calendar, PlainDateTime, PlainTime};

use crate::heap::TemporalValue;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm};

use super::duration::to_temporal_duration;
use super::options::{
    arg, calendar_has_eras, get_difference_settings, get_disambiguation_option,
    get_display_calendar_option, get_fractional_second_digits_option, get_options_object,
    get_overflow_option, get_prop, get_rounding_mode_option, get_simple_rounding_options,
    get_to_string_rounding_options, get_unit_option, parse_calendar_ctor_arg,
    read_date_time_fields, read_partial_time, require_partial_temporal_object,
    to_integer_with_truncation_i32, to_integer_with_truncation_u16, to_integer_with_truncation_u8,
    to_temporal_calendar_identifier, to_temporal_time_zone_identifier,
};
use super::plain_date::wrap_plain_date;
use super::{
    alloc_temporal, ctor_proto_from_new_target, define_getter, define_method, get_temporal,
    install_ctor_and_proto, temporal_err, temporal_value_of,
};

// ===========================================================================
// Temporal.PlainTime (§4)
// ===========================================================================

pub(crate) fn require_plain_time(
    vm: &mut JsVm,
    this_val: JsValue,
) -> Result<PlainTime, JsException> {
    match get_temporal(vm, this_val) {
        Some(TemporalValue::PlainTime(t)) => Ok(*t),
        _ => Err(vm.type_error("Receiver is not a Temporal.PlainTime")),
    }
}

pub(crate) fn wrap_plain_time(vm: &mut JsVm, t: PlainTime) -> JsValue {
    alloc_temporal(
        vm,
        vm.temporal_plain_time_proto,
        TemporalValue::PlainTime(t),
    )
}

pub(crate) fn to_temporal_plain_time(
    vm: &mut JsVm,
    item: JsValue,
    options_val: JsValue,
) -> Result<PlainTime, JsException> {
    if let Some(tv) = get_temporal(vm, item) {
        match tv {
            TemporalValue::PlainTime(t) => {
                let t = *t;
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(t);
            }
            TemporalValue::PlainDateTime(dt) => {
                let t = dt.to_plain_time();
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(t);
            }
            TemporalValue::ZonedDateTime(zdt) => {
                let t = zdt.to_plain_time();
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(t);
            }
            _ => {}
        }
    }
    if let Some(sh) = item.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        let t = PlainTime::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?;
        let opts = get_options_object(vm, options_val)?;
        let _ = get_overflow_option(vm, opts)?;
        return Ok(t);
    }
    let Some(h) = item.as_object() else {
        return Err(vm.type_error("Temporal.PlainTime.from requires a string or object"));
    };
    let partial = read_partial_time(vm, h)?;
    if partial.is_empty() {
        return Err(vm.type_error("PlainTime-like object must specify at least one time field"));
    }
    let opts = get_options_object(vm, options_val)?;
    let overflow = get_overflow_option(vm, opts)?;
    PlainTime::from_partial(partial, overflow).map_err(|e| temporal_err(vm, e))
}

fn pt_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto = ctor_proto_from_new_target(vm, vm.temporal_plain_time_proto, "PlainTime")?;
    let opt_u8 = |vm: &mut JsVm, i: usize| -> Result<u8, JsException> {
        let v = arg(args, i);
        if v.is_undefined() {
            Ok(0)
        } else {
            to_integer_with_truncation_u8(vm, v)
        }
    };
    let opt_u16 = |vm: &mut JsVm, i: usize| -> Result<u16, JsException> {
        let v = arg(args, i);
        if v.is_undefined() {
            Ok(0)
        } else {
            to_integer_with_truncation_u16(vm, v)
        }
    };
    let h = opt_u8(vm, 0)?;
    let min = opt_u8(vm, 1)?;
    let s = opt_u8(vm, 2)?;
    let ms = opt_u16(vm, 3)?;
    let us = opt_u16(vm, 4)?;
    let ns = opt_u16(vm, 5)?;
    let t = PlainTime::try_new(h, min, s, ms, us, ns).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(vm, proto, TemporalValue::PlainTime(t)))
}

fn pt_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let t = to_temporal_plain_time(vm, arg(args, 0), arg(args, 1))?;
    Ok(wrap_plain_time(vm, t))
}

fn pt_compare(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let one = to_temporal_plain_time(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let two = to_temporal_plain_time(vm, arg(args, 1), JsValue::UNDEFINED)?;
    let c = match one.cmp(&two) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(JsValue::from_i32(c))
}

fn pt_hour(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_time(vm, this_val)?.hour(),
    )))
}
fn pt_minute(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_time(vm, this_val)?.minute(),
    )))
}
fn pt_second(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_time(vm, this_val)?.second(),
    )))
}
fn pt_millisecond(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_time(vm, this_val)?.millisecond(),
    )))
}
fn pt_microsecond(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_time(vm, this_val)?.microsecond(),
    )))
}
fn pt_nanosecond(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_time(vm, this_val)?.nanosecond(),
    )))
}

fn pt_add(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let res = t.add(&dur).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_time(vm, res))
}

fn pt_subtract(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let res = t.subtract(&dur).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_time(vm, res))
}

fn pt_with(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let h = require_partial_temporal_object(vm, arg(args, 0))?;
    let partial = read_partial_time(vm, h)?;
    if partial.is_empty() {
        return Err(vm.type_error("PlainTime.prototype.with requires at least one time field"));
    }
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = t.with(partial, overflow).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_time(vm, res))
}

fn pt_until(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let other = to_temporal_plain_time(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = t.until(&other, settings).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn pt_since(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let other = to_temporal_plain_time(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = t.since(&other, settings).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn pt_round(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let opts = get_simple_rounding_options(vm, arg(args, 0))?;
    let res = t.round(opts).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_time(vm, res))
}

fn pt_equals(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let other = to_temporal_plain_time(vm, arg(args, 0), JsValue::UNDEFINED)?;
    Ok(JsValue::from_bool(t == other))
}

fn pt_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let opts = match get_options_object(vm, arg(args, 0))? {
        Some(h) => get_to_string_rounding_options(vm, h)?,
        None => ToStringRoundingOptions::default(),
    };
    let s = t.to_ixdtf_string(opts).map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn pt_to_json(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let t = require_plain_time(vm, this_val)?;
    let s = t
        .to_ixdtf_string(ToStringRoundingOptions::default())
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

// ===========================================================================
// Temporal.PlainDateTime (§5)
// ===========================================================================

pub(crate) fn require_plain_date_time(
    vm: &mut JsVm,
    this_val: JsValue,
) -> Result<PlainDateTime, JsException> {
    match get_temporal(vm, this_val) {
        Some(TemporalValue::PlainDateTime(dt)) => Ok(dt.clone()),
        _ => Err(vm.type_error("Receiver is not a Temporal.PlainDateTime")),
    }
}

pub(crate) fn wrap_plain_date_time(vm: &mut JsVm, dt: PlainDateTime) -> JsValue {
    alloc_temporal(
        vm,
        vm.temporal_plain_date_time_proto,
        TemporalValue::PlainDateTime(dt),
    )
}

pub(crate) fn to_temporal_plain_date_time(
    vm: &mut JsVm,
    item: JsValue,
    options_val: JsValue,
) -> Result<PlainDateTime, JsException> {
    if let Some(tv) = get_temporal(vm, item) {
        match tv {
            TemporalValue::PlainDateTime(dt) => {
                let dt = dt.clone();
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(dt);
            }
            TemporalValue::PlainDate(d) => {
                let dt = d
                    .to_plain_date_time(None)
                    .map_err(|e| temporal_err(vm, e))?;
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(dt);
            }
            TemporalValue::ZonedDateTime(zdt) => {
                let dt = zdt.to_plain_date_time();
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(dt);
            }
            _ => {}
        }
    }
    if let Some(sh) = item.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        let dt = PlainDateTime::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?;
        let opts = get_options_object(vm, options_val)?;
        let _ = get_overflow_option(vm, opts)?;
        return Ok(dt);
    }
    let Some(h) = item.as_object() else {
        return Err(vm.type_error("Temporal.PlainDateTime.from requires a string or object"));
    };
    let cal_v = get_prop(vm, h, "calendar")?;
    let calendar = if cal_v.is_undefined() {
        Calendar::ISO
    } else {
        to_temporal_calendar_identifier(vm, cal_v)?
    };
    let (calendar_fields, time) = read_date_time_fields(vm, h, calendar_has_eras(&calendar))?;
    let opts = get_options_object(vm, options_val)?;
    let overflow = get_overflow_option(vm, opts)?;
    PlainDateTime::from_partial(
        PartialDateTime {
            fields: DateTimeFields {
                calendar_fields,
                time,
            },
            calendar,
        },
        overflow,
    )
    .map_err(|e| temporal_err(vm, e))
}

fn pdt_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto = ctor_proto_from_new_target(vm, vm.temporal_plain_date_time_proto, "PlainDateTime")?;
    let y = to_integer_with_truncation_i32(vm, arg(args, 0))?;
    let m = to_integer_with_truncation_u8(vm, arg(args, 1))?;
    let d = to_integer_with_truncation_u8(vm, arg(args, 2))?;
    let opt_u8 = |vm: &mut JsVm, i: usize| -> Result<u8, JsException> {
        let v = arg(args, i);
        if v.is_undefined() {
            Ok(0)
        } else {
            to_integer_with_truncation_u8(vm, v)
        }
    };
    let opt_u16 = |vm: &mut JsVm, i: usize| -> Result<u16, JsException> {
        let v = arg(args, i);
        if v.is_undefined() {
            Ok(0)
        } else {
            to_integer_with_truncation_u16(vm, v)
        }
    };
    let h = opt_u8(vm, 3)?;
    let min = opt_u8(vm, 4)?;
    let s = opt_u8(vm, 5)?;
    let ms = opt_u16(vm, 6)?;
    let us = opt_u16(vm, 7)?;
    let ns = opt_u16(vm, 8)?;
    let cal = parse_calendar_ctor_arg(vm, arg(args, 9))?;
    let dt = PlainDateTime::try_new(y, m, d, h, min, s, ms, us, ns, cal)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(vm, proto, TemporalValue::PlainDateTime(dt)))
}

fn pdt_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = to_temporal_plain_date_time(vm, arg(args, 0), arg(args, 1))?;
    Ok(wrap_plain_date_time(vm, dt))
}

fn pdt_compare(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let one = to_temporal_plain_date_time(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let two = to_temporal_plain_date_time(vm, arg(args, 1), JsValue::UNDEFINED)?;
    let c = match one.compare_iso(&two) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(JsValue::from_i32(c))
}

fn pdt_calendar_id(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(dt.calendar().identifier()),
    ))
}
fn pdt_era(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    Ok(match dt.era() {
        Some(e) => JsValue::from_string(vm.heap.alloc_string(e.as_str())),
        None => JsValue::UNDEFINED,
    })
}
fn pdt_era_year(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(require_plain_date_time(vm, this_val)?
        .era_year()
        .map_or(JsValue::UNDEFINED, JsValue::from_i32))
}
fn pdt_year(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(
        require_plain_date_time(vm, this_val)?.year(),
    ))
}
fn pdt_month(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.month(),
    )))
}
fn pdt_month_code(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(dt.month_code().as_str()),
    ))
}
fn pdt_day(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.day(),
    )))
}
fn pdt_hour(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.hour(),
    )))
}
fn pdt_minute(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.minute(),
    )))
}
fn pdt_second(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.second(),
    )))
}
fn pdt_millisecond(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.millisecond(),
    )))
}
fn pdt_microsecond(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.microsecond(),
    )))
}
fn pdt_nanosecond(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.nanosecond(),
    )))
}
fn pdt_day_of_week(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.day_of_week(),
    )))
}
fn pdt_day_of_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.day_of_year(),
    )))
}
fn pdt_week_of_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(require_plain_date_time(vm, this_val)?
        .week_of_year()
        .map_or(JsValue::UNDEFINED, |w| JsValue::from_i32(i32::from(w))))
}
fn pdt_year_of_week(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(require_plain_date_time(vm, this_val)?
        .year_of_week()
        .map_or(JsValue::UNDEFINED, JsValue::from_i32))
}
fn pdt_days_in_week(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.days_in_week(),
    )))
}
fn pdt_days_in_month(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.days_in_month(),
    )))
}
fn pdt_days_in_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.days_in_year(),
    )))
}
fn pdt_months_in_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date_time(vm, this_val)?.months_in_year(),
    )))
}
fn pdt_in_leap_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_bool(
        require_plain_date_time(vm, this_val)?.in_leap_year(),
    ))
}

fn pdt_with(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let h = require_partial_temporal_object(vm, arg(args, 0))?;
    let (calendar_fields, time) = read_date_time_fields(vm, h, calendar_has_eras(dt.calendar()))?;
    let fields = DateTimeFields {
        calendar_fields,
        time,
    };
    if fields.is_empty() {
        return Err(vm.type_error("PlainDateTime.prototype.with requires at least one field"));
    }
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = dt.with(fields, overflow).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date_time(vm, res))
}

fn pdt_with_plain_time(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let pt_v = arg(args, 0);
    let pt = if pt_v.is_undefined() {
        None
    } else {
        Some(to_temporal_plain_time(vm, pt_v, JsValue::UNDEFINED)?)
    };
    let res = dt.with_time(pt).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date_time(vm, res))
}

fn pdt_with_calendar(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let cal = to_temporal_calendar_identifier(vm, arg(args, 0))?;
    Ok(wrap_plain_date_time(vm, dt.with_calendar(cal)))
}

fn pdt_add(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = dt.add(&dur, overflow).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date_time(vm, res))
}

fn pdt_subtract(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = dt
        .subtract(&dur, overflow)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date_time(vm, res))
}

fn pdt_until(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let other = to_temporal_plain_date_time(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = dt
        .until(&other, settings)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn pdt_since(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let other = to_temporal_plain_date_time(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = dt
        .since(&other, settings)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn pdt_round(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let opts = get_simple_rounding_options(vm, arg(args, 0))?;
    let res = dt.round(opts).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date_time(vm, res))
}

fn pdt_equals(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let other = to_temporal_plain_date_time(vm, arg(args, 0), JsValue::UNDEFINED)?;
    Ok(JsValue::from_bool(dt == other))
}

fn pdt_to_plain_date(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_plain_date_time(vm, this_val)?.to_plain_date();
    Ok(wrap_plain_date(vm, d))
}

fn pdt_to_plain_time(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    let t = require_plain_date_time(vm, this_val)?.to_plain_time();
    Ok(wrap_plain_time(vm, t))
}

fn pdt_to_zoned_date_time(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let tz = to_temporal_time_zone_identifier(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let dis = get_disambiguation_option(vm, opts)?.unwrap_or(Disambiguation::Compatible);
    let zdt = dt
        .to_zoned_date_time(tz, dis)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_zoned_date_time_proto,
        TemporalValue::ZonedDateTime(zdt),
    ))
}

fn pdt_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let (cal, rounding) = if let Some(h) = get_options_object(vm, arg(args, 0))? {
        // Alphabetical: calendarName, fractionalSecondDigits, roundingMode, smallestUnit
        let cal = get_display_calendar_option(vm, h)?;
        let precision = get_fractional_second_digits_option(vm, h)?;
        let rounding_mode = get_rounding_mode_option(vm, h)?;
        let smallest_unit = get_unit_option(vm, h, "smallestUnit", false)?;
        (
            cal,
            ToStringRoundingOptions {
                precision,
                smallest_unit,
                rounding_mode,
            },
        )
    } else {
        (DisplayCalendar::Auto, ToStringRoundingOptions::default())
    };
    let s = dt
        .to_ixdtf_string(rounding, cal)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn pdt_to_json(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let dt = require_plain_date_time(vm, this_val)?;
    let s = dt
        .to_ixdtf_string(ToStringRoundingOptions::default(), DisplayCalendar::Auto)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

// ===========================================================================
// Installers
// ===========================================================================

pub(crate) fn install_plain_time_types(vm: &mut JsVm, ns: HeapHandle) {
    // PlainTime
    let pt_proto = vm.temporal_plain_time_proto;
    let pt_c = install_ctor_and_proto(vm, ns, "PlainTime", 0, pt_ctor, pt_proto);
    define_method(vm, pt_c, "from", 1, pt_from);
    define_method(vm, pt_c, "compare", 2, pt_compare);
    define_getter(vm, pt_proto, "hour", pt_hour);
    define_getter(vm, pt_proto, "minute", pt_minute);
    define_getter(vm, pt_proto, "second", pt_second);
    define_getter(vm, pt_proto, "millisecond", pt_millisecond);
    define_getter(vm, pt_proto, "microsecond", pt_microsecond);
    define_getter(vm, pt_proto, "nanosecond", pt_nanosecond);
    define_method(vm, pt_proto, "add", 1, pt_add);
    define_method(vm, pt_proto, "subtract", 1, pt_subtract);
    define_method(vm, pt_proto, "with", 1, pt_with);
    define_method(vm, pt_proto, "until", 1, pt_until);
    define_method(vm, pt_proto, "since", 1, pt_since);
    define_method(vm, pt_proto, "round", 1, pt_round);
    define_method(vm, pt_proto, "equals", 1, pt_equals);
    define_method(vm, pt_proto, "toString", 0, pt_to_string);
    define_method(vm, pt_proto, "toJSON", 0, pt_to_json);
    define_method(vm, pt_proto, "toLocaleString", 0, pt_to_json);
    define_method(vm, pt_proto, "valueOf", 0, temporal_value_of);

    // PlainDateTime
    let dt_proto = vm.temporal_plain_date_time_proto;
    let dt_c = install_ctor_and_proto(vm, ns, "PlainDateTime", 3, pdt_ctor, dt_proto);
    define_method(vm, dt_c, "from", 1, pdt_from);
    define_method(vm, dt_c, "compare", 2, pdt_compare);
    define_getter(vm, dt_proto, "calendarId", pdt_calendar_id);
    define_getter(vm, dt_proto, "era", pdt_era);
    define_getter(vm, dt_proto, "eraYear", pdt_era_year);
    define_getter(vm, dt_proto, "year", pdt_year);
    define_getter(vm, dt_proto, "month", pdt_month);
    define_getter(vm, dt_proto, "monthCode", pdt_month_code);
    define_getter(vm, dt_proto, "day", pdt_day);
    define_getter(vm, dt_proto, "hour", pdt_hour);
    define_getter(vm, dt_proto, "minute", pdt_minute);
    define_getter(vm, dt_proto, "second", pdt_second);
    define_getter(vm, dt_proto, "millisecond", pdt_millisecond);
    define_getter(vm, dt_proto, "microsecond", pdt_microsecond);
    define_getter(vm, dt_proto, "nanosecond", pdt_nanosecond);
    define_getter(vm, dt_proto, "dayOfWeek", pdt_day_of_week);
    define_getter(vm, dt_proto, "dayOfYear", pdt_day_of_year);
    define_getter(vm, dt_proto, "weekOfYear", pdt_week_of_year);
    define_getter(vm, dt_proto, "yearOfWeek", pdt_year_of_week);
    define_getter(vm, dt_proto, "daysInWeek", pdt_days_in_week);
    define_getter(vm, dt_proto, "daysInMonth", pdt_days_in_month);
    define_getter(vm, dt_proto, "daysInYear", pdt_days_in_year);
    define_getter(vm, dt_proto, "monthsInYear", pdt_months_in_year);
    define_getter(vm, dt_proto, "inLeapYear", pdt_in_leap_year);
    define_method(vm, dt_proto, "with", 1, pdt_with);
    define_method(vm, dt_proto, "withPlainTime", 0, pdt_with_plain_time);
    define_method(vm, dt_proto, "withCalendar", 1, pdt_with_calendar);
    define_method(vm, dt_proto, "add", 1, pdt_add);
    define_method(vm, dt_proto, "subtract", 1, pdt_subtract);
    define_method(vm, dt_proto, "until", 1, pdt_until);
    define_method(vm, dt_proto, "since", 1, pdt_since);
    define_method(vm, dt_proto, "round", 1, pdt_round);
    define_method(vm, dt_proto, "equals", 1, pdt_equals);
    define_method(vm, dt_proto, "toPlainDate", 0, pdt_to_plain_date);
    define_method(vm, dt_proto, "toPlainTime", 0, pdt_to_plain_time);
    define_method(vm, dt_proto, "toZonedDateTime", 1, pdt_to_zoned_date_time);
    define_method(vm, dt_proto, "toString", 0, pdt_to_string);
    define_method(vm, dt_proto, "toJSON", 0, pdt_to_json);
    define_method(vm, dt_proto, "toLocaleString", 0, pdt_to_json);
    define_method(vm, dt_proto, "valueOf", 0, temporal_value_of);
}
