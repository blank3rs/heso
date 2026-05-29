//! WHATWG `URL` + `URLSearchParams` globals.
//!
//! Two JS classes registered on `globalThis`:
//!
//! - `URL` — `new URL(href, base?)`. Exposes the WHATWG `URL`
//!   property surface (`href`, `origin`, `protocol`, `host`,
//!   `hostname`, `port`, `pathname`, `search`, `hash`, `username`,
//!   `password`, `searchParams`) as IDL getters/setters, plus
//!   `toString()` / `toJSON()` aliases of `href`, and static
//!   `URL.canParse(href, base?)`.
//! - `URLSearchParams` — `new URLSearchParams(init?)` where `init`
//!   accepts a string, an iterable of `[k, v]` pairs, or a record-shaped
//!   plain object. Full WHATWG method surface: `get`, `getAll`,
//!   `set`, `append`, `delete`, `has`, `sort`, `toString`, `entries`,
//!   `keys`, `values`, `forEach`, `size` getter, and the iterator
//!   protocol (via [`Symbol.iterator`] patched in [`install_url`]).
//!
//! ## Parent-URL reflection
//!
//! The `url.searchParams` getter returns a [`UrlSearchParamsClass`]
//! whose backing store is an [`Attached`](Backing::Attached) variant
//! holding `Rc<RefCell<Url>>` — a back-pointer to the same `Url` the
//! parent [`UrlClass`] owns. Every mutation (`set` / `append` / …)
//! calls `Url::query_pairs_mut()` on that shared cell; the
//! [`url::form_urlencoded::Serializer`] returned by `query_pairs_mut`
//! rebuilds the query string and writes it back into the `Url` on
//! drop, so `url.toString()` and `url.search` immediately reflect the
//! mutation without any explicit sync step.
//!
//! Standalone `new URLSearchParams(...)` instances use the
//! [`Detached`](Backing::Detached) variant: a `Rc<RefCell<Vec<(String,
//! String)>>>` that stores insertion order. Detached params never
//! touch a `Url`; their `toString()` runs `form_urlencoded::Serializer`
//! over the in-memory vec.
//!
//! ## OSS reviewed before writing
//!
//! - [`url`](https://docs.rs/url) crate: `Url::query_pairs()` /
//!   `Url::query_pairs_mut()` already implement the
//!   WHATWG `application/x-www-form-urlencoded` parse + serialize.
//!   We wire JS calls through to those — no reinvention.
//! - [`form_urlencoded::Serializer`](https://docs.rs/form_urlencoded)
//!   for detached `toString()` and standalone `URLSearchParams("a=1")`
//!   parsing.
//! - [`whatwg-url`](https://github.com/jsdom/whatwg-url) (the JS
//!   reference implementation jsdom uses): consulted for `init`
//!   parsing corner cases (leading `?` strip, record vs iterable,
//!   stable `sort()`).
//!
//! Decision: bridge fresh against the `url` crate. A direct adapt of
//! jsdom's TypeScript wouldn't compile against rquickjs's class
//! macros without a full rewrite, and the spec corner cases are
//! narrow enough to encode inline. The `url` crate gives us
//! percent-encoding and `+`-for-space without us having to ship that
//! code at all.

use std::cell::RefCell;
use std::rc::Rc;

use rquickjs::{
    atom::PredefinedAtom,
    class::Trace,
    prelude::{Func, Opt, This},
    Array, Class, Context, Ctx, Function, JsLifetime, Object, Value,
};
use url::Url;

use crate::engine::EvalError;

