//! `heso publish` / `heso pull` / `heso list` — the three CLI verbs that
//! talk to the public plat registry at heso.ca/ecosystem.
//!
//! All three are thin HTTP clients over the registry's REST surface:
//! - `POST  /plat`             → upload an existing plat with a description
//! - `GET   /plat/{hash}/file` → download a plat by content hash
//! - `GET   /list?…`           → browse the registry, sorted and filtered
//!
//! The default endpoint is the production registry. Override via the
//! `HESO_ECOSYSTEM_URL` env var (used for staging / self-hosted).

use std::process::ExitCode;
use std::time::Duration;

use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "https://heso.ca/api/ecosystem";

/// Hard cap on a single registry response body. Plats are JSON
/// artifacts of a few hundred KB at most; list pages cap at 100
/// entries. 16 MiB is the headroom a misbehaving (or hostile)
/// registry would need to push the CLI into multi-hundred-MB
/// allocations.
const MAX_REGISTRY_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

fn base_url() -> String {
    std::env::var("HESO_ECOSYSTEM_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("heso/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))
}

/// Read a registry response body, refusing payloads larger than
/// [`MAX_REGISTRY_RESPONSE_BYTES`]. Streams chunks so a hostile
/// `Content-Length: 4_294_967_295` doesn't force a giant pre-alloc,
/// and a chunked response without a declared length is still capped.
async fn read_body_capped(resp: reqwest::Response) -> Result<Vec<u8>, String> {
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_REGISTRY_RESPONSE_BYTES {
            return Err(format!(
                "registry response too large: declared {len} bytes (cap {MAX_REGISTRY_RESPONSE_BYTES})"
            ));
        }
    }
    let mut acc: Vec<u8> = Vec::new();
    let mut resp = resp;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("read body failed: {e}"))?
    {
        if acc.len() + chunk.len() > MAX_REGISTRY_RESPONSE_BYTES {
            return Err(format!(
                "registry response too large: exceeded {MAX_REGISTRY_RESPONSE_BYTES} bytes"
            ));
        }
        acc.extend_from_slice(&chunk);
    }
    Ok(acc)
}

const HEX64_LEN: usize = 64;

fn is_hex64(s: &str) -> bool {
    s.len() == HEX64_LEN
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && (!c.is_ascii_alphabetic() || c.is_ascii_lowercase()))
}

pub(crate) fn is_plat_hash(s: &str) -> bool {
    is_hex64(s)
}

pub(crate) async fn download_plat(hash: &str) -> Result<Vec<u8>, String> {
    if !is_hex64(hash) {
        return Err(format!("`{hash}` is not a 64-char lowercase-hex plat hash"));
    }

    let client = client()?;
    let url = format!("{}/plat/{}/file", base_url().trim_end_matches('/'), hash);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("no plat with hash {hash} in the registry"));
    }
    if !status.is_success() {
        let txt = match read_body_capped(resp).await {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => e,
        };
        return Err(format!("registry returned {status}: {txt}"));
    }
    read_body_capped(resp).await
}

pub(crate) async fn download_plat_text(hash: &str) -> Result<String, String> {
    let bytes = download_plat(hash).await?;
    String::from_utf8(bytes).map_err(|e| format!("downloaded plat is not UTF-8 JSON: {e}"))
}

