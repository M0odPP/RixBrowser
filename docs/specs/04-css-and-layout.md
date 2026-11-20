# Spec 04: Typed CSS Style Engine & Layout Engine (`rix-css`, `rix-layout`)

**Status:** Draft (Phase 1 Target)  
**Governing Architecture:** [docs/ARCHITECTURE.md](../ARCHITECTURE.md)  
**Unsafe Policy:** `#![deny(unsafe_code)]` across `rix-css`, `rix-layout`, and `rix-text`.

---

## 1. Objectives & Anti-Patterns Eliminated

1. **Zero String Round-Tripping:** Parse CSS via `lightningcss` directly into **typed `PropertyDeclaration` enums** at stylesheet parse time. Never call `prop.to_css_string(...)` just to re-parse the string during cascade.
2. **Compact, Shared `ComputedStyle`:** Replace 15+ heap `String` fields and per-element `HashMap<String, String>` clones with compact enums, bitflags, copyable structs, and copy-on-write/inherited `Arc` sub-structs (`InheritedTextStyle`, `BoxStyle`, `BorderStyle`, `FlexGridStyle`, `EffectsStyle`). Comparing layout equality (`layout_eq`) must perform direct field comparisons with **zero allocations and zero clones**.
3. **Stylo-Inspired Cascade:** Fast `RuleIndex` buckets (ID, class, tag, universal) + ancestor Bloom filter (`u64` / `[u64; 2]` per node) + `InvalidationMap` + per-document `StyleCascadeMemo` (never a single global `Mutex<Option<...>>` thrashed across tabs) + parallel top-down traversal via `rayon` for large subtrees.
4. **Full CSS Formatting Contexts (Around Taffy, Not Inside `Display::Block`):**
   - `Taffy` is a first-class Flexbox and CSS Grid solver—use it **strictly** for `Flex` and `Grid` formatting contexts.
   - Implement genuine **Block Formatting Context (BFC)** (including collapsing margins and `float`/`clear`), **Inline Formatting Context (IFC)** (line boxes, `inline-block`, baseline alignment, HarfBuzz shaping), **Table Formatting Context** (`table`, `table-row-group`, `table-row`, `table-cell`, column sizing), and **Positioned Layout** (`relative`, `absolute`, `fixed`, `sticky`).
5. **Real `@font-face` & Unlimited System Fonts:** Remove the 32-file cap in `rix-text`, build a fast character-coverage fallback index, and download/register `@font-face` WOFF2/WOFF/TTF/OTF web fonts asynchronously (triggering relayout + repaint on load).

---

## 2. Typed CSS Engine (`rix-css`)

### 2.1 Stylesheet Compilation (`lightningcss` -> Typed IR)
At stylesheet parse time (once per unique CSS source hash):
- Extract `@media`, `@supports`, `@layer` (explicit cascade layer order), `@container`, `@font-face`, `@keyframes`, and style rules.
- Compile selectors using `selectors` crate (from Servo/Gecko) supporting full Selectors Level 4:
  - `:is()`, `:where()`, `:has()`, full selector-list `:not(a, b)`, `:nth-child(An+B of S)`,
  - `:hover`, `:active`, `:focus`, `:focus-visible`, `:focus-within`, `:checked`, `:disabled`, `:enabled`, `:visited`, `:link`, `:target`, `:empty`, `:root`, `:host()`, `::slotted()`,
  - `::before`, `::after`, `::placeholder`, `::selection`.
- Convert each `lightningcss::properties::Property` directly into a typed `SpecifiedDeclaration`:
  - Concrete lengths (`LengthPercentage`, `CalcExpr`), colors (`CssColor` supporting sRGB, HSL, HWB, Lab, LCH, Oklab, Oklch, `color-mix()`), keywords as compact `u8` enums, shorthands expanded into longhands at parse time.
  - Only CSS custom properties (`--foo`) and values referencing `var(--foo)` retain raw token slices for substitution during cascade.

### 2.2 Compact `ComputedStyle`
Split `ComputedStyle` into cache-friendly, `Arc`-shared property groups (just like Stylo/Blink) so children inherit parent structs in O(1) pointer clone without copying bytes when nothing changes:

```rust
pub struct ComputedStyle {
    pub display: Display,          // Compact enum: Block, Inline, InlineBlock, Flex, InlineFlex, Grid, InlineGrid, Table, TableRow, TableCell, ListItem, Contents, None
    pub position: Position,        // Static, Relative, Absolute, Fixed, Sticky
    pub float: Float,              // None, Left, Right
    pub clear: Clear,              // None, Left, Right, Both
    pub box_sizing: BoxSizing,     // ContentBox, BorderBox
    pub overflow: OverflowPair,    // Visible, Hidden, Clip, Scroll, Auto (x & y)
    pub visibility: Visibility,    // Visible, Hidden, Collapse
    pub pointer_events: PointerEvents,
    pub cursor: Cursor,
    pub z_index: ZIndex,           // Auto | Integer(i32)
    pub opacity: f32,
    pub box_model: Arc<BoxModelStyle>,      // width/height/min/max/margin/padding/inset/aspect-ratio
    pub visual: Arc<VisualStyle>,           // background color/images/gradients/url(), borders, radii, shadows
    pub text: Arc<InheritedTextStyle>,      // font-family (SmallVec<InternedStr>), size, weight, style, line-height, color, text-align, white-space, decoration
    pub flex_grid: Option<Arc<FlexGridStyle>>, // allocated ONLY when Flex/Grid/FlexItem/GridItem
    pub effects: Option<Arc<EffectsStyle>>,    // transform, transform-origin, filter, clip-path, mix-blend-mode, transition, animation
    pub custom_properties: Arc<CustomPropertyMap>, // persistent/layered map, shared when unchanged
}
```

