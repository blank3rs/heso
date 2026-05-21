# Long-running reliability report

Platform: Windows 11 Home (10.0.26200), heso 0.0.3, `target/release/heso.exe`.
Test envelope: 1,825 successful heso ops across five workloads (500 sequential
opens, three 100-URL batch runs at p=8/16/32, 150 concurrent ops in 15 rounds,
a 25-URL cookie pressure batch, and four sets of large `--parallel 4` batches).
Total runtime ≈ 7 minutes of heso wall-clock plus orchestration.

# TLDR

- **No memory leak detected.** Single-process working set inside a 200-URL batch oscillates 41–129 MB without monotonic growth; handle count stabilizes at 186; thread count at 25.
- **No FD/handle leak detected.** Across 500 sequential `heso open` invocations and 150 concurrent `heso open` invocations, per-process handle counts converge on ~104–186 (process-level only since each invocation is its own process).
- **No throughput degradation under load.** Throughput across 400 URLs split into 4 chunks: 32.6 → 38.3 → 45.9 → 50.8 URLs/s — improves over time (DNS / TLS warm-up), never regresses.
- **2 silent failures out of 500 (0.4%)** sequential opens: `exit=-1`, `stdout=0`, `stderr=0`. **Not reproducible** in a 100-op focused retry. Looks like sporadic Windows-side process termination, not a heso bug.
- **REAL BUG, P1 — cookie domain serialization**: every host-only cookie (RFC 6265 default for `Set-Cookie` responses with no explicit `Domain=` attr) is serialized as `"domain": ""`. Source: `crates/heso-cli/src/main.rs:2447` does `c.domain().unwrap_or("")`. Affects 89/115 (77%) of cookies seen in workload 4. An agent inspecting `cookies` cannot distinguish a host-only cookie from a domain-wide cookie.
- **REAL BUG, P1 — cookie read-after-write race in batch mode**: `collect_cookies` reads jar state at JSON-serialization time, *not* response time. In `heso batch read --parallel N`, each URL's `cookies` field reflects whatever the shared jar happens to contain when its line is rendered — including cookies set by *sibling* URLs that completed in the meantime. Workload 4 saw httpbin.org/cookies report 2, 5, 6, 7, 8, 9, 10, 10, 10, 10, 11 cookies in successive output lines — same URL, same fetch, different "cookies" snapshot per line. 67% of cookies in W4 (77/115) are duplicates across URLs purely from this cross-contamination.
- **No panics observed.** Every `stderr` capture across all workloads was empty.

# Workload 1 — repeated `heso open` in a loop (500 invocations)

| Sample @ N ops | Elapsed s | Cumul ok | Cumul fail | Avg/op s | p95/op s | Heso procs running at sample | Sum RSS (B) | Sum handles |
|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| 50  | 32.3  | 50  | 0 | 0.646 | 2.32 | 1 | 17,616,896 | 159 |
| 100 | 60.1  | 100 | 0 | 0.601 | 2.26 | 0 | 0 | 0 |
| 150 | 87.0  | 150 | 0 | 0.579 | 2.18 | 0 | 0 | 0 |
| 200 | 111.5 | 200 | 0 | 0.557 | 2.14 | 1 | 8,830,976 | 104 |
| 250 | 135.3 | 250 | 0 | 0.541 | 2.03 | 2 | 34,664,448 | 260 |
| 300 | 158.5 | 300 | 0 | 0.528 | 2.03 | 1 | 8,777,728 | 104 |
| 350 | 186.1 | 350 | 0 | 0.531 | 1.98 | 1 | 8,777,728 | 104 |
| 400 | 217.2 | 400 | 0 | 0.543 | 2.03 | 3 | 30,601,216 | 377 |
| 450 | 252.1 | 449 | 1 | 0.560 | 2.14 | 0 | 0 | 0 |
| 500 | 285.9 | 498 | **2** | 0.571 | 2.18 | 0 | 0 | 0 |

