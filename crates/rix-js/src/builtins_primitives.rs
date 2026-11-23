//! ECMAScript 2024 core primitive, numeric, text, temporal, regex, JSON,
//! and extended Array/Object standard library built-ins.

use crate::bigint::JsBigInt;
use crate::heap::{JsObject, ObjectKind};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue, JsValueKind};
use crate::vm::{JsException, JsVm};
use crate::vm_builtins::{define_global, define_method, define_method_len};
use regex::{Regex, RegexBuilder};
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
    install_regexp(vm);
    install_json(vm);
    install_extended_array_and_object(vm);
}

fn install_global_functions(vm: &mut JsVm) {
    let parse_int_fn = vm.alloc_native_fn("parseInt", global_parse_int);
    let parse_float_fn = vm.alloc_native_fn("parseFloat", global_parse_float);
    let is_nan_fn = vm.alloc_native_fn("isNaN", global_is_nan);
    let is_finite_fn = vm.alloc_native_fn("isFinite", global_is_finite);
    let enc_uri = vm.alloc_native_fn("encodeURI", global_encode_uri);
    let dec_uri = vm.alloc_native_fn("decodeURI", global_decode_uri);
    let enc_comp = vm.alloc_native_fn("encodeURIComponent", global_encode_uri_component);
    let dec_comp = vm.alloc_native_fn("decodeURIComponent", global_decode_uri_component);

    define_global(vm, "parseInt", JsValue::from_object(parse_int_fn));
    define_global(vm, "parseFloat", JsValue::from_object(parse_float_fn));
    define_global(vm, "isNaN", JsValue::from_object(is_nan_fn));
    define_global(vm, "isFinite", JsValue::from_object(is_finite_fn));
    define_global(vm, "encodeURI", JsValue::from_object(enc_uri));
    define_global(vm, "decodeURI", JsValue::from_object(dec_uri));
    define_global(vm, "encodeURIComponent", JsValue::from_object(enc_comp));
    define_global(vm, "decodeURIComponent", JsValue::from_object(dec_comp));
}

// ---------------------------------------------------------------------------
// String
// ---------------------------------------------------------------------------

fn install_string(vm: &mut JsVm) {
    let sp = vm.string_proto;
    define_method(vm, sp, "charAt", str_char_at);
    define_method(vm, sp, "charCodeAt", str_char_code_at);
    define_method(vm, sp, "codePointAt", str_code_point_at);
    define_method(vm, sp, "slice", str_slice);
    define_method(vm, sp, "substring", str_substring);
    define_method(vm, sp, "indexOf", str_index_of);
    define_method(vm, sp, "lastIndexOf", str_last_index_of);
    define_method(vm, sp, "includes", str_includes);
    define_method(vm, sp, "startsWith", str_starts_with);
    define_method(vm, sp, "endsWith", str_ends_with);
    define_method(vm, sp, "split", str_split);
    define_method(vm, sp, "trim", str_trim);
    define_method(vm, sp, "trimStart", str_trim_start);
    define_method(vm, sp, "trimEnd", str_trim_end);
    define_method(vm, sp, "toLowerCase", str_to_lower);
    define_method(vm, sp, "toUpperCase", str_to_upper);
    define_method(vm, sp, "repeat", str_repeat);
    define_method(vm, sp, "padStart", str_pad_start);
    define_method(vm, sp, "padEnd", str_pad_end);
    define_method(vm, sp, "replace", str_replace);
    define_method(vm, sp, "replaceAll", str_replace_all);
    define_method(vm, sp, "match", str_match);
    define_method(vm, sp, "search", str_search);
    define_method(vm, sp, "concat", str_concat);
    define_method(vm, sp, "at", str_at);
    define_method(vm, sp, "toString", str_to_string);
    define_method(vm, sp, "valueOf", str_to_string);
    let str_iter_fn = vm.alloc_native_fn("[Symbol.iterator]", str_symbol_iterator);
    let sym_iter_key = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        sp,
        sym_iter_key,
        JsValue::from_object(str_iter_fn),
        PropertyAttrs::METHOD,
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

fn str_symbol_iterator(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    if this_val.is_null() || this_val.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    let items: Vec<JsValue> = vm
        .value_to_rust_string(this_val)
        .chars()
        .map(|c| JsValue::from_string(vm.heap.alloc_string(c.to_string())))
        .collect();
    Ok(JsValue::from_object(
        crate::builtins_collections::alloc_values_iterator(vm, items),
    ))
}

fn string_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = args
        .first()
        .map(|&v| vm.value_to_rust_string(v))
        .unwrap_or_default();
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
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
    let s = String::from_utf16_lossy(&units);
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
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
    let out = String::from_utf16_lossy(&units);
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn str_char_at(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let idx = args
        .first()
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0);
    let ch = if idx >= 0 {
        s.chars()
            .nth(idx as usize)
            .map(|c| c.to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(ch)))
}

fn str_char_code_at(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let idx = args
        .first()
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0);
    if idx < 0 {
        return Ok(JsValue::NAN);
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    Ok(units
        .get(idx as usize)
        .map_or(JsValue::NAN, |&u| JsValue::from_i32(i32::from(u))))
}

fn str_code_point_at(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let idx = args
        .first()
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0);
    if idx < 0 {
        return Ok(JsValue::UNDEFINED);
    }
    Ok(s.chars()
        .nth(idx as usize)
        .map_or(JsValue::UNDEFINED, |c| JsValue::from_i32(c as i32)))
}

fn str_slice(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let raw_start = args
        .first()
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0) as i64;
    let raw_end = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .map_or(len, i64::from);
    let start = if raw_start < 0 {
        (len + raw_start).max(0)
    } else {
        raw_start.min(len)
    } as usize;
    let end = if raw_end < 0 {
        (len + raw_end).max(0)
    } else {
        raw_end.min(len)
    } as usize;
    let out: String = if start < end {
        chars[start..end].iter().collect()
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
    let s = vm.value_to_rust_string(this_val);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let a = args
        .first()
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0)
        .max(0) as i64;
    let b = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .map_or(len, |v| i64::from(v.max(0)));
    let start = a.min(len) as usize;
    let end = b.min(len) as usize;
    let (lo, hi) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let out: String = chars[lo..hi].iter().collect();
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_index_of(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let needle = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    let from = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0)
        .max(0) as usize;
    let byte_off = s.char_indices().nth(from).map_or(s.len(), |(b, _)| b);
    let found = s[byte_off..]
        .find(&needle)
        .map(|rel| s[..byte_off + rel].chars().count() as i32);
    Ok(JsValue::from_i32(found.unwrap_or(-1)))
}

