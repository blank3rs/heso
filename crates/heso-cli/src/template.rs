//! Experimental `heso.template/v0` authoring support.
//!
//! Templates are deliberately kept outside HESO/1.0: `template-stamp`
//! live-records a template into an ordinary concrete plat, and the emitted
//! plat carries only a normal `plan`, `cassette`, `steps`, and `plat_hash`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use heso_core::Url;
use heso_engine_fetch::{resolve_locator_from_html, ElementRef, FetchEngine, FetchPage};
use heso_trace::Action;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{execute_step_session, print_json, wait_dom_quiet};

const TEMPLATE_SCHEMA: &str = "heso.template/v0";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateDoc {
    schema: String,
    id: String,
    version: String,
    #[serde(default)]
    #[allow(dead_code)]
    title: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
    #[serde(default)]
    inputs: BTreeMap<String, InputSpec>,
    steps: Vec<TemplateStep>,
    #[serde(default)]
    template_hash: Option<String>,
    #[serde(default)]
    witnesses: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputSpec {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<Value>,
    #[serde(default, rename = "enum")]
    enum_values: Vec<Value>,
    #[serde(default)]
    secret: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "verb", rename_all = "lowercase", deny_unknown_fields)]
enum TemplateStep {
    Open {
        #[serde(default)]
        id: Option<String>,
        url: UrlTemplate,
    },
    Fill {
        #[serde(default)]
        id: Option<String>,
        target: TargetSpec,
        value: ValueExpr,
    },
    Click {
        #[serde(default)]
        id: Option<String>,
        target: TargetSpec,
    },
    Submit {
        #[serde(default)]
        id: Option<String>,
        target: TargetSpec,
    },
}

impl TemplateStep {
    fn verb(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Fill { .. } => "fill",
            Self::Click { .. } => "click",
            Self::Submit { .. } => "submit",
        }
    }

    fn id(&self) -> Option<&str> {
        match self {
            Self::Open { id, .. }
            | Self::Fill { id, .. }
            | Self::Click { id, .. }
            | Self::Submit { id, .. } => id.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum UrlTemplate {
    Literal(String),
    Structured(StructuredUrl),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredUrl {
    base: String,
    #[serde(default)]
    query: BTreeMap<String, ValueExpr>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ValueExpr {
    Input { input: String },
    Literal(Value),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum TargetSpec {
    ByInput { target_by_input: TargetByInput },
    Locator(LocatorSpec),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetByInput {
    input: String,
    cases: BTreeMap<String, LocatorSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocatorSpec {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default, rename = "aria_label")]
    aria_label: Option<String>,
    #[serde(default, rename = "aria-label")]
    aria_label_dash: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    attrs: BTreeMap<String, String>,
}

struct Bindings {
    values: BTreeMap<String, Value>,
}

struct ActionSnapshot {
    url: Url,
    html: String,
    actions: Vec<ElementRef>,
}

#[derive(Debug)]
struct TemplateExecError {
    message: String,
    detail: Option<Value>,
}

impl TemplateExecError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
        }
    }

    fn with_detail(message: impl Into<String>, detail: Value) -> Self {
        Self {
            message: message.into(),
            detail: Some(detail),
        }
    }
}

impl From<String> for TemplateExecError {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Minimal post-validation summary surfaced by [`validate_template_raw`].
/// Carries only the fields the polymorphic `heso verify` and `heso info`
/// verbs need.
pub(crate) struct TemplateSummary {
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) template_hash: String,
    pub(crate) steps: usize,
    pub(crate) schema: String,
}

/// Parse + validate a `heso.template/v0` raw JSON string. Returns the
/// summary on success or a single-line error message on failure.
pub(crate) fn validate_template_raw(raw: &str) -> Result<TemplateSummary, String> {
    let (doc, hash, _matches) = load_template(raw)?;
    Ok(TemplateSummary {
        id: doc.id.clone(),
        version: doc.version.clone(),
        template_hash: hash,
        steps: doc.steps.len(),
        schema: doc.schema.clone(),
    })
}

/// Handle `heso stamp --template <PATH> [--values JSON|@FILE] [--seed N]`
/// by translating the polymorphic flag suite into the shape
/// [`cmd_template_stamp`] consumes, then delegating to its inner core.
/// `--values JSON` accepts a flat JSON object of `{name: scalar}`;
/// `--values @FILE` reads the same shape from disk.
pub(crate) async fn cmd_stamp_from_template_args(args: &[String]) -> ExitCode {
    let mut template_path: Option<String> = None;
    let mut values_src: Option<String> = None;
    let mut seed: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--template" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("stamp --template needs a path");
                    return ExitCode::from(2);
                };
                template_path = Some(v.clone());
                i += 2;
            }
            "--values" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("stamp --values needs a JSON object or @FILE");
                    return ExitCode::from(2);
                };
                values_src = Some(v.clone());
                i += 2;
            }
            "--seed" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("stamp --seed needs a u64");
                    return ExitCode::from(2);
                };
                seed = Some(v.clone());
                i += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("stamp --template: unknown flag `{other}`");
                return ExitCode::from(2);
            }
            other => {
                eprintln!("stamp --template: unexpected positional `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(template_path) = template_path else {
        eprintln!("stamp --template requires a template path");
        return ExitCode::from(2);
    };

    let mut delegated: Vec<String> = Vec::new();
    if let Some(seed) = seed {
        delegated.push("--seed".to_owned());
        delegated.push(seed);
    }
    if let Some(values_src) = values_src.as_deref() {
        let raw = if let Some(rest) = values_src.strip_prefix('@') {
            match std::fs::read_to_string(rest) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("stamp --values: cannot read `{rest}`: {e}");
                    return ExitCode::from(2);
                }
            }
        } else {
            values_src.to_owned()
        };
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("stamp --values: not valid JSON: {e}");
                return ExitCode::from(2);
            }
        };
        let Some(obj) = parsed.as_object() else {
            eprintln!("stamp --values: expected a JSON object of {{name: scalar}}");
            return ExitCode::from(2);
        };
        for (name, value) in obj {
            let Some(scalar) = value_as_scalar_string(value) else {
                eprintln!(
                    "stamp --values: value for `{name}` must be a string, number, or boolean"
                );
                return ExitCode::from(2);
            };
            delegated.push("--param".to_owned());
            delegated.push(format!("{name}={scalar}"));
        }
    }
    delegated.push(template_path);
    cmd_template_stamp(&delegated).await
}

