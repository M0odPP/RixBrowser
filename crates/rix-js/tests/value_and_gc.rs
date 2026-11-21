use rix_js::{
    GcHeap, Generation, HeapHandle, JsBigInt, JsValue, JsValueKind, MajorGcPhase, RootStack,
    SymbolId, WeakTable, CANONICAL_NAN_BITS,
};

#[test]
fn nan_boxed_js_value_ieee754_exact_and_int32_boundaries() {
    assert_eq!(std::mem::size_of::<JsValue>(), 8);

    // Canonical quiet NaN: negative quiet/signaling NaN bit patterns MUST canonicalize
    // to positive quiet NaN (0x7FF8_0000_0000_0000) so they never collide with tag space >= 0xFFF8.
    let neg_qnan = f64::from_bits(0xFFFF_FFFF_FFFF_FFFF);
    let pos_snan = f64::from_bits(0x7FF0_0000_0000_0001);
    let v_neg_nan = JsValue::from_f64(neg_qnan);
    let v_pos_nan = JsValue::from_f64(pos_snan);
    assert_eq!(v_neg_nan.to_raw(), CANONICAL_NAN_BITS);
    assert_eq!(v_pos_nan.to_raw(), CANONICAL_NAN_BITS);
    assert!(v_neg_nan.is_f64());
    assert!(v_neg_nan.as_f64().unwrap().is_nan());
    assert!(!v_neg_nan.strict_eq_fast(v_pos_nan)); // NaN !== NaN
    assert!(v_neg_nan.same_value_fast(v_pos_nan)); // Object.is(NaN, NaN) === true

    // Negative zero (-0.0) preserved distinct from +0.0 under SameValue, equal under ===.
    let pos_zero = JsValue::from_number(0.0);
    let neg_zero = JsValue::from_number(-0.0);
    assert!(pos_zero.is_i32());
    assert!(neg_zero.is_f64(), "-0.0 must stay IEEE-754 f64");
    assert!(neg_zero.as_f64().unwrap().is_sign_negative());
    assert!(pos_zero.strict_eq_fast(neg_zero)); // +0 === -0
    assert!(!pos_zero.same_value_fast(neg_zero)); // !Object.is(+0, -0)

    // Infinities and normal floats.
    let pos_inf = JsValue::from_f64(f64::INFINITY);
    let neg_inf = JsValue::from_f64(f64::NEG_INFINITY);
    assert_eq!(pos_inf.as_f64(), Some(f64::INFINITY));
    assert_eq!(neg_inf.as_f64(), Some(f64::NEG_INFINITY));
    assert_eq!(JsValue::from_number(3.25).as_f64(), Some(3.25));

    // Int32 boundaries and overflow transitions to f64.
    let max_i = JsValue::from_i32(i32::MAX);
    let min_i = JsValue::from_i32(i32::MIN);
    assert_eq!(max_i.as_i32(), Some(i32::MAX));
    assert_eq!(min_i.as_i32(), Some(i32::MIN));
    let overflow_add = JsValue::add_i32_overflow(i32::MAX, 1);
    assert!(overflow_add.is_f64());
    assert_eq!(overflow_add.as_f64(), Some(2_147_483_648.0));
    let overflow_sub = JsValue::sub_i32_overflow(i32::MIN, 1);
    assert!(overflow_sub.is_f64());
    assert_eq!(overflow_sub.as_f64(), Some(-2_147_483_649.0));
    let neg_zero_mul = JsValue::mul_i32_overflow(-5, 0);
    assert!(neg_zero_mul.is_f64() && neg_zero_mul.as_f64().unwrap().is_sign_negative());

    // Specials, Symbols, and 48-bit generational HeapHandles.
    assert!(JsValue::undefined().is_undefined() && JsValue::undefined().is_nullish());
    assert!(JsValue::null().is_null() && JsValue::null().is_nullish());
    assert_eq!(JsValue::from_bool(true).as_bool(), Some(true));
    assert_eq!(JsValue::from_bool(false).as_bool(), Some(false));
    assert_eq!(
        JsValue::from_symbol(SymbolId(77)).as_symbol(),
        Some(SymbolId(77))
    );

    let handle = HeapHandle::new(0x1234_5678, 0xABCD);
    let obj_val = JsValue::from_object(handle);
    assert!(obj_val.is_object() && obj_val.is_heap_ref());
    assert_eq!(obj_val.as_object_handle(), Some(handle));
    assert_eq!(obj_val.kind(), JsValueKind::Object(handle));
}

#[test]
fn bigint_and_rope_strings_trace_and_compute_cleanly() {
    let a = JsBigInt::parse_str("90071992547409931234567890").unwrap();
    let b = JsBigInt::parse_str("10000000000000000000000000").unwrap();
    assert_eq!(a.add(&b).to_decimal_string(), "100071992547409931234567890");
    assert_eq!(a.sub(&b).to_decimal_string(), "80071992547409931234567890");
    let prod = JsBigInt::from_i64(-1_000_000_000).mul(&JsBigInt::from_i64(2_000_000_000));
    assert_eq!(prod.to_decimal_string(), "-2000000000000000000");

    let mut heap = GcHeap::new();
    let s1 = heap.alloc_string("Rix");
    let s2 = heap.alloc_string("Browser");
    let rope = heap.alloc_rope(s1, s2).unwrap();
    let roots = [JsValue::from_string(rope)];
    heap.collect_minor(&roots);
    assert!(heap.is_alive(rope));
    assert!(heap.is_alive(s1), "rope child left kept alive by minor GC");
    assert!(heap.is_alive(s2), "rope child right kept alive by minor GC");
    assert_eq!(heap.materialize_string(rope).as_deref(), Some("RixBrowser"));
}

