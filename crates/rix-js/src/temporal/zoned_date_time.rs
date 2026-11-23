//! ECMAScript `Temporal.ZonedDateTime` (§6) backed by `temporal_rs::ZonedDateTime`.

use std::cmp::Ordering;
use std::str::FromStr;

use temporal_rs::fields::{CalendarFields, ZonedDateTimeFields};
use temporal_rs::options::{
    Disambiguation, DisplayCalendar, DisplayOffset, DisplayTimeZone, OffsetDisambiguation,
    ToStringRoundingOptions,
};
use temporal_rs::partial::{PartialTime, PartialZonedDateTime};
use temporal_rs::provider::TransitionDirection;
use temporal_rs::{Calendar, MonthCode, TimeZone, TinyAsciiStr, UtcOffset, ZonedDateTime};

use crate::bigint::JsBigInt;
use crate::heap::TemporalValue;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm};

use super::duration::to_temporal_duration;
use super::options::{
    arg, calendar_has_eras, get_difference_settings, get_disambiguation_option,
    get_display_calendar_option, get_display_offset_option, get_display_time_zone_option,
    get_fractional_second_digits_option, get_offset_disambiguation_option, get_opt_string,
    get_options_object, get_overflow_option, get_prop, get_rounding_mode_option,
    get_simple_rounding_options, get_unit_option, parse_calendar_ctor_arg,
    require_partial_temporal_object, to_bigint_i128, to_clamped_pos_u8, to_clamped_u16,
    to_clamped_u8, to_integer_with_truncation_i32, to_primitive_require_string,
    to_temporal_calendar_identifier, to_temporal_time_zone_identifier,
};
use super::plain_date::wrap_plain_date;
use super::plain_time::{to_temporal_plain_time, wrap_plain_date_time, wrap_plain_time};
use super::{
    alloc_temporal, ctor_proto_from_new_target, define_getter, define_method, get_temporal,
    install_ctor_and_proto, temporal_err, temporal_value_of,
};

pub(crate) fn require_zdt(vm: &mut JsVm, this_val: JsValue) -> Result<ZonedDateTime, JsException> {
    match get_temporal(vm, this_val) {
        Some(TemporalValue::ZonedDateTime(z)) => Ok(z.clone()),
        _ => Err(vm.type_error("Receiver is not a Temporal.ZonedDateTime")),
    }
}

pub(crate) fn wrap_zdt(vm: &mut JsVm, z: ZonedDateTime) -> JsValue {
    alloc_temporal(
        vm,
        vm.temporal_zoned_date_time_proto,
        TemporalValue::ZonedDateTime(z),
    )
}

/// Reads `ZonedDateTimeFields` + optional `timeZone` in strict alphabetical order:
/// `day`, `era`, `eraYear`, `hour`, `microsecond`, `millisecond`, `minute`,
/// `month`, `monthCode`, `nanosecond`, `offset`, `second`, (`timeZone` if `read_tz`), `year`.
fn read_zdt_fields(
    vm: &mut JsVm,
    obj: HeapHandle,
    has_eras: bool,
    read_tz: bool,
) -> Result<(ZonedDateTimeFields, Option<TimeZone>), JsException> {
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
    let offv = get_prop(vm, obj, "offset")?;
    let offset = if offv.is_undefined() {
        None
    } else {
        let s = to_primitive_require_string(vm, offv, "offset")?;
        Some(UtcOffset::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?)
    };
    let secv = get_prop(vm, obj, "second")?;
    let second = if secv.is_undefined() {
        None
    } else {
        Some(to_clamped_u8(vm, secv)?)
    };
    let tz = if read_tz {
        let tzv = get_prop(vm, obj, "timeZone")?;
        if tzv.is_undefined() {
            None
        } else {
            Some(to_temporal_time_zone_identifier(vm, tzv)?)
        }
    } else {
        None
    };
    let yv = get_prop(vm, obj, "year")?;
    let year = if yv.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_i32(vm, yv)?)
    };
    Ok((
        ZonedDateTimeFields {
            calendar_fields: CalendarFields {
                year,
                month,
                month_code,
                day,
                era,
                era_year,
            },
            time: PartialTime {
                hour,
                minute,
                second,
                millisecond,
                microsecond,
                nanosecond,
            },
            offset,
        },
        tz,
    ))
}

