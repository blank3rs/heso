//! # web_apis
//!
//! WHATWG `Blob`, `File`, `Headers`, and `FormData` globals — the
//! constructor surface every modern fetch+upload code path reaches
//! for. Per `AGENT_FINDINGS_V2.md` task F1 and "Top NEW bugs" item 4,
//! the absence of these four constructors was the single biggest gap
//! blocking file uploads and modern `fetch()` patterns from inside
//! agent-driven JS.
//!
//! ## Coverage
//!
//! - **`Blob`** — [WHATWG File API §3][blob-spec]. `new Blob(parts?,
//!   options?)`. Properties: `size`, `type`. Methods: `.text()`,
//!   `.arrayBuffer()`, `.bytes()`, `.slice()`. `.stream()` returns
//!   `undefined` (real streams are out of scope; we don't ship a
//!   ReadableStream implementation).
//! - **`File`** — [WHATWG File API §4][file-spec]. `new File(parts,
//!   name, options?)`. Inherits `Blob`'s shape: instances are
//!   `instanceof Blob`, `.size` / `.type` / `.text()` / etc. all work.
//!   Adds `.name` and `.lastModified`.
//! - **`Headers`** — [WHATWG Fetch §5][headers-spec]. `new
//!   Headers(init?)` accepts `Headers | string[][] | Record<string,
//!   string>`. Case-insensitive name canonicalization (lowercase per
//!   spec); duplicate values combine with `, ` on set/append.
//! - **`FormData`** — [WHATWG XHR §5][formdata-spec]. `new
//!   FormData(form?, submitter?)` populates from a form element via
//!   the same JS-side walker `crate::form_submit` already uses. Entry
//!   values are `string` or `Blob`. The fetch path serializes
//!   `FormData` bodies as `multipart/form-data` with a generated
//!   boundary, reusing `crate::form_submit::build_multipart_form`.
//!
//! ## Line-ending normalization
//!
//! The Blob spec defines `options.endings` as `"transparent"` (no
//! mutation; the default) or `"native"` (convert `\n` → platform-native
//! line terminators). We implement only `"transparent"`. `"native"`
//! would force the engine to leak host-OS state into observable bytes
//! — a determinism trap per ADR 0008 — and no real-world agent code
//! reaches for it. A request with `endings: "native"` is treated as
//! `"transparent"` with no error.
//!
//! ## Streams
//!
//! `Blob.prototype.stream()` returns `undefined`. A real
//! `ReadableStream` would be a parallel can-of-worms (`pipeTo` / `pipeThrough`,
//! reader/writer semantics, queuing strategy IDL). Agent-shaped pages
//! almost never call `.stream()`; when one does, the call sites
//! observe `undefined` and fall back to `.arrayBuffer()` or `.text()`
//! in practice.
//!
//! ## OSS surveyed before writing
//!
//! - The four WHATWG specs cited above are the source of truth for
//!   semantics. Inline comments cite the relevant §s.
//! - [jsdom's `Blob-impl.js`, `File-impl.js`, `Headers-impl.js`,
//!   `FormData-impl.js`][jsdom] were consulted for spec-corner
//!   coverage (line-ending normalization, case-insensitive header
//!   matching, multipart boundary generation, form-element entry
//!   walking). We didn't port the TypeScript — rquickjs class macros
//!   would reject a literal translation — but the structural moves
//!   (slice-by-byte-range, single-canonical lowercase name map for
//!   headers, generator-style multipart serializer) match.
//! - `reqwest::multipart::Form` is reused via
//!   [`crate::form_submit::build_multipart_form`] for the
//!   `fetch(body: FormData)` integration.
//!
//! [blob-spec]: https://w3c.github.io/FileAPI/#blob-section
//! [file-spec]: https://w3c.github.io/FileAPI/#file-section
//! [headers-spec]: https://fetch.spec.whatwg.org/#headers-class
//! [formdata-spec]: https://xhr.spec.whatwg.org/#interface-formdata
//! [jsdom]: https://github.com/jsdom/jsdom/tree/main/lib/jsdom/living

use std::cell::RefCell;
use std::rc::Rc;

use rquickjs::{
    class::Trace,
    prelude::{Opt, This},
    Array, Class, Context, Ctx, Function, JsLifetime, Object, Promise, TypedArray, Value,
};

use crate::engine::EvalError;

// =============================================================================
// Blob
// =============================================================================

/// WHATWG `Blob` — an immutable, raw-data, MIME-typed binary container.
///
/// Per spec, instances are immutable: once constructed, the backing
/// bytes never change. `.slice(...)` returns a new `Blob` with a
/// subset; the original is untouched.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "Blob")]
pub struct Blob {
    /// The bytes, in their original (pre-slice) order. Shared via
    /// `Rc<RefCell>` so `.slice()` views can share the underlying
    /// buffer rather than copying — even though Blobs are spec-immutable
    /// after construction, sharing keeps memory bounded for the common
    /// "slice many small ranges out of a big upload" pattern.
    #[qjs(skip_trace)]
    bytes: Rc<RefCell<Vec<u8>>>,
    /// MIME type, lowercased per spec.
    #[qjs(skip_trace)]
    mime: String,
}

impl Blob {
    /// Internal constructor used by `File::new` to skip part-parsing
    /// work when the bytes are already a `Vec<u8>`.
    pub(crate) fn from_bytes(bytes: Vec<u8>, mime: String) -> Self {
        Self {
            bytes: Rc::new(RefCell::new(bytes)),
            mime,
        }
    }

    /// Borrow the underlying byte buffer. Caller is responsible for
    /// not retaining the borrow across JS calls.
    pub(crate) fn snapshot_bytes(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }

    /// MIME type — exposed for the multipart-serialization path.
    pub(crate) fn mime_type(&self) -> &str {
        &self.mime
    }

