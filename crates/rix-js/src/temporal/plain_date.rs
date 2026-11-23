//! ECMAScript `Temporal.PlainDate` (§3), `Temporal.PlainYearMonth` (§9),
//! and `Temporal.PlainMonthDay` (§10) backed by `temporal_rs`.

use std::cmp::Ordering;

use temporal_rs::fields::{CalendarFields, YearMonthCalendarFields};
use temporal_rs::options::{DisplayCalendar, Overflow};
use temporal_rs::partial::{PartialDate, PartialYearMonth};
use temporal_rs::{Calendar, PlainDate, PlainMonthDay, PlainYearMonth};

use crate::heap::TemporalValue;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm};

use super::duration::to_temporal_duration;
use super::options::{
    arg, calendar_has_eras, get_difference_settings, get_display_calendar_option,
    get_options_object, get_overflow_option, get_prop, parse_calendar_ctor_arg,
    read_calendar_fields, require_partial_temporal_object, to_integer_with_truncation_i32,
    to_integer_with_truncation_u8, to_positive_integer_with_truncation_u8,
    to_temporal_calendar_identifier, to_temporal_time_zone_identifier,
};
use super::plain_time::to_temporal_plain_time;
use super::{
    alloc_temporal, ctor_proto_from_new_target, define_getter, define_method, get_temporal,
    install_ctor_and_proto, temporal_err, temporal_value_of,
};

// ===========================================================================
// Temporal.PlainDate (§3)
// ===========================================================================

pub(crate) fn require_plain_date(
    vm: &mut JsVm,
    this_val: JsValue,
) -> Result<PlainDate, JsException> {
    match get_temporal(vm, this_val) {
        Some(TemporalValue::PlainDate(d)) => Ok(d.clone()),
        _ => Err(vm.type_error("Receiver is not a Temporal.PlainDate")),
    }
}

pub(crate) fn wrap_plain_date(vm: &mut JsVm, d: PlainDate) -> JsValue {
    alloc_temporal(
        vm,
        vm.temporal_plain_date_proto,
        TemporalValue::PlainDate(d),
    )
}

pub(crate) fn to_temporal_plain_date(
    vm: &mut JsVm,
    item: JsValue,
    options_val: JsValue,
) -> Result<PlainDate, JsException> {
    if let Some(tv) = get_temporal(vm, item) {
        match tv {
            TemporalValue::PlainDate(d) => {
                let d = d.clone();
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(d);
            }
            TemporalValue::PlainDateTime(dt) => {
                let d = dt.to_plain_date();
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(d);
            }
            TemporalValue::ZonedDateTime(zdt) => {
                let d = zdt.to_plain_date();
                let opts = get_options_object(vm, options_val)?;
                let _ = get_overflow_option(vm, opts)?;
                return Ok(d);
            }
            _ => {}
        }
    }
    if let Some(sh) = item.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        let d = PlainDate::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?;
        let opts = get_options_object(vm, options_val)?;
        let _ = get_overflow_option(vm, opts)?;
        return Ok(d);
    }
    let Some(h) = item.as_object() else {
        return Err(vm.type_error("Temporal.PlainDate.from requires a string or object"));
    };
    let cal_v = get_prop(vm, h, "calendar")?;
    let calendar = if cal_v.is_undefined() {
        Calendar::ISO
    } else {
        to_temporal_calendar_identifier(vm, cal_v)?
    };
    let fields = read_calendar_fields(vm, h, calendar_has_eras(&calendar), true, true)?;
    let opts = get_options_object(vm, options_val)?;
    let overflow = get_overflow_option(vm, opts)?;
    PlainDate::from_partial(
        PartialDate {
            calendar_fields: fields,
            calendar,
        },
        overflow,
    )
    .map_err(|e| temporal_err(vm, e))
}

fn pd_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto = ctor_proto_from_new_target(vm, vm.temporal_plain_date_proto, "PlainDate")?;
    let y = to_integer_with_truncation_i32(vm, arg(args, 0))?;
    let m = to_integer_with_truncation_u8(vm, arg(args, 1))?;
    let d = to_integer_with_truncation_u8(vm, arg(args, 2))?;
    let cal = parse_calendar_ctor_arg(vm, arg(args, 3))?;
    let pd = PlainDate::try_new(y, m, d, cal).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(vm, proto, TemporalValue::PlainDate(pd)))
}