fn str_last_index_of(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let needle = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    let found = s
        .rfind(&needle)
        .map(|byte_idx| s[..byte_idx].chars().count() as i32);
    Ok(JsValue::from_i32(found.unwrap_or(-1)))
}

fn str_includes(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let idx = str_index_of(vm, this_val, args)?;
    Ok(JsValue::from_bool(idx.as_i32() != Some(-1)))
}

fn str_starts_with(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let prefix = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    let pos = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0)
        .max(0) as usize;
    let byte_off = s.char_indices().nth(pos).map_or(s.len(), |(b, _)| b);
    Ok(JsValue::from_bool(s[byte_off..].starts_with(&prefix)))
}

fn str_ends_with(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let suffix = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
    let end_chars = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .map(|v| v.max(0) as usize);
    let slice = match end_chars {
        Some(n) => {
            let byte_end = s.char_indices().nth(n).map_or(s.len(), |(b, _)| b);
            &s[..byte_end]
        }
        None => s.as_str(),
    };
    Ok(JsValue::from_bool(slice.ends_with(&suffix)))
}

fn str_split(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let arr = vm.alloc_array();
    let sep = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let limit = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_u32(v))
        .transpose()?
        .unwrap_or(u32::MAX) as usize;
    if sep.is_undefined() {
        if limit > 0 {
            let v = JsValue::from_string(vm.heap.alloc_string(s));
            vm.heap.object_push_element(arr, v);
        }
        return Ok(JsValue::from_object(arr));
    }
    if let Some(re) = extract_compiled_regex(vm, sep)? {
        for part in re.split(&s).take(limit) {
            let v = JsValue::from_string(vm.heap.alloc_string(part));
            vm.heap.object_push_element(arr, v);
        }
        return Ok(JsValue::from_object(arr));
    }
    let sep_str = vm.value_to_rust_string(sep);
    if sep_str.is_empty() {
        for ch in s.chars().take(limit) {
            let v = JsValue::from_string(vm.heap.alloc_string(ch.to_string()));
            vm.heap.object_push_element(arr, v);
        }
    } else {
        for part in s.split(&sep_str).take(limit) {
            let v = JsValue::from_string(vm.heap.alloc_string(part));
            vm.heap.object_push_element(arr, v);
        }
    }
    Ok(JsValue::from_object(arr))
}

fn str_trim(vm: &mut JsVm, this_val: JsValue, _args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    Ok(JsValue::from_string(vm.heap.alloc_string(s.trim())))
}

fn str_trim_start(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    Ok(JsValue::from_string(vm.heap.alloc_string(s.trim_start())))
}

fn str_trim_end(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    Ok(JsValue::from_string(vm.heap.alloc_string(s.trim_end())))
}

fn str_to_lower(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val).to_lowercase();
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn str_to_upper(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val).to_uppercase();
    Ok(JsValue::from_string(vm.heap.alloc_string(s)))
}

fn str_repeat(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let count = args
        .first()
        .map(|&v| vm.to_number(v))
        .transpose()?
        .unwrap_or(0.0);
    if count < 0.0 || !count.is_finite() {
        return Err(vm.range_error("Invalid count value for String.prototype.repeat"));
    }
    let count_usize = count.trunc() as usize;
    if !s.is_empty() && s.len().saturating_mul(count_usize) > 32 * 1024 * 1024 {
        return Err(vm.range_error("Invalid string length"));
    }
    Ok(JsValue::from_string(
        vm.heap.alloc_string(s.repeat(count_usize)),
    ))
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
    let s = vm.value_to_rust_string(this_val);
    let target_len = args
        .first()
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0)
        .max(0) as usize;
    let cur_len = s.chars().count();
    if target_len <= cur_len {
        return Ok(JsValue::from_string(vm.heap.alloc_string(s)));
    }
    if target_len > 32 * 1024 * 1024 {
        return Err(vm.range_error("Invalid string length"));
    }
    let fill = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.value_to_rust_string(v))
        .unwrap_or_else(|| " ".to_owned());
    if fill.is_empty() {
        return Ok(JsValue::from_string(vm.heap.alloc_string(s)));
    }
    let need = target_len - cur_len;
    let pad: String = fill.chars().cycle().take(need).collect();
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
    let s = vm.value_to_rust_string(this_val);
    let search = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let repl_val = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    let repl_is_fn = repl_val
        .as_object()
        .is_some_and(|h| vm.is_callable_handle(h));

    if let Some((re, is_global)) = extract_compiled_regex_with_global(vm, search)? {
        if force_all && !is_global {
            return Err(vm.type_error("String.prototype.replaceAll called with non-global RegExp"));
        }
        let all = force_all || is_global;
        if repl_is_fn {
            let mut out = String::new();
            let mut last = 0usize;
            for caps in re.captures_iter(&s) {
                let m = caps.get(0).unwrap();
                out.push_str(&s[last..m.start()]);
                let mut cb_args = Vec::with_capacity(caps.len() + 2);
                for i in 0..caps.len() {
                    cb_args.push(match caps.get(i) {
                        Some(g) => JsValue::from_string(vm.heap.alloc_string(g.as_str())),
                        None => JsValue::UNDEFINED,
                    });
                }
                cb_args.push(JsValue::from_i32(m.start() as i32));
                cb_args.push(JsValue::from_string(vm.heap.alloc_string(s.as_str())));
                let rep = vm.call_value(repl_val, JsValue::UNDEFINED, &cb_args)?;
                out.push_str(&vm.value_to_rust_string(rep));
                last = m.end();
                if !all {
                    break;
                }
            }
            out.push_str(&s[last..]);
            return Ok(JsValue::from_string(vm.heap.alloc_string(out)));
        }
        let repl_str = vm.value_to_rust_string(repl_val);
        let out = if all {
            re.replace_all(&s, repl_str.as_str()).into_owned()
        } else {
            re.replace(&s, repl_str.as_str()).into_owned()
        };
        return Ok(JsValue::from_string(vm.heap.alloc_string(out)));
    }

    let needle = vm.value_to_rust_string(search);
    if repl_is_fn {
        let mut out = String::new();
        let mut cursor = 0usize;
        while let Some(rel) = s[cursor..].find(&needle) {
            let pos = cursor + rel;
            out.push_str(&s[cursor..pos]);
            let m_val = JsValue::from_string(vm.heap.alloc_string(needle.as_str()));
            let s_val = JsValue::from_string(vm.heap.alloc_string(s.as_str()));
            let rep = vm.call_value(
                repl_val,
                JsValue::UNDEFINED,
                &[m_val, JsValue::from_i32(pos as i32), s_val],
            )?;
            out.push_str(&vm.value_to_rust_string(rep));
            cursor = pos + needle.len().max(1);
            if !force_all || needle.is_empty() {
                break;
            }
        }
        out.push_str(&s[cursor..]);
        return Ok(JsValue::from_string(vm.heap.alloc_string(out)));
    }
    let repl_str = vm.value_to_rust_string(repl_val);
    let out = if force_all {
        s.replace(&needle, &repl_str)
    } else {
        s.replacen(&needle, &repl_str, 1)
    };
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_match(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let arg = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let (re, is_global) = match extract_compiled_regex_with_global(vm, arg)? {
        Some(pair) => pair,
        None => {
            let pat = if arg.is_undefined() {
                String::new()
            } else {
                vm.value_to_rust_string(arg)
            };
            (compile_js_regex(vm, &pat, "")?, false)
        }
    };
    if is_global {
        let matches: Vec<&str> = re.find_iter(&s).map(|m| m.as_str()).collect();
        if matches.is_empty() {
            return Ok(JsValue::NULL);
        }
        let arr = vm.alloc_array();
        for m in matches {
            let v = JsValue::from_string(vm.heap.alloc_string(m));
            vm.heap.object_push_element(arr, v);
        }
        return Ok(JsValue::from_object(arr));
    }
    Ok(build_exec_array(vm, &re, &s, 0).unwrap_or(JsValue::NULL))
}