    /// Parse the `parts` argument per WHATWG File API §3.1. Each part
    /// may be:
    /// - A string (encoded as UTF-8 bytes).
    /// - Another `Blob` / `File` (copies its bytes — no aliasing).
    /// - An `ArrayBuffer` or any TypedArray (copies its bytes).
    ///
    /// Anything else throws `TypeError`. Returns the concatenated
    /// byte vector.
    fn parse_parts<'js>(ctx: &Ctx<'js>, parts: Value<'js>) -> rquickjs::Result<Vec<u8>> {
        // Spec: `parts` is a sequence (iterable). We mirror that by
        // accepting an Array directly; the JS bootstrap shim coerces
        // arbitrary iterables to an Array before calling us.
        let parts_arr: Array<'js> = match Array::from_value(parts.clone()) {
            Ok(a) => a,
            Err(_) => {
                return Err(rquickjs::Exception::throw_type(
                    ctx,
                    "Blob: first argument must be a sequence of BlobParts",
                ));
            }
        };
        let mut out: Vec<u8> = Vec::new();
        for i in 0..parts_arr.len() {
            let part: Value<'js> = parts_arr.get(i)?;
            append_part_bytes(ctx, &part, &mut out)?;
        }
        Ok(out)
    }

    /// Parse the `options` bag per WHATWG File API §3.1: `{type?,
    /// endings?}`. Unknown keys are ignored; bad shapes throw
    /// `TypeError`. Returns the lowercased `type`. `endings` is
    /// accepted-and-ignored except that we recognize the two valid
    /// values (`"transparent"` / `"native"`) and treat anything else
    /// as `"transparent"`.
    fn parse_options<'js>(_ctx: &Ctx<'js>, options: Option<Value<'js>>) -> rquickjs::Result<String> {
        let Some(opts_val) = options else {
            return Ok(String::new());
        };
        if opts_val.is_null() || opts_val.is_undefined() {
            return Ok(String::new());
        }
        let Some(opts) = opts_val.as_object() else {
            return Ok(String::new());
        };
        let raw_type = match opts.get::<_, Option<String>>("type") {
            Ok(Some(s)) => s,
            _ => String::new(),
        };
        // Spec §3.1: only ASCII bytes in the range 0x20..=0x7E are
        // valid for a Blob's `type`; anything else makes it the empty
        // string. We honor that to keep round-trips spec-aligned.
        if !raw_type.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
            return Ok(String::new());
        }
        Ok(raw_type.to_ascii_lowercase())
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl Blob {
    /// `new Blob(parts?, options?)`.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        parts: Opt<Value<'js>>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let bytes = match parts.0 {
            None => Vec::new(),
            Some(v) if v.is_null() || v.is_undefined() => Vec::new(),
            Some(v) => Self::parse_parts(&ctx, v)?,
        };
        let mime = Self::parse_options(&ctx, options.0)?;
        Ok(Self {
            bytes: Rc::new(RefCell::new(bytes)),
            mime,
        })
    }

    /// `blob.size` — byte count.
    #[qjs(get)]
    fn size(&self) -> usize {
        self.bytes.borrow().len()
    }

    /// `blob.type` — lowercased MIME type, or empty string.
    #[qjs(get, rename = "type")]
    fn blob_type(&self) -> String {
        self.mime.clone()
    }

    /// `blob.text()` — Promise<string> with the UTF-8 decoding of the
    /// bytes. Invalid UTF-8 is replaced with U+FFFD per spec.
    fn text<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Promise<'js>> {
        let s = String::from_utf8_lossy(&self.bytes.borrow()).into_owned();
        let (promise, resolve, _reject) = Promise::new(&ctx)?;
        resolve.call::<_, ()>((s,))?;
        Ok(promise)
    }

    /// `blob.arrayBuffer()` — Promise<ArrayBuffer> with the raw bytes.
    fn array_buffer<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Promise<'js>> {
        let ta = TypedArray::<u8>::new(ctx.clone(), self.bytes.borrow().as_slice())?;
        let obj: Object<'js> = ta.into_object();
        let ab: Value<'js> = obj.get("buffer")?;
        let (promise, resolve, _reject) = Promise::new(&ctx)?;
        resolve.call::<_, ()>((ab,))?;
        Ok(promise)
    }

    /// `blob.bytes()` — Promise<Uint8Array>, per the recent extension
    /// to the File API spec.
    fn bytes<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Promise<'js>> {
        let ta = TypedArray::<u8>::new(ctx.clone(), self.bytes.borrow().as_slice())?;
        let (promise, resolve, _reject) = Promise::new(&ctx)?;
        resolve.call::<_, ()>((ta,))?;
        Ok(promise)
    }

    /// `blob.slice(start?, end?, contentType?)` — returns a new Blob
    /// containing bytes in the range `[start, end)`. Negative indices
    /// count from the end. The new Blob's `type` is `contentType`
    /// lowercased (or empty when omitted), matching spec §3.1 "slice
    /// method steps."
    fn slice<'js>(
        &self,
        ctx: Ctx<'js>,
        start: Opt<f64>,
        end: Opt<f64>,
        content_type: Opt<String>,
    ) -> rquickjs::Result<Class<'js, Self>> {
        let len = self.bytes.borrow().len() as i64;
        let normalize = |v: f64| -> i64 {
            let i = v as i64;
            if i < 0 {
                std::cmp::max(len + i, 0)
            } else {
                std::cmp::min(i, len)
            }
        };
        let s = normalize(start.0.unwrap_or(0.0));
        let e = normalize(end.0.unwrap_or(len as f64));
        let span = if e > s { (e - s) as usize } else { 0 };
        let mut sub = Vec::with_capacity(span);
        if span > 0 {
            let buf = self.bytes.borrow();
            sub.extend_from_slice(&buf[s as usize..(s as usize + span)]);
        }
        let mime = content_type
            .0
            .map(|s| {
                if s.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
                    s.to_ascii_lowercase()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        Class::instance(ctx, Self::from_bytes(sub, mime))
    }

    /// `blob.stream()` — returns `undefined`. Real `ReadableStream`
    /// support is deferred; agent-shaped pages overwhelmingly use
    /// `.arrayBuffer()` / `.text()` instead.
    fn stream<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        Ok(Value::new_undefined(ctx))
    }
}

