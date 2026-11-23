//! Pure-Rust ECMAScript `Temporal` built-ins backed by `temporal_rs` (TC39 Temporal).
//!
//! Enforces `#![deny(unsafe_code)]`, strict ECMA-262 option ordering and error types
//! (`TypeError` vs `RangeError`), and zero host/site hardcoding.

pub mod duration;
pub mod instant;
pub mod options;
pub mod plain_date;
pub mod plain_time;
pub mod zoned_date_time;

use temporal_rs::error::ErrorKind;
use temporal_rs::{Instant, PlainDate, PlainDateTime, PlainTime, TemporalError, ZonedDateTime};

use crate::heap::{JsObject, ObjectKind, TemporalValue};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr};

use self::options::{arg, to_temporal_time_zone_identifier};

/// Maps a [`TemporalError`] from `temporal_rs` to the spec-mandated JS error constructor.
pub(crate) fn temporal_err(vm: &mut JsVm, err: TemporalError) -> JsException {
    let msg = err.to_string();
    match err.kind() {
        ErrorKind::Type => vm.type_error(msg),
        ErrorKind::Syntax => vm.make_error("SyntaxError", msg),
        ErrorKind::Range | ErrorKind::Generic | ErrorKind::Assert => vm.range_error(msg),
    }
}

/// Resolves `GetPrototypeFromConstructor(NewTarget, fallback)` for Temporal constructors.
pub(crate) fn ctor_proto_from_new_target(
    vm: &mut JsVm,
    fallback: HeapHandle,
    ctor_name: &str,
) -> Result<HeapHandle, JsException> {
    if vm.native_new_target.is_undefined() {
        return Err(vm.type_error(format!("Constructor Temporal.{ctor_name} requires 'new'")));
    }
    if let Some(nt_h) = vm.native_new_target.as_object() {
        let p = vm.get_property_by_key(nt_h, vm.key_prototype)?;
        if let Some(ph) = p.as_object() {
            return Ok(ph);
        }
    }
    Ok(fallback)
}

/// Allocates a new Temporal JS object carrying an internal [`TemporalValue`] slot.
pub(crate) fn alloc_temporal(vm: &mut JsVm, proto: HeapHandle, val: TemporalValue) -> JsValue {
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    if let Some(o) = vm.heap.get_object_mut(h) {
        o.rare_mut().temporal = Some(Box::new(val));
    }
    JsValue::from_object(h)
}

/// Borrows the internal [`TemporalValue`] slot from `val`, if present.
pub(crate) fn get_temporal(vm: &JsVm, val: JsValue) -> Option<&TemporalValue> {
    let h = val.as_object()?;
    vm.heap
        .get_object(h)
        .and_then(|o| o.rare.as_ref())
        .and_then(|r| r.temporal.as_deref())
}

pub(crate) fn define_method(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    len: u32,
    f: NativeFnPtr,
) {
    let fn_h = vm.alloc_native_fn(name, f);
    vm.set_fn_length(fn_h, len);
    let k = vm.heap.intern_key(name);
    vm.heap.object_define_data(
        target,
        k,
        JsValue::from_object(fn_h),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

pub(crate) fn define_getter(vm: &mut JsVm, proto: HeapHandle, name: &str, f: NativeFnPtr) {
    let fn_h = vm.alloc_native_fn(&format!("get {name}"), f);
    vm.set_fn_length(fn_h, 0);
    let k = vm.heap.intern_key(name);
    vm.heap.object_define_accessor(
        proto,
        k,
        Some(fn_h),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

pub(crate) fn define_to_string_tag(vm: &mut JsVm, target: HeapHandle, tag: &str) {
    let v = JsValue::from_string(vm.heap.alloc_string(tag));
    let k = vm.key_symbol_to_string_tag;
    vm.heap
        .object_define_data(target, k, v, PropertyAttrs::CONFIGURABLE, &mut vm.shapes);
}

pub(crate) fn install_ctor_and_proto(
    vm: &mut JsVm,
    temporal_ns: HeapHandle,
    name: &str,
    ctor_len: u32,
    ctor_fn: NativeFnPtr,
    proto: HeapHandle,
) -> HeapHandle {
    let ctor = vm.alloc_native_fn(name, ctor_fn);
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, ctor_len);
    let kp = vm.key_prototype;
    let kc = vm.key_constructor;
    vm.heap.object_define_data(
        ctor,
        kp,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        proto,
        kc,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_to_string_tag(vm, proto, &format!("Temporal.{name}"));
    let k_name = vm.heap.intern_key(name);
    vm.heap.object_define_data(
        temporal_ns,
        k_name,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    ctor
}

pub(crate) fn temporal_value_of(
    vm: &mut JsVm,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    Err(vm.type_error("Cannot convert Temporal object to primitive via valueOf"))
}

// ---------------------------------------------------------------------------
// Temporal.Now (§2)
// ---------------------------------------------------------------------------

fn now_time_zone_id(
    vm: &mut JsVm,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let tz = temporal_rs::Temporal::local_now()
        .time_zone()
        .unwrap_or_else(|_| temporal_rs::TimeZone::utc());
    let id = tz.identifier().unwrap_or_else(|_| "UTC".to_string());
    Ok(JsValue::from_string(vm.heap.alloc_string(id)))
}

fn now_instant(vm: &mut JsVm, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    let inst: Instant = temporal_rs::Temporal::local_now()
        .instant()
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_instant_proto,
        TemporalValue::Instant(inst),
    ))
}

fn parse_optional_tz(
    vm: &mut JsVm,
    args: &[JsValue],
) -> Result<Option<temporal_rs::TimeZone>, JsException> {
    let v = arg(args, 0);
    if v.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(to_temporal_time_zone_identifier(vm, v)?))
    }
}

fn now_zoned_date_time_iso(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let tz = parse_optional_tz(vm, args)?;
    let zdt: ZonedDateTime = temporal_rs::Temporal::local_now()
        .zoned_date_time_iso(tz)
        .or_else(|_| temporal_rs::Temporal::utc_now().zoned_date_time_iso(tz))
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_zoned_date_time_proto,
        TemporalValue::ZonedDateTime(zdt),
    ))
}

