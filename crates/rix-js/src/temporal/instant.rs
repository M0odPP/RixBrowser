//! ECMAScript `Temporal.Instant` (§8) backed by `temporal_rs::Instant`.

use std::cmp::Ordering;

use temporal_rs::options::ToStringRoundingOptions;
use temporal_rs::Instant;

use crate::bigint::JsBigInt;
use crate::heap::TemporalValue;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, PrimHint};

use super::duration::to_temporal_duration;
use super::options::{
    arg, get_difference_settings, get_fractional_second_digits_option, get_options_object,
    get_prop, get_rounding_mode_option, get_simple_rounding_options, get_unit_option,
    to_bigint_i128, to_integer_if_integral_i64, to_temporal_time_zone_identifier,
};
use super::{
    alloc_temporal, ctor_proto_from_new_target, define_getter, define_method, get_temporal,
    install_ctor_and_proto, temporal_err, temporal_value_of,
};

pub(crate) fn require_instant(vm: &mut JsVm, this_val: JsValue) -> Result<Instant, JsException> {
    match get_temporal(vm, this_val) {
        Some(TemporalValue::Instant(i)) => Ok(*i),
        _ => Err(vm.type_error("Receiver is not a Temporal.Instant")),
    }
}

/// ECMA-262 `ToTemporalInstant(item)`.
pub(crate) fn to_temporal_instant(vm: &mut JsVm, item: JsValue) -> Result<Instant, JsException> {
    if let Some(tv) = get_temporal(vm, item) {
        match tv {
            TemporalValue::Instant(i) => return Ok(*i),
            TemporalValue::ZonedDateTime(zdt) => {
                return Instant::try_new(zdt.epoch_nanoseconds().as_i128())
                    .map_err(|e| temporal_err(vm, e));
            }
            _ => {}
        }
    }
    let prim = if item.is_object() {
        vm.to_primitive(item, PrimHint::String)?
    } else {
        item
    };
    let Some(sh) = prim.as_string() else {
        return Err(vm.type_error("Temporal.Instant.from requires a string or Temporal.Instant"));
    };
    let s = vm.heap.materialize_string(sh).unwrap_or_default();
    Instant::from_utf8(s.as_bytes()).map_err(|e| temporal_err(vm, e))
}

fn wrap_instant(vm: &mut JsVm, inst: Instant) -> JsValue {
    alloc_temporal(vm, vm.temporal_instant_proto, TemporalValue::Instant(inst))
}

fn instant_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let proto = ctor_proto_from_new_target(vm, vm.temporal_instant_proto, "Instant")?;
    let ns = to_bigint_i128(vm, arg(args, 0))?;
    let inst = Instant::try_new(ns).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(vm, proto, TemporalValue::Instant(inst)))
}

fn instant_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let inst = to_temporal_instant(vm, arg(args, 0))?;
    Ok(wrap_instant(vm, inst))
}

fn instant_from_epoch_milliseconds(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ms = to_integer_if_integral_i64(vm, arg(args, 0))?;
    let inst = Instant::from_epoch_milliseconds(ms).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_instant(vm, inst))
}

fn instant_from_epoch_nanoseconds(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ns = to_bigint_i128(vm, arg(args, 0))?;
    let inst = Instant::try_new(ns).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_instant(vm, inst))
}

fn instant_compare(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let one = to_temporal_instant(vm, arg(args, 0))?;
    let two = to_temporal_instant(vm, arg(args, 1))?;
    let cmp = match one.cmp(&two) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(JsValue::from_i32(cmp))
}

fn instant_epoch_milliseconds(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    Ok(JsValue::from_f64(inst.epoch_milliseconds() as f64))
}

fn instant_epoch_nanoseconds(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let bh = vm
        .heap
        .alloc_bigint(JsBigInt::from_i128(inst.epoch_nanoseconds().as_i128()));
    Ok(JsValue::from_bigint(bh))
}

fn instant_add(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let res = inst.add(&dur).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_instant(vm, res))
}

fn instant_subtract(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let dur = to_temporal_duration(vm, arg(args, 0))?;
    let res = inst.subtract(&dur).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_instant(vm, res))
}