/// `heso publish <plat-file> -d "description" [-t "tag1,tag2"]`
pub async fn cmd_publish(args: &[String]) -> ExitCode {
    let mut path: Option<&str> = None;
    let mut description: Option<&str> = None;
    let mut tags_csv: Option<&str> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--description" => {
                if i + 1 >= args.len() {
                    eprintln!("publish: -d requires a value");
                    return ExitCode::from(2);
                }
                description = Some(&args[i + 1]);
                i += 2;
            }
            "-t" | "--tags" => {
                if i + 1 >= args.len() {
                    eprintln!("publish: -t requires a value");
                    return ExitCode::from(2);
                }
                tags_csv = Some(&args[i + 1]);
                i += 2;
            }
            "-h" | "--help" => {
                print_publish_help();
                return ExitCode::SUCCESS;
            }
            a if a.starts_with('-') => {
                eprintln!("publish: unknown flag `{a}`");
                return ExitCode::from(2);
            }
            _ => {
                if path.is_some() {
                    eprintln!("publish: too many positional args");
                    return ExitCode::from(2);
                }
                path = Some(&args[i]);
                i += 1;
            }
        }
    }

    let path = match path {
        Some(p) => p,
        None => {
            print_publish_help();
            return ExitCode::from(2);
        }
    };
    let description = match description {
        Some(d) => d.trim(),
        None => {
            eprintln!("publish: -d \"description\" is required");
            return ExitCode::from(2);
        }
    };
    if description.is_empty() {
        eprintln!("publish: description cannot be empty");
        return ExitCode::from(2);
    }

    let raw = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("publish: cannot read `{path}`: {e}");
            return ExitCode::from(2);
        }
    };
    let plat: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("publish: `{path}` is not valid JSON: {e}");
            return ExitCode::from(2);
        }
    };

    let plat_hash = match plat.get("plat_hash").and_then(Value::as_str) {
        Some(h) if is_hex64(h) => h.to_owned(),
        _ => {
            eprintln!("publish: file missing a 64-char lowercase-hex `plat_hash` — did you stamp it? (`heso stamp <plan.json>`)");
            return ExitCode::from(2);
        }
    };

    let tags: Vec<String> = match tags_csv {
        Some(csv) => csv
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
        None => Vec::new(),
    };

    let body = json!({
        "plat": plat,
        "description": description,
        "tags": tags,
    });

    let client = match client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("publish: {e}");
            return ExitCode::FAILURE;
        }
    };
    let url = format!("{}/plat", base_url().trim_end_matches('/'));
    let body_str = match serde_json::to_string(&body) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("publish: serialize body failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let resp = match client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body_str)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("publish: network error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let status = resp.status();
    let txt = match read_body_capped(resp).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            eprintln!("publish: {e}");
            return ExitCode::FAILURE;
        }
    };
    if !status.is_success() {
        eprintln!("publish: registry returned {status}: {txt}");
        return ExitCode::FAILURE;
    }
    let parsed: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
    let hash_back = parsed
        .get("plat_hash")
        .and_then(Value::as_str)
        .unwrap_or(plat_hash.as_str());
    let kind = parsed.get("status").and_then(Value::as_str).unwrap_or("ok");

    println!("✓ {kind}: {hash_back}");
    println!("  pull:  heso pull {hash_back}");
    println!("  view:  https://heso.ca/ecosystem/p/{hash_back}");

    ExitCode::SUCCESS
}

fn print_publish_help() {
    eprintln!("usage: heso publish <plat-file> -d \"description\" [-t \"tag1,tag2\"]");
    eprintln!();
    eprintln!("  upload a plat (output of `heso stamp` or `heso run`) to the public registry.");
    eprintln!();
    eprintln!("  -d, --description \"…\"   required, max ~240 chars");
    eprintln!("  -t, --tags \"a,b,c\"      comma-separated, max 8");
    eprintln!();
    eprintln!("  override the registry endpoint with HESO_ECOSYSTEM_URL=<base-url>");
}

/// `heso pull <hash> [-o output-path]`
pub async fn cmd_pull(args: &[String]) -> ExitCode {
    let mut hash: Option<&str> = None;
    let mut out_path: Option<&str> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--out" => {
                if i + 1 >= args.len() {
                    eprintln!("pull: -o requires a value");
                    return ExitCode::from(2);
                }
                out_path = Some(&args[i + 1]);
                i += 2;
            }
            "-h" | "--help" => {
                print_pull_help();
                return ExitCode::SUCCESS;
            }
            a if a.starts_with('-') => {
                eprintln!("pull: unknown flag `{a}`");
                return ExitCode::from(2);
            }
            _ => {
                if hash.is_some() {
                    eprintln!("pull: too many positional args");
                    return ExitCode::from(2);
                }
                hash = Some(&args[i]);
                i += 1;
            }
        }
    }

    let hash = match hash {
        Some(h) if is_hex64(h) => h,
        Some(h) => {
            eprintln!("pull: `{h}` is not a 64-char lowercase-hex plat hash");
            return ExitCode::from(2);
        }
        None => {
            print_pull_help();
            return ExitCode::from(2);
        }
    };

    let target = out_path
        .map(String::from)
        .unwrap_or_else(|| format!("{hash}.plat"));

    let bytes = match download_plat(hash).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("pull: {e}");
            return ExitCode::FAILURE;
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pull: downloaded plat `{hash}` is not valid JSON: {e}");
            return ExitCode::FAILURE;
        }
    };
    let embedded = value.get("plat_hash").and_then(Value::as_str);
    let recomputed = heso_engine_fetch::plat_hash(&value);
    if embedded != Some(hash) || recomputed != hash {
        eprintln!("pull: registry returned a plat that does not match requested hash `{hash}`");
        eprintln!("  embedded:   {}", embedded.unwrap_or("(missing)"));
        eprintln!("  recomputed: {recomputed}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = tokio::fs::write(&target, &bytes).await {
        eprintln!("pull: write `{target}` failed: {e}");
        return ExitCode::FAILURE;
    }
    println!("✓ pulled {} bytes → {target}", bytes.len());
    println!("  replay: heso replay {target}");
    ExitCode::SUCCESS
}

