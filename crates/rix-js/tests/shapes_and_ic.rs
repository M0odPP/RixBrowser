use rix_js::{
    ElementsKind, ElementsStorage, GcHeap, IcEntry, IcState, IcStats, IcTable, JsValue,
    MegamorphicCache, PropertyAttrs, ShapeId, ShapeTable, DICTIONARY_TRANSITION_LIMIT,
};

#[test]
fn shape_transition_tree_shares_layouts_and_falls_back_to_dictionary_after_64_transitions() {
    let mut heap = GcHeap::new();
    let mut shapes = ShapeTable::new();

    let kx = heap.intern_key("x");
    let ky = heap.intern_key("y");
    let kz = heap.intern_key("z");

    let p1 = heap.alloc_empty_object();
    let p2 = heap.alloc_empty_object();

    let (_, s1, off_x1) = heap
        .object_define_data(
            p1,
            kx,
            JsValue::from_i32(10),
            PropertyAttrs::DEFAULT,
            &mut shapes,
        )
        .unwrap();
    let (_, s2, off_y1) = heap
        .object_define_data(
            p1,
            ky,
            JsValue::from_i32(20),
            PropertyAttrs::DEFAULT,
            &mut shapes,
        )
        .unwrap();
    assert_eq!(off_x1, 0);
    assert_eq!(off_y1, 1);

    // Second object following same transition order shares exact ShapeIds without allocating new shapes!
    let count_before = shapes.shape_count();
    let (_, s1_b, off_x2) = heap
        .object_define_data(
            p2,
            kx,
            JsValue::from_i32(30),
            PropertyAttrs::DEFAULT,
            &mut shapes,
        )
        .unwrap();
    let (_, s2_b, off_y2) = heap
        .object_define_data(
            p2,
            ky,
            JsValue::from_i32(40),
            PropertyAttrs::DEFAULT,
            &mut shapes,
        )
        .unwrap();
    assert_eq!(s1, s1_b);
    assert_eq!(s2, s2_b);
    assert_eq!(off_x2, 0);
    assert_eq!(off_y2, 1);
    assert_eq!(shapes.shape_count(), count_before);

    // Tail deletion rolls back to parent shape cleanly without switching to dictionary mode.
    assert!(heap.object_delete_own(p2, ky, &mut shapes));
    assert_eq!(heap.get_object(p2).unwrap().shape(), s1);
    assert!(!heap.get_object(p2).unwrap().is_dictionary());

    // Non-tail deletion switches cleanly into dictionary mode preserving remaining properties.
    heap.object_define_data(
        p1,
        kz,
        JsValue::from_i32(30),
        PropertyAttrs::DEFAULT,
        &mut shapes,
    );
    assert!(heap.object_delete_own(p1, kx, &mut shapes));
    assert!(heap.get_object(p1).unwrap().is_dictionary());
    assert!(heap.object_get_own(p1, kx, &shapes).is_none());
    assert_eq!(
        heap.object_get_own(p1, ky, &shapes).unwrap().value,
        JsValue::from_i32(20)
    );
    assert_eq!(
        heap.object_get_own(p1, kz, &shapes).unwrap().value,
        JsValue::from_i32(30)
    );

    // Exceeding DICTIONARY_TRANSITION_LIMIT (64) dynamic properties automatically switches to dictionary mode.
    let dynamic = heap.alloc_empty_object();
    for i in 0..=DICTIONARY_TRANSITION_LIMIT {
        let key = heap.intern_key(&format!("k_{i}"));
        heap.object_define_data(
            dynamic,
            key,
            JsValue::from_i32(i32::from(i)),
            PropertyAttrs::DEFAULT,
            &mut shapes,
        );
    }
    assert!(
        heap.get_object(dynamic).unwrap().is_dictionary(),
        "object must switch to dictionary mode after > {DICTIONARY_TRANSITION_LIMIT} transitions"
    );
    let k0 = heap.intern_key("k_0");
    let k64 = heap.intern_key("k_64");
    assert_eq!(
        heap.object_get_own(dynamic, k0, &shapes).unwrap().value,
        JsValue::from_i32(0)
    );
    assert_eq!(
        heap.object_get_own(dynamic, k64, &shapes).unwrap().value,
        JsValue::from_i32(64)
    );
}

#[test]
fn elements_storage_widens_from_int32_to_float64_values_and_sparse_dict() {
    let mut elems = ElementsStorage::new();
    assert_eq!(elems.kind(), ElementsKind::PackedInt32);

    elems.push(JsValue::from_i32(1));
    elems.push(JsValue::from_i32(2));
    assert_eq!(elems.kind(), ElementsKind::PackedInt32);
    assert_eq!(elems.get(0), Some(JsValue::from_i32(1)));
    assert_eq!(elems.len(), 2);

    // Float widens PackedInt32 -> PackedFloat64.
    elems.push(JsValue::from_f64(3.5));
    assert_eq!(elems.kind(), ElementsKind::PackedFloat64);
    assert_eq!(elems.get(0), Some(JsValue::from_f64(1.0)));
    assert_eq!(elems.get(2), Some(JsValue::from_f64(3.5)));

    // Boolean / Object / hole widens PackedFloat64 -> PackedValues.
    elems.push(JsValue::TRUE);
    assert_eq!(elems.kind(), ElementsKind::PackedValues);
    assert_eq!(elems.get(3), Some(JsValue::TRUE));
    assert!(elems.delete(1));
    assert_eq!(elems.get(1), None);
    assert_eq!(elems.len(), 4);

    // Huge sparse index widens into SparseDict without allocating millions of elements.
    elems.set(1_000_000, JsValue::from_i32(99));
    assert_eq!(elems.kind(), ElementsKind::SparseDict);
    assert_eq!(elems.len(), 1_000_001);
    assert_eq!(elems.get(1_000_000), Some(JsValue::from_i32(99)));
    assert_eq!(elems.get(500_000), None);
    assert_eq!(elems.present_indices(), vec![0, 2, 3, 1_000_000]);

    // Truncating sparse array cleans out-of-range keys.
    elems.set_len(3);
    assert_eq!(elems.len(), 3);
    assert_eq!(elems.get(1_000_000), None);
    assert_eq!(elems.present_indices(), vec![0, 2]);
}