pub(crate) async fn cmd_template_stamp(args: &[String]) -> ExitCode {
    let parsed = match parse_stamp_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let raw = match read_input(&parsed.path) {
        Ok(s) => s,
        Err(e) => return fail("read_error", e),
    };
    let (doc, template_hash, _hash_matches) = match load_template(&raw) {
        Ok(v) => v,
        Err(e) => return fail("invalid_template", e),
    };
    let bindings = match validate_bindings(&doc, parsed.params) {
        Ok(b) => b,
        Err(e) => return fail("invalid_bindings", e),
    };
    let first_url = match first_open_url(&doc, &bindings) {
        Ok(u) => u,
        Err(e) => return fail("invalid_template", e),
    };

    let cassette: Arc<Mutex<heso_engine_fetch::Cassette>> =
        Arc::new(Mutex::new(heso_engine_fetch::Cassette::default()));
    let fetch = match FetchEngine::with_recording_cassette(cassette.clone()) {
        Ok(f) => f,
        Err(e) => return fail("engine_error", e.to_string()),
    };

    let mut current_url = first_url.clone();
    let mut session: Option<heso_engine_js::JsSession> = None;
    let mut current_actions: Vec<ElementRef> = Vec::new();
    let mut snapshot: Option<ActionSnapshot> = None;
    let mut materialized: Vec<Action> = Vec::with_capacity(doc.steps.len());
    let mut steps: Vec<Value> = Vec::with_capacity(doc.steps.len());

    for (index, template_step) in doc.steps.iter().enumerate() {
        let url_before = current_url.clone();
        let action = match materialize_step(template_step, snapshot.as_ref(), &bindings) {
            Ok(a) => a,
            Err(e) => {
                return fail_step(
                    "materialize_failed",
                    index,
                    template_step,
                    e.message,
                    e.detail,
                );
            }
        };
        if let Action::Open { url } = &action {
            if !domain_allowed(url, &doc.domains) {
                return fail_step(
                    "domain_not_allowed",
                    index,
                    template_step,
                    format!("open URL `{url}` is outside template domains"),
                    Some(json!({ "domains": doc.domains.clone() })),
                );
            }
        }

        let res = execute_step_session(
            &fetch,
            &mut session,
            &mut current_url,
            &mut current_actions,
            &action,
            parsed.seed,
        )
        .await;

        let step = match &res {
            Ok(detail) => json!({
                "index": index,
                "verb": action.verb(),
                "action": action,
                "url_before": url_before.to_string(),
                "url_after": current_url.to_string(),
                "ok": true,
                "result": detail,
            }),
            Err(err) => json!({
                "index": index,
                "verb": action.verb(),
                "action": action,
                "url_before": url_before.to_string(),
                "url_after": current_url.to_string(),
                "ok": false,
                "error": err,
            }),
        };
        steps.push(step);
        if let Err(err) = res {
            return fail_step("execute_failed", index, template_step, err, None);
        }

        materialized.push(action);
        snapshot = match refresh_snapshot(&mut session, &mut current_url, &mut current_actions) {
            Ok(s) => Some(s),
            Err(e) => return fail_step("snapshot_failed", index, template_step, e, None),
        };
    }

    let Some(final_snapshot) = snapshot else {
        return fail("invalid_template", "template produced no page state");
    };
    let plan_json = match serde_json::to_value(&materialized) {
        Ok(v) => v,
        Err(e) => return fail("serialize_error", e.to_string()),
    };
    let mut page = FetchPage::from_html(
        first_url.as_str().to_owned(),
        final_snapshot.url,
        200,
        Vec::new(),
        final_snapshot.html,
    );
    page.plan = Some(plan_json);
    let mut body = page.plat_body_base();
    if let Some(obj) = body.as_object_mut() {
        let final_cassette = cassette.lock().expect("cassette mutex poisoned").clone();
        if let Ok(c) = serde_json::to_value(&final_cassette) {
            obj.insert("cassette".to_owned(), c);
        }
        obj.insert("steps".to_owned(), Value::Array(steps));
    }

    // The plat must remain a normal replayable artifact. The template hash is
    // intentionally not embedded; it is returned only as a side-channel on
    // stderr for operators that want to link the witness externally.
    let hash = heso_engine_fetch::plat_hash(&body);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("plat_hash".to_owned(), Value::String(hash));
    }
    eprintln!("template_hash: {template_hash}");
    print_json(&body)
}