fn print_pull_help() {
    eprintln!("usage: heso pull <plat-hash> [-o <output-path>]");
    eprintln!();
    eprintln!("  download a plat from the public registry by its 64-char BLAKE3 hash.");
    eprintln!();
    eprintln!("  -o, --out <path>   default: ./<hash>.plat");
    eprintln!();
    eprintln!("  override the registry endpoint with HESO_ECOSYSTEM_URL=<base-url>");
}

/// `heso list [-q query] [-t tag] [--sort trending|downloads|newest] [--limit N]`
pub async fn cmd_list(args: &[String]) -> ExitCode {
    let mut query: Option<&str> = None;
    let mut tag: Option<&str> = None;
    let mut sort: &str = "trending";
    let mut limit: u32 = 20;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-q" | "--query" => {
                if i + 1 >= args.len() {
                    eprintln!("list: -q requires a value");
                    return ExitCode::from(2);
                }
                query = Some(&args[i + 1]);
                i += 2;
            }
            "-t" | "--tag" => {
                if i + 1 >= args.len() {
                    eprintln!("list: -t requires a value");
                    return ExitCode::from(2);
                }
                tag = Some(&args[i + 1]);
                i += 2;
            }
            "--sort" => {
                if i + 1 >= args.len() {
                    eprintln!("list: --sort requires a value");
                    return ExitCode::from(2);
                }
                let v = args[i + 1].as_str();
                if !matches!(v, "trending" | "downloads" | "newest") {
                    eprintln!("list: --sort must be one of: trending, downloads, newest");
                    return ExitCode::from(2);
                }
                sort = v;
                i += 2;
            }
            "--limit" => {
                if i + 1 >= args.len() {
                    eprintln!("list: --limit requires a value");
                    return ExitCode::from(2);
                }
                limit = match args[i + 1].parse::<u32>() {
                    Ok(n) if (1..=100).contains(&n) => n,
                    _ => {
                        eprintln!("list: --limit must be 1..=100");
                        return ExitCode::from(2);
                    }
                };
                i += 2;
            }
            "-h" | "--help" => {
                print_list_help();
                return ExitCode::SUCCESS;
            }
            a if a.starts_with('-') => {
                eprintln!("list: unknown flag `{a}`");
                return ExitCode::from(2);
            }
            _ => {
                if query.is_some() {
                    eprintln!("list: too many positional args (use `-q \"…\"` for the query)");
                    return ExitCode::from(2);
                }
                query = Some(&args[i]);
                i += 1;
            }
        }
    }

    let client = match client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("list: {e}");
            return ExitCode::FAILURE;
        }
    };

    let base = base_url();
    let url_str = format!("{}/list", base.trim_end_matches('/'));
    let mut url = match reqwest::Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => {
            eprintln!(
                "list: invalid registry endpoint `{url_str}` (set HESO_ECOSYSTEM_URL to a valid base URL): {e}"
            );
            return ExitCode::from(2);
        }
    };
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("sort", sort);
        q.append_pair("limit", &limit.to_string());
        if let Some(qv) = query {
            if !qv.trim().is_empty() {
                q.append_pair("q", qv.trim());
            }
        }
        if let Some(t) = tag {
            if !t.trim().is_empty() {
                q.append_pair("tags", t.trim());
            }
        }
    }

    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("list: network error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let status = resp.status();
    let bytes = match read_body_capped(resp).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("list: {e}");
            return ExitCode::FAILURE;
        }
    };
    let txt = String::from_utf8_lossy(&bytes);
    if !status.is_success() {
        eprintln!("list: registry returned {status}: {txt}");
        return ExitCode::FAILURE;
    }
    let body: Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("list: malformed response from registry: {e}");
            return ExitCode::FAILURE;
        }
    };
    let items = match body.get("items").and_then(Value::as_array) {
        Some(a) => a,
        None => {
            eprintln!("list: response missing `items`");
            return ExitCode::FAILURE;
        }
    };
    if items.is_empty() {
        println!("(no plats match)");
        return ExitCode::SUCCESS;
    }

    println!("{:<12}  {:>5}  {:<18}  DESCRIPTION", "HASH", "DLs", "PUBLISHED");
    for it in items {
        let hash = it.get("plat_hash").and_then(Value::as_str).unwrap_or("?");
        let short_hash = if hash.len() >= 12 { &hash[..12] } else { hash };
        let downloads = it
            .get("downloads_total")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let published = it.get("published_at").and_then(Value::as_str).unwrap_or("");
        let age = relative_age(published);
        let description = it
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("(no description)");
        let desc_trunc = if description.chars().count() > 60 {
            let mut s: String = description.chars().take(57).collect();
            s.push('…');
            s
        } else {
            description.to_owned()
        };
        println!("{short_hash}  {downloads:>5}  {age:<18}  {desc_trunc}");
    }
    let total = items.len();
    let suffix = if total == 1 { "" } else { "s" };
    println!();
    println!("{total} plat{suffix} · sort={sort}");
    println!("(pull a plat: `heso pull <hash>`)");

    ExitCode::SUCCESS
}

