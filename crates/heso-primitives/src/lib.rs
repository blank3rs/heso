//! # heso-primitives
//!
//! The **internal** primitives layer — a small, fixed set of operations the
//! planner emits and the trace runner executes against an [`EngineApi`]
//! instance. **Agents never see this crate.** They see one tool: `heso.run`.
//!
//! ## Mental model: the page is a directory
//!
//! Per [ADR 0010], heso models a browsing session like a Unix shell. The
//! current page is the *working directory*. Elements on the page are *files*.
//! Navigation is `cd`. Listing children is `ls`. Reading is `cat`. Writing is
//! `echo`. Cookies and Web Storage live under `/env/`.
//!
//! This shape gives the planner (and the LLMs that *trained on* shell sessions)
//! a strong prior for what each primitive does, and gives the agent a clear
//! sense of *where it is* and *what's around it* at every step of a trace.
//!
//! ## Command reference
//!
//! | Op | Terminal analogue | Purpose |
//! |---|---|---|
//! | [`PrimitiveOp::Pwd`] | `pwd`        | Current URL + page title |
//! | [`PrimitiveOp::Ls`]  | `ls [path]`  | List interactable elements (or virtual env contents) |
//! | [`PrimitiveOp::Cd`]  | `cd <target>`| Navigate by URL or by clicking a link |
//! | [`PrimitiveOp::Cat`] | `cat <path>` | Read element text or env value |
//! | [`PrimitiveOp::Find`]| `find -<pred>`| Locate elements matching a predicate |
//! | [`PrimitiveOp::Grep`]| `grep <re>`  | Regex-search page text |
//! | [`PrimitiveOp::Echo`]| `echo v > p` | Write a value (field / cookie / storage) |
//! | [`PrimitiveOp::Rm`]  | `rm <path>`  | Clear / delete (field / cookie / storage) |
//! | [`PrimitiveOp::Click`]| (no direct) | Interact with a non-navigating element |
//! | [`PrimitiveOp::Submit`]| (no direct)| Submit a form |
//! | [`PrimitiveOp::Wget`]| `wget <url>` | Fetch URL or element resource as bytes |
//! | [`PrimitiveOp::Wait`]| (no direct)  | Block until a condition holds |
//! | [`PrimitiveOp::Screenshot`]| (no direct) | Capture viewport PNG |
//! | [`PrimitiveOp::Eval`]| `sh -c <src>`| Execute JS in the page context (escape hatch) |
//! | [`PrimitiveOp::Diff`]| `diff a b`   | Diff two snapshots |
//!
//! ## Status
//!
//! Skeleton. The op AST and types are complete and round-trip through JSON.
//! Execution: [`cd`] delegates to [`EngineApi::open`] for URL navigation; the
//! other primitives return [`Error::NotImplemented`] until T-013 grows the
//! engine surface and T-014 / T-015 / T-017 land the determinism preconditions
//! (software rendering, fake clock, network record/replay).
//!
//! [ADR 0010]: ../../decisions/0010-primitives-as-terminal-commands.md

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use heso_core::Url;
use heso_engine_api::{EngineApi, Page};
use serde::{Deserialize, Serialize};

// ============================================================================
// Error type
// ============================================================================

