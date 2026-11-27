#[derive(Debug, Clone, PartialEq)]
pub struct ImageCandidate<'a> {
    pub url: &'a str,
    pub density: Option<f32>,
    pub width: Option<u32>,
}

/// Parses an HTML `srcset` attribute strictly according to WHATWG HTML §4.8.4.3.7
/// ("Parsing a srcset attribute"), preserving embedded commas inside URLs (e.g. Cloudflare
/// Image Resizing `/cdn-cgi/image/width=640,quality=100,format=auto/...` and `data:` URIs)
/// and parenthesized future-extension descriptors.
pub fn parse_srcset(srcset: &str) -> Vec<ImageCandidate<'_>> {
    let mut candidates = Vec::new();
    let bytes = srcset.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;

    let is_ws = |b: u8| matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' ');

    while pos < len {
        // Step 3: Skip leading ASCII whitespace and commas.
        while pos < len && (is_ws(bytes[pos]) || bytes[pos] == b',') {
            pos += 1;
        }
        if pos >= len {
            break;
        }

        // Step 5: Collect URL up to the first ASCII whitespace (commas inside URLs are valid!).
        let url_start = pos;
        while pos < len && !is_ws(bytes[pos]) {
            pos += 1;
        }
        let raw_url = &srcset[url_start..pos];

        // Step 6: If URL ends with one or more U+002C COMMA characters, strip all trailing commas
        // and finalize this candidate immediately with no descriptors (default 1x density).
        if raw_url.ends_with(',') {
            let url = raw_url.trim_end_matches(',');
            if !url.is_empty() {
                candidates.push(ImageCandidate {
                    url,
                    density: Some(1.0),
                    width: None,
                });
            }
            continue;
        }

        let url = raw_url;

        // Step 7: Skip ASCII whitespace between URL and descriptors.
        while pos < len && is_ws(bytes[pos]) {
            pos += 1;
        }

        // Step 8: Parse descriptor list using the WHATWG state machine (in descriptor / in parens / after descriptor).
        let mut descriptors: Vec<&str> = Vec::new();
        let mut paren_depth: u32 = 0;
        let mut token_start: Option<usize> = None;

        while pos < len {
            let b = bytes[pos];
            if paren_depth > 0 {
                if b == b')' {
                    paren_depth -= 1;
                }
                pos += 1;
                continue;
            }
            if b == b',' {
                if let Some(st) = token_start.take() {
                    descriptors.push(&srcset[st..pos]);
                }
                pos += 1;
                break;
            }
            if is_ws(b) {
                if let Some(st) = token_start.take() {
                    descriptors.push(&srcset[st..pos]);
                }
                pos += 1;
                continue;
            }
            if token_start.is_none() {
                token_start = Some(pos);
            }
            if b == b'(' {
                paren_depth += 1;
            }
            pos += 1;
        }
        if let Some(st) = token_start {
            descriptors.push(&srcset[st..pos]);
        }

        // Step 10: Interpret width ('w') and pixel density ('x') descriptors.
        let mut density: Option<f32> = None;
        let mut width: Option<u32> = None;
        let mut error = false;

        for token in descriptors {
            if token.is_empty() {
                continue;
            }
            if let Some(w_str) = token.strip_suffix('w') {
                if width.is_some() || density.is_some() {
                    error = true;
                    break;
                }
                if let Ok(val) = w_str.parse::<u32>() {
                    if val > 0 {
                        width = Some(val);
                    } else {
                        error = true;
                        break;
                    }
                } else {
                    error = true;
                    break;
                }
            } else if let Some(d_str) = token.strip_suffix('x') {
                if width.is_some() || density.is_some() {
                    error = true;
                    break;
                }
                if let Ok(val) = d_str.parse::<f32>() {
                    if val > 0.0 && val.is_finite() {
                        density = Some(val);
                    } else {
                        error = true;
                        break;
                    }
                } else {
                    error = true;
                    break;
                }
            } else if token.ends_with('h') {
                // Future 'h' descriptor (or invalid): ignore unless conflicting.
                if density.is_some() {
                    error = true;
                    break;
                }
            } else {
                error = true;
                break;
            }
        }

        if error {
            continue;
        }
        if density.is_none() && width.is_none() {
            density = Some(1.0);
        }

        if !url.is_empty() {
            candidates.push(ImageCandidate {
                url,
                density,
                width,
            });
        }
    }

    candidates
}

pub fn select_image_source<'a>(src: Option<&'a str>, srcset: Option<&'a str>) -> Option<&'a str> {
    if let Some(srcset) = srcset {
        let candidates = parse_srcset(srcset);
        if !candidates.is_empty() {
            let candidate_1x = candidates
                .iter()
                .find(|c| c.density.is_some_and(|d| (d - 1.0).abs() < 0.1));
            let candidate_w = candidates
                .iter()
                .find(|c| c.width.is_some_and(|w| (300..=1280).contains(&w)));

            let chosen = candidate_1x.or(candidate_w).or_else(|| candidates.first());

            if let Some(chosen) = chosen {
                return Some(chosen.url);
            }
        }
    }

    src.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srcset_preserves_embedded_commas_in_urls_per_whatwg() {
        let s = "/cdn-cgi/image/width=640,quality=100,format=auto/https://images.example.com/a.webp 640w, \
                 /cdn-cgi/image/width=1280,quality=100,format=auto/https://images.example.com/a.webp 1280w";
        let c = parse_srcset(s);
        assert_eq!(c.len(), 2);
        assert_eq!(
            c[0].url,
            "/cdn-cgi/image/width=640,quality=100,format=auto/https://images.example.com/a.webp"
        );
        assert_eq!(c[0].width, Some(640));
        assert_eq!(
            c[1].url,
            "/cdn-cgi/image/width=1280,quality=100,format=auto/https://images.example.com/a.webp"
        );
        assert_eq!(c[1].width, Some(1280));
    }

    #[test]
    fn srcset_handles_trailing_commas_and_data_uris() {
        let s = "a.png, b.png 2x, data:image/png;base64,iVBOR,abc 1x";
        let c = parse_srcset(s);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].url, "a.png");
        assert_eq!(c[0].density, Some(1.0));
        assert_eq!(c[1].url, "b.png");
        assert_eq!(c[1].density, Some(2.0));
        assert_eq!(c[2].url, "data:image/png;base64,iVBOR,abc");
        assert_eq!(c[2].density, Some(1.0));
    }
}