fn str_search(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let arg = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let re = match extract_compiled_regex(vm, arg)? {
        Some(r) => r,
        None => {
            let pat = if arg.is_undefined() {
                String::new()
            } else {
                vm.value_to_rust_string(arg)
            };
            compile_js_regex(vm, &pat, "")?
        }
    };
    let idx = re
        .find(&s)
        .map_or(-1, |m| s[..m.start()].chars().count() as i32);
    Ok(JsValue::from_i32(idx))
}

fn str_concat(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let mut out = vm.value_to_rust_string(this_val);
    for &a in args {
        out.push_str(&vm.value_to_rust_string(a));
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn str_at(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.value_to_rust_string(this_val);
    let chars: Vec<char> = s.chars().collect();
    let idx = args
        .first()
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0) as i64;
    let pos = if idx < 0 {
        chars.len() as i64 + idx
    } else {
        idx
    };
    if pos < 0 || (pos as usize) >= chars.len() {
        return Ok(JsValue::UNDEFINED);
    }
    Ok(JsValue::from_string(
        vm.heap.alloc_string(chars[pos as usize].to_string()),
    ))
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

fn number_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let n = match args.first() {
        Some(&v) => vm.to_number(v)?,
        None => 0.0,
    };
    Ok(JsValue::from_f64(n))
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
    let radix = args
        .first()
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(10);
    if !(2..=36).contains(&radix) {
        return Err(vm.range_error("toString() radix must be between 2 and 36"));
    }
    if let Some(bh) = this_val.as_bigint() {
        let s = vm
            .heap
            .get_bigint(bh)
            .map_or_else(|| "0".to_string(), |bi| bi.to_radix_string(radix as u32));
        return Ok(JsValue::from_string(vm.heap.alloc_string(s)));
    }
    let n = vm.to_number(this_val)?;
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
    if this_val.is_bigint() {
        return Ok(this_val);
    }
    Ok(JsValue::from_f64(vm.to_number(this_val)?))
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
        let s = if vm.to_boolean(this_val) {
            "true"
        } else {
            "false"
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    define_method(vm, bp, "valueOf", |vm, this_val, _| {
        Ok(JsValue::from_bool(vm.to_boolean(this_val)))
    });
    let ctor = vm.alloc_native_fn("Boolean", |vm, _this, args| {
        Ok(JsValue::from_bool(
            args.first().copied().is_some_and(|v| vm.to_boolean(v)),
        ))
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
    let bi_proto = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(bi_proto).unwrap().is_prototype = true;
    define_method(vm, bi_proto, "toString", |vm, this_val, _| {
        let Some(h) = this_val.as_bigint() else {
            return Err(vm.type_error("BigInt.prototype.toString requires a BigInt"));
        };
        let s = vm
            .heap
            .get_bigint(h)
            .map(ToString::to_string)
            .unwrap_or_else(|| "0".to_owned());
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    define_method(vm, bi_proto, "valueOf", |vm, this_val, _| {
        if this_val.as_bigint().is_some() {
            Ok(this_val)
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
    let ctor = vm.alloc_native_fn("Symbol", |vm, _this, args| {
        let desc = args
            .first()
            .filter(|v| !v.is_undefined())
            .map(|&v| vm.value_to_rust_string(v));
        let id = vm.heap.alloc_symbol(desc.clone());
        if let Some(d) = desc {
            vm.symbol_descriptions.insert(id, d);
        }
        Ok(JsValue::from_symbol(id))
    });
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

    for well_known in [
        "iterator",
        "asyncIterator",
        "toStringTag",
        "hasInstance",
        "toPrimitive",
        "species",
    ] {
        let id = if well_known == "iterator" {
            vm.sym_iterator
        } else if well_known == "asyncIterator" {
            vm.sym_async_iterator
        } else {
            vm.heap.alloc_symbol(Some(format!("Symbol.{well_known}")))
        };
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
// RegExp (backed by `regex`)
// ---------------------------------------------------------------------------

fn install_regexp(vm: &mut JsVm) {
    let rp = vm.regexp_proto;
    define_method(vm, rp, "test", regexp_test);
    define_method(vm, rp, "exec", regexp_exec);
    define_method(vm, rp, "toString", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("RegExp.prototype.toString called on non-RegExp"));
        };
        let (p, f) = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::RegExp(r)) => (r.pattern.clone(), r.flags.clone()),
            _ => return Err(vm.type_error("RegExp.prototype.toString called on non-RegExp")),
        };
        Ok(JsValue::from_string(
            vm.heap.alloc_string(format!("/{p}/{f}")),
        ))
    });

    let ctor = vm.alloc_native_fn("RegExp", |vm, _this, args| {
        let pat = args
            .first()
            .filter(|v| !v.is_undefined())
            .map(|&v| vm.value_to_rust_string(v))
            .unwrap_or_default();
        let flags = args
            .get(1)
            .filter(|v| !v.is_undefined())
            .map(|&v| vm.value_to_rust_string(v))
            .unwrap_or_default();
        let _ = compile_js_regex(vm, &pat, &flags)?;
        Ok(JsValue::from_object(vm.alloc_regexp(pat, flags)))
    });
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(rp),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    link_ctor_meta(vm, ctor, rp, 2, true);
    define_global(vm, "RegExp", JsValue::from_object(ctor));
}

fn compile_js_regex(vm: &mut JsVm, pattern: &str, flags: &str) -> Result<Regex, JsException> {
    let mut builder = RegexBuilder::new(pattern);
    builder.case_insensitive(flags.contains('i'));
    builder.multi_line(flags.contains('m'));
    builder.dot_matches_new_line(flags.contains('s'));
    builder.build().map_err(|e| {
        vm.make_error(
            "SyntaxError",
            format!("Invalid RegExp /{pattern}/{flags}: {e}"),
        )
    })
}

fn extract_compiled_regex(vm: &mut JsVm, val: JsValue) -> Result<Option<Regex>, JsException> {
    Ok(extract_compiled_regex_with_global(vm, val)?.map(|(r, _)| r))
}

fn extract_compiled_regex_with_global(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<Option<(Regex, bool)>, JsException> {
    let Some(h) = val.as_object() else {
        return Ok(None);
    };
    let Some((pat, flags)) = vm.heap.get_object(h).and_then(|o| match &o.kind {
        ObjectKind::RegExp(r) => Some((r.pattern.clone(), r.flags.clone())),
        _ => None,
    }) else {
        return Ok(None);
    };
    let re = compile_js_regex(vm, &pat, &flags)?;
    Ok(Some((re, flags.contains('g'))))
}

fn regexp_test(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let exec_res = regexp_exec(vm, this_val, args)?;
    Ok(JsValue::from_bool(!exec_res.is_null()))
}

fn regexp_exec(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype.exec called on non-RegExp"));
    };
    let (re, is_global) = extract_compiled_regex_with_global(vm, this_val)?
        .ok_or_else(|| vm.type_error("RegExp.prototype.exec called on non-RegExp"))?;
    let input = vm.value_to_rust_string(arg0(args));
    let k_last = vm.heap.intern_key("lastIndex");
    let start_char = if is_global {
        let li = vm.get_property_by_key(h, k_last)?;
        vm.to_i32(li)?.max(0) as usize
    } else {
        0
    };
    let start_byte = input
        .char_indices()
        .nth(start_char)
        .map_or(input.len(), |(b, _)| b);

    if let Some(arr) = build_exec_array(vm, &re, &input, start_byte) {
        if is_global {
            if let Some(m) = re.find_at(&input, start_byte) {
                let next_char = input[..m.end()].chars().count() as i32;
                vm.set_property_by_key(h, k_last, JsValue::from_i32(next_char))?;
            }
        }
        Ok(arr)
    } else {
        if is_global {
            vm.set_property_by_key(h, k_last, JsValue::from_i32(0))?;
        }
        Ok(JsValue::NULL)
    }
}

fn build_exec_array(vm: &mut JsVm, re: &Regex, input: &str, start_byte: usize) -> Option<JsValue> {
    let caps = re.captures_at(input, start_byte)?;
    let full = caps.get(0)?;
    let arr = vm.alloc_array();
    for i in 0..caps.len() {
        let v = match caps.get(i) {
            Some(g) => JsValue::from_string(vm.heap.alloc_string(g.as_str())),
            None => JsValue::UNDEFINED,
        };
        vm.heap.object_push_element(arr, v);
    }
    let idx_chars = input[..full.start()].chars().count() as i32;
    let k_index = vm.heap.intern_key("index");
    let k_input = vm.heap.intern_key("input");
    vm.heap.object_define_data(
        arr,
        k_index,
        JsValue::from_i32(idx_chars),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    let inp_v = JsValue::from_string(vm.heap.alloc_string(input));
    vm.heap
        .object_define_data(arr, k_input, inp_v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    Some(JsValue::from_object(arr))
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
    define_method(vm, ap, "concat", array_concat);
    define_method(vm, ap, "reverse", array_reverse);
    define_method(vm, ap, "sort", array_sort);
    define_method(vm, ap, "splice", array_splice);
    define_method(vm, ap, "shift", array_shift);
    define_method(vm, ap, "unshift", array_unshift);
    define_method_len(vm, ap, "find", 1, array_find);
    define_method_len(vm, ap, "findIndex", 1, array_find_index);
    define_method_len(vm, ap, "findLast", 1, array_find_last);
    define_method_len(vm, ap, "findLastIndex", 1, array_find_last_index);
    define_method_len(vm, ap, "reduceRight", 1, array_reduce_right);
    define_method_len(vm, ap, "lastIndexOf", 1, array_last_index_of);
    define_method_len(vm, ap, "copyWithin", 2, array_copy_within);
    define_method_len(vm, ap, "toReversed", 0, array_to_reversed);
    define_method_len(vm, ap, "toSorted", 1, array_to_sorted);
    define_method_len(vm, ap, "toSpliced", 2, array_to_spliced);
    define_method_len(vm, ap, "with", 2, array_with);
    define_method(vm, ap, "some", array_some);
    define_method(vm, ap, "every", array_every);
    define_method(vm, ap, "flat", array_flat);
    define_method(vm, ap, "flatMap", array_flat_map);
    define_method(vm, ap, "fill", array_fill);
    define_method(vm, ap, "at", array_at);

    let k_arr = vm.heap.intern_key("Array");
    if let Ok(arr_ctor) = vm.get_property_by_key(vm.global, k_arr) {
        if let Some(h) = arr_ctor.as_object() {
            define_method_len(vm, h, "from", 1, array_from);
            define_method_len(vm, h, "of", 0, array_of);
        }
    }

    let k_obj = vm.heap.intern_key("Object");
    if let Ok(obj_ctor) = vm.get_property_by_key(vm.global, k_obj) {
        if let Some(h) = obj_ctor.as_object() {
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
    }

    let k_ref = vm.heap.intern_key("Reflect");
    if let Ok(ref_obj) = vm.get_property_by_key(vm.global, k_ref) {
        if let Some(h) = ref_obj.as_object() {
            define_method_len(vm, h, "defineProperty", 3, reflect_define_property);
            let f: crate::vm::NativeFnPtr = reflect_get_own_descriptor;
            define_method_len(vm, h, "getOwnPropertyDescriptor", 2, f);
        }
    }
}

fn array_from(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let src = arg0(args);
    let map_fn = args
        .get(1)
        .copied()
        .filter(|v| v.as_object().is_some_and(|h| vm.is_callable_handle(h)));
    let out = vm.alloc_array();
    if let Some(sh) = src.as_string() {
        let s = vm.heap.materialize_string(sh).unwrap_or_default();
        for (i, ch) in s.chars().enumerate() {
            let mut v = JsValue::from_string(vm.heap.alloc_string(ch.to_string()));
            if let Some(cb) = map_fn {
                v = vm.call_value(cb, JsValue::UNDEFINED, &[v, JsValue::from_i32(i as i32)])?;
            }
            vm.heap.object_push_element(out, v);
        }
    } else if let Some(h) = src.as_object() {
        let iter_m = vm.get_property_by_key(h, vm.key_symbol_iterator)?;
        if !iter_m.is_undefined() && !iter_m.is_null() {
            for (i, mut v) in vm.collect_iterable(src)?.into_iter().enumerate() {
                if let Some(cb) = map_fn {
                    v = vm.call_value(cb, JsValue::UNDEFINED, &[v, JsValue::from_i32(i as i32)])?;
                }
                vm.heap.object_push_element(out, v);
            }
        } else {
            let len_val = vm.get_property_by_key(h, vm.key_length)?;
            let len = vm.to_u32(len_val)?;
            let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000) as u32;
            if len > limit {
                return Err(vm.range_error("Invalid array length"));
            }
            for i in 0..len {
                let mut v = vm.get_elem(src, JsValue::from_i32(i as i32))?;
                if let Some(cb) = map_fn {
                    v = vm.call_value(cb, JsValue::UNDEFINED, &[v, JsValue::from_i32(i as i32)])?;
                }
                vm.heap.object_push_element(out, v);
            }
        }
    }
    Ok(JsValue::from_object(out))
}

fn array_of(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let out = vm.alloc_array();
    for &a in args {
        vm.heap.object_push_element(out, a);
    }
    Ok(JsValue::from_object(out))
}

fn array_concat(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let out = vm.alloc_array();
    let push_item = |vm: &mut JsVm, item: JsValue| {
        if let Some(h) = item.as_object() {
            if matches!(
                vm.heap.get_object(h).map(|o| &o.kind),
                Some(ObjectKind::Array)
            ) {
                let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
                for i in 0..len {
                    let v = vm
                        .heap
                        .get_object(h)
                        .and_then(|o| o.elements.get(i))
                        .unwrap_or(JsValue::UNDEFINED);
                    vm.heap.object_push_element(out, v);
                }
                return;
            }
        }
        vm.heap.object_push_element(out, item);
    };
    push_item(vm, this_val);
    for &a in args {
        push_item(vm, a);
    }
    Ok(JsValue::from_object(out))
}

fn array_reverse(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    if let Some(h) = this_val.as_object() {
        let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
        if len > 1_000_000 {
            return Err(vm.range_error("Invalid array length"));
        }
        let items: Vec<Option<JsValue>> = (0..len)
            .map(|i| vm.heap.get_object(h).and_then(|o| o.elements.get(i)))
            .collect();
        for (i, item) in items.into_iter().rev().enumerate() {
            if let Some(o) = vm.heap.get_object_mut(h) {
                match item {
                    Some(v) => o.elements.set(i as u32, v),
                    None => {
                        o.elements.delete(i as u32);
                    }
                }
            }
        }
    }
    Ok(this_val)
}

fn array_sort(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(this_val);
    };
    let cmp_fn = args
        .first()
        .copied()
        .filter(|v| v.as_object().is_some_and(|fh| vm.is_callable_handle(fh)));
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    let mut vals: Vec<JsValue> = if len > 65_536 {
        vm.heap
            .get_object(h)
            .map(|o| {
                o.elements
                    .present_indices()
                    .into_iter()
                    .filter_map(|i| o.elements.get(i))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        (0..len)
            .filter_map(|i| vm.heap.get_object(h).and_then(|o| o.elements.get(i)))
            .collect()
    };
    // Insertion/merge stable sort supporting JS comparator callbacks cleanly.
    for i in 1..vals.len() {
        let mut j = i;
        while j > 0 {
            let order = if let Some(cb) = cmp_fn {
                let r = vm.call_value(cb, JsValue::UNDEFINED, &[vals[j - 1], vals[j]])?;
                vm.to_number(r)?
            } else {
                let a = vm.value_to_rust_string(vals[j - 1]);
                let b = vm.value_to_rust_string(vals[j]);
                match a.cmp(&b) {
                    std::cmp::Ordering::Less => -1.0,
                    std::cmp::Ordering::Equal => 0.0,
                    std::cmp::Ordering::Greater => 1.0,
                }
            };
            if order > 0.0 {
                vals.swap(j - 1, j);
                j -= 1;
            } else {
                break;
            }
        }
    }
    for (i, v) in vals.into_iter().enumerate() {
        if let Some(o) = vm.heap.get_object_mut(h) {
            o.elements.set(i as u32, v);
        }
    }
    Ok(this_val)
}

fn array_splice(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::from_object(vm.alloc_array()));
    };
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len()) as i64;
    let raw_start = args
        .first()
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0) as i64;
    let start = if raw_start < 0 {
        (len + raw_start).max(0)
    } else {
        raw_start.min(len)
    } as usize;
    let delete_count = if args.len() <= 1 {
        (len as usize).saturating_sub(start)
    } else {
        vm.to_i32(args[1])?.max(0) as usize
    }
    .min((len as usize).saturating_sub(start));
    let insert = if args.len() > 2 { &args[2..] } else { &[] };

    if len > 1_000_000 {
        return Err(vm.range_error("Invalid array length"));
    }
    let mut all: Vec<JsValue> = (0..len as u32)
        .map(|i| {
            vm.heap
                .get_object(h)
                .and_then(|o| o.elements.get(i))
                .unwrap_or(JsValue::UNDEFINED)
        })
        .collect();
    let removed: Vec<JsValue> = all
        .splice(start..start + delete_count, insert.iter().copied())
        .collect();

    if let Some(o) = vm.heap.get_object_mut(h) {
        o.elements.set_len(0);
    }
    for v in all {
        vm.heap.object_push_element(h, v);
    }
    let out = vm.alloc_array();
    for v in removed {
        vm.heap.object_push_element(out, v);
    }
    Ok(JsValue::from_object(out))
}

fn array_shift(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let removed = array_splice(vm, this_val, &[JsValue::from_i32(0), JsValue::from_i32(1)])?;
    vm.get_elem(removed, JsValue::from_i32(0))
}

fn array_unshift(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let mut splice_args = vec![JsValue::from_i32(0), JsValue::from_i32(0)];
    splice_args.extend_from_slice(args);
    let _ = array_splice(vm, this_val, &splice_args)?;
    let len = this_val
        .as_object()
        .and_then(|h| vm.heap.get_object(h))
        .map_or(0, |o| o.elements.len());
    Ok(JsValue::from_f64(f64::from(len)))
}

fn array_find(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::UNDEFINED);
    };
    let cb = arg0(args);
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000) as u32;
    if len > limit {
        return Err(vm.range_error("Script execution iteration limit exceeded"));
    }
    for i in 0..len {
        let v = vm
            .heap
            .get_object(h)
            .and_then(|o| o.elements.get(i))
            .unwrap_or(JsValue::UNDEFINED);
        let ok = vm.call_value(
            cb,
            JsValue::UNDEFINED,
            &[v, JsValue::from_i32(i as i32), this_val],
        )?;
        if vm.to_boolean(ok) {
            return Ok(v);
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn array_find_index(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::from_i32(-1));
    };
    let cb = arg0(args);
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    let limit = vm.max_loop_iterations.unwrap_or(1_000_000).min(1_000_000) as u32;
    if len > limit {
        return Err(vm.range_error("Script execution iteration limit exceeded"));
    }
    for i in 0..len {
        let v = vm
            .heap
            .get_object(h)
            .and_then(|o| o.elements.get(i))
            .unwrap_or(JsValue::UNDEFINED);
        let ok = vm.call_value(
            cb,
            JsValue::UNDEFINED,
            &[v, JsValue::from_i32(i as i32), this_val],
        )?;
        if vm.to_boolean(ok) {
            return Ok(JsValue::from_i32(i as i32));
        }
    }
    Ok(JsValue::from_i32(-1))
}

fn array_some(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let idx = array_find_index(vm, this_val, args)?;
    Ok(JsValue::from_bool(idx.as_i32() != Some(-1)))
}

fn array_every(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::TRUE);
    };
    let cb = arg0(args);
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    if len > 65_536 {
        let indices = vm
            .heap
            .get_object(h)
            .map(|o| o.elements.present_indices())
            .unwrap_or_default();
        for i in indices {
            if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                let ok = vm.call_value(
                    cb,
                    JsValue::UNDEFINED,
                    &[v, JsValue::from_f64(f64::from(i)), this_val],
                )?;
                if !vm.to_boolean(ok) {
                    return Ok(JsValue::FALSE);
                }
            }
        }
        return Ok(JsValue::TRUE);
    }
    for i in 0..len {
        if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
            let ok = vm.call_value(
                cb,
                JsValue::UNDEFINED,
                &[v, JsValue::from_i32(i as i32), this_val],
            )?;
            if !vm.to_boolean(ok) {
                return Ok(JsValue::FALSE);
            }
        }
    }
    Ok(JsValue::TRUE)
}