#[test]
fn generational_gc_write_barrier_cyclic_collection_and_incremental_marking() {
    let mut heap = GcHeap::new();
    let mut roots = RootStack::new();

    // Promote `old_parent` to Old generation across 2 minor cycles.
    let old_parent = heap.alloc_empty_object();
    roots.push(JsValue::from_object(old_parent));
    heap.collect_minor(roots.as_slice());
    heap.collect_minor(roots.as_slice());
    assert_eq!(heap.generation_of(old_parent), Some(Generation::Old));

    // Attach a brand-new Young child to `old_parent`: write barrier must keep it alive in minor GC!
    let young_child = heap.alloc_empty_object();
    assert_eq!(heap.generation_of(young_child), Some(Generation::Young));
    heap.object_set_slot(old_parent, 0, JsValue::from_object(young_child));
    heap.collect_minor(roots.as_slice());
    assert!(
        heap.is_alive(young_child),
        "Old->Young write barrier preserves young child across minor GC"
    );

    // Build an unreachable cyclic Old+Young graph (`cyc_a <-> cyc_b`) and verify Major GC reclaims it.
    let cyc_a = heap.alloc_empty_object();
    let cyc_b = heap.alloc_empty_object();
    heap.object_set_slot(cyc_a, 0, JsValue::from_object(cyc_b));
    heap.object_set_slot(cyc_b, 0, JsValue::from_object(cyc_a));

    // Exercise incremental tri-color marking steps + write barrier during Marking phase.
    heap.start_major_marking(roots.as_slice());
    assert_eq!(heap.major_phase(), MajorGcPhase::Marking);
    let late_young = heap.alloc_empty_object();
    heap.object_set_slot(old_parent, 1, JsValue::from_object(late_young));
    while !heap.step_major_marking(2) {}
    let major_stats = heap.finish_major_sweep(roots.as_slice());
    assert!(major_stats.is_major);
    assert!(major_stats.reclaimed >= 2);
    assert!(!heap.is_alive(cyc_a), "cyclic node A reclaimed");
    assert!(!heap.is_alive(cyc_b), "cyclic node B reclaimed");
    assert!(heap.is_alive(old_parent));
    assert!(heap.is_alive(young_child));
    assert!(heap.is_alive(late_young));
}

#[test]
fn weak_dom_wrapper_table_reclaims_detached_nodes_and_preserves_pinned_listeners() {
    let mut heap = GcHeap::new();
    let mut dom_wrappers: WeakTable<u64> = WeakTable::new();

    // Create 50,000 detached wrappers with no listeners/expandos + 1 wrapper pinned by an event listener.
    let pinned_wrapper = heap.alloc_empty_object();
    dom_wrappers.insert(999_999, pinned_wrapper, true);

    let mut roots = Vec::new();
    for node_id in 0..50_000u64 {
        let w = heap.alloc_empty_object();
        dom_wrappers.insert(node_id, w, false);
        if heap.should_collect_minor() {
            roots.clear();
            dom_wrappers.collect_pinned_roots(&mut roots);
            heap.collect_minor(&roots);
            dom_wrappers.sweep_dead(&heap);
        }
    }
    roots.clear();
    dom_wrappers.collect_pinned_roots(&mut roots);
    heap.collect_major(&roots);
    dom_wrappers.sweep_dead(&heap);

    assert!(dom_wrappers.get_live(&999_999, &heap).is_some());
    assert_eq!(
        dom_wrappers.len(),
        1,
        "all 50k unpinned detached wrappers reclaimed"
    );

    // Once listener is removed (unpinned), major GC reclaims the final wrapper too.
    dom_wrappers.set_pinned(&999_999, false);
    heap.collect_major(&[]);
    dom_wrappers.sweep_dead(&heap);
    assert!(dom_wrappers.is_empty());
}

#[test]
fn stress_1_million_temporary_objects_stabilizes_heap_with_sub_ms_minor_pauses() {
    let mut heap = GcHeap::new();
    let anchor = heap.alloc_empty_object();
    let roots = [JsValue::from_object(anchor)];

    let mut minor_pause_total_us = 0u64;
    let mut minor_count = 0u64;
    let mut peak_live = 0usize;

    for _ in 0..1_000_000usize {
        let _tmp = heap.alloc_empty_object();
        if heap.should_collect_minor() {
            let stats = heap.collect_minor(&roots);
            minor_pause_total_us += stats.pause_us;
            minor_count += 1;
            peak_live = peak_live.max(stats.live_after);
        }
    }
    let final_stats = heap.collect_minor(&roots);
    assert!(minor_count >= 100);
    assert_eq!(final_stats.live_after, 1, "only root anchor remains alive");
    assert!(
        peak_live <= 2,
        "heap stabilized flat across 1M temporary allocations, peak_live={peak_live}"
    );
    let mean_pause_us = minor_pause_total_us as f64 / minor_count as f64;
    assert!(
        mean_pause_us < 1_000.0,
        "mean minor GC pause exceeded 1 ms: {mean_pause_us:.1} us"
    );
}
