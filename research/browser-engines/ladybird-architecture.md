# Ladybird Browser Architecture

**Topic:** Ladybird's clean-room engine and why it's a v2 candidate for heso
**Last updated:** 2026-05-17
**Status:** initial research

## Summary

Ladybird is a from-scratch browser engine — no Blink, WebKit, or Gecko code — originally derived from SerenityOS and now independent. It uses a multi-process architecture (Browser, WebContent, RequestServer, ImageDecoder) and is migrating from C++ to Rust as it stabilizes. Alpha ships in 2026, beta 2027, stable 2028. For heso, Ladybird is plausible as a v2 engine once it stabilizes and exposes an embedding API, but not viable today.

## Library boundary

Ladybird inherits the SerenityOS Lib* layout:

| Library | Purpose |
|---|---|
| `LibWeb` | HTML, CSS, DOM, layout, paint |
| `LibJS` | JavaScript engine (their own, not V8/SpiderMonkey) |
| `LibWasm` | WebAssembly |
| `LibCrypto` / `LibTLS` | Crypto primitives and TLS |
| `LibHTTP` | HTTP/1.1 client |
| `LibGfx` | 2D graphics, image decoding, rendering |
| `LibUnicode` | Unicode + locale |
| `LibMedia` | Audio/video |
| `LibCore` | Event loop, OS abstraction |
| `LibIPC` | Inter-process messaging |

This is a much cleaner separation than Servo's. Each Lib* is a candidate for reuse independently — useful if heso ever wanted a "rendering-only" or "JS-only" embedding.

## Process model

Ladybird runs four kinds of processes:

- **Browser (UI)** — main process, owns the window and tab strip
- **WebContent** — one per tab. Runs LibWeb + LibJS in a sandbox. Crashes here don't kill the browser.
- **RequestServer** — handles all network. Centralized makes cookies/cache/proxy management uniform.
- **ImageDecoder** — out-of-process image decode. Defense against decoder-bug exploits (a real, common CVE class).

IPC pattern:
- Browser ↔ WebContent: input events down, painted bitmaps up
- WebContent ↔ RequestServer: resource fetch requests
- WebContent ↔ ImageDecoder: encoded bytes in, decoded bitmaps out

The "painted bitmaps up" detail is notable: Ladybird ships rasterized output between processes, not display lists. Higher IPC cost but simpler. For an agent-first browser this is fine — agents read bitmaps rarely.

## Language transition

Ladybird is roughly 55% C++, with HTML/JS/Rust/Python making up the rest. The team has publicly committed to Rust as the C++ successor and is using "AI-assisted transition" to incrementally port subsystems. Third-party libraries are allowed for image/audio/video/encryption/graphics.

For heso's standpoint: by the time Ladybird is a credible v2 engine (2027–2028), more of it will be Rust, which makes Rust-side embedding less awkward. Today, embedding it from Rust would mean a C++ FFI layer, defeating much of heso's "all-Rust" appeal.

## What Ladybird does well

- **Clean process boundaries.** Image decoder isolation is a real win; Servo doesn't do this.
- **Spec-driven development.** The team works directly from WHATWG/W3C specs.
- **No fork debt.** No 20 years of Chromium/WebKit baggage.
- **Public roadmap.** Alpha 2026 / Beta 2027 / Stable 2028 is a clear commitment.

## Where Ladybird is behind (today, May 2026)

- **Standards coverage.** Pre-alpha for general use. Many sites won't render. Servo is more mature on this front *for the things both engines implement.*
- **No embedding API.** Ladybird is built as a browser, not a library. There's no `cargo add ladybird`. Anyone embedding it today is doing a soft fork.
- **Performance.** Not optimized. Rust subsystems are being added, not the result of a year of profiling.
- **Sandboxing.** Pre-alpha. Not at parity with Chromium.

## Why Ladybird is a v2 candidate for heso

The `EngineApi` trait pattern in heso exists precisely to make engine swaps cheap. Ladybird fits a future where:

- Servo's per-platform build pain (SpiderMonkey, C++ toolchain) becomes a real cost center
- Ladybird ships a stable embedding API (likely 2027+)
- Ladybird's Rust port crosses a threshold where it can be wrapped without C++ FFI
- Ladybird's standards coverage catches up to a level where it can run the agent's target sites

None of those are true today. Track the Ladybird blog quarterly; revisit the decision when 2027 alpha ships.

## References

- [Ladybird homepage](https://ladybird.org/)
- [LadybirdBrowser/ladybird (GitHub)](https://github.com/LadybirdBrowser/ladybird)
- [Ladybird architecture overview (Mintlify mirror)](https://www.mintlify.com/LadybirdBrowser/ladybird/architecture/overview)
- [Ladybird (web browser) — Wikipedia](https://en.wikipedia.org/wiki/Ladybird_(web_browser))
