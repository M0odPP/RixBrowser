//! Genuine ECMA-402 Internationalization (`Intl`) powered by pure-Rust ICU4X 2.4
//! (`icu::locale`, `icu::collator`, `icu::decimal`, `icu::datetime`, `icu::plurals`,
//! `icu::list`, `icu::segmenter`, `icu::experimental::{relativetime, dimension}`)
//! and bundled IANA TZDB via `jiff`.
//!
//! Reference: Spec 06 §3, Issue #25.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use fixed_decimal::{
    Decimal, FloatPrecision, SignDisplay, SignedRoundingMode, UnsignedRoundingMode,
};
use icu::calendar::Date;
use icu::collator::options::{AlternateHandling, CaseLevel, CollatorOptions, Strength};
use icu::collator::preferences::{CollationCaseFirst, CollationNumericOrdering};
use icu::collator::{Collator, CollatorPreferences};
use icu::datetime::fieldsets::builder::{DateFields, FieldSetBuilder, ZoneStyle};
use icu::datetime::options::{Alignment, Length, TimePrecision, YearStyle};
use icu::datetime::DateTimeFormatter;
use icu::decimal::options::{DecimalFormatterOptions, GroupingStrategy};
use icu::decimal::{CompactDecimalFormatter, DecimalFormatter};
use icu::experimental::dimension::currency::formatter::CurrencyFormatter;
use icu::experimental::dimension::currency::options::CurrencyFormatterOptions;
use icu::experimental::dimension::currency::CurrencyType;
use icu::experimental::dimension::percent::formatter::PercentFormatter;
use icu::experimental::dimension::percent::options::PercentFormatterOptions;
use icu::experimental::relativetime::options::Numeric as RelNumeric;
use icu::experimental::relativetime::{RelativeTimeFormatter, RelativeTimeFormatterOptions};
use icu::list::options::{ListFormatterOptions, ListLength};
use icu::list::ListFormatter;
use icu::locale::extensions::unicode::Key as UnicodeKey;
use icu::locale::subtags::{Language, Region, Script};
use icu::locale::{Locale, LocaleCanonicalizer, LocaleExpander};
use icu::plurals::{
    PluralCategory, PluralOperands, PluralRuleType, PluralRules, PluralRulesOptions,
};
use icu::segmenter::options::{SentenceBreakOptions, WordBreakOptions};
use icu::segmenter::{GraphemeClusterSegmenter, SentenceSegmenter, WordSegmenter};
use icu::time::zone::{IanaParser, UtcOffset};
use icu::time::{Time, ZonedDateTime};
use jiff::tz::TimeZone as JiffTimeZone;
use jiff::Timestamp;
use writeable::{Part, PartsWrite, Writeable};

use crate::heap::{JsObject, ObjectKind};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm};

// ---------------------------------------------------------------------------
// Writeable -> ECMA-402 formatToParts collector
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PartsCollector {
    parts: Vec<(&'static str, String)>,
    current_type: Option<&'static str>,
}

impl PartsCollector {
    fn push_str_typed(&mut self, ty: &'static str, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(last) = self.parts.last_mut() {
            if last.0 == ty {
                last.1.push_str(s);
                return;
            }
        }
        self.parts.push((ty, s.to_string()));
    }
}

impl fmt::Write for PartsCollector {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let ty = self.current_type.unwrap_or("literal");
        self.push_str_typed(ty, s);
        Ok(())
    }
}

fn is_datetime_part(ty: &str) -> bool {
    matches!(
        ty,
        "era"
            | "year"
            | "month"
            | "day"
            | "weekday"
            | "dayPeriod"
            | "hour"
            | "minute"
            | "second"
            | "fractionalSecond"
            | "timeZoneName"
    )
}

impl PartsWrite for PartsCollector {
    type SubPartsWrite = Self;

    fn with_part(
        &mut self,
        part: Part,
        mut f: impl FnMut(&mut Self::SubPartsWrite) -> fmt::Result,
    ) -> fmt::Result {
        let mapped = map_icu_part(part);
        let prev = self.current_type;
        let keep_outer = prev.is_some_and(is_datetime_part) && !is_datetime_part(mapped);
        if !keep_outer && (mapped != "literal" || prev.is_none()) {
            self.current_type = Some(mapped);
        }
        let res = f(self);
        self.current_type = prev;
        res
    }
}

fn map_icu_part(part: Part) -> &'static str {
    match (part.category, part.value) {
        ("decimal", "integer") => "integer",
        ("decimal", "fraction") => "fraction",
        ("decimal", "group") => "group",
        ("decimal", "decimal") => "decimal",
        ("decimal", "minusSign") => "minusSign",
        ("decimal", "plusSign") => "plusSign",
        ("decimal", "compact") => "compact",
        ("currency", "currency") | ("dimension", "currency") => "currency",
        ("percent", "percentSign") | ("dimension", "percentSign") => "percentSign",
        ("list", "element") => "element",
        ("list", "literal") => "literal",
        ("datetime", "era") => "era",
        ("datetime", "year") => "year",
        ("datetime", "month") => "month",
        ("datetime", "day") => "day",
        ("datetime", "weekday") => "weekday",
        ("datetime", "dayPeriod") => "dayPeriod",
        ("datetime", "hour") => "hour",
        ("datetime", "minute") => "minute",
        ("datetime", "second") => "second",
        ("datetime", "fractionalSecond") => "fractionalSecond",
        ("datetime", "timeZoneName") => "timeZoneName",
        _ => "literal",
    }
}

fn collect_parts<W: Writeable>(w: &W) -> Vec<(&'static str, String)> {
    let mut c = PartsCollector::default();
    let _ = w.write_to_parts(&mut c);
    c.parts
}

// ---------------------------------------------------------------------------
// Internal Intl state & helper constructors
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LocaleRecord {
    locale: Locale,
}

#[derive(Clone)]
struct CollatorRecord {
    locale: Locale,
    usage: String,
    sensitivity: String,
    ignore_punctuation: bool,
    numeric: bool,
    case_first: String,
}

#[derive(Clone)]
struct NumberFormatRecord {
    locale: Locale,
    style: String,
    currency: Option<String>,
    currency_display: String,
    unit: Option<String>,
    unit_display: String,
    notation: String,
    compact_display: String,
    use_grouping: String,
    sign_display: String,
    min_int_digits: u16,
    min_frac_digits: Option<u16>,
    max_frac_digits: Option<u16>,
    min_sig_digits: Option<u16>,
    max_sig_digits: Option<u16>,
}

#[derive(Clone)]
struct DateTimeFormatRecord {
    locale: Locale,
    time_zone: String,
    date_style: Option<String>,
    time_style: Option<String>,
    year: Option<String>,
    month: Option<String>,
    day: Option<String>,
    weekday: Option<String>,
    hour: Option<String>,
    minute: Option<String>,
    second: Option<String>,
    time_zone_name: Option<String>,
    hour12: Option<bool>,
    hour_cycle: Option<String>,
}

#[derive(Clone)]
struct PluralRulesRecord {
    locale: Locale,
    rule_type: String,
    min_int_digits: u16,
    min_frac_digits: u16,
    max_frac_digits: u16,
}

#[derive(Clone)]
struct ListFormatRecord {
    locale: Locale,
    list_type: String,
    style: String,
}

#[derive(Clone)]
struct SegmenterRecord {
    locale: Locale,
    granularity: String,
}

#[derive(Clone)]
struct SegmentsRecord {
    input: String,
    /// `(utf16_index, segment_text, is_word_like)`
    items: Vec<(usize, String, Option<bool>)>,
}

#[derive(Clone)]
struct SegmentIterState {
    segments_h: HeapHandle,
    cursor: usize,
}

#[derive(Clone)]
struct RelativeTimeFormatRecord {
    locale: Locale,
    numeric: String,
    style: String,
}

#[derive(Default)]
struct IntlState {
    locales: HashMap<HeapHandle, LocaleRecord>,
    collators: HashMap<HeapHandle, CollatorRecord>,
    number_formats: HashMap<HeapHandle, NumberFormatRecord>,
    date_time_formats: HashMap<HeapHandle, DateTimeFormatRecord>,
    plural_rules: HashMap<HeapHandle, PluralRulesRecord>,
    list_formats: HashMap<HeapHandle, ListFormatRecord>,
    segmenters: HashMap<HeapHandle, SegmenterRecord>,
    segments: HashMap<HeapHandle, SegmentsRecord>,
    segment_iters: HashMap<HeapHandle, SegmentIterState>,
    relative_time_formats: HashMap<HeapHandle, RelativeTimeFormatRecord>,
    locale_proto: Option<HeapHandle>,
    collator_proto: Option<HeapHandle>,
    number_format_proto: Option<HeapHandle>,
    date_time_format_proto: Option<HeapHandle>,
    plural_rules_proto: Option<HeapHandle>,
    list_format_proto: Option<HeapHandle>,
    segmenter_proto: Option<HeapHandle>,
    segments_proto: Option<HeapHandle>,
    segment_iter_proto: Option<HeapHandle>,
    relative_time_format_proto: Option<HeapHandle>,
}

type SharedIntl = Rc<RefCell<IntlState>>;

fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

fn def_data(vm: &mut JsVm, obj: HeapHandle, name: &str, val: JsValue, attrs: PropertyAttrs) {
    let k = vm.heap.intern_key(name);
    vm.heap
        .object_define_data(obj, k, val, attrs, &mut vm.shapes);
}

fn def_host_method(
    vm: &mut JsVm,
    obj: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let fh = vm.alloc_host_fn(name, f);
    def_data(
        vm,
        obj,
        name,
        JsValue::from_object(fh),
        PropertyAttrs::METHOD,
    );
}