fn print_list_help() {
    eprintln!(
        "usage: heso list [-q \"query\"] [-t tag] [--sort trending|downloads|newest] [--limit N]"
    );
    eprintln!();
    eprintln!("  browse the public plat registry.");
    eprintln!();
    eprintln!("  -q, --query  \"…\"               substring match on description / URL / tags");
    eprintln!("  -t, --tag    <tag>             filter by tag (single)");
    eprintln!("      --sort   trending          ranking; default `trending`");
    eprintln!("                downloads        sort by lifetime downloads");
    eprintln!("                newest           sort by publish time");
    eprintln!("      --limit  N                 1..=100, default 20");
    eprintln!();
    eprintln!("  override the registry endpoint with HESO_ECOSYSTEM_URL=<base-url>");
}

fn relative_age(iso: &str) -> String {
    let t = match chrono_parse(iso) {
        Some(t) => t,
        None => return "—".into(),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i128)
        .unwrap_or(0);
    let delta_ms = (now_ms - t).max(0);
    let sec = (delta_ms / 1000) as i64;
    if sec < 60 {
        return "just now".into();
    }
    let min = sec / 60;
    if min < 60 {
        return format!("{min} min ago");
    }
    let hr = min / 60;
    if hr < 24 {
        return format!("{hr} hr ago");
    }
    let day = hr / 24;
    if day < 30 {
        return format!("{day} day{} ago", if day == 1 { "" } else { "s" });
    }
    let mon = day / 30;
    if mon < 12 {
        return format!("{mon} mo ago");
    }
    let yr = mon / 12;
    format!("{yr} yr{} ago", if yr == 1 { "" } else { "s" })
}

/// Tiny ISO 8601 → millis parser for the fixed-shape timestamps the
/// registry emits (`YYYY-MM-DDTHH:MM:SS.fffZ`). Avoids pulling in a
/// chrono / time crate for one display field.
fn chrono_parse(iso: &str) -> Option<i128> {
    let s = iso.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: u32 = t.next()?.parse().ok()?;
    let minute: u32 = t.next()?.parse().ok()?;
    let sec_str = t.next()?;
    let (sec_int, _frac) = sec_str.split_once('.').unwrap_or((sec_str, "0"));
    let second: u32 = sec_int.parse().ok()?;
    // Naive UTC seconds since epoch via the civil-from-days algorithm.
    let days = civil_to_days(year, month as i32, day as i32);
    let secs = days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    Some(secs as i128 * 1000)
}

fn civil_to_days(y: i64, m: i32, d: i32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as i64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