fn array_flat(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let depth = args
        .first()
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(1)
        .clamp(0, 128) as u32;
    let out = vm.alloc_array();
    if let Some(h) = this_val.as_object() {
        flatten_into(vm, h, out, depth);
    }
    Ok(JsValue::from_object(out))
}

fn flatten_into(vm: &mut JsVm, src: HeapHandle, out: HeapHandle, depth: u32) {
    let len = vm.heap.get_object(src).map_or(0, |o| o.elements.len());
    if len > 65_536 {
        let indices = vm
            .heap
            .get_object(src)
            .map(|o| o.elements.present_indices())
            .unwrap_or_default();
        for i in indices {
            if let Some(v) = vm.heap.get_object(src).and_then(|o| o.elements.get(i)) {
                if depth > 0 {
                    if let Some(sub) = v.as_object() {
                        if matches!(
                            vm.heap.get_object(sub).map(|o| &o.kind),
                            Some(ObjectKind::Array)
                        ) {
                            flatten_into(vm, sub, out, depth - 1);
                            continue;
                        }
                    }
                }
                vm.heap.object_push_element(out, v);
            }
        }
        return;
    }
    for i in 0..len {
        if let Some(v) = vm.heap.get_object(src).and_then(|o| o.elements.get(i)) {
            if depth > 0 {
                if let Some(sub) = v.as_object() {
                    if matches!(
                        vm.heap.get_object(sub).map(|o| &o.kind),
                        Some(ObjectKind::Array)
                    ) {
                        flatten_into(vm, sub, out, depth - 1);
                        continue;
                    }
                }
            }
            vm.heap.object_push_element(out, v);
        }
    }
}

