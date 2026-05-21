# fix-06-jquery-airgap — kernel.org jQuery + MDN airgap.js

Branch: `fix-06-jquery-airgap`
Worktree: `C:\Users\Akshay\Documents\projects\heso\.claude\worktrees\agent-adc130cf9265d9a04`

Two real-page crashes were blocking `heso read` on common documentation sites. Both have the same root shape: a third-party JavaScript module captures *prototype-side* method references at module-init time and then calls them later — heso's DOM exposed instance-side data properties where the spec requires accessor descriptors on the prototype, so the capture step returned `undefined` and the later call crashed.

## Failure 1: kernel.org jQuery 3.6 (Sphinx-bundled)

Repro:

```
./target/release/heso.exe read https://www.kernel.org/doc/html/latest/
```

Before (`failed_scripts` = 2):

```
https://www.kernel.org/doc/html/latest/_static/jquery.js
  :: cannot read property 'createElement' of undefined
       at ce (eval_script:2:7289)
       at <anonymous> (eval_script:2:23146)
       at <anonymous> (eval_script:2:23840)
       at <anonymous> (eval_script:2:222)
       at <eval> (eval_script:2:239)

https://www.kernel.org/doc/html/latest/_static/_sphinx_javascript_frameworks_compat.js
  :: jQuery is not defined
       at <eval> (eval_script:23:1)
```

After (`failed_scripts` = 0).

### Root cause

jQuery 3.6's Sizzle selector engine has a `setDocument(e)` function that
caches the document on a closed-over `C` variable. The relevant logic
(de-minified) is:

```js
T = se.setDocument = function (e) {
    var r = e ? (e.ownerDocument || e) : p;  // p = preferred document
    return r != C
        && 9 === r.nodeType                    // <-- the bug
        && r.documentElement
        && (a = (C = r).documentElement,
            E = !i(C),
            // ... register listeners, run feature detects ...
        );
};
```

heso's `Document` rquickjs class did **not** expose a `nodeType` getter,
so `r.nodeType` was `undefined`, `9 === undefined` was `false`, and the
whole `&&` chain short-circuited. `C` stayed `undefined`. Later, when
the IIFE ran its feature-detects via the `ce(fn)` helper:

```js
function ce(e) {
    var t = C.createElement("fieldset");  // <-- crash with C undefined
    try { return !!e(t); } catch (e) { return false; }
    finally { t.parentNode && t.parentNode.removeChild(t); t = null; }
}
```

— the very first `ce(...)` call threw "cannot read property 'createElement' of undefined" and the entire jQuery IIFE died at column 23146 (the call site).

The dependent `_sphinx_javascript_frameworks_compat.js` then failed with `jQuery is not defined` because `$` / `jQuery` were never published.

### The fix

`Document::nodeType` getter returns `9` (the `DOCUMENT_NODE` constant from WHATWG DOM §4.4). Five sibling getters were added in the same commit to round out the document prototype surface so subsequent jQuery branches don't hit the next missing property:

- `Document::nodeType` → `9`
- `Document::nodeName` → `"#document"`
- `Document::ownerDocument` → `null`
- `Document::defaultView` → `ctx.globals()` (= `window`)
- `Document::createComment(data)` → orphan comment node via `dom_query::Tree::create_node(NodeData::Comment{...})`
- `Document::implementation` → a `DOMImplementation`-shaped POJO with `createHTMLDocument(title?)` returning a real orphan `<html>/<head>/<body>` subtree

The `createHTMLDocument` approximation matters for jQuery's
`y.createHTMLDocument = ((_t = E.implementation.createHTMLDocument("").body).innerHTML = "<form></form><form></form>", 2 === _t.childNodes.length)` feature-detect — we attempted a `<template>`-backed approach first but `html5ever` strips `<html>`/`<head>`/`<body>` from template fragment children, so the final approach builds orphan `<html>/<head>/<body>` elements via `document.createElement` directly.

## Failure 2: MDN's Transcend airgap.js

Repro:

```
./target/release/heso.exe read https://developer.mozilla.org/en-US/docs/Web/JavaScript
```

Before (`failed_scripts` = 1):

```
https://transcend-cdn.com/cm/d556c3a1-e57c-4bdf-a490-390a1aebf6dd/airgap.js
  :: not a function
       at <anonymous> (eval_script:5:3968)
       at map (native)
       at <anonymous> (eval_script:5:3960)
       at <anonymous> (eval_script:19:1)
       at <eval> (eval_script:20:1)
```

After (`failed_scripts` = 0).

### Root cause

Transcend's airgap.js (the consent-management bundle vendored on every MDN page) opens with a long destructure of well-known globals and prototype methods. The crash was at column 3960:

