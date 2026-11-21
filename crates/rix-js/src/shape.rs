//! Immutable Shape (Hidden Class) transition tree and dictionary fallback.
//!
//! Reference: Spec 05 §3 (V8 Maps / SpiderMonkey Shapes / JSC Structures).
//! Every ordinary JS object begins at [`ShapeId::ROOT`] and transitions in O(1)
//! on property addition into a shared [`ShapeId`] that maps each interned
//! [`PropertyKey`] to a fixed dense slot index in `JsObject::slots`.
//! Objects exceeding [`DICTIONARY_TRANSITION_LIMIT`] (64) dynamic transitions
//! or undergoing non-tail `delete` switch cleanly to [`ShapeId::DICTIONARY`].

use crate::string::PropertyKey;
use crate::value::{HeapHandle, JsValue};
use bitflags::bitflags;
use std::collections::HashMap;

/// Dynamic transition limit before an object switches to dictionary mode.
pub const DICTIONARY_TRANSITION_LIMIT: u16 = 64;

/// Compact 32-bit identifier for an immutable object Shape (Hidden Class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ShapeId(pub u32);

impl ShapeId {
    /// Empty root shape (`0` properties).
    pub const ROOT: Self = Self(0);
    /// Sentinel shape indicating per-object hash dictionary storage.
    pub const DICTIONARY: Self = Self(u32::MAX);

    #[inline]
    #[must_use]
    pub const fn is_dictionary(self) -> bool {
        self.0 == u32::MAX
    }
}

bitflags! {
    /// ECMAScript property descriptor attributes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PropertyAttrs: u8 {
        const WRITABLE     = 0b0001;
        const ENUMERABLE   = 0b0010;
        const CONFIGURABLE = 0b0100;
        const ACCESSOR     = 0b1000;
        const DEFAULT      = Self::WRITABLE.bits() | Self::ENUMERABLE.bits() | Self::CONFIGURABLE.bits();
        const METHOD       = Self::WRITABLE.bits() | Self::CONFIGURABLE.bits();
        const READONLY     = Self::ENUMERABLE.bits();
    }
}

/// Fixed slot layout entry inside a [`Shape`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeSlot {
    pub offset: u32,
    pub attrs: PropertyAttrs,
}

/// Property storage entry used when an object is in [`ShapeId::DICTIONARY`] mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictProperty {
    pub value: JsValue,
    pub attrs: PropertyAttrs,
    pub getter: Option<HeapHandle>,
    pub setter: Option<HeapHandle>,
}

impl DictProperty {
    #[inline]
    pub const fn data(value: JsValue, attrs: PropertyAttrs) -> Self {
        Self {
            value,
            attrs,
            getter: None,
            setter: None,
        }
    }

    #[inline]
    pub const fn accessor(
        getter: Option<HeapHandle>,
        setter: Option<HeapHandle>,
        attrs: PropertyAttrs,
    ) -> Self {
        Self {
            value: JsValue::UNDEFINED,
            attrs: attrs.union(PropertyAttrs::ACCESSOR),
            getter,
            setter,
        }
    }
}

/// Immutable hidden-class node describing an object's property-to-slot layout.
#[derive(Debug, Clone)]
pub struct Shape {
    pub id: ShapeId,
    pub parent: Option<ShapeId>,
    pub added_key: Option<PropertyKey>,
    pub slot_count: u32,
    props: HashMap<PropertyKey, ShapeSlot>,
    key_order: Vec<PropertyKey>,
}

impl Shape {
    #[inline]
    #[must_use]
    pub fn lookup(&self, key: PropertyKey) -> Option<ShapeSlot> {
        self.props.get(&key).copied()
    }

    #[inline]
    #[must_use]
    pub fn keys(&self) -> &[PropertyKey] {
        &self.key_order
    }

    #[inline]
    #[must_use]
    pub fn property_count(&self) -> usize {
        self.key_order.len()
    }
}

/// Per-realm immutable Shape transition table and prototype-chain validity epoch.
#[derive(Debug)]
pub struct ShapeTable {
    shapes: Vec<Shape>,
    transitions: HashMap<(ShapeId, PropertyKey, PropertyAttrs), ShapeId>,
    prototype_epoch: u64,
}