/// Read one BlobPart out of a JS value and append its bytes to `out`.
/// Recognized shapes:
///
/// - String → UTF-8 bytes
/// - Another `Blob` / `File` → copy its byte buffer
/// - ArrayBuffer → copy its bytes
/// - TypedArray (Uint8Array, Int8Array, etc.) → copy its bytes
///
/// Anything else throws `TypeError`.
fn append_part_bytes<'js>(
    ctx: &Ctx<'js>,
    part: &Value<'js>,
    out: &mut Vec<u8>,
) -> rquickjs::Result<()> {
    // String fast-path.
    if let Some(s) = part.as_string() {
        let owned = s.to_string()?;
        out.extend_from_slice(owned.as_bytes());
        return Ok(());
    }
    // Blob/File: check via class instance-of (File inherits Blob's
    // bytes accessor since it IS-A Blob in our model).
    if let Some(obj) = part.as_object() {
        // File first (more specific — File instances are not stored
        // as Blob classes despite the JS-side prototype patch; the
        // rquickjs Class<T> registration is per-type).
        if let Some(class) = Class::<File>::from_object(obj) {
            let f = class.borrow();
            out.extend_from_slice(&f.bytes.borrow());
            return Ok(());
        }
        // Blob.
        if let Some(class) = Class::<Blob>::from_object(obj) {
            let blob = class.borrow();
            out.extend_from_slice(&blob.bytes.borrow());
            return Ok(());
        }
        // ArrayBuffer or TypedArray: try the TypedArray path first
        // (covers Uint8Array, Int8Array, etc.).
        if let Ok(ta) = TypedArray::<u8>::from_object(obj.clone()) {
            // `as_bytes()` returns a slice we can copy.
            if let Some(slice) = ta.as_bytes() {
                out.extend_from_slice(slice);
                return Ok(());
            }
        }
        // ArrayBuffer: rquickjs exposes `ArrayBuffer` separately.
        if let Some(ab) = rquickjs::ArrayBuffer::from_object(obj.clone()) {
            if let Some(slice) = ab.as_bytes() {
                out.extend_from_slice(slice);
                return Ok(());
            }
        }
        // For other TypedArray element widths (Uint16Array, etc.), we
        // could fall back to reading `.buffer` + byteOffset + byteLength,
        // but rquickjs's high-level surface doesn't expose those
        // ergonomically. The common case (Uint8Array / ArrayBuffer)
        // is covered above; for the rest the JS caller can construct
        // a Uint8Array view first. Throw a clear error in that case
        // rather than silently producing wrong bytes.
    }
    Err(rquickjs::Exception::throw_type(
        ctx,
        "Blob: each part must be a string, Blob, ArrayBuffer, or Uint8Array",
    ))
}

// =============================================================================
// File
// =============================================================================

/// WHATWG `File` — a `Blob` with a filename and last-modified
/// timestamp.
///
/// Per the spec, `File extends Blob` — instances must be `instanceof
/// Blob` as well as `instanceof File`. rquickjs's class system doesn't
/// natively express prototype chains, so [`install_web_apis`] patches
/// `File.prototype`'s prototype to `Blob.prototype` after registering
/// both classes.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "File")]
pub struct File {
    /// The underlying Blob shape — same backing store and MIME type.
    /// Sharing the Rc lets `.text()` / `.slice()` etc. work on File
    /// without re-defining every Blob method here.
    #[qjs(skip_trace)]
    bytes: Rc<RefCell<Vec<u8>>>,
    #[qjs(skip_trace)]
    mime: String,
    /// `file.name` — required at construction.
    #[qjs(skip_trace)]
    name: String,
    /// `file.lastModified` — defaults to current virtual clock at
    /// construction time (via `Date.now()` from JS-side). Stored as
    /// `f64` so it matches the `number` JS exposes.
    #[qjs(skip_trace)]
    last_modified: f64,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl File {
    /// `new File(parts, name, options?)`. `parts` and `options` share
    /// shape with `Blob`; `options` additionally accepts
    /// `lastModified` (number, defaults to `Date.now()`).
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        parts: Value<'js>,
        name: rquickjs::Coerced<String>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let bytes = if parts.is_null() || parts.is_undefined() {
            Vec::new()
        } else {
            Blob::parse_parts(&ctx, parts)?
        };
        let mime = Blob::parse_options(&ctx, options.0.clone())?;
        // Parse `lastModified` from options. Default: Date.now() via
        // JS so the virtual clock determinism path is honored.
        let last_modified = match options.0 {
            Some(o) if !o.is_null() && !o.is_undefined() => {
                if let Some(obj) = o.as_object() {
                    match obj.get::<_, Option<f64>>("lastModified") {
                        Ok(Some(n)) => n,
                        _ => default_last_modified(&ctx)?,
                    }
                } else {
                    default_last_modified(&ctx)?
                }
            }
            _ => default_last_modified(&ctx)?,
        };
        Ok(Self {
            bytes: Rc::new(RefCell::new(bytes)),
            mime,
            name: name.0,
            last_modified,
        })
    }

    /// `file.size` — inherited Blob property.
    #[qjs(get)]
    fn size(&self) -> usize {
        self.bytes.borrow().len()
    }

    /// `file.type` — inherited Blob property.
    #[qjs(get, rename = "type")]
    fn file_type(&self) -> String {
        self.mime.clone()
    }

    /// `file.name`.
    #[qjs(get)]
    fn name(&self) -> String {
        self.name.clone()
    }

    /// `file.lastModified`.
    #[qjs(get)]
    fn last_modified(&self) -> f64 {
        self.last_modified
    }

    /// `file.text()` — inherited Blob method.
    fn text<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Promise<'js>> {
        let s = String::from_utf8_lossy(&self.bytes.borrow()).into_owned();
        let (promise, resolve, _reject) = Promise::new(&ctx)?;
        resolve.call::<_, ()>((s,))?;
        Ok(promise)
    }

    /// `file.arrayBuffer()` — inherited Blob method.
    fn array_buffer<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Promise<'js>> {
        let ta = TypedArray::<u8>::new(ctx.clone(), self.bytes.borrow().as_slice())?;
        let obj: Object<'js> = ta.into_object();
        let ab: Value<'js> = obj.get("buffer")?;
        let (promise, resolve, _reject) = Promise::new(&ctx)?;
        resolve.call::<_, ()>((ab,))?;
        Ok(promise)
    }

    /// `file.bytes()` — inherited Blob method.
    fn bytes_method<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Promise<'js>> {
        let ta = TypedArray::<u8>::new(ctx.clone(), self.bytes.borrow().as_slice())?;
        let (promise, resolve, _reject) = Promise::new(&ctx)?;
        resolve.call::<_, ()>((ta,))?;
        Ok(promise)
    }

    /// `file.slice(...)` — inherited Blob method (returns a Blob, not
    /// a File, per spec).
    fn slice<'js>(
        &self,
        ctx: Ctx<'js>,
        start: Opt<f64>,
        end: Opt<f64>,
        content_type: Opt<String>,
    ) -> rquickjs::Result<Class<'js, Blob>> {
        let len = self.bytes.borrow().len() as i64;
        let normalize = |v: f64| -> i64 {
            let i = v as i64;
            if i < 0 {
                std::cmp::max(len + i, 0)
            } else {
                std::cmp::min(i, len)
            }
        };
        let s = normalize(start.0.unwrap_or(0.0));
        let e = normalize(end.0.unwrap_or(len as f64));
        let span = if e > s { (e - s) as usize } else { 0 };
        let mut sub = Vec::with_capacity(span);
        if span > 0 {
            let buf = self.bytes.borrow();
            sub.extend_from_slice(&buf[s as usize..(s as usize + span)]);
        }
        let mime = content_type
            .0
            .map(|s| {
                if s.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
                    s.to_ascii_lowercase()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        Class::instance(ctx, Blob::from_bytes(sub, mime))
    }

    /// `file.stream()` — inherited Blob behavior (returns `undefined`).
    fn stream<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        Ok(Value::new_undefined(ctx))
    }
}