fn now_plain_date_time_iso(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let tz = parse_optional_tz(vm, args)?;
    let pdt: PlainDateTime = temporal_rs::Temporal::local_now()
        .plain_date_time_iso(tz)
        .or_else(|_| temporal_rs::Temporal::utc_now().plain_date_time_iso(tz))
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_plain_date_time_proto,
        TemporalValue::PlainDateTime(pdt),
    ))
}

fn now_plain_date_iso(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let tz = parse_optional_tz(vm, args)?;
    let pd: PlainDate = temporal_rs::Temporal::local_now()
        .plain_date_iso(tz)
        .or_else(|_| temporal_rs::Temporal::utc_now().plain_date_iso(tz))
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_plain_date_proto,
        TemporalValue::PlainDate(pd),
    ))
}

fn now_plain_time_iso(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let tz = parse_optional_tz(vm, args)?;
    let pt: PlainTime = temporal_rs::Temporal::local_now()
        .plain_time_iso(tz)
        .or_else(|_| temporal_rs::Temporal::utc_now().plain_time_iso(tz))
        .map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_plain_time_proto,
        TemporalValue::PlainTime(pt),
    ))
}

// ---------------------------------------------------------------------------
// Date.prototype.toTemporalInstant (§14.1)
// ---------------------------------------------------------------------------

fn date_to_temporal_instant(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let (_, tv) = crate::builtins_date::this_time_value(vm, this_val)?;
    if !tv.is_finite() {
        return Err(vm.range_error("Invalid Date cannot be converted to Temporal.Instant"));
    }
    let inst = Instant::from_epoch_milliseconds(tv as i64).map_err(|e| temporal_err(vm, e))?;
    Ok(alloc_temporal(
        vm,
        vm.temporal_instant_proto,
        TemporalValue::Instant(inst),
    ))
}

/// Installs the `Temporal` namespace, `Temporal.Now`, `Temporal.Instant`,
/// `Temporal.Duration`, and `Date.prototype.toTemporalInstant`.
pub fn install_temporal(vm: &mut JsVm) {
    let ns = vm.alloc_ordinary_object();
    define_to_string_tag(vm, ns, "Temporal");

    let now_obj = vm.alloc_ordinary_object();
    define_to_string_tag(vm, now_obj, "Temporal.Now");
    define_method(vm, now_obj, "timeZoneId", 0, now_time_zone_id);
    define_method(vm, now_obj, "instant", 0, now_instant);
    define_method(vm, now_obj, "zonedDateTimeISO", 0, now_zoned_date_time_iso);
    define_method(vm, now_obj, "plainDateTimeISO", 0, now_plain_date_time_iso);
    define_method(vm, now_obj, "plainDateISO", 0, now_plain_date_iso);
    define_method(vm, now_obj, "plainTimeISO", 0, now_plain_time_iso);
    let k_now = vm.heap.intern_key("Now");
    vm.heap.object_define_data(
        ns,
        k_now,
        JsValue::from_object(now_obj),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    instant::install_instant(vm, ns);
    duration::install_duration(vm, ns);
    plain_date::install_plain_date_types(vm, ns);
    plain_time::install_plain_time_types(vm, ns);
    zoned_date_time::install_zoned_date_time(vm, ns);

    // Date.prototype.toTemporalInstant
    let date_proto = vm.date_proto;
    define_method(
        vm,
        date_proto,
        "toTemporalInstant",
        0,
        date_to_temporal_instant,
    );

    let k_temporal = vm.heap.intern_key("Temporal");
    let global = vm.global;
    vm.heap.object_define_data(
        global,
        k_temporal,
        JsValue::from_object(ns),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}