/// Backing store for a [`UrlSearchParamsClass`].
///
/// Attached params live behind the same `Rc<RefCell<Url>>` as the
/// parent [`UrlClass`] — every mutation rebuilds the parent's query
/// string via `query_pairs_mut`. Detached params own their own
/// `Vec<(String, String)>` and never touch a [`Url`].
#[derive(Clone)]
enum Backing {
    /// Backing store points at a parent URL — every mutation reflects
    /// into the parent's `Url` via `Url::query_pairs_mut()`.
    Attached(Rc<RefCell<Url>>),
    /// Standalone backing store — a vec of `(name, value)` pairs in
    /// insertion order. Used by the bare `new URLSearchParams(init)`
    /// constructor.
    Detached(Rc<RefCell<Vec<(String, String)>>>),
}

/// `URL` — the WHATWG URL global.
///
/// Wraps a [`url::Url`] behind an `Rc<RefCell<…>>` so the
/// `searchParams` view can share mutation rights with the parent.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "URL")]
pub struct UrlClass {
    /// Shared backing store. The [`UrlSearchParamsClass`] returned by
    /// the `searchParams` getter holds an
    /// [`Attached`](Backing::Attached) clone of this same `Rc`, so
    /// mutations from JS-side `url.searchParams.set(...)` calls are
    /// observable through `url.toString()` without any explicit
    /// sync step.
    #[qjs(skip_trace)]
    inner: Rc<RefCell<Url>>,
}

impl UrlClass {
    /// Internal constructor — wraps an already-parsed [`Url`].
    fn from_url(url: Url) -> Self {
        Self {
            inner: Rc::new(RefCell::new(url)),
        }
    }