fn def_host_getter(
    vm: &mut JsVm,
    obj: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let key = vm.heap.intern_key(name);
    let fh = vm.alloc_host_fn(name, f);
    vm.heap.object_define_accessor(
        obj,
        key,
        Some(fh),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn bind_ctor_proto(vm: &mut JsVm, ctor: HeapHandle, proto: HeapHandle) {
    vm.mark_constructor(ctor);
    def_data(
        vm,
        ctor,
        "prototype",
        JsValue::from_object(proto),
        PropertyAttrs::READONLY,
    );
    def_data(
        vm,
        proto,
        "constructor",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

fn alloc_array_of(vm: &mut JsVm, items: &[JsValue]) -> HeapHandle {
    let arr = vm.alloc_array();
    for &v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

fn get_array_len(vm: &mut JsVm, h: HeapHandle) -> Result<usize, JsException> {
    if let Some(obj) = vm.heap.get_object(h) {
        if matches!(obj.kind, ObjectKind::Array) {
            return Ok(obj.elements.len() as usize);
        }
    }
    let len_k = vm.heap.intern_key("length");
    let len_v = vm.get_property_by_key(h, len_k)?;
    Ok(vm.to_number(len_v).unwrap_or(0.0).max(0.0) as usize)
}

fn get_array_elem(vm: &mut JsVm, h: HeapHandle, i: usize) -> Result<JsValue, JsException> {
    if let Ok(idx) = u32::try_from(i) {
        if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(idx)) {
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    let k = vm.heap.intern_key(&i.to_string());
    vm.get_property_by_key(h, k)
}

fn str_val(vm: &mut JsVm, s: &str) -> JsValue {
    JsValue::from_string(vm.heap.alloc_string(s))
}

fn get_opt_prop(vm: &mut JsVm, opts: JsValue, name: &str) -> Result<JsValue, JsException> {
    if opts.is_undefined() {
        return Ok(JsValue::UNDEFINED);
    }
    if opts.is_null() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    let Some(h) = opts.as_object() else {
        return Ok(JsValue::UNDEFINED);
    };
    let k = vm.heap.intern_key(name);
    vm.get_property_by_key(h, k)
}

fn get_opt_string(
    vm: &mut JsVm,
    opts: JsValue,
    name: &str,
    allowed: Option<&[&str]>,
) -> Result<Option<String>, JsException> {
    let v = get_opt_prop(vm, opts, name)?;
    if v.is_undefined() {
        return Ok(None);
    }
    let s = vm.value_to_rust_string(v);
    if let Some(valid) = allowed {
        if !valid.contains(&s.as_str()) {
            return Err(vm.range_error(format!("Invalid value '{s}' for option '{name}'")));
        }
    }
    Ok(Some(s))
}

fn get_opt_bool(vm: &mut JsVm, opts: JsValue, name: &str) -> Result<Option<bool>, JsException> {
    let v = get_opt_prop(vm, opts, name)?;
    if v.is_undefined() {
        return Ok(None);
    }
    Ok(Some(vm.to_boolean(v)))
}

fn get_opt_u16_range(
    vm: &mut JsVm,
    opts: JsValue,
    name: &str,
    min: u16,
    max: u16,
) -> Result<Option<u16>, JsException> {
    let v = get_opt_prop(vm, opts, name)?;
    if v.is_undefined() {
        return Ok(None);
    }
    let n = vm.to_number(v)?;
    if n.is_nan() || n < f64::from(min) || n > f64::from(max) {
        return Err(vm.range_error(format!(
            "Option '{name}' value {n} is out of range [{min}, {max}]"
        )));
    }
    Ok(Some(n.floor() as u16))
}

// ---------------------------------------------------------------------------
// BCP-47 Locale Parsing & Canonicalization via ICU4X
// ---------------------------------------------------------------------------

fn parse_and_canonicalize_tag(vm: &mut JsVm, raw: &str) -> Result<Locale, JsException> {
    if raw.is_empty()
        || raw.contains('_')
        || raw.contains("--")
        || raw.starts_with('-')
        || raw.ends_with('-')
    {
        return Err(vm.range_error(format!("Invalid language tag: {raw}")));
    }
    let mut loc = Locale::try_from_str(raw)
        .map_err(|_| vm.range_error(format!("Invalid language tag: {raw}")))?;
    let canon = LocaleCanonicalizer::new_extended();
    let _ = canon.canonicalize(&mut loc);
    Ok(loc)
}

fn canonicalize_locale_list(
    vm: &mut JsVm,
    state: &SharedIntl,
    locales_val: JsValue,
) -> Result<Vec<Locale>, JsException> {
    if locales_val.is_undefined() {
        return Ok(Vec::new());
    }
    if locales_val.is_null() {
        return Err(vm.type_error("Cannot convert null to locale list"));
    }
    if locales_val.is_string() {
        let s = vm.value_to_rust_string(locales_val);
        return Ok(vec![parse_and_canonicalize_tag(vm, &s)?]);
    }
    let Some(h) = locales_val.as_object() else {
        return Err(vm.type_error("Language tag must be a string or Intl.Locale"));
    };
    if let Some(rec) = state.borrow().locales.get(&h) {
        return Ok(vec![rec.locale.clone()]);
    }
    let len = get_array_len(vm, h)?;
    let mut out: Vec<Locale> = Vec::new();
    for i in 0..len {
        let elem = get_array_elem(vm, h, i)?;
        if elem.is_undefined() {
            continue;
        }
        let loc = if let Some(eh) = elem.as_object() {
            if let Some(rec) = state.borrow().locales.get(&eh) {
                rec.locale.clone()
            } else {
                let s = vm.value_to_rust_string(elem);
                parse_and_canonicalize_tag(vm, &s)?
            }
        } else if elem.is_string() {
            let s = vm.value_to_rust_string(elem);
            parse_and_canonicalize_tag(vm, &s)?
        } else {
            return Err(vm.type_error("Language tag must be a string or Intl.Locale"));
        };
        if !out.contains(&loc) {
            out.push(loc);
        }
    }
    Ok(out)
}

fn resolve_first_locale(
    vm: &mut JsVm,
    state: &SharedIntl,
    locales_val: JsValue,
) -> Result<Locale, JsException> {
    let list = canonicalize_locale_list(vm, state, locales_val)?;
    Ok(list
        .into_iter()
        .next()
        .unwrap_or_else(|| "en-US".parse::<Locale>().expect("en-US")))
}

fn supported_locales_of(
    vm: &mut JsVm,
    state: &SharedIntl,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let opts = arg(args, 1);
    let _ = get_opt_string(vm, opts, "localeMatcher", Some(&["lookup", "best fit"]))?;
    let list = canonicalize_locale_list(vm, state, arg(args, 0))?;
    let vals: Vec<JsValue> = list
        .into_iter()
        .map(|l| str_val(vm, &l.to_string()))
        .collect();
    Ok(JsValue::from_object(alloc_array_of(vm, &vals)))
}

// ---------------------------------------------------------------------------
// Public entry point: install global `Intl` object
// ---------------------------------------------------------------------------

/// Installs the complete ECMA-402 `Intl` namespace on `vm.global`, backed by ICU4X 2.4
/// and bundled IANA TZDB (`jiff`).
pub fn install_intl(vm: &mut JsVm) {
    let state: SharedIntl = Rc::new(RefCell::new(IntlState::default()));
    let intl_obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));

    // Intl.getCanonicalLocales(locales)
    {
        let st = Rc::clone(&state);
        def_host_method(vm, intl_obj, "getCanonicalLocales", move |vm, _, args| {
            let list = canonicalize_locale_list(vm, &st, arg(args, 0))?;
            let vals: Vec<JsValue> = list
                .into_iter()
                .map(|l| str_val(vm, &l.to_string()))
                .collect();
            Ok(JsValue::from_object(alloc_array_of(vm, &vals)))
        });
    }

    // Intl.supportedValuesOf(key)
    def_host_method(vm, intl_obj, "supportedValuesOf", |vm, _, args| {
        let key = vm.value_to_rust_string(arg(args, 0));
        let values: Vec<String> = match key.as_str() {
            "calendar" => vec![
                "buddhist",
                "chinese",
                "coptic",
                "dangi",
                "ethioaa",
                "ethiopic",
                "gregory",
                "hebrew",
                "indian",
                "islamic-civil",
                "islamic-tbla",
                "islamic-umalqura",
                "iso8601",
                "japanese",
                "persian",
                "roc",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            "collation" => vec![
                "big5han", "compat", "dict", "direct", "ducet", "emoji", "eor", "gb2312",
                "phonebk", "phonetic", "pinyin", "reformed", "searchjl", "stroke", "trad",
                "unihan", "zhuyin",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            "currency" => vec![
                "AUD", "BRL", "CAD", "CHF", "CNY", "CZK", "DKK", "EUR", "GBP", "HKD", "HUF", "IDR",
                "ILS", "INR", "JPY", "KRW", "MXN", "MYR", "NOK", "NZD", "PHP", "PLN", "RON", "SEK",
                "SGD", "THB", "TRY", "TWD", "UAH", "USD", "ZAR",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            "numberingSystem" => vec![
                "arab", "arabext", "beng", "deva", "fullwide", "gujr", "guru", "hanidec", "khmr",
                "knda", "laoo", "latn", "mlym", "mong", "mymr", "orya", "tamldec", "telu", "thai",
                "tibt",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            "timeZone" => {
                let mut zones: Vec<String> = jiff::tz::db()
                    .available()
                    .map(|z| z.as_str().to_string())
                    .collect();
                zones.sort();
                zones.dedup();
                zones
            }
            "unit" => vec![
                "acre",
                "bit",
                "byte",
                "celsius",
                "centimeter",
                "day",
                "degree",
                "fahrenheit",
                "fluid-ounce",
                "foot",
                "gallon",
                "gigabit",
                "gigabyte",
                "gram",
                "hectare",
                "hour",
                "inch",
                "kilobit",
                "kilobyte",
                "kilogram",
                "kilometer",
                "liter",
                "megabit",
                "megabyte",
                "meter",
                "microsecond",
                "mile",
                "mile-scandinavian",
                "milliliter",
                "millimeter",
                "millisecond",
                "minute",
                "month",
                "nanosecond",
                "ounce",
                "percent",
                "petabyte",
                "pound",
                "second",
                "stone",
                "terabit",
                "terabyte",
                "week",
                "yard",
                "year",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            _ => {
                return Err(
                    vm.range_error(format!("Invalid key '{key}' for Intl.supportedValuesOf"))
                );
            }
        };
        let js_vals: Vec<JsValue> = values.iter().map(|s| str_val(vm, s)).collect();
        Ok(JsValue::from_object(alloc_array_of(vm, &js_vals)))
    });

    install_intl_locale(vm, intl_obj, &state);
    install_intl_collator(vm, intl_obj, &state);
    install_intl_number_format(vm, intl_obj, &state);
    install_intl_date_time_format(vm, intl_obj, &state);
    install_intl_plural_rules(vm, intl_obj, &state);
    install_intl_list_format(vm, intl_obj, &state);
    install_intl_segmenter(vm, intl_obj, &state);
    install_intl_relative_time_format(vm, intl_obj, &state);

    let global = vm.global;
    def_data(
        vm,
        global,
        "Intl",
        JsValue::from_object(intl_obj),
        PropertyAttrs::METHOD,
    );
    install_intl_prototype_hooks(vm, intl_obj);
}

fn install_intl_prototype_hooks(vm: &mut JsVm, intl_obj: HeapHandle) {
    let sp = vm.string_proto;
    let lc_fn = vm.alloc_host_fn("localeCompare", move |vm, this_val, args| {
        if this_val.is_null() || this_val.is_undefined() {
            return Err(vm.type_error("Cannot convert undefined or null to object"));
        }
        let a = vm.to_js_string(this_val)?;
        let b = vm.to_js_string(arg(args, 0))?;
        let ck = vm.heap.intern_key("Collator");
        let ctor = vm.get_property_by_key(intl_obj, ck)?;
        let coll = vm.call_value(ctor, JsValue::UNDEFINED, &[arg(args, 1), arg(args, 2)])?;
        let Some(ch) = coll.as_object() else {
            return Ok(JsValue::from_number(0.0));
        };
        let cmp_k = vm.heap.intern_key("compare");
        let cmp_fn = vm.get_property_by_key(ch, cmp_k)?;
        let av = str_val(vm, &a);
        let bv = str_val(vm, &b);
        vm.call_value(cmp_fn, JsValue::UNDEFINED, &[av, bv])
    });
    vm.set_fn_length(lc_fn, 1);
    let k_lc = vm.heap.intern_key("localeCompare");
    vm.heap.object_define_data(
        sp,
        k_lc,
        JsValue::from_object(lc_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let np = vm.number_proto;
    def_host_method(vm, np, "toLocaleString", move |vm, this_val, args| {
        let nk = vm.heap.intern_key("NumberFormat");
        let ctor = vm.get_property_by_key(intl_obj, nk)?;
        let nf = vm.call_value(ctor, JsValue::UNDEFINED, &[arg(args, 0), arg(args, 1)])?;
        let Some(nh) = nf.as_object() else {
            return Ok(str_val(vm, ""));
        };
        let fk = vm.heap.intern_key("format");
        let fmt_fn = vm.get_property_by_key(nh, fk)?;
        vm.call_value(fmt_fn, JsValue::UNDEFINED, &[this_val])
    });

    let dp = vm.date_proto;
    for (method, default_date_style, default_time_style) in [
        ("toLocaleString", Some("short"), Some("medium")),
        ("toLocaleDateString", Some("short"), None),
        ("toLocaleTimeString", None, Some("medium")),
    ] {
        let fh = vm.alloc_host_fn(method, move |vm, this_val, args| {
            let (_, tv) = crate::builtins_date::this_time_value(vm, this_val)?;
            if !tv.is_finite() {
                return Ok(str_val(vm, "Invalid Date"));
            }
            let dk = vm.heap.intern_key("DateTimeFormat");
            let ctor = vm.get_property_by_key(intl_obj, dk)?;
            let opts = if arg(args, 1).is_undefined() {
                let o = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Ordinary,
                    Some(vm.object_proto),
                ));
                if let Some(ds) = default_date_style {
                    let v = str_val(vm, ds);
                    def_data(vm, o, "dateStyle", v, PropertyAttrs::DEFAULT);
                }
                if let Some(ts) = default_time_style {
                    let v = str_val(vm, ts);
                    def_data(vm, o, "timeStyle", v, PropertyAttrs::DEFAULT);
                }
                JsValue::from_object(o)
            } else {
                arg(args, 1)
            };
            let dtf = vm.call_value(ctor, JsValue::UNDEFINED, &[arg(args, 0), opts])?;
            let Some(dh) = dtf.as_object() else {
                return Ok(str_val(vm, ""));
            };
            let fk = vm.heap.intern_key("format");
            let fmt_fn = vm.get_property_by_key(dh, fk)?;
            vm.call_value(fmt_fn, JsValue::UNDEFINED, &[this_val])
        });
        vm.set_fn_length(fh, 0);
        def_data(
            vm,
            dp,
            method,
            JsValue::from_object(fh),
            PropertyAttrs::METHOD,
        );
    }
}

// ---------------------------------------------------------------------------
// 1. Intl.Locale
// ---------------------------------------------------------------------------

fn unicode_ext_get(loc: &Locale, key_str: &str) -> Option<String> {
    let key: UnicodeKey = key_str.parse().ok()?;
    loc.extensions
        .unicode
        .keywords
        .get(&key)
        .map(ToString::to_string)
}

fn unicode_ext_set(loc: &mut Locale, key_str: &str, val_str: &str) -> bool {
    let Ok(key) = key_str.parse::<UnicodeKey>() else {
        return false;
    };
    let Ok(val) = val_str.parse() else {
        return false;
    };
    loc.extensions.unicode.keywords.set(key, val);
    true
}

fn install_intl_locale(vm: &mut JsVm, intl_obj: HeapHandle, state: &SharedIntl) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    state.borrow_mut().locale_proto = Some(proto);

    {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, "baseName", move |vm, this_val, _| {
            let loc = require_locale(vm, &st, this_val)?;
            Ok(str_val(vm, &loc.id.to_string()))
        });
    }
    {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, "language", move |vm, this_val, _| {
            let loc = require_locale(vm, &st, this_val)?;
            Ok(str_val(vm, loc.id.language.as_str()))
        });
    }
    {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, "script", move |vm, this_val, _| {
            let loc = require_locale(vm, &st, this_val)?;
            Ok(loc
                .id
                .script
                .map_or(JsValue::UNDEFINED, |s| str_val(vm, s.as_str())))
        });
    }
    {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, "region", move |vm, this_val, _| {
            let loc = require_locale(vm, &st, this_val)?;
            Ok(loc
                .id
                .region
                .map_or(JsValue::UNDEFINED, |r| str_val(vm, r.as_str())))
        });
    }
    for (prop, ukey) in [
        ("calendar", "ca"),
        ("collation", "co"),
        ("numberingSystem", "nu"),
        ("hourCycle", "hc"),
        ("caseFirst", "kf"),
    ] {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, prop, move |vm, this_val, _| {
            let loc = require_locale(vm, &st, this_val)?;
            Ok(unicode_ext_get(&loc, ukey).map_or(JsValue::UNDEFINED, |v| str_val(vm, &v)))
        });
    }
    {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, "numeric", move |vm, this_val, _| {
            let loc = require_locale(vm, &st, this_val)?;
            let kn = unicode_ext_get(&loc, "kn");
            let is_num = matches!(kn.as_deref(), Some("" | "true"));
            Ok(JsValue::from_bool(is_num))
        });
    }
    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "toString", move |vm, this_val, _| {
            let loc = require_locale(vm, &st, this_val)?;
            Ok(str_val(vm, &loc.to_string()))
        });
    }
    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "maximize", move |vm, this_val, _| {
            let mut loc = require_locale(vm, &st, this_val)?;
            let expander = LocaleExpander::new_extended();
            let _ = expander.maximize(&mut loc.id);
            Ok(alloc_locale_obj(vm, &st, loc))
        });
    }
    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "minimize", move |vm, this_val, _| {
            let mut loc = require_locale(vm, &st, this_val)?;
            let expander = LocaleExpander::new_extended();
            let _ = expander.minimize(&mut loc.id);
            Ok(alloc_locale_obj(vm, &st, loc))
        });
    }

    let st = Rc::clone(state);
    let ctor = vm.alloc_host_fn("Locale", move |vm, _, args| {
        let tag_arg = arg(args, 0);
        let mut loc = if let Some(h) = tag_arg.as_object() {
            if let Some(rec) = st.borrow().locales.get(&h) {
                rec.locale.clone()
            } else {
                let s = vm.value_to_rust_string(tag_arg);
                parse_and_canonicalize_tag(vm, &s)?
            }
        } else if tag_arg.is_string() {
            let s = vm.value_to_rust_string(tag_arg);
            parse_and_canonicalize_tag(vm, &s)?
        } else {
            return Err(vm.type_error("Intl.Locale tag must be a string or Intl.Locale"));
        };

        let opts = arg(args, 1);
        if let Some(lang) = get_opt_string(vm, opts, "language", None)? {
            loc.id.language = lang
                .parse::<Language>()
                .map_err(|_| vm.range_error(format!("Invalid language subtag: {lang}")))?;
        }
        if let Some(scr) = get_opt_string(vm, opts, "script", None)? {
            loc.id.script = Some(
                scr.parse::<Script>()
                    .map_err(|_| vm.range_error(format!("Invalid script subtag: {scr}")))?,
            );
        }
        if let Some(reg) = get_opt_string(vm, opts, "region", None)? {
            loc.id.region = Some(
                reg.parse::<Region>()
                    .map_err(|_| vm.range_error(format!("Invalid region subtag: {reg}")))?,
            );
        }
        if let Some(ca) = get_opt_string(vm, opts, "calendar", None)? {
            if !unicode_ext_set(&mut loc, "ca", &ca) {
                return Err(vm.range_error(format!("Invalid calendar: {ca}")));
            }
        }
        if let Some(co) = get_opt_string(vm, opts, "collation", None)? {
            if !unicode_ext_set(&mut loc, "co", &co) {
                return Err(vm.range_error(format!("Invalid collation: {co}")));
            }
        }
        if let Some(nu) = get_opt_string(vm, opts, "numberingSystem", None)? {
            if !unicode_ext_set(&mut loc, "nu", &nu) {
                return Err(vm.range_error(format!("Invalid numberingSystem: {nu}")));
            }
        }
        if let Some(hc) =
            get_opt_string(vm, opts, "hourCycle", Some(&["h11", "h12", "h23", "h24"]))?
        {
            let _ = unicode_ext_set(&mut loc, "hc", &hc);
        }
        if let Some(kf) = get_opt_string(vm, opts, "caseFirst", Some(&["upper", "lower", "false"]))?
        {
            let _ = unicode_ext_set(&mut loc, "kf", &kf);
        }
        if let Some(kn) = get_opt_bool(vm, opts, "numeric")? {
            let _ = unicode_ext_set(&mut loc, "kn", if kn { "true" } else { "false" });
        }
        let canon = LocaleCanonicalizer::new_extended();
        let _ = canon.canonicalize(&mut loc);
        Ok(alloc_locale_obj(vm, &st, loc))
    });
    bind_ctor_proto(vm, ctor, proto);
    def_data(
        vm,
        intl_obj,
        "Locale",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

fn require_locale(
    vm: &mut JsVm,
    state: &SharedIntl,
    this_val: JsValue,
) -> Result<Locale, JsException> {
    this_val
        .as_object()
        .and_then(|h| state.borrow().locales.get(&h).map(|r| r.locale.clone()))
        .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.Locale"))
}

fn alloc_locale_obj(vm: &mut JsVm, state: &SharedIntl, locale: Locale) -> JsValue {
    let proto = state.borrow().locale_proto.unwrap_or(vm.object_proto);
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    state
        .borrow_mut()
        .locales
        .insert(h, LocaleRecord { locale });
    JsValue::from_object(h)
}

// ---------------------------------------------------------------------------
// 2. Intl.Collator (ICU4X Collator)
// ---------------------------------------------------------------------------

fn compare_with_collator_record(rec: &CollatorRecord, a: &str, b: &str) -> i32 {
    let mut prefs: CollatorPreferences = (&rec.locale).into();
    if rec.numeric {
        prefs.numeric_ordering = Some(CollationNumericOrdering::True);
    }
    prefs.case_first = match rec.case_first.as_str() {
        "upper" => Some(CollationCaseFirst::Upper),
        "lower" => Some(CollationCaseFirst::Lower),
        _ => None,
    };
    let mut opts = CollatorOptions::default();
    match rec.sensitivity.as_str() {
        "base" => {
            opts.strength = Some(Strength::Primary);
        }
        "accent" => {
            opts.strength = Some(Strength::Secondary);
        }
        "case" => {
            opts.strength = Some(Strength::Primary);
            opts.case_level = Some(CaseLevel::On);
        }
        _ => {
            opts.strength = Some(Strength::Tertiary);
        }
    }
    if rec.ignore_punctuation {
        opts.alternate_handling = Some(AlternateHandling::Shifted);
    }
    let collator = Collator::try_new(prefs, opts)
        .or_else(|_| Collator::try_new(CollatorPreferences::default(), opts))
        .expect("root collator");
    match collator.compare(a, b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

fn install_intl_collator(vm: &mut JsVm, intl_obj: HeapHandle, state: &SharedIntl) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    state.borrow_mut().collator_proto = Some(proto);

    {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, "compare", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().collators.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.Collator"))?;
            let bound = vm.alloc_host_fn("compare", move |vm, _, args| {
                let a = vm.value_to_rust_string(arg(args, 0));
                let b = vm.value_to_rust_string(arg(args, 1));
                Ok(JsValue::from_number(f64::from(
                    compare_with_collator_record(&rec, &a, &b),
                )))
            });
            Ok(JsValue::from_object(bound))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "resolvedOptions", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().collators.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.Collator"))?;
            let obj = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let loc_s = str_val(vm, &rec.locale.to_string());
            let usage_s = str_val(vm, &rec.usage);
            let sens_s = str_val(vm, &rec.sensitivity);
            let cf_s = str_val(vm, &rec.case_first);
            let col_s = str_val(vm, "default");
            def_data(vm, obj, "locale", loc_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "usage", usage_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "sensitivity", sens_s, PropertyAttrs::DEFAULT);
            def_data(
                vm,
                obj,
                "ignorePunctuation",
                JsValue::from_bool(rec.ignore_punctuation),
                PropertyAttrs::DEFAULT,
            );
            def_data(vm, obj, "collation", col_s, PropertyAttrs::DEFAULT);
            def_data(
                vm,
                obj,
                "numeric",
                JsValue::from_bool(rec.numeric),
                PropertyAttrs::DEFAULT,
            );
            def_data(vm, obj, "caseFirst", cf_s, PropertyAttrs::DEFAULT);
            Ok(JsValue::from_object(obj))
        });
    }

    let st = Rc::clone(state);
    let ctor = vm.alloc_host_fn("Collator", move |vm, _, args| {
        let locale = resolve_first_locale(vm, &st, arg(args, 0))?;
        let opts = arg(args, 1);
        let usage = get_opt_string(vm, opts, "usage", Some(&["sort", "search"]))?
            .unwrap_or_else(|| "sort".into());
        let sensitivity = get_opt_string(
            vm,
            opts,
            "sensitivity",
            Some(&["base", "accent", "case", "variant"]),
        )?
        .unwrap_or_else(|| "variant".into());
        let ignore_punctuation = get_opt_bool(vm, opts, "ignorePunctuation")?.unwrap_or(false);
        let numeric = get_opt_bool(vm, opts, "numeric")?.unwrap_or_else(|| {
            matches!(unicode_ext_get(&locale, "kn").as_deref(), Some("" | "true"))
        });
        let case_first = get_opt_string(vm, opts, "caseFirst", Some(&["upper", "lower", "false"]))?
            .or_else(|| unicode_ext_get(&locale, "kf"))
            .unwrap_or_else(|| "false".into());
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        st.borrow_mut().collators.insert(
            h,
            CollatorRecord {
                locale,
                usage,
                sensitivity,
                ignore_punctuation,
                numeric,
                case_first,
            },
        );
        Ok(JsValue::from_object(h))
    });
    {
        let st2 = Rc::clone(state);
        def_host_method(vm, ctor, "supportedLocalesOf", move |vm, _, args| {
            supported_locales_of(vm, &st2, args)
        });
    }
    bind_ctor_proto(vm, ctor, proto);
    def_data(
        vm,
        intl_obj,
        "Collator",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

// ---------------------------------------------------------------------------
// 3. Intl.NumberFormat (ICU4X Decimal + Compact + Currency + Percent)
// ---------------------------------------------------------------------------

fn build_fixed_decimal(val: f64, rec: &NumberFormatRecord, scale_pow10: i16) -> Option<Decimal> {
    if !val.is_finite() {
        return None;
    }
    let mut dec = Decimal::try_from_f64(val, FloatPrecision::RoundTrip).ok()?;
    if scale_pow10 != 0 {
        dec.multiply_pow10(scale_pow10);
        dec.trim_start();
    }
    if let (Some(min_sd), Some(max_sd)) = (rec.min_sig_digits, rec.max_sig_digits) {
        let top = dec.nonzero_magnitude_start();
        let round_pos = top - (max_sd as i16) + 1;
        dec.round_with_mode(
            round_pos,
            SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfExpand),
        );
        dec.trim_end();
        let min_pos = dec.nonzero_magnitude_start() - (min_sd as i16) + 1;
        if min_pos < 0 {
            dec.pad_end(min_pos);
        }
    } else {
        let (min_f, max_f) = default_fraction_bounds(rec);
        dec.round_with_mode(
            -(max_f as i16),
            SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfExpand),
        );
        dec.trim_end();
        dec.pad_end(-(min_f as i16));
        if rec.min_int_digits > 1 {
            dec.pad_start(rec.min_int_digits as i16);
        }
    }
    let sign_disp = match rec.sign_display.as_str() {
        "always" => SignDisplay::Always,
        "exceptZero" => SignDisplay::ExceptZero,
        "never" => SignDisplay::Never,
        "negative" => SignDisplay::Negative,
        _ => SignDisplay::Auto,
    };
    dec.apply_sign_display(sign_disp);
    Some(dec)
}