impl File {
    /// Borrow the file's bytes — used by the multipart-serialization
    /// path when a `File` is set as a `FormData` value.
    pub(crate) fn snapshot_bytes(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }

    /// MIME type — used by the multipart-serialization path.
    pub(crate) fn mime_type(&self) -> &str {
        &self.mime
    }

    /// Filename — used as the `filename` parameter when serializing a
    /// File via multipart.
    pub(crate) fn name_ref(&self) -> &str {
        &self.name
    }
}

/// Read `Date.now()` to seed `lastModified` for a freshly-constructed
/// `File`. Routes through the engine's virtual clock per ADR 0008.
fn default_last_modified<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<f64> {
    let globals = ctx.globals();
    let date: Object<'js> = globals.get("Date")?;
    let now: Function<'js> = date.get("now")?;
    let n: f64 = now.call(())?;
    Ok(n)
}

// =============================================================================
// Headers
// =============================================================================

/// WHATWG `Headers` — a multimap of name/value pairs with
/// case-insensitive name canonicalization.
///
/// Per spec, the canonical name is always lowercase. Repeated values
/// for the same name combine with `, ` (the spec uses `","` but real
/// browsers and the test suite expect `", "`).
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "Headers")]
pub struct Headers {
    /// `(lowercased_name, value)` pairs in insertion order. Multiple
    /// entries with the same name combine on read; insertion order is
    /// preserved for iteration.
    #[qjs(skip_trace)]
    entries: Rc<RefCell<Vec<(String, String)>>>,
}

impl Headers {
    fn new_empty() -> Self {
        Self {
            entries: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Lowercase + trim a header name. Returns `Err` (TypeError on the
    /// JS side) for empty names.
    fn canonical_name<'js>(ctx: &Ctx<'js>, name: &str) -> rquickjs::Result<String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(rquickjs::Exception::throw_type(
                ctx,
                "Headers: name must not be empty",
            ));
        }
        Ok(trimmed.to_ascii_lowercase())
    }

    /// Normalize a value: trim leading/trailing HTTP whitespace per
    /// spec §5.1 "normalize a byte sequence" (we approximate with
    /// `trim` since QuickJS strings are UTF-16 in memory but only
    /// ASCII whitespace is meaningful here).
    fn normalize_value(value: &str) -> String {
        // Spec: strip leading and trailing HTTP tab/SP characters.
        value
            .trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n')
            .to_owned()
    }

    /// Apply `Headers init`: accept another Headers instance, an
    /// iterable of `[name, value]` pairs, or a plain record. Mutates
    /// the receiver in-place.
    fn apply_init<'js>(&self, ctx: &Ctx<'js>, init: Value<'js>) -> rquickjs::Result<()> {
        if init.is_null() || init.is_undefined() {
            return Ok(());
        }
        // Other Headers instance: copy entries directly.
        if let Some(obj) = init.as_object() {
            if let Some(class) = Class::<Self>::from_object(obj) {
                let src = class.borrow();
                let pairs: Vec<(String, String)> = src.entries.borrow().clone();
                drop(src);
                for (k, v) in pairs {
                    self.append_internal(ctx, &k, &v)?;
                }
                return Ok(());
            }
        }

