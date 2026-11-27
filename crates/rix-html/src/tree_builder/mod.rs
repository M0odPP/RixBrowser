//! WHATWG HTML5 §13.2.6 tree-builder specification tables, tag categories, and foreign adjusters.

pub mod data;
pub mod foreign;
#[macro_use]
pub mod tag_sets;
pub mod types;

#[cfg(test)]
mod tests {
    use super::{data, foreign, tag_sets};
    use html5ever::interface::{LimitedQuirks, NoQuirks, Quirks};
    use html5ever::tendril::StrTendril;
    use html5ever::tokenizer::{Doctype, StartTag, Tag};
    use markup5ever::{expanded_name, local_name, ns, Attribute, QualName};

    #[test]
    fn doctype_empty_system_id_is_quirks_per_whatwg_12023() {
        let dt_empty = Doctype {
            name: Some(StrTendril::from("html")),
            public_id: Some(StrTendril::from("-//W3C//DTD HTML 4.01 Transitional//")),
            system_id: Some(StrTendril::from("")),
            force_quirks: false,
        };
        assert_eq!(data::doctype_error_and_quirks(&dt_empty, false).1, Quirks);

        let dt_nonempty = Doctype {
            system_id: Some(StrTendril::from("http://www.w3.org/TR/html4/loose.dtd")),
            ..dt_empty
        };
        assert_eq!(
            data::doctype_error_and_quirks(&dt_nonempty, false).1,
            LimitedQuirks
        );

        let dt_html5 = Doctype {
            name: Some(StrTendril::from("html")),
            public_id: None,
            system_id: None,
            force_quirks: false,
        };
        assert_eq!(data::doctype_error_and_quirks(&dt_html5, false).1, NoQuirks);
    }

    #[test]
    fn special_tag_includes_search_element() {
        assert!(tag_sets::special_tag(expanded_name!(html "search")));
        assert!(!tag_sets::special_tag(expanded_name!(html "span")));
    }

    #[test]
    fn meta_charset_extraction_handles_quotes_and_case() {
        assert_eq!(
            data::extract_meta_charset(StrTendril::from("text/html; ChArSeT='iso-8859-15'"))
                .as_deref(),
            Some("iso-8859-15")
        );
    }

    #[test]
    fn foreign_tag_and_attribute_adjustments_follow_whatwg() {
        let mut tag = Tag {
            kind: StartTag,
            name: local_name!("lineargradient"),
            self_closing: false,
            attrs: vec![
                Attribute {
                    name: QualName::new(None, ns!(), local_name!("viewbox")),
                    value: StrTendril::from("0 0 10 10"),
                },
                Attribute {
                    name: QualName::new(None, ns!(), local_name!("xlink:href")),
                    value: StrTendril::from("#id"),
                },
            ],
            had_duplicate_attributes: false,
        };
        foreign::adjust_svg_tag_name(&mut tag);
        foreign::adjust_svg_attributes(&mut tag);
        foreign::adjust_foreign_attributes(&mut tag);
        assert_eq!(tag.name, local_name!("linearGradient"));
        assert_eq!(tag.attrs[0].name.local, local_name!("viewBox"));
        assert_eq!(tag.attrs[1].name.ns, ns!(xlink));
        assert_eq!(tag.attrs[1].name.local, local_name!("href"));
    }
}