struct ZdtOptions {
    disambiguation: Option<Disambiguation>,
    offset: Option<OffsetDisambiguation>,
    overflow: Option<temporal_rs::options::Overflow>,
}

fn read_zdt_options(vm: &mut JsVm, options_val: JsValue) -> Result<ZdtOptions, JsException> {
    let opts = get_options_object(vm, options_val)?;
    // Strict alphabetical order: disambiguation, offset, overflow
    let disambiguation = get_disambiguation_option(vm, opts)?;
    let offset = get_offset_disambiguation_option(vm, opts)?;
    let overflow = get_overflow_option(vm, opts)?;
    Ok(ZdtOptions {
        disambiguation,
        offset,
        overflow,
    })
}

pub(crate) fn to_temporal_zdt(
    vm: &mut JsVm,
    item: JsValue,
    options_val: JsValue,
) -> Result<ZonedDateTime, JsException> {
    if let Some(TemporalValue::ZonedDateTime(z)) = get_temporal(vm, item) {
        let z = z.clone();
        let _ = read_zdt_options(vm, options_val)?;
        return Ok(z);
    }
    if let Some(sh) = item.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        let o = read_zdt_options(vm, options_val)?;
        return ZonedDateTime::from_utf8(
            s.as_bytes(),
            o.disambiguation.unwrap_or(Disambiguation::Compatible),
            o.offset.unwrap_or(OffsetDisambiguation::Reject),
        )
        .map_err(|e| temporal_err(vm, e));
    }
    let Some(h) = item.as_object() else {
        return Err(vm.type_error("Temporal.ZonedDateTime.from requires a string or object"));
    };
    let cal_v = get_prop(vm, h, "calendar")?;
    let calendar = if cal_v.is_undefined() {
        Calendar::ISO
    } else {
        to_temporal_calendar_identifier(vm, cal_v)?
    };
    let (fields, timezone) = read_zdt_fields(vm, h, calendar_has_eras(&calendar), true)?;
    let o = read_zdt_options(vm, options_val)?;
    if timezone.is_none() {
        return Err(vm.type_error("ZonedDateTime object requires a 'timeZone' property"));
    }
    ZonedDateTime::from_partial(
        PartialZonedDateTime {
            fields,
            timezone,
            calendar,
        },
        o.overflow,
        o.disambiguation,
        o.offset,
    )
    .map_err(|e| temporal_err(vm, e))
}

fn zdt_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto = ctor_proto_from_new_target(vm, vm.temporal_zoned_date_time_proto, "ZonedDateTime")?;
    let ns = to_bigint_i128(vm, arg(args, 0))?;
    let tz_v = arg(args, 1);
    let Some(sh) = tz_v.as_string() else {
        return Err(vm.type_error("ZonedDateTime timeZone argument must be a string"));
    };
    let tz_s = vm.heap.materialize_string(sh).unwrap_or_default();
    let tz = TimeZone::try_from_identifier_str(&tz_s).map_err(|e| temporal_err(vm, e))?;
    let cal = parse_calendar_ctor_arg(vm, arg(args, 2))?;
    let zdt = ZonedDateTime::try_new(ns, tz, cal).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(vm, proto, TemporalValue::ZonedDateTime(zdt)))
}

fn zdt_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = to_temporal_zdt(vm, arg(args, 0), arg(args, 1))?;
    Ok(wrap_zdt(vm, z))
}