fn default_fraction_bounds(rec: &NumberFormatRecord) -> (u16, u16) {
    let default_min = match rec.style.as_str() {
        "currency" => currency_minor_units(rec.currency.as_deref().unwrap_or("USD")),
        _ => 0,
    };
    let default_max = match rec.style.as_str() {
        "currency" => currency_minor_units(rec.currency.as_deref().unwrap_or("USD")),
        "percent" => 0,
        _ => 3,
    };
    let min_f = rec.min_frac_digits.unwrap_or(default_min);
    let max_f = rec
        .max_frac_digits
        .unwrap_or_else(|| default_max.max(min_f));
    (min_f, max_f)
}

fn currency_minor_units(code: &str) -> u16 {
    match code {
        "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF" | "UGX"
        | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => 0,
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
        _ => 2,
    }
}

fn grouping_from_str(s: &str) -> GroupingStrategy {
    match s {
        "false" => GroupingStrategy::Never,
        "always" => GroupingStrategy::Always,
        "min2" => GroupingStrategy::Min2,
        _ => GroupingStrategy::Auto,
    }
}

fn format_number_parts(val: f64, rec: &NumberFormatRecord) -> Vec<(&'static str, String)> {
    if val.is_nan() {
        return vec![("nan", "NaN".to_string())];
    }
    if val.is_infinite() {
        let mut out = Vec::new();
        if val.is_sign_negative() && rec.sign_display != "never" {
            out.push(("minusSign", "-".to_string()));
        } else if val.is_sign_positive()
            && matches!(rec.sign_display.as_str(), "always" | "exceptZero")
        {
            out.push(("plusSign", "+".to_string()));
        }
        out.push(("infinity", "∞".to_string()));
        return out;
    }

    let mut dec_opts = DecimalFormatterOptions::default();
    dec_opts.grouping_strategy = Some(grouping_from_str(&rec.use_grouping));

    match rec.style.as_str() {
        "currency" => {
            if let Some(ref cur_code) = rec.currency {
                if let (Ok(ctype), Some(dec)) = (
                    cur_code.to_ascii_lowercase().parse::<CurrencyType>(),
                    build_fixed_decimal(val, rec, 0),
                ) {
                    let copts = CurrencyFormatterOptions::default();
                    let fmt_res = match rec.currency_display.as_str() {
                        "narrowSymbol" => CurrencyFormatter::try_new_symbol_narrow(
                            (&rec.locale).into(),
                            ctype,
                            copts,
                        ),
                        "code" => {
                            CurrencyFormatter::try_new_code((&rec.locale).into(), ctype, copts)
                        }
                        "name" => CurrencyFormatter::try_new_name((&rec.locale).into(), ctype),
                        _ => CurrencyFormatter::try_new_symbol((&rec.locale).into(), ctype, copts),
                    };
                    if let Ok(cf) = fmt_res {
                        return tag_dimension_literal_parts(
                            collect_parts(&cf.format_fixed_decimal(&dec)),
                            "currency",
                        );
                    }
                }
            }
        }
        "percent" => {
            if let Some(dec) = build_fixed_decimal(val, rec, 2) {
                let df = DecimalFormatter::try_new((&rec.locale).into(), dec_opts)
                    .or_else(|_| DecimalFormatter::try_new(Default::default(), dec_opts))
                    .expect("decimal formatter");
                if let Ok(pf) = PercentFormatter::try_new_with_decimal_formatter(
                    (&rec.locale).into(),
                    df,
                    PercentFormatterOptions::default(),
                ) {
                    return tag_dimension_literal_parts(
                        collect_parts(&pf.format(&dec)),
                        "percentSign",
                    );
                }
            }
        }
        _ => {}
    }

    if rec.notation == "compact" {
        if let Some(dec) = build_fixed_decimal(val, rec, 0) {
            let compact = if rec.compact_display == "long" {
                CompactDecimalFormatter::try_new_long((&rec.locale).into(), dec_opts.into())
            } else {
                CompactDecimalFormatter::try_new_short((&rec.locale).into(), dec_opts.into())
            };
            if let Ok(cf) = compact {
                return collect_parts(&cf.format(&dec));
            }
        }
    }

    let dec = build_fixed_decimal(val, rec, 0).unwrap_or_else(|| Decimal::from(0i32));
    let df = DecimalFormatter::try_new((&rec.locale).into(), dec_opts)
        .or_else(|_| DecimalFormatter::try_new(Default::default(), dec_opts))
        .expect("decimal formatter");
    let mut parts = collect_parts(&df.format(&dec));
    if rec.style == "unit" {
        if let Some(ref u) = rec.unit {
            let suffix = format_unit_label(u, &rec.unit_display, val.abs() == 1.0);
            if rec.unit_display != "narrow" {
                parts.push(("literal", " ".to_string()));
            }
            parts.push(("unit", suffix));
        }
    }
    parts
}

