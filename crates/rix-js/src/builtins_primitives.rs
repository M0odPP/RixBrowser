//! ECMAScript 2024 core primitive, numeric, text, temporal, regex, JSON,
//! and extended Array/Object standard library built-ins.

use crate::bigint::JsBigInt;
use crate::builtins_regexp::{
    get_substitution, install_regexp_and_annexb_globals, is_es_whitespace_or_lt, is_regexp,
};
use crate::heap::{JsObject, ObjectKind};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue, JsValueKind};
use crate::vm::{JsException, JsVm};
use crate::vm_builtins::{define_global, define_method, define_method_len};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn install_primitive_builtins(vm: &mut JsVm) {
    install_global_functions(vm);
    install_string(vm);
    install_number(vm);
    install_boolean(vm);
    install_bigint(vm);
    install_math(vm);
    install_symbol(vm);
    install_date(vm);
    install_regexp_and_annexb_globals(vm);
    install_json(vm);
    install_extended_array_and_object(vm);
}

fn install_global_functions(vm: &mut JsVm) {
    let globals: &[(&str, u32, crate::vm::NativeFnPtr)] = &[
        ("parseInt", 2, global_parse_int),
        ("parseFloat", 1, global_parse_float),
        ("isNaN", 1, global_is_nan),
        ("isFinite", 1, global_is_finite),
        ("encodeURI", 1, global_encode_uri),
        ("decodeURI", 1, global_decode_uri),
        ("encodeURIComponent", 1, global_encode_uri_component),
        ("decodeURIComponent", 1, global_decode_uri_component),
    ];
    for &(name, len, f) in globals {
        let h = vm.alloc_native_fn(name, f);
        vm.set_fn_length(h, len);
        define_global(vm, name, JsValue::from_object(h));
    }
}

// ---------------------------------------------------------------------------
// String, %StringIteratorPrototype%, & Annex B String HTML methods
// ---------------------------------------------------------------------------

