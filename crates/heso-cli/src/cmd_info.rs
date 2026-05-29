//! Polymorphic `heso info <file> [file2]`.
//!
//! One file: print a human or JSON summary of the artifact. Two files:
//! both must be plats; emit the diff (extracted inner logic from
//! `plat-diff`).

use std::process::ExitCode;

use serde_json::{json, Value};

use crate::artifact_sniffer::{detect, ArtifactKind};
use crate::template;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Json,
}

struct InfoArgs {
    file_a: String,
    file_b: Option<String>,
    format: Format,
    hash_only: bool,
}

fn parse_args(args: &[String]) -> Result<InfoArgs, ExitCode> {
    let mut positional: Vec<String> = Vec::new();
    let mut format = Format::Text;
    let mut hash_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                return Err(ExitCode::SUCCESS);
            }
            "--format" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--format needs a value (json|text)");
                    return Err(ExitCode::from(2));
                };
                format = match v.as_str() {
                    "json" => Format::Json,
                    "text" => Format::Text,
                    other => {
                        eprintln!("--format: invalid value `{other}` (expected json|text)");
                        return Err(ExitCode::from(2));
                    }
                };
                i += 2;
            }
            "--hash-only" => {
                hash_only = true;
                i += 1;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag `{other}`");
                print_usage();
                return Err(ExitCode::from(2));
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }
    if positional.is_empty() || positional.len() > 2 {
        print_usage();
        return Err(ExitCode::from(2));
    }
    let mut iter = positional.into_iter();
    let file_a = iter.next().expect("at least one positional");
    let file_b = iter.next();
    Ok(InfoArgs {
        file_a,
        file_b,
        format,
        hash_only,
    })
}

fn print_usage() {
    eprintln!("usage: heso info [--format json|text] [--hash-only] <file> [file2]");
    eprintln!();
    eprintln!("Polymorphic summary. Detects whether <file> is a plat, sealed plat,");
    eprintln!("receipt, action-hash fingerprint, or template and prints the right");
    eprintln!("summary. With two arguments, both must be plats — emits a diff.");
}

/// `heso info <file> [file2]` — print a summary of an artifact or diff
/// two plats.
pub async fn cmd_info(args: &[String]) -> ExitCode {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let (contents_a, value_a, kind_a) = match load_artifact(&parsed.file_a).await {
        Ok(t) => t,
        Err(code) => return code,
    };

    if let Some(file_b) = parsed.file_b.as_ref() {
        if parsed.hash_only {
            eprintln!("info: --hash-only takes a single file, not diff mode");
            return ExitCode::from(2);
        }
        let (_, value_b, kind_b) = match load_artifact(file_b).await {
            Ok(t) => t,
            Err(code) => return code,
        };
        if kind_a != ArtifactKind::Plat || kind_b != ArtifactKind::Plat {
            eprintln!("info: diff mode only supports two plats");
            return ExitCode::from(2);
        }
        return diff_plats(&value_a, &value_b, parsed.format);
    }

    match kind_a {
        ArtifactKind::Plat => info_plat(&value_a, contents_a.len(), parsed),
        ArtifactKind::SealedPlat => info_sealed_plat(&value_a, parsed.format),
        ArtifactKind::Receipt => info_receipt(&value_a, parsed.format),
        ArtifactKind::ActionHash => info_action_hash(&value_a, parsed.format),
        ArtifactKind::Template => info_template(&contents_a, parsed.format),
    }
}

async fn load_artifact(path: &str) -> Result<(String, Value, ArtifactKind), ExitCode> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read `{path}`: {e}");
            return Err(ExitCode::from(2));
        }
    };
    let value: Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("`{path}` is not valid JSON: {e}");
            return Err(ExitCode::from(2));
        }
    };
    let kind = match detect(&value) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return Err(ExitCode::from(2));
        }
    };
    Ok((contents, value, kind))
}

fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

