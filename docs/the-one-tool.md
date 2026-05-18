# `heso.run` — the one tool agents use

> Public agent-facing surface. The only tool. Stable contract.
>
> See ADR 0009 for the why. This doc is the *what*.

## Shape

```
heso.run(
    start_url: Url,
    request:   String,        # plain English: what the agent wants
    options?:  Options,
) -> Result
```

### `Options` (all optional, with sensible defaults)

```
session_id:        String?              # continue an existing session (cookies, identity, history)
seed:              u64                  # for determinism; default = derived from request hash
mode:              "deterministic"     # default
                   | "recording"
                   | "live"
credentials:       Map<String, Value>?  # named creds the agent makes available (e.g. {github_token: ...})
timeout_ms:        u64                  # default 30000
verbose:           bool                 # include intermediate steps in result.receipt; default false
planner_version:   String?              # pin a specific planner (default: latest)
```

### `Result`

```
status:  "ok" | "need_clarification" | "failed" | "partial"

data:    Value          # the answer, shape depends on the request
receipt: Receipt        # what heso did, signed by the agent's key
cost:    Cost           # bytes downloaded, cpu_ms, wall_ms, planner tokens

# only present when status != "ok":
clarification: String?  # heso asks the agent a follow-up question
error:         Error?   # structured error with retry hints
```

### `Receipt`

```
trace:         [PrimitiveOp]   # the exact sequence of internal ops heso ran
pages_seen:    [ContentHash]   # content-addressable hash of every page fetched
planner_id:    String          # which planner produced this trace
seed:          u64
mode:          Mode
signed:        Signature       # Ed25519 over a canonical encoding of the above
```

Anyone with the receipt + the recorded network trace can replay the run and verify byte-identical output.

## What the agent sees, end-to-end

### Read

```
heso.run("https://news.ycombinator.com",
         "top 10 stories with title, url, score, comments")

→ status: ok
  data: [
    {title: "...", url: "...", score: 412, comments: 87},
    ...
  ]
  receipt: {...}
  cost: {bytes: 38421, cpu_ms: 12, wall_ms: 410}
```

### Read with filter / sort

```
heso.run("https://www.amazon.com/s?k=laptop",
         "find the cheapest laptop under $1000, return URL, name, price, specs")

→ status: ok
  data: {url: "...", name: "...", price: 849, specs: {...}}
```

### Submit a form

```
heso.run("https://example.com/signup",
         "sign up with email=akku@gmail.com and password from credentials.signup_pw",
         options: {credentials: {signup_pw: "..."}})

→ status: ok
  data: {account_id: "...", welcome_message: "..."}
```

### Multi-page

```
heso.run("https://docs.example.com",
         "find every page that mentions 'rate limit' and summarize what each says about limits")

→ status: ok
  data: [
    {url: "...", summary: "..."},
    ...
  ]
  cost: {bytes: ..., wall_ms: 3200}  # heso paginated
```

### Watch (subscription)

```
heso.run("https://example.com/product/123",
         "watch this page; tell me when price changes or stock drops below 5")

→ status: ok
  data: <stream of events>
  receipt: <ongoing>
```

### Clarification

When the request is ambiguous, heso asks back rather than guessing:

```
heso.run("https://amazon.com/s?k=laptop",
         "get me the best one")

→ status: need_clarification
  clarification: "Best by which criterion? (price, rating, recency, sales rank)"
```

The agent responds with another `heso.run` carrying the clarification:

```
heso.run(
    session_id: <from previous receipt>,
    request: "best by rating"
)
```

### Failure

When heso can't fulfill the request, it returns what it tried:

```
→ status: failed
  error: {
    reason: "no_price_field_found",
    tried: [".price", "[data-price]", "regex $\\d+ on visible text"],
    page_was: <ContentHash>,
    suggestion: "page may have changed structure; try with verbose=true to see DOM"
  }
```

## What the agent does NOT see

- The DOM
- CSS selectors or XPath
- The trace, by default (only when `verbose: true` or via `result.receipt.trace`)
- Engine internals
- The planner's reasoning
- Specific primitive operations the engine supports

## Stability guarantees

- The signature `heso.run(start_url, request, options) → Result` is stable across heso versions. New options can be added; existing ones don't change meaning.
- `data` shapes are *responsive to the request* — they're not stable across requests, only within a single request's intent.
- `receipt.trace` schema is stable; specific operation types may be added in new versions but existing ones don't change.
- Planner versions are pinned in receipts. Old receipts always replay against their original planner version.

## What this doc doesn't cover

- The internal primitive operations the planner emits. See [`research/browser-engines/agent-first-design.md`](../research/browser-engines/agent-first-design.md) for that surface.
- How the planner works. See [`docs/planner.md`](planner.md) once it exists.
- The skill MD that teaches request phrasing. See [`skills/heso/SKILL.md`](../skills/heso/SKILL.md).