fn install_string(vm: &mut JsVm) {
    // %StringIteratorPrototype%
    let sip = vm.string_iter_proto;
    define_method_len(vm, sip, "next", 0, str_iter_next);
    let stag = JsValue::from_string(vm.heap.alloc_string("String Iterator"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        sip,
        k_tag,
        stag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    let sp = vm.string_proto;
    let methods: &[(&str, u32, crate::vm::NativeFnPtr)] = &[
        ("charAt", 1, str_char_at),
        ("charCodeAt", 1, str_char_code_at),
        ("codePointAt", 1, str_code_point_at),
        ("slice", 2, str_slice),
        ("substring", 2, str_substring),
        ("substr", 2, str_substr),
        ("indexOf", 1, str_index_of),
        ("lastIndexOf", 1, str_last_index_of),
        ("includes", 1, str_includes),
        ("startsWith", 1, str_starts_with),
        ("endsWith", 1, str_ends_with),
        ("split", 2, str_split),
        ("trim", 0, str_trim),
        ("toLowerCase", 0, str_to_lower),
        ("toUpperCase", 0, str_to_upper),
        ("toLocaleLowerCase", 0, str_to_lower),
        ("toLocaleUpperCase", 0, str_to_upper),
        ("localeCompare", 1, str_locale_compare),
        ("normalize", 0, str_normalize),
        ("isWellFormed", 0, str_is_well_formed),
        ("toWellFormed", 0, str_to_well_formed),
        ("repeat", 1, str_repeat),
        ("padStart", 1, str_pad_start),
        ("padEnd", 1, str_pad_end),
        ("replace", 2, str_replace),
        ("replaceAll", 2, str_replace_all),
        ("match", 1, str_match),
        ("matchAll", 1, str_match_all),
        ("search", 1, str_search),
        ("concat", 1, str_concat),
        ("at", 1, str_at),
        ("toString", 0, str_to_string),
        ("valueOf", 0, str_to_string),
        ("anchor", 1, |vm, t, a| {
            str_html(vm, t, "a", Some(("name", arg0(a))))
        }),
        ("big", 0, |vm, t, _| str_html(vm, t, "big", None)),
        ("blink", 0, |vm, t, _| str_html(vm, t, "blink", None)),
        ("bold", 0, |vm, t, _| str_html(vm, t, "b", None)),
        ("fixed", 0, |vm, t, _| str_html(vm, t, "tt", None)),
        ("fontcolor", 1, |vm, t, a| {
            str_html(vm, t, "font", Some(("color", arg0(a))))
        }),
        ("fontsize", 1, |vm, t, a| {
            str_html(vm, t, "font", Some(("size", arg0(a))))
        }),
        ("italics", 0, |vm, t, _| str_html(vm, t, "i", None)),
        ("link", 1, |vm, t, a| {
            str_html(vm, t, "a", Some(("href", arg0(a))))
        }),
        ("small", 0, |vm, t, _| str_html(vm, t, "small", None)),
        ("strike", 0, |vm, t, _| str_html(vm, t, "strike", None)),
        ("sub", 0, |vm, t, _| str_html(vm, t, "sub", None)),
        ("sup", 0, |vm, t, _| str_html(vm, t, "sup", None)),
    ];
    for &(name, len, f) in methods {
        define_method_len(vm, sp, name, len, f);
    }
    // Annex B §B.2.3.1: `trimLeft === trimStart` and `trimRight === trimEnd`.
    let ts_fn = vm.alloc_native_fn("trimStart", str_trim_start);
    vm.set_fn_length(ts_fn, 0);
    for kname in ["trimStart", "trimLeft"] {
        let k = vm.heap.intern_key(kname);
        vm.heap.object_define_data(
            sp,
            k,
            JsValue::from_object(ts_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    let te_fn = vm.alloc_native_fn("trimEnd", str_trim_end);
    vm.set_fn_length(te_fn, 0);
    for kname in ["trimEnd", "trimRight"] {
        let k = vm.heap.intern_key(kname);
        vm.heap.object_define_data(
            sp,
            k,
            JsValue::from_object(te_fn),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }

    let str_iter_fn = vm.alloc_native_fn("[Symbol.iterator]", str_symbol_iterator);
    vm.set_fn_length(str_iter_fn, 0);
    let sym_iter_key = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        sp,
        sym_iter_key,
        JsValue::from_object(str_iter_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        sp,
        vm.key_length,
        JsValue::from_i32(0),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );

    let ctor = vm.alloc_native_fn("String", string_ctor);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(sp),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    link_ctor_meta(vm, ctor, sp, 1, true);
    define_method_len(vm, ctor, "fromCharCode", 1, str_from_char_code);
    define_method_len(vm, ctor, "fromCodePoint", 1, str_from_code_point);
    define_method_len(vm, ctor, "raw", 1, str_raw);
    define_global(vm, "String", JsValue::from_object(ctor));
}

fn link_ctor_meta(
    vm: &mut JsVm,
    ctor: HeapHandle,
    proto: HeapHandle,
    len: u32,
    constructable: bool,
) {
    if constructable {
        vm.mark_constructor(ctor);
    }
    vm.set_fn_length(ctor, len);
    vm.heap.object_define_data(
        proto,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

fn require_coercible(vm: &mut JsVm, val: JsValue) -> Result<(), JsException> {
    if val.is_null() || val.is_undefined() {
        Err(vm.type_error("Cannot convert undefined or null to object"))
    } else {
        Ok(())
    }
}

fn require_string_this(vm: &mut JsVm, this_val: JsValue) -> Result<String, JsException> {
    require_coercible(vm, this_val)?;
    vm.to_js_string(this_val)
}

fn get_method_opt(
    vm: &mut JsVm,
    val: JsValue,
    key: crate::PropertyKey,
) -> Result<Option<JsValue>, JsException> {
    let h = vm.to_object(val)?;
    let func = vm.get_property_by_key(h, key)?;
    if func.is_undefined() || func.is_null() {
        return Ok(None);
    }
    if !func.as_object().is_some_and(|fh| vm.is_callable_handle(fh)) {
        return Err(vm.type_error("Property is not a function"));
    }
    Ok(Some(func))
}

fn clamp_u16_idx(n: f64, len: usize) -> usize {
    if n <= 0.0 || n.is_nan() {
        0
    } else if n >= len as f64 {
        len
    } else {
        n as usize
    }
}

fn find_u16_subslice(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    if from + needle.len() > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|rel| from + rel)
}

fn str_symbol_iterator(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let iter = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.string_iter_proto),
    ));
    let chars: Vec<String> = s.chars().map(|c| c.to_string()).collect();
    let idx = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let step = vm.alloc_host_fn("", move |vm, _t, _a| {
        let i = idx.get();
        if i >= chars.len() {
            return Ok(crate::promise::make_iter_result(
                vm,
                JsValue::UNDEFINED,
                true,
            ));
        }
        idx.set(i + 1);
        let v = JsValue::from_string(vm.heap.alloc_string(chars[i].as_str()));
        Ok(crate::promise::make_iter_result(vm, v, false))
    });
    let k_step = vm.heap.intern_key("__str_iter_step");
    vm.heap.object_define_data(
        iter,
        k_step,
        JsValue::from_object(step),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    Ok(JsValue::from_object(iter))
}

fn str_iter_next(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("String Iterator.next called on incompatible receiver"));
    };
    let k_step = vm.heap.intern_key("__str_iter_step");
    let Some(step) = vm
        .heap
        .object_get_own(h, k_step, &vm.shapes)
        .map(|p| p.value)
    else {
        return Err(vm.type_error("String Iterator.next called on incompatible receiver"));
    };
    vm.call_value(step, JsValue::UNDEFINED, &[])
}

fn unbox_prim(vm: &JsVm, val: JsValue) -> JsValue {
    if let Some(h) = val.as_object() {
        if let Some(bv) = vm
            .heap
            .get_object(h)
            .and_then(|o| o.rare.as_ref())
            .and_then(|r| r.boxed_value)
        {
            return bv;
        }
    }
    val
}

fn string_ctor(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let is_construct = !vm.native_new_target.is_undefined();
    let s = if let Some(&v) = args.first() {
        if !is_construct && v.is_symbol() {
            let desc = v
                .as_symbol()
                .and_then(|id| vm.symbol_descriptions.get(&id))
                .cloned()
                .unwrap_or_default();
            format!("Symbol({desc})")
        } else {
            vm.to_js_string(v)?
        }
    } else {
        String::new()
    };
    let sv = JsValue::from_string(vm.heap.alloc_string(s.clone()));
    if is_construct {
        if let Some(h) = this_val.as_object() {
            let units: Vec<u16> = s.encode_utf16().collect();
            let len = units.len() as f64;
            if let Some(o) = vm.heap.get_object_mut(h) {
                o.rare_mut().boxed_value = Some(sv);
            }
            for (i, u) in units.into_iter().enumerate() {
                let ch = char::from_u32(u32::from(u))
                    .map_or_else(|| "\u{FFFD}".to_string(), |c| c.to_string());
                let cv = JsValue::from_string(vm.heap.alloc_string(ch));
                let k = vm.heap.intern_key(&i.to_string());
                vm.heap
                    .object_define_data(h, k, cv, PropertyAttrs::ENUMERABLE, &mut vm.shapes);
            }
            vm.heap.object_define_data(
                h,
                vm.key_length,
                JsValue::from_f64(len),
                PropertyAttrs::empty(),
                &mut vm.shapes,
            );
            vm.heap.write_barrier(h, sv);
            return Ok(this_val);
        }
    }
    Ok(sv)
}

fn str_from_char_code(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let mut units = Vec::with_capacity(args.len());
    for &a in args {
        units.push((vm.to_u32(a)? & 0xFFFF) as u16);
    }
    Ok(JsValue::from_string(
        vm.heap.alloc_string(String::from_utf16_lossy(&units)),
    ))
}

fn str_from_code_point(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let mut units: Vec<u16> = Vec::with_capacity(args.len());
    for &a in args {
        let n = vm.to_number(a)?;
        if !n.is_finite() || n.fract() != 0.0 || !(0.0..=1_114_111.0).contains(&n) {
            return Err(vm.range_error("Invalid code point"));
        }
        let cp = n as u32;
        if cp <= 0xFFFF {
            units.push(cp as u16);
        } else {
            let adj = cp - 0x10000;
            units.push((0xD800 + (adj >> 10)) as u16);
            units.push((0xDC00 + (adj & 0x3FF)) as u16);
        }
    }
    Ok(JsValue::from_string(
        vm.heap.alloc_string(String::from_utf16_lossy(&units)),
    ))
}

fn str_raw(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let cooked = vm.to_object(arg0(args))?;
    let k_raw = vm.heap.intern_key("raw");
    let raw_v = vm.get_property_by_key(cooked, k_raw)?;
    let raw = vm.to_object(raw_v)?;
    let lit_count = vm.array_get_len(raw)?;
    if lit_count == 0 {
        return Ok(JsValue::from_string(vm.heap.alloc_string("")));
    }
    let mut out = String::new();
    let num_subs = args.len().saturating_sub(1) as u64;
    for next_idx in 0..lit_count {
        let seg_v = vm.array_get_index(raw, next_idx)?;
        out.push_str(&vm.to_js_string(seg_v)?);
        if next_idx + 1 == lit_count {
            break;
        }
        if next_idx < num_subs {
            out.push_str(&vm.to_js_string(args[(next_idx + 1) as usize])?);
        }
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let prim = unbox_prim(vm, this_val);
    if prim.is_string() {
        return Ok(prim);
    }
    Err(vm.type_error("String.prototype.toString requires that 'this' be a String"))
}

fn str_char_at(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let pos = vm.to_integer_or_infinity(arg0(args))?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let out = if pos >= 0.0 && pos < units.len() as f64 {
        String::from_utf16_lossy(&units[pos as usize..pos as usize + 1])
    } else {
        String::new()
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_char_code_at(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let pos = vm.to_integer_or_infinity(arg0(args))?;
    let units: Vec<u16> = s.encode_utf16().collect();
    if pos < 0.0 || pos >= units.len() as f64 {
        return Ok(JsValue::NAN);
    }
    Ok(JsValue::from_i32(i32::from(units[pos as usize])))
}

fn str_code_point_at(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let pos = vm.to_integer_or_infinity(arg0(args))?;
    let units: Vec<u16> = s.encode_utf16().collect();
    if pos < 0.0 || pos >= units.len() as f64 {
        return Ok(JsValue::UNDEFINED);
    }
    let idx = pos as usize;
    let first = units[idx];
    let cp = if (0xD800..=0xDBFF).contains(&first) && idx + 1 < units.len() {
        let second = units[idx + 1];
        if (0xDC00..=0xDFFF).contains(&second) {
            (((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00) + 0x10000) as i32
        } else {
            i32::from(first)
        }
    } else {
        i32::from(first)
    };
    Ok(JsValue::from_i32(cp))
}

fn str_slice(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len() as u64;
    let start = match args.first().copied() {
        Some(v) if !v.is_undefined() => clamp_rel_f64(vm.to_integer_or_infinity(v)?, len) as usize,
        _ => 0,
    };
    let end = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => clamp_rel_f64(vm.to_integer_or_infinity(v)?, len) as usize,
        _ => units.len(),
    };
    let out = if start < end {
        String::from_utf16_lossy(&units[start..end])
    } else {
        String::new()
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_substring(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len();
    let start = match args.first().copied() {
        Some(v) if !v.is_undefined() => clamp_u16_idx(vm.to_integer_or_infinity(v)?, len),
        _ => 0,
    };
    let end = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => clamp_u16_idx(vm.to_integer_or_infinity(v)?, len),
        _ => len,
    };
    let (lo, hi) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    Ok(JsValue::from_string(
        vm.heap
            .alloc_string(String::from_utf16_lossy(&units[lo..hi])),
    ))
}

fn str_substr(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len() as u64;
    let start_f = vm.to_integer_or_infinity(arg0(args))?;
    let start = if start_f == f64::INFINITY {
        len as usize
    } else {
        clamp_rel_f64(start_f, len) as usize
    };
    let count = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => {
            let l = vm.to_integer_or_infinity(v)?;
            if l <= 0.0 {
                0
            } else {
                l.min((units.len() - start) as f64) as usize
            }
        }
        _ => units.len() - start,
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(
        String::from_utf16_lossy(&units[start..start + count]),
    )))
}

fn str_index_of(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let needle = vm.to_js_string(arg0(args))?;
    let pos_f = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => vm.to_integer_or_infinity(v)?,
        _ => 0.0,
    };
    let hay: Vec<u16> = s.encode_utf16().collect();
    let ndl: Vec<u16> = needle.encode_utf16().collect();
    let from = clamp_u16_idx(pos_f, hay.len());
    let found = find_u16_subslice(&hay, &ndl, from).map_or(-1.0, |i| i as f64);
    Ok(JsValue::from_f64(found))
}

fn str_last_index_of(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let needle = vm.to_js_string(arg0(args))?;
    let num_pos = match args.get(1).copied() {
        Some(v) => vm.to_number(v)?,
        None => f64::NAN,
    };
    let hay: Vec<u16> = s.encode_utf16().collect();
    let ndl: Vec<u16> = needle.encode_utf16().collect();
    let start = if num_pos.is_nan() {
        hay.len()
    } else {
        clamp_u16_idx(num_pos.trunc(), hay.len())
    };
    if ndl.is_empty() {
        return Ok(JsValue::from_f64(start as f64));
    }
    if ndl.len() > hay.len() {
        return Ok(JsValue::from_i32(-1));
    }
    let max_i = start.min(hay.len() - ndl.len());
    for i in (0..=max_i).rev() {
        if hay[i..i + ndl.len()] == ndl[..] {
            return Ok(JsValue::from_f64(i as f64));
        }
    }
    Ok(JsValue::from_i32(-1))
}

fn str_includes(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let search = arg0(args);
    if is_regexp(vm, search)? {
        return Err(vm.type_error(
            "First argument to String.prototype.includes must not be a regular expression",
        ));
    }
    let needle = vm.to_js_string(search)?;
    let pos_f = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => vm.to_integer_or_infinity(v)?,
        _ => 0.0,
    };
    let hay: Vec<u16> = s.encode_utf16().collect();
    let ndl: Vec<u16> = needle.encode_utf16().collect();
    let from = clamp_u16_idx(pos_f, hay.len());
    Ok(JsValue::from_bool(
        find_u16_subslice(&hay, &ndl, from).is_some(),
    ))
}

fn str_starts_with(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let search = arg0(args);
    if is_regexp(vm, search)? {
        return Err(vm.type_error(
            "First argument to String.prototype.startsWith must not be a regular expression",
        ));
    }
    let prefix = vm.to_js_string(search)?;
    let pos_f = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => vm.to_integer_or_infinity(v)?,
        _ => 0.0,
    };
    let hay: Vec<u16> = s.encode_utf16().collect();
    let pfx: Vec<u16> = prefix.encode_utf16().collect();
    let start = clamp_u16_idx(pos_f, hay.len());
    Ok(JsValue::from_bool(
        start + pfx.len() <= hay.len() && hay[start..start + pfx.len()] == pfx[..],
    ))
}

fn str_ends_with(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let search = arg0(args);
    if is_regexp(vm, search)? {
        return Err(vm.type_error(
            "First argument to String.prototype.endsWith must not be a regular expression",
        ));
    }
    let suffix = vm.to_js_string(search)?;
    let hay: Vec<u16> = s.encode_utf16().collect();
    let end = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => clamp_u16_idx(vm.to_integer_or_infinity(v)?, hay.len()),
        _ => hay.len(),
    };
    let sfx: Vec<u16> = suffix.encode_utf16().collect();
    Ok(JsValue::from_bool(
        sfx.len() <= end && hay[end - sfx.len()..end] == sfx[..],
    ))
}

fn str_split(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    require_coercible(vm, this_val)?;
    let sep = arg0(args);
    let lim_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    if !sep.is_null() && !sep.is_undefined() {
        if let Some(splitter) = get_method_opt(vm, sep, vm.key_symbol_split)? {
            return vm.call_value(splitter, sep, &[this_val, lim_arg]);
        }
    }
    let s = vm.to_js_string(this_val)?;
    let lim = if lim_arg.is_undefined() {
        u32::MAX
    } else {
        vm.to_u32(lim_arg)?
    };
    let sep_str = vm.to_js_string(sep)?;
    let arr = vm.alloc_array();
    if lim == 0 {
        return Ok(JsValue::from_object(arr));
    }
    if sep.is_undefined() {
        let v = JsValue::from_string(vm.heap.alloc_string(s));
        vm.heap.object_push_element(arr, v);
        return Ok(JsValue::from_object(arr));
    }
    let hay: Vec<u16> = s.encode_utf16().collect();
    let ndl: Vec<u16> = sep_str.encode_utf16().collect();
    if ndl.is_empty() {
        for u in hay.iter().take(lim as usize) {
            let v = JsValue::from_string(vm.heap.alloc_string(String::from_utf16_lossy(&[*u])));
            vm.heap.object_push_element(arr, v);
        }
        return Ok(JsValue::from_object(arr));
    }
    let (mut p, mut count) = (0usize, 0u32);
    while let Some(pos) = find_u16_subslice(&hay, &ndl, p) {
        let v = JsValue::from_string(vm.heap.alloc_string(String::from_utf16_lossy(&hay[p..pos])));
        vm.heap.object_push_element(arr, v);
        count += 1;
        if count == lim {
            return Ok(JsValue::from_object(arr));
        }
        p = pos + ndl.len();
    }
    let tail = JsValue::from_string(vm.heap.alloc_string(String::from_utf16_lossy(&hay[p..])));
    vm.heap.object_push_element(arr, tail);
    Ok(JsValue::from_object(arr))
}

fn str_trim(vm: &mut JsVm, this_val: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let trimmed = s.trim_matches(is_es_whitespace_or_lt);
    Ok(JsValue::from_string(vm.heap.alloc_string(trimmed)))
}

fn str_trim_start(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let trimmed = s.trim_start_matches(is_es_whitespace_or_lt);
    Ok(JsValue::from_string(vm.heap.alloc_string(trimmed)))
}

fn str_trim_end(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let trimmed = s.trim_end_matches(is_es_whitespace_or_lt);
    Ok(JsValue::from_string(vm.heap.alloc_string(trimmed)))
}

fn str_to_lower(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?.to_lowercase();
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn str_to_upper(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?.to_uppercase();
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn str_locale_compare(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let a = require_string_this(vm, this_val)?;
    let b = vm.to_js_string(arg0(args))?;
    let ord = match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(JsValue::from_i32(ord))
}

fn str_normalize(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let form = match args.first().copied() {
        Some(v) if !v.is_undefined() => vm.to_js_string(v)?,
        _ => "NFC".to_string(),
    };
    let out = match form.as_str() {
        "NFC" => icu::normalizer::ComposingNormalizer::new_nfc().normalize(&s),
        "NFD" => icu::normalizer::DecomposingNormalizer::new_nfd().normalize(&s),
        "NFKC" => icu::normalizer::ComposingNormalizer::new_nfkc().normalize(&s),
        "NFKD" => icu::normalizer::DecomposingNormalizer::new_nfkd().normalize(&s),
        _ => {
            return Err(vm.range_error(format!(
                "The normalization form should be one of NFC, NFD, NFKC, NFKD (got '{form}')"
            )))
        }
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_is_well_formed(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let _ = require_string_this(vm, this_val)?;
    Ok(JsValue::TRUE)
}

fn str_to_well_formed(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn str_repeat(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let n = vm.to_integer_or_infinity(arg0(args))?;
    if n < 0.0 || n.is_infinite() {
        return Err(vm.range_error("Invalid count value for String.prototype.repeat"));
    }
    let count = n as usize;
    if count == 0 || s.is_empty() {
        return Ok(JsValue::from_string(vm.heap.alloc_string("")));
    }
    if s.len().saturating_mul(count) > 32 * 1024 * 1024 {
        return Err(vm.range_error("Invalid string length"));
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(s.repeat(count))))
}

fn str_pad_start(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    str_pad_impl(vm, this_val, args, true)
}

fn str_pad_end(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    str_pad_impl(vm, this_val, args, false)
}

fn str_pad_impl(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    start: bool,
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let max_len = vm.to_length(arg0(args))?;
    let fill = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => vm.to_js_string(v)?,
        _ => " ".to_owned(),
    };
    let s_units: Vec<u16> = s.encode_utf16().collect();
    if max_len <= s_units.len() as u64 || fill.is_empty() {
        return Ok(JsValue::from_string(vm.heap.alloc_string(s)));
    }
    if max_len > 32 * 1024 * 1024 {
        return Err(vm.range_error("Invalid string length"));
    }
    let fill_units: Vec<u16> = fill.encode_utf16().collect();
    let need = (max_len as usize) - s_units.len();
    let mut pad_units = Vec::with_capacity(need);
    while pad_units.len() < need {
        let take = (need - pad_units.len()).min(fill_units.len());
        pad_units.extend_from_slice(&fill_units[..take]);
    }
    let pad = String::from_utf16_lossy(&pad_units);
    let out = if start {
        format!("{pad}{s}")
    } else {
        format!("{s}{pad}")
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_replace(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    str_replace_impl(vm, this_val, args, false)
}

fn str_replace_all(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    str_replace_impl(vm, this_val, args, true)
}

fn str_replace_impl(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    force_all: bool,
) -> Result<JsValue, JsException> {
    require_coercible(vm, this_val)?;
    let search = arg0(args);
    let repl_val = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    if !search.is_null() && !search.is_undefined() {
        if force_all && is_regexp(vm, search)? {
            let sh = search.as_object().unwrap();
            let k_flg = vm.heap.intern_key("flags");
            let flg_v = vm.get_property_by_key(sh, k_flg)?;
            require_coercible(vm, flg_v)?;
            if !vm.to_js_string(flg_v)?.contains('g') {
                return Err(
                    vm.type_error("String.prototype.replaceAll called with non-global RegExp")
                );
            }
        }
        if let Some(replacer) = get_method_opt(vm, search, vm.key_symbol_replace)? {
            return vm.call_value(replacer, search, &[this_val, repl_val]);
        }
    }
    let s = vm.to_js_string(this_val)?;
    let needle = vm.to_js_string(search)?;
    let functional = repl_val
        .as_object()
        .is_some_and(|h| vm.is_callable_handle(h));
    let repl_str = if functional {
        String::new()
    } else {
        vm.to_js_string(repl_val)?
    };
    let hay: Vec<u16> = s.encode_utf16().collect();
    let ndl: Vec<u16> = needle.encode_utf16().collect();
    let mut positions = Vec::new();
    let advance = ndl.len().max(1);
    if let Some(first) = find_u16_subslice(&hay, &ndl, 0) {
        positions.push(first);
        if force_all {
            let mut cur = first + advance;
            while cur <= hay.len() {
                if let Some(pos) = find_u16_subslice(&hay, &ndl, cur) {
                    positions.push(pos);
                    cur = pos + advance;
                } else {
                    break;
                }
            }
        }
    }
    let mut out = String::new();
    let mut end_of_last = 0usize;
    let sv = JsValue::from_string(vm.heap.alloc_string(s));
    let mv = JsValue::from_string(vm.heap.alloc_string(needle.as_str()));
    for pos in positions {
        out.push_str(&String::from_utf16_lossy(&hay[end_of_last..pos]));
        let rep = if functional {
            let r = vm.call_value(
                repl_val,
                JsValue::UNDEFINED,
                &[mv, JsValue::from_f64(pos as f64), sv],
            )?;
            vm.to_js_string(r)?
        } else {
            get_substitution(vm, &needle, &hay, pos, &[], None, &repl_str)?
        };
        out.push_str(&rep);
        end_of_last = pos + ndl.len();
    }
    out.push_str(&String::from_utf16_lossy(&hay[end_of_last..]));
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_match(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    require_coercible(vm, this_val)?;
    let rx_arg = arg0(args);
    if !rx_arg.is_null() && !rx_arg.is_undefined() {
        if let Some(matcher) = get_method_opt(vm, rx_arg, vm.key_symbol_match)? {
            return vm.call_value(matcher, rx_arg, &[this_val]);
        }
    }
    let s = vm.to_js_string(this_val)?;
    let sv = JsValue::from_string(vm.heap.alloc_string(s));
    let k_re = vm.heap.intern_key("RegExp");
    let re_ctor = vm.get_property_by_key(vm.global, k_re)?;
    let rx = vm.construct_value(re_ctor, &[rx_arg, JsValue::UNDEFINED], re_ctor)?;
    let m = vm.get_property_by_key(rx.as_object().unwrap(), vm.key_symbol_match)?;
    vm.call_value(m, rx, &[sv])
}

fn str_match_all(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    require_coercible(vm, this_val)?;
    let rx_arg = arg0(args);
    if !rx_arg.is_null() && !rx_arg.is_undefined() {
        if is_regexp(vm, rx_arg)? {
            let rh = rx_arg.as_object().unwrap();
            let k_flg = vm.heap.intern_key("flags");
            let flg_v = vm.get_property_by_key(rh, k_flg)?;
            require_coercible(vm, flg_v)?;
            if !vm.to_js_string(flg_v)?.contains('g') {
                return Err(
                    vm.type_error("String.prototype.matchAll called with non-global RegExp")
                );
            }
        }
        if let Some(matcher) = get_method_opt(vm, rx_arg, vm.key_symbol_match_all)? {
            return vm.call_value(matcher, rx_arg, &[this_val]);
        }
    }
    let s = vm.to_js_string(this_val)?;
    let sv = JsValue::from_string(vm.heap.alloc_string(s));
    let k_re = vm.heap.intern_key("RegExp");
    let re_ctor = vm.get_property_by_key(vm.global, k_re)?;
    let g_v = JsValue::from_string(vm.heap.alloc_string("g"));
    let rx = vm.construct_value(re_ctor, &[rx_arg, g_v], re_ctor)?;
    let m = vm.get_property_by_key(rx.as_object().unwrap(), vm.key_symbol_match_all)?;
    vm.call_value(m, rx, &[sv])
}

fn str_search(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    require_coercible(vm, this_val)?;
    let rx_arg = arg0(args);
    if !rx_arg.is_null() && !rx_arg.is_undefined() {
        if let Some(searcher) = get_method_opt(vm, rx_arg, vm.key_symbol_search)? {
            return vm.call_value(searcher, rx_arg, &[this_val]);
        }
    }
    let s = vm.to_js_string(this_val)?;
    let sv = JsValue::from_string(vm.heap.alloc_string(s));
    let k_re = vm.heap.intern_key("RegExp");
    let re_ctor = vm.get_property_by_key(vm.global, k_re)?;
    let rx = vm.construct_value(re_ctor, &[rx_arg, JsValue::UNDEFINED], re_ctor)?;
    let m = vm.get_property_by_key(rx.as_object().unwrap(), vm.key_symbol_search)?;
    vm.call_value(m, rx, &[sv])
}

fn str_concat(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let mut out = require_string_this(vm, this_val)?;
    for &a in args {
        out.push_str(&vm.to_js_string(a)?);
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_at(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let rel = vm.to_integer_or_infinity(arg0(args))?;
    let k = if rel >= 0.0 {
        rel
    } else {
        units.len() as f64 + rel
    };
    if k < 0.0 || k >= units.len() as f64 {
        return Ok(JsValue::UNDEFINED);
    }
    let idx = k as usize;
    Ok(JsValue::from_string(vm.heap.alloc_string(
        String::from_utf16_lossy(&units[idx..idx + 1]),
    )))
}

/// ECMA-262 Annex B §B.2.3.2.1 `CreateHTML(string, tag, attribute, value)`.
fn str_html(
    vm: &mut JsVm,
    this_val: JsValue,
    tag: &str,
    attr: Option<(&str, JsValue)>,
) -> Result<JsValue, JsException> {
    let s = require_string_this(vm, this_val)?;
    let out = if let Some((attr_name, attr_val)) = attr {
        let v = vm.to_js_string(attr_val)?.replace('"', "&quot;");
        format!("<{tag} {attr_name}=\"{v}\">{s}</{tag}>")
    } else {
        format!("<{tag}>{s}</{tag}>")
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

// ---------------------------------------------------------------------------
// Number & Global numeric/URI functions
// ---------------------------------------------------------------------------

fn install_number(vm: &mut JsVm) {
    let np = vm.number_proto;
    define_method(vm, np, "toFixed", num_to_fixed);
    define_method(vm, np, "toString", num_to_string);
    define_method(vm, np, "valueOf", num_value_of);

    let ctor = vm.alloc_native_fn("Number", number_ctor);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(np),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    link_ctor_meta(vm, ctor, np, 1, true);
    define_method(vm, ctor, "isNaN", num_is_nan);
    define_method(vm, ctor, "isFinite", num_is_finite);
    define_method(vm, ctor, "isInteger", num_is_integer);
    define_method(vm, ctor, "isSafeInteger", num_is_safe_integer);
    define_method(vm, ctor, "parseInt", global_parse_int);
    define_method(vm, ctor, "parseFloat", global_parse_float);

    let consts = [
        ("EPSILON", f64::EPSILON),
        ("MAX_SAFE_INTEGER", 9_007_199_254_740_991.0),
        ("MIN_SAFE_INTEGER", -9_007_199_254_740_991.0),
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", 5e-324),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
    ];
    for (name, val) in consts {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            ctor,
            k,
            JsValue::from_f64(val),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
    }
    define_global(vm, "Number", JsValue::from_object(ctor));
}

fn number_ctor(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let n = match args.first() {
        Some(&v) => {
            if let Some(bh) = v.as_bigint() {
                vm.heap
                    .get_bigint(bh)
                    .map_or(0.0, |b| b.to_i64_lossy() as f64)
            } else {
                vm.to_number(v)?
            }
        }
        None => 0.0,
    };
    let nv = JsValue::from_f64(n);
    if let Some(h) = this_val.as_object() {
        if vm.heap.get_object(h).and_then(|o| o.prototype) == Some(vm.number_proto) {
            if let Some(o) = vm.heap.get_object_mut(h) {
                o.rare_mut().boxed_value = Some(nv);
            }
            return Ok(this_val);
        }
    }
    Ok(nv)
}

fn num_to_fixed(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let n = vm.to_number(this_val)?;
    let digits = args
        .first()
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0);
    if !(0..=100).contains(&digits) {
        return Err(vm.range_error("toFixed() digits argument must be between 0 and 100"));
    }
    let out = if !n.is_finite() {
        vm.value_to_rust_string(JsValue::from_f64(n))
    } else {
        format!("{n:.prec$}", prec = digits as usize)
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn num_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let prim = unbox_prim(vm, this_val);
    let Some(n) = prim.as_number() else {
        return Err(vm.type_error("Number.prototype.toString requires that 'this' be a Number"));
    };
    let radix = args
        .first()
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(10);
    if !(2..=36).contains(&radix) {
        return Err(vm.range_error("toString() radix must be between 2 and 36"));
    }
    if radix == 10 || !n.is_finite() {
        return Ok(JsValue::from_string(
            vm.heap
                .alloc_string(vm.value_to_rust_string(JsValue::from_f64(n))),
        ));
    }
    let neg = n < 0.0;
    let mut int = n.abs().trunc() as u64;
    if int == 0 {
        return Ok(JsValue::from_string(vm.heap.alloc_string("0")));
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    let r = radix as u64;
    while int > 0 {
        buf.push(DIGITS[(int % r) as usize] as char);
        int /= r;
    }
    if neg {
        buf.push('-');
    }
    let s: String = buf.into_iter().rev().collect();
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn num_value_of(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let prim = unbox_prim(vm, this_val);
    if prim.is_number() {
        return Ok(prim);
    }
    Err(vm.type_error("Number.prototype.valueOf requires that 'this' be a Number"))
}

fn num_is_nan(_vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let is = args
        .first()
        .and_then(|v| v.as_number())
        .is_some_and(f64::is_nan);
    Ok(JsValue::from_bool(is))
}

fn num_is_finite(_vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let is = args
        .first()
        .and_then(|v| v.as_number())
        .is_some_and(f64::is_finite);
    Ok(JsValue::from_bool(is))
}

fn num_is_integer(
    _vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let is = args
        .first()
        .and_then(|v| v.as_number())
        .is_some_and(|n| n.is_finite() && n.fract() == 0.0);
    Ok(JsValue::from_bool(is))
}

fn num_is_safe_integer(
    _vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let is = args
        .first()
        .and_then(|v| v.as_number())
        .is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0);
    Ok(JsValue::from_bool(is))
}

fn global_is_nan(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let n = vm.to_number(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    Ok(JsValue::from_bool(n.is_nan()))
}

fn global_is_finite(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let n = vm.to_number(args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
    Ok(JsValue::from_bool(n.is_finite()))
}

fn global_parse_int(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let raw = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    let mut s = raw.trim_start();
    let mut sign = 1.0f64;
    if let Some(rest) = s.strip_prefix('-') {
        sign = -1.0;
        s = rest;
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    }
    let mut radix = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0);
    if radix == 0 {
        if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            radix = 16;
            s = rest;
        } else {
            radix = 10;
        }
    } else if radix == 16 {
        if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            s = rest;
        }
    } else if !(2..=36).contains(&radix) {
        return Ok(JsValue::NAN);
    }

    let r = radix as u32;
    let mut acc = 0.0f64;
    let mut saw_digit = false;
    for ch in s.chars() {
        let Some(d) = ch.to_digit(r) else {
            break;
        };
        saw_digit = true;
        acc = acc * f64::from(r) + f64::from(d);
    }
    Ok(if saw_digit {
        JsValue::from_f64(sign * acc)
    } else {
        JsValue::NAN
    })
}

fn global_parse_float(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let raw = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    let s = raw.trim_start();
    if s.starts_with("Infinity") || s.starts_with("+Infinity") {
        return Ok(JsValue::from_f64(f64::INFINITY));
    }
    if s.starts_with("-Infinity") {
        return Ok(JsValue::from_f64(f64::NEG_INFINITY));
    }
    let mut end = 0usize;
    let bytes = s.as_bytes();
    if end < bytes.len() && matches!(bytes[end], b'+' | b'-') {
        end += 1;
    }
    let mut has_digits = false;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        has_digits = true;
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            has_digits = true;
            end += 1;
        }
    }
    if has_digits && end < bytes.len() && matches!(bytes[end], b'e' | b'E') {
        let exp_start = end;
        end += 1;
        if end < bytes.len() && matches!(bytes[end], b'+' | b'-') {
            end += 1;
        }
        let digit_start = end;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == digit_start {
            end = exp_start;
        }
    }
    if !has_digits {
        return Ok(JsValue::NAN);
    }
    Ok(JsValue::from_f64(
        s[..end].parse::<f64>().unwrap_or(f64::NAN),
    ))
}

fn percent_encode(input: &str, keep_uri_reserved: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        let safe = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
            || (keep_uri_reserved
                && matches!(
                    b,
                    b';' | b',' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b'#'
                ));
        if safe {
            out.push(b as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn global_encode_uri(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    Ok(JsValue::from_string(
        vm.heap.alloc_string(percent_encode(&s, true)),
    ))
}

fn global_encode_uri_component(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    Ok(JsValue::from_string(
        vm.heap.alloc_string(percent_encode(&s, false)),
    ))
}

fn global_decode_uri(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    let dec = percent_decode(&s).ok_or_else(|| vm.make_error("URIError", "URI malformed"))?;
    Ok(JsValue::from_string(vm.heap.alloc_string(dec)))
}

fn global_decode_uri_component(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    global_decode_uri(vm, this_val, args)
}

// ---------------------------------------------------------------------------
// Boolean
// ---------------------------------------------------------------------------

fn install_boolean(vm: &mut JsVm) {
    let bp = vm.boolean_proto;
    define_method(vm, bp, "toString", |vm, this_val, _| {
        let Some(b) = unbox_prim(vm, this_val).as_bool() else {
            return Err(
                vm.type_error("Boolean.prototype.toString requires that 'this' be a Boolean")
            );
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(if b {
            "true"
        } else {
            "false"
        })))
    });
    define_method(vm, bp, "valueOf", |vm, this_val, _| {
        let prim = unbox_prim(vm, this_val);
        if prim.is_bool() {
            Ok(prim)
        } else {
            Err(vm.type_error("Boolean.prototype.valueOf requires that 'this' be a Boolean"))
        }
    });
    let ctor = vm.alloc_native_fn("Boolean", |vm, this_val, args| {
        let bv = JsValue::from_bool(args.first().copied().is_some_and(|v| vm.to_boolean(v)));
        if let Some(h) = this_val.as_object() {
            if vm.heap.get_object(h).and_then(|o| o.prototype) == Some(vm.boolean_proto) {
                if let Some(o) = vm.heap.get_object_mut(h) {
                    o.rare_mut().boxed_value = Some(bv);
                }
                return Ok(this_val);
            }
        }
        Ok(bv)
    });
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(bp),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    link_ctor_meta(vm, ctor, bp, 1, true);
    define_global(vm, "Boolean", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// BigInt
// ---------------------------------------------------------------------------

fn bigint_as_n(vm: &mut JsVm, args: &[JsValue], signed: bool) -> Result<JsValue, JsException> {
    let bits_f = vm.to_number(arg0(args))?;
    if !bits_f.is_finite() || bits_f < 0.0 {
        return Err(vm.range_error("Invalid index for BigInt clamp"));
    }
    let bits = bits_f.trunc() as u32;
    let Some(h) = args.get(1).and_then(|v| v.as_bigint()) else {
        return Err(vm.type_error("BigInt clamp requires a BigInt"));
    };
    let raw = vm.heap.get_bigint(h).map_or(0i64, JsBigInt::to_i64_lossy);
    let bi = if signed {
        let out = if bits == 0 {
            0i64
        } else if bits >= 64 {
            raw
        } else {
            (raw << (64 - bits)) >> (64 - bits)
        };
        JsBigInt::from_i64(out)
    } else {
        let u = raw as u64;
        let out = if bits == 0 {
            0u64
        } else if bits >= 64 {
            u
        } else {
            u & ((1u64 << bits) - 1)
        };
        JsBigInt::from_i128(i128::from(out))
    };
    Ok(JsValue::from_bigint(vm.heap.alloc_bigint(bi)))
}

fn install_bigint(vm: &mut JsVm) {
    let bi_proto = vm.bigint_proto;
    define_method(vm, bi_proto, "toString", |vm, this_val, args| {
        let Some(h) = unbox_prim(vm, this_val).as_bigint() else {
            return Err(vm.type_error("BigInt.prototype.toString requires a BigInt"));
        };
        let radix = args
            .first()
            .filter(|v| !v.is_undefined())
            .map(|&v| vm.to_i32(v))
            .transpose()?
            .unwrap_or(10);
        if !(2..=36).contains(&radix) {
            return Err(vm.range_error("toString() radix must be between 2 and 36"));
        }
        let s = vm
            .heap
            .get_bigint(h)
            .map_or_else(|| "0".to_owned(), |bi| bi.to_radix_string(radix as u32));
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    define_method(vm, bi_proto, "valueOf", |vm, this_val, _| {
        let prim = unbox_prim(vm, this_val);
        if prim.as_bigint().is_some() {
            Ok(prim)
        } else {
            Err(vm.type_error("BigInt.prototype.valueOf requires a BigInt"))
        }
    });
    // Note: `BigInt` is NOT a constructor (`new BigInt(1)` throws TypeError per ECMA-262 §21.2.1.1).
    let ctor = vm.alloc_native_fn("BigInt", |vm, _this, args| {
        let v = arg0(args);
        let bi = match v.kind() {
            JsValueKind::BigInt(_) => return Ok(v),
            JsValueKind::Bool(b) => JsBigInt::from_i64(i64::from(b)),
            JsValueKind::Int32(i) => JsBigInt::from_i64(i64::from(i)),
            JsValueKind::Float(f) if f.is_finite() && f.fract() == 0.0 => {
                JsBigInt::from_i128(f as i128)
            }
            JsValueKind::Float(_) => {
                return Err(vm.range_error("Cannot convert non-integer number to BigInt"))
            }
            JsValueKind::String(_) | JsValueKind::Object(_) => {
                let s = vm.value_to_rust_string(v);
                JsBigInt::parse_str(&s).ok_or_else(|| {
                    vm.make_error("SyntaxError", format!("Cannot convert {s} to a BigInt"))
                })?
            }
            _ => return Err(vm.type_error("Cannot convert value to a BigInt")),
        };
        Ok(JsValue::from_bigint(vm.heap.alloc_bigint(bi)))
    });
    let as_int_n = |vm: &mut JsVm, _this: JsValue, args: &[JsValue]| bigint_as_n(vm, args, true);
    let as_uint_n = |vm: &mut JsVm, _this: JsValue, args: &[JsValue]| bigint_as_n(vm, args, false);
    define_method_len(vm, ctor, "asIntN", 2, as_int_n);
    define_method_len(vm, ctor, "asUintN", 2, as_uint_n);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(bi_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    link_ctor_meta(vm, ctor, bi_proto, 1, false);
    define_global(vm, "BigInt", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// Math
// ---------------------------------------------------------------------------

fn install_math(vm: &mut JsVm) {
    let math = vm.alloc_ordinary_object();
    let consts = [
        ("PI", std::f64::consts::PI),
        ("E", std::f64::consts::E),
        ("LN2", std::f64::consts::LN_2),
        ("LN10", std::f64::consts::LN_10),
        ("LOG2E", std::f64::consts::LOG2_E),
        ("LOG10E", std::f64::consts::LOG10_E),
        ("SQRT2", std::f64::consts::SQRT_2),
        ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
    ];
    for (name, val) in consts {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            math,
            k,
            JsValue::from_f64(val),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
    }

    define_method(vm, math, "abs", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.abs()))
    });
    define_method(vm, math, "floor", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.floor()))
    });
    define_method(vm, math, "ceil", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.ceil()))
    });
    define_method(vm, math, "trunc", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.trunc()))
    });
    define_method(vm, math, "round", |vm, _, a| {
        let n = vm.to_number(arg0(a))?;
        let r = if !n.is_finite() || n == 0.0 {
            n
        } else {
            (n + 0.5).floor()
        };
        Ok(JsValue::from_f64(r))
    });
    define_method(vm, math, "sign", |vm, _, a| {
        let n = vm.to_number(arg0(a))?;
        Ok(JsValue::from_f64(if n.is_nan() || n == 0.0 {
            n
        } else if n > 0.0 {
            1.0
        } else {
            -1.0
        }))
    });
    define_method(vm, math, "min", |vm, _, args| {
        let mut res = f64::INFINITY;
        for &v in args {
            let n = vm.to_number(v)?;
            if n.is_nan() {
                return Ok(JsValue::NAN);
            }
            if n < res || (n == 0.0 && res == 0.0 && n.is_sign_negative()) {
                res = n;
            }
        }
        Ok(JsValue::from_f64(res))
    });
    define_method(vm, math, "max", |vm, _, args| {
        let mut res = f64::NEG_INFINITY;
        for &v in args {
            let n = vm.to_number(v)?;
            if n.is_nan() {
                return Ok(JsValue::NAN);
            }
            if n > res || (n == 0.0 && res == 0.0 && n.is_sign_positive()) {
                res = n;
            }
        }
        Ok(JsValue::from_f64(res))
    });
    define_method(vm, math, "pow", |vm, _, a| {
        let x = vm.to_number(arg0(a))?;
        let y = vm.to_number(a.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
        Ok(JsValue::from_f64(x.powf(y)))
    });
    define_method(vm, math, "sqrt", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.sqrt()))
    });
    define_method(vm, math, "cbrt", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.cbrt()))
    });
    define_method(vm, math, "hypot", |vm, _, args| {
        let mut sum = 0.0f64;
        for &v in args {
            let n = vm.to_number(v)?;
            sum += n * n;
        }
        Ok(JsValue::from_f64(sum.sqrt()))
    });
    define_method(vm, math, "sin", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.sin()))
    });
    define_method(vm, math, "cos", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.cos()))
    });
    define_method(vm, math, "tan", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.tan()))
    });
    define_method(vm, math, "asin", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.asin()))
    });
    define_method(vm, math, "acos", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.acos()))
    });
    define_method(vm, math, "atan", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.atan()))
    });
    define_method(vm, math, "atan2", |vm, _, a| {
        let y = vm.to_number(arg0(a))?;
        let x = vm.to_number(a.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
        Ok(JsValue::from_f64(y.atan2(x)))
    });
    define_method(vm, math, "exp", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.exp()))
    });
    define_method(vm, math, "log", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.ln()))
    });
    define_method(vm, math, "log2", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.log2()))
    });
    define_method(vm, math, "log10", |vm, _, a| {
        Ok(JsValue::from_f64(vm.to_number(arg0(a))?.log10()))
    });
    define_method(vm, math, "imul", |vm, _, a| {
        let x = vm.to_i32(arg0(a))?;
        let y = vm.to_i32(a.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
        Ok(JsValue::from_i32(x.wrapping_mul(y)))
    });
    define_method(vm, math, "clz32", |vm, _, a| {
        let x = vm.to_u32(arg0(a))?;
        Ok(JsValue::from_i32(x.leading_zeros() as i32))
    });
    define_method(vm, math, "fround", |vm, _, a| {
        let x = vm.to_number(arg0(a))?;
        Ok(JsValue::from_f64(f64::from(x as f32)))
    });
    define_method(vm, math, "random", |vm, _, _| {
        // xorshift64* producing uniform [0, 1) f64
        let mut x = vm.rng_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        vm.rng_state = x;
        let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        Ok(JsValue::from_f64(
            (bits as f64) * (1.0 / ((1u64 << 53) as f64)),
        ))
    });

    define_global(vm, "Math", JsValue::from_object(math));
}