#[test]
fn inline_cache_transitions_mono_poly_mega_and_invalidates_prototype_epoch() {
    let mut heap = GcHeap::new();
    let mut shapes = ShapeTable::new();
    let mut mega = MegamorphicCache::new();
    let mut stats = IcStats::default();

    let key_val = heap.intern_key("val");
    let proto = heap.alloc_empty_object();
    let (_, _, proto_off) = heap
        .object_define_data(
            proto,
            key_val,
            JsValue::from_i32(100),
            PropertyAttrs::DEFAULT,
            &mut shapes,
        )
        .unwrap();

    // Create 5 distinct shapes sharing `proto` so we exercise Mono -> Poly(2..=4) -> Mega.
    let mut objs = Vec::new();
    for i in 0..5 {
        let o = heap.alloc_empty_object();
        heap.object_set_prototype(o, Some(proto), &mut shapes);
        let tag_key = heap.intern_key(&format!("tag_{i}"));
        heap.object_define_data(
            o,
            tag_key,
            JsValue::from_i32(i),
            PropertyAttrs::DEFAULT,
            &mut shapes,
        );
        objs.push(o);
    }

    let mut ic = IcTable::with_len(2);
    let shape0 = heap.get_object(objs[0]).unwrap().shape();

    // 1. Monomorphic loop (1,000 hits after 1 miss -> 99.9% hit rate!).
    let mut mono_stats = IcStats::default();
    for _ in 0..1_000 {
        let epoch = shapes.prototype_epoch();
        if ic
            .probe_load(0, shape0, key_val, epoch, &mega, &mut mono_stats)
            .is_none()
        {
            let entry = IcEntry::proto(shape0, proto, proto_off, epoch);
            ic.update(0, key_val, entry, false, &mut mega);
        }
    }
    assert!(matches!(ic.state(0), IcState::Monomorphic(_)));
    assert_eq!(mono_stats.misses, 1);
    assert_eq!(mono_stats.monomorphic_hits, 999);
    assert!(mono_stats.hit_rate() > 0.99);

    // 2. Prototype mutation bumps prototype_epoch -> instant O(1) cache miss!
    heap.object_define_data(
        proto,
        key_val,
        JsValue::from_i32(200),
        PropertyAttrs::DEFAULT,
        &mut shapes,
    );
    assert!(
        ic.probe_load(
            0,
            shape0,
            key_val,
            shapes.prototype_epoch(),
            &mega,
            &mut stats
        )
        .is_none(),
        "mutating prototype must invalidate prototype-chain IC immediately"
    );

    // 3. Feed 5 distinct shapes into site 1 -> Mono -> Poly(2,3,4) -> Megamorphic.
    for (idx, &obj) in objs.iter().enumerate() {
        let s = heap.get_object(obj).unwrap().shape();
        let epoch = shapes.prototype_epoch();
        assert!(ic
            .probe_load(1, s, key_val, epoch, &mega, &mut stats)
            .is_none());
        ic.update(
            1,
            key_val,
            IcEntry::proto(s, proto, proto_off, epoch),
            false,
            &mut mega,
        );
        match idx {
            0 => assert!(matches!(ic.state(1), IcState::Monomorphic(_))),
            1 => assert!(matches!(ic.state(1), IcState::Polymorphic { len: 2, .. })),
            2 => assert!(matches!(ic.state(1), IcState::Polymorphic { len: 3, .. })),
            3 => assert!(matches!(ic.state(1), IcState::Polymorphic { len: 4, .. })),
            4 => assert_eq!(ic.state(1), IcState::Megamorphic),
            _ => unreachable!(),
        }
    }

    // All 5 shapes now hit via the shared MegamorphicCache!
    let mut mega_hits_before = stats.megamorphic_hits;
    for &obj in &objs {
        let s = heap.get_object(obj).unwrap().shape();
        let hit = ic
            .probe_load(1, s, key_val, shapes.prototype_epoch(), &mega, &mut stats)
            .expect("megamorphic hit");
        assert_eq!(hit.holder, Some(proto));
        assert_eq!(hit.offset, proto_off);
        mega_hits_before += 1;
    }
    assert_eq!(stats.megamorphic_hits, mega_hits_before);
    assert_ne!(shape0, ShapeId::DICTIONARY);
}
