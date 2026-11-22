use std::sync::Arc;

use rix_css::{CascadeOrigin, MediaContext, Stylesheet};

#[test]
fn stylesheet_parse_cached_reuses_arc_and_separates_media_contexts() {
    let css = r#"
        @import url("/reset.css");
        @import url("/dark-overrides.css") (prefers-color-scheme: dark);
        .hero {
            background-image: url("/images/hero.png");
            color: #111827;
        }
        .badge {
            background-image: url('/images/badge.svg'), url("/images/hero.png");
        }
        @media (prefers-color-scheme: dark) {
            .hero { color: #f8fafc; }
        }
    "#;

    let light_media = MediaContext {
        viewport_width: 1280.0,
        viewport_height: 800.0,
        dark_mode: false,
        ..MediaContext::default()
    };
    let dark_media = MediaContext {
        dark_mode: true,
        ..light_media
    };

    let s1 = Stylesheet::parse_cached(css, CascadeOrigin::Author, &light_media);
    let s2 = Stylesheet::parse_cached(css, CascadeOrigin::Author, &light_media);
    assert!(
        Arc::ptr_eq(&s1, &s2),
        "identical CSS + media should hit the process-wide parsed stylesheet cache"
    );

    let s_dark = Stylesheet::parse_cached(css, CascadeOrigin::Author, &dark_media);
    assert!(
        !Arc::ptr_eq(&s1, &s_dark),
        "dark mode change must produce a distinct compiled stylesheet"
    );

    assert_eq!(s1.imports, vec!["/reset.css".to_string()]);
    assert_eq!(
        s_dark.imports,
        vec!["/reset.css".to_string(), "/dark-overrides.css".to_string()]
    );

    let imgs = s1.referenced_image_urls();
    assert_eq!(imgs.len(), 2);
    assert!(imgs.contains(&"/images/hero.png".to_string()));
    assert!(imgs.contains(&"/images/badge.svg".to_string()));
}
