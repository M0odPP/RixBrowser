//! Spec regression tests for CSS Logical Properties Level 1, two-phase logical
//! cascade, CSS Backgrounds L3 border-width computation, and Dynamic Tarjan SCC
//! custom property cycle resolution (CSS Variables L1 §3 + CSSWG #11500).

#![deny(unsafe_code)]

use rix_core::intern_str;
use rix_css::{
    parse_inline_style, resolve_element_style, resolve_logical_to_physical, serialize_css_tokens,
    CascadeOrigin, ComputedStyle, DirectionMode, LengthPercentage, LengthPercentageAuto,
    MediaContext, PropertyIdKind, SizeValue, Stylesheet, WritingMode,
};

fn resolve_inline(css: &str, parent: Option<&ComputedStyle>) -> ComputedStyle {
    let mut missing = Vec::new();
    let decls = parse_inline_style(css, &mut missing);
    resolve_element_style(&[], &decls, parent, 16.0, &MediaContext::default())
}

fn custom_prop_str(style: &ComputedStyle, name: &str) -> Option<String> {
    style
        .custom_properties
        .get(&intern_str(name))
        .map(serialize_css_tokens)
}

#[test]
fn logical_to_physical_all_eight_writing_mode_direction_combinations() {
    use DirectionMode::{Ltr, Rtl};
    use PropertyIdKind as P;
    use WritingMode::{HorizontalTb, SidewaysLr, SidewaysRl, VerticalLr, VerticalRl};

    // (wm, dir) -> (block-start, block-end, inline-start, inline-end) for margin
    let cases = [
        (
            HorizontalTb,
            Ltr,
            (P::MarginTop, P::MarginBottom, P::MarginLeft, P::MarginRight),
            (P::Width, P::Height),
            P::BorderTopLeftRadius,
        ),
        (
            HorizontalTb,
            Rtl,
            (P::MarginTop, P::MarginBottom, P::MarginRight, P::MarginLeft),
            (P::Width, P::Height),
            P::BorderTopRightRadius,
        ),
        (
            VerticalRl,
            Ltr,
            (P::MarginRight, P::MarginLeft, P::MarginTop, P::MarginBottom),
            (P::Height, P::Width),
            P::BorderTopRightRadius,
        ),
        (
            VerticalRl,
            Rtl,
            (P::MarginRight, P::MarginLeft, P::MarginBottom, P::MarginTop),
            (P::Height, P::Width),
            P::BorderBottomRightRadius,
        ),
        (
            VerticalLr,
            Ltr,
            (P::MarginLeft, P::MarginRight, P::MarginTop, P::MarginBottom),
            (P::Height, P::Width),
            P::BorderTopLeftRadius,
        ),
        (
            VerticalLr,
            Rtl,
            (P::MarginLeft, P::MarginRight, P::MarginBottom, P::MarginTop),
            (P::Height, P::Width),
            P::BorderBottomLeftRadius,
        ),
        (
            SidewaysRl,
            Ltr,
            (P::MarginRight, P::MarginLeft, P::MarginTop, P::MarginBottom),
            (P::Height, P::Width),
            P::BorderTopRightRadius,
        ),
        (
            SidewaysLr,
            Ltr,
            (P::MarginLeft, P::MarginRight, P::MarginBottom, P::MarginTop),
            (P::Height, P::Width),
            P::BorderBottomLeftRadius,
        ),
    ];

    for (wm, dir, (bs, be, is, ie), (inline_sz, block_sz), ss_corner) in cases {
        assert_eq!(
            resolve_logical_to_physical(P::MarginBlockStart, wm, dir),
            bs
        );
        assert_eq!(resolve_logical_to_physical(P::MarginBlockEnd, wm, dir), be);
        assert_eq!(
            resolve_logical_to_physical(P::MarginInlineStart, wm, dir),
            is
        );
        assert_eq!(resolve_logical_to_physical(P::MarginInlineEnd, wm, dir), ie);
        assert_eq!(
            resolve_logical_to_physical(P::InlineSize, wm, dir),
            inline_sz
        );
        assert_eq!(resolve_logical_to_physical(P::BlockSize, wm, dir), block_sz);
        assert_eq!(
            resolve_logical_to_physical(P::BorderStartStartRadius, wm, dir),
            ss_corner
        );
    }
}

#[test]
fn two_phase_logical_cascade_respects_var_writing_mode_and_source_order() {
    let style = resolve_inline(
        "--wm: vertical-rl; writing-mode: var(--wm); margin-block-start: 12px; margin-right: 4px; inline-size: 90px;",
        None,
    );
    assert_eq!(style.text.writing_mode, WritingMode::VerticalRl);
    // In vertical-rl, margin-block-start maps to margin-right; later margin-right: 4px wins.
    assert_eq!(
        style.box_model.margin.right,
        LengthPercentageAuto::Value(LengthPercentage::px(4.0))
    );
    // inline-size maps to height in vertical-rl.
    assert_eq!(
        style.box_model.height,
        SizeValue::Length(LengthPercentage::px(90.0))
    );

    // Reverse source order: margin-block-start declared after margin-right wins.
    let style2 = resolve_inline(
        "writing-mode: vertical-rl; margin-right: 4px; margin-block-start: 12px;",
        None,
    );
    assert_eq!(
        style2.box_model.margin.right,
        LengthPercentageAuto::Value(LengthPercentage::px(12.0))
    );
}