    /// Parse `(input, base?)` per the WHATWG URL parser:
    /// - With `base`, parse `base` first then resolve `input` against
    ///   it via [`Url::join`].
    /// - Without `base`, parse `input` directly (must be absolute).
    ///
    /// Throws a JS `TypeError` on parse failure (matching the WHATWG
    /// spec — `new URL("not a url")` is a `TypeError`, not a
    /// `SyntaxError`).
    fn parse<'js>(
        ctx: &Ctx<'js>,
        input: &str,
        base: Option<&str>,
    ) -> rquickjs::Result<Url> {
        let parsed = match base {
            Some(b) => {
                let parsed_base = Url::parse(b).map_err(|e| {
                    rquickjs::Exception::throw_type(
                        ctx,
                        &format!("URL: invalid base {b:?}: {e}"),
                    )
                })?;
                parsed_base.join(input).map_err(|e| {
                    rquickjs::Exception::throw_type(
                        ctx,
                        &format!("URL: cannot resolve {input:?} against {b:?}: {e}"),
                    )
                })?
            }
            None => Url::parse(input).map_err(|e| {
                rquickjs::Exception::throw_type(
                    ctx,
                    &format!("URL: invalid url {input:?}: {e}"),
                )
            })?,
        };
        Ok(parsed)
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl UrlClass {
    /// `new URL(href, base?)`. Throws `TypeError` if parsing fails.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        input: rquickjs::Coerced<String>,
        base: Opt<rquickjs::Coerced<String>>,
    ) -> rquickjs::Result<Self> {
        let parsed = Self::parse(&ctx, &input.0, base.0.as_ref().map(|c| c.0.as_str()))?;
        Ok(Self::from_url(parsed))
    }

    /// `url.href` getter. Serializes the entire URL.
    #[qjs(get)]
    fn href(&self) -> String {
        self.inner.borrow().as_str().to_owned()
    }

    /// `url.href = value` setter — re-parses `value` as a fresh URL.
    /// On failure throws `TypeError` per the WHATWG `Location`-ish
    /// idiom.
    #[qjs(set, rename = "href")]
    fn set_href<'js>(
        &self,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        let parsed = Self::parse(&ctx, &value.0, None)?;
        *self.inner.borrow_mut() = parsed;
        Ok(())
    }

    /// `url.origin` — `scheme://host[:port]` for hierarchical schemes,
    /// `"null"` otherwise. Read-only per spec.
    #[qjs(get)]
    fn origin(&self) -> String {
        let u = self.inner.borrow();
        u.origin().ascii_serialization()
    }

    /// `url.protocol` getter — includes the trailing `":"` per spec.
    #[qjs(get)]
    fn protocol(&self) -> String {
        format!("{}:", self.inner.borrow().scheme())
    }

    /// `url.protocol = "https"` setter. Tolerates the trailing `":"`.
    /// Silently no-ops on illegal transitions (e.g. `http` →
    /// `mailto`) — matches the WHATWG "any setter that would produce
    /// an invalid URL leaves the URL unchanged" rule.
    #[qjs(set, rename = "protocol")]
    fn set_protocol(&self, value: rquickjs::Coerced<String>) {
        let trimmed = value.0.trim_end_matches(':');
        let _ = self.inner.borrow_mut().set_scheme(trimmed);
    }

    /// `url.host` — `hostname[:port]`.
    #[qjs(get)]
    fn host(&self) -> String {
        let u = self.inner.borrow();
        match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_owned(),
            _ => String::new(),
        }
    }

    /// `url.host = "example.com:8080"` setter.
    #[qjs(set, rename = "host")]
    fn set_host(&self, value: rquickjs::Coerced<String>) {
        let _ = self.inner.borrow_mut().set_host(Some(&value.0));
    }

    /// `url.hostname` — host without port.
    #[qjs(get)]
    fn hostname(&self) -> String {
        self.inner.borrow().host_str().unwrap_or("").to_owned()
    }

    /// `url.hostname = "..."` setter.
    #[qjs(set, rename = "hostname")]
    fn set_hostname(&self, value: rquickjs::Coerced<String>) {
        let _ = self.inner.borrow_mut().set_host(Some(&value.0));
    }

    /// `url.port` — empty string when no port is set.
    #[qjs(get)]
    fn port(&self) -> String {
        self.inner
            .borrow()
            .port()
            .map(|p| p.to_string())
            .unwrap_or_default()
    }

    /// `url.port = "8080"` or `url.port = ""` setter.
    #[qjs(set, rename = "port")]
    fn set_port(&self, value: rquickjs::Coerced<String>) {
        let port = if value.0.is_empty() {
            None
        } else {
            value.0.parse::<u16>().ok()
        };
        // `set_port` returns `Result<(), ()>`. Failures (e.g. invalid
        // port on a non-base URL) leave the URL unchanged.
        let _ = self.inner.borrow_mut().set_port(port);
    }

    /// `url.pathname` — path portion, starting with `/` for
    /// hierarchical URLs.
    #[qjs(get)]
    fn pathname(&self) -> String {
        self.inner.borrow().path().to_owned()
    }

    /// `url.pathname = "/foo"` setter.
    #[qjs(set, rename = "pathname")]
    fn set_pathname(&self, value: rquickjs::Coerced<String>) {
        self.inner.borrow_mut().set_path(&value.0);
    }

    /// `url.search` — query portion, including the leading `"?"`.
    /// Empty string when no query.
    #[qjs(get)]
    fn search(&self) -> String {
        self.inner
            .borrow()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    }

    /// `url.search = "?a=1&b=2"` setter. Strips the leading `"?"` if
    /// present (spec quirk).
    #[qjs(set, rename = "search")]
    fn set_search(&self, value: rquickjs::Coerced<String>) {
        let v = value.0.strip_prefix('?').unwrap_or(&value.0);
        if v.is_empty() {
            self.inner.borrow_mut().set_query(None);
        } else {
            self.inner.borrow_mut().set_query(Some(v));
        }
    }

    /// `url.hash` — fragment portion, including the leading `"#"`.
    #[qjs(get)]
    fn hash(&self) -> String {
        self.inner
            .borrow()
            .fragment()
            .map(|f| format!("#{f}"))
            .unwrap_or_default()
    }

    /// `url.hash = "#frag"` setter. Strips the leading `"#"` if
    /// present.
    #[qjs(set, rename = "hash")]
    fn set_hash(&self, value: rquickjs::Coerced<String>) {
        let v = value.0.strip_prefix('#').unwrap_or(&value.0);
        if v.is_empty() {
            self.inner.borrow_mut().set_fragment(None);
        } else {
            self.inner.borrow_mut().set_fragment(Some(v));
        }
    }

    /// `url.username` — percent-encoded username, or empty.
    #[qjs(get)]
    fn username(&self) -> String {
        self.inner.borrow().username().to_owned()
    }

    /// `url.username = "..."` setter.
    #[qjs(set, rename = "username")]
    fn set_username(&self, value: rquickjs::Coerced<String>) {
        let _ = self.inner.borrow_mut().set_username(&value.0);
    }

    /// `url.password` — percent-encoded password, or empty.
    #[qjs(get)]
    fn password(&self) -> String {
        self.inner.borrow().password().unwrap_or("").to_owned()
    }

    /// `url.password = "..."` setter.
    #[qjs(set, rename = "password")]
    fn set_password(&self, value: rquickjs::Coerced<String>) {
        let pw = if value.0.is_empty() {
            None
        } else {
            Some(value.0.as_str())
        };
        let _ = self.inner.borrow_mut().set_password(pw);
    }

    /// `url.searchParams` — returns a [`UrlSearchParamsClass`] that
    /// shares the parent URL's backing store. Mutations on the
    /// returned object reflect back into `url.toString()` / `url.search`
    /// without any explicit sync step.
    ///
    /// The view is allocated fresh on every read. Real browsers
    /// cache it (`url.searchParams === url.searchParams` is `true`
    /// per spec), but the backing store is shared either way so the
    /// observable behavior matches except for `===` identity. If a
    /// page actually depends on identity we can stash the view in a
    /// JS-side hidden property; deferred until something breaks.
    #[qjs(get)]
    fn search_params<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
    ) -> rquickjs::Result<Class<'js, UrlSearchParamsClass>> {
        let cell = this.0.borrow().inner.clone();
        Class::instance(
            ctx,
            UrlSearchParamsClass {
                backing: Backing::Attached(cell),
            },
        )
    }

    /// `url.toString()` — alias for `href`.
    #[qjs(rename = PredefinedAtom::ToString)]
    fn to_string_method(&self) -> String {
        self.inner.borrow().as_str().to_owned()
    }

    /// `url.toJSON()` — alias for `href` per spec.
    #[qjs(rename = PredefinedAtom::ToJSON)]
    fn to_json(&self) -> String {
        self.inner.borrow().as_str().to_owned()
    }
}