fn array_flat_map(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::from_object(vm.alloc_array()));
    };
    let cb = arg0(args);
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    let out = vm.alloc_array();
    if len > 65_536 {
        let indices = vm
            .heap
            .get_object(h)
            .map(|o| o.elements.present_indices())
            .unwrap_or_default();
        for i in indices {
            if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                let mapped = vm.call_value(
                    cb,
                    JsValue::UNDEFINED,
                    &[v, JsValue::from_f64(f64::from(i)), this_val],
                )?;
                if let Some(sub) = mapped.as_object() {
                    if matches!(
                        vm.heap.get_object(sub).map(|o| &o.kind),
                        Some(ObjectKind::Array)
                    ) {
                        flatten_into(vm, sub, out, 0);
                        continue;
                    }
                }
                vm.heap.object_push_element(out, mapped);
            }
        }
        return Ok(JsValue::from_object(out));
    }
    for i in 0..len {
        if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
            let mapped = vm.call_value(
                cb,
                JsValue::UNDEFINED,
                &[v, JsValue::from_i32(i as i32), this_val],
            )?;
            if let Some(sub) = mapped.as_object() {
                if matches!(
                    vm.heap.get_object(sub).map(|o| &o.kind),
                    Some(ObjectKind::Array)
                ) {
                    flatten_into(vm, sub, out, 0);
                    continue;
                }
            }
            vm.heap.object_push_element(out, mapped);
        }
    }
    Ok(JsValue::from_object(out))
}