#[test]
fn border_width_computed_value_follows_css_backgrounds_l3() {
    // Default style `none` -> computed border-width is 0px even if width specified.
    let none_border = resolve_inline("border-top-width: 10px;", None);
    assert_eq!(none_border.box_model.border_width.top, 0.0);

    // Solid style without explicit width -> initial `medium` computes to 3px.
    let solid_default = resolve_inline("border-block-start-style: solid;", None);
    assert_eq!(solid_default.box_model.border_width.top, 3.0);
    assert_eq!(solid_default.box_model.border_width.bottom, 0.0);

    // Explicit em width resolved against element's own font-size.
    let em_border = resolve_inline(
        "font-size: 20px; border-inline-start: 0.5em solid red;",
        None,
    );
    assert_eq!(em_border.box_model.border_width.left, 10.0);
}

#[test]
fn dynamic_tarjan_scc_handles_secondary_cycles_and_unused_fallback_cycles() {
    // 1. Overlapping secondary cycle (`--a -> --b -> --c -> {--d, --a}`, `--d -> {--b, --y}`)
    let s1 = resolve_inline(
        "--x: var(--a, valid); --a: var(--b, cycle); --b: var(--c, cycle); \
         --c: var(--d, cycle) var(--a, cycle); --d: var(--b, cycle) var(--y); --y: valid;",
        None,
    );
    assert_eq!(custom_prop_str(&s1, "--a"), None);
    assert_eq!(custom_prop_str(&s1, "--b"), None);
    assert_eq!(custom_prop_str(&s1, "--c"), None);
    assert_eq!(custom_prop_str(&s1, "--d"), None);
    assert_eq!(custom_prop_str(&s1, "--x").as_deref(), Some("valid"));
    assert_eq!(custom_prop_str(&s1, "--y").as_deref(), Some("valid"));

    // 2. Deeper secondary cycle (`--a` references both `--b` and `--c` while on stack)
    let s2 = resolve_inline(
        "--x: var(--a, valid); --a: var(--b, cycle) var(--y, valid) var(--c, cycle); \
         --b: var(--a, cycle); --c: var(--d, cycle); --d: var(--a, cycle); --y: valid;",
        None,
    );
    assert_eq!(custom_prop_str(&s2, "--a"), None);
    assert_eq!(custom_prop_str(&s2, "--b"), None);
    assert_eq!(custom_prop_str(&s2, "--c"), None);
    assert_eq!(custom_prop_str(&s2, "--d"), None);
    assert_eq!(custom_prop_str(&s2, "--x").as_deref(), Some("valid"));
    assert_eq!(custom_prop_str(&s2, "--y").as_deref(), Some("valid"));

    // 3. CSSWG #11500: cycle solely inside unused fallbacks must NOT invalidate any variable.
    let s3 = resolve_inline(
        "--x: var(--a, valid); --a: var(--y, var(--b, cycle)); \
         --b: var(--y, var(--c, cycle)); --c: var(--y, var(--a, cycle)); --y: valid;",
        None,
    );
    assert_eq!(custom_prop_str(&s3, "--a").as_deref(), Some("valid"));
    assert_eq!(custom_prop_str(&s3, "--b").as_deref(), Some("valid"));
    assert_eq!(custom_prop_str(&s3, "--c").as_deref(), Some("valid"));
    assert_eq!(custom_prop_str(&s3, "--x").as_deref(), Some("valid"));
    assert_eq!(custom_prop_str(&s3, "--y").as_deref(), Some("valid"));
}

#[test]
fn custom_property_revert_rolls_back_entire_author_origin_and_post_var_keywords() {
    let parent = resolve_inline("--var: 20px;", None);
    let media = MediaContext::default();
    let sheet = Stylesheet::parse("div { --var: 10px; }", CascadeOrigin::Author, &media);
    let matched: Vec<_> = sheet.rules.iter().collect();
    let mut missing = Vec::new();
    let inline = parse_inline_style("--var: revert;", &mut missing);
    let child = resolve_element_style(&matched, &inline, Some(&parent), 16.0, &media);
    // Inline `revert` must roll back ALL author-origin rules (including `div { --var: 10px; }`)
    // and inherit `20px` from parent.
    assert_eq!(custom_prop_str(&child, "--var").as_deref(), Some("20px"));

    // Post-var() substitution resulting in `initial` makes the custom property guaranteed-invalid.
    let post_initial = resolve_inline("--empty: ; --x: var(--empty) initial;", Some(&parent));
    assert_eq!(custom_prop_str(&post_initial, "--x"), None);
}