/// Errors returned by primitive execution.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A primitive is not yet wired to the engine. Expected during M0/M1
    /// skeleton work; should not appear in a released build.
    #[error("primitive not yet implemented: {0}")]
    NotImplemented(&'static str),

    /// The underlying engine returned an error.
    #[error("engine error: {0}")]
    Engine(#[from] heso_core::Error),

    /// An [`ElementRef`] was not present in the current page's AX tree.
    /// The planner should take a fresh snapshot and reissue.
    #[error("no such element: {element}")]
    NoSuchElement {
        /// The ref the primitive was given.
        element: ElementRef,
    },

    /// A [`PrimitiveOp::Wait`] exceeded its deadline.
    #[error("wait timed out after {waited_ms} ms")]
    WaitTimeout {
        /// Fake-clock milliseconds the primitive waited before giving up.
        waited_ms: u64,
    },

    /// A path provided to [`cat`], [`echo`], [`rm`], or [`ls`] is not a known
    /// virtual path.
    #[error("unknown env path: {path}")]
    UnknownEnvPath {
        /// The offending path string.
        path: String,
    },
}

/// Convenience alias for primitive results.
pub type Result<T> = std::result::Result<T, Error>;

// ============================================================================
// Opaque handles
// ============================================================================

/// Opaque handle to a node in the page's accessibility tree.
///
/// Refs are minted by the engine on snapshot. They are stable within a single
/// snapshot; a page mutation invalidates outstanding refs and the planner must
/// take a new snapshot.
///
/// ```
/// use heso_primitives::ElementRef;
/// let r = ElementRef::new("@e3");
/// assert_eq!(r.to_string(), "@e3");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ElementRef(pub String);

impl ElementRef {
    /// Construct an element ref from its string form (e.g. `"@e3"`).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for ElementRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque handle to a stored page snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub String);

impl SnapshotId {
    /// Construct a snapshot id from its string form (e.g. `"@s7"`).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

// ============================================================================
// Virtual env paths — the /env/ hierarchy
// ============================================================================

/// A virtual path under `/env/`. Cookies and Web Storage are addressable as
/// files under this hierarchy so they can be read/written via [`cat`] /
/// [`echo`] / [`rm`] / [`ls`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvPath {
    /// One cookie on the current origin: `/env/cookie/<name>`.
    Cookie {
        /// Cookie name.
        name: String,
    },
    /// One `localStorage` entry: `/env/storage/local/<key>`.
    StorageLocal {
        /// Storage key.
        key: String,
    },
    /// One `sessionStorage` entry: `/env/storage/session/<key>`.
    StorageSession {
        /// Storage key.
        key: String,
    },
}

/// A "directory" in the virtual env hierarchy — what [`ls`] and [`rm`] can
/// target as a group instead of an individual key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvScope {
    /// `/env/cookie/` — every cookie on the current origin.
    Cookie,
    /// `/env/storage/local/` — every `localStorage` entry.
    StorageLocal,
    /// `/env/storage/session/` — every `sessionStorage` entry.
    StorageSession,
}

// ============================================================================
// PrimitiveOp AST
// ============================================================================

/// One operation in a trace.
///
/// This is the JSON-serializable AST node the planner emits and the trace
/// runner consumes. A [`Trace`] is `Vec<PrimitiveOp>`. Operations serialize
/// with an `"op"` discriminator and flatten their input fields:
///
/// ```
/// use heso_primitives::{PrimitiveOp, CdInput, CdTarget};
/// use heso_core::Url;
///
/// let op = PrimitiveOp::Cd(CdInput {
///     target: CdTarget::Url { url: Url::parse("https://example.com/").unwrap() },
/// });
/// let json = serde_json::to_value(&op).unwrap();
/// assert_eq!(json["op"], "cd");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PrimitiveOp {
    /// Print the working "directory" — current URL + page title.
    Pwd(PwdInput),
    /// List interactable elements on the current page, or contents of a
    /// virtual env scope.
    Ls(LsInput),
    /// Navigate by URL or by clicking a link element.
    Cd(CdInput),
    /// Read element text content or an env value.
    Cat(CatInput),
    /// Locate elements matching a predicate.
    Find(FindInput),
    /// Regex-search the current page's text.
    Grep(GrepInput),
    /// Write a value (fill a field, set a cookie, set a storage key).
    Echo(EchoInput),
    /// Clear or delete (field, cookie, storage key, storage scope).
    Rm(RmInput),
    /// Interact with a non-navigating element (button, toggle, custom widget).
    Click(ClickInput),
    /// Submit a form by element ref.
    Submit(SubmitInput),
    /// Fetch a URL or element resource as raw bytes (without navigating).
    Wget(WgetInput),
    /// Block until a condition holds or a timeout fires.
    Wait(WaitInput),
    /// Capture a PNG screenshot of the viewport or a single element.
    Screenshot(ScreenshotInput),
    /// Evaluate JS in the page context (escape hatch).
    Eval(EvalInput),
    /// Diff two page snapshots.
    Diff(DiffInput),
}

/// An ordered sequence of primitive operations — the canonical artifact signed
/// in a receipt.
pub type Trace = Vec<PrimitiveOp>;

// ============================================================================
// PrimitiveResult
// ============================================================================

/// The typed output of one [`PrimitiveOp`]. Returned by [`execute`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PrimitiveResult {
    /// Output of [`pwd`].
    Pwd(PwdOutput),
    /// Output of [`ls`].
    Ls(LsOutput),
    /// Output of [`cd`].
    Cd(CdOutput),
    /// Output of [`cat`].
    Cat(CatOutput),
    /// Output of [`find`].
    Find(FindOutput),
    /// Output of [`grep`].
    Grep(GrepOutput),
    /// Output of [`echo`].
    Echo(EchoOutput),
    /// Output of [`rm`].
    Rm(RmOutput),
    /// Output of [`click`].
    Click(ClickOutput),
    /// Output of [`submit`].
    Submit(SubmitOutput),
    /// Output of [`wget`].
    Wget(WgetOutput),
    /// Output of [`wait`].
    Wait(WaitOutput),
    /// Output of [`screenshot`].
    Screenshot(ScreenshotOutput),
    /// Output of [`eval`].
    Eval(EvalOutput),
    /// Output of [`diff`].
    Diff(DiffOutput),
}