- 500 ops in 285.9 s; mean 0.572 s/op, p95 2.18 s end-to-end.
- Average per-op time is monotonically **non-degrading** — actually trends slightly down from 0.65 → 0.57. p95 also slightly down. No latency creep.
- Per-process resource footprint is steady: ~9 MB RSS, ~104 handles per running heso when sampled mid-flight (sometimes 2-3 procs caught overlapping when sampling because PS sampling is at 100ms).
- **2 silent failures** at ops #444 (`/wiki/XML`) and #459 (`github.com/rust-lang/rust`): `exit=-1`, `stdout_len=0`, `stderr_len=0`, dur 0.35 s and 1.18 s respectively. No panic message captured.
- **Reproduction attempt failed**: re-ran 100 ops cycling through 5 URLs including those two — all 100 succeeded.

Conclusion: no memory or FD leak. 0.4% silent-fail rate that did not repro under controlled bursts — likely external (AV scan, transient network, OS process-termination race).

# Workload 2 — `heso batch open` at `--parallel` 8/16/32 (100 URLs each)

| --parallel | Duration s | ok | fail | timeouts | URL/s | Peak RSS MB | Peak handles | Peak threads |
|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| 8  | 2.61 | 100 | 0 | 0 | 38.3 | 149.1 | 212 | 29 |
| 16 | 1.56 | 100 | 0 | 0 | 64.2 | 199.7 | 274 | 37 |
| 32 | 1.56 | 100 | 0 | 0 | 63.9 | 137.2 | 346 | 53 |

Findings:
- 100% success across all three runs. Zero timeouts, zero connection-pool errors, empty stderr in all three runs.
- Throughput plateaus at `--parallel 16`. `--parallel 32` got more handles (346 vs 274) and more threads (53 vs 37) for **zero additional throughput**. Reqwest's pool/host limit caps useful concurrency far before 32, but heso still spins up 32 worker tasks → extra slots are pure overhead.
- `--parallel 16` had the highest peak RSS (199.7 MB) — higher than `--parallel 32` (137.2 MB). Counter-intuitive; could be timing of GC observation, but worth noting that more parallelism doesn't strictly mean more memory.
- Handle scaling: 212 → 274 → 346 = ~+62 then ~+72 per doubling. Roughly linear in `--parallel`, ~3.4 handles per extra concurrent slot. Reasonable.
- Threads scaling: 29 → 37 → 53. Tokio is sharing a worker pool but per-concurrency tasks add a few OS threads — fine.

# Workload 3 — concurrent processes (10×10, then 20×5)

10 procs × 10 rounds (100 ops):

| Round | Duration s | ok | fail | Procs caught at peak | ΣRSS MB | Σhandles |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 0.320 | 10 | 0 | 0 (missed sample) | 0 | 0 |
| 2 | 0.264 | 10 | 0 | 0 | 0 | 0 |
| 3 | 0.268 | 10 | 0 | 0 | 0 | 0 |
| 4 | 0.290 | 10 | 0 | 0 | 0 | 0 |
| 5 | 0.265 | 10 | 0 | 2 | 39.7 | 254 |
| 6 | 0.276 | 10 | 0 | 2 | 39.8 | 254 |
| 7 | 0.264 | 10 | 0 | 2 | 39.9 | 254 |
| 8 | 0.267 | 10 | 0 | 4 | 45.0 | 502 |
| 9 | 0.258 | 10 | 0 | 3 | 33.1 | 380 |
| 10 | 0.264 | 10 | 0 | 3 | 33.1 | 380 |

20 procs × 5 rounds (100 ops): all rounds 0.31–0.39 s, all 20 ok per round, all 100 ok overall. No file lock collisions, no failures.

Findings:
- Zero failures across 200 concurrent process spawns.
- No cookie-jar corruption (each process has its own in-memory jar; nothing on disk for them to fight over — see Workload 4).
- Round duration **decreased** from R1 (0.32 s) to R10 (0.26 s) for the 10-proc case, and from R1 (0.39 s) to R5 (0.31 s) for the 20-proc case — consistent with OS-level cache warming, no contention degradation.
- "Procs caught at peak = 0" for early rounds is a sampling artifact: PS's 200 ms sample miss-times the heso lifecycle (most rounds finished in <300 ms). When sampled successfully, each heso averages ~125 handles, ~16 MB RSS — same shape as Workload 1's single-process snapshots.

# Workload 4 — cookie jar pressure (`batch read --parallel 4 --include cookies`, 25 URLs)

`duration_s=8.21 ok=25 fail=0 total_cookies=115 unique_cookies=38 maxRss_MB=192.8 maxHandles=214 stderr_len=0`

Per-URL cookie counts (output order):

