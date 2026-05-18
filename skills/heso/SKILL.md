---
name: heso
description: Headless browser for AI agents. Use to fetch, extract, submit forms, navigate, or watch web pages. One tool — `heso.run(start_url, request)` — takes a starting URL and a plain-English request, returns structured data plus a signed audit trail. Triggers on any task involving web pages, scraping, form submission, web navigation, monitoring a site, or extracting structured data from a URL.
---

# heso — one tool, plain English

heso is your headless browser. You have **one tool**:

```
heso.run(start_url, request, options?)
  → { status, data, receipt, cost }
```

You tell heso *what you want, in plain English*, and give it a URL to start from. heso plans the steps, runs them deterministically, and gives you back the answer.

Internally, heso treats the page like a Unix shell — every step in `receipt.trace` is a terminal command (`pwd`, `ls`, `cd`, `cat`, `find`, `grep`, `echo`, `rm`, `click`, `submit`, `wget`, `wait`, `screenshot`, `eval`, `diff`). You don't write those yourself; the planner emits them. But knowing this mental model helps you phrase requests: if you can describe the task as "walk to this page, look around, read these things, fill these fields, submit" — that's exactly what heso does under the hood.

## When to use this skill

Trigger heso whenever a task involves:
- Reading a webpage (extracting article text, headlines, prices, listings, anything)
- Submitting a form (signup, login, search, contact)
- Following links across multiple pages to gather data
- Watching a page for changes
- Anything else that involves URLs and structured results

If the task does not involve the web, do not use heso.

## How to phrase a request

Phrase requests like you'd phrase them to a competent human assistant. Be specific about what fields you want. Use natural language.

**Good:**
- "Top 10 Hacker News stories with title, URL, score, and comment count."
- "Find the cheapest laptop under $1000 on this page and return URL, name, price, and specs."
- "Sign up with email = akku@gmail.com and password from credentials.signup_pw, then return the new account ID."
- "Follow every link from this page that points to a blog post, and return the title and publish date of each."
- "Watch this product page; notify me when the price drops below $50 or stock goes under 5 units."

**Bad (heso has to guess):**
- "Get the data." (which data?)
- "Find a good laptop." (good by what? price? rating?)
- "Click the button." (heso doesn't expose clicks — describe the *goal*)
- "Get all the elements." (which elements?)

If you write an ambiguous request, heso will come back with `status: need_clarification` and a question. Reply with another `heso.run` that adds detail.

## What you get back

```
{
  "status": "ok",
  "data": <whatever you asked for, shaped sensibly>,
  "receipt": {
    "trace": [...],          // every internal step heso took
    "pages_seen": [...],     // content hashes
    "signed": "<sig>"        // Ed25519 over the above
  },
  "cost": {
    "bytes": 38421,
    "cpu_ms": 12,
    "wall_ms": 410
  }
}
```

`data` is what you asked for. `receipt` is the audit trail — it's signed by your session key so you can prove later exactly what was done. `cost` lets you budget across many calls.

## Status values

- `"ok"` — got what you asked for; `data` is populated
- `"need_clarification"` — your request was ambiguous; reply with another `heso.run` that clarifies
- `"failed"` — heso tried but couldn't; check `error.reason`, `error.tried`, `error.suggestion`
- `"partial"` — got some of what you asked for; rest is in `error`

## Examples

### Extract a list

```
heso.run(
  start_url = "https://news.ycombinator.com",
  request   = "top 10 stories with title, URL, score, and comment count"
)
```

### Find one thing

```
heso.run(
  start_url = "https://www.example.com/products?category=laptops",
  request   = "find the cheapest laptop under $1000; return URL, name, price, and specs"
)
```

### Submit a form

```
heso.run(
  start_url   = "https://example.com/signup",
  request     = "sign up with email=akku@gmail.com and password from credentials.signup_pw",
  options     = { credentials: { signup_pw: "..." } }
)
```

### Cross-page navigation

```
heso.run(
  start_url = "https://docs.example.com",
  request   = "find every page in the docs that mentions 'rate limit' and summarize what each says about the limits"
)
```

### Continue a session

```
# first call
r1 = heso.run("https://example.com/login",
              "log in with email=... password=...")

# subsequent calls use the session_id from r1.receipt
r2 = heso.run("https://example.com/dashboard",
              "list all my projects with name and last-modified date",
              options = { session_id: r1.receipt.session_id })
```

### Watch (subscription)

```
heso.run(
  start_url = "https://example.com/product/123",
  request   = "watch this page; emit an event when the price drops below 50 or stock < 5"
)
# `data` is a stream of events the agent reads as they arrive
```

## What you should NOT do

- **Do not** ask heso to "click X" or "fill field Y." There is no click. Describe the goal: "submit the search form with q=rust" not "click the search box, type rust, press enter."
- **Do not** include CSS selectors or XPath in your request. heso decides those internally.
- **Do not** ask for screenshots unless you genuinely need one (rare for an LLM). heso prefers text-and-data, and screenshots cost ~50× more tokens.
- **Do not** chain many `heso.run` calls when one would do. Prefer a single request that describes the full goal (heso paginates and joins internally for free).
- **Do not** put credentials in the `request` string. Pass them via `options.credentials` and refer to them by name.

## Determinism (you get it for free)

Every call has a `seed`. With the same seed and the same recorded network trace, two calls produce byte-identical results. heso defaults `mode: "deterministic"` — clocks, RNG, network, and rendering are all controlled. If you need real-world entropy (real wall clock for a benchmark, real RNG for a security demo), set `mode: "live"` — but doing so disables receipt signing.

## Cost awareness

`result.cost` reports bytes/CPU/wall-time/planner-tokens for every call. heso pages large lists internally; if you ask for "top 10," heso fetches just enough pages. Don't loop in your code asking for "next page" — describe the full set you want in one request.

## Errors and recovery

If `status == "failed"`, the response includes:
- `error.reason` — short token: `no_field_found`, `auth_required`, `page_changed`, `rate_limited`, etc.
- `error.tried` — what heso tried before giving up
- `error.suggestion` — what to try next
- `error.page_was` — content hash of the page that failed (lets you fetch the same page later for debugging)

Common recoveries:
- `auth_required` → call again with `options.credentials`
- `page_changed` → call again with a more specific request
- `rate_limited` → wait, then retry with `options.session_id`
- `no_field_found` → restructure the request, be more specific about field names

## Versioning

`receipt.planner_id` records which planner version produced this trace. Old receipts always replay against their original planner version. Pin a specific planner with `options.planner_version` for stable behavior across agent sessions.

---

That's the whole skill. One tool. Plain English. heso does the rest.