fn tag_dimension_literal_parts(
    parts: Vec<(&'static str, String)>,
    dim_type: &'static str,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::with_capacity(parts.len());
    for (ty, s) in parts {
        if ty == "literal" && !s.trim().is_empty() {
            let trimmed = s.trim();
            let lead_end = s.find(trimmed).unwrap_or(0);
            let trail_start = lead_end + trimmed.len();
            if lead_end > 0 {
                out.push(("literal", s[..lead_end].to_string()));
            }
            out.push((dim_type, trimmed.to_string()));
            if trail_start < s.len() {
                out.push(("literal", s[trail_start..].to_string()));
            }
        } else {
            out.push((ty, s));
        }
    }
    out
}

fn format_unit_label(unit: &str, display: &str, singular: bool) -> String {
    match (unit, display) {
        ("kilometer", "short" | "narrow") => "km".into(),
        ("meter", "short" | "narrow") => "m".into(),
        ("centimeter", "short" | "narrow") => "cm".into(),
        ("millimeter", "short" | "narrow") => "mm".into(),
        ("kilogram", "short" | "narrow") => "kg".into(),
        ("gram", "short" | "narrow") => "g".into(),
        ("liter", "short" | "narrow") => "L".into(),
        ("celsius", "short" | "narrow") => "°C".into(),
        ("fahrenheit", "short" | "narrow") => "°F".into(),
        ("second", "short") => "sec".into(),
        ("second", "narrow") => "s".into(),
        ("minute", "short") => "min".into(),
        ("minute", "narrow") => "m".into(),
        ("hour", "short") => "hr".into(),
        ("hour", "narrow") => "h".into(),
        ("percent", _) => "%".into(),
        (u, _) if singular => u.to_string(),
        (u, _) => format!("{u}s"),
    }
}

fn parts_to_js_array(
    vm: &mut JsVm,
    parts: Vec<(&'static str, String)>,
    unit: Option<&str>,
) -> HeapHandle {
    let mut objs = Vec::with_capacity(parts.len());
    for (ty, val) in parts {
        let obj = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(vm.object_proto),
        ));
        let tv = str_val(vm, ty);
        let vv = str_val(vm, &val);
        def_data(vm, obj, "type", tv, PropertyAttrs::DEFAULT);
        def_data(vm, obj, "value", vv, PropertyAttrs::DEFAULT);
        if let Some(u) = unit {
            if ty != "literal" {
                let uv = str_val(vm, u);
                def_data(vm, obj, "unit", uv, PropertyAttrs::DEFAULT);
            }
        }
        objs.push(JsValue::from_object(obj));
    }
    alloc_array_of(vm, &objs)
}

