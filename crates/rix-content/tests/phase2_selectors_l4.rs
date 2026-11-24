use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_text::FontDatabase;

fn make_runtime() -> ContentRuntime {
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    ContentRuntime::new(doc, Size::new(800.0, 600.0), font_db)
}

#[test]
fn selectors_l4_has_invalidation_and_dom_mixins() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      #subject { color: rgb(128, 128, 128); }
      #subject:has(> .trigger) { color: rgb(0, 128, 0); }
      #prev { color: rgb(128, 128, 128); }
      #prev:has(+ #next.active) { color: rgb(0, 0, 255); }
      #parent:has(.deep) #child { background-color: rgb(255, 0, 0); }
    </style>
    <div id="subject"></div>
    <div id="prev"></div><div id="next"></div>
    <div id="parent"><div id="child"></div><div id="holder"></div></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                if (getComputedStyle(subject).color !== "rgb(128, 128, 128)") return "init-subject";
                const trig = document.createElement("span");
                trig.className = "trigger";
                subject.append(trig);
                if (getComputedStyle(subject).color !== "rgb(0, 128, 0)") return "after-append:" + getComputedStyle(subject).color;
                trig.remove();
                if (getComputedStyle(subject).color !== "rgb(128, 128, 128)") return "after-remove:" + getComputedStyle(subject).color;

                next.classList.add("active");
                if (getComputedStyle(prev).color !== "rgb(0, 0, 255)") return "sibling-has-add:" + getComputedStyle(prev).color;
                next.classList.remove("active");
                if (getComputedStyle(prev).color !== "rgb(128, 128, 128)") return "sibling-has-rem";

                const deep = document.createElement("em");
                deep.className = "deep";
                holder.replaceChildren(deep);
                if (getComputedStyle(child).backgroundColor !== "rgb(255, 0, 0)") return "non-subject-has:" + getComputedStyle(child).backgroundColor;
                const replacement = document.createElement("i");
                deep.replaceWith(replacement);
                if (getComputedStyle(child).backgroundColor === "rgb(255, 0, 0)") return "non-subject-has-clear";

                return "ok";
            })()
            "#,
        )
        .unwrap();
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn selectors_l4_dir_lang_attributes_namespaces_and_grouping_rules() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style id="s">
      @namespace xml "http://www.w3.org/XML/1998/namespace";
      [xml|lang="A" i] { visibility: hidden; }
      .styled[style*="text-decoration"] { color: rgb(0, 128, 0); }
      #langbox:lang("*-Latn") { opacity: 0.5; }
    </style>
    <div id="rtlWrap" dir="rtl"><input id="telInput" type="tel"></div>
    <div id="autoWrap" dir="auto"><div id="autoChild">a</div></div>
    <div id="nsTarget"></div>
    <div id="styleTarget" class="styled"></div>
    <div id="langbox" lang="sr-Latn-RS"></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                if (getComputedStyle(telInput).direction !== "ltr") return "tel-dir-ltr";
                telInput.type = "text";
                if (getComputedStyle(telInput).direction !== "rtl") return "text-dir-rtl:" + getComputedStyle(telInput).direction;

                if (!autoWrap.matches(":dir(ltr)")) return "auto-init-ltr";
                autoChild.innerText = "\u05EA";
                if (!autoWrap.matches(":dir(rtl)")) return "auto-rtl-after-innerText";

                if (getComputedStyle(langbox).opacity !== "0.5") return "lang-wildcard-fail";

                nsTarget.setAttributeNS("http://www.w3.org/XML/1998/namespace", "lang", "a");
                if (getComputedStyle(nsTarget).visibility !== "hidden") return "ns-attr-fail:" + getComputedStyle(nsTarget).visibility;

                // setAttributeNS preserves case so [foo='bar' i] must not match FOO.
                nsTarget.setAttributeNS(null, "FOO", "bar");
                if (nsTarget.matches('[foo="bar" i]')) return "ns-case-should-not-match";

                // Element.style [PutForwards=cssText] setter + invalidation.
                styleTarget.style = "text-decoration: underline";
                if (getComputedStyle(styleTarget).color !== "rgb(0, 128, 0)") return "style-setter-fail";

                // CSSGroupingRule live cssRules / insertRule / deleteRule + canonical CSSOM selectorText.
                const st = document.createElement("style");
                document.head.appendChild(st);
                st.textContent = "@media all {}";
                const media = st.sheet.cssRules[0];
                media.insertRule('[foo="bar" /**/ i] {}', 0);
                if (media.cssRules.length !== 1) return "media-insert-len";
                if (media.cssRules[0].selectorText !== '[foo="bar" i]') return "media-sel:" + media.cssRules[0].selectorText;
                media.deleteRule(0);
                if (media.cssRules.length !== 0) return "media-delete-len";

                // Focus & activeElement.
                telInput.focus();
                if (document.activeElement !== telInput || !telInput.matches(":focus")) return "focus-fail";
                telInput.blur();
                if (telInput.matches(":focus")) return "blur-fail";

                return "ok";
            })()
            "#,
        )
        .unwrap();
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}
