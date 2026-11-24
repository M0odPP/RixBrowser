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

pub fn install_primitive_builtins(vm: &mut JsVm) {
    install_global_functions(vm);
    install_string(vm);
    install_number(vm);
    install_boolean(vm);
    install_bigint(vm);
    install_math(vm);
    install_symbol(vm);
    crate::builtins_date::install_date(vm);
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
    if let Some(o) = vm.heap.get_object_mut(np) {
        o.rare_mut().boxed_value = Some(JsValue::from_i32(0));
    }
    define_method_len(vm, np, "toFixed", 1, num_to_fixed);
    define_method_len(vm, np, "toExponential", 1, num_to_exponential);
    define_method_len(vm, np, "toPrecision", 1, num_to_precision);
    define_method_len(vm, np, "toString", 1, num_to_string);
    define_method_len(vm, np, "valueOf", 0, num_value_of);
    define_method_len(vm, np, "toLocaleString", 0, num_to_string);

    let ctor = vm.alloc_native_fn("Number", number_ctor);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(np),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    link_ctor_meta(vm, ctor, np, 1, true);
    define_method_len(vm, ctor, "isNaN", 1, num_is_nan);
    define_method_len(vm, ctor, "isFinite", 1, num_is_finite);
    define_method_len(vm, ctor, "isInteger", 1, num_is_integer);
    define_method_len(vm, ctor, "isSafeInteger", 1, num_is_safe_integer);
    // ECMA-262 §21.1.2.12–13: `Number.parseInt === globalThis.parseInt` and `Number.parseFloat === globalThis.parseFloat`.
    for name in ["parseInt", "parseFloat"] {
        let k = vm.heap.intern_key(name);
        if let Ok(gfn) = vm.get_property_by_key(vm.global, k) {
            vm.heap
                .object_define_data(ctor, k, gfn, PropertyAttrs::METHOD, &mut vm.shapes);
        }
    }

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
            let prim = if v.is_object() {
                vm.to_primitive(v, crate::vm::PrimHint::Number)?
            } else {
                v
            };
            if let Some(bh) = prim.as_bigint() {
                vm.heap.get_bigint(bh).map_or(0.0, |b| {
                    b.to_radix_string(10).parse::<f64>().unwrap_or(f64::NAN)
                })
            } else {
                vm.to_number(prim)?
            }
        }
        None => 0.0,
    };
    let nv = JsValue::from_f64(n);
    if !vm.native_new_target.is_undefined() {
        if let Some(h) = this_val.as_object() {
            if let Some(o) = vm.heap.get_object_mut(h) {
                o.rare_mut().boxed_value = Some(nv);
            }
            return Ok(this_val);
        }
    }
    Ok(nv)
}

fn require_number_this(vm: &mut JsVm, this_val: JsValue, method: &str) -> Result<f64, JsException> {
    unbox_prim(vm, this_val).as_number().ok_or_else(|| {
        vm.type_error(format!(
            "Number.prototype.{method} requires that 'this' be a Number"
        ))
    })
}

fn format_exp_js(s: String) -> String {
    if let Some((mant, exp)) = s.split_once('e') {
        if exp.starts_with('-') || exp.starts_with('+') {
            format!("{mant}e{exp}")
        } else {
            format!("{mant}e+{exp}")
        }
    } else {
        s
    }
}