fn pd_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = to_temporal_plain_date(vm, arg(args, 0), arg(args, 1))?;
    Ok(wrap_plain_date(vm, d))
}

fn pd_compare(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let one = to_temporal_plain_date(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let two = to_temporal_plain_date(vm, arg(args, 1), JsValue::UNDEFINED)?;
    let c = match one.compare_iso(&two) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(JsValue::from_i32(c))
}

fn pd_calendar_id(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(d.calendar().identifier()),
    ))
}

fn pd_era(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    Ok(match d.era() {
        Some(e) => JsValue::from_string(vm.heap.alloc_string(e.as_str())),
        None => JsValue::UNDEFINED,
    })
}

fn pd_era_year(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    Ok(d.era_year().map_or(JsValue::UNDEFINED, JsValue::from_i32))
}

fn pd_year(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(require_plain_date(vm, this_val)?.year()))
}

fn pd_month(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date(vm, this_val)?.month(),
    )))
}

fn pd_month_code(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(d.month_code().as_str()),
    ))
}

fn pd_day(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date(vm, this_val)?.day(),
    )))
}

fn pd_day_of_week(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date(vm, this_val)?.day_of_week(),
    )))
}

fn pd_day_of_year(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date(vm, this_val)?.day_of_year(),
    )))
}

fn pd_week_of_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(require_plain_date(vm, this_val)?
        .week_of_year()
        .map_or(JsValue::UNDEFINED, |w| JsValue::from_i32(i32::from(w))))
}

fn pd_year_of_week(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(require_plain_date(vm, this_val)?
        .year_of_week()
        .map_or(JsValue::UNDEFINED, JsValue::from_i32))
}

fn pd_days_in_week(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date(vm, this_val)?.days_in_week(),
    )))
}

fn pd_days_in_month(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date(vm, this_val)?.days_in_month(),
    )))
}

fn pd_days_in_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date(vm, this_val)?.days_in_year(),
    )))
}

fn pd_months_in_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_date(vm, this_val)?.months_in_year(),
    )))
}

fn pd_in_leap_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_bool(
        require_plain_date(vm, this_val)?.in_leap_year(),
    ))
}

fn pd_to_plain_year_month(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    let ym = require_plain_date(vm, this_val)?
        .to_plain_year_month()
        .map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_year_month(vm, ym))
}

fn pd_to_plain_month_day(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    let md = require_plain_date(vm, this_val)?
        .to_plain_month_day()
        .map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_month_day(vm, md))
}

fn pd_add(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = d.add(&dur, overflow).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date(vm, res))
}

fn pd_subtract(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = d
        .subtract(&dur, overflow)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date(vm, res))
}

fn pd_with(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let h = require_partial_temporal_object(vm, arg(args, 0))?;
    let fields = read_calendar_fields(vm, h, calendar_has_eras(d.calendar()), true, true)?;
    if fields.is_empty() {
        return Err(vm.type_error("PlainDate.prototype.with requires at least one date field"));
    }
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = d.with(fields, overflow).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date(vm, res))
}

fn pd_with_calendar(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let cal = to_temporal_calendar_identifier(vm, arg(args, 0))?;
    Ok(wrap_plain_date(vm, d.with_calendar(cal)))
}

