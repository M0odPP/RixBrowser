//! Shared WHATWG Streams Standard primitives: `QueueWithSizes`, IEEE-754 chunk size
//! arithmetic, `QueuingStrategy` extraction/validation, WebIDL `GetMethod`, GC anchors,
//! and spec-internal Promise combinators.

use std::collections::VecDeque;

use rix_js::heap::PrivateEntry;
use rix_js::promise::{alloc_promise, promise_resolve_value, promise_then, reject_promise};
use rix_js::{HeapHandle, JsException, JsValue, JsVm, ObjectKind, PromiseState, PropertyAttrs};

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct QueueEntry {
    pub(crate) value: JsValue,
    pub(crate) size: f64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub(crate) struct QueueWithSizes {
    pub(crate) entries: VecDeque<QueueEntry>,
    pub(crate) total_size: f64,
}

#[allow(dead_code)]
impl QueueWithSizes {
    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            total_size: 0.0,
        }
    }

    pub(crate) fn enqueue_value_with_size(
        &mut self,
        vm: &mut JsVm,
        value: JsValue,
        size: f64,
    ) -> Result<(), JsException> {
        if !size.is_finite() || size < 0.0 {
            return Err(vm.range_error("Chunk size must be a finite non-negative number"));
        }
        self.entries.push_back(QueueEntry { value, size });
        self.total_size += size;
        Ok(())
    }

    pub(crate) fn dequeue_value(&mut self) -> Option<JsValue> {
        let entry = self.entries.pop_front()?;
        self.total_size -= entry.size;
        if self.total_size < 0.0 {
            self.total_size = 0.0;
        }
        Some(entry.value)
    }

    pub(crate) fn reset(&mut self) {
        self.entries.clear();
        self.total_size = 0.0;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub(crate) fn promise_reject_value(vm: &mut JsVm, reason: JsValue) -> HeapHandle {
    let p = alloc_promise(vm);
    reject_promise(vm, p, reason);
    p
}

pub(crate) fn promise_mark_handled(vm: &mut JsVm, p: HeapHandle) {
    if let Some(ObjectKind::Promise(pd)) = vm.heap.get_object_mut(p).map(|o| &mut o.kind) {
        pd.handled = true;
    }
}

pub(crate) fn promise_is_pending(vm: &JsVm, p: HeapHandle) -> bool {
    matches!(
        vm.heap.get_object(p).map(|o| &o.kind),
        Some(ObjectKind::Promise(pd)) if matches!(pd.state, PromiseState::Pending)
    )
}

#[allow(dead_code)]
pub(crate) fn promise_map_to_undefined(vm: &mut JsVm, p: HeapHandle) -> HeapHandle {
    let on_ok = vm.alloc_host_fn("stream_map_undef", |_, _, _| Ok(JsValue::UNDEFINED));
    promise_then(vm, p, Some(on_ok), None)
}

pub(crate) fn call_returning_promise(
    vm: &mut JsVm,
    fn_h: HeapHandle,
    this_val: JsValue,
    args: &[JsValue],
) -> HeapHandle {
    match vm.call_value(JsValue::from_object(fn_h), this_val, args) {
        Ok(v) => promise_resolve_value(vm, v),
        Err(e) => promise_reject_value(vm, e.value),
    }
}

pub(crate) fn attach_gc_anchor(vm: &mut JsVm, holder: HeapHandle, target: HeapHandle) {
    let k = vm.heap.intern_key("__rix_stream_anchor__");
    let v = JsValue::from_object(target);
    if let Some(o) = vm.heap.get_object_mut(holder) {
        o.rare_mut()
            .private_fields
            .insert(k, PrivateEntry::Field(v));
    }
    vm.heap.write_barrier(holder, v);
}

pub(crate) fn set_to_string_tag(vm: &mut JsVm, proto: HeapHandle, tag: &str) {
    let k = vm.key_symbol_to_string_tag;
    let s = JsValue::from_string(vm.heap.alloc_string(tag));
    vm.heap
        .object_define_data(proto, k, s, PropertyAttrs::CONFIGURABLE, &mut vm.shapes);
}

pub(crate) fn get_method_checked(
    vm: &mut JsVm,
    obj: HeapHandle,
    name: &str,
) -> Result<Option<HeapHandle>, JsException> {
    let k = vm.heap.intern_key(name);
    let v = vm.get_property_by_key(obj, k)?;
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    if let Some(fh) = v.as_object() {
        if vm.is_callable_handle(fh) {
            return Ok(Some(fh));
        }
    }
    Err(vm.type_error(format!("'{name}' is not callable")))
}

pub(crate) fn validate_and_normalize_hwm(vm: &mut JsVm, val: JsValue) -> Result<f64, JsException> {
    let n = vm.to_number(val)?;
    if n.is_nan() || n < 0.0 {
        return Err(vm.range_error("highWaterMark must be a non-negative number"));
    }
    Ok(n)
}

pub(crate) fn extract_strategy(
    vm: &mut JsVm,
    strategy_val: JsValue,
) -> Result<(Option<JsValue>, Option<HeapHandle>), JsException> {
    if strategy_val.is_undefined() || strategy_val.is_null() {
        return Ok((None, None));
    }
    let Some(sh) = strategy_val.as_object() else {
        return Err(vm.type_error("QueuingStrategy must be an object"));
    };
    let k_hwm = vm.heap.intern_key("highWaterMark");
    let hwm_v = vm.get_property_by_key(sh, k_hwm)?;
    let hwm_opt = if hwm_v.is_undefined() {
        None
    } else {
        Some(hwm_v)
    };
    let k_size = vm.heap.intern_key("size");
    let size_v = vm.get_property_by_key(sh, k_size)?;
    let size_fn = if size_v.is_undefined() {
        None
    } else if let Some(fh) = size_v.as_object() {
        if vm.is_callable_handle(fh) {
            Some(fh)
        } else {
            return Err(vm.type_error("QueuingStrategy.size must be a function"));
        }
    } else {
        return Err(vm.type_error("QueuingStrategy.size must be a function"));
    };
    Ok((hwm_opt, size_fn))
}

pub(crate) fn call_size_algorithm(
    vm: &mut JsVm,
    size_fn: Option<HeapHandle>,
    chunk: JsValue,
) -> Result<f64, JsException> {
    let Some(sf) = size_fn else {
        return Ok(1.0);
    };
    let raw = vm.call_value(JsValue::from_object(sf), JsValue::UNDEFINED, &[chunk])?;
    let n = vm.to_number(raw)?;
    if !n.is_finite() || n < 0.0 {
        return Err(vm.range_error("Chunk size must be a finite non-negative number"));
    }
    Ok(n)
}