fn instant_until(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let other = to_temporal_instant(vm, arg(args, 0))?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = inst
        .until(&other, settings)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn instant_since(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let other = to_temporal_instant(vm, arg(args, 0))?;
    let settings = get_difference_settings(vm, arg(args, 1))?;
    let dur = inst
        .since(&other, settings)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_duration_proto,
        TemporalValue::Duration(dur),
    ))
}

fn instant_round(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let opts = get_simple_rounding_options(vm, arg(args, 0))?;
    let res = inst.round(opts).map_err(|e| temporal_err(vm, e))?;
    Ok(wrap_instant(vm, res))
}

fn instant_equals(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let other = to_temporal_instant(vm, arg(args, 0))?;
    Ok(JsValue::from_bool(inst == other))
}

fn instant_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let opts_h = get_options_object(vm, arg(args, 0))?;
    let (tz, rounding) = if let Some(h) = opts_h {
        // Alphabetical read order: fractionalSecondDigits, roundingMode, smallestUnit, timeZone
        let precision = get_fractional_second_digits_option(vm, h)?;
        let rounding_mode = get_rounding_mode_option(vm, h)?;
        let smallest_unit = get_unit_option(vm, h, "smallestUnit", false)?;
        let tz_v = get_prop(vm, h, "timeZone")?;
        let tz = if tz_v.is_undefined() {
            None
        } else {
            Some(to_temporal_time_zone_identifier(vm, tz_v)?)
        };
        (
            tz,
            ToStringRoundingOptions {
                precision,
                smallest_unit,
                rounding_mode,
            },
        )
    } else {
        (None, ToStringRoundingOptions::default())
    };
    let s = inst
        .to_ixdtf_string(tz, rounding)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn instant_to_json(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let s = inst
        .to_ixdtf_string(None, ToStringRoundingOptions::default())
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn instant_to_locale_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let s = inst
        .to_ixdtf_string(None, ToStringRoundingOptions::default())
        .map_err(|e| temporal_err(vm, e))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn instant_to_zoned_date_time_iso(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let inst = require_instant(vm, this_val)?;
    let tz = to_temporal_time_zone_identifier(vm, arg(args, 0))?;
    let zdt = inst
        .to_zoned_date_time_iso(tz)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_zoned_date_time_proto,
        TemporalValue::ZonedDateTime(zdt),
    ))
}

pub(crate) fn install_instant(vm: &mut JsVm, ns: HeapHandle) {
    let proto = vm.temporal_instant_proto;
    let ctor = install_ctor_and_proto(vm, ns, "Instant", 1, instant_ctor, proto);

    define_method(vm, ctor, "from", 1, instant_from);
    define_method(
        vm,
        ctor,
        "fromEpochMilliseconds",
        1,
        instant_from_epoch_milliseconds,
    );
    define_method(
        vm,
        ctor,
        "fromEpochNanoseconds",
        1,
        instant_from_epoch_nanoseconds,
    );
    define_method(vm, ctor, "compare", 2, instant_compare);

    define_getter(vm, proto, "epochMilliseconds", instant_epoch_milliseconds);
    define_getter(vm, proto, "epochNanoseconds", instant_epoch_nanoseconds);
    define_method(vm, proto, "add", 1, instant_add);
    define_method(vm, proto, "subtract", 1, instant_subtract);
    define_method(vm, proto, "until", 1, instant_until);
    define_method(vm, proto, "since", 1, instant_since);
    define_method(vm, proto, "round", 1, instant_round);
    define_method(vm, proto, "equals", 1, instant_equals);
    define_method(vm, proto, "toString", 0, instant_to_string);
    define_method(vm, proto, "toJSON", 0, instant_to_json);
    define_method(vm, proto, "toLocaleString", 0, instant_to_locale_string);
    define_method(vm, proto, "valueOf", 0, temporal_value_of);
    define_method(
        vm,
        proto,
        "toZonedDateTimeISO",
        1,
        instant_to_zoned_date_time_iso,
    );
}