fn pd_until(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let other = to_temporal_plain_date(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = d.until(&other, settings).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn pd_since(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let other = to_temporal_plain_date(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = d.since(&other, settings).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn pd_equals(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let other = to_temporal_plain_date(vm, arg(args, 0), JsValue::UNDEFINED)?;
    Ok(JsValue::from_bool(d == other))
}

fn pd_to_plain_date_time(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let pt_v = arg(args, 0);
    let pt = if pt_v.is_undefined() {
        None
    } else {
        Some(to_temporal_plain_time(vm, pt_v, JsValue::UNDEFINED)?)
    };
    let dt = d.to_plain_date_time(pt).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_plain_date_time_proto,
        TemporalValue::PlainDateTime(dt),
    ))
}

fn pd_to_zoned_date_time(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let item = arg(args, 0);
    let (tz, pt) = if let Some(h) = item.as_object() {
        if get_temporal(vm, item).is_some() {
            (to_temporal_time_zone_identifier(vm, item)?, None)
        } else {
            // Alphabetical order: plainTime, timeZone
            let pt_v = get_prop(vm, h, "plainTime")?;
            let tz_v = get_prop(vm, h, "timeZone")?;
            if tz_v.is_undefined() {
                return Err(vm.type_error("toZonedDateTime requires a timeZone"));
            }
            let tz = to_temporal_time_zone_identifier(vm, tz_v)?;
            let pt = if pt_v.is_undefined() {
                None
            } else {
                Some(to_temporal_plain_time(vm, pt_v, JsValue::UNDEFINED)?)
            };
            (tz, pt)
        }
    } else {
        (to_temporal_time_zone_identifier(vm, item)?, None)
    };
    let zdt = d
        .to_zoned_date_time(tz, pt)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_zoned_date_time_proto,
        TemporalValue::ZonedDateTime(zdt),
    ))
}

fn pd_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    let cal = match get_options_object(vm, arg(args, 0))? {
        Some(h) => get_display_calendar_option(vm, h)?,
        None => DisplayCalendar::Auto,
    };
    Ok(JsValue::from_string(
        vm.heap.alloc_string(d.to_ixdtf_string(cal)),
    ))
}

fn pd_to_json(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let d = require_plain_date(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap
            .alloc_string(d.to_ixdtf_string(DisplayCalendar::Auto)),
    ))
}

// ===========================================================================
// Temporal.PlainYearMonth (§9)
// ===========================================================================

fn require_plain_year_month(
    vm: &mut JsVm,
    this_val: JsValue,
) -> Result<PlainYearMonth, JsException> {
    match get_temporal(vm, this_val) {
        Some(TemporalValue::PlainYearMonth(ym)) => Ok(ym.clone()),
        _ => Err(vm.type_error("Receiver is not a Temporal.PlainYearMonth")),
    }
}

fn wrap_plain_year_month(vm: &mut JsVm, ym: PlainYearMonth) -> JsValue {
    alloc_temporal(
        vm,
        vm.temporal_plain_year_month_proto,
        TemporalValue::PlainYearMonth(ym),
    )
}

fn to_temporal_year_month(
    vm: &mut JsVm,
    item: JsValue,
    options_val: JsValue,
) -> Result<PlainYearMonth, JsException> {
    if let Some(TemporalValue::PlainYearMonth(ym)) = get_temporal(vm, item) {
        let ym = ym.clone();
        let opts = get_options_object(vm, options_val)?;
        let _ = get_overflow_option(vm, opts)?;
        return Ok(ym);
    }
    if let Some(sh) = item.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        let ym = PlainYearMonth::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?;
        let opts = get_options_object(vm, options_val)?;
        let _ = get_overflow_option(vm, opts)?;
        return Ok(ym);
    }
    let Some(h) = item.as_object() else {
        return Err(vm.type_error("Temporal.PlainYearMonth.from requires a string or object"));
    };
    let cal_v = get_prop(vm, h, "calendar")?;
    let calendar = if cal_v.is_undefined() {
        Calendar::ISO
    } else {
        to_temporal_calendar_identifier(vm, cal_v)?
    };
    let cf = read_calendar_fields(vm, h, calendar_has_eras(&calendar), false, true)?;
    let opts = get_options_object(vm, options_val)?;
    let overflow = get_overflow_option(vm, opts)?;
    let ym_fields = YearMonthCalendarFields {
        year: cf.year,
        month: cf.month,
        month_code: cf.month_code,
        era: cf.era,
        era_year: cf.era_year,
    };
    PlainYearMonth::from_partial(
        PartialYearMonth {
            calendar_fields: ym_fields,
            calendar,
        },
        overflow,
    )
    .map_err(|e| temporal_err(vm, e))
}