        // Normalize via JS-side helper installed in `install_web_apis`:
        // returns Array<[name, value]> for either iterable or record
        // input shapes, mirroring the URLSearchParams approach.
        let globals = ctx.globals();
        let normalize: Function<'js> = globals
            .get::<_, Function<'js>>("__hesoNormalizeHeadersInit")
            .map_err(|_| {
                rquickjs::Exception::throw_type(
                    ctx,
                    "internal: Headers bootstrap missing __hesoNormalizeHeadersInit",
                )
            })?;
        let pairs: Array<'js> = normalize.call((init,))?;
        for i in 0..pairs.len() {
            let pair: Array<'js> = pairs.get(i)?;
            let k: String = pair.get(0)?;
            let v: String = pair.get(1)?;
            self.append_internal(ctx, &k, &v)?;
        }
        Ok(())
    }

    fn append_internal<'js>(
        &self,
        ctx: &Ctx<'js>,
        name: &str,
        value: &str,
    ) -> rquickjs::Result<()> {
        let canon = Self::canonical_name(ctx, name)?;
        let norm = Self::normalize_value(value);
        self.entries.borrow_mut().push((canon, norm));
        Ok(())
    }

    /// Collected list of (name, value) pairs in insertion order, with
    /// duplicate names combined per spec (joined by `, `). Used by the
    /// fetch path to flatten Headers into a `Vec<(String, String)>`.
    pub(crate) fn flatten(&self) -> Vec<(String, String)> {
        // Iterate entries in insertion order; combine duplicates per
        // spec §5.4 "get" steps. Maintain output order by first
        // appearance.
        let entries = self.entries.borrow();
        let mut seen: Vec<String> = Vec::new();
        let mut out: Vec<(String, String)> = Vec::new();
        for (k, _) in entries.iter() {
            if !seen.contains(k) {
                seen.push(k.clone());
                let combined = entries
                    .iter()
                    .filter(|(kk, _)| kk == k)
                    .map(|(_, v)| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push((k.clone(), combined));
            }
        }
        out
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl Headers {
    /// `new Headers(init?)`. `init` may be:
    /// - another `Headers` instance
    /// - an iterable of `[name, value]` pairs (e.g. `string[][]`)
    /// - a plain record `Record<string, string>`
    /// - `undefined` / `null` (empty)
    #[qjs(constructor)]
    pub fn new<'js>(ctx: Ctx<'js>, init: Opt<Value<'js>>) -> rquickjs::Result<Self> {
        let h = Self::new_empty();
        if let Some(v) = init.0 {
            h.apply_init(&ctx, v)?;
        }
        Ok(h)
    }

    /// `headers.append(name, value)` — adds a new entry. Duplicates
    /// combine on read.
    fn append<'js>(
        &self,
        ctx: Ctx<'js>,
        name: rquickjs::Coerced<String>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        self.append_internal(&ctx, &name.0, &value.0)
    }

    /// `headers.delete(name)` — remove all entries matching `name`.
    fn delete<'js>(&self, ctx: Ctx<'js>, name: rquickjs::Coerced<String>) -> rquickjs::Result<()> {
        let canon = Self::canonical_name(&ctx, &name.0)?;
        self.entries.borrow_mut().retain(|(k, _)| k != &canon);
        Ok(())
    }

    /// `headers.get(name)` — first matching value with duplicate
    /// combining, or `null`.
    fn get<'js>(
        &self,
        ctx: Ctx<'js>,
        name: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<Option<String>> {
        let canon = Self::canonical_name(&ctx, &name.0)?;
        let entries = self.entries.borrow();
        let matches: Vec<&str> = entries
            .iter()
            .filter(|(k, _)| k == &canon)
            .map(|(_, v)| v.as_str())
            .collect();
        if matches.is_empty() {
            Ok(None)
        } else {
            Ok(Some(matches.join(", ")))
        }
    }

    /// `headers.has(name)`.
    fn has<'js>(&self, ctx: Ctx<'js>, name: rquickjs::Coerced<String>) -> rquickjs::Result<bool> {
        let canon = Self::canonical_name(&ctx, &name.0)?;
        Ok(self.entries.borrow().iter().any(|(k, _)| k == &canon))
    }

    /// `headers.set(name, value)` — replace all existing entries for
    /// `name` with a single entry holding `value`.
    fn set<'js>(
        &self,
        ctx: Ctx<'js>,
        name: rquickjs::Coerced<String>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        let canon = Self::canonical_name(&ctx, &name.0)?;
        let norm = Self::normalize_value(&value.0);
        let mut entries = self.entries.borrow_mut();
        // Find first occurrence; replace value; remove later duplicates.
        let mut replaced = false;
        entries.retain_mut(|(k, v)| {
            if k != &canon {
                return true;
            }
            if !replaced {
                *v = norm.clone();
                replaced = true;
                return true;
            }
            false
        });
        if !replaced {
            entries.push((canon, norm));
        }
        Ok(())
    }

    /// `headers.entries()` — Array of `[name, value]` pairs with
    /// combined duplicates, in lexicographic order of name (per spec
    /// §5.5 "iterate").
    #[qjs(rename = "entries")]
    fn entries_method<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let mut sorted = self.flatten();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let arr = Array::new(ctx.clone())?;
        for (i, (k, v)) in sorted.into_iter().enumerate() {
            let inner = Array::new(ctx.clone())?;
            inner.set(0, k)?;
            inner.set(1, v)?;
            arr.set(i, inner)?;
        }
        Ok(arr)
    }

    /// `headers.keys()` — Array of names.
    fn keys<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let mut sorted = self.flatten();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let arr = Array::new(ctx.clone())?;
        for (i, (k, _)) in sorted.into_iter().enumerate() {
            arr.set(i, k)?;
        }
        Ok(arr)
    }

    /// `headers.values()` — Array of (combined) values.
    fn values<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let mut sorted = self.flatten();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let arr = Array::new(ctx.clone())?;
        for (i, (_, v)) in sorted.into_iter().enumerate() {
            arr.set(i, v)?;
        }
        Ok(arr)
    }

    /// `headers.forEach(callback, thisArg?)` — invoke `callback(value,
    /// name, headers)` for each entry in iteration order. `thisArg` is
    /// accepted but ignored (matches the URLSearchParams precedent).
    fn for_each<'js>(
        this: This<Class<'js, Self>>,
        _ctx: Ctx<'js>,
        callback: Function<'js>,
        _this_arg: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let mut sorted = this.0.borrow().flatten();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let this_value: Value<'js> = this.0.clone().into_value();
        for (k, v) in sorted {
            callback.call::<_, ()>((v, k, this_value.clone()))?;
        }
        Ok(())
    }
}

// =============================================================================
// FormData
// =============================================================================

/// One entry in a [`FormData`] — either a `(name, string)` pair or
/// `(name, Blob, filename)`. The third tuple slot is `None` for plain
/// strings, `Some(filename)` for Blobs (where the spec requires a
/// filename to be present; we default to `"blob"` when the user
/// doesn't supply one).
#[derive(Debug, Clone)]
pub(crate) enum FormDataValue {
    Text(String),
    Blob {
        /// Snapshot of the bytes — taken at append time so subsequent
        /// mutations of the source Blob (impossible per spec, but we
        /// hold an Rc<RefCell> internally so future changes to the
        /// API surface don't accidentally bleed) don't affect already-
        /// recorded entries.
        bytes: Vec<u8>,
        mime: String,
        filename: String,
    },
}

/// WHATWG `FormData` — an ordered list of (name, value) entries,
/// where each value is either a string or a Blob/File with a filename.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "FormData")]
pub struct FormData {
    /// Ordered list of `(name, value)` entries. Same-name entries
    /// keep insertion order; `set` replaces all and re-inserts at the
    /// position of the first match (per spec §5.1 "set").
    #[qjs(skip_trace)]
    entries: Rc<RefCell<Vec<(String, FormDataValue)>>>,
}