impl Default for ShapeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ShapeTable {
    #[must_use]
    pub fn new() -> Self {
        let root = Shape {
            id: ShapeId::ROOT,
            parent: None,
            added_key: None,
            slot_count: 0,
            props: HashMap::new(),
            key_order: Vec::new(),
        };
        Self {
            shapes: vec![root],
            transitions: HashMap::new(),
            prototype_epoch: 1,
        }
    }

    #[inline]
    #[must_use]
    pub fn shape_count(&self) -> usize {
        self.shapes.len()
    }

    #[inline]
    #[must_use]
    pub fn prototype_epoch(&self) -> u64 {
        self.prototype_epoch
    }

    /// Invalidates all prototype-chain inline caches in O(1) whenever any
    /// prototype object is mutated or `Object.setPrototypeOf` runs.
    #[inline]
    pub fn bump_prototype_epoch(&mut self) -> u64 {
        self.prototype_epoch = self.prototype_epoch.wrapping_add(1).max(1);
        self.prototype_epoch
    }

    #[inline]
    #[must_use]
    pub fn get(&self, id: ShapeId) -> Option<&Shape> {
        if id.is_dictionary() {
            return None;
        }
        self.shapes.get(id.0 as usize)
    }

    #[inline]
    #[must_use]
    pub fn lookup(&self, shape_id: ShapeId, key: PropertyKey) -> Option<ShapeSlot> {
        self.get(shape_id)?.lookup(key)
    }

    /// Transitions `from` by adding `key` with `attrs`, returning `(new_shape, slot_offset)`.
    ///
    /// If `key` already exists on `from` with matching `attrs`, returns `(from, existing_offset)`.
    /// If an identical transition already exists in the tree, reuses the existing [`ShapeId`] in O(1).
    pub fn transition_add(
        &mut self,
        from: ShapeId,
        key: PropertyKey,
        attrs: PropertyAttrs,
    ) -> (ShapeId, u32) {
        debug_assert!(!from.is_dictionary());
        if let Some(existing) = self.lookup(from, key) {
            if existing.attrs == attrs {
                return (from, existing.offset);
            }
            // Attribute reconfiguration (e.g. converting writable data prop to getter/setter):
            // create/reuse a transition with updated attrs on the same slot offset.
            if let Some(&next_id) = self.transitions.get(&(from, key, attrs)) {
                let offset = self.shapes[next_id.0 as usize]
                    .lookup(key)
                    .map_or(existing.offset, |s| s.offset);
                return (next_id, offset);
            }
            let prev = &self.shapes[from.0 as usize];
            let mut props = prev.props.clone();
            props.insert(
                key,
                ShapeSlot {
                    offset: existing.offset,
                    attrs,
                },
            );
            let key_order = prev.key_order.clone();
            let slot_count = prev.slot_count;
            let new_id = ShapeId(self.shapes.len() as u32);
            self.shapes.push(Shape {
                id: new_id,
                parent: Some(from),
                added_key: Some(key),
                slot_count,
                props,
                key_order,
            });
            self.transitions.insert((from, key, attrs), new_id);
            return (new_id, existing.offset);
        }

        if let Some(&next_id) = self.transitions.get(&(from, key, attrs)) {
            let offset = self.shapes[next_id.0 as usize]
                .lookup(key)
                .map_or(0, |s| s.offset);
            return (next_id, offset);
        }

        let prev = &self.shapes[from.0 as usize];
        let offset = prev.slot_count;
        let mut props = prev.props.clone();
        props.insert(key, ShapeSlot { offset, attrs });
        let mut key_order = prev.key_order.clone();
        key_order.push(key);
        let new_id = ShapeId(self.shapes.len() as u32);
        self.shapes.push(Shape {
            id: new_id,
            parent: Some(from),
            added_key: Some(key),
            slot_count: offset + 1,
            props,
            key_order,
        });
        self.transitions.insert((from, key, attrs), new_id);
        (new_id, offset)
    }

    /// Checks if deleting `key` can roll back cleanly to `parent` (tail deletion)
    /// without forcing dictionary mode.
    #[must_use]
    pub fn try_tail_delete(&self, from: ShapeId, key: PropertyKey) -> Option<(ShapeId, u32)> {
        let shape = self.get(from)?;
        if shape.added_key == Some(key) {
            let parent = shape.parent?;
            let slot = shape.lookup(key)?;
            // Only safe as tail rollback if parent did not already define `key`.
            if self.lookup(parent, key).is_none() {
                return Some((parent, slot.offset));
            }
        }
        None
    }
}