fn pym_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto =
        ctor_proto_from_new_target(vm, vm.temporal_plain_year_month_proto, "PlainYearMonth")?;
    let y = to_integer_with_truncation_i32(vm, arg(args, 0))?;
    let m = to_integer_with_truncation_u8(vm, arg(args, 1))?;
    let cal = parse_calendar_ctor_arg(vm, arg(args, 2))?;
    let ref_day_v = arg(args, 3);
    let ref_day = if ref_day_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_u8(vm, ref_day_v)?)
    };
    let ym = PlainYearMonth::try_new(y, m, ref_day, cal).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(vm, proto, TemporalValue::PlainYearMonth(ym)))
}

fn pym_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = to_temporal_year_month(vm, arg(args, 0), arg(args, 1))?;
    Ok(wrap_plain_year_month(vm, ym))
}

fn pym_compare(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let one = to_temporal_year_month(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let two = to_temporal_year_month(vm, arg(args, 1), JsValue::UNDEFINED)?;
    let c = match one.compare_iso(&two) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(JsValue::from_i32(c))
}

fn pym_calendar_id(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    Ok(JsValue::from_string(vm.heap.alloc_string(ym.calendar_id())))
}

fn pym_era(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    Ok(match ym.era() {
        Some(e) => JsValue::from_string(vm.heap.alloc_string(e.as_str())),
        None => JsValue::UNDEFINED,
    })
}

fn pym_era_year(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(require_plain_year_month(vm, this_val)?
        .era_year()
        .map_or(JsValue::UNDEFINED, JsValue::from_i32))
}

fn pym_year(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(
        require_plain_year_month(vm, this_val)?.year(),
    ))
}

fn pym_month(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_year_month(vm, this_val)?.month(),
    )))
}

fn pym_month_code(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(ym.month_code().as_str()),
    ))
}

fn pym_days_in_month(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_year_month(vm, this_val)?.days_in_month(),
    )))
}

fn pym_days_in_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_year_month(vm, this_val)?.days_in_year(),
    )))
}

fn pym_months_in_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_year_month(vm, this_val)?.months_in_year(),
    )))
}

fn pym_in_leap_year(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    Ok(JsValue::from_bool(
        require_plain_year_month(vm, this_val)?.in_leap_year(),
    ))
}

fn pym_with(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    let h = require_partial_temporal_object(vm, arg(args, 0))?;
    let cf = read_calendar_fields(vm, h, calendar_has_eras(ym.calendar()), false, true)?;
    let fields = YearMonthCalendarFields {
        year: cf.year,
        month: cf.month,
        month_code: cf.month_code,
        era: cf.era,
        era_year: cf.era_year,
    };
    if cf.is_empty() {
        return Err(vm.type_error("PlainYearMonth.prototype.with requires at least one field"));
    }
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = ym.with(fields, overflow).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_year_month(vm, res))
}

fn pym_add(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?.unwrap_or(Overflow::Constrain);
    let res = ym.add(&dur, overflow).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_year_month(vm, res))
}

fn pym_subtract(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?.unwrap_or(Overflow::Constrain);
    let res = ym
        .subtract(&dur, overflow)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_year_month(vm, res))
}

fn pym_until(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    let other = to_temporal_year_month(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = ym
        .until(&other, settings)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn pym_since(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    let other = to_temporal_year_month(vm, arg(args, 0), JsValue::UNDEFINED)?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = ym
        .since(&other, settings)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn pym_equals(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    let other = to_temporal_year_month(vm, arg(args, 0), JsValue::UNDEFINED)?;
    Ok(JsValue::from_bool(ym == other))
}

fn pym_to_plain_date(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    let Some(h) = arg(args, 0).as_object() else {
        return Err(vm.type_error("PlainYearMonth.prototype.toPlainDate requires an object"));
    };
    let dv = get_prop(vm, h, "day")?;
    let day = if dv.is_undefined() {
        None
    } else {
        Some(to_positive_integer_with_truncation_u8(vm, dv)?)
    };
    let mut fields = CalendarFields::new();
    fields.day = day;
    let d = ym
        .to_plain_date(Some(fields))
        .map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date(vm, d))
}

fn pym_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    let cal = match get_options_object(vm, arg(args, 0))? {
        Some(h) => get_display_calendar_option(vm, h)?,
        None => DisplayCalendar::Auto,
    };
    Ok(JsValue::from_string(
        vm.heap.alloc_string(ym.to_ixdtf_string(cal)),
    ))
}

fn pym_to_json(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let ym = require_plain_year_month(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap
            .alloc_string(ym.to_ixdtf_string(DisplayCalendar::Auto)),
    ))
}

