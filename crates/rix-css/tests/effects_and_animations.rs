//! Typed CSS parsing & cascade tests for 2D/3D `transform`, `transform-origin`,
//! `filter` (including `drop-shadow`), `clip-path`, `mix-blend-mode`,
//! `linear-gradient`, `radial-gradient`, multi-layer `background-image`,
//! `transition`, and `@keyframes` / `animation`.

use std::sync::Arc;

use rix_css::{
    AnimationDirectionMode, AnimationFillModeKind, BackgroundImageValue, CascadeOrigin,
    ClipPathValue, FilterOp, MediaContext, MixBlendMode, StyleContext, Stylesheet, TimingFunction,
    TransformOp,
};
use rix_html::parse_html;

#[test]
fn parses_and_cascades_transforms_filters_clip_paths_blend_gradients_and_animations() {
    let html = r#"<!doctype html><html><body>
        <div id="fx"></div>
        <div id="bg"></div>
        <div id="anim"></div>
    </body></html>"#;
    let mut doc = parse_html(html);
    let css = r#"
        @keyframes slideFade {
            from { transform: translateX(0px); opacity: 0; }
            50% { transform: translateX(50px) scale(1.2); opacity: 0.5; }
            to { transform: translateX(100px); opacity: 1; }
        }
        #fx {
            transform: perspective(600px) translate3d(10px, 20px, 5px) rotateX(15deg) rotateY(30deg) scale3d(1.5, 1.5, 1);
            transform-origin: 25% 75%;
            filter: blur(4px) brightness(1.2) contrast(0.9) grayscale(50%) opacity(0.8) drop-shadow(2px 4px 6px #ff0000);
            clip-path: polygon(50% 0%, 100% 100%, 0% 100%);
            mix-blend-mode: multiply;
        }
        #bg {
            background-image:
                url("https://example.org/top.png"),
                radial-gradient(circle at 20% 30%, #ff0000, #0000ff),
                linear-gradient(90deg, #00ff00 0%, #ffff00 100%);
        }
        #anim {
            transition: transform 250ms ease-out 50ms, opacity 100ms linear;
            animation: slideFade 1s ease-in-out 2 alternate forwards;
        }
    "#;
    let media = MediaContext::default();
    let sheet = Arc::new(Stylesheet::parse(css, CascadeOrigin::Author, &media));
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(sheet);
    ctx.cascade_full(&mut doc);

    // 1. Check @keyframes lookup
    let kf = ctx.keyframes("slideFade").expect("keyframes slideFade");
    assert_eq!(kf.steps.len(), 3);
    assert!((kf.steps[0].offsets[0] - 0.0).abs() < 1e-4);
    assert!((kf.steps[1].offsets[0] - 0.5).abs() < 1e-4);
    assert!((kf.steps[2].offsets[0] - 1.0).abs() < 1e-4);

    // 2. Check #fx effects
    let fx_id = doc.get_element_by_id("fx").expect("#fx");
    let fx_style = ctx.style_for(fx_id).expect("fx style");
    let fx = fx_style.effects.as_ref().expect("effects");
    assert!(fx
        .transform
        .iter()
        .any(|op| matches!(op, TransformOp::Perspective(p) if (*p - 600.0).abs() < 1e-3)));
    assert!(fx
        .transform
        .iter()
        .any(|op| matches!(op, TransformOp::Translate3d(_, _, z) if (*z - 5.0).abs() < 1e-3)));
    assert!(fx
        .transform
        .iter()
        .any(|op| matches!(op, TransformOp::RotateXDeg(d) if (*d - 15.0).abs() < 1e-3)));
    assert!(fx
        .transform
        .iter()
        .any(|op| matches!(op, TransformOp::RotateYDeg(d) if (*d - 30.0).abs() < 1e-3)));
    assert_eq!(fx.mix_blend_mode, MixBlendMode::Multiply);
    assert!(fx
        .filter
        .iter()
        .any(|f| matches!(f, FilterOp::DropShadow { .. })));
    assert_eq!(fx.filter.len(), 6);
    match &fx.clip_path {
        ClipPathValue::Polygon { fill_odd, points } => {
            assert!(!fill_odd);
            assert_eq!(points.len(), 3);
        }
        other => panic!("expected Polygon clip-path, got {other:?}"),
    }

    // 3. Check #bg multi-layer background-image (url + radial-gradient + linear-gradient)
    let bg_id = doc.get_element_by_id("bg").expect("#bg");
    let bg_style = ctx.style_for(bg_id).expect("bg style");
    assert_eq!(bg_style.visual.background_layers.len(), 3);
    assert!(matches!(
        &bg_style.visual.background_layers[0].image,
        BackgroundImageValue::Url(u) if u.as_str() == "https://example.org/top.png"
    ));
    assert!(matches!(
        &bg_style.visual.background_layers[1].image,
        BackgroundImageValue::RadialGradient {
            shape_circle: true,
            stops,
            ..
        } if stops.len() == 2
    ));
    assert!(matches!(
        &bg_style.visual.background_layers[2].image,
        BackgroundImageValue::LinearGradient {
            angle_deg,
            stops,
        } if (*angle_deg - 90.0).abs() < 1e-3 && stops.len() == 2
    ));

    // 4. Check #anim transitions & animations + easing evaluation
    let anim_id = doc.get_element_by_id("anim").expect("#anim");
    let anim_style = ctx.style_for(anim_id).expect("anim style");
    let ae = anim_style.effects.as_ref().expect("anim effects");
    assert_eq!(ae.transitions.len(), 2);
    assert_eq!(ae.transitions[0].property.as_str(), "transform");
    assert!((ae.transitions[0].duration_ms - 250.0).abs() < 1e-3);
    assert!((ae.transitions[0].delay_ms - 50.0).abs() < 1e-3);
    assert_eq!(ae.animations.len(), 1);
    assert_eq!(ae.animations[0].name.as_str(), "slideFade");
    assert!((ae.animations[0].duration_ms - 1000.0).abs() < 1e-3);
    assert_eq!(ae.animations[0].iteration_count, 2.0);
    assert_eq!(
        ae.animations[0].direction,
        AnimationDirectionMode::Alternate
    );
    assert_eq!(ae.animations[0].fill_mode, AnimationFillModeKind::Forwards);

    // Check cubic-bezier easing monotonicity and endpoints
    let ease_mid = TimingFunction::Ease.evaluate(0.5);
    assert!(ease_mid > 0.65 && ease_mid < 0.9, "ease(0.5)={ease_mid}");
    assert_eq!(TimingFunction::Linear.evaluate(0.25), 0.25);
}
