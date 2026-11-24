use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct DirtyFlags: u8 {
        const SELF_STYLE_DIRTY        = 1 << 0;
        const DESCENDANT_STYLE_DIRTY  = 1 << 1;
        const SUBTREE_STYLE_DIRTY     = 1 << 2;
        const SELF_LAYOUT_DIRTY       = 1 << 3;
        const DESCENDANT_LAYOUT_DIRTY = 1 << 4;
        const PAINT_DIRTY             = 1 << 5;
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ElementState: u32 {
        const HOVER         = 1 << 0;
        const ACTIVE        = 1 << 1;
        const FOCUS         = 1 << 2;
        const FOCUS_VISIBLE = 1 << 3;
        const FOCUS_WITHIN  = 1 << 4;
        const CHECKED       = 1 << 5;
        const DISABLED      = 1 << 6;
        const ENABLED       = 1 << 7;
        const VISITED       = 1 << 8;
        const LINK          = 1 << 9;
        const TARGET        = 1 << 10;
        const VALID         = 1 << 11;
        const INVALID       = 1 << 12;
        const INDETERMINATE = 1 << 13;
        const DEFINED       = 1 << 14;
        const MODAL         = 1 << 15;
        const POPOVER_OPEN  = 1 << 16;
        const FULLSCREEN    = 1 << 17;
        const OPEN          = 1 << 18;
        const USER_VALID    = 1 << 19;
        const USER_INVALID  = 1 << 20;
        const AUTOFILL      = 1 << 21;
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

#[inline]
pub fn hash_ident(s: &str) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in s.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[inline]
pub fn hash_ident_ascii_lower(s: &str) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in s.as_bytes() {
        hash ^= u64::from(b.to_ascii_lowercase());
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Folds a 64-bit hash into a non-zero 32-bit hash compatible with `selectors::PrecomputedHash`.
#[inline]
pub fn fold_hash_u32(hash: u64) -> u32 {
    let folded = ((hash >> 32) as u32) ^ (hash as u32);
    if folded == 0 {
        1
    } else {
        folded
    }
}