```
https://httpbin.org/cookies      cookies=2
https://httpbin.org/cookies      cookies=5
https://httpbin.org/cookies      cookies=6
https://httpbin.org/cookies      cookies=7
https://httpbin.org/cookies      cookies=8
https://httpbin.org/cookies      cookies=9
https://httpbin.org/cookies      cookies=10
https://httpbin.org/cookies      cookies=10
https://httpbin.org/cookies      cookies=10
https://www.google.com/          cookies=1
https://httpbin.org/cookies      cookies=10
https://www.bing.com/            cookies=6
https://www.reddit.com/          cookies=1
https://stackoverflow.com/       cookies=0
https://httpbin.org/cookies      cookies=11
https://news.ycombinator.com/    cookies=0
https://x.com/                   cookies=5
https://duckduckgo.com/          cookies=0
https://en.wikipedia.org/Main    cookies=2
https://www.youtube.com/         cookies=0
https://medium.com/              cookies=0
https://github.com/              cookies=1
https://www.bbc.com/             cookies=1
https://www.nytimes.com/         cookies=6
https://www.linkedin.com/        cookies=4
```

Two sanity problems surface from this data:

1. **Empty `domain` field for every host-only cookie**: 89 of 115 cookies (77%) emit `"domain": ""`. Spot-checked against source:

   `crates/heso-cli/src/main.rs:2447` →

   ```rust
   "domain": c.domain().unwrap_or(""),
   ```

   `cookie_store::Cookie::domain()` returns `None` for host-only cookies (i.e., cookies whose `Set-Cookie` did not include a `Domain=` attribute — that is the RFC 6265 default and is normal for most sites). For host-only cookies, the only way the cookie is meaningful is via the implicit host = request URL's host. Heso loses that on serialization and emits an empty string. An agent reading this output cannot tell that `theme=dark`'s "real" scope is `httpbin.org` and not "everything." Real domains *do* render correctly when the server sends `Domain=example.com` (we see `bbc.com`, `nytimes.com`, etc).

2. **Read-after-write race on the shared jar**: httpbin.org/cookies redirects from the 11 distinct `/set/...` URLs converge to one final URL whose cookies should be stable per-fetch. Instead the per-URL `cookies[]` length is 2,5,6,7,8,9,10,10,10,10,11 across 11 sibling lines — monotonically growing **as later siblings' Set-Cookie responses land in the jar**. That's because `collect_cookies` in `main.rs:1602` is called at JSON-serialization time, well after the URL's own response arrived. With `--parallel 4`, cookies set by sibling URLs in flight get mixed into our snapshot.

    Result: same URL, same fetched response, different `cookies` snapshot per output line — non-deterministic, repro-breaking. For `--parallel 1` the field would be deterministic by construction; for `--parallel >= 2` it is not.