fn array_fill(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(this_val);
    };
    let val = arg0(args);
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len()) as i64;
    let raw_start = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0) as i64;
    let raw_end = args
        .get(2)
        .filter(|v| !v.is_undefined())
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .map_or(len, i64::from);
    let start = if raw_start < 0 {
        (len + raw_start).max(0)
    } else {
        raw_start.min(len)
    } as u32;
    let end = if raw_end < 0 {
        (len + raw_end).max(0)
    } else {
        raw_end.min(len)
    } as u32;
    if end.saturating_sub(start) > 1_000_000 {
        return Err(vm.range_error("Invalid array fill length"));
    }
    for i in start..end {
        if let Some(o) = vm.heap.get_object_mut(h) {
            o.elements.set(i, val);
        }
        vm.heap.write_barrier(h, val);
    }
    Ok(this_val)
}

fn array_at(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Ok(JsValue::UNDEFINED);
    };
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len()) as i64;
    let idx = args
        .first()
        .map(|&v| vm.to_i32(v))
        .transpose()?
        .unwrap_or(0) as i64;
    let pos = if idx < 0 { len + idx } else { idx };
    if pos < 0 || pos >= len {
        return Ok(JsValue::UNDEFINED);
    }
    Ok(vm
        .heap
        .get_object(h)
        .and_then(|o| o.elements.get(pos as u32))
        .unwrap_or(JsValue::UNDEFINED))
}

