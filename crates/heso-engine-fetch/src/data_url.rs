//! # data_url
//!
//! RFC-2397 `data:` URL parsing, shared across the fetch and JS engines.
//!
//! A `data:` URL carries a self-contained document inline
//! (`data:[<mime>][;base64],<payload>`). Two callers decode it through
//! this one parser:
//!
//! - The static read path ([`crate::FetchEngine::open_typed`]) intercepts
//!   `data:` before reqwest — reqwest only speaks HTTP(S) — and treats a
//!   text/HTML-ish body as a document with no network round-trip.
//! - The JS engine's inline-`<script src="data:...">` loader
//!   (`heso-engine-js::scripts::fetch_script_source`) decodes the body and
//!   hands it straight to the script engine.
//!
//! One decoder, two consumers — base64 and percent-encoding are handled
//! here so neither caller hand-rolls its own.

/// Decoded `data:` URL: its bare MIME type and body bytes.
pub struct DataPayload {
    /// The MIME type from the URL's meta segment (`text/plain;charset=US-ASCII`
    /// when the segment is empty, per RFC 2397).
    pub mime: String,
    /// The decoded body — base64-decoded or percent-decoded depending on
    /// the URL's `;base64` flag.
    pub body: Vec<u8>,
}

/// Parse a `data:[<mime>][;base64],<payload>` URL into its mime type
/// and body bytes. Returns `None` if `url` isn't a data URL (no `data:`
/// prefix, or no `,` separator) or its base64 payload is malformed.
pub fn parse_data_url(url: &str) -> Option<DataPayload> {
    let rest = url.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let payload = &rest[comma + 1..];
    let is_base64 = meta.ends_with(";base64");
    let mime = if is_base64 {
        meta.trim_end_matches(";base64").to_owned()
    } else {
        meta.to_owned()
    };
    let mime = if mime.is_empty() {
        "text/plain;charset=US-ASCII".to_owned()
    } else {
        mime
    };
    let body = if is_base64 {
        base64_decode(payload)?
    } else {
        urlencoding_decode(payload).unwrap_or_else(|| payload.as_bytes().to_vec())
    };
    Some(DataPayload { mime, body })
}

/// Tiny base64 decoder for `data:;base64,...` URLs — not worth a crate.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for b in bytes {
        if b == b'=' {
            break;
        }
        let v = lookup[b as usize];
        if v == 255 {
            return None;
        }
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Tiny percent-decoder for `data:,...` URLs.
fn urlencoding_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            let hi = hex(bytes[i + 1])?;
            let lo = hex(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else if b == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Some(out)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_url_plain_text() {
        let p = parse_data_url("data:text/plain,hello").expect("parse");
        assert_eq!(p.mime, "text/plain");
        assert_eq!(p.body, b"hello");
    }

    #[test]
    fn parse_data_url_percent_decoded() {
        let p = parse_data_url("data:text/plain,hello%20world").expect("parse");
        assert_eq!(p.body, b"hello world");
    }

    #[test]
    fn parse_data_url_base64() {
        // base64("hi") == "aGk="
        let p = parse_data_url("data:application/octet-stream;base64,aGk=").expect("parse");
        assert_eq!(p.mime, "application/octet-stream");
        assert_eq!(p.body, b"hi");
    }

    #[test]
    fn parse_data_url_default_mime_for_empty() {
        let p = parse_data_url("data:,abc").expect("parse");
        assert_eq!(p.mime, "text/plain;charset=US-ASCII");
        assert_eq!(p.body, b"abc");
    }

    #[test]
    fn parse_data_url_returns_none_for_non_data() {
        assert!(parse_data_url("https://example.com").is_none());
        assert!(parse_data_url("data:no-comma").is_none());
    }

    #[test]
    fn base64_round_trip_short() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
    }
}