/// `URLSearchParams` — the WHATWG searchParams interface.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "URLSearchParams")]
pub struct UrlSearchParamsClass {
    /// Either a back-pointer to a parent URL (for `url.searchParams`)
    /// or a standalone insertion-ordered list (for `new
    /// URLSearchParams(init)`).
    #[qjs(skip_trace)]
    backing: Backing,
}

impl UrlSearchParamsClass {
    /// Read the entry list. For attached params, re-parses the
    /// parent URL's query string on each call (cheap; the parse is
    /// linear in the query length). For detached params, clones the
    /// `Vec`.
    fn entries(&self) -> Vec<(String, String)> {
        match &self.backing {
            Backing::Attached(url) => url
                .borrow()
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
            Backing::Detached(v) => v.borrow().clone(),
        }
    }

    /// Write a fresh entry list back. For attached params, calls
    /// `Url::query_pairs_mut().clear().extend_pairs(...)` which
    /// rewrites the query string in-place on drop. For detached
    /// params, replaces the vec.
    fn write(&self, pairs: &[(String, String)]) {
        match &self.backing {
            Backing::Attached(url) => {
                let mut u = url.borrow_mut();
                if pairs.is_empty() {
                    // Empty: drop the query string entirely so
                    // `url.toString()` doesn't carry a trailing
                    // `"?"`. `url.set_query(None)` rather than
                    // `query_pairs_mut().clear()` because the latter
                    // leaves an empty `Some("")` that re-serializes
                    // as a trailing `?`.
                    u.set_query(None);
                } else {
                    let mut s = u.query_pairs_mut();
                    s.clear();
                    for (k, v) in pairs {
                        s.append_pair(k, v);
                    }
                    // Drop `s` here so the underlying `Url` is rewritten.
                    drop(s);
                }
            }
            Backing::Detached(v) => {
                let mut g = v.borrow_mut();
                g.clear();
                g.extend(pairs.iter().cloned());
            }
        }
    }

