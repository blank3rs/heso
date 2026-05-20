//! # cookies
//!
//! Real `document.cookie` wiring. Bridges the same
//! [`reqwest_cookie_store::CookieStoreMutex`] that `reqwest`'s
//! `cookie_provider` is wired against (see
//! [`heso_engine_fetch::FetchEngine::cookie_jar`]) to the JS
//! `document.cookie` getter and setter, so:
//!
//! - `Set-Cookie` response headers populate the jar via `reqwest`.
//! - JS `document.cookie = '...'` parses + inserts into the same jar.
//! - JS `document.cookie` reads the matching cookies for the current
//!   document URL out of the same jar (skipping `HttpOnly`).
//! - The next `fetch(...)` (in JS or via the static engine) sends
//!   every Cookie matching the request URL.
//!
//! That single shared jar is what makes login flows work end-to-end:
//! the server's `Set-Cookie` lands in the jar, then the next request
//! — whether triggered by `<form>` submission, in-JS `fetch`, or
//! `JsSession::navigate` — picks it back up.
//!
//! ## Spec map
//!
//! - **Cookie syntax** — IETF RFC 6265 §4 (Set-Cookie syntax) and §5
//!   (User Agent Requirements). Implemented inside `cookie_store`
//!   0.21's `parse` method; we don't re-roll any of that.
//! - **`document.cookie` IDL** — WHATWG HTML §6.1
//!   ([`document.cookie`](https://html.spec.whatwg.org/multipage/dom.html#dom-document-cookie)).
//!   The getter returns a `;`-separated list of `name=value` pairs
//!   filtered by path/domain/secure rules **and excluding `HttpOnly`
//!   cookies** (per spec: "an HttpOnly cookie is not exposed to
//!   client-side scripts"). The setter takes a single Set-Cookie
//!   string and inserts it as if it had arrived in a response from
//!   the current document URL. Setting `HttpOnly` via JS is allowed
//!   by `document.cookie` setters in real browsers per the spec
//!   ("The HttpOnly attribute is set if the attribute name matches
//!   case-insensitively"), but the resulting cookie is then invisible
//!   to subsequent `document.cookie` reads — same store behavior, the
//!   filter is read-only-side.
//!
//! ## Why bridge through JS globals
//!
//! [`crate::dom::Document::cookie`] is a `#[rquickjs::class]` method:
//! it has access to the `rquickjs::Ctx<'js>` of the current call but
//! does NOT have access to the Rust-side `Arc<CookieStoreMutex>`
//! (rquickjs's class-instance pattern doesn't let class-method
//! closures capture engine-level state). To bridge, we install two
//! callbacks at engine bootstrap time:
//!
//! - `globalThis.__hesoCookieGet()` — returns the `;`-joined cookie
//!   string for `globalThis.location.href`.
//! - `globalThis.__hesoCookieSet(spec)` — inserts `spec` as a
//!   Set-Cookie from `globalThis.location.href`.
//!
//! Both are native Rust closures that hold a clone of the shared
//! `Arc<CookieStoreMutex>`. The `document.cookie` getter/setter in
//! [`crate::dom::Document`] thunks through them. Same trick the
//! `__hesoFormSubmitNow` and `__hesoCurrentScript` bridges use for
//! the same reason.

use std::sync::Arc;

use reqwest_cookie_store::CookieStoreMutex;
use rquickjs::{Context, Function as RqFunction};
use url::Url;

use crate::engine::EvalError;

/// JS-global name of the getter bridge. Called by
/// [`crate::dom::Document::cookie`].
pub(crate) const GETTER_GLOBAL: &str = "__hesoCookieGet";

/// JS-global name of the setter bridge. Called by
/// [`crate::dom::Document::set_cookie`].
pub(crate) const SETTER_GLOBAL: &str = "__hesoCookieSet";

/// Read the current document URL from `globalThis.location.href`.
/// Returns `None` if `location.href` is missing, not a string, or
/// fails to parse as an absolute URL (e.g. `"about:blank"` is valid;
/// `""` is not). Mirrors the same helper inside `dom.rs` that the
/// hyperlink utils use, kept local here so this module stays
/// self-contained.
fn current_url(ctx: &rquickjs::Ctx<'_>) -> Option<Url> {
    let location: Option<rquickjs::Object> = ctx.globals().get("location").ok()?;
    let location = location?;
    let href: Option<String> = location.get::<_, Option<String>>("href").ok()?;
    let href = href?;
    Url::parse(&href).ok()
}