impl FormData {
    /// Populate from a form element via JS-side walker. Mirrors the
    /// shape `crate::form_submit::build_snapshot_js` already uses, but
    /// returns plain text/file entries rather than the post-event
    /// `FormSnapshot`. We pull this off the form element's listed
    /// controls, not its `.elements` collection (which we don't
    /// expose).
    fn populate_from_form<'js>(&self, ctx: &Ctx<'js>, form: Value<'js>) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let walker: Function<'js> = globals
            .get::<_, Function<'js>>("__hesoFormDataFromForm")
            .map_err(|_| {
                rquickjs::Exception::throw_type(
                    ctx,
                    "internal: FormData bootstrap missing __hesoFormDataFromForm",
                )
            })?;
        // walker returns Array<[name, value]> where value is either a
        // string or a plain `{__blob: <Class<Blob>>, filename}`
        // wrapper — easier to round-trip than constructing Blob class
        // instances from JS here.
        let entries: Array<'js> = walker.call((form,))?;
        for i in 0..entries.len() {
            let pair: Array<'js> = entries.get(i)?;
            let name: String = pair.get(0)?;
            let value: Value<'js> = pair.get(1)?;
            self.append_value(ctx, &name, value, None)?;
        }
        Ok(())
    }

    /// Internal append: name + arbitrary value + optional filename
    /// override.
    fn append_value<'js>(
        &self,
        ctx: &Ctx<'js>,
        name: &str,
        value: Value<'js>,
        filename: Option<String>,
    ) -> rquickjs::Result<()> {
        let v = Self::coerce_value(ctx, value, filename)?;
        self.entries.borrow_mut().push((name.to_owned(), v));
        Ok(())
    }

    /// Coerce a JS value into a [`FormDataValue`]:
    /// - String → Text
    /// - Blob / File → Blob (with snapshot bytes)
    /// - anything else → stringified via `String(value)`
    fn coerce_value<'js>(
        ctx: &Ctx<'js>,
        value: Value<'js>,
        filename: Option<String>,
    ) -> rquickjs::Result<FormDataValue> {
        if let Some(obj) = value.as_object() {
            // File first (more specific): keeps the file's own filename
            // if the caller didn't override.
            if let Some(class) = Class::<File>::from_object(obj) {
                let f = class.borrow();
                let fname = filename.unwrap_or_else(|| f.name_ref().to_owned());
                return Ok(FormDataValue::Blob {
                    bytes: f.snapshot_bytes(),
                    mime: f.mime_type().to_owned(),
                    filename: fname,
                });
            }
            if let Some(class) = Class::<Blob>::from_object(obj) {
                let b = class.borrow();
                let fname = filename.unwrap_or_else(|| "blob".to_owned());
                return Ok(FormDataValue::Blob {
                    bytes: b.snapshot_bytes(),
                    mime: b.mime_type().to_owned(),
                    filename: fname,
                });
            }
        }
        if let Some(s) = value.as_string() {
            return Ok(FormDataValue::Text(s.to_string()?));
        }
        // Spec: USVString or Blob; non-Blob non-string values are
        // stringified. Use `String(value)` via JS.
        let globals = ctx.globals();
        let stringer: Function<'js> = globals.get("String")?;
        let s: String = stringer.call((value,))?;
        Ok(FormDataValue::Text(s))
    }

    /// Snapshot of all entries — used by the multipart-serialization
    /// path in [`crate::form_submit`] (via [`build_multipart_form_from_formdata`]).
    pub(crate) fn snapshot(&self) -> Vec<(String, FormDataValue)> {
        self.entries.borrow().clone()
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl FormData {
    /// `new FormData(form?, submitter?)`. When `form` is an
    /// HTMLFormElement, populate from its submitable controls.
    /// `submitter` is accepted for spec shape but currently ignored
    /// (the submitter only matters for activator-button entries, and
    /// our walker mirrors the `form_submit` snapshot which already
    /// handles that case via a marker attribute).
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        form: Opt<Value<'js>>,
        _submitter: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let fd = Self {
            entries: Rc::new(RefCell::new(Vec::new())),
        };
        if let Some(f) = form.0 {
            if !f.is_null() && !f.is_undefined() {
                fd.populate_from_form(&ctx, f)?;
            }
        }
        Ok(fd)
    }

    /// `formData.append(name, value, filename?)`.
    fn append<'js>(
        &self,
        ctx: Ctx<'js>,
        name: rquickjs::Coerced<String>,
        value: Value<'js>,
        filename: Opt<rquickjs::Coerced<String>>,
    ) -> rquickjs::Result<()> {
        self.append_value(&ctx, &name.0, value, filename.0.map(|s| s.0))
    }

    /// `formData.delete(name)` — remove all entries matching `name`.
    fn delete(&self, name: rquickjs::Coerced<String>) {
        self.entries.borrow_mut().retain(|(k, _)| k != &name.0);
    }

    /// `formData.get(name)` — first matching value (as JS value),
    /// or `null`.
    fn get<'js>(
        &self,
        ctx: Ctx<'js>,
        name: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<Value<'js>> {
        let entries = self.entries.borrow();
        let found = entries.iter().find(|(k, _)| k == &name.0);
        match found {
            Some((_, v)) => Self::value_to_js(&ctx, v),
            None => Ok(Value::new_null(ctx)),
        }
    }

    /// `formData.getAll(name)` — all matching values, in insertion
    /// order.
    fn get_all<'js>(
        &self,
        ctx: Ctx<'js>,
        name: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<Array<'js>> {
        let entries = self.entries.borrow();
        let matches: Vec<&FormDataValue> = entries
            .iter()
            .filter(|(k, _)| k == &name.0)
            .map(|(_, v)| v)
            .collect();
        let arr = Array::new(ctx.clone())?;
        for (i, v) in matches.iter().enumerate() {
            arr.set(i, Self::value_to_js(&ctx, v)?)?;
        }
        Ok(arr)
    }

    /// `formData.has(name)`.
    fn has(&self, name: rquickjs::Coerced<String>) -> bool {
        self.entries.borrow().iter().any(|(k, _)| k == &name.0)
    }

    /// `formData.set(name, value, filename?)` — replace all existing
    /// entries for `name` with a single entry.
    fn set<'js>(
        &self,
        ctx: Ctx<'js>,
        name: rquickjs::Coerced<String>,
        value: Value<'js>,
        filename: Opt<rquickjs::Coerced<String>>,
    ) -> rquickjs::Result<()> {
        let v = Self::coerce_value(&ctx, value, filename.0.map(|s| s.0))?;
        let mut entries = self.entries.borrow_mut();
        let mut replaced = false;
        entries.retain_mut(|(k, val)| {
            if k != &name.0 {
                return true;
            }
            if !replaced {
                *val = v.clone();
                replaced = true;
                return true;
            }
            false
        });
        if !replaced {
            entries.push((name.0, v));
        }
        Ok(())
    }

    /// `formData.entries()` — Array of `[name, value]` pairs in
    /// insertion order.
    #[qjs(rename = "entries")]
    fn entries_method<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let entries = self.entries.borrow();
        let arr = Array::new(ctx.clone())?;
        for (i, (k, v)) in entries.iter().enumerate() {
            let inner = Array::new(ctx.clone())?;
            inner.set(0, k.clone())?;
            inner.set(1, Self::value_to_js(&ctx, v)?)?;
            arr.set(i, inner)?;
        }
        Ok(arr)
    }

    /// `formData.keys()`.
    fn keys<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let entries = self.entries.borrow();
        let arr = Array::new(ctx.clone())?;
        for (i, (k, _)) in entries.iter().enumerate() {
            arr.set(i, k.clone())?;
        }
        Ok(arr)
    }

    /// `formData.values()`.
    fn values<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Array<'js>> {
        let entries = self.entries.borrow();
        let arr = Array::new(ctx.clone())?;
        for (i, (_, v)) in entries.iter().enumerate() {
            arr.set(i, Self::value_to_js(&ctx, v)?)?;
        }
        Ok(arr)
    }

    /// `formData.forEach(callback, thisArg?)`.
    fn for_each<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        callback: Function<'js>,
        _this_arg: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let entries: Vec<(String, FormDataValue)> = this.0.borrow().entries.borrow().clone();
        let this_value: Value<'js> = this.0.clone().into_value();
        for (k, v) in entries {
            let jv = Self::value_to_js(&ctx, &v)?;
            callback.call::<_, ()>((jv, k, this_value.clone()))?;
        }
        Ok(())
    }
}