// ============================================================================
// pwd — print working directory
// ============================================================================

/// Input to [`pwd`]. No fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PwdInput {}

/// Output of [`pwd`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PwdOutput {
    /// The current page URL.
    pub url: Url,
    /// The current page title.
    pub title: String,
}

/// Print the working "directory": current URL + page title.
pub fn pwd<E: EngineApi>(_engine: &E, _input: &PwdInput) -> Result<PwdOutput> {
    Err(Error::NotImplemented(
        "pwd — pending engine page-state surface (T-013)",
    ))
}

// ============================================================================
// ls — list elements or env scope
// ============================================================================

/// What [`ls`] should list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LsTarget {
    /// List interactable elements on the current page.
    Page,
    /// List entries in one virtual env scope (e.g. all cookies).
    Env {
        /// The scope to enumerate.
        scope: EnvScope,
    },
}

/// Input to [`ls`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LsInput {
    /// What to list (defaults to the current page).
    pub target: LsTarget,
}

/// One entry returned by [`ls`] when targeting a page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageEntry {
    /// Element ref.
    pub element: ElementRef,
    /// ARIA role (e.g. `"link"`, `"button"`, `"textbox"`).
    pub role: String,
    /// Accessible name (the element's label).
    pub name: String,
}

/// Output of [`ls`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LsOutput {
    /// Page entries (when `target = Page`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub page_entries: Vec<PageEntry>,
    /// Env entries (when `target = Env`) — the keys/names present in the scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_entries: Vec<String>,
}

/// List interactable elements or env scope contents.
pub fn ls<E: EngineApi>(_engine: &E, _input: &LsInput) -> Result<LsOutput> {
    Err(Error::NotImplemented(
        "ls — pending engine AX-tree + cookie/storage surfaces (T-013)",
    ))
}

// ============================================================================
// cd — change directory (navigate)
// ============================================================================

/// What [`cd`] should navigate to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CdTarget {
    /// Load an absolute URL as the new page.
    Url {
        /// The URL to load.
        url: Url,
    },
    /// Click a link element and follow its navigation.
    Element {
        /// The link to follow.
        element: ElementRef,
    },
    /// Go back one entry in history (`cd ..`).
    Back,
    /// Toggle to the previous page (`cd -`).
    Previous,
}

/// Input to [`cd`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdInput {
    /// Where to go.
    pub target: CdTarget,
}

/// Output of [`cd`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdOutput {
    /// URL after navigation (after redirects, if any).
    pub url: Url,
}

/// Navigate by URL or by clicking a link.
pub async fn cd<E: EngineApi>(engine: &E, input: &CdInput) -> Result<CdOutput> {
    match &input.target {
        CdTarget::Url { url } => {
            let page = engine.open(url).await?;
            Ok(CdOutput { url: page.url().clone() })
        }
        CdTarget::Element { .. } => Err(Error::NotImplemented(
            "cd @element — pending engine click+navigation surface (T-013)",
        )),
        CdTarget::Back => Err(Error::NotImplemented(
            "cd .. — pending engine history surface (T-013)",
        )),
        CdTarget::Previous => Err(Error::NotImplemented(
            "cd - — pending engine history surface (T-013)",
        )),
    }
}

// ============================================================================
// cat — read content
// ============================================================================

/// What [`cat`] should read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatTarget {
    /// Read the text content of one element.
    Element {
        /// The element to read.
        element: ElementRef,
        /// Which property to read (`"text"`, `"href"`, `"value"`, ARIA attr, …).
        attr: String,
    },
    /// Read one virtual env value.
    Env {
        /// The env path (e.g. `/env/cookie/sid`).
        path: EnvPath,
    },
}

/// Input to [`cat`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatInput {
    /// What to read.
    pub target: CatTarget,
}

/// Output of [`cat`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatOutput {
    /// Read value. `None` for absent env keys; element reads always return
    /// `Some` (or fail with [`Error::NoSuchElement`]).
    pub value: Option<String>,
}

/// Read element content or an env value.
pub fn cat<E: EngineApi>(_engine: &E, _input: &CatInput) -> Result<CatOutput> {
    Err(Error::NotImplemented(
        "cat — pending engine AX-tree + env surfaces (T-013)",
    ))
}

// ============================================================================
// find — locate by predicate
// ============================================================================