fn num_to_fixed(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let n = require_number_this(vm, this_val, "toFixed")?;
    let f = vm.to_integer_or_infinity(arg0(args))?;
    if !(0.0..=100.0).contains(&f) {
        return Err(vm.range_error("toFixed() digits argument must be between 0 and 100"));
    }
    let out = if !n.is_finite() || n.abs() >= 1e21 {
        vm.value_to_rust_string(JsValue::from_f64(n))
    } else {
        let mut s = format!("{n:.prec$}", prec = f as usize);
        if n == 0.0 && s.starts_with('-') {
            s.remove(0);
        }
        s
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn num_to_exponential(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let n = require_number_this(vm, this_val, "toExponential")?;
    let fd = arg0(args);
    let f = vm.to_integer_or_infinity(fd)?;
    if !n.is_finite() {
        return Ok(JsValue::from_string(
            vm.heap
                .alloc_string(vm.value_to_rust_string(JsValue::from_f64(n))),
        ));
    }
    if !(0.0..=100.0).contains(&f) {
        return Err(vm.range_error("toExponential() argument must be between 0 and 100"));
    }
    let n_clean = if n == 0.0 { 0.0 } else { n };
    let out = if fd.is_undefined() {
        format_exp_js(format!("{n_clean:e}"))
    } else {
        format_exp_js(format!("{n_clean:.prec$e}", prec = f as usize))
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn num_to_precision(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let n = require_number_this(vm, this_val, "toPrecision")?;
    let p_arg = arg0(args);
    if p_arg.is_undefined() {
        return Ok(JsValue::from_string(
            vm.heap
                .alloc_string(vm.value_to_rust_string(JsValue::from_f64(n))),
        ));
    }
    let p = vm.to_integer_or_infinity(p_arg)?;
    if !n.is_finite() {
        return Ok(JsValue::from_string(
            vm.heap
                .alloc_string(vm.value_to_rust_string(JsValue::from_f64(n))),
        ));
    }
    if !(1.0..=100.0).contains(&p) {
        return Err(vm.range_error("toPrecision() argument must be between 1 and 100"));
    }
    let prec = p as usize;
    let n_clean = if n == 0.0 { 0.0 } else { n };
    let sci = format_exp_js(format!("{n_clean:.d$e}", d = prec - 1));
    let out = if let Some((mant, exp_str)) = sci.split_once('e') {
        let exp: i32 = exp_str.parse().unwrap_or(0);
        if exp < -6 || exp >= prec as i32 {
            sci
        } else {
            let neg = mant.starts_with('-');
            let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
            let mut res = if neg {
                String::from("-")
            } else {
                String::new()
            };
            if exp < 0 {
                res.push_str("0.");
                for _ in 0..((-exp - 1) as usize) {
                    res.push('0');
                }
                res.push_str(&digits);
            } else {
                let split = (exp + 1) as usize;
                if split >= digits.len() {
                    res.push_str(&digits);
                    for _ in 0..(split - digits.len()) {
                        res.push('0');
                    }
                } else {
                    res.push_str(&digits[..split]);
                    res.push('.');
                    res.push_str(&digits[split..]);
                }
            }
            res
        }
    } else {
        sci
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

fn to_bigint_val(vm: &mut JsVm, val: JsValue) -> Result<JsBigInt, JsException> {
    let prim = if val.is_object() {
        vm.to_primitive(val, crate::vm::PrimHint::Number)?
    } else {
        val
    };
    match prim.kind() {
        JsValueKind::BigInt(h) => Ok(vm
            .heap
            .get_bigint(h)
            .cloned()
            .unwrap_or_else(|| JsBigInt::from_i64(0))),
        JsValueKind::Bool(b) => Ok(JsBigInt::from_i64(i64::from(b))),
        JsValueKind::String(h) => {
            let s = vm.heap.materialize_string(h).unwrap_or_default();
            JsBigInt::parse_str(&s).ok_or_else(|| {
                vm.make_error("SyntaxError", format!("Cannot convert {s} to a BigInt"))
            })
        }
        _ => Err(vm.type_error("Cannot convert value to a BigInt")),
    }
}

fn bigint_as_n(vm: &mut JsVm, args: &[JsValue], signed: bool) -> Result<JsValue, JsException> {
    let bits_idx = crate::builtins_atomics::to_index(vm, arg0(args))?;
    let bi_val = to_bigint_val(vm, args.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
    let raw = bi_val.to_i64_lossy();
    let bi = if signed {
        let out = if bits_idx == 0 {
            0i64
        } else if bits_idx >= 64 {
            raw
        } else {
            let shift = 64 - bits_idx as u32;
            (raw << shift) >> shift
        };
        JsBigInt::from_i64(out)
    } else {
        let u = raw as u64;
        let out = if bits_idx == 0 {
            0u64
        } else if bits_idx >= 64 {
            u
        } else {
            u & ((1u64 << (bits_idx as u32)) - 1)
        };
        JsBigInt::from_i128(i128::from(out))
    };
    Ok(JsValue::from_bigint(vm.heap.alloc_bigint(bi)))
}

fn bigint_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = unbox_prim(vm, this_val).as_bigint() else {
        return Err(vm.type_error("BigInt.prototype.toString requires a BigInt"));
    };
    let radix = args
        .first()
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_integer_or_infinity(v))
        .transpose()?
        .unwrap_or(10.0);
    if !(2.0..=36.0).contains(&radix) {
        return Err(vm.range_error("toString() radix must be between 2 and 36"));
    }
    let s = vm
        .heap
        .get_bigint(h)
        .map_or_else(|| "0".to_owned(), |bi| bi.to_radix_string(radix as u32));
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn install_bigint(vm: &mut JsVm) {
    let bi_proto = vm.bigint_proto;
    define_method_len(vm, bi_proto, "toString", 0, bigint_to_string);
    define_method_len(vm, bi_proto, "toLocaleString", 0, bigint_to_string);
    define_method_len(vm, bi_proto, "valueOf", 0, |vm, this_val, _| {
        let prim = unbox_prim(vm, this_val);
        if prim.as_bigint().is_some() {
            Ok(prim)
        } else {
            Err(vm.type_error("BigInt.prototype.valueOf requires a BigInt"))
        }
    });
    let tag = JsValue::from_string(vm.heap.alloc_string("BigInt"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        bi_proto,
        k_tag,
        tag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    // Note: `BigInt` is NOT a constructor (`new BigInt(1)` throws TypeError per ECMA-262 §21.2.1.1).
    let ctor = vm.alloc_native_fn("BigInt", |vm, _this, args| {
        let prim = {
            let v = arg0(args);
            if v.is_object() {
                vm.to_primitive(v, crate::vm::PrimHint::Number)?
            } else {
                v
            }
        };
        let bi = match prim.kind() {
            JsValueKind::BigInt(_) => return Ok(prim),
            JsValueKind::Bool(b) => JsBigInt::from_i64(i64::from(b)),
            JsValueKind::Int32(i) => JsBigInt::from_i64(i64::from(i)),
            JsValueKind::Float(f) if f.is_finite() && f.fract() == 0.0 => {
                JsBigInt::from_i128(f as i128)
            }
            JsValueKind::Float(_) => {
                return Err(vm.range_error("Cannot convert non-integer number to BigInt"))
            }
            JsValueKind::String(h) => {
                let s = vm.heap.materialize_string(h).unwrap_or_default();
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
// Math (ECMA-262 §21.3 + ES2025 `f16round` & `sumPrecise`)
// ---------------------------------------------------------------------------

/// Shewchuk exact adaptive floating-point expansion sum (`Math.sumPrecise`).
fn math_sum_precise(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let iter = vm.create_iterator_public(arg0(args))?;
    let mut partials: Vec<f64> = Vec::new();
    let mut has_pos_inf = false;
    let mut has_neg_inf = false;
    let mut has_nan = false;
    let mut minus_zero = true;
    loop {
        let (val, done) = vm.step_iterator_public(JsValue::from_object(iter))?;
        if done {
            break;
        }
        let Some(mut x) = val.as_number() else {
            let _ = vm.iterator_close_handle(iter, true);
            return Err(vm.type_error("Math.sumPrecise requires number values"));
        };
        if x.is_nan() {
            has_nan = true;
            continue;
        }
        if x.is_infinite() {
            if x.is_sign_positive() {
                has_pos_inf = true;
            } else {
                has_neg_inf = true;
            }
            continue;
        }
        if x == 0.0 {
            if x.is_sign_positive() {
                minus_zero = false;
            }
            continue;
        }
        minus_zero = false;
        // Shewchuk Grow-Expansion (Two-Sum)
        let mut write = 0usize;
        for read in 0..partials.len() {
            let mut y = partials[read];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[write] = lo;
                write += 1;
            }
            x = hi;
        }
        partials.truncate(write);
        if x != 0.0 {
            partials.push(x);
        }
    }
    if has_nan || (has_pos_inf && has_neg_inf) {
        return Ok(JsValue::NAN);
    }
    if has_pos_inf {
        return Ok(JsValue::from_f64(f64::INFINITY));
    }
    if has_neg_inf {
        return Ok(JsValue::from_f64(f64::NEG_INFINITY));
    }
    if minus_zero {
        return Ok(JsValue::from_f64(-0.0));
    }
    // Exact final rounding of non-overlapping expansion (Python `math.fsum` finalization).
    let mut n = partials.len();
    let mut hi = 0.0f64;
    if n > 0 {
        n -= 1;
        hi = partials[n];
        while n > 0 {
            n -= 1;
            let x = hi;
            let y = partials[n];
            hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                if n > 0
                    && ((lo < 0.0 && partials[n - 1] < 0.0) || (lo > 0.0 && partials[n - 1] > 0.0))
                {
                    let y2 = lo * 2.0;
                    let x2 = hi + y2;
                    if y2 == x2 - hi {
                        hi = x2;
                    }
                }
                break;
            }
        }
    }
    Ok(JsValue::from_f64(if hi == 0.0 { 0.0 } else { hi }))
}

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
    let mtag = JsValue::from_string(vm.heap.alloc_string("Math"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        math,
        k_tag,
        mtag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    let fns: &[(&str, u32, crate::vm::NativeFnPtr)] = &[
        ("abs", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.abs()))
        }),
        ("acos", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.acos()))
        }),
        ("acosh", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.acosh()))
        }),
        ("asin", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.asin()))
        }),
        ("asinh", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.asinh() }))
        }),
        ("atan", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.atan() }))
        }),
        ("atanh", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.atanh() }))
        }),
        ("atan2", 2, |vm, _, a| {
            let y = vm.to_number(arg0(a))?;
            let x = vm.to_number(a.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
            Ok(JsValue::from_f64(y.atan2(x)))
        }),
        ("cbrt", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.cbrt() }))
        }),
        ("ceil", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.ceil()))
        }),
        ("clz32", 1, |vm, _, a| {
            let x = vm.to_u32(arg0(a))?;
            Ok(JsValue::from_i32(x.leading_zeros() as i32))
        }),
        ("cos", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.cos()))
        }),
        ("cosh", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.cosh()))
        }),
        ("exp", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.exp()))
        }),
        ("expm1", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.exp_m1() }))
        }),
        ("floor", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.floor()))
        }),
        ("fround", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(f64::from(x as f32)))
        }),
        ("f16round", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            let bits = crate::builtins_collections::f64_to_f16(x);
            Ok(JsValue::from_f64(crate::builtins_collections::f16_to_f64(
                bits,
            )))
        }),
        ("hypot", 2, |vm, _, args| {
            let mut vals = Vec::with_capacity(args.len());
            for &v in args {
                vals.push(vm.to_number(v)?);
            }
            let mut has_nan = false;
            let mut max_abs = 0.0f64;
            for x in vals.iter().copied() {
                if x.is_infinite() {
                    return Ok(JsValue::from_f64(f64::INFINITY));
                }
                if x.is_nan() {
                    has_nan = true;
                } else {
                    max_abs = max_abs.max(x.abs());
                }
            }
            if has_nan {
                return Ok(JsValue::NAN);
            }
            if max_abs == 0.0 {
                return Ok(JsValue::from_f64(0.0));
            }
            let mut sum = 0.0f64;
            for x in vals {
                let s = x / max_abs;
                sum += s * s;
            }
            Ok(JsValue::from_f64(max_abs * sum.sqrt()))
        }),
        ("imul", 2, |vm, _, a| {
            let x = vm.to_i32(arg0(a))?;
            let y = vm.to_i32(a.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
            Ok(JsValue::from_i32(x.wrapping_mul(y)))
        }),
        ("log", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.ln()))
        }),
        ("log1p", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.ln_1p() }))
        }),
        ("log2", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.log2()))
        }),
        ("log10", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.log10()))
        }),
        ("max", 2, |vm, _, args| {
            let mut res = f64::NEG_INFINITY;
            let mut nan = false;
            for &v in args {
                let n = vm.to_number(v)?;
                if n.is_nan() {
                    nan = true;
                } else if !nan && (n > res || (n == 0.0 && res == 0.0 && n.is_sign_positive())) {
                    res = n;
                }
            }
            Ok(JsValue::from_f64(if nan { f64::NAN } else { res }))
        }),
        ("min", 2, |vm, _, args| {
            let mut res = f64::INFINITY;
            let mut nan = false;
            for &v in args {
                let n = vm.to_number(v)?;
                if n.is_nan() {
                    nan = true;
                } else if !nan && (n < res || (n == 0.0 && res == 0.0 && n.is_sign_negative())) {
                    res = n;
                }
            }
            Ok(JsValue::from_f64(if nan { f64::NAN } else { res }))
        }),
        ("pow", 2, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            let y = vm.to_number(a.get(1).copied().unwrap_or(JsValue::UNDEFINED))?;
            let out = if y.is_nan() {
                f64::NAN
            } else if y == 0.0 {
                1.0
            } else if x.abs() == 1.0 && y.is_infinite() {
                f64::NAN
            } else {
                x.powf(y)
            };
            Ok(JsValue::from_f64(out))
        }),
        ("random", 0, |vm, _, _| {
            let mut x = vm.rng_state;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            vm.rng_state = x;
            let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
            Ok(JsValue::from_f64(
                (bits as f64) * (1.0 / ((1u64 << 53) as f64)),
            ))
        }),
        ("round", 1, |vm, _, a| {
            let n = vm.to_number(arg0(a))?;
            let r = if !n.is_finite() || n == 0.0 {
                n
            } else if (-0.5..0.0).contains(&n) {
                -0.0
            } else {
                (n + 0.5).floor()
            };
            Ok(JsValue::from_f64(r))
        }),
        ("sign", 1, |vm, _, a| {
            let n = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if n.is_nan() || n == 0.0 {
                n
            } else if n > 0.0 {
                1.0
            } else {
                -1.0
            }))
        }),
        ("sin", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.sin() }))
        }),
        ("sinh", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.sinh() }))
        }),
        ("sqrt", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.sqrt()))
        }),
        ("sumPrecise", 1, math_sum_precise),
        ("tan", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.tan() }))
        }),
        ("tanh", 1, |vm, _, a| {
            let x = vm.to_number(arg0(a))?;
            Ok(JsValue::from_f64(if x == 0.0 { x } else { x.tanh() }))
        }),
        ("trunc", 1, |vm, _, a| {
            Ok(JsValue::from_f64(vm.to_number(arg0(a))?.trunc()))
        }),
    ];
    for &(name, len, f) in fns {
        define_method_len(vm, math, name, len, f);
    }

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
    define_method_len(vm, sym_proto, "valueOf", 0, |vm, this_val, _| {
        let prim = unbox_prim(vm, this_val);
        if prim.is_symbol() {
            Ok(prim)
        } else {
            Err(vm.type_error("Symbol.prototype.valueOf requires a Symbol"))
        }
    });
    define_method_len(vm, sym_proto, "toString", 0, |vm, this_val, _| {
        let Some(id) = unbox_prim(vm, this_val).as_symbol() else {
            return Err(vm.type_error("Symbol.prototype.toString requires a Symbol"));
        };
        let desc = vm.symbol_descriptions.get(&id).cloned().unwrap_or_default();
        Ok(JsValue::from_string(
            vm.heap.alloc_string(format!("Symbol({desc})")),
        ))
    });
    let to_prim = vm.alloc_native_fn("[Symbol.toPrimitive]", |vm, this_val, _| {
        let prim = unbox_prim(vm, this_val);
        if prim.is_symbol() {
            Ok(prim)
        } else {
            Err(vm.type_error("Symbol.prototype[Symbol.toPrimitive] requires a Symbol"))
        }
    });
    vm.set_fn_length(to_prim, 1);
    let k_tp = vm.key_symbol_to_primitive;
    vm.heap.object_define_data(
        sym_proto,
        k_tp,
        JsValue::from_object(to_prim),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    let stag = JsValue::from_string(vm.heap.alloc_string("Symbol"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        sym_proto,
        k_tag,
        stag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    let desc_getter = vm.alloc_native_fn("get description", |vm, this_val, _| {
        let Some(id) = unbox_prim(vm, this_val).as_symbol() else {
            return Err(vm.type_error("Symbol.prototype.description requires a Symbol"));
        };
        Ok(match vm.symbol_descriptions.get(&id).cloned() {
            Some(d) => JsValue::from_string(vm.heap.alloc_string(d)),
            None => JsValue::UNDEFINED,
        })
    });
    vm.set_fn_length(desc_getter, 0);
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
    define_method_len(vm, ctor, "for", 1, |vm, _this, args| {
        let key = vm.to_js_string(arg0(args))?;
        if let Some(&id) = vm.symbol_registry.get(&key) {
            return Ok(JsValue::from_symbol(id));
        }
        let id = vm.heap.alloc_symbol(Some(key.clone()));
        vm.symbol_registry.insert(key.clone(), id);
        vm.symbol_descriptions.insert(id, key);
        Ok(JsValue::from_symbol(id))
    });
    define_method_len(vm, ctor, "keyFor", 1, |vm, _this, args| {
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
        ("dispose", vm.sym_dispose),
        ("asyncDispose", vm.sym_async_dispose),
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
// JSON (ECMA-262 §25.5 + ES2025 `rawJSON` / `isRawJSON` / reviver `context`)
// ---------------------------------------------------------------------------

fn install_json(vm: &mut JsVm) {
    let json = vm.alloc_ordinary_object();
    define_method_len(vm, json, "parse", 2, json_parse);
    define_method_len(vm, json, "stringify", 3, json_stringify);
    define_method_len(vm, json, "rawJSON", 1, json_raw_json);
    define_method_len(vm, json, "isRawJSON", 1, json_is_raw_json);
    let jtag = JsValue::from_string(vm.heap.alloc_string("JSON"));
    let k_tag = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        json,
        k_tag,
        jtag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    define_global(vm, "JSON", JsValue::from_object(json));
}

fn is_json_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Pure-Rust ECMA-262 §25.5.1 JSON parser that allocates directly onto `JsVm`'s heap,
/// accepts lone UTF-16 surrogates in `\uXXXX` escapes (required by ECMA-262, rejected by
/// `serde_json`), preserves `-0`, and enforces strict RFC 8259 / ECMA-262 syntax.
struct EcmaJsonParser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> EcmaJsonParser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && is_json_ws(self.bytes[self.pos]) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn err(&self, vm: &mut JsVm, msg: &str) -> JsException {
        vm.make_error(
            "SyntaxError",
            format!("JSON.parse: {msg} at position {}", self.pos),
        )
    }

    fn parse_root(&mut self, vm: &mut JsVm) -> Result<JsValue, JsException> {
        self.skip_ws();
        let val = self.parse_value(vm)?;
        self.skip_ws();
        if self.pos != self.bytes.len() {
            return Err(self.err(vm, "unexpected trailing token"));
        }
        Ok(val)
    }

    fn parse_value(&mut self, vm: &mut JsVm) -> Result<JsValue, JsException> {
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Err(self.err(vm, "unexpected end of JSON input"));
        };
        match b {
            b'"' => {
                let s = self.parse_string(vm)?;
                Ok(JsValue::from_string(vm.heap.alloc_string(s)))
            }
            b'{' => self.parse_object(vm),
            b'[' => self.parse_array(vm),
            b't' => {
                if self.bytes[self.pos..].starts_with(b"true") {
                    self.pos += 4;
                    Ok(JsValue::TRUE)
                } else {
                    Err(self.err(vm, "invalid token"))
                }
            }
            b'f' => {
                if self.bytes[self.pos..].starts_with(b"false") {
                    self.pos += 5;
                    Ok(JsValue::FALSE)
                } else {
                    Err(self.err(vm, "invalid token"))
                }
            }
            b'n' => {
                if self.bytes[self.pos..].starts_with(b"null") {
                    self.pos += 4;
                    Ok(JsValue::NULL)
                } else {
                    Err(self.err(vm, "invalid token"))
                }
            }
            b'-' | b'0'..=b'9' => self.parse_number(vm),
            _ => Err(self.err(vm, "unexpected token")),
        }
    }

    fn parse_hex4(&mut self, vm: &mut JsVm) -> Result<u16, JsException> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.err(vm, "unexpected end of unicode escape"));
        }
        let mut val = 0u16;
        for i in 0..4 {
            let b = self.bytes[self.pos + i];
            let d = match b {
                b'0'..=b'9' => (b - b'0') as u16,
                b'a'..=b'f' => (b - b'a' + 10) as u16,
                b'A'..=b'F' => (b - b'A' + 10) as u16,
                _ => return Err(self.err(vm, "invalid unicode escape")),
            };
            val = (val << 4) | d;
        }
        self.pos += 4;
        Ok(val)
    }

    fn parse_string(&mut self, vm: &mut JsVm) -> Result<String, JsException> {
        if self.peek() != Some(b'"') {
            return Err(self.err(vm, "expected '\"'"));
        }
        self.pos += 1;
        let mut out = String::new();
        let mut chunk_start = self.pos;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                out.push_str(&self.src[chunk_start..self.pos]);
                self.pos += 1;
                return Ok(out);
            }
            if b < 0x20 {
                return Err(self.err(vm, "unescaped control character in string"));
            }
            if b == b'\\' {
                out.push_str(&self.src[chunk_start..self.pos]);
                self.pos += 1;
                let Some(esc) = self.peek() else {
                    return Err(self.err(vm, "unterminated escape sequence"));
                };
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\x08'),
                    b'f' => out.push('\x0C'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let unit = self.parse_hex4(vm)?;
                        if (0xD800..=0xDBFF).contains(&unit)
                            && self.pos + 6 <= self.bytes.len()
                            && self.bytes[self.pos] == b'\\'
                            && self.bytes[self.pos + 1] == b'u'
                        {
                            let saved = self.pos;
                            self.pos += 2;
                            if let Ok(low) = self.parse_hex4(vm) {
                                if (0xDC00..=0xDFFF).contains(&low) {
                                    let cp = 0x10000
                                        + (((u32::from(unit) - 0xD800) << 10)
                                            | (u32::from(low) - 0xDC00));
                                    if let Some(ch) = char::from_u32(cp) {
                                        out.push(ch);
                                        chunk_start = self.pos;
                                        continue;
                                    }
                                }
                            }
                            self.pos = saved;
                        }
                        if let Some(ch) = char::from_u32(u32::from(unit)) {
                            out.push(ch);
                        } else {
                            // Lone UTF-16 surrogate: ECMA-262 permits it in JSON strings.
                            out.push('\u{FFFD}');
                        }
                    }
                    _ => return Err(self.err(vm, "invalid escape character")),
                }
                chunk_start = self.pos;
            } else {
                self.pos += 1;
            }
        }
        Err(self.err(vm, "unterminated string"))
    }

    fn parse_number(&mut self, vm: &mut JsVm) -> Result<JsValue, JsException> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err(vm, "leading zero in number"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err(vm, "invalid number")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err(vm, "missing fractional digits"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err(vm, "missing exponent digits"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let slice = &self.src[start..self.pos];
        if !is_float && !slice.starts_with("-0") {
            if let Ok(i) = slice.parse::<i32>() {
                return Ok(JsValue::from_i32(i));
            }
        }
        let f = slice
            .parse::<f64>()
            .map_err(|_| self.err(vm, "invalid number"))?;
        Ok(JsValue::from_f64(f))
    }

    fn parse_array(&mut self, vm: &mut JsVm) -> Result<JsValue, JsException> {
        self.depth += 1;
        if self.depth > 512 {
            return Err(vm.range_error("JSON.parse: maximum nesting depth exceeded"));
        }
        self.pos += 1; // '['
        let arr = vm.alloc_array();
        vm.temp_roots.push(JsValue::from_object(arr));
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            vm.temp_roots.pop();
            self.depth -= 1;
            return Ok(JsValue::from_object(arr));
        }
        let res = (|| loop {
            let item = self.parse_value(vm)?;
            vm.heap.object_push_element(arr, item);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsValue::from_object(arr));
                }
                _ => return Err(self.err(vm, "expected ',' or ']' in array")),
            }
        })();
        vm.temp_roots.pop();
        self.depth -= 1;
        res
    }

    fn parse_object(&mut self, vm: &mut JsVm) -> Result<JsValue, JsException> {
        self.depth += 1;
        if self.depth > 512 {
            return Err(vm.range_error("JSON.parse: maximum nesting depth exceeded"));
        }
        self.pos += 1; // '{'
        let obj = vm.alloc_ordinary_object();
        vm.temp_roots.push(JsValue::from_object(obj));
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            vm.temp_roots.pop();
            self.depth -= 1;
            return Ok(JsValue::from_object(obj));
        }
        let res = (|| loop {
            self.skip_ws();
            let key_str = self.parse_string(vm)?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err(vm, "expected ':' after property name"));
            }
            self.pos += 1;
            let val = self.parse_value(vm)?;
            let key = vm.heap.intern_key(&key_str);
            vm.heap
                .object_define_data(obj, key, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsValue::from_object(obj));
                }
                _ => return Err(self.err(vm, "expected ',' or '}' in object")),
            }
        })();
        vm.temp_roots.pop();
        self.depth -= 1;
        res
    }
}