// ===========================================================================
// Temporal.PlainMonthDay (§10)
// ===========================================================================

fn require_plain_month_day(vm: &mut JsVm, this_val: JsValue) -> Result<PlainMonthDay, JsException> {
    match get_temporal(vm, this_val) {
        Some(TemporalValue::PlainMonthDay(md)) => Ok(md.clone()),
        _ => Err(vm.type_error("Receiver is not a Temporal.PlainMonthDay")),
    }
}

fn wrap_plain_month_day(vm: &mut JsVm, md: PlainMonthDay) -> JsValue {
    alloc_temporal(
        vm,
        vm.temporal_plain_month_day_proto,
        TemporalValue::PlainMonthDay(md),
    )
}

fn to_temporal_month_day(
    vm: &mut JsVm,
    item: JsValue,
    options_val: JsValue,
) -> Result<PlainMonthDay, JsException> {
    if let Some(TemporalValue::PlainMonthDay(md)) = get_temporal(vm, item) {
        let md = md.clone();
        let opts = get_options_object(vm, options_val)?;
        let _ = get_overflow_option(vm, opts)?;
        return Ok(md);
    }
    if let Some(sh) = item.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        let md = PlainMonthDay::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))?;
        let opts = get_options_object(vm, options_val)?;
        let _ = get_overflow_option(vm, opts)?;
        return Ok(md);
    }
    let Some(h) = item.as_object() else {
        return Err(vm.type_error("Temporal.PlainMonthDay.from requires a string or object"));
    };
    let cal_v = get_prop(vm, h, "calendar")?;
    let calendar = if cal_v.is_undefined() {
        Calendar::ISO
    } else {
        to_temporal_calendar_identifier(vm, cal_v)?
    };
    let fields = read_calendar_fields(vm, h, calendar_has_eras(&calendar), true, true)?;
    let opts = get_options_object(vm, options_val)?;
    let overflow = get_overflow_option(vm, opts)?;
    PlainMonthDay::from_partial(
        PartialDate {
            calendar_fields: fields,
            calendar,
        },
        overflow,
    )
    .map_err(|e| temporal_err(vm, e))
}

fn pmd_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto = ctor_proto_from_new_target(vm, vm.temporal_plain_month_day_proto, "PlainMonthDay")?;
    let m = to_integer_with_truncation_u8(vm, arg(args, 0))?;
    let d = to_integer_with_truncation_u8(vm, arg(args, 1))?;
    let cal = parse_calendar_ctor_arg(vm, arg(args, 2))?;
    let ref_y_v = arg(args, 3);
    let ref_y = if ref_y_v.is_undefined() {
        None
    } else {
        Some(to_integer_with_truncation_i32(vm, ref_y_v)?)
    };
    let md = PlainMonthDay::new_with_overflow(m, d, cal, Overflow::Reject, ref_y)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(vm, proto, TemporalValue::PlainMonthDay(md)))
}

fn pmd_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let md = to_temporal_month_day(vm, arg(args, 0), arg(args, 1))?;
    Ok(wrap_plain_month_day(vm, md))
}

fn pmd_calendar_id(
    vm: &mut JsVm,
    this_val: JsValue,
    _: &[JsValue],
) -> Result<JsValue, JsException> {
    let md = require_plain_month_day(vm, this_val)?;
    Ok(JsValue::from_string(vm.heap.alloc_string(md.calendar_id())))
}

fn pmd_month_code(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let md = require_plain_month_day(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(md.month_code().as_str()),
    ))
}

fn pmd_day(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_i32(i32::from(
        require_plain_month_day(vm, this_val)?.day(),
    )))
}

