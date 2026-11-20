# Spec 03: Streaming HTML5 Parsing & Arena DOM (`rix-html`, `rix-dom`)

**Status:** Foundation Active (Phase 0 -> Phase 1 Upgrade)  
**Governing Architecture:** [docs/ARCHITECTURE.md](../ARCHITECTURE.md)  
**Unsafe Policy:** `#![deny(unsafe_code)]` across both `rix-html` and `rix-dom`.

---

## 1. Objectives & Core Invariants

1. **Single-Pass Streaming Parse:** HTML documents MUST be parsed **once**, inside the Content Process, directly from streaming byte chunks received from `rix-net` / Network Process. Never buffer the full HTML in Browser Process only to parse it a second time in a worker.
2. **Speculative Preload Scanner:** While the main HTML parser is blocked (e.g., waiting on a classic synchronous `<script src="...">`), a lightweight zero-DOM tokenizer scans ahead over incoming bytes to discover `<link rel="stylesheet">`, `<script>`, `<img src/srcset>`, and `<link rel="preload">` and dispatches network requests immediately.
3. **Zero Raw Pixel Buffers in DOM:** `ElementData` MUST NOT store `Vec<u8>` RGBA bitmaps for `<canvas>` or `<video>`. Replaced media elements hold lightweight numeric handles (`SurfaceId` / `ImageId`) resolved by the compositor.
4. **Scoped Style & Layout Dirty Tracking:** Mutating an attribute, class, text node, or pseudo-class (`:hover`, `:focus`, `:active`) MUST mark only the minimal affected set of nodes (never blanket-dirtying `Document::root` on every pointer move).
5. **Memory-Safe JS Ownership:** DOM nodes are stored in an arena with stable generational handles (`NodeKey`) so JS wrappers hold weak/indexed references rather than leaking `JsObject`s for the lifetime of the document.

---

## 2. Streaming HTML5 Pipeline (`rix-html`)

Reference: [WHATWG HTML Parsing Specification](https://html.spec.whatwg.org/multipage/parsing.html).

### 2.1 Incremental `HtmlParserStream`
- Wraps `html5ever::Parser<DomTreeSink>` using UTF-8 streaming decoder (`encoding_rs` for `charset` detection via BOM, HTTP `Content-Type`, and `<meta charset>`).
- Supports **pausing and resuming** at `</script>` boundaries:
  1. When `TreeSink` finishes parsing a classic non-`async`, non-`defer` `<script>`, parser yields `ParsePause::ExecuteScript(NodeId)`.
  2. Content Process executes the script (which may call `document.write(...)` to inject bytes directly at the current insertion point, or mutate already-parsed DOM nodes).
  3. Parser resumes feeding buffered + incoming network chunks.
- Handles `<script defer>` (queued in document order before `DOMContentLoaded`), `<script async>` (executes as soon as fetched), and `<script type="module">` (deferred by default; resolved via module graph + Import Maps).

### 2.2 Speculative Preload Scanner (`rix-html::preload`)
- Runs on raw UTF-8 byte slices using `memchr` fast tag scanning without building DOM nodes.
- Respects `<base href>`, `<noscript>`, `<template>`, and `media` attributes.
- Emits prioritized `PreloadHint { url, kind: Style | Script | Module | Font | Image, priority, crossorigin }` into the Content Process resource scheduler immediately upon chunk arrival.

---

## 3. DOM Tree Model (`rix-dom`)

Reference: [WHATWG DOM Standard](https://dom.spec.whatwg.org/).

### 3.1 Node Storage & Metadata
- Arena-backed tree (`Document`) with monotonic `DocumentUid(u64)`.
- Fast-path metadata per element:
  - Interned `QualName` (`ns`, `local`, `prefix`),
  - Attribute list (`SmallVec<[(QualName, String); 4]>` or compact attribute store) with O(1) cached `id` and pre-tokenized `class` hashes (`SmallVec<[u64; 4]>`), avoiding repeated `split_ascii_whitespace()` during selector matching,
  - Per-node `DirtyFlags` bitmask:
    - `SELF_STYLE_DIRTY` — recompute this element's style,
    - `DESCENDANT_STYLE_DIRTY` — at least one descendant needs style restyle,
    - `SUBTREE_STYLE_DIRTY` — force restyle of entire subtree (e.g., inherited property or CSS custom property changed),
    - `SELF_LAYOUT_DIRTY` / `DESCENDANT_LAYOUT_DIRTY` — incremental layout invalidation,
  - Ancestor Bloom fingerprint cache updated lazily during style traversal,
  - `shadow_root: Option<NodeId>` (open/closed, `delegatesFocus`, `slotAssignment`),
  - `template_contents: Option<NodeId>`,
  - `pseudo_state: ElementState` bitflags (`HOVER | ACTIVE | FOCUS | FOCUS_VISIBLE | FOCUS_WITHIN | CHECKED | DISABLED | ENABLED | VISITED | LINK | TARGET | VALID | INVALID | INDETERMINATE`).

### 3.2 Scoped Pseudo-Class Invalidation
- When pointer hover changes from node `A` to node `B`:
  - Compute the symmetric difference between ancestor chain `ancestors(A)` and `ancestors(B)` (usually 1–4 nodes).
  - Toggle `ElementState::HOVER` only on nodes entering/leaving hover.
  - Query the CSS `InvalidationMap` (see [Spec 04](04-css-and-layout.md)) to mark ONLY elements whose matching rules actually depend on `:hover` (or descendant combinators beneath them). If no rule references `:hover` on those tags/classes, **zero elements are restyled**.

### 3.3 Document Indexing & Live Collections
- `id_map: HashMap<InternedStr, SmallVec<[NodeId; 1]>>` maintained incrementally on connect/disconnect/attribute change for O(1) `getElementById`.
- Declarative Shadow DOM (`<template shadowrootmode="open|closed">`) attached automatically during parsing.
- Mutation Observer records emitted strictly when `active_mutation_observer_count > 0` (zero overhead when unobserved).

---

## 4. Verification & Acceptance Criteria

- **Unit tests:**
  - Chunk-by-chunk 1-byte streaming parse produces identical DOM to whole-document parse.
  - Pause-on-`<script>` + `document.write` insertion order matches WHATWG HTML5 spec.
  - Preload scanner discovers `<link rel="stylesheet">`, `<script>`, and `<img srcset>` before parser unblocks.
  - Hover transition on a 10,000-node tree marks only affected elements (0 restyles when no `:hover` rules apply).
- **WPT Subsets:** Pass core `html/syntax/parsing` and `dom/nodes` suites as Phase 1/2 features land.