fn json_raw_json(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let text = vm.to_js_string(arg0(args))?;
    let bytes = text.as_bytes();
    if bytes.is_empty()
        || is_json_ws(bytes[0])
        || is_json_ws(bytes[bytes.len() - 1])
        || matches!(bytes[0], b'{' | b'[')
    {
        return Err(vm.make_error(
            "SyntaxError",
            "JSON.rawJSON requires a valid JSON primitive",
        ));
    }
    let parsed = EcmaJsonParser::new(&text).parse_root(vm)?;
    if parsed.is_object() {
        return Err(vm.make_error("SyntaxError", "JSON.rawJSON cannot wrap an object or array"));
    }
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, None));
    let k_raw = vm.heap.intern_key("rawJSON");
    let tv = JsValue::from_string(vm.heap.alloc_string(text));
    vm.heap
        .object_define_data(obj, k_raw, tv, PropertyAttrs::ENUMERABLE, &mut vm.shapes);
    if let Some(o) = vm.heap.get_object_mut(obj) {
        o.extensible = false;
        o.rare_mut().is_raw_json = true;
    }
    Ok(JsValue::from_object(obj))
}

fn json_is_raw_json(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let is_raw = arg0(args)
        .as_object()
        .and_then(|h| vm.heap.get_object(h))
        .and_then(|o| o.rare.as_ref())
        .is_some_and(|r| r.is_raw_json);
    Ok(JsValue::from_bool(is_raw))
}