fn install_intl_number_format(vm: &mut JsVm, intl_obj: HeapHandle, state: &SharedIntl) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    state.borrow_mut().number_format_proto = Some(proto);

    {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, "format", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().number_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.NumberFormat"))?;
            let bound = vm.alloc_host_fn("format", move |vm, _, args| {
                let n = vm.to_number(arg(args, 0))?;
                let text: String = format_number_parts(n, &rec)
                    .into_iter()
                    .map(|(_, s)| s)
                    .collect();
                Ok(str_val(vm, &text))
            });
            Ok(JsValue::from_object(bound))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "formatToParts", move |vm, this_val, args| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().number_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.NumberFormat"))?;
            let n = vm.to_number(arg(args, 0))?;
            let parts = format_number_parts(n, &rec);
            Ok(JsValue::from_object(parts_to_js_array(vm, parts, None)))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "resolvedOptions", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().number_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.NumberFormat"))?;
            let obj = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let (min_f, max_f) = default_fraction_bounds(&rec);
            let loc_s = str_val(vm, &rec.locale.to_string());
            let style_s = str_val(vm, &rec.style);
            let not_s = str_val(vm, &rec.notation);
            let sd_s = str_val(vm, &rec.sign_display);
            let ns_s = str_val(
                vm,
                &unicode_ext_get(&rec.locale, "nu").unwrap_or_else(|| "latn".into()),
            );
            def_data(vm, obj, "locale", loc_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "numberingSystem", ns_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "style", style_s, PropertyAttrs::DEFAULT);
            if let Some(ref c) = rec.currency {
                let cv = str_val(vm, c);
                let cd = str_val(vm, &rec.currency_display);
                def_data(vm, obj, "currency", cv, PropertyAttrs::DEFAULT);
                def_data(vm, obj, "currencyDisplay", cd, PropertyAttrs::DEFAULT);
            }
            if let Some(ref u) = rec.unit {
                let uv = str_val(vm, u);
                let ud = str_val(vm, &rec.unit_display);
                def_data(vm, obj, "unit", uv, PropertyAttrs::DEFAULT);
                def_data(vm, obj, "unitDisplay", ud, PropertyAttrs::DEFAULT);
            }
            def_data(
                vm,
                obj,
                "minimumIntegerDigits",
                JsValue::from_number(f64::from(rec.min_int_digits)),
                PropertyAttrs::DEFAULT,
            );
            def_data(
                vm,
                obj,
                "minimumFractionDigits",
                JsValue::from_number(f64::from(min_f)),
                PropertyAttrs::DEFAULT,
            );
            def_data(
                vm,
                obj,
                "maximumFractionDigits",
                JsValue::from_number(f64::from(max_f)),
                PropertyAttrs::DEFAULT,
            );
            let ug_val = if rec.use_grouping == "false" {
                JsValue::from_bool(false)
            } else {
                str_val(vm, &rec.use_grouping)
            };
            def_data(vm, obj, "useGrouping", ug_val, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "notation", not_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "signDisplay", sd_s, PropertyAttrs::DEFAULT);
            Ok(JsValue::from_object(obj))
        });
    }

    let st = Rc::clone(state);
    let ctor = vm.alloc_host_fn("NumberFormat", move |vm, _, args| {
        let locale = resolve_first_locale(vm, &st, arg(args, 0))?;
        let opts = arg(args, 1);
        let style = get_opt_string(
            vm,
            opts,
            "style",
            Some(&["decimal", "currency", "percent", "unit"]),
        )?
        .unwrap_or_else(|| "decimal".into());

        let currency = match get_opt_string(vm, opts, "currency", None)? {
            Some(c) => {
                if c.len() != 3 || !c.bytes().all(|b| b.is_ascii_alphabetic()) {
                    return Err(vm.range_error(format!("Invalid currency code: {c}")));
                }
                Some(c.to_ascii_uppercase())
            }
            None if style == "currency" => {
                return Err(vm.type_error("Currency code is required with currency style"));
            }
            None => None,
        };
        let currency_display = get_opt_string(
            vm,
            opts,
            "currencyDisplay",
            Some(&["symbol", "narrowSymbol", "code", "name"]),
        )?
        .unwrap_or_else(|| "symbol".into());

        let unit = get_opt_string(vm, opts, "unit", None)?;
        if style == "unit" && unit.is_none() {
            return Err(vm.type_error("Unit is required with unit style"));
        }
        let unit_display =
            get_opt_string(vm, opts, "unitDisplay", Some(&["short", "long", "narrow"]))?
                .unwrap_or_else(|| "short".into());

        let notation = get_opt_string(
            vm,
            opts,
            "notation",
            Some(&["standard", "scientific", "engineering", "compact"]),
        )?
        .unwrap_or_else(|| "standard".into());
        let compact_display = get_opt_string(vm, opts, "compactDisplay", Some(&["short", "long"]))?
            .unwrap_or_else(|| "short".into());
        let sign_display = get_opt_string(
            vm,
            opts,
            "signDisplay",
            Some(&["auto", "always", "exceptZero", "negative", "never"]),
        )?
        .unwrap_or_else(|| "auto".into());

        let ug_raw = get_opt_prop(vm, opts, "useGrouping")?;
        let use_grouping = if ug_raw.is_undefined() {
            if notation == "compact" {
                "min2".to_string()
            } else {
                "auto".to_string()
            }
        } else if let Some(b) = ug_raw.as_bool() {
            if b {
                "always".to_string()
            } else {
                "false".to_string()
            }
        } else {
            let s = vm.value_to_rust_string(ug_raw);
            match s.as_str() {
                "always" | "auto" | "min2" => s,
                "false" => "false".to_string(),
                _ => return Err(vm.range_error(format!("Invalid useGrouping value: {s}"))),
            }
        };

        let min_int_digits =
            get_opt_u16_range(vm, opts, "minimumIntegerDigits", 1, 21)?.unwrap_or(1);
        let min_frac_digits = get_opt_u16_range(vm, opts, "minimumFractionDigits", 0, 20)?;
        let max_frac_digits = get_opt_u16_range(vm, opts, "maximumFractionDigits", 0, 20)?;
        if let (Some(mn), Some(mx)) = (min_frac_digits, max_frac_digits) {
            if mn > mx {
                return Err(vm.range_error("minimumFractionDigits exceeds maximumFractionDigits"));
            }
        }
        let min_sig_digits = get_opt_u16_range(vm, opts, "minimumSignificantDigits", 1, 21)?;
        let max_sig_digits = get_opt_u16_range(vm, opts, "maximumSignificantDigits", 1, 21)?;
        if let (Some(mn), Some(mx)) = (min_sig_digits, max_sig_digits) {
            if mn > mx {
                return Err(
                    vm.range_error("minimumSignificantDigits exceeds maximumSignificantDigits")
                );
            }
        }

        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        st.borrow_mut().number_formats.insert(
            h,
            NumberFormatRecord {
                locale,
                style,
                currency,
                currency_display,
                unit,
                unit_display,
                notation,
                compact_display,
                use_grouping,
                sign_display,
                min_int_digits,
                min_frac_digits,
                max_frac_digits,
                min_sig_digits,
                max_sig_digits,
            },
        );
        Ok(JsValue::from_object(h))
    });
    {
        let st2 = Rc::clone(state);
        def_host_method(vm, ctor, "supportedLocalesOf", move |vm, _, args| {
            supported_locales_of(vm, &st2, args)
        });
    }
    bind_ctor_proto(vm, ctor, proto);
    def_data(
        vm,
        intl_obj,
        "NumberFormat",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

// ---------------------------------------------------------------------------
// 4. Intl.DateTimeFormat (ICU4X DateTimeFormatter + jiff IANA TZDB)
// ---------------------------------------------------------------------------

fn resolve_iana_timezone(tz_str: &str) -> Option<(String, JiffTimeZone)> {
    if tz_str.eq_ignore_ascii_case("utc") || tz_str.eq_ignore_ascii_case("etc/utc") {
        return Some(("UTC".to_string(), JiffTimeZone::UTC));
    }
    if let Ok(tz) = JiffTimeZone::get(tz_str) {
        let canonical = tz.iana_name().unwrap_or(tz_str).to_string();
        return Some((canonical, tz));
    }
    None
}

fn parse_length_style(s: &str) -> Length {
    match s {
        "full" | "long" => Length::Long,
        "medium" => Length::Medium,
        _ => Length::Short,
    }
}

fn build_datetime_fieldset(rec: &DateTimeFormatRecord) -> FieldSetBuilder {
    let mut b = FieldSetBuilder::new();
    let has_date_style = rec.date_style.is_some();
    let has_time_style = rec.time_style.is_some();

    if has_date_style || has_time_style {
        if let Some(ref ds) = rec.date_style {
            b.length = Some(parse_length_style(ds));
            b.date_fields = Some(if ds == "full" {
                DateFields::YMDE
            } else {
                DateFields::YMD
            });
        }
        if let Some(ref ts) = rec.time_style {
            if b.length.is_none() {
                b.length = Some(parse_length_style(ts));
            }
            b.time_precision = Some(match ts.as_str() {
                "short" => TimePrecision::Minute,
                _ => TimePrecision::Second,
            });
            if ts == "full" {
                b.zone_style = Some(ZoneStyle::SpecificLong);
            } else if ts == "long" {
                b.zone_style = Some(ZoneStyle::SpecificShort);
            }
        }
        return b;
    }

    let has_y = rec.year.is_some();
    let has_m = rec.month.is_some();
    let has_d = rec.day.is_some();
    let has_e = rec.weekday.is_some();
    let has_h = rec.hour.is_some();
    let has_min = rec.minute.is_some();
    let has_sec = rec.second.is_some();
    let has_any = has_y || has_m || has_d || has_e || has_h || has_min || has_sec;

    if !has_any {
        b.date_fields = Some(DateFields::YMD);
        b.length = Some(Length::Short);
    } else {
        b.date_fields = match (has_y, has_m, has_d, has_e) {
            (true, true, true, true) => Some(DateFields::YMDE),
            (true, true, true, false) => Some(DateFields::YMD),
            (false, true, true, true) => Some(DateFields::MDE),
            (false, true, true, false) => Some(DateFields::MD),
            (false, false, true, true) => Some(DateFields::DE),
            (false, false, true, false) => Some(DateFields::D),
            (true, true, false, _) => Some(DateFields::YM),
            (true, false, false, _) => Some(DateFields::Y),
            (false, true, false, _) => Some(DateFields::M),
            (false, false, false, true) => Some(DateFields::E),
            (true, false, true, true) => Some(DateFields::YMDE),
            (true, false, true, false) => Some(DateFields::YMD),
            (false, false, false, false) => None,
        };
        let len = if matches!(rec.month.as_deref(), Some("long"))
            || matches!(rec.weekday.as_deref(), Some("long"))
        {
            Length::Long
        } else if matches!(rec.month.as_deref(), Some("short"))
            || matches!(rec.weekday.as_deref(), Some("short"))
        {
            Length::Medium
        } else {
            Length::Short
        };
        b.length = Some(len);
        if matches!(rec.year.as_deref(), Some("numeric")) {
            b.year_style = Some(YearStyle::Full);
        }
        if matches!(rec.day.as_deref(), Some("2-digit"))
            || matches!(rec.month.as_deref(), Some("2-digit"))
            || matches!(rec.hour.as_deref(), Some("2-digit"))
        {
            b.alignment = Some(Alignment::Column);
        }
        if has_sec {
            b.time_precision = Some(TimePrecision::Second);
        } else if has_min {
            b.time_precision = Some(TimePrecision::Minute);
        } else if has_h {
            b.time_precision = Some(TimePrecision::Hour);
        }
    }

    if let Some(ref tzn) = rec.time_zone_name {
        b.zone_style = Some(match tzn.as_str() {
            "long" | "longGeneric" => ZoneStyle::SpecificLong,
            "shortOffset" => ZoneStyle::LocalizedOffsetShort,
            "longOffset" => ZoneStyle::LocalizedOffsetLong,
            _ => ZoneStyle::SpecificShort,
        });
    }
    b
}

fn format_datetime_parts(
    vm: &mut JsVm,
    ms: f64,
    rec: &DateTimeFormatRecord,
) -> Result<Vec<(&'static str, String)>, JsException> {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return Err(vm.range_error("Invalid time value"));
    }
    let (_, jiff_tz) = resolve_iana_timezone(&rec.time_zone)
        .unwrap_or_else(|| ("UTC".to_string(), JiffTimeZone::UTC));
    let ts = Timestamp::from_millisecond(ms.trunc() as i64)
        .map_err(|_| vm.range_error("Invalid time value"))?;
    let zoned = ts.to_zoned_datetime(jiff_tz);

    let iso_date = Date::try_new_iso(
        i32::from(zoned.year()),
        zoned.month() as u8,
        zoned.day() as u8,
    )
    .map_err(|_| vm.range_error("Invalid date value"))?;
    let icu_time = Time::try_new(
        zoned.hour() as u8,
        zoned.minute() as u8,
        zoned.second() as u8,
        zoned.subsec_nanosecond() as u32,
    )
    .map_err(|_| vm.range_error("Invalid time value"))?;

    let offset_secs = zoned.offset().seconds();
    let utc_offset = UtcOffset::try_from_seconds(offset_secs).unwrap_or(UtcOffset::zero());
    let icu_tz = IanaParser::new().parse(&rec.time_zone);
    let tz_info = icu_tz
        .with_offset(Some(utc_offset))
        .at_date_time(icu::time::DateTime {
            date: iso_date,
            time: icu_time,
        });
    let zdt = ZonedDateTime {
        date: iso_date,
        time: icu_time,
        zone: tz_info,
    };

    let mut effective_locale = rec.locale.clone();
    if let Some(ref hc) = rec.hour_cycle {
        let _ = unicode_ext_set(&mut effective_locale, "hc", hc);
    } else if let Some(h12) = rec.hour12 {
        let _ = unicode_ext_set(&mut effective_locale, "hc", if h12 { "h12" } else { "h23" });
    }

    let builder = build_datetime_fieldset(rec);
    let fieldset = builder.build_composite().unwrap_or_else(|_| {
        let mut fb = FieldSetBuilder::new();
        fb.date_fields = Some(DateFields::YMD);
        fb.length = Some(Length::Short);
        fb.build_composite().expect("fallback YMD fieldset")
    });
    let dtf = DateTimeFormatter::try_new((&effective_locale).into(), fieldset)
        .or_else(|_| DateTimeFormatter::try_new(Default::default(), fieldset))
        .map_err(|e| vm.range_error(format!("DateTimeFormatter error: {e}")))?;
    Ok(collect_parts(&dtf.format(&zdt)))
}