fn array_find_last_impl(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
    return_index: bool,
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.findLast called on non-object"));
    };
    let (cb, this_arg) = (
        arg0(args),
        args.get(1).copied().unwrap_or(JsValue::UNDEFINED),
    );
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    for i in (0..len).rev() {
        let v = vm
            .heap
            .get_object(h)
            .and_then(|o| o.elements.get(i))
            .unwrap_or(JsValue::UNDEFINED);
        let r = vm.call_value(cb, this_arg, &[v, JsValue::from_i32(i as i32), this_val])?;
        if vm.to_boolean(r) {
            return Ok(if return_index {
                JsValue::from_i32(i as i32)
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

fn array_find_last(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    array_find_last_impl(vm, this_val, args, false)
}

fn array_find_last_index(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    array_find_last_impl(vm, this_val, args, true)
}

fn array_reduce_right(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.reduceRight called on non-object"));
    };
    let cb = arg0(args);
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    let mut acc = args.get(1).copied();
    for i in (0..len).rev() {
        if let Some(elem) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
            acc = Some(if let Some(cur) = acc {
                vm.call_value(
                    cb,
                    JsValue::UNDEFINED,
                    &[cur, elem, JsValue::from_i32(i as i32), this_val],
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
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.lastIndexOf called on non-object"));
    };
    let target = arg0(args);
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len()) as i64;
    if len == 0 {
        return Ok(JsValue::from_i32(-1));
    }
    let from = if let Some(&fi) = args.get(1) {
        let n = vm.to_number(fi)?;
        let n = if n.is_nan() { 0 } else { n.trunc() as i64 };
        if n >= 0 {
            n.min(len - 1)
        } else {
            len + n
        }
    } else {
        len - 1
    };
    if from < 0 {
        return Ok(JsValue::from_i32(-1));
    }
    for i in (0..=from as u32).rev() {
        if let Some(v) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
            if vm.strict_eq(v, target) {
                return Ok(JsValue::from_i32(i as i32));
            }
        }
    }
    Ok(JsValue::from_i32(-1))
}

fn clamp_idx(idx: i64, len: i64) -> usize {
    if idx < 0 {
        (len + idx).max(0) as usize
    } else {
        idx.min(len) as usize
    }
}

fn clone_array_elements(vm: &mut JsVm, h: HeapHandle, reverse: bool) -> HeapHandle {
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
    let out = vm.alloc_array();
    for step in 0..len {
        let i = if reverse { len - 1 - step } else { step };
        let v = vm
            .heap
            .get_object(h)
            .and_then(|o| o.elements.get(i))
            .unwrap_or(JsValue::UNDEFINED);
        vm.heap.object_push_element(out, v);
    }
    out
}

fn array_copy_within(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.copyWithin called on non-object"));
    };
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len()) as i64;
    let to = clamp_idx(vm.to_i32(arg0(args))? as i64, len);
    let from = clamp_idx(
        args.get(1).map(|&v| vm.to_i32(v)).transpose()?.unwrap_or(0) as i64,
        len,
    );
    let end = match args.get(2).copied() {
        Some(v) if !v.is_undefined() => clamp_idx(vm.to_i32(v)? as i64, len),
        _ => len as usize,
    };
    let count = end
        .saturating_sub(from)
        .min((len as usize).saturating_sub(to));
    let tmp: Vec<_> = (0..count)
        .map(|i| {
            vm.heap
                .get_object(h)
                .and_then(|o| o.elements.get((from + i) as u32))
        })
        .collect();
    for (i, item) in tmp.into_iter().enumerate() {
        if let Some(v) = item {
            if let Some(o) = vm.heap.get_object_mut(h) {
                o.elements.set((to + i) as u32, v);
            }
            vm.heap.write_barrier(h, v);
        } else if let Some(o) = vm.heap.get_object_mut(h) {
            o.elements.delete((to + i) as u32);
        }
    }
    Ok(this_val)
}

fn array_to_reversed(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.toReversed called on non-object"));
    };
    Ok(JsValue::from_object(clone_array_elements(vm, h, true)))
}

fn array_to_sorted(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.toSorted called on non-object"));
    };
    let out = clone_array_elements(vm, h, false);
    array_sort(vm, JsValue::from_object(out), args)
}

fn array_to_spliced(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.toSpliced called on non-object"));
    };
    let out = clone_array_elements(vm, h, false);
    let _ = array_splice(vm, JsValue::from_object(out), args)?;
    Ok(JsValue::from_object(out))
}