fn json_parse(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let text = vm.to_js_string(arg0(args))?;
    let val = EcmaJsonParser::new(&text).parse_root(vm)?;
    if let Some(reviver) = args
        .get(1)
        .copied()
        .filter(|v| v.as_object().is_some_and(|h| vm.is_callable_handle(h)))
    {
        let holder = vm.alloc_ordinary_object();
        let empty_k = vm.heap.intern_key("");
        vm.heap
            .object_define_data(holder, empty_k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
        let root_src = if !val.is_object() {
            Some(text.trim().to_string())
        } else {
            None
        };
        return json_revive(vm, holder, "", reviver, root_src.as_deref());
    }
    Ok(val)
}

fn json_revive(
    vm: &mut JsVm,
    holder: HeapHandle,
    name: &str,
    reviver: JsValue,
    prim_source: Option<&str>,
) -> Result<JsValue, JsException> {
    let name_v = JsValue::from_string(vm.heap.alloc_string(name));
    let val = vm.get_elem(JsValue::from_object(holder), name_v)?;
    let ctx = vm.alloc_ordinary_object();
    if let Some(obj) = val.as_object() {
        let is_arr = vm.is_array_value(val).unwrap_or(false);
        if is_arr {
            let len = vm.array_get_len(obj)?;
            for i in 0..len {
                let idx_s = i.to_string();
                let new_v = json_revive(vm, obj, &idx_s, reviver, None)?;
                if new_v.is_undefined() {
                    let _ = vm.array_delete_index_strict(obj, i);
                } else {
                    let _ = vm.array_create_data_prop(obj, i, new_v);
                }
            }
        } else {
            let keys = vm.own_enumerable_keys_including_elements(obj);
            for k in keys {
                let new_v = json_revive(vm, obj, &k, reviver, None)?;
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
    } else {
        let src_str = prim_source
            .map(str::to_string)
            .unwrap_or_else(|| match val.kind() {
                JsValueKind::Null => "null".into(),
                JsValueKind::Bool(b) => if b { "true" } else { "false" }.into(),
                JsValueKind::Int32(i) => i.to_string(),
                JsValueKind::Float(f) => vm.value_to_rust_string(JsValue::from_f64(f)),
                JsValueKind::String(sh) => {
                    let s = vm.heap.materialize_string(sh).unwrap_or_default();
                    serde_json::to_string(&s).unwrap_or_else(|_| "\"\"".into())
                }
                _ => String::new(),
            });
        if !src_str.is_empty() {
            let k_src = vm.heap.intern_key("source");
            let sv = JsValue::from_string(vm.heap.alloc_string(src_str));
            vm.heap
                .object_define_data(ctx, k_src, sv, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
    }
    let name_v = JsValue::from_string(vm.heap.alloc_string(name));
    vm.call_value(
        reviver,
        JsValue::from_object(holder),
        &[name_v, val, JsValue::from_object(ctx)],
    )
}

enum JsonReplacer {
    None,
    Fn(JsValue),
    List(Vec<String>),
}

fn json_stringify(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let val = arg0(args);
    let rep_arg = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let replacer = if let Some(rh) = rep_arg.as_object() {
        if vm.is_callable_handle(rh) {
            JsonReplacer::Fn(rep_arg)
        } else if vm
            .is_array_value(rep_arg)
            .map_err(|_| vm.type_error("Proxy has been revoked"))?
        {
            let len = vm.array_get_len(rh)?;
            let mut list = Vec::new();
            for i in 0..len {
                let v = vm.array_get_index(rh, i)?;
                let item = match v.kind() {
                    JsValueKind::String(sh) => vm.heap.materialize_string(sh),
                    JsValueKind::Int32(_) | JsValueKind::Float(_) => Some(vm.to_js_string(v)?),
                    JsValueKind::Object(oh) => {
                        let boxed = vm
                            .heap
                            .get_object(oh)
                            .and_then(|o| o.rare.as_ref())
                            .and_then(|r| r.boxed_value);
                        if boxed.is_some_and(|b| b.is_string() || b.is_number()) {
                            Some(vm.to_js_string(v)?)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(s) = item {
                    if !list.contains(&s) {
                        list.push(s);
                    }
                }
            }
            JsonReplacer::List(list)
        } else {
            JsonReplacer::None
        }
    } else {
        JsonReplacer::None
    };

    let space_arg = args.get(2).copied().unwrap_or(JsValue::UNDEFINED);
    let space_unboxed = if let Some(sh) = space_arg.as_object() {
        let boxed = vm
            .heap
            .get_object(sh)
            .and_then(|o| o.rare.as_ref())
            .and_then(|r| r.boxed_value);
        match boxed {
            Some(b) if b.is_number() => JsValue::from_f64(vm.to_number(space_arg)?),
            Some(b) if b.is_string() => {
                let s = vm.to_js_string(space_arg)?;
                JsValue::from_string(vm.heap.alloc_string(s))
            }
            _ => space_arg,
        }
    } else {
        space_arg
    };
    let indent = match space_unboxed.kind() {
        JsValueKind::Int32(_) | JsValueKind::Float(_) => {
            let n = vm.to_integer_or_infinity(space_unboxed)?;
            let clamped = if n < 1.0 { 0 } else { n.min(10.0) as usize };
            " ".repeat(clamped)
        }
        JsValueKind::String(sh) => {
            let s = vm.heap.materialize_string(sh).unwrap_or_default();
            let u16s: Vec<u16> = s.encode_utf16().take(10).collect();
            String::from_utf16_lossy(&u16s)
        }
        _ => String::new(),
    };

    let wrapper = vm.alloc_ordinary_object();
    let empty_k = vm.heap.intern_key("");
    vm.heap.object_define_data(
        wrapper,
        empty_k,
        val,
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    let mut stack = Vec::new();
    match stringify_prop(vm, wrapper, "", val, &replacer, &indent, 0, &mut stack)? {
        Some(s) => Ok(JsValue::from_string(vm.heap.alloc_string(s))),
        None => Ok(JsValue::UNDEFINED),
    }
}

#[allow(clippy::too_many_arguments)]
fn stringify_prop(
    vm: &mut JsVm,
    holder: HeapHandle,
    key: &str,
    mut val: JsValue,
    replacer: &JsonReplacer,
    indent: &str,
    depth: usize,
    stack: &mut Vec<HeapHandle>,
) -> Result<Option<String>, JsException> {
    // §25.5.2.1 step 2: invoke `.toJSON(key)` on objects AND BigInt primitives.
    if val.is_object() || val.is_bigint() {
        let lookup_h = if let Some(h) = val.as_object() {
            h
        } else {
            vm.bigint_proto
        };
        let k_to_json = vm.heap.intern_key("toJSON");
        let to_json = vm.get_property_by_key(lookup_h, k_to_json)?;
        if to_json
            .as_object()
            .is_some_and(|fh| vm.is_callable_handle(fh))
        {
            let kv = JsValue::from_string(vm.heap.alloc_string(key));
            val = vm.call_value(to_json, val, &[kv])?;
        }
    }
    if let JsonReplacer::Fn(rep_fn) = replacer {
        let kv = JsValue::from_string(vm.heap.alloc_string(key));
        val = vm.call_value(*rep_fn, JsValue::from_object(holder), &[kv, val])?;
    }
    // Unbox Number / String / Boolean / BigInt wrapper objects per §25.5.2.1 step 4.
    if let Some(h) = val.as_object() {
        let boxed = vm
            .heap
            .get_object(h)
            .and_then(|o| o.rare.as_ref())
            .and_then(|r| r.boxed_value);
        if let Some(b) = boxed {
            if b.is_number() {
                val = JsValue::from_f64(vm.to_number(val)?);
            } else if b.is_string() {
                let s = vm.to_js_string(val)?;
                val = JsValue::from_string(vm.heap.alloc_string(s));
            } else if b.is_bool() || b.is_bigint() {
                val = b;
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
                Ok(Some(vm.value_to_rust_string(JsValue::from_f64(f))))
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
            if vm
                .heap
                .get_object(h)
                .and_then(|o| o.rare.as_ref())
                .is_some_and(|r| r.is_raw_json)
            {
                let k_raw = vm.heap.intern_key("rawJSON");
                let raw_v = vm.get_property_by_key(h, k_raw)?;
                return Ok(Some(vm.value_to_rust_string(raw_v)));
            }
            if vm.is_callable_handle(h) {
                return Ok(None);
            }
            if stack.contains(&h) {
                return Err(vm.type_error("Converting circular structure to JSON"));
            }
            let is_array = vm
                .is_array_value(val)
                .map_err(|_| vm.type_error("Proxy has been revoked"))?;
            stack.push(h);
            let res = if is_array {
                let len = vm.array_get_len(h)?;
                if len > 4_000_000 {
                    stack.pop();
                    return Err(vm.range_error("Invalid string length"));
                }
                let mut items = Vec::with_capacity((len as usize).min(16_384));
                for i in 0..len {
                    let elem = vm.array_get_index(h, i)?;
                    let idx_s = i.to_string();
                    let rendered =
                        stringify_prop(vm, h, &idx_s, elem, replacer, indent, depth + 1, stack)?
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
                let keys: Vec<String> = match replacer {
                    JsonReplacer::List(list) => list.clone(),
                    _ => vm.own_enumerable_keys_including_elements(h),
                };
                let mut pairs = Vec::new();
                for k in keys {
                    let kv = JsValue::from_string(vm.heap.alloc_string(k.as_str()));
                    let pv = vm.get_elem(JsValue::from_object(h), kv)?;
                    if let Some(sv) =
                        stringify_prop(vm, h, &k, pv, replacer, indent, depth + 1, stack)?
                    {
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
            Ok(Some(res))
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
        define_method_len(vm, h, "fromAsync", 1, crate::promise::array_from_async);
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
        let mark = vm.temp_roots.len();
        let res = (|| {
            let out = alloc_from_target(vm, this_val, None)?;
            vm.temp_roots.push(JsValue::from_object(out));
            let items = vm.collect_iterable(src)?;
            for &it in &items {
                vm.temp_roots.push(it);
            }
            let count = items.len() as u64;
            for (i, mut v) in items.into_iter().enumerate() {
                if let Some(cb) = map_fn {
                    v = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(i as f64)])?;
                }
                vm.array_create_data_prop(out, i as u64, v)?;
            }
            vm.array_set_length(out, count)?;
            Ok(JsValue::from_object(out))
        })();
        vm.temp_roots.truncate(mark);
        return res;
    }
    let src_h = vm.to_object(src)?;
    let len = vm.array_get_len(src_h)?;
    check_iter_limit(vm, len)?;
    let mark = vm.temp_roots.len();
    let res = (|| {
        vm.temp_roots.push(JsValue::from_object(src_h));
        let out = alloc_from_target(vm, this_val, Some(len))?;
        vm.temp_roots.push(JsValue::from_object(out));
        for i in 0..len {
            let mut v = vm.array_get_index(src_h, i)?;
            if let Some(cb) = map_fn {
                v = vm.call_value(cb, this_arg, &[v, JsValue::from_f64(i as f64)])?;
            }
            vm.array_create_data_prop(out, i, v)?;
        }
        vm.array_set_length(out, len)?;
        Ok(JsValue::from_object(out))
    })();
    vm.temp_roots.truncate(mark);
    res
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
    let mark = vm.temp_roots.len();
    let res = (|| {
        vm.temp_roots.push(JsValue::from_object(h));
        let out = vm.array_species_create(h, 0)?;
        vm.temp_roots.push(JsValue::from_object(out));
        let _ = flatten_into_array(vm, out, h, len, 0, depth, None)?;
        Ok(JsValue::from_object(out))
    })();
    vm.temp_roots.truncate(mark);
    res
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
    let mark = vm.temp_roots.len();
    let res = (|| {
        vm.temp_roots.push(JsValue::from_object(h));
        let out = vm.array_species_create(h, 0)?;
        vm.temp_roots.push(JsValue::from_object(out));
        let _ = flatten_into_array(vm, out, h, len, 0, 1.0, Some((cb, this_arg)))?;
        Ok(JsValue::from_object(out))
    })();
    vm.temp_roots.truncate(mark);
    res
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
    let desc = crate::proxy::to_property_descriptor(
        vm,
        args.get(2).copied().unwrap_or(JsValue::UNDEFINED),
    )?;
    let key = vm.heap.intern_key(&prop_name);
    if !crate::proxy::mop_define_own_property(vm, obj, key, desc)? {
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
    let desc = crate::proxy::to_property_descriptor(
        vm,
        args.get(2).copied().unwrap_or(JsValue::UNDEFINED),
    )?;
    let key = vm.heap.intern_key(&prop_name);
    Ok(JsValue::from_bool(crate::proxy::mop_define_own_property(
        vm, obj, key, desc,
    )?))
}

pub(crate) fn define_properties_on_object(
    vm: &mut JsVm,
    obj: HeapHandle,
    props_arg: JsValue,
) -> Result<(), JsException> {
    let props_h = vm.to_object(props_arg)?;
    let keys = crate::proxy::mop_own_property_keys(vm, props_h)?;
    let mut pairs = Vec::with_capacity(keys.len());
    for (_, pk) in keys {
        if crate::proxy::mop_get_own_property(vm, props_h, pk)?
            .is_some_and(|d| d.enumerable == Some(true))
        {
            let desc_val = vm.get_property_by_key(props_h, pk)?;
            let desc = crate::proxy::to_property_descriptor(vm, desc_val)?;
            pairs.push((pk, desc));
        }
    }
    for (pk, desc) in pairs {
        if !crate::proxy::mop_define_own_property(vm, obj, pk, desc)? {
            return Err(vm.type_error("Cannot redefine property"));
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
            return Ok(crate::proxy::from_property_descriptor(
                vm,
                Some(crate::proxy::PropDesc {
                    value: Some(JsValue::from_f64(s.encode_utf16().count() as f64)),
                    writable: Some(false),
                    enumerable: Some(false),
                    configurable: Some(false),
                    ..crate::proxy::PropDesc::default()
                }),
            ));
        }
        if let Some(ch) = prop_name
            .parse::<usize>()
            .ok()
            .and_then(|idx| s.chars().nth(idx))
        {
            let cv = JsValue::from_string(vm.heap.alloc_string(ch.to_string()));
            return Ok(crate::proxy::from_property_descriptor(
                vm,
                Some(crate::proxy::PropDesc {
                    value: Some(cv),
                    writable: Some(false),
                    enumerable: Some(true),
                    configurable: Some(false),
                    ..crate::proxy::PropDesc::default()
                }),
            ));
        }
        return Ok(JsValue::UNDEFINED);
    }
    let Some(obj) = target.as_object() else {
        return Ok(JsValue::UNDEFINED);
    };
    let key = vm.heap.intern_key(&prop_name);
    let desc = crate::proxy::mop_get_own_property(vm, obj, key)?;
    Ok(crate::proxy::from_property_descriptor(vm, desc))
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
    let obj = vm.to_object(arg0(args))?;
    let out = vm.alloc_ordinary_object();
    let keys = crate::proxy::mop_own_property_keys(vm, obj)?;
    for (_, pk) in keys {
        if let Some(desc) = crate::proxy::mop_get_own_property(vm, obj, pk)? {
            let desc_val = crate::proxy::from_property_descriptor(vm, Some(desc));
            vm.heap
                .object_define_data(out, pk, desc_val, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
    }
    Ok(JsValue::from_object(out))
}

fn object_get_own_property_symbols(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let h = vm.to_object(arg0(args))?;
    let arr = vm.alloc_array();
    for (kv, _) in crate::proxy::mop_own_property_keys(vm, h)? {
        if kv.is_symbol() {
            vm.heap.object_push_element(arr, kv);
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
    Ok(JsValue::from_bool(crate::proxy::mop_is_extensible(vm, h)?))
}

fn object_prevent_extensions(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let v = arg0(args);
    if let Some(h) = v.as_object() {
        if !crate::proxy::mop_prevent_extensions(vm, h)? {
            return Err(vm.type_error("Cannot prevent extensions on object"));
        }
    }
    Ok(v)
}

pub(crate) fn set_object_integrity_level(
    vm: &mut JsVm,
    h: HeapHandle,
    freeze: bool,
) -> Result<bool, JsException> {
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::Proxy(_))
    ) {
        if !crate::proxy::mop_prevent_extensions(vm, h)? {
            return Ok(false);
        }
        let keys = crate::proxy::mop_own_property_keys(vm, h)?;
        if !freeze {
            for (_, pk) in keys {
                let d = crate::proxy::PropDesc {
                    configurable: Some(false),
                    ..crate::proxy::PropDesc::default()
                };
                if !crate::proxy::mop_define_own_property(vm, h, pk, d)? {
                    return Err(vm.type_error("Cannot seal property on Proxy"));
                }
            }
        } else {
            for (_, pk) in keys {
                if let Some(cur) = crate::proxy::mop_get_own_property(vm, h, pk)? {
                    let d = if cur.is_accessor() {
                        crate::proxy::PropDesc {
                            configurable: Some(false),
                            ..crate::proxy::PropDesc::default()
                        }
                    } else {
                        crate::proxy::PropDesc {
                            configurable: Some(false),
                            writable: Some(false),
                            ..crate::proxy::PropDesc::default()
                        }
                    };
                    if !crate::proxy::mop_define_own_property(vm, h, pk, d)? {
                        return Err(vm.type_error("Cannot freeze property on Proxy"));
                    }
                }
            }
        }
        return Ok(true);
    }
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
        None => return Ok(true),
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
    Ok(true)
}

fn object_seal(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let v = arg0(args);
    if let Some(h) = v.as_object() {
        if !set_object_integrity_level(vm, h, false)? {
            return Err(vm.type_error("Cannot seal object"));
        }
    }
    Ok(v)
}

fn test_object_integrity_level(
    vm: &mut JsVm,
    val: JsValue,
    frozen: bool,
) -> Result<bool, JsException> {
    let Some(h) = val.as_object() else {
        return Ok(true);
    };
    if matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::Proxy(_))
    ) {
        if crate::proxy::mop_is_extensible(vm, h)? {
            return Ok(false);
        }
        let keys = crate::proxy::mop_own_property_keys(vm, h)?;
        for (_, pk) in keys {
            if let Some(cur) = crate::proxy::mop_get_own_property(vm, h, pk)? {
                if cur.configurable == Some(true) {
                    return Ok(false);
                }
                if frozen && cur.is_data() && cur.writable == Some(true) {
                    return Ok(false);
                }
            }
        }
        return Ok(true);
    }
    let Some(o) = vm.heap.get_object(h) else {
        return Ok(true);
    };
    if o.extensible || !o.elements.present_indices().is_empty() {
        return Ok(false);
    }
    if frozen
        && matches!(o.kind, ObjectKind::Array)
        && vm
            .heap
            .object_get_own(h, vm.key_length, &vm.shapes)
            .is_none_or(|p| p.attrs.contains(PropertyAttrs::WRITABLE))
    {
        return Ok(false);
    }
    let ok = |a: PropertyAttrs| {
        !a.contains(PropertyAttrs::CONFIGURABLE)
            && !(frozen
                && !a.contains(PropertyAttrs::ACCESSOR)
                && a.contains(PropertyAttrs::WRITABLE))
    };
    Ok(if o.is_dictionary() {
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
    })
}

fn object_is_sealed(vm: &mut JsVm, _t: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    Ok(JsValue::from_bool(test_object_integrity_level(
        vm,
        arg0(args),
        false,
    )?))
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
    for (kv, _) in crate::proxy::mop_own_property_keys(vm, h)? {
        if kv.is_string() {
            vm.heap.object_push_element(arr, kv);
        }
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
    )?))
}