fn extract_date_ms(vm: &mut JsVm, val: JsValue) -> Result<f64, JsException> {
    if val.is_undefined() {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
        return Ok(secs.trunc());
    }
    if let Some(h) = val.as_object() {
        if let Some(ObjectKind::Date(ms)) = vm.heap.get_object(h).map(|o| &o.kind) {
            return Ok(*ms);
        }
    }
    vm.to_number(val)
}

fn install_intl_date_time_format(vm: &mut JsVm, intl_obj: HeapHandle, state: &SharedIntl) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    state.borrow_mut().date_time_format_proto = Some(proto);

    {
        let st = Rc::clone(state);
        def_host_getter(vm, proto, "format", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().date_time_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.DateTimeFormat"))?;
            let bound = vm.alloc_host_fn("format", move |vm, _, args| {
                let ms = extract_date_ms(vm, arg(args, 0))?;
                let parts = format_datetime_parts(vm, ms, &rec)?;
                let text: String = parts.into_iter().map(|(_, s)| s).collect();
                Ok(str_val(vm, &text))
            });
            Ok(JsValue::from_object(bound))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "formatToParts", move |vm, this_val, args| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().date_time_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.DateTimeFormat"))?;
            let ms = extract_date_ms(vm, arg(args, 0))?;
            let parts = format_datetime_parts(vm, ms, &rec)?;
            Ok(JsValue::from_object(parts_to_js_array(vm, parts, None)))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "resolvedOptions", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().date_time_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.DateTimeFormat"))?;
            let obj = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let loc_s = str_val(vm, &rec.locale.to_string());
            let cal_s = str_val(
                vm,
                &unicode_ext_get(&rec.locale, "ca").unwrap_or_else(|| "gregory".into()),
            );
            let nu_s = str_val(
                vm,
                &unicode_ext_get(&rec.locale, "nu").unwrap_or_else(|| "latn".into()),
            );
            let tz_s = str_val(vm, &rec.time_zone);
            def_data(vm, obj, "locale", loc_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "calendar", cal_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "numberingSystem", nu_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "timeZone", tz_s, PropertyAttrs::DEFAULT);
            for (name, opt) in [
                ("dateStyle", &rec.date_style),
                ("timeStyle", &rec.time_style),
                ("year", &rec.year),
                ("month", &rec.month),
                ("day", &rec.day),
                ("weekday", &rec.weekday),
                ("hour", &rec.hour),
                ("minute", &rec.minute),
                ("second", &rec.second),
                ("timeZoneName", &rec.time_zone_name),
                ("hourCycle", &rec.hour_cycle),
            ] {
                if let Some(ref v) = opt {
                    let sv = str_val(vm, v);
                    def_data(vm, obj, name, sv, PropertyAttrs::DEFAULT);
                }
            }
            if let Some(h12) = rec.hour12 {
                def_data(
                    vm,
                    obj,
                    "hour12",
                    JsValue::from_bool(h12),
                    PropertyAttrs::DEFAULT,
                );
            }
            Ok(JsValue::from_object(obj))
        });
    }

    let st = Rc::clone(state);
    let ctor = vm.alloc_host_fn("DateTimeFormat", move |vm, _, args| {
        let locale = resolve_first_locale(vm, &st, arg(args, 0))?;
        let opts = arg(args, 1);

        let time_zone = match get_opt_string(vm, opts, "timeZone", None)? {
            Some(tz_raw) => {
                let (canon, _) = resolve_iana_timezone(&tz_raw)
                    .ok_or_else(|| vm.range_error(format!("Invalid time zone specified: {tz_raw}")))?;
                canon
            }
            None => "UTC".to_string(),
        };

        let date_style = get_opt_string(
            vm,
            opts,
            "dateStyle",
            Some(&["full", "long", "medium", "short"]),
        )?;
        let time_style = get_opt_string(
            vm,
            opts,
            "timeStyle",
            Some(&["full", "long", "medium", "short"]),
        )?;
        let year = get_opt_string(vm, opts, "year", Some(&["numeric", "2-digit"]))?;
        let month = get_opt_string(
            vm,
            opts,
            "month",
            Some(&["numeric", "2-digit", "long", "short", "narrow"]),
        )?;
        let day = get_opt_string(vm, opts, "day", Some(&["numeric", "2-digit"]))?;
        let weekday = get_opt_string(vm, opts, "weekday", Some(&["long", "short", "narrow"]))?;
        let hour = get_opt_string(vm, opts, "hour", Some(&["numeric", "2-digit"]))?;
        let minute = get_opt_string(vm, opts, "minute", Some(&["numeric", "2-digit"]))?;
        let second = get_opt_string(vm, opts, "second", Some(&["numeric", "2-digit"]))?;
        let time_zone_name = get_opt_string(
            vm,
            opts,
            "timeZoneName",
            Some(&[
                "short",
                "long",
                "shortOffset",
                "longOffset",
                "shortGeneric",
                "longGeneric",
            ]),
        )?;
        let hour12 = get_opt_bool(vm, opts, "hour12")?;
        let hour_cycle =
            get_opt_string(vm, opts, "hourCycle", Some(&["h11", "h12", "h23", "h24"]))?;

        if (date_style.is_some() || time_style.is_some())
            && (year.is_some()
                || month.is_some()
                || day.is_some()
                || weekday.is_some()
                || hour.is_some()
                || minute.is_some()
                || second.is_some()
                || time_zone_name.is_some())
        {
            return Err(vm.type_error(
                "Cannot specify dateStyle/timeStyle alongside individual date/time component options",
            ));
        }

        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        st.borrow_mut().date_time_formats.insert(
            h,
            DateTimeFormatRecord {
                locale,
                time_zone,
                date_style,
                time_style,
                year,
                month,
                day,
                weekday,
                hour,
                minute,
                second,
                time_zone_name,
                hour12,
                hour_cycle,
            },
        );
        Ok(JsValue::from_object(h))
    });
    {
        let st2 = Rc::clone(state);
        def_host_method(vm, ctor, "supportedLocalesOf", move |vm, _, args| {
            supported_locales_of(vm, &st2, args)
        });
    }
    bind_ctor_proto(vm, ctor, proto);
    def_data(
        vm,
        intl_obj,
        "DateTimeFormat",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

// ---------------------------------------------------------------------------
// 5. Intl.PluralRules (ICU4X PluralRules)
// ---------------------------------------------------------------------------

fn plural_category_str(cat: PluralCategory) -> &'static str {
    match cat {
        PluralCategory::Zero => "zero",
        PluralCategory::One => "one",
        PluralCategory::Two => "two",
        PluralCategory::Few => "few",
        PluralCategory::Many => "many",
        PluralCategory::Other => "other",
    }
}