struct StampArgs {
    path: String,
    params: BTreeMap<String, String>,
    seed: Option<u64>,
}

fn parse_stamp_args(args: &[String]) -> Result<StampArgs, ExitCode> {
    let mut path: Option<String> = None;
    let mut params = BTreeMap::new();
    let mut seed = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--param" => {
                let Some(raw) = args.get(i + 1) else {
                    eprintln!("template-stamp: --param requires NAME=VALUE");
                    return Err(ExitCode::from(2));
                };
                let Some((name, value)) = raw.split_once('=') else {
                    eprintln!("template-stamp: --param requires NAME=VALUE");
                    return Err(ExitCode::from(2));
                };
                if name.is_empty() {
                    eprintln!("template-stamp: --param name cannot be empty");
                    return Err(ExitCode::from(2));
                }
                if params.insert(name.to_owned(), value.to_owned()).is_some() {
                    eprintln!("template-stamp: duplicate --param `{name}`");
                    return Err(ExitCode::from(2));
                }
                i += 2;
            }
            "--seed" => {
                let Some(raw) = args.get(i + 1) else {
                    eprintln!("template-stamp: --seed requires an integer");
                    return Err(ExitCode::from(2));
                };
                seed = match raw.parse::<u64>() {
                    Ok(n) => Some(n),
                    Err(e) => {
                        eprintln!("template-stamp: invalid --seed `{raw}`: {e}");
                        return Err(ExitCode::from(2));
                    }
                };
                i += 2;
            }
            "-h" | "--help" => {
                println!("usage: heso template-stamp [--seed N] --param k=v... <template.json|->");
                return Err(ExitCode::from(0));
            }
            other if other.starts_with('-') && other != "-" => {
                eprintln!("template-stamp: unknown flag `{other}`");
                return Err(ExitCode::from(2));
            }
            other => {
                if path.is_some() {
                    eprintln!("template-stamp: too many positional arguments");
                    return Err(ExitCode::from(2));
                }
                path = Some(other.to_owned());
                i += 1;
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: heso template-stamp [--seed N] --param k=v... <template.json|->");
        return Err(ExitCode::from(2));
    };
    Ok(StampArgs { path, params, seed })
}