fn zdt_compare(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let one = to_temporal_zdt(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let two = to_temporal_zdt(vm, arg(args, 1), JsValue::UNDEFINED)?;
    let c = match one.compare_instant(&two) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(JsValue::from_i32(c))
}

fn zdt_calendar_id(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(z.calendar().identifier()),
    ))
}
fn zdt_time_zone_id(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let id = z
        .time_zone()
        .identifier()
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(id)))
}
fn zdt_era(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    Ok(match z.era() {
        Some(e) => JsValue::from_string(vm.heap.alloc_string(e.as_str())),
        None => JsValue::UNDEFINED,
    })
}
fn zdt_era_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(require_zdt(vm, t)?
        .era_year()
        .map_or(JsValue::UNDEFINED, JsValue::from_i32))
}
fn zdt_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(require_zdt(vm, t)?.year()))
}
fn zdt_month(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(require_zdt(vm, t)?.month())))
}
fn zdt_month_code(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(z.month_code().as_str()),
    ))
}
fn zdt_day(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(require_zdt(vm, t)?.day())))
}
fn zdt_hour(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(require_zdt(vm, t)?.hour())))
}
fn zdt_minute(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(require_zdt(vm, t)?.minute())))
}
fn zdt_second(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(require_zdt(vm, t)?.second())))
}
fn zdt_millisecond(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.millisecond(),
    )))
}
fn zdt_microsecond(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.microsecond(),
    )))
}
fn zdt_nanosecond(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.nanosecond(),
    )))
}
fn zdt_epoch_milliseconds(
    vm: &mut JsVm,
    t: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_f64(
        require_zdt(vm, t)?.epoch_milliseconds() as f64
    ))
}
fn zdt_epoch_nanoseconds(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let ns = require_zdt(vm, t)?.epoch_nanoseconds().as_i128();
    let bh = vm.heap.alloc_bigint(JsBigInt::from_i128(ns));
    Ok(JsValue::from_bigint(bh))
}
fn zdt_day_of_week(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.day_of_week(),
    )))
}
fn zdt_day_of_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.day_of_year(),
    )))
}
fn zdt_week_of_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(require_zdt(vm, t)?
        .week_of_year()
        .map_or(JsValue::UNDEFINED, |w| JsValue::from_i32(i32::from(w))))
}
fn zdt_year_of_week(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(require_zdt(vm, t)?
        .year_of_week()
        .map_or(JsValue::UNDEFINED, JsValue::from_i32))
}
fn zdt_hours_in_day(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let h = require_zdt(vm, t)?
        .hours_in_day()
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_f64(h))
}
fn zdt_days_in_week(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.days_in_week(),
    )))
}
fn zdt_days_in_month(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.days_in_month(),
    )))
}
fn zdt_days_in_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.days_in_year(),
    )))
}
fn zdt_months_in_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_zdt(vm, t)?.months_in_year(),
    )))
}
fn zdt_in_leap_year(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_bool(require_zdt(vm, t)?.in_leap_year()))
}
fn zdt_offset_nanoseconds(
    vm: &mut JsVm,
    t: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_f64(
        require_zdt(vm, t)?.offset_nanoseconds() as f64
    ))
}
fn zdt_offset(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let off = require_zdt(vm, t)?.offset();
    Ok(JsValue::from_string(vm.heap.alloc_string(off)))
}

fn zdt_with(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let h = require_partial_temporal_object(vm, arg(args, 0))?;
    let (fields, _) = read_zdt_fields(vm, h, calendar_has_eras(z.calendar()), false)?;
    if fields.is_empty() {
        return Err(vm.type_error("ZonedDateTime.prototype.with requires at least one field"));
    }
    let o = read_zdt_options(vm, arg(args, 1))?;
    let res = z
        .with(fields, o.disambiguation, o.offset, o.overflow)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_zdt(vm, res))
}

fn zdt_with_plain_time(
    vm: &mut JsVm,
    t: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let pt_v = arg(args, 0);
    let pt = if pt_v.is_undefined() {
        None
    } else {
        Some(to_temporal_plain_time(vm, pt_v, JsValue::UNDEFINED)?)
    };
    let res = z.with_plain_time(pt).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_zdt(vm, res))
}

fn zdt_with_time_zone(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let tz = to_temporal_time_zone_identifier(vm, arg(args, 0))?;
    let res = z.with_timezone(tz).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_zdt(vm, res))
}

fn zdt_with_calendar(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let cal = to_temporal_calendar_identifier(vm, arg(args, 0))?;
    Ok(wrap_zdt(vm, z.with_calendar(cal)))
}