fn pmd_with(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let md = require_plain_month_day(vm, this_val)?;
    let h = require_partial_temporal_object(vm, arg(args, 0))?;
    let fields = read_calendar_fields(vm, h, calendar_has_eras(md.calendar()), true, true)?;
    if fields.is_empty() {
        return Err(vm.type_error("PlainMonthDay.prototype.with requires at least one field"));
    }
    let opts = get_options_object(vm, arg(args, 1))?;
    let overflow = get_overflow_option(vm, opts)?;
    let res = md.with(fields, overflow).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_month_day(vm, res))
}

fn pmd_equals(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let md = require_plain_month_day(vm, this_val)?;
    let other = to_temporal_month_day(vm, arg(args, 0), JsValue::UNDEFINED)?;
    Ok(JsValue::from_bool(md == other))
}

fn pmd_to_plain_date(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let md = require_plain_month_day(vm, this_val)?;
    let Some(h) = arg(args, 0).as_object() else {
        return Err(vm.type_error("PlainMonthDay.prototype.toPlainDate requires an object"));
    };
    let has_eras = calendar_has_eras(md.calendar());
    let mut fields = CalendarFields::new();
    if has_eras {
        let ev = get_prop(vm, h, "era")?;
        if !ev.is_undefined() {
            let s = super::options::to_primitive_require_string(vm, ev, "era")?;
            fields.era = Some(
                temporal_rs::TinyAsciiStr::<19>::try_from_str(&s)
                    .map_err(|_| vm.range_error("Invalid era"))?,
            );
        }
        let eyv = get_prop(vm, h, "eraYear")?;
        if !eyv.is_undefined() {
            fields.era_year = Some(to_integer_with_truncation_i32(vm, eyv)?);
        }
    }
    let yv = get_prop(vm, h, "year")?;
    if !yv.is_undefined() {
        fields.year = Some(to_integer_with_truncation_i32(vm, yv)?);
    }
    let d = md
        .to_plain_date(Some(fields))
        .map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_plain_date(vm, d))
}

fn pmd_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let md = require_plain_month_day(vm, this_val)?;
    let cal = match get_options_object(vm, arg(args, 0))? {
        Some(h) => get_display_calendar_option(vm, h)?,
        None => DisplayCalendar::Auto,
    };
    Ok(JsValue::from_string(
        vm.heap.alloc_string(md.to_ixdtf_string(cal)),
    ))
}

fn pmd_to_json(vm: &mut JsVm, this_val: JsValue, _: &[JsValue]) -> Result<JsValue, JsException> {
    let md = require_plain_month_day(vm, this_val)?;
    Ok(JsValue::from_string(
        vm.heap
            .alloc_string(md.to_ixdtf_string(DisplayCalendar::Auto)),
    ))
}

// ===========================================================================
// Installers
// ===========================================================================