fn read_input(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        return Ok(s);
    }
    std::fs::read_to_string(Path::new(path)).map_err(|e| format!("cannot read `{path}`: {e}"))
}

fn load_template(raw: &str) -> Result<(TemplateDoc, String, Option<bool>), String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| format!("template is not JSON: {e}"))?;
    let hash = template_hash(&value)?;
    let doc: TemplateDoc =
        serde_json::from_value(value).map_err(|e| format!("template schema error: {e}"))?;
    validate_template(&doc, &hash)?;
    let hash_matches = doc.template_hash.as_ref().map(|embedded| embedded == &hash);
    Ok((doc, hash, hash_matches))
}

fn validate_template(doc: &TemplateDoc, hash: &str) -> Result<(), String> {
    if doc.schema != TEMPLATE_SCHEMA {
        return Err(format!(
            "unsupported schema `{}`; expected `{TEMPLATE_SCHEMA}`",
            doc.schema
        ));
    }
    if doc.id.trim().is_empty() {
        return Err("id cannot be empty".to_owned());
    }
    if doc.version.trim().is_empty() {
        return Err("version cannot be empty".to_owned());
    }
    if doc.witnesses.is_some() {
        return Err(
            "witnesses are external metadata in v0; remove top-level `witnesses`".to_owned(),
        );
    }
    if let Some(embedded) = doc.template_hash.as_deref() {
        if !is_hex64(embedded) {
            return Err("template_hash must be 64 lowercase hex characters".to_owned());
        }
        if embedded != hash {
            return Err(format!(
                "template_hash mismatch: embedded {embedded}, computed {hash}"
            ));
        }
    }
    if doc.steps.is_empty() {
        return Err("steps cannot be empty".to_owned());
    }
    if !matches!(doc.steps.first(), Some(TemplateStep::Open { .. })) {
        return Err("steps[0] must be an open step".to_owned());
    }
    for (name, spec) in &doc.inputs {
        validate_input_spec(name, spec)?;
    }
    for (i, step) in doc.steps.iter().enumerate() {
        validate_step(i, step)?;
        if let TemplateStep::Open { url, .. } = step {
            let literal = match url {
                UrlTemplate::Literal(s) => Some(s.as_str()),
                UrlTemplate::Structured(spec) => Some(spec.base.as_str()),
            };
            if let Some(s) = literal {
                if !domain_allowed(s, &doc.domains) {
                    return Err(format!(
                        "steps[{i}] open URL `{s}` is outside template domains"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_input_spec(name: &str, spec: &InputSpec) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("input names cannot be empty".to_owned());
    }
    match spec.kind.as_str() {
        "string" | "number" | "boolean" | "date" | "enum" | "secret" => {}
        other => return Err(format!("input `{name}` has unsupported type `{other}`")),
    }
    if spec.secret && !matches!(spec.kind.as_str(), "string" | "secret") {
        return Err(format!(
            "input `{name}` can only set secret=true for string-like inputs"
        ));
    }
    if spec.kind == "enum" && spec.enum_values.is_empty() {
        return Err(format!(
            "input `{name}` type enum requires non-empty `enum`"
        ));
    }
    if !spec.enum_values.is_empty() {
        for v in &spec.enum_values {
            if !matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
                return Err(format!("input `{name}` enum values must be scalar"));
            }
        }
    }
    Ok(())
}

fn validate_step(index: usize, step: &TemplateStep) -> Result<(), String> {
    match step {
        TemplateStep::Open { .. } => Ok(()),
        TemplateStep::Fill { target, .. }
        | TemplateStep::Click { target, .. }
        | TemplateStep::Submit { target, .. } => validate_target(index, target),
    }
}

fn validate_target(index: usize, target: &TargetSpec) -> Result<(), String> {
    match target {
        TargetSpec::Locator(locator) => validate_locator(index, locator),
        TargetSpec::ByInput { target_by_input } => {
            if target_by_input.input.trim().is_empty() {
                return Err(format!(
                    "steps[{index}] target_by_input input cannot be empty"
                ));
            }
            if target_by_input.cases.is_empty() {
                return Err(format!(
                    "steps[{index}] target_by_input cases cannot be empty"
                ));
            }
            for locator in target_by_input.cases.values() {
                validate_locator(index, locator)?;
            }
            Ok(())
        }
    }
}

fn validate_locator(index: usize, locator: &LocatorSpec) -> Result<(), String> {
    let has_semantic = locator.role.is_some()
        || locator.name.is_some()
        || locator.text.is_some()
        || locator.selector.is_some()
        || locator.aria_label.is_some()
        || locator.aria_label_dash.is_some()
        || locator.tag.is_some()
        || locator.section.is_some()
        || !locator.attrs.is_empty();
    if !has_semantic {
        return Err(format!("steps[{index}] target cannot be empty"));
    }
    Ok(())
}

fn template_hash(value: &Value) -> Result<String, String> {
    let cleaned = match value {
        Value::Object(map) if map.contains_key("template_hash") => {
            let mut stripped = map.clone();
            stripped.remove("template_hash");
            Value::Object(stripped)
        }
        other => other.clone(),
    };
    let bytes = serde_jcs::to_vec(&cleaned).map_err(|e| format!("canonicalize template: {e}"))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn validate_bindings(
    doc: &TemplateDoc,
    params: BTreeMap<String, String>,
) -> Result<Bindings, String> {
    for key in params.keys() {
        if !doc.inputs.contains_key(key) {
            return Err(format!("unknown parameter `{key}`"));
        }
    }

    let mut values = BTreeMap::new();
    for (name, spec) in &doc.inputs {
        let raw = match params.get(name) {
            Some(v) => Some(Value::String(v.clone())),
            None => spec.default.clone(),
        };
        let Some(raw) = raw else {
            if spec.required {
                return Err(format!("missing required parameter `{name}`"));
            }
            continue;
        };
        let value = coerce_binding(name, spec, raw)?;
        values.insert(name.clone(), value);
    }
    Ok(Bindings { values })
}

fn coerce_binding(name: &str, spec: &InputSpec, raw: Value) -> Result<Value, String> {
    let s = value_as_scalar_string(&raw)
        .ok_or_else(|| format!("parameter `{name}` must be a scalar value"))?;
    let value = match spec.kind.as_str() {
        "string" | "secret" => Value::String(s),
        "date" => {
            if !looks_like_date(&s) {
                return Err(format!("parameter `{name}` must be YYYY-MM-DD"));
            }
            Value::String(s)
        }
        "number" => {
            let n = s
                .parse::<f64>()
                .map_err(|e| format!("parameter `{name}` must be a number: {e}"))?;
            let num = serde_json::Number::from_f64(n)
                .ok_or_else(|| format!("parameter `{name}` must be a finite number"))?;
            Value::Number(num)
        }
        "boolean" => match s.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(format!("parameter `{name}` must be true or false")),
        },
        "enum" => {
            let ok = spec
                .enum_values
                .iter()
                .filter_map(value_as_scalar_string)
                .any(|choice| choice == s);
            if !ok {
                return Err(format!("parameter `{name}` is not an allowed enum value"));
            }
            Value::String(s)
        }
        other => return Err(format!("parameter `{name}` has unsupported type `{other}`")),
    };

    if !spec.enum_values.is_empty() && spec.kind != "enum" {
        let as_s = value_as_scalar_string(&value).unwrap_or_default();
        let ok = spec
            .enum_values
            .iter()
            .filter_map(value_as_scalar_string)
            .any(|choice| choice == as_s);
        if !ok {
            return Err(format!("parameter `{name}` is not an allowed enum value"));
        }
    }

    Ok(value)
}

fn first_open_url(doc: &TemplateDoc, bindings: &Bindings) -> Result<Url, String> {
    let Some(TemplateStep::Open { url, .. }) = doc.steps.first() else {
        return Err("steps[0] must be open".to_owned());
    };
    materialize_url(url, bindings)
}

fn materialize_step(
    step: &TemplateStep,
    snapshot: Option<&ActionSnapshot>,
    bindings: &Bindings,
) -> Result<Action, TemplateExecError> {
    match step {
        TemplateStep::Open { url, .. } => Ok(Action::Open {
            url: materialize_url(url, bindings)?.to_string(),
        }),
        TemplateStep::Fill { target, value, .. } => {
            let snap = snapshot
                .ok_or_else(|| TemplateExecError::new("fill step has no current page snapshot"))?;
            let elem = resolve_template_target(snap, target, bindings)?;
            Ok(Action::Fill {
                target: elem.ref_id,
                value: expr_to_string(value, bindings)?,
            })
        }
        TemplateStep::Click { target, .. } => {
            let snap = snapshot
                .ok_or_else(|| TemplateExecError::new("click step has no current page snapshot"))?;
            let elem = resolve_template_target(snap, target, bindings)?;
            Ok(Action::Click {
                target: elem.ref_id,
            })
        }
        TemplateStep::Submit { target, .. } => {
            let snap = snapshot.ok_or_else(|| {
                TemplateExecError::new("submit step has no current page snapshot")
            })?;
            let elem = resolve_template_target(snap, target, bindings)?;
            Ok(Action::Submit {
                target: elem.ref_id,
            })
        }
    }
}

fn materialize_url(url: &UrlTemplate, bindings: &Bindings) -> Result<Url, String> {
    match url {
        UrlTemplate::Literal(s) => Url::parse(s).map_err(|e| format!("invalid url `{s}`: {e}")),
        UrlTemplate::Structured(spec) => {
            let mut u = Url::parse(&spec.base)
                .map_err(|e| format!("invalid url base `{}`: {e}", spec.base))?;
            if !spec.query.is_empty() {
                let mut qp = u.query_pairs_mut();
                for (key, expr) in &spec.query {
                    let value = expr_to_string(expr, bindings)?;
                    qp.append_pair(key, &value);
                }
            }
            Ok(u)
        }
    }
}

fn domain_allowed(url: &str, domains: &[String]) -> bool {
    if domains.is_empty() {
        return true;
    }
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    domains.iter().any(|domain| {
        let d = domain.trim().trim_start_matches("*.").to_ascii_lowercase();
        let host = host.to_ascii_lowercase();
        host == d || host.ends_with(&format!(".{d}"))
    })
}

fn expr_to_string(expr: &ValueExpr, bindings: &Bindings) -> Result<String, String> {
    match expr {
        ValueExpr::Input { input } => bindings
            .values
            .get(input)
            .and_then(value_as_scalar_string)
            .ok_or_else(|| format!("input `{input}` is not bound to a scalar value")),
        ValueExpr::Literal(v) => value_as_scalar_string(v)
            .ok_or_else(|| "literal binding value must be string, number, or boolean".to_owned()),
    }
}

fn resolve_template_target(
    snapshot: &ActionSnapshot,
    target: &TargetSpec,
    bindings: &Bindings,
) -> Result<ElementRef, TemplateExecError> {
    let locator = match target {
        TargetSpec::Locator(locator) => locator,
        TargetSpec::ByInput { target_by_input } => {
            let value = bindings
                .values
                .get(&target_by_input.input)
                .and_then(value_as_scalar_string)
                .ok_or_else(|| {
                    TemplateExecError::new(format!(
                        "target input `{}` is not bound",
                        target_by_input.input
                    ))
                })?;
            target_by_input.cases.get(&value).ok_or_else(|| {
                TemplateExecError::new(format!(
                    "target input `{}` value `{}` has no locator case",
                    target_by_input.input, value
                ))
            })?
        }
    };
    resolve_locator(snapshot, locator)
}

fn resolve_locator(
    snapshot: &ActionSnapshot,
    locator: &LocatorSpec,
) -> Result<ElementRef, TemplateExecError> {
    let aria_label = locator
        .aria_label
        .as_deref()
        .or(locator.aria_label_dash.as_deref());
    let mut candidates: Vec<ElementRef> =
        if locator.text.is_some() || locator.selector.is_some() || aria_label.is_some() {
            resolve_locator_from_html(
                &snapshot.html,
                &snapshot.actions,
                locator.text.as_deref(),
                locator.selector.as_deref(),
                aria_label,
            )
            .map_err(|e| TemplateExecError::new(e.to_string()))?
        } else {
            snapshot.actions.clone()
        };

    candidates.retain(|el| locator_matches(el, locator));
    match candidates.len() {
        0 => Err(TemplateExecError::new(format!(
            "no element matched target {}",
            locator_summary(locator)
        ))),
        1 => Ok(candidates.remove(0)),
        _ => Err(TemplateExecError::with_detail(
            format!("ambiguous target {}", locator_summary(locator)),
            json!({
            "candidates": candidates,
            }),
        )),
    }
}

fn locator_matches(el: &ElementRef, locator: &LocatorSpec) -> bool {
    if let Some(role) = locator.role.as_deref() {
        if el.role != role {
            return false;
        }
    }
    if let Some(tag) = locator.tag.as_deref() {
        if el.tag != tag {
            return false;
        }
    }
    if let Some(name) = locator.name.as_deref() {
        if !el
            .name
            .as_deref()
            .map(|have| have.eq_ignore_ascii_case(name))
            .unwrap_or(false)
        {
            return false;
        }
    }
    if let Some(section) = locator.section.as_deref() {
        let want = section.trim_end_matches('/');
        let want_child = format!("{want}/");
        if el.section != want && !el.section.starts_with(&want_child) {
            return false;
        }
    }
    for (key, want) in &locator.attrs {
        if el.attrs.get(key) != Some(want) {
            return false;
        }
    }
    true
}

fn refresh_snapshot(
    session: &mut Option<heso_engine_js::JsSession>,
    current_url: &mut Url,
    current_actions: &mut Vec<ElementRef>,
) -> Result<ActionSnapshot, String> {
    let sess = session
        .as_mut()
        .ok_or_else(|| "template execution has no JS session".to_owned())?;
    wait_dom_quiet(sess);
    *current_url = sess.url().clone();
    let html = sess.document_html();
    let actions = heso_engine_fetch::extract_actions_from_html(&html);
    *current_actions = actions.clone();
    Ok(ActionSnapshot {
        url: current_url.clone(),
        html,
        actions,
    })
}

fn fail(kind: &str, message: impl Into<String>) -> ExitCode {
    let value = json!({
        "ok": false,
        "error": {
            "kind": kind,
            "message": message.into(),
        }
    });
    emit_failure(&value);
    ExitCode::from(1)
}

fn fail_step(
    kind: &str,
    index: usize,
    step: &TemplateStep,
    message: impl Into<String>,
    extra: Option<Value>,
) -> ExitCode {
    let mut error = json!({
        "kind": kind,
        "message": message.into(),
        "step_index": index,
        "step_id": step.id(),
        "verb": step.verb(),
    });
    if let Some(extra) = extra {
        if let Some(obj) = error.as_object_mut() {
            obj.insert("detail".to_owned(), extra);
        }
    }
    let value = json!({ "ok": false, "error": error });
    emit_failure(&value);
    ExitCode::from(1)
}

fn emit_failure(value: &Value) {
    if let Ok(s) = serde_json::to_string_pretty(value) {
        if writeln!(std::io::stdout(), "{s}").is_err() {
            eprintln!("{s}");
        }
    }
}

fn locator_summary(locator: &LocatorSpec) -> String {
    let mut parts = Vec::new();
    if let Some(v) = &locator.role {
        parts.push(format!("role={v}"));
    }
    if let Some(v) = &locator.name {
        parts.push(format!("name={v:?}"));
    }
    if let Some(v) = &locator.text {
        parts.push(format!("text={v:?}"));
    }
    if let Some(v) = &locator.selector {
        parts.push(format!("selector={v:?}"));
    }
    if let Some(v) = locator
        .aria_label
        .as_ref()
        .or(locator.aria_label_dash.as_ref())
    {
        parts.push(format!("aria_label={v:?}"));
    }
    if let Some(v) = &locator.tag {
        parts.push(format!("tag={v}"));
    }
    format!("{{{}}}", parts.join(", "))
}

fn value_as_scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