fn install_intl_plural_rules(vm: &mut JsVm, intl_obj: HeapHandle, state: &SharedIntl) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    state.borrow_mut().plural_rules_proto = Some(proto);

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "select", move |vm, this_val, args| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().plural_rules.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.PluralRules"))?;
            let n = vm.to_number(arg(args, 0))?;
            if !n.is_finite() {
                return Ok(str_val(vm, "other"));
            }
            let mut dec = Decimal::try_from_f64(n, FloatPrecision::RoundTrip)
                .unwrap_or_else(|_| Decimal::from(0i32));
            dec.round_with_mode(
                -(rec.max_frac_digits as i16),
                SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfExpand),
            );
            dec.trim_end();
            dec.pad_end(-(rec.min_frac_digits as i16));
            let pr_opts = PluralRulesOptions::default().with_type(if rec.rule_type == "ordinal" {
                PluralRuleType::Ordinal
            } else {
                PluralRuleType::Cardinal
            });
            let pr = PluralRules::try_new((&rec.locale).into(), pr_opts)
                .or_else(|_| PluralRules::try_new(Default::default(), pr_opts))
                .expect("root plural rules");
            let cat = pr.category_for(PluralOperands::from(&dec));
            Ok(str_val(vm, plural_category_str(cat)))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "resolvedOptions", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().plural_rules.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.PluralRules"))?;
            let obj = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let loc_s = str_val(vm, &rec.locale.to_string());
            let ty_s = str_val(vm, &rec.rule_type);
            let pr_opts = PluralRulesOptions::default().with_type(if rec.rule_type == "ordinal" {
                PluralRuleType::Ordinal
            } else {
                PluralRuleType::Cardinal
            });
            let pr = PluralRules::try_new((&rec.locale).into(), pr_opts)
                .or_else(|_| PluralRules::try_new(Default::default(), pr_opts))
                .expect("root plural rules");
            let cats: Vec<JsValue> = pr
                .categories()
                .map(|c| str_val(vm, plural_category_str(c)))
                .collect();
            let cats_arr = alloc_array_of(vm, &cats);
            def_data(vm, obj, "locale", loc_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "type", ty_s, PropertyAttrs::DEFAULT);
            def_data(
                vm,
                obj,
                "minimumIntegerDigits",
                JsValue::from_number(f64::from(rec.min_int_digits)),
                PropertyAttrs::DEFAULT,
            );
            def_data(
                vm,
                obj,
                "minimumFractionDigits",
                JsValue::from_number(f64::from(rec.min_frac_digits)),
                PropertyAttrs::DEFAULT,
            );
            def_data(
                vm,
                obj,
                "maximumFractionDigits",
                JsValue::from_number(f64::from(rec.max_frac_digits)),
                PropertyAttrs::DEFAULT,
            );
            def_data(
                vm,
                obj,
                "pluralCategories",
                JsValue::from_object(cats_arr),
                PropertyAttrs::DEFAULT,
            );
            Ok(JsValue::from_object(obj))
        });
    }

    let st = Rc::clone(state);
    let ctor = vm.alloc_host_fn("PluralRules", move |vm, _, args| {
        let locale = resolve_first_locale(vm, &st, arg(args, 0))?;
        let opts = arg(args, 1);
        let rule_type = get_opt_string(vm, opts, "type", Some(&["cardinal", "ordinal"]))?
            .unwrap_or_else(|| "cardinal".into());
        let min_int_digits =
            get_opt_u16_range(vm, opts, "minimumIntegerDigits", 1, 21)?.unwrap_or(1);
        let min_frac_digits =
            get_opt_u16_range(vm, opts, "minimumFractionDigits", 0, 20)?.unwrap_or(0);
        let max_frac_digits = get_opt_u16_range(vm, opts, "maximumFractionDigits", 0, 20)?
            .unwrap_or_else(|| 3u16.max(min_frac_digits));
        if min_frac_digits > max_frac_digits {
            return Err(vm.range_error("minimumFractionDigits exceeds maximumFractionDigits"));
        }
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        st.borrow_mut().plural_rules.insert(
            h,
            PluralRulesRecord {
                locale,
                rule_type,
                min_int_digits,
                min_frac_digits,
                max_frac_digits,
            },
        );
        Ok(JsValue::from_object(h))
    });
    {
        let st2 = Rc::clone(state);
        def_host_method(vm, ctor, "supportedLocalesOf", move |vm, _, args| {
            supported_locales_of(vm, &st2, args)
        });
    }
    bind_ctor_proto(vm, ctor, proto);
    def_data(
        vm,
        intl_obj,
        "PluralRules",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

// ---------------------------------------------------------------------------
// 6. Intl.ListFormat (ICU4X ListFormatter)
// ---------------------------------------------------------------------------

fn string_list_from_js(vm: &mut JsVm, val: JsValue) -> Result<Vec<String>, JsException> {
    if val.is_undefined() {
        return Ok(Vec::new());
    }
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Iterable expected for Intl.ListFormat"));
    };
    let len = get_array_len(vm, h)?;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let elem = get_array_elem(vm, h, i)?;
        if !elem.is_string() {
            return Err(vm.type_error("Iterable element for Intl.ListFormat must be a string"));
        }
        out.push(vm.value_to_rust_string(elem));
    }
    Ok(out)
}

fn build_list_formatter(rec: &ListFormatRecord) -> ListFormatter {
    let length = match rec.style.as_str() {
        "short" => ListLength::Short,
        "narrow" => ListLength::Narrow,
        _ => ListLength::Wide,
    };
    let opts = ListFormatterOptions::default().with_length(length);
    match rec.list_type.as_str() {
        "disjunction" => ListFormatter::try_new_or((&rec.locale).into(), opts)
            .or_else(|_| ListFormatter::try_new_or(Default::default(), opts))
            .expect("list or"),
        "unit" => ListFormatter::try_new_unit((&rec.locale).into(), opts)
            .or_else(|_| ListFormatter::try_new_unit(Default::default(), opts))
            .expect("list unit"),
        _ => ListFormatter::try_new_and((&rec.locale).into(), opts)
            .or_else(|_| ListFormatter::try_new_and(Default::default(), opts))
            .expect("list and"),
    }
}

fn install_intl_list_format(vm: &mut JsVm, intl_obj: HeapHandle, state: &SharedIntl) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    state.borrow_mut().list_format_proto = Some(proto);

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "format", move |vm, this_val, args| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().list_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.ListFormat"))?;
            let items = string_list_from_js(vm, arg(args, 0))?;
            let lf = build_list_formatter(&rec);
            let formatted = lf.format_to_string(items.iter().map(String::as_str));
            Ok(str_val(vm, &formatted))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "formatToParts", move |vm, this_val, args| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().list_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.ListFormat"))?;
            let items = string_list_from_js(vm, arg(args, 0))?;
            let lf = build_list_formatter(&rec);
            let parts = collect_parts(&lf.format(items.iter().map(String::as_str)));
            Ok(JsValue::from_object(parts_to_js_array(vm, parts, None)))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "resolvedOptions", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().list_formats.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.ListFormat"))?;
            let obj = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let loc_s = str_val(vm, &rec.locale.to_string());
            let ty_s = str_val(vm, &rec.list_type);
            let st_s = str_val(vm, &rec.style);
            def_data(vm, obj, "locale", loc_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "type", ty_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "style", st_s, PropertyAttrs::DEFAULT);
            Ok(JsValue::from_object(obj))
        });
    }

    let st = Rc::clone(state);
    let ctor = vm.alloc_host_fn("ListFormat", move |vm, _, args| {
        let locale = resolve_first_locale(vm, &st, arg(args, 0))?;
        let opts = arg(args, 1);
        let list_type = get_opt_string(
            vm,
            opts,
            "type",
            Some(&["conjunction", "disjunction", "unit"]),
        )?
        .unwrap_or_else(|| "conjunction".into());
        let style = get_opt_string(vm, opts, "style", Some(&["long", "short", "narrow"]))?
            .unwrap_or_else(|| "long".into());
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        st.borrow_mut().list_formats.insert(
            h,
            ListFormatRecord {
                locale,
                list_type,
                style,
            },
        );
        Ok(JsValue::from_object(h))
    });
    {
        let st2 = Rc::clone(state);
        def_host_method(vm, ctor, "supportedLocalesOf", move |vm, _, args| {
            supported_locales_of(vm, &st2, args)
        });
    }
    bind_ctor_proto(vm, ctor, proto);
    def_data(
        vm,
        intl_obj,
        "ListFormat",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

// ---------------------------------------------------------------------------
// 7. Intl.Segmenter (ICU4X Grapheme / Word / Sentence Segmenters)
// ---------------------------------------------------------------------------

fn compute_segments(rec: &SegmenterRecord, input: &str) -> Vec<(usize, String, Option<bool>)> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut utf16_offset = 0usize;
    match rec.granularity.as_str() {
        "word" => {
            let mut opts = WordBreakOptions::default();
            opts.content_locale = Some(&rec.locale.id);
            let seg = WordSegmenter::try_new_auto(opts).unwrap_or_else(|_| {
                WordSegmenter::try_new_auto(WordBreakOptions::default()).expect("word seg")
            });
            let mut iter = seg.as_borrowed().segment_str(input);
            let mut prev = iter.next().unwrap_or(0);
            while let Some(next_idx) = iter.next() {
                let word_like = iter.is_word_like();
                let slice = &input[prev..next_idx];
                out.push((utf16_offset, slice.to_string(), Some(word_like)));
                utf16_offset += slice.encode_utf16().count();
                prev = next_idx;
            }
        }
        "sentence" => {
            let mut opts = SentenceBreakOptions::default();
            opts.content_locale = Some(&rec.locale.id);
            let seg = SentenceSegmenter::try_new(opts).unwrap_or_else(|_| {
                SentenceSegmenter::try_new(SentenceBreakOptions::default()).expect("sentence seg")
            });
            let mut iter = seg.as_borrowed().segment_str(input);
            let mut prev = iter.next().unwrap_or(0);
            for next_idx in iter {
                let slice = &input[prev..next_idx];
                out.push((utf16_offset, slice.to_string(), None));
                utf16_offset += slice.encode_utf16().count();
                prev = next_idx;
            }
        }
        _ => {
            let seg = GraphemeClusterSegmenter::new();
            let mut iter = seg.segment_str(input);
            let mut prev = iter.next().unwrap_or(0);
            for next_idx in iter {
                let slice = &input[prev..next_idx];
                out.push((utf16_offset, slice.to_string(), None));
                utf16_offset += slice.encode_utf16().count();
                prev = next_idx;
            }
        }
    }
    out
}

fn alloc_segment_data_obj(
    vm: &mut JsVm,
    index: usize,
    segment: &str,
    input: &str,
    is_word_like: Option<bool>,
) -> HeapHandle {
    let obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let seg_v = str_val(vm, segment);
    let inp_v = str_val(vm, input);
    def_data(vm, obj, "segment", seg_v, PropertyAttrs::DEFAULT);
    def_data(
        vm,
        obj,
        "index",
        JsValue::from_number(index as f64),
        PropertyAttrs::DEFAULT,
    );
    def_data(vm, obj, "input", inp_v, PropertyAttrs::DEFAULT);
    if let Some(wl) = is_word_like {
        def_data(
            vm,
            obj,
            "isWordLike",
            JsValue::from_bool(wl),
            PropertyAttrs::DEFAULT,
        );
    }
    obj
}