pub(crate) fn install_plain_date_types(vm: &mut JsVm, ns: HeapHandle) {
    // PlainDate
    let pd_proto = vm.temporal_plain_date_proto;
    let pd_c = install_ctor_and_proto(vm, ns, "PlainDate", 3, pd_ctor, pd_proto);
    define_method(vm, pd_c, "from", 1, pd_from);
    define_method(vm, pd_c, "compare", 2, pd_compare);
    define_getter(vm, pd_proto, "calendarId", pd_calendar_id);
    define_getter(vm, pd_proto, "era", pd_era);
    define_getter(vm, pd_proto, "eraYear", pd_era_year);
    define_getter(vm, pd_proto, "year", pd_year);
    define_getter(vm, pd_proto, "month", pd_month);
    define_getter(vm, pd_proto, "monthCode", pd_month_code);
    define_getter(vm, pd_proto, "day", pd_day);
    define_getter(vm, pd_proto, "dayOfWeek", pd_day_of_week);
    define_getter(vm, pd_proto, "dayOfYear", pd_day_of_year);
    define_getter(vm, pd_proto, "weekOfYear", pd_week_of_year);
    define_getter(vm, pd_proto, "yearOfWeek", pd_year_of_week);
    define_getter(vm, pd_proto, "daysInWeek", pd_days_in_week);
    define_getter(vm, pd_proto, "daysInMonth", pd_days_in_month);
    define_getter(vm, pd_proto, "daysInYear", pd_days_in_year);
    define_getter(vm, pd_proto, "monthsInYear", pd_months_in_year);
    define_getter(vm, pd_proto, "inLeapYear", pd_in_leap_year);
    define_method(vm, pd_proto, "toPlainYearMonth", 0, pd_to_plain_year_month);
    define_method(vm, pd_proto, "toPlainMonthDay", 0, pd_to_plain_month_day);
    define_method(vm, pd_proto, "add", 1, pd_add);
    define_method(vm, pd_proto, "subtract", 1, pd_subtract);
    define_method(vm, pd_proto, "with", 1, pd_with);
    define_method(vm, pd_proto, "withCalendar", 1, pd_with_calendar);
    define_method(vm, pd_proto, "until", 1, pd_until);
    define_method(vm, pd_proto, "since", 1, pd_since);
    define_method(vm, pd_proto, "equals", 1, pd_equals);
    define_method(vm, pd_proto, "toPlainDateTime", 0, pd_to_plain_date_time);
    define_method(vm, pd_proto, "toZonedDateTime", 1, pd_to_zoned_date_time);
    define_method(vm, pd_proto, "toString", 0, pd_to_string);
    define_method(vm, pd_proto, "toJSON", 0, pd_to_json);
    define_method(vm, pd_proto, "toLocaleString", 0, pd_to_json);
    define_method(vm, pd_proto, "valueOf", 0, temporal_value_of);

    // PlainYearMonth
    let ym_proto = vm.temporal_plain_year_month_proto;
    let ym_c = install_ctor_and_proto(vm, ns, "PlainYearMonth", 2, pym_ctor, ym_proto);
    define_method(vm, ym_c, "from", 1, pym_from);
    define_method(vm, ym_c, "compare", 2, pym_compare);
    define_getter(vm, ym_proto, "calendarId", pym_calendar_id);
    define_getter(vm, ym_proto, "era", pym_era);
    define_getter(vm, ym_proto, "eraYear", pym_era_year);
    define_getter(vm, ym_proto, "year", pym_year);
    define_getter(vm, ym_proto, "month", pym_month);
    define_getter(vm, ym_proto, "monthCode", pym_month_code);
    define_getter(vm, ym_proto, "daysInMonth", pym_days_in_month);
    define_getter(vm, ym_proto, "daysInYear", pym_days_in_year);
    define_getter(vm, ym_proto, "monthsInYear", pym_months_in_year);
    define_getter(vm, ym_proto, "inLeapYear", pym_in_leap_year);
    define_method(vm, ym_proto, "with", 1, pym_with);
    define_method(vm, ym_proto, "add", 1, pym_add);
    define_method(vm, ym_proto, "subtract", 1, pym_subtract);
    define_method(vm, ym_proto, "until", 1, pym_until);
    define_method(vm, ym_proto, "since", 1, pym_since);
    define_method(vm, ym_proto, "equals", 1, pym_equals);
    define_method(vm, ym_proto, "toPlainDate", 1, pym_to_plain_date);
    define_method(vm, ym_proto, "toString", 0, pym_to_string);
    define_method(vm, ym_proto, "toJSON", 0, pym_to_json);
    define_method(vm, ym_proto, "toLocaleString", 0, pym_to_json);
    define_method(vm, ym_proto, "valueOf", 0, temporal_value_of);

    // PlainMonthDay
    let md_proto = vm.temporal_plain_month_day_proto;
    let md_c = install_ctor_and_proto(vm, ns, "PlainMonthDay", 2, pmd_ctor, md_proto);
    define_method(vm, md_c, "from", 1, pmd_from);
    define_getter(vm, md_proto, "calendarId", pmd_calendar_id);
    define_getter(vm, md_proto, "monthCode", pmd_month_code);
    define_getter(vm, md_proto, "day", pmd_day);
    define_method(vm, md_proto, "with", 1, pmd_with);
    define_method(vm, md_proto, "equals", 1, pmd_equals);
    define_method(vm, md_proto, "toPlainDate", 1, pmd_to_plain_date);
    define_method(vm, md_proto, "toString", 0, pmd_to_string);
    define_method(vm, md_proto, "toJSON", 0, pmd_to_json);
    define_method(vm, md_proto, "toLocaleString", 0, pmd_to_json);
    define_method(vm, md_proto, "valueOf", 0, temporal_value_of);
}