#[inline]
fn arg0(args: &[JsValue]) -> JsValue {
    args.first().copied().unwrap_or(JsValue::UNDEFINED)
}

// ---------------------------------------------------------------------------
// Symbol
// ---------------------------------------------------------------------------

fn install_symbol(vm: &mut JsVm) {
    let sym_proto = vm.symbol_proto;
    define_method(vm, sym_proto, "valueOf", |vm, this_val, _| {
        let prim = unbox_prim(vm, this_val);
        if prim.is_symbol() {
            Ok(prim)
        } else {
            Err(vm.type_error("Symbol.prototype.valueOf requires a Symbol"))
        }
    });
    define_method(vm, sym_proto, "toString", |vm, this_val, _| {
        let Some(id) = unbox_prim(vm, this_val).as_symbol() else {
            return Err(vm.type_error("Symbol.prototype.toString requires a Symbol"));
        };
        let desc = vm.symbol_descriptions.get(&id).cloned().unwrap_or_default();
        Ok(JsValue::from_string(
            vm.heap.alloc_string(format!("Symbol({desc})")),
        ))
    });
    let desc_getter = vm.alloc_native_fn("get description", |vm, this_val, _| {
        let Some(id) = unbox_prim(vm, this_val).as_symbol() else {
            return Err(vm.type_error("Symbol.prototype.description requires a Symbol"));
        };
        Ok(match vm.symbol_descriptions.get(&id).cloned() {
            Some(d) => JsValue::from_string(vm.heap.alloc_string(d)),
            None => JsValue::UNDEFINED,
        })
    });
    let k_desc = vm.heap.intern_key("description");
    vm.heap.object_define_accessor(
        sym_proto,
        k_desc,
        Some(desc_getter),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    let ctor = vm.alloc_native_fn("Symbol", |vm, _this, args| {
        let desc = args
            .first()
            .filter(|v| !v.is_undefined())
            .map(|&v| vm.to_js_string(v))
            .transpose()?;
        let id = vm.heap.alloc_symbol(desc.clone());
        if let Some(d) = desc {
            vm.symbol_descriptions.insert(id, d);
        }
        Ok(JsValue::from_symbol(id))
    });
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(sym_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    link_ctor_meta(vm, ctor, sym_proto, 0, false);
    define_method(vm, ctor, "for", |vm, _this, args| {
        let key = vm.value_to_rust_string(arg0(args));
        if let Some(&id) = vm.symbol_registry.get(&key) {
            return Ok(JsValue::from_symbol(id));
        }
        let id = vm.heap.alloc_symbol(Some(key.clone()));
        vm.symbol_registry.insert(key.clone(), id);
        vm.symbol_descriptions.insert(id, key);
        Ok(JsValue::from_symbol(id))
    });
    define_method(vm, ctor, "keyFor", |vm, _this, args| {
        let Some(sym) = arg0(args).as_symbol() else {
            return Err(vm.type_error("Symbol.keyFor requires a Symbol"));
        };
        for (k, &v) in &vm.symbol_registry {
            if v == sym {
                let s = k.clone();
                return Ok(JsValue::from_string(vm.heap.alloc_string(s)));
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    for (well_known, id) in [
        ("iterator", vm.sym_iterator),
        ("asyncIterator", vm.sym_async_iterator),
        ("toStringTag", vm.sym_to_string_tag),
        ("hasInstance", vm.sym_has_instance),
        ("toPrimitive", vm.sym_to_primitive),
        ("species", vm.sym_species),
        ("isConcatSpreadable", vm.sym_is_concat_spreadable),
        ("unscopables", vm.sym_unscopables),
        ("match", vm.sym_match),
        ("matchAll", vm.sym_match_all),
        ("replace", vm.sym_replace),
        ("search", vm.sym_search),
        ("split", vm.sym_split),
    ] {
        vm.symbol_descriptions
            .insert(id, format!("Symbol.{well_known}"));
        let k = vm.heap.intern_key(well_known);
        vm.heap.object_define_data(
            ctor,
            k,
            JsValue::from_symbol(id),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
    }
    define_global(vm, "Symbol", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// Date (UTC calendar math)
// ---------------------------------------------------------------------------

fn install_date(vm: &mut JsVm) {
    let dp = vm.date_proto;
    define_method(vm, dp, "getTime", date_get_time);
    define_method(vm, dp, "valueOf", date_get_time);
    define_method(vm, dp, "toISOString", date_to_iso_string);
    define_method(vm, dp, "toString", date_to_iso_string);
    define_method(vm, dp, "getUTCFullYear", |vm, t, _| Ok(date_part(vm, t, 0)));
    define_method(vm, dp, "getUTCMonth", |vm, t, _| Ok(date_part(vm, t, 1)));
    define_method(vm, dp, "getUTCDate", |vm, t, _| Ok(date_part(vm, t, 2)));
    define_method(vm, dp, "getUTCHours", |vm, t, _| Ok(date_part(vm, t, 3)));
    define_method(vm, dp, "getUTCMinutes", |vm, t, _| Ok(date_part(vm, t, 4)));
    define_method(vm, dp, "getUTCSeconds", |vm, t, _| Ok(date_part(vm, t, 5)));
    define_method(vm, dp, "getUTCMilliseconds", |vm, t, _| {
        Ok(date_part(vm, t, 6))
    });

    let ctor = vm.alloc_native_fn("Date", date_ctor);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(dp),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    link_ctor_meta(vm, ctor, dp, 7, true);
    define_method(vm, ctor, "now", |_, _, _| Ok(JsValue::from_f64(now_ms())));
    define_method(vm, ctor, "UTC", date_utc);
    define_method(vm, ctor, "parse", |vm, _, args| {
        let s = vm.value_to_rust_string(arg0(args));
        Ok(JsValue::from_f64(parse_iso_date(&s).unwrap_or(f64::NAN)))
    });
    define_global(vm, "Date", JsValue::from_object(ctor));
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

fn date_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let ms = if args.is_empty() {
        now_ms()
    } else if args.len() == 1 {
        if args[0].is_string() {
            let s = vm.value_to_rust_string(args[0]);
            parse_iso_date(&s).unwrap_or(f64::NAN)
        } else {
            vm.to_number(args[0])?
        }
    } else {
        let utc = date_utc(vm, JsValue::UNDEFINED, args)?;
        utc.as_number().unwrap_or(f64::NAN)
    };
    Ok(JsValue::from_object(vm.alloc_date(ms)))
}

fn date_utc(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let year = vm.to_i32(arg0(args))? as i64;
    let month = args.get(1).map(|&v| vm.to_i32(v)).transpose()?.unwrap_or(0) as i64;
    let day = args.get(2).map(|&v| vm.to_i32(v)).transpose()?.unwrap_or(1) as i64;
    let hour = args.get(3).map(|&v| vm.to_i32(v)).transpose()?.unwrap_or(0) as i64;
    let min = args.get(4).map(|&v| vm.to_i32(v)).transpose()?.unwrap_or(0) as i64;
    let sec = args.get(5).map(|&v| vm.to_i32(v)).transpose()?.unwrap_or(0) as i64;
    let ms = args.get(6).map(|&v| vm.to_i32(v)).transpose()?.unwrap_or(0) as i64;
    let full_year = if (0..=99).contains(&year) {
        1900 + year
    } else {
        year
    };
    let days = days_from_civil(full_year, month + 1, day);
    let total_ms = (((days * 24 + hour) * 60 + min) * 60 + sec) * 1000 + ms;
    Ok(JsValue::from_f64(total_ms as f64))
}

fn date_get_time(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ms = this_val
        .as_object()
        .and_then(|h| vm.heap.get_object(h))
        .and_then(|o| match o.kind {
            ObjectKind::Date(ms) => Some(ms),
            _ => None,
        })
        .ok_or_else(|| vm.type_error("Date method called on non-Date"))?;
    Ok(JsValue::from_f64(ms))
}

fn date_to_iso_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let ms = date_get_time(vm, this_val, &[])?
        .as_number()
        .unwrap_or(f64::NAN);
    if !ms.is_finite() {
        return Err(vm.range_error("Invalid time value"));
    }
    let (y, m, d, h, min, s, milli) = civil_from_ms(ms as i64);
    let iso = format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{milli:03}Z");
    Ok(JsValue::from_string(vm.heap.alloc_string(iso)))
}

fn date_part(vm: &JsVm, this_val: JsValue, field: u8) -> JsValue {
    let Some(ms) = this_val
        .as_object()
        .and_then(|h| vm.heap.get_object(h))
        .and_then(|o| match o.kind {
            ObjectKind::Date(ms) if ms.is_finite() => Some(ms as i64),
            _ => None,
        })
    else {
        return JsValue::NAN;
    };
    let (y, m, d, h, min, s, milli) = civil_from_ms(ms);
    let val = match field {
        0 => y as i32,
        1 => (m - 1) as i32,
        2 => d as i32,
        3 => h as i32,
        4 => min as i32,
        5 => s as i32,
        _ => milli as i32,
    };
    JsValue::from_i32(val)
}

/// Howard Hinnant civil-to-days algorithm (exact proleptic Gregorian calendar).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Howard Hinnant days-to-civil algorithm -> `(year, month 1..=12, day, h, m, s, ms)`.
fn civil_from_ms(total_ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let ms_in_day = 86_400_000i64;
    let days = total_ms.div_euclid(ms_in_day);
    let rem = total_ms.rem_euclid(ms_in_day);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    let year = y + i64::from(m <= 2);
    let milli = (rem % 1000) as u32;
    let secs = rem / 1000;
    let s = (secs % 60) as u32;
    let min = ((secs / 60) % 60) as u32;
    let h = (secs / 3600) as u32;
    (year, m, d, h, min, s, milli)
}

fn parse_iso_date(s: &str) -> Option<f64> {
    // Supports ISO-8601 `YYYY-MM-DD` and `YYYY-MM-DDTHH:MM:SS(.sss)?Z`.
    let trimmed = s.trim();
    let (date_part, time_part) = match trimmed.split_once('T') {
        Some((d, t)) => (d, Some(t.trim_end_matches('Z'))),
        None => (trimmed, None),
    };
    let mut dp = date_part.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: i64 = dp.next().unwrap_or("1").parse().ok()?;
    let d: i64 = dp.next().unwrap_or("1").parse().ok()?;
    let (mut h, mut min, mut sec, mut ms) = (0i64, 0i64, 0i64, 0i64);
    if let Some(tp) = time_part {
        let mut tparts = tp.split(':');
        h = tparts.next().unwrap_or("0").parse().ok()?;
        min = tparts.next().unwrap_or("0").parse().ok()?;
        if let Some(sec_str) = tparts.next() {
            if let Some((s_int, s_frac)) = sec_str.split_once('.') {
                sec = s_int.parse().ok()?;
                let padded = format!("{s_frac:0<3}");
                ms = padded[..3].parse().ok()?;
            } else {
                sec = sec_str.parse().ok()?;
            }
        }
    }
    let days = days_from_civil(y, m, d);
    Some(((((days * 24 + h) * 60 + min) * 60 + sec) * 1000 + ms) as f64)
}

// ---------------------------------------------------------------------------
// JSON (`JSON.parse` / `JSON.stringify` with cycle detection & reviver/space)
// ---------------------------------------------------------------------------

fn install_json(vm: &mut JsVm) {
    let json = vm.alloc_ordinary_object();
    define_method(vm, json, "parse", json_parse);
    define_method(vm, json, "stringify", json_stringify);
    define_global(vm, "JSON", JsValue::from_object(json));
}

fn json_parse(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let text = vm.value_to_rust_string(arg0(args));
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| vm.make_error("SyntaxError", format!("JSON.parse: {e}")))?;
    let val = serde_to_js(vm, &parsed);
    if let Some(reviver) = args
        .get(1)
        .copied()
        .filter(|v| v.as_object().is_some_and(|h| vm.is_callable_handle(h)))
    {
        let holder = vm.alloc_ordinary_object();
        let empty_k = vm.heap.intern_key("");
        vm.heap
            .object_define_data(holder, empty_k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
        return json_revive(vm, holder, "", reviver);
    }
    Ok(val)
}

fn serde_to_js(vm: &mut JsVm, val: &serde_json::Value) -> JsValue {
    match val {
        serde_json::Value::Null => JsValue::NULL,
        serde_json::Value::Bool(b) => JsValue::from_bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64().and_then(|v| i32::try_from(v).ok()) {
                JsValue::from_i32(i)
            } else {
                JsValue::from_f64(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => JsValue::from_string(vm.heap.alloc_string(s.as_str())),
        serde_json::Value::Array(items) => {
            let arr = vm.alloc_array();
            for item in items {
                let v = serde_to_js(vm, item);
                vm.heap.object_push_element(arr, v);
            }
            JsValue::from_object(arr)
        }
        serde_json::Value::Object(map) => {
            let obj = vm.alloc_ordinary_object();
            for (k, v) in map {
                let key = vm.heap.intern_key(k);
                let jv = serde_to_js(vm, v);
                vm.heap
                    .object_define_data(obj, key, jv, PropertyAttrs::DEFAULT, &mut vm.shapes);
            }
            JsValue::from_object(obj)
        }
    }
}

fn json_revive(
    vm: &mut JsVm,
    holder: HeapHandle,
    name: &str,
    reviver: JsValue,
) -> Result<JsValue, JsException> {
    let name_v = JsValue::from_string(vm.heap.alloc_string(name));
    let val = vm.get_elem(JsValue::from_object(holder), name_v)?;
    if let Some(obj) = val.as_object() {
        if matches!(
            vm.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        ) {
            let len = vm.heap.get_object(obj).map_or(0, |o| o.elements.len());
            for i in 0..len {
                let idx_s = i.to_string();
                let new_v = json_revive(vm, obj, &idx_s, reviver)?;
                if new_v.is_undefined() {
                    if let Some(o) = vm.heap.get_object_mut(obj) {
                        o.elements.delete(i);
                    }
                } else {
                    vm.set_elem(
                        JsValue::from_object(obj),
                        JsValue::from_i32(i as i32),
                        new_v,
                    )?;
                }
            }
        } else {
            let keys = vm.own_enumerable_keys_including_elements(obj);
            for k in keys {
                let new_v = json_revive(vm, obj, &k, reviver)?;
                let pk = vm.heap.intern_key(&k);
                if new_v.is_undefined() {
                    vm.heap.object_delete_own(obj, pk, &mut vm.shapes);
                } else {
                    vm.heap.object_define_data(
                        obj,
                        pk,
                        new_v,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                }
            }
        }
    }
    let name_v = JsValue::from_string(vm.heap.alloc_string(name));
    vm.call_value(reviver, JsValue::from_object(holder), &[name_v, val])
}

fn json_stringify(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let val = arg0(args);
    let indent = match args.get(2).copied() {
        Some(v) if v.is_number() => {
            let n = vm.to_i32(v)?.clamp(0, 10) as usize;
            " ".repeat(n)
        }
        Some(v) if v.is_string() => {
            let s = vm.value_to_rust_string(v);
            s.chars().take(10).collect()
        }
        _ => String::new(),
    };
    let mut stack = Vec::new();
    match stringify_value(vm, val, &indent, 0, &mut stack)? {
        Some(s) => Ok(JsValue::from_string(vm.heap.alloc_string(s))),
        None => Ok(JsValue::UNDEFINED),
    }
}

fn stringify_value(
    vm: &mut JsVm,
    mut val: JsValue,
    indent: &str,
    depth: usize,
    stack: &mut Vec<HeapHandle>,
) -> Result<Option<String>, JsException> {
    // Honor `.toJSON()` hook on objects (e.g. `Date.prototype.toJSON` / custom objects).
    if let Some(h) = val.as_object() {
        if matches!(
            vm.heap.get_object(h).map(|o| &o.kind),
            Some(ObjectKind::Date(_))
        ) {
            val = date_to_iso_string(vm, val, &[])?;
        } else {
            let k_to_json = vm.heap.intern_key("toJSON");
            let to_json = vm.get_property_by_key(h, k_to_json)?;
            if to_json
                .as_object()
                .is_some_and(|fh| vm.is_callable_handle(fh))
            {
                val = vm.call_value(to_json, val, &[])?;
            }
        }
    }

    match val.kind() {
        JsValueKind::Undefined | JsValueKind::Empty | JsValueKind::Symbol(_) => Ok(None),
        JsValueKind::Null => Ok(Some("null".into())),
        JsValueKind::Bool(b) => Ok(Some(if b { "true" } else { "false" }.into())),
        JsValueKind::Int32(i) => Ok(Some(i.to_string())),
        JsValueKind::Float(f) => {
            if f.is_finite() {
                Ok(Some(if f == 0.0 { "0".into() } else { f.to_string() }))
            } else {
                Ok(Some("null".into()))
            }
        }
        JsValueKind::BigInt(_) => {
            Err(vm.type_error("Do not know how to serialize a BigInt in JSON.stringify"))
        }
        JsValueKind::String(h) => {
            let s = vm.heap.materialize_string(h).unwrap_or_default();
            Ok(Some(
                serde_json::to_string(&s).unwrap_or_else(|_| "\"\"".into()),
            ))
        }
        JsValueKind::Object(h) => {
            if vm.is_callable_handle(h) {
                return Ok(None);
            }
            if stack.contains(&h) {
                return Err(vm.type_error("Converting circular structure to JSON"));
            }
            stack.push(h);
            let is_array = matches!(
                vm.heap.get_object(h).map(|o| &o.kind),
                Some(ObjectKind::Array)
            );
            let out = if is_array {
                let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
                if len > 4_000_000 {
                    stack.pop();
                    return Err(vm.range_error("Invalid string length"));
                }
                let mut items = Vec::with_capacity((len as usize).min(16_384));
                for i in 0..len {
                    let elem = vm
                        .heap
                        .get_object(h)
                        .and_then(|o| o.elements.get(i))
                        .unwrap_or(JsValue::UNDEFINED);
                    let rendered = stringify_value(vm, elem, indent, depth + 1, stack)?
                        .unwrap_or_else(|| "null".into());
                    items.push(rendered);
                }
                if items.is_empty() {
                    "[]".into()
                } else if indent.is_empty() {
                    format!("[{}]", items.join(","))
                } else {
                    let pad = indent.repeat(depth + 1);
                    let close = indent.repeat(depth);
                    format!("[\n{pad}{}\n{close}]", items.join(&format!(",\n{pad}")))
                }
            } else {
                let keys = vm.own_enumerable_keys_including_elements(h);
                let mut pairs = Vec::new();
                for k in keys {
                    let kv = JsValue::from_string(vm.heap.alloc_string(k.as_str()));
                    let pv = vm.get_elem(JsValue::from_object(h), kv)?;
                    if let Some(sv) = stringify_value(vm, pv, indent, depth + 1, stack)? {
                        let jk = serde_json::to_string(&k).unwrap_or_else(|_| "\"\"".into());
                        if indent.is_empty() {
                            pairs.push(format!("{jk}:{sv}"));
                        } else {
                            pairs.push(format!("{jk}: {sv}"));
                        }
                    }
                }
                if pairs.is_empty() {
                    "{}".into()
                } else if indent.is_empty() {
                    format!("{{{}}}", pairs.join(","))
                } else {
                    let pad = indent.repeat(depth + 1);
                    let close = indent.repeat(depth);
                    format!("{{\n{pad}{}\n{close}}}", pairs.join(&format!(",\n{pad}")))
                }
            };
            stack.pop();
            Ok(Some(out))
        }
    }
}

// ---------------------------------------------------------------------------
// Extended Array & Object methods
// ---------------------------------------------------------------------------

fn install_extended_array_and_object(vm: &mut JsVm) {
    let ap = vm.array_proto;
    let ap_methods: &[(&str, u32, crate::vm::NativeFnPtr)] = &[
        ("concat", 1, array_concat),
        ("reverse", 0, array_reverse),
        ("sort", 1, array_sort),
        ("splice", 2, array_splice),
        ("shift", 0, array_shift),
        ("unshift", 1, array_unshift),
        ("find", 1, array_find),
        ("findIndex", 1, array_find_index),
        ("findLast", 1, array_find_last),
        ("findLastIndex", 1, array_find_last_index),
        ("reduceRight", 1, array_reduce_right),
        ("lastIndexOf", 1, array_last_index_of),
        ("copyWithin", 2, array_copy_within),
        ("toReversed", 0, array_to_reversed),
        ("toSorted", 1, array_to_sorted),
        ("toSpliced", 2, array_to_spliced),
        ("with", 2, array_with),
        ("some", 1, array_some),
        ("every", 1, array_every),
        ("flat", 0, array_flat),
        ("flatMap", 1, array_flat_map),
        ("fill", 1, array_fill),
        ("at", 1, array_at),
    ];
    for &(name, len, f) in ap_methods {
        define_method_len(vm, ap, name, len, f);
    }

    let k_arr = vm.heap.intern_key("Array");
    if let Some(h) = vm
        .get_property_by_key(vm.global, k_arr)
        .ok()
        .and_then(|v| v.as_object())
    {
        define_method_len(vm, h, "from", 1, array_from);
        define_method_len(vm, h, "of", 0, array_of);
    }

    let k_obj = vm.heap.intern_key("Object");
    if let Some(h) = vm
        .get_property_by_key(vm.global, k_obj)
        .ok()
        .and_then(|v| v.as_object())
    {
        let methods: &[(&str, u32, crate::vm::NativeFnPtr)] = &[
            ("defineProperty", 3, object_define_property),
            ("defineProperties", 2, object_define_properties),
            ("getOwnPropertyDescriptor", 2, object_get_own_descriptor),
            ("getOwnPropertyDescriptors", 1, object_get_own_descriptors),
            ("getOwnPropertyNames", 1, object_get_own_property_names),
            ("getOwnPropertySymbols", 1, object_get_own_property_symbols),
            ("fromEntries", 1, object_from_entries),
            ("isFrozen", 1, object_is_frozen),
            ("seal", 1, object_seal),
            ("isSealed", 1, object_is_sealed),
            ("isExtensible", 1, object_is_extensible),
            ("preventExtensions", 1, object_prevent_extensions),
            ("groupBy", 2, object_group_by),
        ];
        for &(name, len, f) in methods {
            define_method_len(vm, h, name, len, f);
        }
    }

    let k_ref = vm.heap.intern_key("Reflect");
    if let Some(h) = vm
        .get_property_by_key(vm.global, k_ref)
        .ok()
        .and_then(|v| v.as_object())
    {
        define_method_len(vm, h, "defineProperty", 3, reflect_define_property);
        define_method_len(
            vm,
            h,
            "getOwnPropertyDescriptor",
            2,
            reflect_get_own_descriptor,
        );
    }
}

fn clamp_rel_f64(rel: f64, len: u64) -> u64 {
    if rel == f64::NEG_INFINITY {
        0
    } else if rel < 0.0 {
        (len as f64 + rel).max(0.0) as u64
    } else if rel >= len as f64 {
        len
    } else {
        rel as u64
    }
}

fn require_callable_cb(vm: &mut JsVm, cb: JsValue) -> Result<(), JsException> {
    if cb.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        Ok(())
    } else {
        Err(vm.type_error("Callback is not a function"))
    }
}

fn alloc_from_target(
    vm: &mut JsVm,
    this_val: JsValue,
    len: Option<u64>,
) -> Result<HeapHandle, JsException> {
    if this_val
        .as_object()
        .is_some_and(|h| vm.is_constructor_handle(h))
    {
        let args = len.map(|l| [JsValue::from_f64(l as f64)]);
        let slice: &[JsValue] = args.as_ref().map_or(&[], |a| a.as_slice());
        return vm
            .construct_value(this_val, slice, this_val)?
            .as_object()
            .ok_or_else(|| vm.type_error("Constructor returned non-object"));
    }
    let a = vm.alloc_array();
    if let Some(l) = len.filter(|&l| l <= u64::from(u32::MAX)) {
        if let Some(o) = vm.heap.get_object_mut(a) {
            o.elements.set_len(l as u32);
        }
    }
    Ok(a)
}

fn move_index(vm: &mut JsVm, h: HeapHandle, from: u64, to: u64) -> Result<(), JsException> {
    if vm.array_has_index(h, from)? {
        let v = vm.array_get_index(h, from)?;
        vm.array_set_index_strict(h, to, v)
    } else {
        vm.array_delete_index_strict(h, to)
    }
}

fn parse_splice_delete_count(
    vm: &mut JsVm,
    args: &[JsValue],
    len: u64,
    start: u64,
) -> Result<u64, JsException> {
    if args.is_empty() {
        Ok(0)
    } else if args.len() == 1 {
        Ok(len - start)
    } else {
        let dc = vm.to_integer_or_infinity(args[1])?;
        Ok(if dc <= 0.0 {
            0
        } else {
            dc.min((len - start) as f64) as u64
        })
    }
}

fn check_iter_limit(vm: &mut JsVm, n: u64) -> Result<(), JsException> {
    if n >= vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000) {
        Err(vm.range_error("Script execution iteration limit exceeded"))
    } else {
        Ok(())
    }
}

fn array_from(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let src = arg0(args);
    if src.is_null() || src.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    let map_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let map_fn = if map_arg.is_undefined() {
        None
    } else {
        require_callable_cb(vm, map_arg)?;
        Some(map_arg)
    };
    let this_arg = args.get(2).copied().unwrap_or(JsValue::UNDEFINED);
    let iter_method = if let Some(h) = src.as_object() {
        vm.get_property_by_key(h, vm.key_symbol_iterator)?
    } else if src.is_string() {
        vm.get_property_by_key(vm.string_proto, vm.key_symbol_iterator)?
    } else {
        JsValue::UNDEFINED
    };
    if !iter_method.is_undefined() && !iter_method.is_null() {
        require_callable_cb(vm, iter_method)?;
        let out = alloc_from_target(vm, this_val, None)?;
        let items = vm.collect_iterable(src)?;
        let count = items.len() as u64;
        for (i, mut v) in items.into_iter().enumerate() {
            if let Some(cb) = map_fn {
                v = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(i as f64)])?;
            }
            vm.array_create_data_prop(out, i as u64, v)?;
        }
        vm.array_set_length(out, count)?;
        return Ok(JsValue::from_object(out));
    }
    let src_h = vm.to_object(src)?;
    let len = vm.array_get_len(src_h)?;
    check_iter_limit(vm, len)?;
    let out = alloc_from_target(vm, this_val, Some(len))?;
    for i in 0..len {
        let mut v = vm.array_get_index(src_h, i)?;
        if let Some(cb) = map_fn {
            v = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(i as f64)])?;
        }
        vm.array_create_data_prop(out, i, v)?;
    }
    vm.array_set_length(out, len)?;
    Ok(JsValue::from_object(out))
}

fn array_of(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let len = args.len() as u64;
    let out = alloc_from_target(vm, this_val, Some(len))?;
    for (i, &a) in args.iter().enumerate() {
        vm.array_create_data_prop(out, i as u64, a)?;
    }
    vm.array_set_length(out, len)?;
    Ok(JsValue::from_object(out))
}

fn is_concat_spreadable(vm: &mut JsVm, val: JsValue) -> Result<bool, JsException> {
    let Some(h) = val.as_object() else {
        return Ok(false);
    };
    let spreadable = vm.get_property_by_key(h, vm.key_symbol_is_concat_spreadable)?;
    if !spreadable.is_undefined() {
        return Ok(vm.to_boolean(spreadable));
    }
    vm.is_array_value(val)
        .map_err(|_| vm.type_error("Proxy has been revoked"))
}

fn array_concat(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let out = vm.array_species_create(h, 0)?;
    let mut n = 0u64;
    let mut all_items = Vec::with_capacity(1 + args.len());
    all_items.push(JsValue::from_object(h));
    all_items.extend_from_slice(args);
    for item in all_items {
        if is_concat_spreadable(vm, item)? {
            let ih = item.as_object().unwrap();
            let len = vm.array_get_len(ih)?;
            if n + len > 9_007_199_254_740_991 {
                return Err(vm.type_error("Array length exceeds 2^53 - 1"));
            }
            if let Some(indices) = vm.sparse_present_indices_if_safe(ih, len) {
                for i in indices {
                    if vm.array_has_index(ih, i)? {
                        let v = vm.array_get_index(ih, i)?;
                        vm.array_create_data_prop(out, n + i, v)?;
                    }
                }
                n += len;
            } else {
                let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
                for i in 0..len {
                    if i >= limit {
                        return Err(vm.range_error("Script execution iteration limit exceeded"));
                    }
                    if vm.array_has_index(ih, i)? {
                        let v = vm.array_get_index(ih, i)?;
                        vm.array_create_data_prop(out, n, v)?;
                    }
                    n += 1;
                }
            }
        } else {
            if n >= 9_007_199_254_740_991 {
                return Err(vm.type_error("Array length exceeds 2^53 - 1"));
            }
            vm.array_create_data_prop(out, n, item)?;
            n += 1;
        }
    }
    vm.array_set_length(out, n)?;
    Ok(JsValue::from_object(out))
}

fn array_reverse(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    if len <= 1 {
        return Ok(JsValue::from_object(h));
    }
    let middle = len / 2;
    if middle > 1_000_000 {
        return Err(vm.range_error("Script execution iteration limit exceeded"));
    }
    for lower in 0..middle {
        let upper = len - lower - 1;
        let lower_exists = vm.array_has_index(h, lower)?;
        let lower_val = if lower_exists {
            Some(vm.array_get_index(h, lower)?)
        } else {
            None
        };
        let upper_exists = vm.array_has_index(h, upper)?;
        let upper_val = if upper_exists {
            Some(vm.array_get_index(h, upper)?)
        } else {
            None
        };
        match (lower_val, upper_val) {
            (Some(lv), Some(uv)) => {
                vm.array_set_index_strict(h, lower, uv)?;
                vm.array_set_index_strict(h, upper, lv)?;
            }
            (None, Some(uv)) => {
                vm.array_set_index_strict(h, lower, uv)?;
                vm.array_delete_index_strict(h, upper)?;
            }
            (Some(lv), None) => {
                vm.array_delete_index_strict(h, lower)?;
                vm.array_set_index_strict(h, upper, lv)?;
            }
            (None, None) => {}
        }
    }
    Ok(JsValue::from_object(h))
}

fn sort_js_values(
    vm: &mut JsVm,
    vals: &mut [JsValue],
    cmp_fn: Option<JsValue>,
) -> Result<(), JsException> {
    // Move all `undefined` values to the end (preserving relative order), then stable-sort non-undefined.
    let mut defined = Vec::with_capacity(vals.len());
    let mut undef_count = 0usize;
    for &v in vals.iter() {
        if v.is_undefined() {
            undef_count += 1;
        } else {
            defined.push(v);
        }
    }
    // Stable insertion/merge sort for JS comparator callbacks (reentrant-safe).
    for i in 1..defined.len() {
        let mut j = i;
        while j > 0 {
            let order = if let Some(cb) = cmp_fn {
                let r = vm.call_value(cb, JsValue::UNDEFINED, &[defined[j - 1], defined[j]])?;
                let n = vm.to_number(r)?;
                if n.is_nan() {
                    0.0
                } else {
                    n
                }
            } else {
                let a = vm.to_js_string(defined[j - 1])?;
                let b = vm.to_js_string(defined[j])?;
                match a.encode_utf16().cmp(b.encode_utf16()) {
                    std::cmp::Ordering::Less => -1.0,
                    std::cmp::Ordering::Equal => 0.0,
                    std::cmp::Ordering::Greater => 1.0,
                }
            };
            if order > 0.0 {
                defined.swap(j - 1, j);
                j -= 1;
            } else {
                break;
            }
        }
    }
    let def_len = defined.len();
    vals[..def_len].copy_from_slice(&defined);
    for slot in &mut vals[def_len..def_len + undef_count] {
        *slot = JsValue::UNDEFINED;
    }
    Ok(())
}

fn array_sort(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let cmp_arg = arg0(args);
    let cmp_fn = if cmp_arg.is_undefined() {
        None
    } else {
        require_callable_cb(vm, cmp_arg)?;
        Some(cmp_arg)
    };
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let mut vals = Vec::new();
    if let Some(indices) = vm.sparse_present_indices_if_safe(h, len) {
        for i in indices {
            if vm.array_has_index(h, i)? {
                vals.push(vm.array_get_index(h, i)?);
            }
        }
    } else {
        if len > 1_000_000 {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        for i in 0..len {
            if vm.array_has_index(h, i)? {
                vals.push(vm.array_get_index(h, i)?);
            }
        }
    }
    sort_js_values(vm, &mut vals, cmp_fn)?;
    let item_count = vals.len() as u64;
    for (i, v) in vals.into_iter().enumerate() {
        vm.array_set_index_strict(h, i as u64, v)?;
    }
    if len <= 1_000_000 {
        for j in item_count..len {
            vm.array_delete_index_strict(h, j)?;
        }
    } else if let Some(indices) = vm.sparse_present_indices_if_safe(h, len) {
        for j in indices {
            if j >= item_count {
                vm.array_delete_index_strict(h, j)?;
            }
        }
    }
    Ok(JsValue::from_object(h))
}

fn array_splice(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let start = match args.first().copied() {
        Some(v) => clamp_rel_f64(vm.to_integer_or_infinity(v)?, len),
        None => 0,
    };
    let insert_count = args.len().saturating_sub(2) as u64;
    let delete_count = parse_splice_delete_count(vm, args, len, start)?;
    if len - delete_count + insert_count > 9_007_199_254_740_991 {
        return Err(vm.type_error("Array length exceeds 2^53 - 1"));
    }
    let out = vm.array_species_create(h, delete_count)?;
    check_iter_limit(vm, delete_count.max(len - start))?;
    for k in 0..delete_count {
        if vm.array_has_index(h, start + k)? {
            let v = vm.array_get_index(h, start + k)?;
            vm.array_create_data_prop(out, k, v)?;
        }
    }
    vm.array_set_length(out, delete_count)?;
    if insert_count < delete_count {
        for k in start..(len - delete_count) {
            move_index(vm, h, k + delete_count, k + insert_count)?;
        }
        for k in ((len - delete_count + insert_count)..len).rev() {
            vm.array_delete_index_strict(h, k)?;
        }
    } else if insert_count > delete_count {
        for k in (start..(len - delete_count)).rev() {
            move_index(vm, h, k + delete_count, k + insert_count)?;
        }
    }
    for (i, &item) in args.iter().skip(2).enumerate() {
        vm.array_set_index_strict(h, start + i as u64, item)?;
    }
    vm.array_set_length(h, len - delete_count + insert_count)?;
    Ok(JsValue::from_object(out))
}

fn array_shift(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    if len == 0 {
        vm.array_set_length(h, 0)?;
        return Ok(JsValue::UNDEFINED);
    }
    let first = vm.array_get_index(h, 0)?;
    check_iter_limit(vm, len)?;
    for k in 1..len {
        move_index(vm, h, k, k - 1)?;
    }
    vm.array_delete_index_strict(h, len - 1)?;
    vm.array_set_length(h, len - 1)?;
    Ok(first)
}

fn array_unshift(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let arg_count = args.len() as u64;
    if arg_count > 0 {
        if len + arg_count > 9_007_199_254_740_991 {
            return Err(vm.type_error("Array length exceeds 2^53 - 1"));
        }
        check_iter_limit(vm, len)?;
        for k in (0..len).rev() {
            move_index(vm, h, k, k + arg_count)?;
        }
        for (j, &a) in args.iter().enumerate() {
            vm.array_set_index_strict(h, j as u64, a)?;
        }
    }
    let new_len = len + arg_count;
    vm.array_set_length(h, new_len)?;
    Ok(JsValue::from_f64(new_len as f64))
}

fn array_find_impl(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    return_index: bool,
    reverse: bool,
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let cb = arg0(args);
    require_callable_cb(vm, cb)?;
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let obj_val = JsValue::from_object(h);
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
    for step in 0..len {
        if step >= limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        let i = if reverse { len - 1 - step } else { step };
        let v = vm.array_get_index(h, i)?;
        let ok = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(i as f64), obj_val])?;
        if vm.to_boolean(ok) {
            return Ok(if return_index {
                JsValue::from_f64(i as f64)
            } else {
                v
            });
        }
    }
    Ok(if return_index {
        JsValue::from_i32(-1)
    } else {
        JsValue::UNDEFINED
    })
}

fn array_find(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    array_find_impl(vm, this_val, args, false, false)
}
fn array_find_index(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    array_find_impl(vm, this_val, args, true, false)
}
fn array_find_last(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    array_find_impl(vm, this_val, args, false, true)
}
fn array_find_last_index(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    array_find_impl(vm, this_val, args, true, true)
}

fn array_quantifier(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    stop_on: bool,
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let cb = arg0(args);
    require_callable_cb(vm, cb)?;
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let obj_val = JsValue::from_object(h);
    let sparse = vm.sparse_present_indices_if_safe(h, len);
    let count = sparse.as_ref().map_or(len, |v| v.len() as u64);
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
    for step in 0..count {
        if step >= limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        let i = sparse.as_ref().map_or(step, |v| v[step as usize]);
        if vm.array_has_index(h, i)? {
            let v = vm.array_get_index(h, i)?;
            let ok = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(i as f64), obj_val])?;
            if vm.to_boolean(ok) == stop_on {
                return Ok(JsValue::from_bool(stop_on));
            }
        }
    }
    Ok(JsValue::from_bool(!stop_on))
}

fn array_some(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    array_quantifier(vm, this_val, args, true)
}

fn array_every(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    array_quantifier(vm, this_val, args, false)
}

#[allow(clippy::too_many_arguments)]
fn flatten_into_array(
    vm: &mut JsVm,
    target: HeapHandle,
    source: HeapHandle,
    source_len: u64,
    mut target_idx: u64,
    depth: f64,
    mapper: Option<(JsValue, JsValue)>,
) -> Result<u64, JsException> {
    let source_val = JsValue::from_object(source);
    let indices_opt = vm.sparse_present_indices_if_safe(source, source_len);
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
    let iter_len = indices_opt.as_ref().map_or(source_len, |v| v.len() as u64);
    for step in 0..iter_len {
        if step >= limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        let i = indices_opt.as_ref().map_or(step, |v| v[step as usize]);
        if !vm.array_has_index(source, i)? {
            continue;
        }
        let mut elem = vm.array_get_index(source, i)?;
        if let Some((cb, this_arg)) = mapper {
            elem = vm.call_value(
                cb,
                this_arg,
                &[elem, JsValue::from_f64(i as f64), source_val],
            )?;
        }
        let should_flatten = if depth > 0.0 {
            vm.is_array_value(elem)
                .map_err(|_| vm.type_error("Proxy has been revoked"))?
        } else {
            false
        };
        if should_flatten {
            let sub = elem.as_object().unwrap();
            let sub_len = vm.array_get_len(sub)?;
            let next_depth = if depth.is_infinite() {
                depth
            } else {
                depth - 1.0
            };
            target_idx =
                flatten_into_array(vm, target, sub, sub_len, target_idx, next_depth, None)?;
        } else {
            if target_idx >= 9_007_199_254_740_991 {
                return Err(vm.type_error("Array length exceeds 2^53 - 1"));
            }
            vm.array_create_data_prop(target, target_idx, elem)?;
            target_idx += 1;
        }
    }
    Ok(target_idx)
}

fn array_flat(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let depth = match args.first().copied() {
        Some(v) if !v.is_undefined() => {
            let d = vm.to_integer_or_infinity(v)?;
            if d < 0.0 {
                0.0
            } else {
                d
            }
        }
        _ => 1.0,
    };
    let out = vm.array_species_create(h, 0)?;
    let _ = flatten_into_array(vm, out, h, len, 0, depth, None)?;
    Ok(JsValue::from_object(out))
}

fn array_flat_map(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let cb = arg0(args);
    require_callable_cb(vm, cb)?;
    let this_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let out = vm.array_species_create(h, 0)?;
    let _ = flatten_into_array(vm, out, h, len, 0, 1.0, Some((cb, this_arg)))?;
    Ok(JsValue::from_object(out))
}

fn array_fill(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let val = arg0(args);
    let start = match args.get(1).copied() {
        Some(v) if !v.is_undefined() => clamp_rel_f64(vm.to_integer_or_infinity(v)?, len),
        _ => 0,
    };
    let end = match args.get(2).copied() {
        Some(v) if !v.is_undefined() => clamp_rel_f64(vm.to_integer_or_infinity(v)?, len),
        _ => len,
    };
    if end.saturating_sub(start) > 1_000_000 {
        return Err(vm.range_error("Script execution iteration limit exceeded"));
    }
    for i in start..end {
        vm.array_set_index_strict(h, i, val)?;
    }
    Ok(JsValue::from_object(h))
}

fn array_at(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let rel = vm.to_integer_or_infinity(arg0(args))?;
    let k = if rel >= 0.0 { rel } else { len as f64 + rel };
    if k < 0.0 || k >= len as f64 {
        return Ok(JsValue::UNDEFINED);
    }
    vm.array_get_index(h, k as u64)
}

fn array_reduce_right(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let cb = arg0(args);
    require_callable_cb(vm, cb)?;
    let obj_val = JsValue::from_object(h);
    let mut acc = if args.len() >= 2 { Some(args[1]) } else { None };
    let sparse = vm.sparse_present_indices_if_safe(h, len);
    let count = sparse.as_ref().map_or(len, |v| v.len() as u64);
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
    for step in 0..count {
        if step >= limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        let i = sparse
            .as_ref()
            .map_or(len - 1 - step, |v| v[(count - 1 - step) as usize]);
        if vm.array_has_index(h, i)? {
            let elem = vm.array_get_index(h, i)?;
            acc = Some(if let Some(cur) = acc {
                vm.call_value(
                    cb,
                    JsValue::UNDEFINED,
                    &[cur, elem, JsValue::from_f64(i as f64), obj_val],
                )?
            } else {
                elem
            });
        }
    }
    acc.ok_or_else(|| vm.type_error("Reduce of empty array with no initial value"))
}

fn array_last_index_of(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    if len == 0 {
        return Ok(JsValue::from_i32(-1));
    }
    let target = arg0(args);
    let n = if args.len() >= 2 {
        vm.to_integer_or_infinity(args[1])?
    } else {
        (len - 1) as f64
    };
    let start_f = if n >= 0.0 {
        n.min((len - 1) as f64)
    } else {
        len as f64 + n
    };
    if start_f < 0.0 {
        return Ok(JsValue::from_i32(-1));
    }
    let start = start_f as u64;
    let sparse = vm.sparse_present_indices_if_safe(h, len);
    let count = sparse.as_ref().map_or(start + 1, |v| v.len() as u64);
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000);
    for step in 0..count {
        if step >= limit {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
        let i = sparse
            .as_ref()
            .map_or(start - step, |v| v[(count - 1 - step) as usize]);
        if i <= start && vm.array_has_index(h, i)? {
            let v = vm.array_get_index(h, i)?;
            if vm.strict_eq(v, target) {
                return Ok(JsValue::from_f64(i as f64));
            }
        }
    }
    Ok(JsValue::from_i32(-1))
}

fn array_copy_within(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let mut to = clamp_rel_f64(vm.to_integer_or_infinity(arg0(args))?, len);
    let mut from = clamp_rel_f64(
        args.get(1)
            .map(|&v| vm.to_integer_or_infinity(v))
            .transpose()?
            .unwrap_or(0.0),
        len,
    );
    let fin = match args.get(2).copied() {
        Some(v) if !v.is_undefined() => clamp_rel_f64(vm.to_integer_or_infinity(v)?, len),
        _ => len,
    };
    let mut count = fin.saturating_sub(from).min(len.saturating_sub(to));
    if count > 1_000_000 {
        return Err(vm.range_error("Script execution iteration limit exceeded"));
    }
    let dir: i64 = if from < to && to < from + count {
        from = from + count - 1;
        to = to + count - 1;
        -1
    } else {
        1
    };
    while count > 0 {
        move_index(vm, h, from, to)?;
        from = (from as i64 + dir) as u64;
        to = (to as i64 + dir) as u64;
        count -= 1;
    }
    Ok(JsValue::from_object(h))
}

fn array_to_reversed(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    check_iter_limit(vm, len)?;
    let out = vm.alloc_array();
    for k in 0..len {
        let v = vm.array_get_index(h, len - 1 - k)?;
        vm.heap.object_push_element(out, v);
    }
    Ok(JsValue::from_object(out))
}

fn array_to_sorted(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let cmp_arg = arg0(args);
    let cmp_fn = if cmp_arg.is_undefined() {
        None
    } else {
        require_callable_cb(vm, cmp_arg)?;
        Some(cmp_arg)
    };
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    check_iter_limit(vm, len)?;
    let mut vals = Vec::with_capacity(len as usize);
    for i in 0..len {
        vals.push(vm.array_get_index(h, i)?);
    }
    sort_js_values(vm, &mut vals, cmp_fn)?;
    let out = vm.alloc_array();
    for v in vals {
        vm.heap.object_push_element(out, v);
    }
    Ok(JsValue::from_object(out))
}

fn array_to_spliced(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let start = match args.first().copied() {
        Some(v) => clamp_rel_f64(vm.to_integer_or_infinity(v)?, len),
        None => 0,
    };
    let insert = if args.len() > 2 { &args[2..] } else { &[] };
    let skip_count = parse_splice_delete_count(vm, args, len, start)?;
    check_iter_limit(vm, len - skip_count + insert.len() as u64)?;
    let out = vm.alloc_array();
    for i in 0..start {
        let v = vm.array_get_index(h, i)?;
        vm.heap.object_push_element(out, v);
    }
    for &item in insert {
        vm.heap.object_push_element(out, item);
    }
    for i in (start + skip_count)..len {
        let v = vm.array_get_index(h, i)?;
        vm.heap.object_push_element(out, v);
    }
    Ok(JsValue::from_object(out))
}

fn array_with(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let h = vm.to_object(this_val)?;
    let len = vm.array_get_len(h)?;
    let rel = vm.to_integer_or_infinity(arg0(args))?;
    let actual_f = if rel >= 0.0 { rel } else { len as f64 + rel };
    if actual_f < 0.0 || actual_f >= len as f64 {
        return Err(vm.range_error("Invalid index in Array.prototype.with"));
    }
    let actual = actual_f as u64;
    if len > 1_000_000 {
        return Err(vm.range_error("Invalid array length"));
    }
    let new_val = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let out = vm.alloc_array();
    for i in 0..len {
        let v = if i == actual {
            new_val
        } else {
            vm.array_get_index(h, i)?
        };
        vm.heap.object_push_element(out, v);
    }
    Ok(JsValue::from_object(out))
}

fn read_opt_bool(
    vm: &mut JsVm,
    desc: HeapHandle,
    key: crate::PropertyKey,
) -> Result<Option<bool>, JsException> {
    if vm.has_property_by_key(desc, key)? {
        let v = vm.get_property_by_key(desc, key)?;
        Ok(Some(vm.to_boolean(v)))
    } else {
        Ok(None)
    }
}

fn read_opt_accessor(
    vm: &mut JsVm,
    desc: HeapHandle,
    key: crate::PropertyKey,
    label: &str,
) -> Result<Option<Option<HeapHandle>>, JsException> {
    if !vm.has_property_by_key(desc, key)? {
        return Ok(None);
    }
    let v = vm.get_property_by_key(desc, key)?;
    if v.is_undefined() {
        Ok(Some(None))
    } else if let Some(h) = v.as_object().filter(|&h| vm.is_callable_handle(h)) {
        Ok(Some(Some(h)))
    } else {
        Err(vm.type_error(format!("{label} must be a function")))
    }
}

fn build_attrs(w: bool, e: bool, c: bool) -> PropertyAttrs {
    let mut attrs = PropertyAttrs::empty();
    attrs.set(PropertyAttrs::WRITABLE, w);
    attrs.set(PropertyAttrs::ENUMERABLE, e);
    attrs.set(PropertyAttrs::CONFIGURABLE, c);
    attrs
}

fn define_property_checked(
    vm: &mut JsVm,
    obj: HeapHandle,
    prop_name: &str,
    desc: HeapHandle,
) -> Result<bool, JsException> {
    let (k_w, k_e, k_c) = (
        vm.heap.intern_key("writable"),
        vm.heap.intern_key("enumerable"),
        vm.heap.intern_key("configurable"),
    );
    let desc_e = read_opt_bool(vm, desc, k_e)?;
    let desc_c = read_opt_bool(vm, desc, k_c)?;
    let desc_w = read_opt_bool(vm, desc, k_w)?;
    let desc_v = if vm.has_property_by_key(desc, vm.key_value)? {
        Some(vm.get_property_by_key(desc, vm.key_value)?)
    } else {
        None
    };
    let desc_get = read_opt_accessor(vm, desc, vm.key_get, "Getter")?;
    let desc_set = read_opt_accessor(vm, desc, vm.key_set, "Setter")?;

    let (is_data_desc, is_acc_desc) = (
        desc_w.is_some() || desc_v.is_some(),
        desc_get.is_some() || desc_set.is_some(),
    );
    if is_data_desc && is_acc_desc {
        return Err(vm.type_error(
            "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute",
        ));
    }

    let is_arr = matches!(
        vm.heap.get_object(obj).map(|o| &o.kind),
        Some(ObjectKind::Array)
    );
    if prop_name == "length" && is_arr {
        if desc_c == Some(true) || desc_e == Some(true) || is_acc_desc {
            return Ok(false);
        }
        let cur_w = vm
            .heap
            .object_get_own(obj, vm.key_length, &vm.shapes)
            .is_none_or(|p| p.attrs.contains(PropertyAttrs::WRITABLE));
        if !cur_w && desc_w == Some(true) {
            return Ok(false);
        }
        if let Some(v) = desc_v {
            if !vm.set_array_length_checked(obj, v)? {
                return Ok(false);
            }
        }
        if desc_w == Some(false) {
            let cur_len = vm.heap.get_object(obj).map_or(0, |o| o.elements.len());
            vm.heap.object_define_data(
                obj,
                vm.key_length,
                JsValue::from_f64(f64::from(cur_len)),
                PropertyAttrs::empty(),
                &mut vm.shapes,
            );
        }
        return Ok(true);
    }

    let key = vm.heap.intern_key(prop_name);
    if is_arr {
        if let Some(idx) = crate::vm::parse_array_index_str(prop_name) {
            let cur_len = vm.heap.get_object(obj).map_or(0, |o| o.elements.len());
            let len_writable = vm
                .heap
                .object_get_own(obj, vm.key_length, &vm.shapes)
                .is_none_or(|p| p.attrs.contains(PropertyAttrs::WRITABLE));
            if idx >= cur_len && !len_writable {
                return Ok(false);
            }
            let has_dict_own = vm.heap.object_get_own(obj, key, &vm.shapes).is_some();
            if !has_dict_own {
                let elem_val = vm.heap.get_object(obj).and_then(|o| o.elements.get(idx));
                let default_data = !is_acc_desc
                    && desc_w != Some(false)
                    && desc_c != Some(false)
                    && desc_e != Some(false);
                if default_data
                    && (elem_val.is_some()
                        || (desc_w == Some(true) && desc_c == Some(true) && desc_e == Some(true)))
                {
                    if elem_val.is_none() && !vm.heap.get_object(obj).is_some_and(|o| o.extensible)
                    {
                        return Ok(false);
                    }
                    let val = desc_v.unwrap_or_else(|| elem_val.unwrap_or(JsValue::UNDEFINED));
                    if let Some(o) = vm.heap.get_object_mut(obj) {
                        o.elements.set(idx, val);
                    }
                    vm.heap.write_barrier(obj, val);
                    return Ok(true);
                }
                // Migrate existing fast element to dictionary descriptor before applying non-default attrs.
                if let Some(existing) = elem_val {
                    if let Some(o) = vm.heap.get_object_mut(obj) {
                        o.elements.delete(idx);
                    }
                    vm.heap.object_define_data(
                        obj,
                        key,
                        existing,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                } else if idx >= cur_len {
                    if !vm.heap.get_object(obj).is_some_and(|o| o.extensible) {
                        return Ok(false);
                    }
                    if let Some(o) = vm.heap.get_object_mut(obj) {
                        o.elements.set_len(idx + 1);
                    }
                }
            }
        }
    }

    let Some(cur) = vm.heap.object_get_own(obj, key, &vm.shapes) else {
        if !vm.heap.get_object(obj).is_some_and(|o| o.extensible) {
            return Ok(false);
        }
        let attrs = build_attrs(
            !is_acc_desc && desc_w.unwrap_or(false),
            desc_e.unwrap_or(false),
            desc_c.unwrap_or(false),
        );
        if is_acc_desc {
            vm.heap.object_define_accessor(
                obj,
                key,
                desc_get.flatten(),
                desc_set.flatten(),
                attrs,
                &mut vm.shapes,
            );
        } else {
            vm.heap.object_define_data(
                obj,
                key,
                desc_v.unwrap_or(JsValue::UNDEFINED),
                attrs,
                &mut vm.shapes,
            );
        }
        return Ok(true);
    };

    // Existing own property validation (ECMA-262 §10.1.6.3 steps 4–10).
    let cur_c = cur.attrs.contains(PropertyAttrs::CONFIGURABLE);
    let cur_e = cur.attrs.contains(PropertyAttrs::ENUMERABLE);
    let cur_w = cur.attrs.contains(PropertyAttrs::WRITABLE);
    let cur_is_acc = cur.attrs.contains(PropertyAttrs::ACCESSOR);
    if !cur_c && (desc_c == Some(true) || desc_e.is_some_and(|e| e != cur_e)) {
        return Ok(false);
    }

    if !is_data_desc && !is_acc_desc {
        let mut attrs = cur.attrs;
        if let Some(e) = desc_e {
            attrs.set(PropertyAttrs::ENUMERABLE, e);
        }
        if let Some(c) = desc_c {
            attrs.set(PropertyAttrs::CONFIGURABLE, c);
        }
        if cur_is_acc {
            vm.heap
                .object_define_accessor(obj, key, cur.getter, cur.setter, attrs, &mut vm.shapes);
        } else {
            vm.heap
                .object_define_data(obj, key, cur.value, attrs, &mut vm.shapes);
        }
        return Ok(true);
    }

    if cur_is_acc != is_acc_desc {
        if !cur_c {
            return Ok(false);
        }
        let attrs = build_attrs(
            !is_acc_desc && desc_w.unwrap_or(false),
            desc_e.unwrap_or(cur_e),
            desc_c.unwrap_or(cur_c),
        );
        if is_acc_desc {
            vm.heap.object_define_accessor(
                obj,
                key,
                desc_get.flatten(),
                desc_set.flatten(),
                attrs,
                &mut vm.shapes,
            );
        } else {
            vm.heap.object_define_data(
                obj,
                key,
                desc_v.unwrap_or(JsValue::UNDEFINED),
                attrs,
                &mut vm.shapes,
            );
        }
        return Ok(true);
    }

    if !cur_is_acc {
        if !cur_c && !cur_w {
            if desc_w == Some(true) {
                return Ok(false);
            }
            if let Some(v) = desc_v {
                let same = match (v.as_number(), cur.value.as_number()) {
                    (Some(a), Some(b)) => {
                        (a.is_nan() && b.is_nan()) || (a == b && a.to_bits() == b.to_bits())
                    }
                    _ => vm.strict_eq(v, cur.value),
                };
                if !same {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        let attrs = build_attrs(
            desc_w.unwrap_or(cur_w),
            desc_e.unwrap_or(cur_e),
            desc_c.unwrap_or(cur_c),
        );
        vm.heap
            .object_define_data(obj, key, desc_v.unwrap_or(cur.value), attrs, &mut vm.shapes);
    } else {
        if !cur_c {
            if desc_get.is_some_and(|g| g != cur.getter)
                || desc_set.is_some_and(|s| s != cur.setter)
            {
                return Ok(false);
            }
            return Ok(true);
        }
        let attrs = build_attrs(false, desc_e.unwrap_or(cur_e), desc_c.unwrap_or(cur_c));
        vm.heap.object_define_accessor(
            obj,
            key,
            desc_get.unwrap_or(cur.getter),
            desc_set.unwrap_or(cur.setter),
            attrs,
            &mut vm.shapes,
        );
    }
    Ok(true)
}

fn object_define_property(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let obj_val = arg0(args);
    let Some(obj) = obj_val.as_object() else {
        return Err(vm.type_error("Object.defineProperty called on non-object"));
    };
    let prop_name = vm.to_property_key(args.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
    let Some(desc) = args.get(2).and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Property description must be an object"));
    };
    if !define_property_checked(vm, obj, &prop_name, desc)? {
        return Err(vm.type_error(format!("Cannot redefine property: {prop_name}")));
    }
    Ok(obj_val)
}

fn reflect_define_property(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(obj) = arg0(args).as_object() else {
        return Err(vm.type_error("Reflect.defineProperty called on non-object"));
    };
    let prop_name = vm.to_property_key(args.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
    let Some(desc) = args.get(2).and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Property description must be an object"));
    };
    Ok(JsValue::from_bool(define_property_checked(
        vm, obj, &prop_name, desc,
    )?))
}

pub(crate) fn define_properties_on_object(
    vm: &mut JsVm,
    obj: HeapHandle,
    props_arg: JsValue,
) -> Result<(), JsException> {
    let props_h = vm.to_object(props_arg)?;
    let mut keys = vm.own_enumerable_keys_including_elements(props_h);
    let sym_keys: Vec<String> = vm
        .own_enumerable_keys(props_h)
        .into_iter()
        .filter_map(|k| {
            let s = vm.heap.interner().get(k)?;
            crate::vm::is_symbol_key_str(s).then(|| s.to_string())
        })
        .collect();
    keys.extend(sym_keys);
    let mut pairs = Vec::with_capacity(keys.len());
    for name in keys {
        let k = vm.heap.intern_key(&name);
        let desc_val = if crate::vm::is_symbol_key_str(&name) {
            vm.get_property_by_key(props_h, k)?
        } else {
            let kv = JsValue::from_string(vm.heap.alloc_string(name.clone()));
            vm.get_elem(JsValue::from_object(props_h), kv)?
        };
        let Some(desc_h) = desc_val.as_object() else {
            return Err(vm.type_error("Property description must be an object"));
        };
        pairs.push((name, desc_h));
    }
    for (name, desc_h) in pairs {
        if !define_property_checked(vm, obj, &name, desc_h)? {
            return Err(vm.type_error(format!("Cannot redefine property: {name}")));
        }
    }
    Ok(())
}

fn object_define_properties(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let obj_val = arg0(args);
    let Some(obj) = obj_val.as_object() else {
        return Err(vm.type_error("Object.defineProperties called on non-object"));
    };
    let props_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    define_properties_on_object(vm, obj, props_arg)?;
    Ok(obj_val)
}

fn make_desc_obj(vm: &mut JsVm, pairs: &[(crate::PropertyKey, JsValue)]) -> JsValue {
    let out = vm.alloc_ordinary_object();
    for &(k, v) in pairs {
        vm.heap
            .object_define_data(out, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    JsValue::from_object(out)
}

fn make_data_desc_obj(vm: &mut JsVm, value: JsValue, w: bool, e: bool, c: bool) -> JsValue {
    let (kw, ke, kc) = (
        vm.heap.intern_key("writable"),
        vm.heap.intern_key("enumerable"),
        vm.heap.intern_key("configurable"),
    );
    make_desc_obj(
        vm,
        &[
            (vm.key_value, value),
            (kw, JsValue::from_bool(w)),
            (ke, JsValue::from_bool(e)),
            (kc, JsValue::from_bool(c)),
        ],
    )
}

fn object_get_own_descriptor(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let target = arg0(args);
    if target.is_null() || target.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    let prop_name = vm.to_property_key(args.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
    if let Some(sh) = target.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        if prop_name == "length" {
            return Ok(make_data_desc_obj(
                vm,
                JsValue::from_f64(s.encode_utf16().count() as f64),
                false,
                false,
                false,
            ));
        }
        if let Some(ch) = prop_name
            .parse::<usize>()
            .ok()
            .and_then(|idx| s.chars().nth(idx))
        {
            let cv = JsValue::from_string(vm.heap.alloc_string(ch.to_string()));
            return Ok(make_data_desc_obj(vm, cv, false, true, false));
        }
        return Ok(JsValue::UNDEFINED);
    }
    let Some(obj) = target.as_object() else {
        return Ok(JsValue::UNDEFINED);
    };
    let key = vm.heap.intern_key(&prop_name);
    if let Some(prop) = vm.heap.object_get_own(obj, key, &vm.shapes) {
        if prop.attrs.contains(PropertyAttrs::ACCESSOR) {
            let (ke, kc) = (
                vm.heap.intern_key("enumerable"),
                vm.heap.intern_key("configurable"),
            );
            return Ok(make_desc_obj(
                vm,
                &[
                    (
                        vm.key_get,
                        prop.getter.map_or(JsValue::UNDEFINED, JsValue::from_object),
                    ),
                    (
                        vm.key_set,
                        prop.setter.map_or(JsValue::UNDEFINED, JsValue::from_object),
                    ),
                    (
                        ke,
                        JsValue::from_bool(prop.attrs.contains(PropertyAttrs::ENUMERABLE)),
                    ),
                    (
                        kc,
                        JsValue::from_bool(prop.attrs.contains(PropertyAttrs::CONFIGURABLE)),
                    ),
                ],
            ));
        }
        return Ok(make_data_desc_obj(
            vm,
            prop.value,
            prop.attrs.contains(PropertyAttrs::WRITABLE),
            prop.attrs.contains(PropertyAttrs::ENUMERABLE),
            prop.attrs.contains(PropertyAttrs::CONFIGURABLE),
        ));
    }
    if prop_name == "length"
        && matches!(
            vm.heap.get_object(obj).map(|o| &o.kind),
            Some(ObjectKind::Array)
        )
    {
        let len = vm.heap.get_object(obj).map_or(0, |o| o.elements.len());
        return Ok(make_data_desc_obj(
            vm,
            JsValue::from_f64(f64::from(len)),
            true,
            false,
            false,
        ));
    }
    if let Ok(idx) = prop_name.parse::<u32>() {
        if let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(obj).map(|o| &o.kind) {
            let ta = *ta;
            if idx < ta.length {
                let v = crate::builtins_collections::typed_array_read_element(vm, ta, idx);
                return Ok(make_data_desc_obj(vm, v, true, true, true));
            }
        }
        if let Some(v) = vm.heap.get_object(obj).and_then(|o| o.elements.get(idx)) {
            return Ok(make_data_desc_obj(vm, v, true, true, true));
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn reflect_get_own_descriptor(
    vm: &mut JsVm,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    if arg0(args).as_object().is_none() {
        return Err(vm.type_error("Reflect.getOwnPropertyDescriptor called on non-object"));
    }
    object_get_own_descriptor(vm, this, args)
}

fn object_get_own_descriptors(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let target = arg0(args);
    if target.is_null() || target.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    let out = vm.alloc_ordinary_object();
    let names_arr = object_get_own_property_names(vm, JsValue::UNDEFINED, &[target])?;
    let syms_arr = object_get_own_property_symbols(vm, JsValue::UNDEFINED, &[target])?;
    for keys_arr in [names_arr, syms_arr] {
        if let Some(nh) = keys_arr.as_object() {
            let len = vm.heap.get_object(nh).map_or(0, |o| o.elements.len());
            for i in 0..len {
                if let Some(kv) = vm.heap.get_object(nh).and_then(|o| o.elements.get(i)) {
                    let desc = object_get_own_descriptor(vm, JsValue::UNDEFINED, &[target, kv])?;
                    if !desc.is_undefined() {
                        let s = vm.to_property_key(kv)?;
                        let k = vm.heap.intern_key(&s);
                        vm.heap.object_define_data(
                            out,
                            k,
                            desc,
                            PropertyAttrs::DEFAULT,
                            &mut vm.shapes,
                        );
                    }
                }
            }
        }
    }
    Ok(JsValue::from_object(out))
}

fn object_get_own_property_symbols(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let target = arg0(args);
    if target.is_null() || target.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    let arr = vm.alloc_array();
    if let Some(h) = target.as_object() {
        let mut keys = Vec::new();
        if let Some(o) = vm.heap.get_object(h) {
            if o.is_dictionary() {
                if let Some(rare) = &o.rare {
                    keys.extend(rare.dict_order.iter().copied());
                }
            } else if let Some(shape) = vm.shapes.get(o.shape()) {
                keys.extend(shape.keys().iter().copied());
            }
        }
        for k in keys {
            if let Some(s) = vm.heap.interner().get(k) {
                if let Some(id_str) = s.strip_prefix("Symbol(").and_then(|r| r.strip_suffix(')')) {
                    if let Ok(id) = id_str.parse::<u32>() {
                        vm.heap.object_push_element(
                            arr,
                            JsValue::from_symbol(crate::value::SymbolId(id)),
                        );
                    }
                }
            }
        }
    }
    Ok(JsValue::from_object(arr))
}

fn object_is_extensible(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = arg0(args).as_object() else {
        return Ok(JsValue::FALSE);
    };
    let ext = vm.heap.get_object(h).is_some_and(|o| o.extensible);
    Ok(JsValue::from_bool(ext))
}

fn object_prevent_extensions(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let v = arg0(args);
    if let Some(h) = v.as_object() {
        if let Some(o) = vm.heap.get_object_mut(h) {
            o.extensible = false;
        }
    }
    Ok(v)
}

pub(crate) fn set_object_integrity_level(vm: &mut JsVm, h: HeapHandle, freeze: bool) {
    let (is_arr, arr_len, elem_pairs) = match vm.heap.get_object(h) {
        Some(o) => {
            let pairs: Vec<(u32, JsValue)> = o
                .elements
                .present_indices()
                .into_iter()
                .filter_map(|i| o.elements.get(i).map(|v| (i, v)))
                .collect();
            (matches!(o.kind, ObjectKind::Array), o.elements.len(), pairs)
        }
        None => return,
    };
    // Migrate fast elements into dictionary properties so their attributes can be locked.
    for (idx, val) in elem_pairs {
        let k = vm.heap.intern_key(&idx.to_string());
        if vm.heap.object_get_own(h, k, &vm.shapes).is_none() {
            vm.heap
                .object_define_data(h, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        if let Some(o) = vm.heap.get_object_mut(h) {
            o.elements.delete(idx);
        }
    }
    if is_arr
        && freeze
        && vm
            .heap
            .object_get_own(h, vm.key_length, &vm.shapes)
            .is_none()
    {
        vm.heap.object_define_data(
            h,
            vm.key_length,
            JsValue::from_f64(f64::from(arr_len)),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
    }
    if let Some(o) = vm.heap.get_object_mut(h) {
        o.extensible = false;
        o.to_dictionary_mode(&vm.shapes);
        if let Some(rare) = o.rare.as_mut() {
            for p in rare.dict_props.values_mut() {
                p.attrs.remove(PropertyAttrs::CONFIGURABLE);
                if freeze && !p.attrs.contains(PropertyAttrs::ACCESSOR) {
                    p.attrs.remove(PropertyAttrs::WRITABLE);
                }
            }
        }
    }
}

fn object_seal(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let v = arg0(args);
    if let Some(h) = v.as_object() {
        set_object_integrity_level(vm, h, false);
    }
    Ok(v)
}

fn test_object_integrity_level(vm: &JsVm, val: JsValue, frozen: bool) -> bool {
    let Some(h) = val.as_object() else {
        return true;
    };
    let Some(o) = vm.heap.get_object(h) else {
        return true;
    };
    if o.extensible || !o.elements.present_indices().is_empty() {
        return false;
    }
    if frozen
        && matches!(o.kind, ObjectKind::Array)
        && vm
            .heap
            .object_get_own(h, vm.key_length, &vm.shapes)
            .is_none_or(|p| p.attrs.contains(PropertyAttrs::WRITABLE))
    {
        return false;
    }
    let ok = |a: PropertyAttrs| {
        !a.contains(PropertyAttrs::CONFIGURABLE)
            && !(frozen
                && !a.contains(PropertyAttrs::ACCESSOR)
                && a.contains(PropertyAttrs::WRITABLE))
    };
    if o.is_dictionary() {
        o.rare
            .as_ref()
            .is_none_or(|r| r.dict_props.values().all(|p| ok(p.attrs)))
    } else {
        vm.shapes.get(o.shape()).is_none_or(|sh| {
            sh.keys()
                .iter()
                .filter_map(|&k| sh.lookup(k))
                .all(|s| ok(s.attrs))
        })
    }
}

fn object_is_sealed(vm: &mut JsVm, _t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_bool(test_object_integrity_level(
        vm,
        arg0(args),
        false,
    )))
}

fn object_group_by(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let items = vm.collect_iterable(arg0(args))?;
    let cb = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    if !cb.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
        return Err(vm.type_error("Object.groupBy callback is not a function"));
    }
    let out = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, None));
    for (i, item) in items.into_iter().enumerate() {
        let key_val =
            vm.call_value(cb, JsValue::UNDEFINED, &[item, JsValue::from_f64(i as f64)])?;
        let key_str = vm.to_property_key(key_val)?;
        let key = vm.heap.intern_key(&key_str);
        let arr_h = if let Some(prop) = vm.heap.object_get_own(out, key, &vm.shapes) {
            prop.value.as_object().unwrap()
        } else {
            let a = vm.alloc_array();
            vm.heap.object_define_data(
                out,
                key,
                JsValue::from_object(a),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            a
        };
        vm.heap.object_push_element(arr_h, item);
    }
    Ok(JsValue::from_object(out))
}

fn object_get_own_property_names(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(arg0(args))?;
    let arr = vm.alloc_array();
    let (mut idx_keys, mut str_keys, mut has_len) = (Vec::new(), Vec::new(), false);
    let is_arr = matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::Array)
    );
    if let Some(o) = vm.heap.get_object(h) {
        if let ObjectKind::TypedArray(ta) = o.kind {
            idx_keys.extend(0..ta.length);
        } else {
            idx_keys.extend(o.elements.present_indices());
        }
        let mut push_key = |k: crate::PropertyKey| {
            let Some(s) = vm.heap.interner().get(k) else {
                return;
            };
            if crate::vm::is_symbol_key_str(s) {
                return;
            }
            if s.as_ref() == "length" {
                has_len = true;
            }
            if let Some(idx) = crate::vm::parse_array_index_str(s) {
                if !idx_keys.contains(&idx) {
                    idx_keys.push(idx);
                }
            } else if !str_keys.iter().any(|x: &String| x == s.as_ref()) {
                str_keys.push(s.to_string());
            }
        };
        if o.is_dictionary() {
            if let Some(r) = &o.rare {
                r.dict_order.iter().copied().for_each(&mut push_key);
            }
        } else if let Some(sh) = vm.shapes.get(o.shape()) {
            sh.keys().iter().copied().for_each(&mut push_key);
        }
    }
    if is_arr && !has_len {
        str_keys.push("length".to_string());
    }
    idx_keys.sort_unstable();
    idx_keys.dedup();
    for s in idx_keys.into_iter().map(|i| i.to_string()).chain(str_keys) {
        let v = JsValue::from_string(vm.heap.alloc_string(s));
        vm.heap.object_push_element(arr, v);
    }
    Ok(JsValue::from_object(arr))
}

fn object_from_entries(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let entries = vm.collect_iterable(arg0(args))?;
    let out = vm.alloc_ordinary_object();
    for entry in entries {
        if entry.as_object().is_none() {
            return Err(vm.type_error("Iterator value is not an entry object"));
        }
        let k = vm.get_elem(entry, JsValue::from_i32(0))?;
        let v = vm.get_elem(entry, JsValue::from_i32(1))?;
        let ks = vm.to_property_key(k)?;
        let pk = vm.heap.intern_key(&ks);
        vm.heap
            .object_define_data(out, pk, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    Ok(JsValue::from_object(out))
}

fn object_is_frozen(vm: &mut JsVm, _t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_bool(test_object_integrity_level(
        vm,
        arg0(args),
        true,
    )))
}
