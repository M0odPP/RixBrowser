use rix_html::{
    is_blocking_classic_script, parse_html, HtmlParserStream, ParserStreamStep, PreloadKind,
    PreloadPriority, PreloadScanner,
};

#[test]
fn one_byte_streaming_parse_matches_single_buffer_parse_including_multibyte_utf8() {
    let ascii_safe_html = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
        <title>Streaming \u{20AC} Test</title></head>\
        <body><div id=\"main\" class=\"card hot\">Caf\u{00E9} \u{2014} \
        <b>Hello \u{1F680}</b></div>\
        <article><template shadowrootmode=\"open\"><slot></slot></template></article>\
        </body></html>";

    let expected_doc = parse_html(ascii_safe_html);
    let expected = expected_doc.to_html_string(expected_doc.root());

    let mut stream = HtmlParserStream::new();
    for &byte in ascii_safe_html.as_bytes() {
        stream.feed_bytes(&[byte]);
        while matches!(stream.step(), ParserStreamStep::Script(_)) {}
    }
    stream.finish_bytes();

    assert_eq!(stream.resolved_encoding_name(), "UTF-8");
    let streamed_doc = stream.document();
    let streamed = streamed_doc
        .borrow()
        .to_html_string(streamed_doc.borrow().root());
    assert_eq!(streamed, expected);
}

#[test]
fn streaming_charset_sniffing_supports_windows_1252_meta_and_http_hint() {
    // 0xE9 in windows-1252 is 'é' (\u{00E9}), 0x80 is '€' (\u{20AC}).
    let mut raw = b"<html><head><meta charset=\"windows-1252\"></head><body>".to_vec();
    raw.extend_from_slice(&[b'C', b'a', b'f', 0xE9, b' ', 0x80]);
    raw.extend_from_slice(b"</body></html>");

    let mut stream = HtmlParserStream::new();
    for &b in &raw {
        stream.feed_bytes(&[b]);
    }
    stream.finish_bytes();
    assert_eq!(stream.resolved_encoding_name(), "windows-1252");

    let doc = stream.document();
    let text = doc.borrow().inner_text(doc.borrow().root());
    assert!(
        text.contains("Caf\u{00E9} \u{20AC}"),
        "decoded text: {text}"
    );

    // HTTP hint overrides default UTF-8 immediately.
    let mut hinted = HtmlParserStream::new().with_charset_hint("text/html; charset=windows-1252");
    hinted.feed_bytes(b"<html><body>");
    hinted.feed_bytes(&[0x80]);
    hinted.feed_bytes(b"</body></html>");
    hinted.finish_bytes();
    assert_eq!(hinted.resolved_encoding_name(), "windows-1252");
    let doc2 = hinted.document();
    assert!(doc2
        .borrow()
        .inner_text(doc2.borrow().root())
        .contains('\u{20AC}'));
}

#[test]
fn pause_at_script_and_document_write_preserves_whatwg_insertion_order() {
    let mut stream = HtmlParserStream::new();
    stream.feed_bytes(b"<html><head><script>run()</script></head><body><p>before</p><script>inject()</script><p>after</p></body></html>");

    // Step 1: <head><script>
    let s1 = stream.step();
    let ParserStreamStep::Script(n1) = s1 else {
        panic!("expected first script pause, got {s1:?}");
    };
    assert!(is_blocking_classic_script(&stream.document().borrow(), n1));

    // Step 2: <body><script> -> call document.write("<span>written</span>")
    let s2 = stream.step();
    let ParserStreamStep::Script(n2) = s2 else {
        panic!("expected second script pause, got {s2:?}");
    };
    assert!(is_blocking_classic_script(&stream.document().borrow(), n2));
    stream.insert_document_write("<span id=\"injected\">written</span>");

    assert_eq!(stream.step(), ParserStreamStep::Done);
    stream.finish_bytes();

    let doc = stream.document();
    let serialized = doc.borrow().to_html_string(doc.borrow().root());
    let before_pos = serialized.find("<p>before</p>").unwrap();
    let injected_pos = serialized
        .find("<span id=\"injected\">written</span>")
        .unwrap();
    let after_pos = serialized.find("<p>after</p>").unwrap();
    assert!(
        before_pos < injected_pos && injected_pos < after_pos,
        "unexpected insertion order: {serialized}"
    );
}

#[test]
fn preload_scanner_discovers_resources_across_chunks_respecting_base_template_noscript() {
    let html = br#"<!doctype html>
<html>
<head>
  <!-- <link rel="stylesheet" href="commented.css"> -->
  <base href="https://cdn.example.org/app/v2/">
  <link rel="stylesheet" href="styles/main.css" media="screen">
  <link rel="preload" as="font" href="/fonts/inter.woff2" crossorigin="anonymous">
  <script defer src="js/app.js"></script>
  <script type="module" src="js/mod.mjs"></script>
</head>
<body>
  <template><link rel="stylesheet" href="inside-template.css"><img src="tmpl.png"></template>
  <noscript><img src="fallback.png"><link rel="stylesheet" href="nojs.css"></noscript>
  <script>/* <img src="inside-js-string.png"> */</script>
  <picture>
    <source srcset="hero-1x.webp 1x, hero-2x.webp 2x">
    <img src="hero.png" srcset="hero-sm.png 480w, hero-lg.png 960w">
  </picture>
</body>
</html>"#;

    let mut scanner = PreloadScanner::new(Some("https://example.org/index.html"));
    // Feed in 3-byte chunks across tag boundaries.
    for chunk in html.chunks(3) {
        scanner.push_bytes(chunk);
    }
    let hints = scanner.take_hints();
    let summary: Vec<(PreloadKind, PreloadPriority, String)> = hints
        .iter()
        .map(|h| (h.kind, h.priority, h.url.clone()))
        .collect();

    assert!(summary.contains(&(
        PreloadKind::Style,
        PreloadPriority::Highest,
        "https://cdn.example.org/app/v2/styles/main.css".to_string()
    )));
    assert!(summary.contains(&(
        PreloadKind::Font,
        PreloadPriority::High,
        "https://cdn.example.org/fonts/inter.woff2".to_string()
    )));
    assert!(summary.contains(&(
        PreloadKind::Script,
        PreloadPriority::Medium,
        "https://cdn.example.org/app/v2/js/app.js".to_string()
    )));
    assert!(summary.contains(&(
        PreloadKind::Module,
        PreloadPriority::Medium,
        "https://cdn.example.org/app/v2/js/mod.mjs".to_string()
    )));
    for img in [
        "https://cdn.example.org/app/v2/hero-1x.webp",
        "https://cdn.example.org/app/v2/hero-2x.webp",
        "https://cdn.example.org/app/v2/hero.png",
        "https://cdn.example.org/app/v2/hero-sm.png",
        "https://cdn.example.org/app/v2/hero-lg.png",
    ] {
        assert!(
            summary.contains(&(PreloadKind::Image, PreloadPriority::Low, img.to_string())),
            "missing image preload {img} in {summary:?}"
        );
    }

    // Must ignore comments, template contents, noscript (scripting enabled), and script bodies.
    let urls: Vec<&str> = hints.iter().map(|h| h.url.as_str()).collect();
    for forbidden in [
        "commented.css",
        "inside-template.css",
        "tmpl.png",
        "fallback.png",
        "nojs.css",
        "inside-js-string.png",
    ] {
        assert!(
            !urls.iter().any(|u| u.contains(forbidden)),
            "unexpected forbidden preload {forbidden} in {urls:?}"
        );
    }
}