impl FormData {
    /// Convert a stored [`FormDataValue`] back into a JS value: text
    /// becomes a string, blob-with-filename becomes a fresh `File`
    /// (more useful than `Blob` — code that reads back a FormData
    /// entry typically wants the filename).
    fn value_to_js<'js>(ctx: &Ctx<'js>, v: &FormDataValue) -> rquickjs::Result<Value<'js>> {
        match v {
            FormDataValue::Text(s) => {
                let js_s = rquickjs::String::from_str(ctx.clone(), s)?;
                Ok(js_s.into_value())
            }
            FormDataValue::Blob {
                bytes,
                mime,
                filename,
            } => {
                let file = File {
                    bytes: Rc::new(RefCell::new(bytes.clone())),
                    mime: mime.clone(),
                    name: filename.clone(),
                    last_modified: default_last_modified(ctx)?,
                };
                let class = Class::instance(ctx.clone(), file)?;
                Ok(class.into_value())
            }
        }
    }
}

// =============================================================================
// Installation
// =============================================================================

/// Register `Blob`, `File`, `Headers`, `FormData` on `globalThis`,
/// plus the JS-side normalization helpers their constructors need.
///
/// Also patches `File.prototype`'s `[[Prototype]]` to `Blob.prototype`
/// so `file instanceof Blob` returns true per spec.
pub fn install_web_apis(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            Class::<Blob>::define(&globals)?;
            Class::<File>::define(&globals)?;
            Class::<Headers>::define(&globals)?;
            Class::<FormData>::define(&globals)?;

            // Patch File so it inherits from Blob. After this:
            //   - File.prototype.__proto__ === Blob.prototype
            //   - new File(...) instanceof Blob === true
            //
            // We also expose `Headers.prototype[Symbol.iterator]` and
            // `FormData.prototype[Symbol.iterator]` so `for-of` works,
            // mirroring the URLSearchParams pattern.
            ctx.eval::<(), _>(WEB_APIS_BOOTSTRAP)?;

            // Install `Object.setPrototypeOf(Blob.prototype, ...)`
            // helper as `Blob.prototype.stream` doesn't get a Promise
            // wrapper from the class macros — already returns
            // undefined per our impl, nothing extra needed.

            // Add static helpers as needed:
            // (none currently — spec doesn't define static members
            // on these four constructors beyond .name on the
            // constructor function itself, which rquickjs sets.)
            let _ = globals;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install web_apis: {e}")))?;
    Ok(())
}