fn array_with(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Array.prototype.with called on non-object"));
    };
    let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len()) as i64;
    let raw_idx = vm.to_number(arg0(args))?;
    let idx = if raw_idx.is_nan() {
        0i64
    } else {
        raw_idx.trunc() as i64
    };
    let actual = if idx < 0 { len + idx } else { idx };
    if actual < 0 || actual >= len {
        return Err(vm.range_error("Invalid index in Array.prototype.with"));
    }
    let out = clone_array_elements(vm, h, false);
    let new_val = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
    if let Some(o) = vm.heap.get_object_mut(out) {
        o.elements.set(actual as u32, new_val);
    }
    vm.heap.write_barrier(out, new_val);
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
        if let Some(v) = desc_v {
            let new_len = vm.to_u32(v)?;
            if let Some(o) = vm.heap.get_object_mut(obj) {
                o.elements.set_len(new_len);
            }
        }
        return Ok(true);
    }

    if let Ok(idx) = prop_name.parse::<u32>() {
        if is_arr
            && idx < u32::MAX
            && !is_acc_desc
            && desc_w != Some(false)
            && desc_c != Some(false)
            && desc_e != Some(false)
        {
            let exists = vm.heap.get_object(obj).is_some_and(|o| o.elements.has(idx));
            if !exists && !vm.heap.get_object(obj).is_some_and(|o| o.extensible) {
                return Ok(false);
            }
            let val = desc_v.unwrap_or_else(|| {
                vm.heap
                    .get_object(obj)
                    .and_then(|o| o.elements.get(idx))
                    .unwrap_or(JsValue::UNDEFINED)
            });
            if let Some(o) = vm.heap.get_object_mut(obj) {
                o.elements.set(idx, val);
            }
            vm.heap.write_barrier(obj, val);
            return Ok(true);
        }
    }

    let key = vm.heap.intern_key(prop_name);
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
    let prop_name = vm.value_to_rust_string(args.get(1).copied().unwrap_or(JsValue::UNDEFINED));
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
    let prop_name = vm.value_to_rust_string(args.get(1).copied().unwrap_or(JsValue::UNDEFINED));
    let Some(desc) = args.get(2).and_then(|v| v.as_object()) else {
        return Err(vm.type_error("Property description must be an object"));
    };
    Ok(JsValue::from_bool(define_property_checked(
        vm, obj, &prop_name, desc,
    )?))
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
    if props_arg.is_null() || props_arg.is_undefined() {
        return Err(vm.type_error("Cannot convert undefined or null to object"));
    }
    if let Some(props_h) = props_arg.as_object() {
        let keys = vm.own_enumerable_keys_including_elements(props_h);
        let mut pairs = Vec::with_capacity(keys.len());
        for name in keys {
            let k = vm.heap.intern_key(&name);
            let Some(desc_h) = vm.get_property_by_key(props_h, k)?.as_object() else {
                return Err(vm.type_error("Property description must be an object"));
            };
            pairs.push((name, desc_h));
        }
        for (name, desc_h) in pairs {
            if !define_property_checked(vm, obj, &name, desc_h)? {
                return Err(vm.type_error(format!("Cannot redefine property: {name}")));
            }
        }
    }
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
    let prop_name = vm.value_to_rust_string(args.get(1).copied().unwrap_or(JsValue::UNDEFINED));
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
    if let Some(nh) = names_arr.as_object() {
        let len = vm.heap.get_object(nh).map_or(0, |o| o.elements.len());
        for i in 0..len {
            if let Some(kv) = vm.heap.get_object(nh).and_then(|o| o.elements.get(i)) {
                let desc = object_get_own_descriptor(vm, JsValue::UNDEFINED, &[target, kv])?;
                if !desc.is_undefined() {
                    let s = vm.value_to_rust_string(kv);
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

fn object_seal(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let v = arg0(args);
    if let Some(h) = v.as_object() {
        if let Some(o) = vm.heap.get_object_mut(h) {
            o.extensible = false;
            o.to_dictionary_mode(&vm.shapes);
            if let Some(rare) = o.rare.as_mut() {
                for p in rare.dict_props.values_mut() {
                    p.attrs.remove(PropertyAttrs::CONFIGURABLE);
                }
            }
        }
    }
    Ok(v)
}

fn object_is_sealed(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = arg0(args).as_object() else {
        return Ok(JsValue::TRUE);
    };
    let Some(o) = vm.heap.get_object(h) else {
        return Ok(JsValue::TRUE);
    };
    if o.extensible {
        return Ok(JsValue::FALSE);
    }
    if o.is_dictionary() {
        if let Some(rare) = &o.rare {
            for p in rare.dict_props.values() {
                if p.attrs.contains(PropertyAttrs::CONFIGURABLE) {
                    return Ok(JsValue::FALSE);
                }
            }
        }
    } else if let Some(shape) = vm.shapes.get(o.shape()) {
        for &k in shape.keys() {
            if let Some(slot) = shape.lookup(k) {
                if slot.attrs.contains(PropertyAttrs::CONFIGURABLE) {
                    return Ok(JsValue::FALSE);
                }
            }
        }
    }
    Ok(JsValue::TRUE)
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
        let key_str = vm.value_to_rust_string(key_val);
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
    let arr = vm.alloc_array();
    if let Some(h) = arg0(args).as_object() {
        let mut names = Vec::new();
        if let Some(o) = vm.heap.get_object(h) {
            for idx in o.elements.present_indices() {
                names.push(idx.to_string());
            }
            if o.is_dictionary() {
                if let Some(rare) = &o.rare {
                    for &k in &rare.dict_order {
                        if let Some(s) = vm.heap.interner().get(k) {
                            names.push(s.to_string());
                        }
                    }
                }
            } else if let Some(shape) = vm.shapes.get(o.shape()) {
                for &k in shape.keys() {
                    if let Some(s) = vm.heap.interner().get(k) {
                        names.push(s.to_string());
                    }
                }
            }
        }
        for n in names {
            let v = JsValue::from_string(vm.heap.alloc_string(n));
            vm.heap.object_push_element(arr, v);
        }
    }
    Ok(JsValue::from_object(arr))
}

fn object_from_entries(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let out = vm.alloc_ordinary_object();
    if let Some(h) = arg0(args).as_object() {
        let len = vm.heap.get_object(h).map_or(0, |o| o.elements.len());
        let indices: Vec<u32> = if len > 65_536 {
            vm.heap
                .get_object(h)
                .map(|o| o.elements.present_indices())
                .unwrap_or_default()
        } else {
            (0..len).collect()
        };
        for i in indices {
            if let Some(pair) = vm.heap.get_object(h).and_then(|o| o.elements.get(i)) {
                let k = vm.get_elem(pair, JsValue::from_i32(0))?;
                let v = vm.get_elem(pair, JsValue::from_i32(1))?;
                let ks = vm.value_to_rust_string(k);
                let pk = vm.heap.intern_key(&ks);
                vm.heap
                    .object_define_data(out, pk, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
            }
        }
    }
    Ok(JsValue::from_object(out))
}

fn object_is_frozen(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let frozen = match arg0(args).as_object() {
        Some(h) => vm.heap.get_object(h).is_some_and(|o| !o.extensible),
        None => true,
    };
    Ok(JsValue::from_bool(frozen))
}