fn zdt_add(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let ovf = get_overflow_option(vm, opts)?;
    let res = z.add(&dur, ovf).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_zdt(vm, res))
}

fn zdt_subtract(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let ovf = get_overflow_option(vm, opts)?;
    let res = z.subtract(&dur, ovf).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_zdt(vm, res))
}

fn zdt_until(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let other = to_temporal_zdt(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = z.until(&other, settings).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn zdt_since(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let other = to_temporal_zdt(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = z.since(&other, settings).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn zdt_round(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let opts = get_simple_rounding_options(vm, arg(args, 0))?;
    let res = z.round(opts).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_zdt(vm, res))
}

fn zdt_equals(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let other = to_temporal_zdt(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let eq = z.equals(&other).map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_bool(eq))
}

fn zdt_start_of_day(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let res = z.start_of_day().map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_zdt(vm, res))
}

fn zdt_get_time_zone_transition(
    vm: &mut JsVm,
    t: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let dp = arg(args, 0);
    if dp.is_undefined() {
        return Err(vm.type_error("getTimeZoneTransition requires a direction string or object"));
    }
    let dir_str = if let Some(sh) = dp.as_string() {
        vm.heap.materialize_string(sh).unwrap_or_default()
    } else if let Some(h) = dp.as_object() {
        get_opt_string(vm, h, "direction")?.ok_or_else(|| {
            vm.range_error("getTimeZoneTransition options must specify 'direction'")
        })?
    } else {
        return Err(vm.type_error("getTimeZoneTransition direction must be a string or object"));
    };
    let dir = TransitionDirection::from_str(&dir_str)
        .map_err(|_| vm.range_error(format!("Invalid transition direction: {dir_str}")))?;
    let next = z
        .get_time_zone_transition(dir)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(match next {
        Some(n) => wrap_zdt(vm, n),
        None => JsValue::NULL,
    })
}

fn zdt_to_instant(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let inst = require_zdt(vm, t)?.to_instant();
    Ok(alloc_temporal(
        vm,
        vm.temporal_instant_proto,
        TemporalValue::Instant(inst),
    ))
}

fn zdt_to_plain_date(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_zdt(vm, t)?.to_plain_date();
    Ok(wrap_plain_date(vm, d))
}

fn zdt_to_plain_time(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let pt = require_zdt(vm, t)?.to_plain_time();
    Ok(wrap_plain_time(vm, pt))
}

fn zdt_to_plain_date_time(
    vm: &mut JsVm,
    t: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    let dt = require_zdt(vm, t)?.to_plain_date_time();
    Ok(wrap_plain_date_time(vm, dt))
}

fn zdt_to_string(vm: &mut JsVm, t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let (disp_off, disp_tz, disp_cal, rounding) =
        if let Some(h) = get_options_object(vm, arg(args, 0))? {
            // Strict alphabetical order:
            // calendarName, fractionalSecondDigits, offset, roundingMode, smallestUnit, timeZoneName
            let cal = get_display_calendar_option(vm, h)?;
            let precision = get_fractional_second_digits_option(vm, h)?;
            let off = get_display_offset_option(vm, h)?;
            let rounding_mode = get_rounding_mode_option(vm, h)?;
            let smallest_unit = get_unit_option(vm, h, "smallestUnit", false)?;
            let tz = get_display_time_zone_option(vm, h)?;
            (
                off,
                tz,
                cal,
                ToStringRoundingOptions {
                    precision,
                    smallest_unit,
                    rounding_mode,
                },
            )
        } else {
            (
                DisplayOffset::Auto,
                DisplayTimeZone::Auto,
                DisplayCalendar::Auto,
                ToStringRoundingOptions::default(),
            )
        };
    let s = z
        .to_ixdtf_string(disp_off, disp_tz, disp_cal, rounding)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn zdt_to_json(vm: &mut JsVm, t: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let z = require_zdt(vm, t)?;
    let s = z
        .to_ixdtf_string(
            DisplayOffset::Auto,
            DisplayTimeZone::Auto,
            DisplayCalendar::Auto,
            ToStringRoundingOptions::default(),
        )
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

pub(crate) fn install_zoned_date_time(vm: &mut JsVm, ns: HeapHandle) {
    let proto = vm.temporal_zoned_date_time_proto;
    let ctor = install_ctor_and_proto(vm, ns, "ZonedDateTime", 2, zdt_ctor, proto);

    define_method(vm, ctor, "from", 1, zdt_from);
    define_method(vm, ctor, "compare", 2, zdt_compare);

    define_getter(vm, proto, "calendarId", zdt_calendar_id);
    define_getter(vm, proto, "timeZoneId", zdt_time_zone_id);
    define_getter(vm, proto, "era", zdt_era);
    define_getter(vm, proto, "eraYear", zdt_era_year);
    define_getter(vm, proto, "year", zdt_year);
    define_getter(vm, proto, "month", zdt_month);
    define_getter(vm, proto, "monthCode", zdt_month_code);
    define_getter(vm, proto, "day", zdt_day);
    define_getter(vm, proto, "hour", zdt_hour);
    define_getter(vm, proto, "minute", zdt_minute);
    define_getter(vm, proto, "second", zdt_second);
    define_getter(vm, proto, "millisecond", zdt_millisecond);
    define_getter(vm, proto, "microsecond", zdt_microsecond);
    define_getter(vm, proto, "nanosecond", zdt_nanosecond);
    define_getter(vm, proto, "epochMilliseconds", zdt_epoch_milliseconds);
    define_getter(vm, proto, "epochNanoseconds", zdt_epoch_nanoseconds);
    define_getter(vm, proto, "dayOfWeek", zdt_day_of_week);
    define_getter(vm, proto, "dayOfYear", zdt_day_of_year);
    define_getter(vm, proto, "weekOfYear", zdt_week_of_year);
    define_getter(vm, proto, "yearOfWeek", zdt_year_of_week);
    define_getter(vm, proto, "hoursInDay", zdt_hours_in_day);
    define_getter(vm, proto, "daysInWeek", zdt_days_in_week);
    define_getter(vm, proto, "daysInMonth", zdt_days_in_month);
    define_getter(vm, proto, "daysInYear", zdt_days_in_year);
    define_getter(vm, proto, "monthsInYear", zdt_months_in_year);
    define_getter(vm, proto, "inLeapYear", zdt_in_leap_year);
    define_getter(vm, proto, "offsetNanoseconds", zdt_offset_nanoseconds);
    define_getter(vm, proto, "offset", zdt_offset);

    define_method(vm, proto, "with", 1, zdt_with);
    define_method(vm, proto, "withPlainTime", 0, zdt_with_plain_time);
    define_method(vm, proto, "withTimeZone", 1, zdt_with_time_zone);
    define_method(vm, proto, "withCalendar", 1, zdt_with_calendar);
    define_method(vm, proto, "add", 1, zdt_add);
    define_method(vm, proto, "subtract", 1, zdt_subtract);
    define_method(vm, proto, "until", 1, zdt_until);
    define_method(vm, proto, "since", 1, zdt_since);
    define_method(vm, proto, "round", 1, zdt_round);
    define_method(vm, proto, "equals", 1, zdt_equals);
    define_method(vm, proto, "startOfDay", 0, zdt_start_of_day);
    define_method(
        vm,
        proto,
        "getTimeZoneTransition",
        1,
        zdt_get_time_zone_transition,
    );
    define_method(vm, proto, "toInstant", 0, zdt_to_instant);
    define_method(vm, proto, "toPlainDate", 0, zdt_to_plain_date);
    define_method(vm, proto, "toPlainTime", 0, zdt_to_plain_time);
    define_method(vm, proto, "toPlainDateTime", 0, zdt_to_plain_date_time);
    define_method(vm, proto, "toString", 0, zdt_to_string);
    define_method(vm, proto, "toJSON", 0, zdt_to_json);
    define_method(vm, proto, "toLocaleString", 0, zdt_to_json);
    define_method(vm, proto, "valueOf", 0, temporal_value_of);
}