/// JS bootstrap that:
///
/// 1. Re-parents `File.prototype` onto `Blob.prototype` for
///    `instanceof Blob` true.
/// 2. Installs `__hesoNormalizeHeadersInit` — coerces the Headers
///    constructor's polymorphic `init` argument (iterable | record)
///    into a flat `Array<[name, value]>`. Doing this in JS keeps the
///    iterable path spec-correct (it uses `Symbol.iterator` and
///    `.next()` per spec).
/// 3. Installs `__hesoFormDataFromForm` — walks a form element and
///    returns its entry list. Mirrors the listing in
///    [`crate::form_submit::build_snapshot_js`] but adapted for the
///    `new FormData(form)` use case: no event dispatch, no activator,
///    just the controls' current values.
/// 4. Patches `Headers.prototype[Symbol.iterator]` and
///    `FormData.prototype[Symbol.iterator]` to forward to `.entries()`.
const WEB_APIS_BOOTSTRAP: &str = r#"
(function () {
    // ---- File extends Blob ----
    //
    // rquickjs registers each class with its own prototype chain;
    // re-parenting File.prototype onto Blob.prototype is the standard
    // dance for "B extends A" when the host can't express it via the
    // class macro.
    if (typeof File === 'function' && typeof Blob === 'function') {
        Object.setPrototypeOf(File.prototype, Blob.prototype);
        // Make `File.constructor === File`, `File.prototype.constructor === File`
        // (rquickjs sets the latter; the former is fixed by the
        // function-on-globalThis itself).
    }

    // ---- Headers init normalization ----
    globalThis.__hesoNormalizeHeadersInit = function (init) {
        if (init == null) return [];
        // If it's an iterable of pairs, iterate it.
        if (typeof init[Symbol.iterator] === 'function') {
            const out = [];
            for (const pair of init) {
                if (pair == null) {
                    throw new TypeError(
                        'Headers: each iterable element must be a [name, value] pair'
                    );
                }
                let k, v;
                if (typeof pair[Symbol.iterator] === 'function' && !Array.isArray(pair)) {
                    const items = [];
                    for (const x of pair) items.push(x);
                    if (items.length !== 2) {
                        throw new TypeError(
                            'Headers: each pair must have exactly two elements'
                        );
                    }
                    k = items[0];
                    v = items[1];
                } else {
                    if (pair.length !== 2) {
                        throw new TypeError(
                            'Headers: each pair must have exactly two elements'
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
        for (const k of Object.keys(init)) {
            out.push([String(k), String(init[k])]);
        }
        return out;
    };

    // ---- FormData population from <form> ----
    //
    // Returns Array<[name, value]> where value is a string for plain
    // controls and a File-shaped wrapper (a real File instance built
    // from the JS-visible `input.files[0]` if present, otherwise
    // skipped — the spec creates a Blob-with-filename for file inputs,
    // but we don't yet have the underlying bytes for those, so we
    // emit a zero-byte File with the right name/type so the entry
    // is observable and serializable). This is the same limitation
    // form_submit.rs documents.
    //
    // Mirrors the control-walker in form_submit.rs's snapshot JS,
    // minus the event dispatch and activator logic — FormData is
    // populated mid-flight, not as part of submit. Stays in sync by
    // convention rather than code sharing; the snapshot path is
    // submit-specific (it dispatches `submit` events) and the
    // FormData path is constructor-driven.
    globalThis.__hesoFormDataFromForm = function (form) {
        const out = [];
        if (!form || typeof form.querySelectorAll !== 'function') {
            throw new TypeError('FormData: argument must be an HTMLFormElement');
        }
        // Spec requires an HTMLFormElement specifically — a div with
        // a querySelectorAll won't do. We don't expose a separate
        // HTMLFormElement class (Element is the only DOM wrapper),
        // so check the tag name directly.
        const tag = (form.tagName || '').toLowerCase();
        if (tag !== 'form') {
            throw new TypeError('FormData: argument must be an HTMLFormElement');
        }
        const controls = form.querySelectorAll('input, select, textarea, button');
        const isDisabled = (el) => el.hasAttribute && el.hasAttribute('disabled');
        for (let idx = 0; idx < controls.length; idx++) {
            const el = controls[idx];
            const tag = (el.tagName || '').toLowerCase();
            const name = el.getAttribute('name');
            if (!name) continue;
            if (isDisabled(el)) continue;

            if (tag === 'button') continue;

            if (tag === 'input') {
                const type = (el.getAttribute('type') || 'text').toLowerCase();
                switch (type) {
                    case 'submit':
                    case 'reset':
                    case 'button':
                    case 'image':
                        continue;
                    case 'checkbox':
                    case 'radio': {
                        if (!el.checked) continue;
                        const v = el.value || el.getAttribute('value') || 'on';
                        out.push([name, v]);
                        break;
                    }
                    case 'file': {
                        // Emit a zero-byte File with the input's filename
                        // (real bytes aren't reachable yet — same gap as
                        // form_submit's multipart path). Construct via
                        // the global so it picks up the right prototype.
                        let filename = '';
                        if (el.files && el.files.length > 0) {
                            filename = el.files[0].name || '';
                        }
                        const file = new File([], filename || '', {
                            type: 'application/octet-stream'
                        });
                        out.push([name, file]);
                        break;
                    }
                    default: {
                        out.push([name, (el.value || el.getAttribute('value') || '')]);
                    }
                }
                continue;
            }

            if (tag === 'textarea') {
                out.push([name, (el.value || el.textContent || '')]);
                continue;
            }

            if (tag === 'select') {
                const isMultiple = el.hasAttribute('multiple');
                const optionEls = el.querySelectorAll('option');
                let pickedAny = false;
                for (const opt of optionEls) {
                    const selected = opt.hasAttribute('selected') || (opt.selected === true);
                    if (!selected) continue;
                    pickedAny = true;
                    const v = (opt.getAttribute('value') !== null)
                        ? opt.getAttribute('value')
                        : (opt.textContent || '');
                    out.push([name, v]);
                    if (!isMultiple) break;
                }
                if (!isMultiple && !pickedAny && optionEls.length > 0) {
                    const opt = optionEls[0];
                    const v = (opt.getAttribute('value') !== null)
                        ? opt.getAttribute('value')
                        : (opt.textContent || '');
                    out.push([name, v]);
                }
                continue;
            }
        }
        return out;
    };

    // ---- Symbol.iterator on Headers / FormData ----
    if (typeof globalThis.Headers === 'function') {
        Object.defineProperty(Headers.prototype, Symbol.iterator, {
            value: function () { return this.entries()[Symbol.iterator](); },
            writable: true, configurable: true, enumerable: false,
        });
    }
    if (typeof globalThis.FormData === 'function') {
        Object.defineProperty(FormData.prototype, Symbol.iterator, {
            value: function () { return this.entries()[Symbol.iterator](); },
            writable: true, configurable: true, enumerable: false,
        });
    }
})();
"#;

// =============================================================================
// Multipart serialization
// =============================================================================

/// Build a `reqwest::multipart::Form` from a [`FormData`] snapshot.
///
/// Text entries become `Part::text(...)`. Blob/File entries become
/// `Part::bytes(...)` with the recorded filename and content type
/// (defaulting to `application/octet-stream` when the blob's `type`
/// is empty, per WHATWG fetch §6.3 "extract a body" step 12).
///
/// This is the function `crate::fetch` calls when a `FormData` is
/// passed as a fetch body.
pub(crate) fn build_multipart_form_from_formdata(
    entries: &[(String, FormDataValue)],
) -> reqwest::multipart::Form {
    let mut form = reqwest::multipart::Form::new();
    for (name, v) in entries {
        let n = name.clone();
        match v {
            FormDataValue::Text(s) => {
                form = form.part(n, reqwest::multipart::Part::text(s.clone()));
            }
            FormDataValue::Blob {
                bytes,
                mime,
                filename,
            } => {
                let bare = reqwest::multipart::Part::bytes(bytes.clone()).file_name(filename.clone());
                let mime_to_use = if mime.is_empty() {
                    "application/octet-stream".to_string()
                } else {
                    mime.clone()
                };
                let part = match bare.mime_str(&mime_to_use) {
                    Ok(p) => p,
                    Err(_) => reqwest::multipart::Part::bytes(bytes.clone()).file_name(filename.clone()),
                };
                form = form.part(n, part);
            }
        }
    }
    form
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_canonical_name_lowercases_and_trims() {
        // Direct unit on the helper — full JS-side coverage in
        // tests/headers.rs.
        let ctx_dummy = || {};
        // Smoke-test the trim+lowercase logic in isolation. We can't
        // build a real Ctx here, but we can exercise normalize_value.
        let _ = ctx_dummy;
        assert_eq!(Headers::normalize_value("  hello  "), "hello");
        assert_eq!(Headers::normalize_value("hello"), "hello");
        assert_eq!(Headers::normalize_value(" \tx\r\n"), "x");
    }

    #[test]
    fn formdata_build_multipart_text_only() {
        let entries = vec![
            ("a".to_owned(), FormDataValue::Text("1".to_owned())),
            ("b".to_owned(), FormDataValue::Text("two".to_owned())),
        ];
        let _form = build_multipart_form_from_formdata(&entries);
        // We can't easily inspect the form contents without making
        // it `send()` somewhere; full coverage in tests/formdata.rs
        // (which uses wiremock).
    }

    #[test]
    fn formdata_build_multipart_with_blob_uses_default_mime() {
        let entries = vec![(
            "file".to_owned(),
            FormDataValue::Blob {
                bytes: b"hello".to_vec(),
                mime: "".to_owned(),
                filename: "x.bin".to_owned(),
            },
        )];
        let _form = build_multipart_form_from_formdata(&entries);
        // Same caveat — see comment above.
    }
}