fn info_plat(value: &Value, size_bytes: usize, args: InfoArgs) -> ExitCode {
    let embedded_hash = value
        .get("plat_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if args.hash_only {
        if embedded_hash.is_empty() {
            eprintln!("plat has no `plat_hash` field");
            return ExitCode::from(2);
        }
        println!("{embedded_hash}");
        return ExitCode::SUCCESS;
    }

    let url = value.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let title = value.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let plan_count = value
        .get("plan")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let cassette_count = value
        .get("cassette")
        .and_then(|c| c.get("records"))
        .and_then(|r| r.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let step_count = value
        .get("steps")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let sealed = value.get("alg").is_some() && value.get("signature").is_some();
    let partial = value
        .get("partial")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let verified = matches!(heso_engine_fetch::plat_verify(value), Ok(true));

    match args.format {
        Format::Json => {
            let out = json!({
                "kind": "plat",
                "plat_hash": embedded_hash,
                "verified": verified,
                "size_bytes": size_bytes,
                "url": url,
                "title": title,
                "plan_count": plan_count,
                "cassette_records": cassette_count,
                "step_count": step_count,
                "sealed": sealed,
                "partial": partial,
            });
            crate::print_json(&out)
        }
        Format::Text => {
            println!("kind:           plat");
            println!("plat_hash:      {embedded_hash}");
            println!(
                "verified:       {}",
                if verified {
                    "yes"
                } else {
                    "no (embedded hash does not match recomputed)"
                }
            );
            println!("size:           {}", format_bytes(size_bytes));
            println!("url:            {url}");
            println!("title:          {title}");
            println!("plan:           {plan_count} actions");
            println!("cassette:       {cassette_count} records");
            println!("steps:          {step_count}");
            println!("sealed:         {}", if sealed { "yes" } else { "no" });
            println!("partial:        {partial}");
            ExitCode::SUCCESS
        }
    }
}

fn info_sealed_plat(value: &Value, format: Format) -> ExitCode {
    let alg = value.get("alg").and_then(|v| v.as_str()).unwrap_or("");
    let pubkey = value
        .get("signature")
        .and_then(|s| s.get("public_key"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let inner_hash = value
        .get("content")
        .and_then(|c| c.get("plat_hash"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let content_url = value
        .get("content")
        .and_then(|c| c.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match format {
        Format::Json => {
            let out = json!({
                "kind": "sealed-plat",
                "alg": alg,
                "public_key": pubkey,
                "content_plat_hash": inner_hash,
                "content_url": content_url,
            });
            crate::print_json(&out)
        }
        Format::Text => {
            println!("kind:           sealed-plat");
            println!("alg:            {alg}");
            println!("public_key:     {pubkey}");
            println!("content_url:    {content_url}");
            println!("inner plat_hash: {inner_hash}");
            ExitCode::SUCCESS
        }
    }
}

fn info_receipt(value: &Value, format: Format) -> ExitCode {
    let mode = value.get("mode").and_then(|v| v.as_str()).unwrap_or("");
    let seed = value.get("seed").and_then(|v| v.as_u64()).unwrap_or(0);
    let trace_hash = value
        .get("trace_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let pubkey = value
        .get("signature")
        .and_then(|s| s.get("public_key"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let gen_time = value
        .get("tsa_anchor")
        .and_then(|a| a.get("gen_time"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let produced_plat_hash = value
        .get("produced_plat_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match format {
        Format::Json => {
            let out = json!({
                "kind": "receipt",
                "mode": mode,
                "seed": seed,
                "trace_hash": trace_hash,
                "public_key": pubkey,
                "gen_time": gen_time,
                "produced_plat_hash": produced_plat_hash,
            });
            crate::print_json(&out)
        }
        Format::Text => {
            println!("kind:               receipt");
            println!("mode:               {mode}");
            println!("seed:               {seed}");
            println!("trace_hash:         {trace_hash}");
            println!("public_key:         {pubkey}");
            if !gen_time.is_empty() {
                println!("tsa_anchor.gen_time: {gen_time}");
            }
            if !produced_plat_hash.is_empty() {
                println!("produced_plat_hash: {produced_plat_hash}");
            }
            ExitCode::SUCCESS
        }
    }
}

fn info_action_hash(value: &Value, format: Format) -> ExitCode {
    let algorithm = value
        .get("algorithm")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let url = value.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let site_id = value
        .get("site_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let trace_id = value
        .get("trace_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let action_count = value
        .get("actions")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    match format {
        Format::Json => {
            let out = json!({
                "kind": "action-hash",
                "algorithm": algorithm,
                "url": url,
                "site_id": site_id,
                "trace_id": trace_id,
                "action_count": action_count,
            });
            crate::print_json(&out)
        }
        Format::Text => {
            println!("kind:        action-hash");
            println!("algorithm:   {algorithm}");
            println!("url:         {url}");
            println!("site_id:     {site_id}");
            println!("trace_id:    {trace_id}");
            println!("actions:     {action_count}");
            ExitCode::SUCCESS
        }
    }
}

fn info_template(raw: &str, format: Format) -> ExitCode {
    match template::validate_template_raw(raw) {
        Ok(summary) => match format {
            Format::Json => {
                let out = json!({
                    "kind": "template",
                    "ok": true,
                    "schema": summary.schema,
                    "id": summary.id,
                    "version": summary.version,
                    "template_hash": summary.template_hash,
                    "steps": summary.steps,
                    "hash_matches": summary.hash_matches,
                    "secret_warnings": summary.secret_warnings,
                });
                crate::print_json(&out)
            }
            Format::Text => {
                println!("kind:          template");
                println!("schema:        {}", summary.schema);
                println!("id:            {}", summary.id);
                println!("version:       {}", summary.version);
                println!("template_hash: {}", summary.template_hash);
                println!("steps:         {}", summary.steps);
                ExitCode::SUCCESS
            }
        },
        Err(e) => match format {
            Format::Json => {
                let out = json!({
                    "kind": "template",
                    "ok": false,
                    "error": {
                        "kind": "invalid_template",
                        "message": e,
                    },
                });
                crate::print_json(&out);
                ExitCode::from(1)
            }
            Format::Text => {
                eprintln!("invalid template: {e}");
                ExitCode::from(1)
            }
        },
    }
}

fn diff_plats(a: &Value, b: &Value, format: Format) -> ExitCode {
    let hash_a = heso_engine_fetch::plat_hash(a);
    let hash_b = heso_engine_fetch::plat_hash(b);
    let identical = hash_a == hash_b;

    let plan_a = a.get("plan").and_then(|v| v.as_array()).map(Vec::as_slice);
    let plan_b = b.get("plan").and_then(|v| v.as_array()).map(Vec::as_slice);
    let plan_summary = match (plan_a, plan_b) {
        (Some(pa), Some(pb)) if pa == pb => format!("identical ({} actions)", pa.len()),
        (Some(pa), Some(pb)) => format!("different (a: {} / b: {})", pa.len(), pb.len()),
        (Some(_), None) => "only in a".to_owned(),
        (None, Some(_)) => "only in b".to_owned(),
        (None, None) => "absent".to_owned(),
    };

    match format {
        Format::Json => {
            let out = json!({
                "kind": "plat-diff",
                "plat_hash_a": hash_a,
                "plat_hash_b": hash_b,
                "identical": identical,
                "plan": plan_summary,
            });
            let _ = crate::print_json(&out);
        }
        Format::Text => {
            if identical {
                println!("plat_hash:   IDENTICAL ({hash_a})");
            } else {
                println!("plat_hash:   DIFFERENT");
                println!("             a: {hash_a}");
                println!("             b: {hash_b}");
            }
            println!("plan:        {plan_summary}");
        }
    }
    if identical {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