/// Build the `;`-joined `name=value` cookie string for `url` from
/// `jar`, **excluding any cookie whose `HttpOnly` attribute is set**.
///
/// Per WHATWG HTML §6.1 (`document.cookie` getter):
///   > The getter ... must return the cookie-string for the
///   > document's URL, as defined by RFC 6265, modified by replacing
///   > the cookie list passed to the algorithm with one that only
///   > contains cookies whose http-only-flag is not set.
///
/// `cookie_store::CookieStore::matches` already implements RFC 6265
/// §5.4's domain / path / secure matching (it also already filters
/// expired cookies); we just additionally filter HttpOnly here.
///
/// Returns the empty string if nothing matches, which is the spec
/// default and matches what real browsers return.
pub fn cookies_for_url(jar: &CookieStoreMutex, url: &Url) -> String {
    let guard = match jar.lock() {
        Ok(g) => g,
        // Poisoned mutex — treat as empty store rather than panicking
        // in JS-callable code. Should never happen unless an earlier
        // panic poisoned it; the engine is single-threaded for normal
        // operation.
        Err(_) => return String::new(),
    };
    let mut out = String::new();
    for c in guard.matches(url) {
        // `http_only()` returns `Option<bool>` — `None` means "the
        // Set-Cookie did not mention HttpOnly", which per RFC 6265
        // §5.3.10 means the flag is not set. So we exclude only when
        // the value is `Some(true)`.
        if matches!(c.http_only(), Some(true)) {
            continue;
        }
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(c.name());
        out.push('=');
        out.push_str(c.value());
    }
    out
}

/// Parse `spec` as a Set-Cookie string and insert it into `jar` as if
/// it had arrived from a response to `url`.
///
/// Returns `()` on every path: per WHATWG HTML §6.1
/// (`document.cookie` setter), the IDL setter is void and a malformed
/// Set-Cookie string is silently ignored (matching what `cookie_store`
/// already does — `parse` returning an `Err` is not propagated).
///
/// `cookie_store::CookieStore::parse` is the spec-compliant parser:
/// it handles `Max-Age`, `Expires`, `Path=`, `Domain=`, `Secure`,
/// `HttpOnly`, `SameSite=Lax|Strict|None` per RFC 6265 §4.1 and
/// §5.3. Multi-cookie `Set-Cookie` strings are explicitly NOT
/// supported by the spec for `document.cookie` (it sets one cookie at
/// a time); the parser handles only the first cookie if multiple
/// appear, which matches browser behavior.
pub fn set_cookie_from_js(jar: &CookieStoreMutex, spec: &str, url: &Url) {
    let mut guard = match jar.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    // Best-effort: a malformed cookie string is a silent no-op per
    // spec. The `_` swallows both `CookieError` (parse failure) and a
    // successful `StoreAction::*` discriminant we don't care about.
    let _ = guard.parse(spec, url);
}

/// Install the `__hesoCookieGet()` / `__hesoCookieSet(spec)` JS
/// globals against `jar`. Called once at [`JsEngine`] bootstrap time
/// when a cookie jar is wired in.
///
/// Both closures hold a clone of the shared `Arc<CookieStoreMutex>`.
/// Either is safe to leave installed when no document URL is set
/// (e.g. `eval` before `eval_with_html`): the getter returns `""` if
/// `location.href` is missing, the setter silently ignores cookies
/// inserted against `about:blank` (cookie_store will reject them per
/// RFC 6265 §5.3 since there is no host).
pub fn install_cookie_bridge(
    context: &Context,
    jar: Arc<CookieStoreMutex>,
) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();

            // Getter: returns the `;`-joined cookie string for the
            // current `location.href`.
            let getter_jar = jar.clone();
            let getter = RqFunction::new(ctx.clone(), move |ctx: rquickjs::Ctx| -> String {
                match current_url(&ctx) {
                    Some(url) => cookies_for_url(&getter_jar, &url),
                    None => String::new(),
                }
            })?;
            globals.set(GETTER_GLOBAL, getter)?;

            // Setter: parses `spec` and inserts it into the jar from
            // the current `location.href`. Silently no-ops if there is
            // no current URL (per spec — no host means no cookie).
            let setter_jar = jar.clone();
            let setter = RqFunction::new(
                ctx.clone(),
                move |ctx: rquickjs::Ctx, spec: String| -> () {
                    if let Some(url) = current_url(&ctx) {
                        set_cookie_from_js(&setter_jar, &spec, &url);
                    }
                },
            )?;
            globals.set(SETTER_GLOBAL, setter)?;

            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install cookie bridge: {e}")))?;
    Ok(())
}
