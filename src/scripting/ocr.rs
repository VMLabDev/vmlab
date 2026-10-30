//! wscript's OCR options (§10.3): the `opts` map `m.ocr_opts` and
//! `m.wait_for_text_opts` take, read into the region and the
//! [`OcrOptions`](crate::vision::OcrOptions) the capture runs with.

use std::collections::HashMap;

use crate::vision::{Layout, OcrOptions};

/// One OCR as a script asked for it: the `(x, y, w, h)` region, if any, and
/// the upscale and layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OcrRequest {
    pub region: Option<(u32, u32, u32, u32)>,
    pub options: OcrOptions,
}

/// Read an `opts` map. The keys are `region` (`"x,y,w,h"`, negatives
/// clamped to zero as everywhere else), `scale` (a whole number, checked
/// against 1 to 4 when the OCR is planned) and `layout` (block, sparse, line
/// or auto). An unknown key is refused by name rather than ignored.
pub fn parse_opts(opts: &HashMap<String, String>) -> Result<OcrRequest, String> {
    let mut out = OcrRequest::default();
    for (key, value) in opts {
        let value = value.trim();
        match key.as_str() {
            "region" => {
                let parts = value
                    .split(',')
                    .map(|p| p.trim().parse::<i64>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| format!("ocr opts: region `{value}` is not x,y,w,h"))?;
                let [x, y, w, h] = parts[..] else {
                    return Err(format!(
                        "ocr opts: region needs x,y,w,h, got {} numbers",
                        parts.len()
                    ));
                };
                let at = |v: i64| v.max(0) as u32;
                out.region = Some((at(x), at(y), at(w), at(h)));
            }
            "scale" => {
                out.options.scale = Some(
                    value
                        .parse()
                        .map_err(|_| format!("ocr opts: scale `{value}` is not a whole number"))?,
                )
            }
            "layout" => {
                out.options.layout = Some(
                    value
                        .parse::<Layout>()
                        .map_err(|e| format!("ocr opts: {e}"))?,
                )
            }
            other => {
                return Err(format!(
                    "ocr opts: unknown key `{other}`; the keys are region, scale and layout"
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_empty_map_is_the_defaults() {
        assert_eq!(parse_opts(&map(&[])).unwrap(), OcrRequest::default());
    }

    #[test]
    fn every_key_reads_into_the_request() {
        let r = parse_opts(&map(&[
            ("region", " 0, 400 ,1024,-2"),
            ("scale", "2"),
            ("layout", "sparse"),
        ]))
        .unwrap();
        assert_eq!(r.region, Some((0, 400, 1024, 0)));
        assert_eq!(r.options.scale, Some(2));
        assert_eq!(r.options.layout, Some(Layout::Sparse));
    }

    #[test]
    fn bad_values_and_unknown_keys_are_refused_by_name() {
        let err = |pairs: &[(&str, &str)]| parse_opts(&map(pairs)).unwrap_err();
        assert!(err(&[("region", "1,2,3")]).contains("got 3 numbers"));
        assert!(err(&[("region", "a,b,c,d")]).contains("is not x,y,w,h"));
        assert!(err(&[("scale", "two")]).contains("not a whole number"));
        assert!(err(&[("layout", "column")]).contains("block, sparse, line and auto"));
        assert!(err(&[("psm", "6")]).contains("unknown key `psm`"));
    }
}
