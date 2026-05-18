# The Browser Rendering Pipeline

**Topic:** Stages from HTML bytes to on-screen pixels, with notes on what an agent-first browser can skip
**Last updated:** 2026-05-17
**Status:** initial research

## Summary

A browser engine moves bytes through seven stages: parse HTML, build DOM, parse CSS, build CSSOM, compute style, lay out, paint, composite. Each stage has a cost profile and a "must-finish-before" dependency on the next. For heso, the load-bearing observation is that **paint and composite are the most expensive stages and the easiest to make optional** when the consumer is an LLM that just wants a structured DOM/AX snapshot.

## Stage-by-stage

### 1. HTML parsing → DOM tree
The parser is a streaming tokenizer (HTML5 spec, WHATWG). Servo uses `html5ever`, a Rust implementation of the spec — same crate is usable standalone in WASM if heso ever needs a DOM-only mode. Cost is mostly linear in bytes; the surprise costs come from `document.write` and synchronous script injection, both of which stall parsing.

**Agent-relevant note:** the DOM is the cheapest faithful representation of a page. Many agent flows can stop here.

### 2. CSS parsing → CSSOM
CSS is parsed into a stylesheet object model (CSSOM). Servo's `style` crate (aka Stylo, also embedded into Firefox/Gecko) does this in parallel — one of the few places parallelism is a clean win. External stylesheets are render-blocking by default; an agent-first mode can lie and treat all CSS as `media="print"` to avoid the wait.

### 3. Style calculation (matching + cascade)
Each DOM node is matched against every selector to produce a computed style. This is `O(nodes × selectors)` in the naive case; real engines use Bloom filters on ancestor classes. Stylo parallelizes this across CPU cores using `rayon` — Servo's main perf moat over WebKit.

### 4. Layout (box generation, line breaking, positioning)
Builds two trees:
- **Box tree** — formal CSS box model output (block, inline, flex, grid containers)
- **Fragment tree** — actual placed rectangles after line breaking, hyphenation, flexbox/grid resolution

This is where reflow lives. Layout is the most algorithmically intricate stage: float resolution, intrinsic sizing, table layout, `position: sticky`, subpixel rounding. Servo's layout has been rewritten multiple times; the current one (Layout 2020+) is being optimized monthly per the Servo blog.

**Agent-relevant note:** layout produces *positions*, which agents almost never need. If heso has a strict "no coordinate-based actions" rule, layout could be disabled or deferred for many DOM-only operations. But CSS-driven visibility (`display: none`, `visibility: hidden`) is resolved during style/layout, so an LLM that's supposed to "see what a human sees" still needs at least style.

### 5. Paint → display list
The fragment tree becomes a serialized **display list** of drawing commands (rectangles, glyph runs, images, gradients, shadows). Servo emits a WebRender display list; Ladybird emits commands consumed by LibGfx. The display list is portable — heso could intercept it for telemetry, dump it to JSON, or skip it entirely.

### 6. Composite → GPU layers
WebRender (Servo + Firefox Gecko) ships the display list straight to the GPU and rasterizes there. Chromium uses Skia with a CPU raster + GPU composite split. The GPU is where animation, scrolling, and transforms live cheaply. **Composite is the most skippable stage for headless agent runs.** Servo supports a `SoftwareRenderingContext` precisely for this.

### 7. Present
Pixels hit the framebuffer. For heso, this is `RenderingContext::read_to_image()` returning an `ImageBuffer<Rgba<u8>, Vec<u8>>` instead of a window swap.

## What an agent-first browser can defer

| Stage | Skip? | Why |
|---|---|---|
| Parse HTML | Never | DOM is the minimum viable representation |
| Parse CSS | Conditionally | Skip if mode = `dom-only` |
| Style | Conditionally | Need at least for visibility/`hidden`/`display:none` |
| Layout | Conditionally | Skip if no coordinate actions and no screenshot |
| Paint | Often | Only needed for screenshots / visual diff |
| Composite | Often | Only needed for visual output to a human or screenshot |
| Present | Often | Only when caller requested pixels |

heso should ship modes like `dom`, `dom+style`, `dom+style+layout`, `full` and pick the cheapest one per call.

## References

- [How browsers work — MDN](https://developer.mozilla.org/en-US/docs/Web/Performance/Guides/How_browsers_work)
- [Critical rendering path — MDN](https://developer.mozilla.org/en-US/docs/Web/Performance/Guides/Critical_rendering_path)
- [Servo architecture overview (Servo Book)](https://book.servo.org/design-documentation/architecture.html)
- [WebRender — Mozilla / Stylo embedding in Gecko (Servo blog)](https://servo.org/blog/2024/04/15/spidermonkey/)