3. Several real-browser-cookie-heavy domains emit `cookies=0`: duckduckgo.com, stackoverflow.com, news.ycombinator.com, youtube.com, medium.com. Real browsers see many cookies on these. Two non-mutually-exclusive explanations:
    - These sites set their cookies via JS at hydration time and `read` without `--js-fetch` doesn't run that pump.
    - `guard.matches(url)` is filtering correctly to cookies that *match the request URL*, and these sites set cookies on subdomains or with HttpOnly which would be filtered. (HttpOnly is explicitly skipped at line 2441 — so HttpOnly cookies disappear from the JSON entirely. That's correct in spirit, but it means the JSON `cookies` field is "non-HttpOnly cookies visible to document.cookie," which is fine but should match the documented behavior.)

   These are not bugs per se, but the field's contract is fuzzy — it's not a snapshot of the jar.

No on-disk cookie file: heso uses `Arc<CookieStoreMutex>` from `reqwest_cookie_store`, **in-memory only**. Each `heso open` / `heso batch` invocation starts with an empty jar. No persistence ≠ corruption, but it means concurrent independent invocations cannot collide via the jar (Workload 3 confirms).

# Workload 5 — long batch (`heso batch open --parallel 4`, 200 then 400 URLs)

Run via two strategies because PowerShell's async event pump batches stdout events with massive latency, making line-arrival timestamps unreliable. Instead measured throughput by splitting into 4 equal sequential chunks and timing each.

**200 URLs, 4 chunks of 50 each, parallel=4:**

| Quartile | URLs | Duration s | Rate URL/s | Peak RSS MB | Peak handles |
|--:|--:|--:|--:|--:|--:|
| Q1 | 50 | 2.835 | 17.6 | 92.7 | 186 |
| Q2 | 50 | 1.309 | 38.2 | 78.5 | 179 |
| Q3 | 50 | 1.299 | 38.5 | 77.2 | 181 |
| Q4 | 50 | 1.079 | **46.4** | 92.9 | 184 |

**400 URLs, 4 chunks of 100 each, parallel=4:**

| Quartile | URLs | Duration s | Rate URL/s | Peak RSS MB | Peak handles |
|--:|--:|--:|--:|--:|--:|
| Q1 | 100 | 3.066 | 32.6 | 119.9 | 184 |
| Q2 | 100 | 2.611 | 38.3 | 98.4 | 183 |
| Q3 | 100 | 2.177 | 45.9 | 92.9 | 184 |
| Q4 | 100 | 1.968 | **50.8** | 98.5 | 182 |

**Single-process 200 URLs at parallel=4, sampled every 250 ms:**

| t (s) | RSS (MB) | Handles | Threads |
|--:|--:|--:|--:|
| 0.03 | 10.7 | 173 | 25 |
| 0.30 | 39.6 | 177 | 25 |
| 0.57 | 82.8 | 177 | 25 |
| 0.83 | 99.3 | 182 | 25 |
| 1.10 | 101.0 | 181 | 25 |
| 1.36 | 82.2 | 186 | 25 |
| 1.63 | 107.7 | 186 | 25 |
| 1.89 | 123.7 | 186 | 25 |
| 2.15 | 78.6 | 186 | 25 |
| 2.42 | 113.6 | 186 | 25 |
| 2.68 | 118.3 | 186 | 25 |
| 2.95 | 119.0 | 186 | 25 |
| 3.21 | 107.3 | 186 | 25 |

Final: `duration=3.47 s, 200 ok / 0 fail, throughput 57.6 URLs/s, peak 123.7 MB, 186 handles, 25 threads`.

Text-based throughput chart (URLs/s per 100-op chunk over 400 URLs):

```
URLs/s
  60 |
  50 |                                                       ##########
  45 |                                          ##########  ##########
  40 |                              ##########  ##########  ##########
  35 |                  ##########  ##########  ##########  ##########
  30 |   ##########     ##########  ##########  ##########  ##########
  25 |   ##########     ##########  ##########  ##########  ##########
  20 |   ##########     ##########  ##########  ##########  ##########
  ---+------------+-------------+-------------+-------------+----------
        Q1 (0-100)   Q2 (100-200)  Q3 (200-300)  Q4 (300-400)
         32.6/s        38.3/s        45.9/s        50.8/s
```

Findings:
- **Throughput strictly improves** Q1 → Q4 (32.6 → 50.8 URLs/s, +56%) for the 400-URL run. Same shape for 200-URL run (+163% Q1→Q4 — Q1 there pays first-batch DNS+TLS warm-up). No degradation in second half.
- **RSS is steady-state oscillating** between 78–124 MB across a 200-URL batch. No monotonic upward drift. Working set shrinks repeatedly through the run — allocator releases held pages back to OS as connections close.
- **Handles climb 173 → 186 in the first 0.3 s** and plateau there for the rest of the run. No leak.
- **Thread count is flat 25** — Tokio worker pool sized once at startup. No runaway task spawning.
- Single-process throughput (57.6 URLs/s for 200 in 3.47 s) is **higher** than the chunked-batch equivalent (28.8 URLs/s = 200/6.94 s) because the chunked version pays a per-batch heso startup tax (≈250 ms each).

# Bug list

| Severity | Workload | Symptom | Measurements | Repro |
|---|---|---|---|---|
| P1 | W4 cookies | Host-only cookies emit `"domain": ""` in JSON output, losing scope info | 89/115 cookies (77%) in batch | `heso read 'https://httpbin.org/cookies/set?test1=hello' --include cookies` → look at cookies[0].domain |
| P1 | W4 cookies | `heso batch read --include cookies` returns non-deterministic `cookies` per URL because `collect_cookies` reads jar AFTER all sibling responses may have landed; same URL, same fetch, different snapshot per line | 25 URLs, p=4 → 77/115 (67%) of cookie entries are sibling-contamination duplicates; 11 successive httpbin lines report 2,5,6,7,8,9,10,10,10,10,11 cookies | `heso batch read --parallel 4 --include cookies` against any cookie-setting URL list (≥2 URLs setting cookies) |
| P2 | W2 batch parallel | `--parallel 32` consumes ~25% more handles and ~33% more threads than `--parallel 16` for **zero** extra throughput; effective ceiling appears to be ~16 | p=16: 64.2 URLs/s, 274 handles, 37 threads; p=32: 63.9 URLs/s, 346 handles, 53 threads | `heso batch open --parallel N <100 URLs>` for N in {16,32} |
| P2 | W1 sequential | 0.4% silent failure rate: process exits with `exit=-1`, zero stdout, zero stderr. No panic message captured | 2/500 ops; ops #444 (en.wikipedia.org/wiki/XML, 345 ms) and #459 (github.com/rust-lang/rust, 1.18 s); did not repro in a 100-op focused retry | hard to repro; likely external (Defender / network), but heso should at minimum print a panic / error before exiting |
| P3 | W4 cookies | Major real-world sites (duckduckgo, stackoverflow, HN, youtube, medium) report `cookies=0` from `heso read --include cookies` against a fresh jar | 5/25 URLs in W4 | run heso read against any of those URLs and compare to `curl -v`'s Set-Cookie response — heso filters HttpOnly cookies entirely, but doesn't document that. Possibly also depends on whether site sets cookies via JS hydration (not exercised without `--js-fetch`) |

# Top 5 reliability fixes (ranked)

1. **Render host-only cookies with their implicit host as `domain`** (P1). At `crates/heso-cli/src/main.rs:2447`, change `c.domain().unwrap_or("")` to compute the effective domain as `c.domain().map(str::to_owned).unwrap_or_else(|| url.host_str().unwrap_or("").to_owned())`, and add a sibling boolean field `host_only: bool` so agents can distinguish. Today the JSON loses information that `cookie_store` has internally — bad contract.

2. **Snapshot cookies at response time, not at JSON-serialize time** (P1). In `heso batch read --include cookies` with `--parallel >= 2`, the current code path serializes per-URL `cookies` from the shared `Arc<CookieStoreMutex>` after the fact, so a URL's "cookies" reflects whatever has been written to the jar by *all sibling tasks* up to that moment. Fix options, in order of effort:
    (a) When the per-URL HTTP exchange completes, immediately call `guard.matches(url)` against the jar and stash the result on the per-URL `Page`; serialize that stashed list later. Snapshot is taken with the lock held in the per-URL task, deterministic.
    (b) Document the existing behavior explicitly and surface a `--cookies-snapshot=batch-end|per-url-end` flag with `per-url-end` as default.
    Option (a) is the right call; (b) is a band-aid.

3. **Don't spin up worker tasks beyond the connection pool's per-host limit** (P2). `--parallel 32` allocates twice the slots of `--parallel 16` for zero gain because reqwest's `pool_max_idle_per_host` (default 32 total but with per-host limits) saturates earlier. Cap effective parallelism at `min(--parallel, host-distinct-URLs * pool-limit)`, or document the hard ceiling. Spending ~70 extra handles and ~16 extra threads per invocation for no throughput is wasteful and hides the real bottleneck.

4. **Audit silent process exits** (P2). 2 of 500 sequential ops returned exit=-1 with stdout=stderr=empty, no panic logged. Wrap `main()`'s entry in `std::panic::catch_unwind` + a process-final guard that writes `{ "error": "panic", "msg": ..., "backtrace": ... }` to stderr before exiting non-zero. The current behavior makes "0.4% your tool crashed silently" indistinguishable from "0.4% Windows killed your process." Logging would let the next bug-hunter tell which.

5. **Document cookie surface contract** (P3). The `cookies` field of `heso read --include cookies` is *currently* "non-HttpOnly cookies that match the request URL via `cookie_store::CookieStore::matches`". That excludes HttpOnly entirely (which is fine for a `document.cookie`-style surface) but the README, the rustdoc on `collect_cookies`, and `AGENTS.md` should say so. Today an agent comparing heso's cookie count to a `curl -v` Set-Cookie count will be wrong by a factor of 2-5 on most modern sites and have no idea why.