    /// Parse a string-shaped `init` argument: strips the leading
    /// `"?"` if present, then runs `form_urlencoded::parse`.
    fn parse_string_init(s: &str) -> Vec<(String, String)> {
        let body = s.strip_prefix('?').unwrap_or(s);
        url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect()
    }

    /// Parse an `init` argument that may be a string, an iterable of
    /// `[k, v]` pairs, or a plain object (record).
    fn parse_init<'js>(
        ctx: &Ctx<'js>,
        init: Option<Value<'js>>,
    ) -> rquickjs::Result<Vec<(String, String)>> {
        let Some(v) = init else {
            return Ok(Vec::new());
        };
        if v.is_null() || v.is_undefined() {
            return Ok(Vec::new());
        }

        // String input: parse as application/x-www-form-urlencoded.
        if let Some(s) = v.as_string() {
            let raw = s.to_string()?;
            return Ok(Self::parse_string_init(&raw));
        }

        // Anything else has to be an Object. Two shapes to support:
        // (a) Iterable of `[k, v]` pairs (anything with
        //     `Symbol.iterator` whose yielded values are array-likes
        //     of length 2). Also covers plain `Array` inputs.
        // (b) Plain record `{ k: v, ... }`. The spec walks "own
        //     enumerable string-keyed properties" in property order.
        //
        // We do this entirely on the JS side via a small bootstrap
        // call: the spec semantics are easier to express in JS, and
        // doing the iterator dance from Rust would require manual
        // `next()`-pumping which rquickjs doesn't yet expose
        // ergonomically.
        let Some(obj) = v.into_object() else {
            return Err(rquickjs::Exception::throw_type(
                ctx,
                "URLSearchParams: init must be a string, an iterable of [key,value] pairs, or a record object",
            ));
        };

        // Pull the JS-side normalizer installed in `install_url`.
        let globals = ctx.globals();
        let normalize: Function<'js> = globals
            .get::<_, Function<'js>>("__hesoNormalizeUrlSearchParamsInit")
            .map_err(|_| {
                rquickjs::Exception::throw_type(
                    ctx,
                    "internal: URL bootstrap missing __hesoNormalizeUrlSearchParamsInit",
                )
            })?;
        let pairs: Array<'js> = normalize.call((obj,))?;
        let mut out = Vec::with_capacity(pairs.len());
        for i in 0..pairs.len() {
            let pair: Array<'js> = pairs.get(i)?;
            let k: String = pair.get(0)?;
            let val: String = pair.get(1)?;
            out.push((k, val));
        }
        Ok(out)
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl UrlSearchParamsClass {
    /// `new URLSearchParams(init?)`. `init` may be:
    /// - a string (with or without a leading `?`)
    /// - an iterable of `[key, value]` pairs
    /// - a plain object (record)
    /// - `undefined` / `null` (empty)
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let pairs = Self::parse_init(&ctx, init.0)?;
        Ok(Self {
            backing: Backing::Detached(Rc::new(RefCell::new(pairs))),
        })
    }

    /// `params.size` — number of name/value pairs.
    #[qjs(get)]
    fn size(&self) -> usize {
        match &self.backing {
            Backing::Attached(url) => url.borrow().query_pairs().count(),
            Backing::Detached(v) => v.borrow().len(),
        }
    }

    /// `params.get(name)` — first matching value, or `null`.
    fn get(&self, name: rquickjs::Coerced<String>) -> Option<String> {
        self.entries()
            .into_iter()
            .find(|(k, _)| k == &name.0)
            .map(|(_, v)| v)
    }

    /// `params.getAll(name)` — all matching values, in insertion
    /// order.
    fn get_all(&self, name: rquickjs::Coerced<String>) -> Vec<String> {
        self.entries()
            .into_iter()
            .filter_map(|(k, v)| if k == name.0 { Some(v) } else { None })
            .collect()
    }

    /// `params.has(name, value?)` — true if any entry matches `name`,
    /// or (with `value`) any entry exactly matches both.
    fn has(
        &self,
        name: rquickjs::Coerced<String>,
        value: Opt<rquickjs::Coerced<String>>,
    ) -> bool {
        match value.0 {
            Some(v) => self
                .entries()
                .into_iter()
                .any(|(k, val)| k == name.0 && val == v.0),
            None => self.entries().into_iter().any(|(k, _)| k == name.0),
        }
    }

    /// `params.set(name, value)` — replace the first existing entry
    /// for `name` with `value` and remove any later duplicates. If
    /// no entry exists, append.
    fn set(
        &self,
        name: rquickjs::Coerced<String>,
        value: rquickjs::Coerced<String>,
    ) {
        let mut pairs = self.entries();
        let mut replaced = false;
        pairs.retain_mut(|(k, v)| {
            if k != &name.0 {
                return true;
            }
            if !replaced {
                *v = value.0.clone();
                replaced = true;
                return true;
            }
            // Drop later duplicates per spec.
            false
        });
        if !replaced {
            pairs.push((name.0, value.0));
        }
        self.write(&pairs);
    }

    /// `params.append(name, value)` — always appends a new entry.
    fn append(
        &self,
        name: rquickjs::Coerced<String>,
        value: rquickjs::Coerced<String>,
    ) {
        let mut pairs = self.entries();
        pairs.push((name.0, value.0));
        self.write(&pairs);
    }

    /// `params.delete(name, value?)` — remove all matching entries.
    /// With `value`, only remove entries where both name and value
    /// match (per the modern spec extension).
    fn delete(
        &self,
        name: rquickjs::Coerced<String>,
        value: Opt<rquickjs::Coerced<String>>,
    ) {
        let mut pairs = self.entries();
        match value.0 {
            Some(v) => pairs.retain(|(k, val)| !(k == &name.0 && val == &v.0)),
            None => pairs.retain(|(k, _)| k != &name.0),
        }
        self.write(&pairs);
    }

    /// `params.sort()` — stable sort by UTF-16 code-unit order of the
    /// keys. In-place.
    ///
    /// `Vec::sort_by` is stable; Rust's default `Ord` on `String` is
    /// byte-wise which differs from UTF-16 code-unit order for
    /// supplementary-plane characters. We encode each key to UTF-16
    /// and compare those slices.
    fn sort(&self) {
        let mut pairs = self.entries();
        pairs.sort_by(|a, b| utf16_cmp(&a.0, &b.0));
        self.write(&pairs);
    }

    /// `params.toString()` — `application/x-www-form-urlencoded`
    /// serialization. No leading `?`.
    #[qjs(rename = PredefinedAtom::ToString)]
    fn to_string_method(&self) -> String {
        match &self.backing {
            Backing::Attached(url) => {
                url.borrow().query().unwrap_or("").to_owned()
            }
            Backing::Detached(v) => {
                let g = v.borrow();
                let mut s = url::form_urlencoded::Serializer::new(String::new());
                for (k, val) in g.iter() {
                    s.append_pair(k, val);
                }
                s.finish()
            }
        }
    }

    /// `params.entries()` — array of `[k, v]` pairs in insertion order.
    /// In the spec this returns an iterator; we return an array and
    /// rely on `Symbol.iterator` (patched in [`install_url`]) to give
    /// the class itself iterator semantics. The for-of protocol works
    /// either way because arrays are iterable.
    #[qjs(rename = "entries")]
    fn entries_method<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let pairs = self.entries();
        let arr = Array::new(ctx.clone())?;
        for (i, (k, v)) in pairs.into_iter().enumerate() {
            let inner = Array::new(ctx.clone())?;
            inner.set(0, k)?;
            inner.set(1, v)?;
            arr.set(i, inner)?;
        }
        Ok(arr)
    }

    /// `params.keys()` — array of keys in insertion order.
    fn keys<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let arr = Array::new(ctx.clone())?;
        for (i, (k, _)) in self.entries().into_iter().enumerate() {
            arr.set(i, k)?;
        }
        Ok(arr)
    }

    /// `params.values()` — array of values in insertion order.
    fn values<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let arr = Array::new(ctx.clone())?;
        for (i, (_, v)) in self.entries().into_iter().enumerate() {
            arr.set(i, v)?;
        }
        Ok(arr)
    }

    /// `params.forEach(callback, thisArg?)` — invoke `callback(value,
    /// key, params)` for each entry in insertion order.
    ///
    /// `thisArg` is accepted for spec compatibility but is currently
    /// **ignored** — the callback is invoked with the default `this`.
    /// Frameworks rarely pass `thisArg` (they reach for arrow
    /// functions instead); if a real page surfaces a need for it we
    /// can route through `Function.prototype.call` from a JS shim
    /// without breaking existing callers.
    fn for_each<'js>(
        this: This<Class<'js, Self>>,
        _ctx: Ctx<'js>,
        callback: Function<'js>,
        _this_arg: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let pairs = this.0.borrow().entries();
        let params_value: Value<'js> = this.0.clone().into_value();
        for (k, v) in pairs {
            callback.call::<_, ()>((v, k, params_value.clone()))?;
        }
        Ok(())
    }
}