/// A predicate [`find`] matches against AX-tree nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FindPredicate {
    /// Match nodes with this ARIA role (e.g. `"link"`, `"button"`).
    Role {
        /// Role to match.
        role: String,
    },
    /// Match nodes whose accessible name equals this string exactly.
    NameEquals {
        /// Name to match.
        name: String,
    },
    /// Match nodes whose accessible name contains this substring (case-
    /// insensitive).
    NameContains {
        /// Substring to match.
        substring: String,
    },
    /// Match nodes with the given role AND a matching name substring.
    RoleAndName {
        /// Role to match.
        role: String,
        /// Substring of the name to match.
        substring: String,
    },
}

/// Input to [`find`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindInput {
    /// The predicate to match.
    pub predicate: FindPredicate,
}

/// Output of [`find`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindOutput {
    /// Element refs that satisfied the predicate.
    pub matches: Vec<ElementRef>,
}

/// Locate elements matching a predicate.
pub fn find<E: EngineApi>(_engine: &E, _input: &FindInput) -> Result<FindOutput> {
    Err(Error::NotImplemented(
        "find — pending engine AX-tree surface (T-013)",
    ))
}

// ============================================================================
// grep — regex search through page text
// ============================================================================

/// Input to [`grep`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepInput {
    /// The regex to search for.
    pub pattern: String,
    /// Case-insensitive match if `true`. Mirrors `grep -i`.
    #[serde(default)]
    pub ignore_case: bool,
    /// If set, scope the search to this element instead of the whole page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<ElementRef>,
}

/// One match returned by [`grep`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    /// The element where the match was found.
    pub element: ElementRef,
    /// The matched text.
    pub text: String,
}

/// Output of [`grep`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepOutput {
    /// Matches in document order.
    pub matches: Vec<GrepMatch>,
}

/// Regex-search the current page text.
pub fn grep<E: EngineApi>(_engine: &E, _input: &GrepInput) -> Result<GrepOutput> {
    Err(Error::NotImplemented(
        "grep — pending engine AX-tree text surface (T-013)",
    ))
}

// ============================================================================
// echo — write a value
// ============================================================================

/// Where [`echo`] should write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EchoTarget {
    /// Write to a form field (`<input>`, `<textarea>`, etc.).
    Field {
        /// The element to fill.
        element: ElementRef,
    },
    /// Write to an env path (cookie or storage key).
    Env {
        /// The env path to set.
        path: EnvPath,
    },
}

/// Input to [`echo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EchoInput {
    /// The value to write.
    pub value: String,
    /// Where to write it.
    pub target: EchoTarget,
    /// If `true`, append to the existing value (`>>`); else overwrite (`>`).
    #[serde(default)]
    pub append: bool,
}

/// Output of [`echo`]. Empty — success is signalled by absence of error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EchoOutput {}

/// Write a value (field, cookie, or storage).
pub fn echo<E: EngineApi>(_engine: &E, _input: &EchoInput) -> Result<EchoOutput> {
    Err(Error::NotImplemented(
        "echo — pending engine fill + env surfaces (T-013)",
    ))
}

// ============================================================================
// rm — clear / delete
// ============================================================================

/// What [`rm`] should clear or delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RmTarget {
    /// Clear the value of a form field.
    Field {
        /// The element to clear.
        element: ElementRef,
    },
    /// Delete one env key.
    Env {
        /// The env path.
        path: EnvPath,
    },
    /// Clear every entry in an env scope (e.g. all cookies).
    EnvScope {
        /// The scope to clear.
        scope: EnvScope,
    },
}

/// Input to [`rm`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RmInput {
    /// What to remove.
    pub target: RmTarget,
}

/// Output of [`rm`]. Empty — success is signalled by absence of error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RmOutput {}

/// Clear / delete (field, cookie, storage key, or env scope).
pub fn rm<E: EngineApi>(_engine: &E, _input: &RmInput) -> Result<RmOutput> {
    Err(Error::NotImplemented(
        "rm — pending engine fill + env surfaces (T-013)",
    ))
}

// ============================================================================
// click — interact with a non-navigating element
// ============================================================================

/// Input to [`click`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClickInput {
    /// The element to click. Use [`cd`] instead for links that navigate; this
    /// op is for buttons, toggles, and custom interactive widgets where no
    /// navigation is expected.
    pub element: ElementRef,
}

/// Output of [`click`]. Empty — observe the post-state with [`pwd`] / [`ls`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClickOutput {}

/// Interact with a non-navigating element.
pub fn click<E: EngineApi>(_engine: &E, _input: &ClickInput) -> Result<ClickOutput> {
    Err(Error::NotImplemented(
        "click — pending engine click surface (T-013)",
    ))
}