fn install_intl_segmenter(vm: &mut JsVm, intl_obj: HeapHandle, state: &SharedIntl) {
    let seg_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let segments_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let iter_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    {
        let mut st = state.borrow_mut();
        st.segmenter_proto = Some(seg_proto);
        st.segments_proto = Some(segments_proto);
        st.segment_iter_proto = Some(iter_proto);
    }

    // SegmentIterator.prototype.next()
    {
        let st = Rc::clone(state);
        def_host_method(vm, iter_proto, "next", move |vm, this_val, _| {
            let item = this_val.as_object().and_then(|ih| {
                let mut s = st.borrow_mut();
                let (seg_h, idx) = {
                    let it = s.segment_iters.get_mut(&ih)?;
                    let cur = it.cursor;
                    it.cursor += 1;
                    (it.segments_h, cur)
                };
                let segs = s.segments.get(&seg_h)?;
                segs.items
                    .get(idx)
                    .map(|(u16_i, text, wl)| (*u16_i, text.clone(), segs.input.clone(), *wl))
            });
            let res = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            if let Some((u16_i, text, input, wl)) = item {
                let data_obj = alloc_segment_data_obj(vm, u16_i, &text, &input, wl);
                def_data(
                    vm,
                    res,
                    "value",
                    JsValue::from_object(data_obj),
                    PropertyAttrs::DEFAULT,
                );
                def_data(
                    vm,
                    res,
                    "done",
                    JsValue::from_bool(false),
                    PropertyAttrs::DEFAULT,
                );
            } else {
                def_data(vm, res, "value", JsValue::UNDEFINED, PropertyAttrs::DEFAULT);
                def_data(
                    vm,
                    res,
                    "done",
                    JsValue::from_bool(true),
                    PropertyAttrs::DEFAULT,
                );
            }
            Ok(JsValue::from_object(res))
        });
    }
    let sym_iter = vm.key_symbol_iterator;
    let self_iter_fn = vm.alloc_host_fn("[Symbol.iterator]", |_, this_val, _| Ok(this_val));
    vm.heap.object_define_data(
        iter_proto,
        sym_iter,
        JsValue::from_object(self_iter_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    // Segments.prototype[Symbol.iterator]()
    {
        let st = Rc::clone(state);
        let it_fn = vm.alloc_host_fn("[Symbol.iterator]", move |vm, this_val, _| {
            let seg_h = this_val
                .as_object()
                .filter(|h| st.borrow().segments.contains_key(h))
                .ok_or_else(|| vm.type_error("Incompatible receiver for Segments"))?;
            let iter_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(iter_proto)));
            st.borrow_mut().segment_iters.insert(
                iter_h,
                SegmentIterState {
                    segments_h: seg_h,
                    cursor: 0,
                },
            );
            Ok(JsValue::from_object(iter_h))
        });
        vm.heap.object_define_data(
            segments_proto,
            sym_iter,
            JsValue::from_object(it_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    // Segments.prototype.containing(codeUnitIndex)
    {
        let st = Rc::clone(state);
        def_host_method(
            vm,
            segments_proto,
            "containing",
            move |vm, this_val, args| {
                let rec = this_val
                    .as_object()
                    .and_then(|h| st.borrow().segments.get(&h).cloned())
                    .ok_or_else(|| {
                        vm.type_error("Incompatible receiver for Segments.containing")
                    })?;
                let n = vm.to_number(arg(args, 0))?;
                if n.is_nan() || n < 0.0 {
                    return Ok(JsValue::UNDEFINED);
                }
                let target = n.trunc() as usize;
                let total_u16 = rec.input.encode_utf16().count();
                if target >= total_u16 {
                    return Ok(JsValue::UNDEFINED);
                }
                for (u16_i, text, wl) in &rec.items {
                    let len = text.encode_utf16().count();
                    if target >= *u16_i && target < *u16_i + len {
                        let obj = alloc_segment_data_obj(vm, *u16_i, text, &rec.input, *wl);
                        return Ok(JsValue::from_object(obj));
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // Segmenter.prototype.segment(input)
    {
        let st = Rc::clone(state);
        def_host_method(vm, seg_proto, "segment", move |vm, this_val, args| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().segmenters.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.Segmenter"))?;
            let input = vm.value_to_rust_string(arg(args, 0));
            let items = compute_segments(&rec, &input);
            let segs_h = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(segments_proto),
            ));
            st.borrow_mut()
                .segments
                .insert(segs_h, SegmentsRecord { input, items });
            Ok(JsValue::from_object(segs_h))
        });
    }

    // Segmenter.prototype.resolvedOptions()
    {
        let st = Rc::clone(state);
        def_host_method(vm, seg_proto, "resolvedOptions", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().segmenters.get(&h).cloned())
                .ok_or_else(|| vm.type_error("Incompatible receiver for Intl.Segmenter"))?;
            let obj = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let loc_s = str_val(vm, &rec.locale.to_string());
            let gran_s = str_val(vm, &rec.granularity);
            def_data(vm, obj, "locale", loc_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "granularity", gran_s, PropertyAttrs::DEFAULT);
            Ok(JsValue::from_object(obj))
        });
    }

    let st = Rc::clone(state);
    let ctor = vm.alloc_host_fn("Segmenter", move |vm, _, args| {
        let locale = resolve_first_locale(vm, &st, arg(args, 0))?;
        let opts = arg(args, 1);
        let granularity = get_opt_string(
            vm,
            opts,
            "granularity",
            Some(&["grapheme", "word", "sentence"]),
        )?
        .unwrap_or_else(|| "grapheme".into());
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(seg_proto)));
        st.borrow_mut().segmenters.insert(
            h,
            SegmenterRecord {
                locale,
                granularity,
            },
        );
        Ok(JsValue::from_object(h))
    });
    {
        let st2 = Rc::clone(state);
        def_host_method(vm, ctor, "supportedLocalesOf", move |vm, _, args| {
            supported_locales_of(vm, &st2, args)
        });
    }
    bind_ctor_proto(vm, ctor, seg_proto);
    def_data(
        vm,
        intl_obj,
        "Segmenter",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

// ---------------------------------------------------------------------------
// 8. Intl.RelativeTimeFormat (ICU4X RelativeTimeFormatter)
// ---------------------------------------------------------------------------

fn canonical_rel_unit(raw: &str) -> Option<&'static str> {
    match raw {
        "second" | "seconds" => Some("second"),
        "minute" | "minutes" => Some("minute"),
        "hour" | "hours" => Some("hour"),
        "day" | "days" => Some("day"),
        "week" | "weeks" => Some("week"),
        "month" | "months" => Some("month"),
        "quarter" | "quarters" => Some("quarter"),
        "year" | "years" => Some("year"),
        _ => None,
    }
}

fn build_relative_time_formatter(
    rec: &RelativeTimeFormatRecord,
    unit: &str,
) -> Option<RelativeTimeFormatter> {
    let mut opts = RelativeTimeFormatterOptions::default();
    opts.numeric = if rec.numeric == "auto" {
        Some(RelNumeric::Auto)
    } else {
        Some(RelNumeric::Always)
    };
    let prefs = (&rec.locale).into();
    let res = match (rec.style.as_str(), unit) {
        ("short", "second") => RelativeTimeFormatter::try_new_short_second(prefs, opts),
        ("short", "minute") => RelativeTimeFormatter::try_new_short_minute(prefs, opts),
        ("short", "hour") => RelativeTimeFormatter::try_new_short_hour(prefs, opts),
        ("short", "day") => RelativeTimeFormatter::try_new_short_day(prefs, opts),
        ("short", "week") => RelativeTimeFormatter::try_new_short_week(prefs, opts),
        ("short", "month") => RelativeTimeFormatter::try_new_short_month(prefs, opts),
        ("short", "quarter") => RelativeTimeFormatter::try_new_short_quarter(prefs, opts),
        ("short", "year") => RelativeTimeFormatter::try_new_short_year(prefs, opts),
        ("narrow", "second") => RelativeTimeFormatter::try_new_narrow_second(prefs, opts),
        ("narrow", "minute") => RelativeTimeFormatter::try_new_narrow_minute(prefs, opts),
        ("narrow", "hour") => RelativeTimeFormatter::try_new_narrow_hour(prefs, opts),
        ("narrow", "day") => RelativeTimeFormatter::try_new_narrow_day(prefs, opts),
        ("narrow", "week") => RelativeTimeFormatter::try_new_narrow_week(prefs, opts),
        ("narrow", "month") => RelativeTimeFormatter::try_new_narrow_month(prefs, opts),
        ("narrow", "quarter") => RelativeTimeFormatter::try_new_narrow_quarter(prefs, opts),
        ("narrow", "year") => RelativeTimeFormatter::try_new_narrow_year(prefs, opts),
        (_, "second") => RelativeTimeFormatter::try_new_long_second(prefs, opts),
        (_, "minute") => RelativeTimeFormatter::try_new_long_minute(prefs, opts),
        (_, "hour") => RelativeTimeFormatter::try_new_long_hour(prefs, opts),
        (_, "day") => RelativeTimeFormatter::try_new_long_day(prefs, opts),
        (_, "week") => RelativeTimeFormatter::try_new_long_week(prefs, opts),
        (_, "month") => RelativeTimeFormatter::try_new_long_month(prefs, opts),
        (_, "quarter") => RelativeTimeFormatter::try_new_long_quarter(prefs, opts),
        (_, "year") => RelativeTimeFormatter::try_new_long_year(prefs, opts),
        _ => return None,
    };
    res.ok()
}

fn install_intl_relative_time_format(vm: &mut JsVm, intl_obj: HeapHandle, state: &SharedIntl) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    state.borrow_mut().relative_time_format_proto = Some(proto);

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "format", move |vm, this_val, args| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().relative_time_formats.get(&h).cloned())
                .ok_or_else(|| {
                    vm.type_error("Incompatible receiver for Intl.RelativeTimeFormat")
                })?;
            let val = vm.to_number(arg(args, 0))?;
            if !val.is_finite() {
                return Err(vm.range_error("Value must be finite for Intl.RelativeTimeFormat"));
            }
            let unit_raw = vm.value_to_rust_string(arg(args, 1));
            let unit = canonical_rel_unit(&unit_raw)
                .ok_or_else(|| vm.range_error(format!("Invalid unit argument: {unit_raw}")))?;
            let dec = Decimal::try_from_f64(val, FloatPrecision::RoundTrip)
                .map_err(|_| vm.range_error("Invalid numeric value"))?;
            let rtf = build_relative_time_formatter(&rec, unit)
                .ok_or_else(|| vm.range_error("Unable to construct RelativeTimeFormatter"))?;
            let formatted = rtf.format(dec).write_to_string().into_owned();
            Ok(str_val(vm, &formatted))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "formatToParts", move |vm, this_val, args| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().relative_time_formats.get(&h).cloned())
                .ok_or_else(|| {
                    vm.type_error("Incompatible receiver for Intl.RelativeTimeFormat")
                })?;
            let val = vm.to_number(arg(args, 0))?;
            if !val.is_finite() {
                return Err(vm.range_error("Value must be finite for Intl.RelativeTimeFormat"));
            }
            let unit_raw = vm.value_to_rust_string(arg(args, 1));
            let unit = canonical_rel_unit(&unit_raw)
                .ok_or_else(|| vm.range_error(format!("Invalid unit argument: {unit_raw}")))?;
            let dec = Decimal::try_from_f64(val, FloatPrecision::RoundTrip)
                .map_err(|_| vm.range_error("Invalid numeric value"))?;
            let rtf = build_relative_time_formatter(&rec, unit)
                .ok_or_else(|| vm.range_error("Unable to construct RelativeTimeFormatter"))?;
            let parts = collect_parts(&rtf.format(dec));
            Ok(JsValue::from_object(parts_to_js_array(
                vm,
                parts,
                Some(unit),
            )))
        });
    }

    {
        let st = Rc::clone(state);
        def_host_method(vm, proto, "resolvedOptions", move |vm, this_val, _| {
            let rec = this_val
                .as_object()
                .and_then(|h| st.borrow().relative_time_formats.get(&h).cloned())
                .ok_or_else(|| {
                    vm.type_error("Incompatible receiver for Intl.RelativeTimeFormat")
                })?;
            let obj = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let loc_s = str_val(vm, &rec.locale.to_string());
            let st_s = str_val(vm, &rec.style);
            let num_s = str_val(vm, &rec.numeric);
            let nu_s = str_val(
                vm,
                &unicode_ext_get(&rec.locale, "nu").unwrap_or_else(|| "latn".into()),
            );
            def_data(vm, obj, "locale", loc_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "style", st_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "numeric", num_s, PropertyAttrs::DEFAULT);
            def_data(vm, obj, "numberingSystem", nu_s, PropertyAttrs::DEFAULT);
            Ok(JsValue::from_object(obj))
        });
    }

    let st = Rc::clone(state);
    let ctor = vm.alloc_host_fn("RelativeTimeFormat", move |vm, _, args| {
        let locale = resolve_first_locale(vm, &st, arg(args, 0))?;
        let opts = arg(args, 1);
        let numeric = get_opt_string(vm, opts, "numeric", Some(&["always", "auto"]))?
            .unwrap_or_else(|| "always".into());
        let style = get_opt_string(vm, opts, "style", Some(&["long", "short", "narrow"]))?
            .unwrap_or_else(|| "long".into());
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        st.borrow_mut().relative_time_formats.insert(
            h,
            RelativeTimeFormatRecord {
                locale,
                numeric,
                style,
            },
        );
        Ok(JsValue::from_object(h))
    });
    {
        let st2 = Rc::clone(state);
        def_host_method(vm, ctor, "supportedLocalesOf", move |vm, _, args| {
            supported_locales_of(vm, &st2, args)
        });
    }
    bind_ctor_proto(vm, ctor, proto);
    def_data(
        vm,
        intl_obj,
        "RelativeTimeFormat",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}