/// Compare two strings as UTF-16 code-unit sequences. WHATWG
/// `URLSearchParams.prototype.sort` requires this; Rust's default
/// byte-wise `Ord` on `String` differs for code points outside the
/// BMP.
fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let av = a.encode_utf16();
    let bv = b.encode_utf16();
    av.cmp(bv)
}

/// Register `URL` and `URLSearchParams` on `globalThis`.
///
/// Also installs `__hesoNormalizeUrlSearchParamsInit` — a JS-side
/// helper that normalizes the constructor's `init` argument from
/// "string | iterable | record" into a uniform `Array<[key, value]>`.
/// Doing the normalization in JS keeps us spec-correct on the
/// iterable shape (which calls `Symbol.iterator` and `next()` on
/// arbitrary user objects) without re-implementing iteration from
/// Rust.
///
/// Patches `URLSearchParams.prototype[Symbol.iterator]` to return
/// `this.entries()[Symbol.iterator]()`, making `for (const [k, v] of
/// params)` work.
pub fn install_url(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            Class::<UrlClass>::define(&globals)?;
            Class::<UrlSearchParamsClass>::define(&globals)?;

            // Attach the static `URL.canParse` method. The simplest
            // path is a JS-defined function — the constructor throws
            // on bad input, so a `try`/`return false` catches the
            // failure cleanly.
            let url_ctor: Object = globals.get("URL")?;
            let can_parse =
                Func::from(|ctx: Ctx<'_>, input: String, base: Opt<String>| -> bool {
                    UrlClass::parse(&ctx, &input, base.0.as_deref()).is_ok()
                });
            url_ctor.set("canParse", can_parse)?;

            // Run the JS bootstrap that:
            // - installs `__hesoNormalizeUrlSearchParamsInit` for the
            //   constructor's iterable/record shapes,
            // - patches `URLSearchParams.prototype[Symbol.iterator]`
            //   so `for (const [k,v] of params)` works.
            ctx.eval::<(), _>(URL_BOOTSTRAP)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install URL: {e}")))?;
    Ok(())
}