// ============================================================================
// submit — submit a form
// ============================================================================

/// Input to [`submit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitInput {
    /// The form to submit.
    pub form: ElementRef,
}

/// Output of [`submit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitOutput {
    /// URL after submission (the response page).
    pub url: Url,
}

/// Submit a form by element ref.
pub fn submit<E: EngineApi>(_engine: &E, _input: &SubmitInput) -> Result<SubmitOutput> {
    Err(Error::NotImplemented(
        "submit — pending engine submit surface (T-013)",
    ))
}

// ============================================================================
// wget — fetch bytes
// ============================================================================

/// What [`wget`] should fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WgetTarget {
    /// Fetch an absolute URL.
    Url {
        /// The URL to fetch.
        url: Url,
    },
    /// Fetch the resource referenced by an element (`<img src>`, `<video src>`,
    /// `<a href>` as raw bytes, etc.).
    Element {
        /// The element whose resource to fetch.
        element: ElementRef,
    },
}

/// Input to [`wget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WgetInput {
    /// What to fetch.
    pub target: WgetTarget,
}

/// Output of [`wget`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WgetOutput {
    /// Raw response bytes.
    pub bytes: Vec<u8>,
    /// MIME type, if the response declared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

/// Fetch a URL or element resource as raw bytes (does not navigate).
pub fn wget<E: EngineApi>(_engine: &E, _input: &WgetInput) -> Result<WgetOutput> {
    Err(Error::NotImplemented(
        "wget — pending engine resource-fetch surface (T-013, T-017)",
    ))
}

// ============================================================================
// wait — block on a condition
// ============================================================================

/// What [`wait`] should block on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitCondition {
    /// Wait until an element with this ref appears in the AX tree.
    ElementExists {
        /// Element to poll for.
        element: ElementRef,
    },
    /// Wait until the page URL contains the given fragment.
    UrlContains {
        /// Substring to match in the page URL.
        fragment: String,
    },
    /// Advance the fake clock by a fixed amount with no other condition.
    Sleep {
        /// Milliseconds to advance.
        ms: u64,
    },
}

/// Input to [`wait`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitInput {
    /// The condition to block on.
    pub condition: WaitCondition,
    /// Maximum fake-clock milliseconds before returning [`Error::WaitTimeout`].
    pub timeout_ms: u64,
}

/// Output of [`wait`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitOutput {
    /// Fake-clock milliseconds spent waiting before the condition held.
    pub waited_ms: u64,
}

/// Block until a condition holds or a timeout fires.
pub fn wait<E: EngineApi>(_engine: &E, _input: &WaitInput) -> Result<WaitOutput> {
    Err(Error::NotImplemented(
        "wait — pending fake clock + engine wait surface (T-013, T-015)",
    ))
}

// ============================================================================
// screenshot — capture viewport PNG
// ============================================================================

/// Input to [`screenshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotInput {
    /// If set, capture only this element; otherwise capture the full viewport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<ElementRef>,
}

/// Output of [`screenshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotOutput {
    /// Raw PNG bytes.
    pub png_bytes: Vec<u8>,
}

/// Capture a PNG screenshot.
///
/// Determinism: this primitive depends on the software-rendering and pinned-
/// font preconditions tracked in T-014. The bytes returned by two runs of the
/// same trace with the same seed will be byte-identical once T-014 lands.
pub fn screenshot<E: EngineApi>(
    _engine: &E,
    _input: &ScreenshotInput,
) -> Result<ScreenshotOutput> {
    Err(Error::NotImplemented(
        "screenshot — pending engine render surface (T-013, T-014)",
    ))
}

// ============================================================================
// eval — JS escape hatch
// ============================================================================

/// Input to [`eval`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalInput {
    /// JS source to evaluate in the page context.
    pub source: String,
}

/// Output of [`eval`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalOutput {
    /// Script return value, JSON-encoded.
    pub return_value: serde_json::Value,
}

/// Evaluate JS in the page context (escape hatch).
pub fn eval<E: EngineApi>(_engine: &E, _input: &EvalInput) -> Result<EvalOutput> {
    Err(Error::NotImplemented(
        "eval — pending SpiderMonkey eval surface (T-013)",
    ))
}

// ============================================================================
// diff — diff two snapshots
// ============================================================================

/// Input to [`diff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffInput {
    /// The earlier snapshot.
    pub before: SnapshotId,
    /// The later snapshot.
    pub after: SnapshotId,
}