### 2.3 Cascade & Invalidation
- **Cascade Order:** UA -> User -> Author (ordered by `@layer`, then specificity, then source order) -> Inline `style=""` -> Animations -> `!important` (reversed origin order).
- **Per-Document `StyleContext`:** Each `Document` owns its compiled `RuleIndex`, `InvalidationMap`, and `StyleMemo` (zero cross-tab lock contention).
- **Missing-Feature Telemetry:** Any unrecognized CSS property name or unsupported at-rule records a deduplicated entry to `MissingFeatureReported` for the `F12` HUD.

---

## 3. Layout Engine (`rix-layout`)

Inspired by **Blink LayoutNG** and **Servo Layout 2020**: separate **Box Tree** (structure derived from DOM + `ComputedStyle`) from **Fragment Tree** (immutable geometric outputs for a given `ConstraintSpace`).

### 3.1 Pipeline
```text
DOM + ComputedStyle
   │  (reconcile only dirty subtrees)
   ▼
Box Tree (FormattingContext roots, anonymous block/inline/table wrappers, ::before/::after)
   │  (solve with ConstraintSpace -> cache Fragment per BoxId + ConstraintKey)
   ▼
Fragment Tree (immutable boxes, line boxes, shaped glyph runs, baselines, overflow rects)
   │  (single pass in true CSS 2.1 Appendix E painting order)
   ▼
Hierarchical DisplayList (Spec 02)
```

### 3.2 Formatting Contexts
1. **Block Formatting Context (BFC):**
   - Vertical stacking, CSS 2.1 margin collapsing (adjacent siblings, parent-first/last-child, empty blocks), `float: left/right` exclusion band tracking, `clear`.
2. **Inline Formatting Context (IFC):**
   - Collects text runs, inline boxes (preserving padding/border/background across line wraps), and atomic inlines (`inline-block`, images, inputs, `inline-flex`, `inline-grid`).
   - Shapes text with `rix-text` (HarfBuzz + UAX #9 BiDi + UAX #14 line breaking) using a **thread-safe shared `Arc<RwLock<TextShapingCache>>`** (never `thread_local!`).
   - Computes line boxes, `vertical-align`, `text-align`, `text-overflow: ellipsis`, and `white-space` modes.
3. **Flex & Grid Formatting Contexts (`Taffy` Sub-Solver):**
   - Delegates `Flex` and `Grid` container sizing/placement to a **retained `taffy::TaffyTree`** scoped to the container (preserving Taffy's internal layout cache instead of calling `TaffyLayoutEngine::new()` every pass).
   - Leaf measurement callbacks invoke our IFC/BFC/replaced intrinsic sizing (`min-content` / `max-content`).
4. **Table Formatting Context (Phase 1):**
   - Anonymous table fixup (`table` -> `table-row-group` -> `table-row` -> `table-cell`), auto & fixed table layout algorithms, `colspan`/`rowspan`, `border-collapse`.
5. **Positioned & Stacking Contexts:**
   - Out-of-flow `absolute`/`fixed` resolved against containing block; `sticky` emits a `SpatialNodeKind::StickyFrame` to the compositor.
   - Painting follows **CSS 2.1 Appendix E** strictly (background/borders -> negative `z-index` stacking contexts -> in-flow blocks -> floats -> in-flow inlines -> `z-index: 0`/auto positioned -> positive `z-index` stacking contexts).

---

## 4. Font Loading Pipeline (`rix-text`)

- **System Fonts:** Index all installed system font metadata via Fontconfig/directory scan (path + family + weight/style + Unicode coverage bitset) lazily—mmap/read actual font bytes only when a family or fallback glyph is first needed. Remove the 32-file hard limit.
- **Web Fonts (`@font-face`):**
  1. Style engine registers `@font-face` descriptors (`font-family`, `src: url(...)`, `font-weight`, `font-style`, `unicode-range`, `font-display`).
  2. First text run requesting that family triggers an async fetch via Network IPC (`ResourcePriority::Font`).
  3. Decode WOFF2 (pure Rust `wuff` / `brotli` + `read-fonts`) -> register face in document `FontFaceSet` -> emit `document.fonts` promise resolution -> mark text-bearing layout boxes dirty -> repaint.

---

## 5. Verification & Acceptance Criteria

- **Benchmarks (`criterion`):**
  - Full style cascade on 10,000 DOM nodes with 3,000 CSS rules: **< 10 ms**.
  - Incremental single-node hover/class toggle restyle on 10,000 nodes: **< 0.5 ms**.
  - Layout pass with cached unchanged subtrees: **< 5 ms**.
- **Visual Reftests & WPT:** Pass targeted WPT `css/CSS2` (box model, floats, tables, stacking contexts), `css/css-flexbox`, `css/css-grid`, `css/selectors`, and `css/css-fonts` suites.
- **Manual Check:** Wikipedia, MDN, Rust docs, and Hacker News render with correct typography, tables, navigation bars, and zero overlapping/misaligned layers.