```js
var ia = [[], new Ne, new ao, ""];                 // [array, Set, Map, string]
h && s(Fc, ia, UC, Q.createElement("_").classList, GC);  // push classList in
// First pass: destructure Symbol.iterator from each.
var [HC, FC, VC, qC, WC, KC, BC] = ia.map(({[$s]: e}) => e);
// Second pass: call e[Symbol.iterator]() and take .next.
var [YC, Gu, $C, zC, jC, XC, QC] = ia.map(e => e && e[$s]().next);
//                                                  ^ "not a function"
```

heso's `DomTokenList` (the rquickjs class backing `element.classList`)
had no `Symbol.iterator` member. The first destructure yielded `undefined`
for that slot; the second `map` callback then evaluated
`classListInstance[Symbol.iterator]()` — calling `undefined` — and threw
"not a function". The bundle died before publishing any of its globals.

### The fix

Patched `DOMTokenList.prototype[Symbol.iterator]` in `BROWSER_APIS_BOOTSTRAP`. The shim returns an iterator-shaped object whose `.next` is a real function — enough to satisfy airgap's capture-and-call pattern even though the actual class-token enumeration is a best-effort approximation.

The same commit closes ~40 other potential follow-on crashes the airgap bundle would have hit downstream: missing constructors (`MessagePort`, `History`, `Response`, `TextEncoder`, `Intl.DateTimeFormat`, etc.) and missing accessor descriptors on the prototypes of `Element`, `Document`, `Navigator`, `HTMLCollection`, `XMLHttpRequest`, etc.

The most invasive piece is the permissive `EventTarget.prototype` override: airgap captures `EventTarget.prototype.addEventListener` and calls it with arbitrary singletons as `this` (cookieStore, performance). heso's rquickjs `EventTarget` class enforced a strict `this`-must-be-a-real-EventTarget guard that rejected POJOs with "Error converting from js 'object' into type 'EventTarget'". We replaced the prototype methods with JS-only duck-typed shims that store listeners as own-property maps on the receiver — preserves spec semantics (dedupe by `(callback, capture)`, throw on non-Event arg in `dispatchEvent`, honor `stopImmediatePropagation`) while accepting any object as `this`. Element / Document have their own prototype methods that shadow the EventTarget version, so DOM-node dispatch is unaffected.

`makeEventSubclass(name, defaultMembers)` was rewritten to use
`Reflect.construct(Event, args, ctor)` rather than `Event.call(this, ...)`
because the rquickjs `Event` class also rejects `.call`-on-non-Event
("Error converting from js 'object' into type 'Event'");
`Reflect.construct` allocates a real Event with the subclass's prototype
reparented, so `new SubmitEvent('submit', {submitter})` returns a real
Event whose `event instanceof Event` is true (load-bearing for the
permissive dispatchEvent's duck check) and whose `event.constructor.name`
is `"SubmitEvent"`.

## Files & line counts

| File | Δ |
|------|---|
| `crates/heso-engine-js/src/dom.rs` | +136 |
| `crates/heso-engine-js/src/engine.rs` | +2007 / -32 |
| `crates/heso-engine-js/tests/jquery_airgap_smoke.rs` (new) | +470 |

## Tests

- `crates/heso-engine-js/tests/jquery_airgap_smoke.rs`: 18 tests, all pass. Covers `Document::nodeType`, `nodeName`, `ownerDocument`, `defaultView`, `createComment`, `implementation`, the inline jQuery setDocument simulation, the inline airgap module-init simulation, MessagePort/Intl/SubmitEvent constructibility, Element prototype getters (baseURI / ownerDocument / namespaceURI / isConnected), Navigator.prototype.languages accessor, EventTarget permissive call-on-POJO, and the SkipExternal inline-script policy smoke test.
- `crates/heso-engine-js` lib tests: **259 passed, 0 failed**.
- `crates/heso-engine-js` integration tests (excluding `jquery_airgap_smoke`): **all green** (~430+ tests across 23 files).
- `crates/heso-core`: 13/13. `crates/heso-engine-fetch`: 166/166.

Workspace-wide `cargo test --workspace` was attempted but ran the host machine out of disk space (88+ test executables, ~10 MB each in release). The per-crate suites that exercise heso-engine-js's surface all pass.

## Before/after `failed_scripts`

| URL | Before | After |
|-----|--------|-------|
| `https://www.kernel.org/doc/html/latest/` | 2 (jquery.js, sphinx-compat) | **0** |
| `https://developer.mozilla.org/en-US/docs/Web/JavaScript` | 1 (airgap.js) | **0** |

## Branch + commits

Branch: `fix-06-jquery-airgap` (worktree base: `f4c71b7`)

| SHA | Title |
|-----|-------|
| `748b8ab` | engine-js: jQuery + airgap.js stub burst (bug-report 06) |
| `9c1f70c` | engine-js: spec-correct EventTarget shim + Reflect.construct events + smoke test |
| `d55c3fb` | engine-js: XHR / Request prototype accessors read via Reflect.getOwnPropertyDescriptor |