/// One change reported by [`diff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffChange {
    /// A node was added between `before` and `after`.
    Added {
        /// Ref of the new node, valid in `after`.
        element: ElementRef,
    },
    /// A node was removed between `before` and `after`.
    Removed {
        /// Ref of the removed node, valid in `before`.
        element: ElementRef,
    },
    /// A node's attributes or text changed.
    Changed {
        /// Ref of the changed node, valid in `after`.
        element: ElementRef,
    },
}

/// Output of [`diff`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffOutput {
    /// Per-node changes between the two snapshots.
    pub changes: Vec<DiffChange>,
}

/// Diff two page snapshots and return the per-node changes.
pub fn diff<E: EngineApi>(_engine: &E, _input: &DiffInput) -> Result<DiffOutput> {
    Err(Error::NotImplemented(
        "diff — pending snapshot store (T-021)",
    ))
}

// ============================================================================
// Central dispatch
// ============================================================================

/// Execute one op from the AST and return the matching [`PrimitiveResult`].
///
/// Async because `cd` may await an `EngineApi::open` call. The other 14
/// primitives are sync-bodied today (they return `NotImplemented`) so
/// awaiting them is free.
///
/// This is the entry point the trace runner uses ([`heso-trace-exec`], T-021).
pub async fn execute<E: EngineApi>(engine: &E, op: &PrimitiveOp) -> Result<PrimitiveResult> {
    match op {
        PrimitiveOp::Pwd(i) => pwd(engine, i).map(PrimitiveResult::Pwd),
        PrimitiveOp::Ls(i) => ls(engine, i).map(PrimitiveResult::Ls),
        PrimitiveOp::Cd(i) => cd(engine, i).await.map(PrimitiveResult::Cd),
        PrimitiveOp::Cat(i) => cat(engine, i).map(PrimitiveResult::Cat),
        PrimitiveOp::Find(i) => find(engine, i).map(PrimitiveResult::Find),
        PrimitiveOp::Grep(i) => grep(engine, i).map(PrimitiveResult::Grep),
        PrimitiveOp::Echo(i) => echo(engine, i).map(PrimitiveResult::Echo),
        PrimitiveOp::Rm(i) => rm(engine, i).map(PrimitiveResult::Rm),
        PrimitiveOp::Click(i) => click(engine, i).map(PrimitiveResult::Click),
        PrimitiveOp::Submit(i) => submit(engine, i).map(PrimitiveResult::Submit),
        PrimitiveOp::Wget(i) => wget(engine, i).map(PrimitiveResult::Wget),
        PrimitiveOp::Wait(i) => wait(engine, i).map(PrimitiveResult::Wait),
        PrimitiveOp::Screenshot(i) => screenshot(engine, i).map(PrimitiveResult::Screenshot),
        PrimitiveOp::Eval(i) => eval(engine, i).map(PrimitiveResult::Eval),
        PrimitiveOp::Diff(i) => diff(engine, i).map(PrimitiveResult::Diff),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn rt<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    // --- AST shape ---

    #[test]
    fn cd_url_serializes_with_op_tag_and_nested_target_kind() {
        let op = PrimitiveOp::Cd(CdInput {
            target: CdTarget::Url { url: u("https://example.com/") },
        });
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "cd");
        assert_eq!(json["target"]["kind"], "url");
        assert_eq!(json["target"]["url"], "https://example.com/");
    }

    #[test]
    fn cd_special_targets_roundtrip() {
        for target in [CdTarget::Back, CdTarget::Previous] {
            let op = PrimitiveOp::Cd(CdInput { target: target.clone() });
            match rt(&op) {
                PrimitiveOp::Cd(CdInput { target: round }) => assert_eq!(round, target),
                other => panic!("wrong variant: {other:?}"),
            }
        }
    }

    #[test]
    fn echo_with_env_path_serializes_cleanly() {
        let op = PrimitiveOp::Echo(EchoInput {
            value: "abc".into(),
            target: EchoTarget::Env { path: EnvPath::Cookie { name: "sid".into() } },
            append: false,
        });
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "echo");
        assert_eq!(json["target"]["kind"], "env");
        assert_eq!(json["target"]["path"]["kind"], "cookie");
        assert_eq!(json["target"]["path"]["name"], "sid");
        assert_eq!(json["value"], "abc");
        assert_eq!(json["append"], false);
    }

    #[test]
    fn all_fifteen_op_variants_roundtrip() {
        let ops: Vec<PrimitiveOp> = vec![
            PrimitiveOp::Pwd(PwdInput {}),
            PrimitiveOp::Ls(LsInput { target: LsTarget::Page }),
            PrimitiveOp::Cd(CdInput { target: CdTarget::Url { url: u("https://example.com/") } }),
            PrimitiveOp::Cat(CatInput {
                target: CatTarget::Element {
                    element: ElementRef::new("@e1"),
                    attr: "text".into(),
                },
            }),
            PrimitiveOp::Find(FindInput {
                predicate: FindPredicate::Role { role: "link".into() },
            }),
            PrimitiveOp::Grep(GrepInput {
                pattern: "rust".into(),
                ignore_case: true,
                within: None,
            }),
            PrimitiveOp::Echo(EchoInput {
                value: "v".into(),
                target: EchoTarget::Field { element: ElementRef::new("@e2") },
                append: false,
            }),
            PrimitiveOp::Rm(RmInput {
                target: RmTarget::EnvScope { scope: EnvScope::Cookie },
            }),
            PrimitiveOp::Click(ClickInput { element: ElementRef::new("@e3") }),
            PrimitiveOp::Submit(SubmitInput { form: ElementRef::new("@e4") }),
            PrimitiveOp::Wget(WgetInput { target: WgetTarget::Url { url: u("https://example.com/x.png") } }),
            PrimitiveOp::Wait(WaitInput {
                condition: WaitCondition::Sleep { ms: 100 },
                timeout_ms: 1000,
            }),
            PrimitiveOp::Screenshot(ScreenshotInput { element: None }),
            PrimitiveOp::Eval(EvalInput { source: "1+1".into() }),
            PrimitiveOp::Diff(DiffInput {
                before: SnapshotId::new("@s1"),
                after: SnapshotId::new("@s2"),
            }),
        ];
        assert_eq!(ops.len(), 15, "exactly fifteen terminal primitives per ADR 0010");

        for op in &ops {
            assert_eq!(&rt(op), op);
        }
    }

    #[test]
    fn op_names_in_json_match_terminal_command_names() {
        // The whole point of ADR 0010 is that the op tag IS the shell command
        // name. Verify every variant.
        let pairs = [
            (PrimitiveOp::Pwd(PwdInput {}), "pwd"),
            (PrimitiveOp::Ls(LsInput { target: LsTarget::Page }), "ls"),
            (PrimitiveOp::Cd(CdInput { target: CdTarget::Back }), "cd"),
            (
                PrimitiveOp::Cat(CatInput {
                    target: CatTarget::Element { element: ElementRef::new("@e1"), attr: "text".into() },
                }),
                "cat",
            ),
            (
                PrimitiveOp::Find(FindInput { predicate: FindPredicate::Role { role: "link".into() } }),
                "find",
            ),
            (
                PrimitiveOp::Grep(GrepInput { pattern: "x".into(), ignore_case: false, within: None }),
                "grep",
            ),
            (
                PrimitiveOp::Echo(EchoInput {
                    value: "v".into(),
                    target: EchoTarget::Field { element: ElementRef::new("@e1") },
                    append: false,
                }),
                "echo",
            ),
            (
                PrimitiveOp::Rm(RmInput {
                    target: RmTarget::Field { element: ElementRef::new("@e1") },
                }),
                "rm",
            ),
            (PrimitiveOp::Click(ClickInput { element: ElementRef::new("@e1") }), "click"),
            (PrimitiveOp::Submit(SubmitInput { form: ElementRef::new("@e1") }), "submit"),
            (
                PrimitiveOp::Wget(WgetInput { target: WgetTarget::Url { url: u("https://example.com/") } }),
                "wget",
            ),
            (
                PrimitiveOp::Wait(WaitInput {
                    condition: WaitCondition::Sleep { ms: 1 },
                    timeout_ms: 10,
                }),
                "wait",
            ),
            (PrimitiveOp::Screenshot(ScreenshotInput { element: None }), "screenshot"),
            (PrimitiveOp::Eval(EvalInput { source: "1".into() }), "eval"),
            (
                PrimitiveOp::Diff(DiffInput {
                    before: SnapshotId::new("@s1"),
                    after: SnapshotId::new("@s2"),
                }),
                "diff",
            ),
        ];
        for (op, name) in &pairs {
            let json = serde_json::to_value(op).unwrap();
            assert_eq!(json["op"], *name, "op tag mismatch for {op:?}");
        }
    }

    #[test]
    fn trace_serializes_as_json_array() {
        let trace: Trace = vec![
            PrimitiveOp::Pwd(PwdInput {}),
            PrimitiveOp::Cd(CdInput { target: CdTarget::Url { url: u("https://example.com/") } }),
            PrimitiveOp::Ls(LsInput { target: LsTarget::Page }),
        ];
        let json = serde_json::to_value(&trace).unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 3);
        assert_eq!(json[0]["op"], "pwd");
        assert_eq!(json[1]["op"], "cd");
        assert_eq!(json[2]["op"], "ls");
    }

    #[test]
    fn env_path_variants_roundtrip() {
        for path in [
            EnvPath::Cookie { name: "sid".into() },
            EnvPath::StorageLocal { key: "k".into() },
            EnvPath::StorageSession { key: "k".into() },
        ] {
            assert_eq!(rt(&path), path);
        }
    }

    #[test]
    fn primitive_result_uses_same_op_tag_as_op() {
        let res = PrimitiveResult::Cd(CdOutput { url: u("https://example.com/") });
        let json = serde_json::to_value(&res).unwrap();
        assert_eq!(json["op"], "cd");
        assert_eq!(json["url"], "https://example.com/");
    }

    #[test]
    fn element_ref_and_snapshot_id_display() {
        assert_eq!(ElementRef::new("@e3").to_string(), "@e3");
        assert_eq!(SnapshotId::new("@s7").0, "@s7");
    }

    #[test]
    fn error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    // --- execution against a dummy engine ---

    struct DummyEngine;
    struct DummyPage(Url);

    impl heso_engine_api::Page for DummyPage {
        fn url(&self) -> &Url {
            &self.0
        }
        async fn text(&self) -> heso_core::Result<String> {
            Err(heso_core::Error::NotImplemented("DummyPage::text"))
        }
    }

    impl EngineApi for DummyEngine {
        type Page = DummyPage;
        async fn open(&self, url: &Url) -> heso_core::Result<Self::Page> {
            Ok(DummyPage(url.clone()))
        }
    }

    #[tokio::test]
    async fn cd_url_delegates_to_engine_open() {
        let out = cd(
            &DummyEngine,
            &CdInput { target: CdTarget::Url { url: u("https://example.com/foo") } },
        )
        .await
        .unwrap();
        assert_eq!(out.url.as_str(), "https://example.com/foo");
    }

    #[tokio::test]
    async fn execute_dispatches_cd_to_engine() {
        let op = PrimitiveOp::Cd(CdInput {
            target: CdTarget::Url { url: u("https://example.com/bar") },
        });
        match execute(&DummyEngine, &op).await.unwrap() {
            PrimitiveResult::Cd(out) => assert_eq!(out.url.as_str(), "https://example.com/bar"),
            other => panic!("wrong result variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cd_element_back_previous_all_return_not_implemented() {
        for target in [
            CdTarget::Element { element: ElementRef::new("@e1") },
            CdTarget::Back,
            CdTarget::Previous,
        ] {
            let res = cd(&DummyEngine, &CdInput { target }).await;
            assert!(matches!(res.unwrap_err(), Error::NotImplemented(_)));
        }
    }

    #[tokio::test]
    async fn unimplemented_primitives_return_not_implemented_error() {
        let e = DummyEngine;
        let ops = [
            PrimitiveOp::Pwd(PwdInput {}),
            PrimitiveOp::Ls(LsInput { target: LsTarget::Page }),
            PrimitiveOp::Cat(CatInput {
                target: CatTarget::Element { element: ElementRef::new("@e1"), attr: "text".into() },
            }),
            PrimitiveOp::Find(FindInput { predicate: FindPredicate::Role { role: "link".into() } }),
            PrimitiveOp::Grep(GrepInput { pattern: "x".into(), ignore_case: false, within: None }),
            PrimitiveOp::Echo(EchoInput {
                value: "v".into(),
                target: EchoTarget::Field { element: ElementRef::new("@e1") },
                append: false,
            }),
            PrimitiveOp::Rm(RmInput {
                target: RmTarget::Field { element: ElementRef::new("@e1") },
            }),
            PrimitiveOp::Click(ClickInput { element: ElementRef::new("@e1") }),
            PrimitiveOp::Submit(SubmitInput { form: ElementRef::new("@e1") }),
            PrimitiveOp::Wget(WgetInput { target: WgetTarget::Url { url: u("https://example.com/") } }),
            PrimitiveOp::Wait(WaitInput {
                condition: WaitCondition::Sleep { ms: 1 },
                timeout_ms: 10,
            }),
            PrimitiveOp::Screenshot(ScreenshotInput { element: None }),
            PrimitiveOp::Eval(EvalInput { source: "1".into() }),
            PrimitiveOp::Diff(DiffInput {
                before: SnapshotId::new("@s1"),
                after: SnapshotId::new("@s2"),
            }),
        ];
        for op in &ops {
            match execute(&e, op).await.unwrap_err() {
                Error::NotImplemented(_) => {}
                other => panic!("expected NotImplemented for {op:?}, got {other:?}"),
            }
        }
    }
}
