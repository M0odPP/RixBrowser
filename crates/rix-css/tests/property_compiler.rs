//! Tests for typed CSS declaration compilation, shorthand expansion, and token streams.

use rix_css::{
    parse_inline_style, parse_property_from_tokens, ColorRgba, CssColor, CssToken, DisplayMode,
    FlexDirectionMode, FlexWrapMode, FloatMode, GlobalKeyword, LengthPercentage,
    LengthPercentageAuto, LengthUnit, LineStyle, MissingCssKind, OverflowMode, PointerEventsMode,
    PropertyIdKind, PropertyValue, SizeValue, TextDecorationLine,
};

#[test]
fn compiles_shorthands_into_typed_longhands() {
    let mut missing = Vec::new();
    let decls = parse_inline_style(
        "margin: 10px 20px; padding: 4px 8px 12px 16px; border: 2px solid #ff0000; \
         border-radius: 6px; overflow: hidden auto; flex: 1 0 120px; flex-flow: column wrap; \
         gap: 8px 16px; text-decoration: underline #00ff00; font: italic 700 18px/1.5 sans-serif; \
         float: left; pointer-events: none; display: flex !important;",
        &mut missing,
    );
    assert!(
        missing.is_empty(),
        "unexpected missing features: {missing:?}"
    );

    let find = |kind: PropertyIdKind| {
        decls
            .iter()
            .rev()
            .find(|d| d.property == kind)
            .unwrap_or_else(|| panic!("missing declaration for {kind:?}"))
    };

    assert_eq!(
        find(PropertyIdKind::MarginTop).value,
        PropertyValue::LengthAuto(LengthPercentageAuto::Value(LengthPercentage::px(10.0)))
    );
    assert_eq!(
        find(PropertyIdKind::MarginRight).value,
        PropertyValue::LengthAuto(LengthPercentageAuto::Value(LengthPercentage::px(20.0)))
    );
    assert_eq!(
        find(PropertyIdKind::PaddingLeft).value,
        PropertyValue::LengthAuto(LengthPercentageAuto::Value(LengthPercentage::px(16.0)))
    );
    assert_eq!(
        find(PropertyIdKind::BorderTopWidth).value,
        PropertyValue::Length(LengthPercentage::px(2.0))
    );
    assert_eq!(
        find(PropertyIdKind::BorderTopStyle).value,
        PropertyValue::LineStyle(LineStyle::Solid)
    );
    assert_eq!(
        find(PropertyIdKind::BorderTopColor).value,
        PropertyValue::Color(CssColor::Rgba(ColorRgba::rgb(255, 0, 0)))
    );
    assert_eq!(
        find(PropertyIdKind::OverflowX).value,
        PropertyValue::Overflow(OverflowMode::Hidden)
    );
    assert_eq!(
        find(PropertyIdKind::OverflowY).value,
        PropertyValue::Overflow(OverflowMode::Auto)
    );
    assert_eq!(
        find(PropertyIdKind::FlexGrow).value,
        PropertyValue::Number(1.0)
    );
    assert_eq!(
        find(PropertyIdKind::FlexShrink).value,
        PropertyValue::Number(0.0)
    );
    assert_eq!(
        find(PropertyIdKind::FlexDirection).value,
        PropertyValue::FlexDirection(FlexDirectionMode::Column)
    );
    assert_eq!(
        find(PropertyIdKind::FlexWrap).value,
        PropertyValue::FlexWrap(FlexWrapMode::Wrap)
    );
    assert_eq!(
        find(PropertyIdKind::RowGap).value,
        PropertyValue::Length(LengthPercentage::px(8.0))
    );
    assert_eq!(
        find(PropertyIdKind::ColumnGap).value,
        PropertyValue::Length(LengthPercentage::px(16.0))
    );
    assert_eq!(
        find(PropertyIdKind::TextDecorationLine).value,
        PropertyValue::TextDecorationLine(TextDecorationLine::UNDERLINE)
    );
    assert_eq!(
        find(PropertyIdKind::FontWeight).value,
        PropertyValue::Number(700.0)
    );
    assert_eq!(
        find(PropertyIdKind::Float).value,
        PropertyValue::Float(FloatMode::Left)
    );
    assert_eq!(
        find(PropertyIdKind::PointerEvents).value,
        PropertyValue::PointerEvents(PointerEventsMode::None)
    );
    let disp = find(PropertyIdKind::Display);
    assert_eq!(disp.value, PropertyValue::Display(DisplayMode::Flex));
    assert!(disp.important);
}

#[test]
fn compiles_custom_properties_global_keywords_and_token_reparsing() {
    let mut missing = Vec::new();
    let decls = parse_inline_style(
        "--accent: #336699; color: var(--accent, red); width: inherit; height: revert-layer; \
         nonexistent-css-prop: 42;",
        &mut missing,
    );

    assert!(missing.iter().any(|m| {
        m.kind == MissingCssKind::Property && m.name.as_str() == "nonexistent-css-prop"
    }));

    assert!(decls
        .iter()
        .any(|d| d.property == PropertyIdKind::Custom
            && d.custom_name.as_deref() == Some("--accent")));
    assert!(decls.iter().any(|d| {
        d.property == PropertyIdKind::Color
            && matches!(&d.value, PropertyValue::Unresolved(toks) if toks.iter().any(|t| matches!(t, CssToken::Var { .. })))
    }));
    assert!(decls.iter().any(|d| {
        d.property == PropertyIdKind::Width
            && d.value == PropertyValue::Global(GlobalKeyword::Inherit)
    }));
    assert!(decls.iter().any(|d| {
        d.property == PropertyIdKind::Height
            && d.value == PropertyValue::Global(GlobalKeyword::RevertLayer)
    }));

    // Fast-path single token re-parsing (zero string formatting)
    let reparsed = parse_property_from_tokens(
        PropertyIdKind::Width,
        &[CssToken::Dimension(24.0, rix_core::intern_str("rem"))],
        false,
    );
    assert_eq!(reparsed.len(), 1);
    assert_eq!(
        reparsed[0].value,
        PropertyValue::Size(SizeValue::Length(LengthPercentage::Length(
            24.0,
            LengthUnit::Rem
        )))
    );

    // Multi-token shorthand re-parsing after var() expansion
    let border_reparsed = parse_property_from_tokens(
        PropertyIdKind::Border,
        &[
            CssToken::Dimension(3.0, rix_core::intern_str("px")),
            CssToken::Space,
            CssToken::Ident(rix_core::intern_str("dashed")),
            CssToken::Space,
            CssToken::Color(CssColor::rgba(0, 128, 255, 255)),
        ],
        true,
    );
    assert!(border_reparsed.iter().any(|d| {
        d.property == PropertyIdKind::BorderTopWidth
            && d.value == PropertyValue::Length(LengthPercentage::px(3.0))
            && d.important
    }));
    assert!(border_reparsed.iter().any(|d| {
        d.property == PropertyIdKind::BorderBottomStyle
            && d.value == PropertyValue::LineStyle(LineStyle::Dashed)
    }));
}
