//! Response classification for the search backend pool.
//!
//! Per ADR 0026, `classify_response(backend, status, body)` maps a raw
//! HTTP response into a typed outcome — genuine results, a rate-limit, a
//! bot challenge, or a config error — so a throttled backend can never be
//! silently parsed as an empty result set. This is the home for that
//! classifier and its per-backend throttle signatures; it reuses
//! `heso_engine_fetch::is_bot_challenge` for the shared WAF-body needles.

use reqwest::StatusCode;

use super::parse::{brave_parse_html, ddg_lite_parse_html, ddg_parse_html, mojeek_parse_html};
use super::{BackendId, RawResult};

/// The typed result of asking one backend for one page. `Results(vec![])`
/// (a genuine no-match) and `RateLimited`/`BotChallenge`/`ConfigError`
/// (the backend declined to answer) carry different meanings by design, so
/// the orchestrator can surface each loudly to the caller rather than fold
/// a throttle into "zero results".
#[derive(Debug)]
pub(super) enum BackendOutcome {
    /// Parsed rows. May be empty — that is a genuine no-match, not a block.
    Results(Vec<RawResult>),
    /// The backend rate-limited us (a 429/503/202, or a 2xx whose body is
    /// the abuse/landing page rather than a result set). `retried` is how
    /// many retries the transport layer already burned before giving up.
    RateLimited { status: Option<u16>, retried: u32 },
    /// The backend served a WAF / proof-of-work / CAPTCHA interstitial.
    BotChallenge { marker: &'static str },
    /// The backend is misconfigured for our use (e.g. SearXNG with JSON
    /// output disabled, returning HTML when we asked for `format=json`).
    ConfigError(String),
}

/// Number of retries the transport layer performs before handing a still-
/// throttled response back for classification. Carried into
/// `RateLimited.retried` so the error envelope can report it.
pub(super) const TRANSPORT_RETRIES: u32 = super::http::MAX_RETRIES as u32;

/// Body markers DuckDuckGo serves on its anomaly-detection / bot screen.
/// Matched as the `marker` in a [`BackendOutcome::BotChallenge`] so an
/// agent sees exactly which signal fired.
const DDG_CHALLENGE_MARKERS: &[&str] = &[
    "DDG.deep.anomalyDetectionBlock",
    "window.execDeep",
    "not a Robot",
];

/// Phrases Mojeek puts on the per-IP abuse / rate-limit interstitial it
/// serves in place of a result page. Used only when the result container
/// is absent, so a genuine no-match page (container present, zero `<li>`)
/// is never mistaken for a block.
const MOJEEK_ABUSE_NEEDLES: &[&str] = &[
    "unusual traffic",
    "too many requests",
    "rate limit",
    "automated queries",
];

/// Markers Brave serves on its proof-of-work / CAPTCHA challenge screen,
/// matched only when no result container is present so a genuine empty
/// page is never mistaken for a challenge.
const BRAVE_CHALLENGE_MARKERS: &[&str] = &[
    "challenge-platform",
    "captcha",
    "Please verify you are a human",
];

/// Classify one page from `backend`. `status` is the HTTP status the
/// transport layer settled on (after any retries) and `body` is the
/// already-read response body.
pub(super) fn classify_response(
    backend: BackendId,
    status: StatusCode,
    body: &str,
) -> BackendOutcome {
    let code = status.as_u16();

    // A retryable status that survived the transport layer's retries is a
    // throttle no matter which backend served it — the retries are spent,
    // so reporting it loudly is the only honest move.
    if code == 429 || code == 503 || code == 408 || (500..=599).contains(&code) {
        return BackendOutcome::RateLimited {
            status: Some(code),
            retried: TRANSPORT_RETRIES,
        };
    }

    match backend {
        // The HTML and lite DuckDuckGo endpoints share one gate (202
        // throttle, anomaly-detection markers, redirect-wrapped hrefs).
        BackendId::DdgHtml => classify_ddg(code, body, ddg_parse_html),
        BackendId::DdgLite => classify_ddg(code, body, ddg_lite_parse_html),
        BackendId::Mojeek => classify_mojeek(code, body),
        BackendId::Brave => classify_brave(code, body),
        // SearXNG and Marginalia are JSON (their config/throttle
        // signatures are checked in `http` against the parsed body), and
        // Wikipedia is the knowledge block, not a result backend — none
        // routes its body through this HTML classifier. The generic status
        // gate above is their only shared classification; anything past it
        // is a clean body for the caller to parse.
        BackendId::SearxNg | BackendId::Marginalia | BackendId::Wiki => {
            BackendOutcome::Results(Vec::new())
        }
    }
}

/// DuckDuckGo (HTML and lite): a `403` is its hard per-IP block (the retry
/// layer treats 403 as permanent, so it arrives here intact), a
/// `202 Accepted` is its scripted-caller throttle, and its
/// anomaly-detection screen ships recognisable body markers. Anything else
/// parses normally with the endpoint-specific `parse` — an empty parse on a
/// 200 is a genuine no-match.
fn classify_ddg(code: u16, body: &str, parse: fn(&str) -> Vec<RawResult>) -> BackendOutcome {
    if code == 403 {
        return BackendOutcome::RateLimited {
            status: Some(403),
            retried: 0,
        };
    }
    if code == 202 {
        return BackendOutcome::RateLimited {
            status: Some(202),
            retried: TRANSPORT_RETRIES,
        };
    }
    for marker in DDG_CHALLENGE_MARKERS {
        if body.contains(marker) {
            return BackendOutcome::BotChallenge { marker };
        }
    }
    if let Some(marker) = waf_marker(body) {
        return BackendOutcome::BotChallenge { marker };
    }
    BackendOutcome::Results(parse(body))
}

/// Brave Search: a `403` is its hard per-IP block (the retry layer already
/// treats 403 as permanent, so it arrives here intact). A 2xx body that
/// carries a proof-of-work / CAPTCHA challenge script and no result
/// container is a bot challenge; otherwise the page parses normally and an
/// empty parse is a genuine no-match.
fn classify_brave(code: u16, body: &str) -> BackendOutcome {
    if code == 403 {
        return BackendOutcome::RateLimited {
            status: Some(403),
            retried: 0,
        };
    }
    let rows = brave_parse_html(body);
    if !rows.is_empty() {
        return BackendOutcome::Results(rows);
    }
    // No results — disambiguate a genuine empty page from a challenge.
    for marker in BRAVE_CHALLENGE_MARKERS {
        if body.contains(marker) {
            return BackendOutcome::BotChallenge { marker };
        }
    }
    if let Some(marker) = waf_marker(body) {
        return BackendOutcome::BotChallenge { marker };
    }
    BackendOutcome::Results(rows)
}

/// Mojeek: a `403` is a hard per-IP block (permanent, so the retry layer
/// passes it through intact). Otherwise the result container
/// (`ul.results-standard`) is the tell: when it is present, the page is
/// genuine — zero `<li>` means an honest no-match. When it is absent AND
/// the body carries an abuse/rate-limit phrase, Mojeek served its
/// interstitial in place of results, which is a throttle.
fn classify_mojeek(code: u16, body: &str) -> BackendOutcome {
    if code == 403 {
        return BackendOutcome::RateLimited {
            status: Some(403),
            retried: 0,
        };
    }
    if let Some(marker) = waf_marker(body) {
        return BackendOutcome::BotChallenge { marker };
    }
    let rows = mojeek_parse_html(body);
    if !rows.is_empty() {
        return BackendOutcome::Results(rows);
    }
    if body.contains("results-standard") {
        // Container present, zero rows: a genuine empty result page.
        return BackendOutcome::Results(rows);
    }
    let lowered = body.to_ascii_lowercase();
    if MOJEEK_ABUSE_NEEDLES.iter().any(|n| lowered.contains(n)) {
        return BackendOutcome::RateLimited {
            status: None,
            retried: TRANSPORT_RETRIES,
        };
    }
    // No container, no abuse phrase — an empty page Mojeek genuinely
    // returned (e.g. a nonsense query). Honest no-match.
    BackendOutcome::Results(rows)
}

/// Shared WAF / Cloudflare / Reddit interstitial detection — reuses the
/// engine's `is_bot_challenge` needles rather than reimplementing them, so
/// the search verb and the fetch engine agree on what a block looks like.
fn waf_marker(body: &str) -> Option<&'static str> {
    if heso_engine_fetch::is_bot_challenge(body) {
        Some("waf_challenge")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(code: u16) -> StatusCode {
        StatusCode::from_u16(code).unwrap()
    }

    #[test]
    fn ddg_202_is_rate_limited() {
        let out = classify_response(BackendId::DdgHtml, st(202), "<html></html>");
        assert!(matches!(
            out,
            BackendOutcome::RateLimited {
                status: Some(202),
                ..
            }
        ));
    }

    #[test]
    fn ddg_lite_202_is_rate_limited() {
        // The lite endpoint shares the DDG gate: a 202 is the same throttle.
        let out = classify_response(BackendId::DdgLite, st(202), "<html></html>");
        assert!(matches!(
            out,
            BackendOutcome::RateLimited {
                status: Some(202),
                ..
            }
        ));
    }

    #[test]
    fn ddg_403_is_rate_limited() {
        // A DDG 403 is a permanent per-IP block: it must surface loudly,
        // never parse a block page into an empty result set.
        let out = classify_response(BackendId::DdgHtml, st(403), "<html>blocked</html>");
        assert!(matches!(
            out,
            BackendOutcome::RateLimited {
                status: Some(403),
                retried: 0,
            }
        ));
    }

    #[test]
    fn ddg_anomaly_body_is_bot_challenge() {
        let body = "<html><script>DDG.deep.anomalyDetectionBlock = 1;</script></html>";
        let out = classify_response(BackendId::DdgHtml, st(200), body);
        match out {
            BackendOutcome::BotChallenge { marker } => {
                assert_eq!(marker, "DDG.deep.anomalyDetectionBlock");
            }
            other => panic!("expected BotChallenge, got {other:?}"),
        }
    }

    #[test]
    fn brave_403_is_rate_limited() {
        let out = classify_response(BackendId::Brave, st(403), "<html>blocked</html>");
        assert!(matches!(
            out,
            BackendOutcome::RateLimited {
                status: Some(403),
                retried: 0,
            }
        ));
    }

    #[test]
    fn brave_challenge_body_is_bot_challenge() {
        // A 2xx with a PoW/CAPTCHA script and no result container.
        let body = r#"<html><body><div id="challenge-platform"></div></body></html>"#;
        let out = classify_response(BackendId::Brave, st(200), body);
        match out {
            BackendOutcome::BotChallenge { marker } => assert_eq!(marker, "challenge-platform"),
            other => panic!("expected BotChallenge, got {other:?}"),
        }
    }

    #[test]
    fn brave_clean_200_parses_results() {
        let body = r#"<html><body>
            <div class="snippet" data-type="web">
                <a class="heading-serpresult" href="https://example.com/">
                    <div class="title">Example</div>
                </a>
                <div class="snippet-description">snippet</div>
            </div>
        </body></html>"#;
        let out = classify_response(BackendId::Brave, st(200), body);
        match out {
            BackendOutcome::Results(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Results, got {other:?}"),
        }
    }

    #[test]
    fn brave_genuine_empty_is_results_not_challenge() {
        // No results AND no challenge marker — a genuine no-match page.
        let body = "<html><body><p>No results found.</p></body></html>";
        let out = classify_response(BackendId::Brave, st(200), body);
        match out {
            BackendOutcome::Results(rows) => assert!(rows.is_empty()),
            other => panic!("expected empty Results, got {other:?}"),
        }
    }

    #[test]
    fn mojeek_429_is_rate_limited() {
        let out = classify_response(BackendId::Mojeek, st(429), "anything");
        assert!(matches!(
            out,
            BackendOutcome::RateLimited {
                status: Some(429),
                ..
            }
        ));
    }

    #[test]
    fn mojeek_403_is_rate_limited() {
        // A bare Mojeek 403 block page carries no WAF marker, no result
        // container, and no abuse needle — without the status guard it would
        // parse to an empty result set. It must surface as a throttle.
        let out = classify_response(BackendId::Mojeek, st(403), "<html>blocked</html>");
        assert!(matches!(
            out,
            BackendOutcome::RateLimited {
                status: Some(403),
                retried: 0,
            }
        ));
    }

    #[test]
    fn mojeek_container_present_zero_li_is_genuine_empty() {
        // The result container exists but holds no <li> — a genuine
        // no-match page, NOT a throttle.
        let body = r#"<html><body><ul class="results-standard"></ul></body></html>"#;
        let out = classify_response(BackendId::Mojeek, st(200), body);
        match out {
            BackendOutcome::Results(rows) => assert!(rows.is_empty()),
            other => panic!("expected empty Results, got {other:?}"),
        }
    }

    #[test]
    fn mojeek_no_container_with_abuse_text_is_rate_limited() {
        let body = "<html><body><h1>Too many requests from your network</h1></body></html>";
        let out = classify_response(BackendId::Mojeek, st(200), body);
        assert!(matches!(
            out,
            BackendOutcome::RateLimited { status: None, .. }
        ));
    }

    #[test]
    fn mojeek_2xx_with_results_parses() {
        let body = r#"<html><body><ul class="results-standard">
            <li><h2><a class="title" href="https://example.com/">Example</a></h2>
            <p class="s">snippet</p></li>
        </ul></body></html>"#;
        let out = classify_response(BackendId::Mojeek, st(200), body);
        match out {
            BackendOutcome::Results(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Results, got {other:?}"),
        }
    }

    #[test]
    fn generic_5xx_is_rate_limited_for_any_backend() {
        let out = classify_response(BackendId::Mojeek, st(503), "");
        assert!(matches!(
            out,
            BackendOutcome::RateLimited {
                status: Some(503),
                ..
            }
        ));
        let out = classify_response(BackendId::DdgHtml, st(500), "");
        assert!(matches!(
            out,
            BackendOutcome::RateLimited {
                status: Some(500),
                ..
            }
        ));
    }

    #[test]
    fn ddg_clean_200_parses_results() {
        let body = r#"<html><body>
            <div class="result">
                <div class="result__title">
                    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F">Example</a>
                </div>
                <div class="result__snippet">snippet</div>
            </div>
        </body></html>"#;
        let out = classify_response(BackendId::DdgHtml, st(200), body);
        match out {
            BackendOutcome::Results(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Results, got {other:?}"),
        }
    }
}