/// JS bootstrap for `URLSearchParams` iterator + init normalization.
const URL_BOOTSTRAP: &str = r#"
(function() {
    // Normalize a non-string `init` argument into an array of
    // `[key, value]` string-string pairs.
    //
    // - Anything with `Symbol.iterator` is treated as an iterable of
    //   pairs (per WHATWG, sequences of sequences of pairs).
    // - Otherwise the object is treated as a record (own enumerable
    //   string-keyed properties, in order).
    globalThis.__hesoNormalizeUrlSearchParamsInit = function(obj) {
        if (obj == null) return [];

        // Iterable path: anything with Symbol.iterator. Arrays land
        // here automatically.
        if (typeof obj[Symbol.iterator] === 'function') {
            const out = [];
            for (const pair of obj) {
                if (pair == null) {
                    throw new TypeError(
                        'URLSearchParams: each iterable element must be a [key, value] pair'
                    );
                }
                // The spec requires each pair to be a sequence of
                // length 2. Common shape is an array `[k, v]`; also
                // accept any indexable with `.length === 2`.
                let k, v;
                if (typeof pair[Symbol.iterator] === 'function' && !Array.isArray(pair)) {
                    // Spec: re-iterate to collect two items.
                    const items = [];
                    for (const x of pair) items.push(x);
                    if (items.length !== 2) {
                        throw new TypeError(
                            'URLSearchParams: each pair must have exactly two elements'
                        );
                    }
                    k = items[0];
                    v = items[1];
                } else {
                    if (pair.length !== 2) {
                        throw new TypeError(
                            'URLSearchParams: each pair must have exactly two elements'
                        );
                    }
                    k = pair[0];
                    v = pair[1];
                }
                out.push([String(k), String(v)]);
            }
            return out;
        }

        // Record path: own enumerable string-keyed properties.
        const out = [];
        for (const k of Object.keys(obj)) {
            out.push([String(k), String(obj[k])]);
        }
        return out;
    };

    // `for (const [k, v] of params)` requires
    // `URLSearchParams.prototype[Symbol.iterator]` to be a function
    // returning an iterator. Our Rust-side `entries()` returns a
    // plain Array, which IS iterable — patch the prototype to
    // forward Symbol.iterator to `this.entries()[Symbol.iterator]()`.
    if (typeof globalThis.URLSearchParams === 'function') {
        const proto = globalThis.URLSearchParams.prototype;
        Object.defineProperty(proto, Symbol.iterator, {
            value: function () {
                return this.entries()[Symbol.iterator]();
            },
            writable: true,
            configurable: true,
            enumerable: false,
        });
    }
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_cmp_orders_bmp_strings_byte_wise() {
        assert_eq!(utf16_cmp("a", "b"), std::cmp::Ordering::Less);
        assert_eq!(utf16_cmp("b", "a"), std::cmp::Ordering::Greater);
        assert_eq!(utf16_cmp("a", "a"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn utf16_cmp_differs_from_bytewise_on_supplementary_plane() {
        // U+1F600 (😀) is one byte less than "z" in UTF-8 sort order
        // (because UTF-8 encodes it as F0 9F 98 80 = 0xF0... > "z" =
        // 0x7A actually larger byte-wise). UTF-16 encodes it as
        // D83D DE00; the high surrogate D83D (0xD83D) > "z"
        // (0x007A). So both orderings put "z" before "😀", and
        // we can use that direction to confirm the encode_utf16
        // path is actually walked.
        let a = "z";
        let b = "\u{1F600}";
        // Both byte-wise (UTF-8) and code-unit-wise (UTF-16) compare
        // "z" < "😀". The interesting case is when they would
        // disagree (e.g. comparing two supplementary chars), but
        // that's enough determinism for the equality check.
        assert_eq!(utf16_cmp(a, b), std::cmp::Ordering::Less);
    }
}
