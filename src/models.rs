//! Small provider adapter used by the `execute-model` Unix applet.
//!
//! It deliberately keeps transport and response parsing in one process. The
//! workflow runtime owns this applet as a normal child process, so cancellation
//! kills an in-flight request with the rest of its process group.

use anyhow::{anyhow, bail, Context, Result};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::time::Duration;
use url::{Host, Url};

const DEFAULT_MAX_BYTES: usize = 1024 * 1024;
const MAX_MAX_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_OPENAI_MAX_TOKENS: u32 = 512;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_LIST_ITEMS: usize = 128;
const MAX_ENV_FILE_BYTES: usize = 64 * 1024;
const JEV_DEFAULT_MODEL: &str = "jev-latest";
const MAX_BATCH_QUESTIONS: usize = 32;
const MAX_NAME_BYTES: usize = 128;
const JEV_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const OPENAI_CHAT_BASE: &str = "https://api.openai.com/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Jev,
    OpenAi,
}

/// OpenAI wire protocol. `Responses` is the default; `ChatCompletions` serves
/// llama.cpp, Ollama, vLLM and other compatible hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Api {
    Responses,
    ChatCompletions,
}

/// Jev question type of one batched question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuestionKind {
    Choice,
    Noul,
    Score,
}

impl QuestionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Noul => "noul",
            Self::Score => "score",
        }
    }
}

/// One named question inside a `jev.ask` batch, validated before any request.
#[derive(Clone, Debug, PartialEq)]
struct BatchQuestion {
    name: String,
    kind: QuestionKind,
    question: String,
    labels: Vec<String>,
    criteria: Option<BTreeMap<String, String>>,
    levels: Vec<String>,
    minimum: u8,
}

impl Provider {
    fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::OpenAi => "openai",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Classify,
    Judge,
    Score,
    Ask,
    Generate,
    Extract,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Classify => "classify",
            Self::Judge => "judge",
            Self::Score => "score",
            Self::Ask => "ask",
            Self::Generate => "generate",
            Self::Extract => "extract",
        }
    }
}

#[derive(Debug)]
struct Options {
    kind: Kind,
    provider: Provider,
    api: Api,
    model: Option<String>,
    question: Option<String>,
    instruction: Option<String>,
    /// For classify: the allowed labels. With `criteria`, these are its keys.
    labels: Vec<String>,
    /// Classify label descriptions, mutually exclusive with `--labels`.
    criteria: Option<BTreeMap<String, String>>,
    levels: Vec<String>,
    questions: Option<Vec<BatchQuestion>>,
    minimum: u8,
    max_tokens: Option<u32>,
    effort: Option<String>,
    temperature: Option<f64>,
    timeout_ms: u64,
    max_bytes: usize,
    env_file: Option<String>,
    endpoint: Option<String>,
    api_key_env: Option<String>,
    fixture: Option<String>,
    schema: Option<Value>,
}

#[derive(Clone, Copy, Debug)]
enum FailureClass {
    Configuration,
    Offline,
    Input,
    Fixture,
    FixtureMismatch,
    CredentialsMissing,
    Transport,
    HttpStatus(u16),
    Refusal,
    Incomplete,
    Schema,
    ProviderOutput,
    OutputLimit,
    Io,
}

#[derive(Debug)]
struct ModelFailure(FailureClass);

impl fmt::Display for ModelFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.0 {
            FailureClass::Configuration => "configuration",
            FailureClass::Offline => "offline",
            FailureClass::Input => "input",
            FailureClass::Fixture => "fixture",
            FailureClass::FixtureMismatch => "fixture_mismatch",
            FailureClass::CredentialsMissing => "credentials_missing",
            FailureClass::Transport => "transport",
            FailureClass::HttpStatus(_) => "http_status",
            FailureClass::Refusal => "refusal",
            FailureClass::Incomplete => "incomplete",
            FailureClass::Schema => "schema",
            FailureClass::ProviderOutput => "provider_output",
            FailureClass::OutputLimit => "output_limit",
            FailureClass::Io => "io",
        };
        match self.0 {
            FailureClass::HttpStatus(status) => write!(formatter, "{message} {status}"),
            _ => formatter.write_str(message),
        }
    }
}

impl StdError for ModelFailure {}

#[derive(Debug)]
struct DuplicateJsonKey;

impl fmt::Display for DuplicateJsonKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("duplicate JSON object key")
    }
}

impl StdError for DuplicateJsonKey {}

fn failure(class: FailureClass) -> anyhow::Error {
    ModelFailure(class).into()
}

fn enforce_offline_mode(offline: bool, has_fixture: bool) -> Result<()> {
    if offline && !has_fixture {
        return Err(failure(FailureClass::Offline));
    }
    Ok(())
}

/// A stable, sanitized diagnostic suitable for stderr and a runtime error field.
/// Provider data, request values, paths, credentials, and error causes are omitted.
pub fn diagnostic(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<ModelFailure>()
        .map(ToString::to_string)
        .unwrap_or_else(|| "provider_output".to_owned())
}

/// Run one provider request or deterministic fixture request.
///
/// Errors intentionally contain no request body, response body, endpoint, or
/// credential material. The CLI renders only a static failure class.
pub fn run<R: Read, W: Write>(args: &[String], input: &mut R, output: &mut W) -> Result<()> {
    let options = parse_args(args).map_err(|error| {
        if error.downcast_ref::<ModelFailure>().is_some() {
            error
        } else {
            failure(FailureClass::Configuration)
        }
    })?;
    enforce_offline_mode(
        std::env::var_os("EXECUTE_MODELS_OFFLINE").is_some(),
        options.fixture.is_some(),
    )?;
    let endpoint = endpoint_for(&options).map_err(|_| failure(FailureClass::Configuration))?;
    validate_endpoint(&endpoint).map_err(|_| failure(FailureClass::Configuration))?;
    let parsed = Url::parse(&endpoint).map_err(|_| failure(FailureClass::Configuration))?;
    let transport = if options.fixture.is_some() {
        "fixture"
    } else if parsed.scheme() == "https" {
        "https"
    } else {
        "http_loopback"
    };
    let state = read_state(input, options.max_bytes).map_err(|_| failure(FailureClass::Input))?;
    let semantic =
        semantic_request(&options, state).map_err(|_| failure(FailureClass::Configuration))?;
    let request = provider_request(&options, semantic.get("state"))
        .map_err(|_| failure(FailureClass::Configuration))?;
    let request = encode_bounded(&request, options.max_bytes)
        .map_err(|_| failure(FailureClass::OutputLimit))?;
    let provider_response = if let Some(path) = options.fixture.as_deref() {
        read_fixture(path, &semantic, options.max_bytes).map_err(|error| {
            if error.downcast_ref::<ModelFailure>().is_some() {
                error
            } else {
                failure(FailureClass::Fixture)
            }
        })?
    } else {
        let key = select_credential(
            options.provider,
            &parsed,
            options.api_key_env.as_deref(),
            options.env_file.as_deref(),
            &|name: &str| std::env::var(name).ok(),
        )
        .map_err(|error| {
            if error.downcast_ref::<ModelFailure>().is_some() {
                error
            } else {
                failure(FailureClass::Configuration)
            }
        })?;
        post_json(
            &endpoint,
            key.as_deref(),
            &request,
            options.timeout_ms,
            options.max_bytes,
        )?
    };
    let normalized = normalize_response(&options, &semantic, &provider_response, transport)
        .map_err(|error| {
            if error.downcast_ref::<ModelFailure>().is_some() {
                error
            } else {
                failure(FailureClass::ProviderOutput)
            }
        })?;
    let bytes = encode_bounded(&normalized, options.max_bytes)
        .map_err(|_| failure(FailureClass::OutputLimit))?;
    output
        .write_all(&bytes)
        .map_err(|_| failure(FailureClass::Io))?;
    output
        .write_all(b"\n")
        .map_err(|_| failure(FailureClass::Io))?;
    Ok(())
}

/// Concise stable command synopsis for scripts and `execute-model --help`.
pub fn help() -> &'static str {
    "Usage: execute-model <classify|judge|score|ask|generate|extract> [options]\n\
Reads state/input bytes from stdin and writes one execute.model/1 JSON result.\n\
Options: --provider jev|openai --model NAME --question TEXT --instruction TEXT\n\
         --labels LABEL... | --criteria JSON --levels LEVEL... --questions JSON\n\
         --schema JSON --minimum 0..100 --max-tokens N --timeout-ms N\n\
         --max-bytes N --env-file PATH --endpoint URL --api-key-env NAME\n\
         --fixture PATH\n\
         --api responses|chat_completions --effort none|low|medium|high|xhigh|max\n\
         --temperature 0..2\n\
Jev defaults to jev-latest. OpenAI requires --model. Fixture mode never reads\n\
credentials or contacts a network service. OpenAI output is capped at 512\n\
tokens unless --max-tokens overrides it. Values beginning with -- cannot be\n\
used as labels or levels. Set EXECUTE_MODELS_OFFLINE to reject live requests.\n\
Default provider keys go only to the provider default host. A custom endpoint\n\
gets a key only from --api-key-env NAME; otherwise it gets none. No automatic\n\
retries are\n\
performed."
}

fn parse_args(args: &[String]) -> Result<Options> {
    if args.len() == 1 && args[0] == "--help" {
        bail!("help requested");
    }
    let Some(mode) = args.first() else {
        bail!("missing operation");
    };
    let kind = match mode.as_str() {
        "classify" => Kind::Classify,
        "judge" => Kind::Judge,
        "score" => Kind::Score,
        "ask" => Kind::Ask,
        "generate" => Kind::Generate,
        "extract" => Kind::Extract,
        _ => bail!("invalid operation"),
    };
    let mut options = Options {
        kind,
        provider: Provider::Jev,
        api: Api::Responses,
        model: None,
        question: None,
        instruction: None,
        labels: Vec::new(),
        criteria: None,
        levels: Vec::new(),
        questions: None,
        minimum: 0,
        max_tokens: None,
        effort: None,
        temperature: None,
        timeout_ms: DEFAULT_TIMEOUT_MS,
        max_bytes: DEFAULT_MAX_BYTES,
        env_file: None,
        endpoint: None,
        api_key_env: None,
        fixture: None,
        schema: None,
    };
    let mut seen = BTreeSet::new();
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        if flag == "--labels" || flag == "--levels" {
            if !seen.insert(flag.to_owned()) {
                bail!("duplicate list option");
            }
            index += 1;
            let mut values = Vec::new();
            while index < args.len() && !args[index].starts_with("--") {
                values.push(args[index].clone());
                index += 1;
            }
            if values.is_empty() {
                bail!("list option requires values");
            }
            if flag == "--labels" {
                options.labels = values;
            } else {
                options.levels = values;
            }
            continue;
        }
        if !seen.insert(flag.to_owned()) {
            bail!("duplicate option");
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| anyhow!("option requires a value"))?;
        match flag {
            "--provider" => {
                options.provider = match value.as_str() {
                    "jev" => Provider::Jev,
                    "openai" => Provider::OpenAi,
                    _ => bail!("unsupported provider"),
                }
            }
            "--api" => {
                options.api = match value.as_str() {
                    "responses" => Api::Responses,
                    "chat_completions" => Api::ChatCompletions,
                    _ => bail!("unsupported api"),
                }
            }
            "--model" => options.model = Some(nonempty(value, "model")?),
            "--question" => options.question = Some(nonempty(value, "question")?),
            "--instruction" => options.instruction = Some(nonempty(value, "instruction")?),
            "--criteria" => options.criteria = Some(parse_criteria(value)?),
            "--questions" => options.questions = Some(parse_questions(value)?),
            "--effort" => options.effort = Some(parse_effort(value)?),
            "--temperature" => options.temperature = Some(parse_temperature(value)?),
            "--minimum" => options.minimum = parse_integer(value, 0, 100, "minimum")? as u8,
            "--max-tokens" => {
                options.max_tokens = Some(parse_integer(value, 1, 65_536, "max-tokens")? as u32)
            }
            "--timeout-ms" => {
                options.timeout_ms = parse_integer(value, 1, MAX_TIMEOUT_MS, "timeout-ms")?
            }
            "--max-bytes" => {
                options.max_bytes =
                    parse_integer(value, 1, MAX_MAX_BYTES as u64, "max-bytes")? as usize
            }
            "--env-file" => options.env_file = Some(nonempty(value, "env-file")?),
            "--endpoint" => options.endpoint = Some(nonempty(value, "endpoint")?),
            "--api-key-env" => {
                let name = nonempty(value, "api-key-env")?;
                if !valid_env_name(&name) || is_default_credential_name(&name) {
                    bail!("api-key-env must name a non-default environment variable");
                }
                options.api_key_env = Some(name);
            }
            "--fixture" => {
                if value.is_empty() {
                    return Err(failure(FailureClass::Fixture));
                }
                options.fixture = Some(value.clone());
            }
            "--schema" => {
                let schema = parse_unique_json(value.as_bytes())
                    .map_err(|_| failure(FailureClass::Schema))?;
                validate_schema_definition(&schema).map_err(|_| failure(FailureClass::Schema))?;
                options.schema = Some(schema);
            }
            _ => bail!("unknown option"),
        }
        index += 2;
    }
    validate_options(&mut options, &seen)?;
    Ok(options)
}

fn parse_effort(value: &str) -> Result<String> {
    match value {
        "none" | "low" | "medium" | "high" | "xhigh" | "max" => Ok(value.to_owned()),
        _ => bail!("effort is not supported"),
    }
}

fn parse_temperature(value: &str) -> Result<f64> {
    let parsed: Value = serde_json::from_str(value).context("invalid temperature")?;
    let temperature = parsed
        .as_f64()
        .filter(|number| number.is_finite() && (0.0..=2.0).contains(number))
        .ok_or_else(|| anyhow!("temperature is outside its supported range"))?;
    // Normalize negative zero so the semantic request has one canonical form.
    Ok(temperature + 0.0)
}

fn validate_label(label: &str) -> Result<()> {
    if label.is_empty() || label.starts_with("--") || label.contains('\0') {
        bail!("labels must be nonempty, must not contain NUL, and must not begin with --");
    }
    Ok(())
}

/// Label-to-description map from a JSON object. Each label is a valid choice
/// label; each description is nonempty text.
fn criteria_from_value(value: &Value) -> Result<BTreeMap<String, String>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("criteria must be an object"))?;
    if object.len() < 2 || object.len() > MAX_LIST_ITEMS {
        bail!("criteria needs between two and 128 entries");
    }
    let mut criteria = BTreeMap::new();
    for (label, description) in object {
        validate_label(label)?;
        let description = description
            .as_str()
            .filter(|text| !text.trim().is_empty() && !text.contains('\0'))
            .ok_or_else(|| anyhow!("criteria descriptions must be nonempty text"))?;
        criteria.insert(label.clone(), description.to_owned());
    }
    Ok(criteria)
}

fn parse_criteria(value: &str) -> Result<BTreeMap<String, String>> {
    let parsed = parse_unique_json(value.as_bytes())?;
    criteria_from_value(&parsed)
}

/// A nonempty-element, unique, bounded list of labels or levels.
fn string_list(value: Option<&Value>, name: &str) -> Result<Vec<String>> {
    let items = value
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("{name} must be a list of text"))?;
    if items.len() < 2 || items.len() > MAX_LIST_ITEMS {
        bail!("{name} needs between two and 128 entries");
    }
    let mut unique = BTreeSet::new();
    let mut list = Vec::with_capacity(items.len());
    for item in items {
        let text = item
            .as_str()
            .ok_or_else(|| anyhow!("{name} must be a list of text"))?;
        validate_label(text)?;
        if !unique.insert(text) {
            bail!("{name} entries must be unique");
        }
        list.push(text.to_owned());
    }
    Ok(list)
}

fn parse_question(name: &str, spec: &Value) -> Result<BatchQuestion> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.contains('\0') {
        bail!("question names must be nonempty and at most 128 bytes");
    }
    let object = spec
        .as_object()
        .ok_or_else(|| anyhow!("question must be an object"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "type" | "question" | "labels" | "criteria" | "levels" | "minimum"
        )
    }) {
        bail!("question contains an unsupported field");
    }
    let kind = match object.get("type").and_then(Value::as_str) {
        Some("choice") => QuestionKind::Choice,
        Some("noul") => QuestionKind::Noul,
        Some("score") => QuestionKind::Score,
        _ => bail!("question type must be choice, noul, or score"),
    };
    let question = object
        .get("question")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty() && !text.contains('\0'))
        .ok_or_else(|| anyhow!("question text must be nonempty"))?
        .to_owned();
    let minimum = match object.get("minimum") {
        None => 0,
        Some(value) => value
            .as_u64()
            .filter(|number| *number <= 100)
            .ok_or_else(|| anyhow!("minimum must be an integer from 0 to 100"))?
            as u8,
    };
    let has = |key: &str| object.contains_key(key);
    let (labels, criteria, levels) = match kind {
        QuestionKind::Choice => {
            if has("levels") {
                bail!("choice questions take labels or criteria, not levels");
            }
            match (has("labels"), has("criteria")) {
                (true, false) => (
                    string_list(object.get("labels"), "labels")?,
                    None,
                    Vec::new(),
                ),
                (false, true) => {
                    let criteria = criteria_from_value(&object["criteria"])?;
                    let labels = criteria.keys().cloned().collect();
                    (labels, Some(criteria), Vec::new())
                }
                _ => bail!("choice questions need exactly one of labels or criteria"),
            }
        }
        QuestionKind::Noul => {
            if has("labels") || has("criteria") || has("levels") {
                bail!("noul questions take no labels, criteria, or levels");
            }
            (Vec::new(), None, Vec::new())
        }
        QuestionKind::Score => {
            if has("labels") || has("criteria") {
                bail!("score questions take levels, not labels or criteria");
            }
            (
                Vec::new(),
                None,
                string_list(object.get("levels"), "levels")?,
            )
        }
    };
    Ok(BatchQuestion {
        name: name.to_owned(),
        kind,
        question,
        labels,
        criteria,
        levels,
        minimum,
    })
}

fn parse_questions(value: &str) -> Result<Vec<BatchQuestion>> {
    let parsed = parse_unique_json(value.as_bytes())?;
    let object = parsed
        .as_object()
        .ok_or_else(|| anyhow!("questions must be an object"))?;
    if object.is_empty() || object.len() > MAX_BATCH_QUESTIONS {
        bail!("ask needs between one and 32 questions");
    }
    object
        .iter()
        .map(|(name, spec)| parse_question(name, spec))
        .collect()
}

fn validate_options(options: &mut Options, seen: &BTreeSet<String>) -> Result<()> {
    if options.labels.len() > MAX_LIST_ITEMS || options.levels.len() > MAX_LIST_ITEMS {
        bail!("too many labels or levels");
    }
    for values in [&options.labels, &options.levels] {
        let mut unique = BTreeSet::new();
        for value in values {
            if value.is_empty() || value.starts_with("--") || !unique.insert(value) {
                bail!("labels and levels must be nonempty, unique, and not begin with --");
            }
        }
    }
    if let Some(criteria) = &options.criteria {
        if !options.labels.is_empty() {
            bail!("classify accepts labels or criteria, not both");
        }
        options.labels = criteria.keys().cloned().collect();
    }
    if options.kind != Kind::Classify && options.criteria.is_some() {
        bail!("criteria are only supported by classify");
    }
    if options.kind != Kind::Ask && options.questions.is_some() {
        bail!("questions are only supported by ask");
    }
    let question_mode = matches!(options.kind, Kind::Classify | Kind::Judge | Kind::Score);
    if question_mode {
        if options.question.as_deref().is_none_or(str::is_empty) || options.instruction.is_some() {
            bail!("operation requires --question only");
        }
    } else if options.kind == Kind::Ask {
        if options.question.is_some()
            || options.instruction.is_some()
            || !options.labels.is_empty()
            || !options.levels.is_empty()
            || seen.contains("--minimum")
            || options.schema.is_some()
        {
            bail!("ask takes only questions, model, and transport options");
        }
    } else if options.instruction.as_deref().is_none_or(str::is_empty) || options.question.is_some()
    {
        bail!("operation requires --instruction only");
    }
    match (options.provider, options.kind) {
        (Provider::Jev, Kind::Classify) if options.labels.len() >= 2 => {}
        (Provider::Jev, Kind::Classify) => {
            bail!("classify needs labels or criteria with at least two entries")
        }
        (Provider::Jev, Kind::Judge) if options.labels.is_empty() && options.levels.is_empty() => {}
        (Provider::Jev, Kind::Judge) => bail!("judge does not accept labels or levels"),
        (Provider::Jev, Kind::Score) if options.levels.len() >= 2 && options.labels.is_empty() => {}
        (Provider::Jev, Kind::Score) => bail!("score needs at least two levels"),
        (Provider::Jev, Kind::Ask) if options.questions.is_some() => {}
        (Provider::OpenAi, Kind::Generate)
            if options.labels.is_empty()
                && options.levels.is_empty()
                && options.schema.is_none() => {}
        (Provider::OpenAi, Kind::Extract)
            if options.labels.is_empty()
                && options.levels.is_empty()
                && options.schema.is_some() => {}
        _ => bail!("operation is not supported by the selected provider"),
    }
    if options.provider == Provider::OpenAi && options.model.as_deref().is_none_or(str::is_empty) {
        bail!("OpenAI requires an explicit model");
    }
    if options.provider == Provider::Jev {
        if options.max_tokens.is_some() {
            bail!("max-tokens is only supported by OpenAI Responses");
        }
        if options.schema.is_some() {
            bail!("schema is only supported by OpenAI extraction");
        }
        if seen.contains("--api") {
            bail!("api is only supported by OpenAI");
        }
        if options.effort.is_some() || options.temperature.is_some() {
            bail!("effort and temperature are only supported by OpenAI");
        }
    }
    if !matches!(options.kind, Kind::Classify | Kind::Ask) && !options.labels.is_empty() {
        bail!("labels are only supported by classify");
    }
    if !matches!(options.kind, Kind::Score | Kind::Ask) && !options.levels.is_empty() {
        bail!("levels are only supported by score");
    }
    Ok(())
}

fn nonempty(value: &str, name: &str) -> Result<String> {
    if value.trim().is_empty() || value.contains('\0') {
        bail!("{name} must be nonempty text");
    }
    Ok(value.to_owned())
}

fn parse_integer(value: &str, minimum: u64, maximum: u64, name: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .with_context(|| format!("invalid {name}"))?;
    if !(minimum..=maximum).contains(&parsed) {
        bail!("{name} is outside its supported range");
    }
    Ok(parsed)
}

struct UniqueJson(Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        let number = serde_json::Number::from_f64(value)
            .ok_or_else(|| E::custom("non-finite JSON number"))?;
        Ok(UniqueJson(Value::Number(number)))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::String(value)))
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Null))
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(UniqueJson(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(UniqueJson(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        let mut keys = BTreeSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let UniqueJson(value) = object.next_value()?;
            values.insert(key, value);
        }
        Ok(UniqueJson(Value::Object(values)))
    }
}

fn parse_unique_json(bytes: &[u8]) -> Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let UniqueJson(value) = match UniqueJson::deserialize(&mut deserializer) {
        Ok(value) => value,
        Err(error) if error.to_string().starts_with("duplicate JSON object key") => {
            return Err(DuplicateJsonKey.into());
        }
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = deserializer.end() {
        if error.to_string().starts_with("duplicate JSON object key") {
            return Err(DuplicateJsonKey.into());
        }
        return Err(error.into());
    }
    Ok(value)
}

fn read_state<R: Read>(input: &mut R, limit: usize) -> Result<Value> {
    let mut bytes = Vec::new();
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("reading model state")?;
    if bytes.len() > limit {
        bail!("model state exceeds byte limit");
    }
    let text = String::from_utf8(bytes).context("model state must be UTF-8")?;
    match parse_unique_json(text.as_bytes()) {
        Ok(value @ (Value::String(_) | Value::Array(_) | Value::Object(_))) => Ok(value),
        Ok(_) => Ok(Value::String(text)),
        Err(error) if error.downcast_ref::<DuplicateJsonKey>().is_some() => {
            Err(failure(FailureClass::Input))
        }
        Err(_) => Ok(Value::String(text)),
    }
}

fn semantic_request(options: &Options, state: Value) -> Result<Value> {
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| JEV_DEFAULT_MODEL.to_owned());
    let mut request = Map::new();
    request.insert("provider".into(), json!(options.provider.as_str()));
    request.insert("kind".into(), json!(options.kind.as_str()));
    request.insert("model".into(), json!(model));
    request.insert("state".into(), state);
    match options.provider {
        Provider::Jev if options.kind == Kind::Ask => {
            request.insert("questions".into(), batch_questions_semantic(options));
        }
        Provider::Jev => {
            request.insert("question".into(), json!(options.question));
            request.insert("minimum".into(), json!(options.minimum));
            if options.kind == Kind::Classify {
                // Keep the semantic key for each form: criteria replaces labels.
                match &options.criteria {
                    Some(criteria) => request.insert("criteria".into(), json!(criteria)),
                    None => request.insert("labels".into(), json!(options.labels)),
                };
            }
            if options.kind == Kind::Score {
                request.insert("levels".into(), json!(options.levels));
            }
        }
        Provider::OpenAi => {
            request.insert("instruction".into(), json!(options.instruction));
            if let Some(schema) = &options.schema {
                request.insert("schema".into(), schema.clone());
            }
            if let Some(max_tokens) = options.max_tokens {
                request.insert("max_tokens".into(), json!(max_tokens));
            }
            // Transport-only fields stay out of the request. `api` changes the
            // response shape a fixture must describe, so it is semantic.
            if options.api == Api::ChatCompletions {
                request.insert("api".into(), json!("chat_completions"));
            }
            if let Some(effort) = &options.effort {
                request.insert("effort".into(), json!(effort));
            }
            if let Some(temperature) = options.temperature {
                request.insert("temperature".into(), json!(temperature));
            }
        }
    }
    Ok(Value::Object(request))
}

/// Canonical batch question map used by fixtures. Every question carries its
/// type, text and minimum; choice questions carry either labels or criteria.
fn batch_questions_semantic(options: &Options) -> Value {
    let questions = options.questions.as_deref().unwrap_or_default();
    let map = questions
        .iter()
        .map(|question| {
            let mut spec = Map::new();
            spec.insert("type".into(), json!(question.kind.as_str()));
            spec.insert("question".into(), json!(question.question));
            spec.insert("minimum".into(), json!(question.minimum));
            match question.kind {
                QuestionKind::Choice => match &question.criteria {
                    Some(criteria) => spec.insert("criteria".into(), json!(criteria)),
                    None => spec.insert("labels".into(), json!(question.labels)),
                },
                QuestionKind::Noul => None,
                QuestionKind::Score => spec.insert("levels".into(), json!(question.levels)),
            };
            (question.name.clone(), Value::Object(spec))
        })
        .collect::<Map<_, _>>();
    Value::Object(map)
}

/// The Jev question object for one decision. A choice's criteria map gives each
/// label its description; without criteria, each label is its own criterion.
fn jev_question(
    kind: QuestionKind,
    instructions: &str,
    labels: &[String],
    criteria: Option<&BTreeMap<String, String>>,
    levels: &[String],
) -> Value {
    let mut question = Map::new();
    question.insert("type".into(), json!(kind.as_str()));
    question.insert("instructions".into(), json!(instructions));
    match kind {
        QuestionKind::Choice => {
            let map = match criteria {
                Some(criteria) => criteria
                    .iter()
                    .map(|(label, text)| (label.clone(), Value::String(text.clone())))
                    .collect::<Map<_, _>>(),
                None => labels
                    .iter()
                    .map(|label| (label.clone(), Value::String(label.clone())))
                    .collect::<Map<_, _>>(),
            };
            question.insert("criteria".into(), Value::Object(map));
        }
        QuestionKind::Noul => {}
        QuestionKind::Score => {
            question.insert("criteria".into(), json!(levels));
        }
    }
    Value::Object(question)
}

fn provider_request(options: &Options, state: Option<&Value>) -> Result<Value> {
    let semantic = semantic_request(options, state.cloned().unwrap_or(Value::Null))?;
    let model = semantic["model"].clone();
    match options.provider {
        Provider::Jev => {
            if options.kind == Kind::Ask {
                let questions = options
                    .questions
                    .as_deref()
                    .ok_or_else(|| anyhow!("missing batch questions"))?
                    .iter()
                    .map(|question| {
                        (
                            question.name.clone(),
                            jev_question(
                                question.kind,
                                &question.question,
                                &question.labels,
                                question.criteria.as_ref(),
                                &question.levels,
                            ),
                        )
                    })
                    .collect::<Map<_, _>>();
                return Ok(json!({
                    "model": model,
                    "state": semantic["state"],
                    "questions": Value::Object(questions)
                }));
            }
            let text = options.question.as_deref().unwrap_or_default();
            let question = match options.kind {
                Kind::Classify => jev_question(
                    QuestionKind::Choice,
                    text,
                    &options.labels,
                    options.criteria.as_ref(),
                    &[],
                ),
                Kind::Judge => jev_question(QuestionKind::Noul, text, &[], None, &[]),
                Kind::Score => jev_question(QuestionKind::Score, text, &[], None, &options.levels),
                _ => bail!("unsupported Jev operation"),
            };
            Ok(json!({
                "model": model,
                "state": semantic["state"],
                "questions": { "decision": question }
            }))
        }
        Provider::OpenAi => {
            let state = semantic
                .get("state")
                .ok_or_else(|| anyhow!("missing model state"))?;
            let input = match state {
                Value::String(text) => text.clone(),
                value => serde_json::to_string(value).context("encoding model state")?,
            };
            let max_tokens = options.max_tokens.unwrap_or(DEFAULT_OPENAI_MAX_TOKENS);
            let mut request = match options.api {
                Api::Responses => json!({
                    "model": model,
                    "store": false,
                    "instructions": options.instruction,
                    "input": input,
                    "max_output_tokens": max_tokens
                }),
                Api::ChatCompletions => json!({
                    "model": model,
                    "stream": false,
                    "messages": [
                        { "role": "system", "content": options.instruction },
                        { "role": "user", "content": input }
                    ],
                    "max_tokens": max_tokens
                }),
            };
            if let Some(schema) = &options.schema {
                match options.api {
                    Api::Responses => {
                        request["text"] = json!({
                            "format": {
                                "type": "json_schema",
                                "name": "execute_model_extract",
                                "strict": true,
                                "schema": schema
                            }
                        });
                    }
                    Api::ChatCompletions => {
                        request["response_format"] = json!({
                            "type": "json_schema",
                            "json_schema": {
                                "name": "execute_model_extract",
                                "strict": true,
                                "schema": schema
                            }
                        });
                    }
                }
            }
            if let Some(effort) = &options.effort {
                match options.api {
                    Api::Responses => request["reasoning"] = json!({ "effort": effort }),
                    Api::ChatCompletions => request["reasoning_effort"] = json!(effort),
                }
            }
            if let Some(temperature) = options.temperature {
                request["temperature"] = json!(temperature);
            }
            Ok(request)
        }
    }
}

fn read_fixture(path: &str, expected: &Value, limit: usize) -> Result<Value> {
    let mut file = File::open(path).context("opening fixture")?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((limit.saturating_mul(4).saturating_add(4096)) as u64)
        .read_to_end(&mut bytes)
        .context("reading fixture")?;
    if bytes.len() > limit.saturating_mul(4).saturating_add(4096) {
        bail!("fixture exceeds size limit");
    }
    let fixture = parse_unique_json(&bytes).context("parsing fixture")?;
    let object = fixture
        .as_object()
        .ok_or_else(|| anyhow!("fixture must be an object"))?;
    if object.len() != 2
        || !object.contains_key("expected_request")
        || !object.contains_key("response")
    {
        bail!("fixture must contain expected_request and response only");
    }
    if &object["expected_request"] != expected {
        return Err(failure(FailureClass::FixtureMismatch));
    }
    let response = object["response"].clone();
    if !response.is_object() {
        bail!("fixture response must be a JSON object");
    }
    Ok(response)
}

/// The URL that receives the provider request. Responses uses `endpoint` as the
/// full URL; Chat Completions treats it as the base and appends `/chat/completions`.
fn endpoint_for(options: &Options) -> Result<String> {
    match (options.provider, options.api) {
        (Provider::Jev, _) => Ok(options
            .endpoint
            .clone()
            .unwrap_or_else(|| JEV_ENDPOINT.to_owned())),
        (Provider::OpenAi, Api::Responses) => Ok(options
            .endpoint
            .clone()
            .unwrap_or_else(|| OPENAI_ENDPOINT.to_owned())),
        (Provider::OpenAi, Api::ChatCompletions) => {
            let base = options.endpoint.as_deref().unwrap_or(OPENAI_CHAT_BASE);
            Ok(format!("{}/chat/completions", base.trim_end_matches('/')))
        }
    }
}

/// Validate transport endpoints before reading credentials or opening a socket.
pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    // `Url::username()` is empty for an empty userinfo section such as
    // `http://@localhost`; reject the delimiter in the raw authority too.
    let authority = endpoint
        .split_once("://")
        .map(|(_, remainder)| remainder.split(['/', '?', '#']).next().unwrap_or(""));
    if authority.is_some_and(|authority| authority.contains('@')) {
        bail!("endpoint cannot contain userinfo, query, or fragment");
    }
    let parsed = Url::parse(endpoint).context("invalid endpoint URL")?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.host().is_none()
    {
        bail!("endpoint cannot contain userinfo, query, or fragment");
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(parsed.host().expect("checked host")) => Ok(()),
        _ => bail!("endpoint must use HTTPS or HTTP to a loopback host"),
    }
}

fn is_loopback(host: Host<&str>) -> bool {
    match host {
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
    }
}

/// Environment names that carry the provider's default key. They are never
/// valid for `api_key_env`, and they reach a request only on the default host.
const DEFAULT_CREDENTIAL_NAMES: [&str; 5] = [
    "OPENAI_API_KEY",
    "openai",
    "JEV_API_KEY",
    "TYPESAFE_API_KEY",
    "jev",
];

fn is_default_credential_name(name: &str) -> bool {
    DEFAULT_CREDENTIAL_NAMES
        .iter()
        .any(|default| default.eq_ignore_ascii_case(name))
}

/// Default key names in lookup order for one provider.
fn provider_credential_names(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::OpenAi => &["OPENAI_API_KEY", "openai"],
        Provider::Jev => &["JEV_API_KEY", "TYPESAFE_API_KEY", "jev"],
    }
}

/// True only for the provider's own HTTPS host on the default port. The host
/// check is exact, so lookalike or subdomain hosts do not qualify.
fn is_default_host(provider: Provider, endpoint: &Url) -> bool {
    let expected = match provider {
        Provider::Jev => "api.typesafe.ai",
        Provider::OpenAi => "api.openai.com",
    };
    endpoint.scheme() == "https"
        && endpoint.port().is_none()
        && endpoint
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(expected))
}

fn check_secret(value: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        return Err(failure(FailureClass::Configuration));
    }
    Ok(())
}

/// Choose the bearer credential for one live request. Its rules are the
/// security boundary for provider keys:
///
/// - `api_key_env` names one explicit variable, read from the process
///   environment or the explicit env file. It may go to any endpoint.
/// - Default provider keys (process or env file) are attached only to the
///   provider's default host. Any other endpoint never receives them.
/// - A custom endpoint without `api_key_env` gets no `Authorization` header.
///
/// `process` supplies process environment values, so tests can inject them.
fn select_credential(
    provider: Provider,
    endpoint: &Url,
    api_key_env: Option<&str>,
    env_file: Option<&str>,
    process: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    if let Some(name) = api_key_env {
        let wanted = [name];
        let file_values = match env_file {
            Some(path) => parse_env_file(path, &wanted)?,
            None => BTreeMap::new(),
        };
        let value = process(name)
            .or_else(|| file_values.get(name).cloned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| failure(FailureClass::CredentialsMissing))?;
        check_secret(&value)?;
        return Ok(Some(value));
    }
    if !is_default_host(provider, endpoint) {
        return Ok(None);
    }
    let names = provider_credential_names(provider);
    let value = match names
        .iter()
        .find_map(|name| process(name).filter(|v| !v.is_empty()))
    {
        Some(value) => value,
        None => {
            let file_values = match env_file {
                Some(path) => parse_env_file(path, names)?,
                None => BTreeMap::new(),
            };
            names
                .iter()
                .find_map(|name| file_values.get(*name).cloned())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| failure(FailureClass::CredentialsMissing))?
        }
    };
    check_secret(&value)?;
    Ok(Some(value))
}

/// Read only the requested names from an explicit env file. Other entries are
/// skipped without being parsed, so unrelated lines cannot affect a request.
fn parse_env_file(path: &str, wanted: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut file = File::open(path).context("opening explicit environment file")?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_ENV_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("reading explicit environment file")?;
    if bytes.len() > MAX_ENV_FILE_BYTES {
        bail!("environment file exceeds size limit");
    }
    let text = String::from_utf8(bytes).context("environment file is not UTF-8")?;
    let mut values = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((name, raw_value)) = line.split_once('=') else {
            bail!("invalid environment file entry");
        };
        let name = name.trim();
        if !valid_env_name(name) {
            bail!("invalid environment file name");
        }
        if !wanted.contains(&name) {
            continue;
        }
        let raw_value = raw_value.trim();
        let value = parse_env_value(raw_value)?;
        values.insert(name.to_owned(), value);
    }
    Ok(values)
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn parse_env_value(raw: &str) -> Result<String> {
    if let Some(inner) = raw.strip_prefix('\'') {
        let Some(inner) = inner.strip_suffix('\'') else {
            bail!("invalid quoted environment value");
        };
        return Ok(inner.to_owned());
    }
    if let Some(inner) = raw.strip_prefix('"') {
        let Some(inner) = inner.strip_suffix('"') else {
            bail!("invalid quoted environment value");
        };
        let mut output = String::with_capacity(inner.len());
        let mut escaped = false;
        for ch in inner.chars() {
            if escaped {
                match ch {
                    'n' => output.push('\n'),
                    'r' => output.push('\r'),
                    't' => output.push('\t'),
                    '\\' | '"' => output.push(ch),
                    _ => bail!("invalid escape in environment value"),
                }
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else {
                output.push(ch);
            }
        }
        if escaped {
            bail!("unfinished escape in environment value");
        }
        return Ok(output);
    }
    Ok(raw
        .split_once(" #")
        .map_or(raw, |(value, _)| value)
        .trim()
        .to_owned())
}

fn post_json(
    endpoint: &str,
    key: Option<&str>,
    body: &[u8],
    timeout_ms: u64,
    max_bytes: usize,
) -> Result<Value> {
    validate_endpoint(endpoint).map_err(|_| failure(FailureClass::Configuration))?;
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(timeout_ms)))
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None);
    #[cfg(windows)]
    let config = config.tls_config(
        ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .build(),
    );
    let config = config.build();
    let agent: ureq::Agent = config.into();
    let request = agent
        .post(endpoint)
        .header("Accept", "application/json")
        .content_type("application/json");
    let request = match key {
        Some(key) => request.header("Authorization", &format!("Bearer {key}")),
        None => request,
    };
    let mut response = request.send(body).map_err(|error| match error {
        ureq::Error::StatusCode(status) => failure(FailureClass::HttpStatus(status)),
        _ => failure(FailureClass::Transport),
    })?;
    if !response.status().is_success() {
        return Err(failure(FailureClass::HttpStatus(
            response.status().as_u16(),
        )));
    }
    let response_bytes = response
        .body_mut()
        .with_config()
        .limit(max_bytes as u64)
        .read_to_vec()
        .map_err(|_| failure(FailureClass::OutputLimit))?;
    parse_unique_json(&response_bytes).map_err(|_| failure(FailureClass::ProviderOutput))
}

fn normalize_response(
    options: &Options,
    semantic: &Value,
    raw: &Value,
    transport: &str,
) -> Result<Value> {
    let model = required_string(raw, "model")?;
    let usage = match (options.provider, options.api) {
        (Provider::Jev, _) => normalize_jev_usage(raw.get("usage"))?,
        (Provider::OpenAi, Api::Responses) => normalize_openai_usage(raw.get("usage"))?,
        (Provider::OpenAi, Api::ChatCompletions) => normalize_chat_usage(raw.get("usage"))?,
    };
    let mut normalized = Map::new();
    normalized.insert("protocol".into(), json!("execute.model/1"));
    normalized.insert("provider".into(), json!(options.provider.as_str()));
    normalized.insert("model".into(), json!(model));
    normalized.insert("kind".into(), json!(options.kind.as_str()));
    normalized.insert("transport".into(), json!(transport));
    normalized.insert("usage".into(), usage);
    match options.provider {
        Provider::Jev => normalize_jev_answer(options, raw, &mut normalized)?,
        Provider::OpenAi => normalize_openai_answer(options, semantic, raw, &mut normalized)?,
    }
    Ok(Value::Object(normalized))
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("provider response is missing a required string"))
}

fn required_object<'a>(value: &'a Value, key: &str) -> Result<&'a Map<String, Value>> {
    value
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("provider response is missing a required object"))
}

fn normalize_jev_usage(value: Option<&Value>) -> Result<Value> {
    let usage = value
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("provider response usage is malformed"))?;
    let input = nonnegative_integer(usage.get("input_tokens"))?;
    let output = nonnegative_integer(usage.get("output_tokens"))?;
    let total = input
        .checked_add(output)
        .ok_or_else(|| anyhow!("provider token counts overflow"))?;
    Ok(json!({ "input_tokens": input, "output_tokens": output, "total_tokens": total }))
}

fn normalize_openai_usage(value: Option<&Value>) -> Result<Value> {
    let usage = value
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("provider response usage is malformed"))?;
    let input = nonnegative_integer(usage.get("input_tokens"))?;
    let output = nonnegative_integer(usage.get("output_tokens"))?;
    let total = match usage.get("total_tokens") {
        Some(value) => nonnegative_integer(Some(value))?,
        None => input
            .checked_add(output)
            .ok_or_else(|| anyhow!("provider token counts overflow"))?,
    };
    if input.checked_add(output) != Some(total) {
        bail!("provider token counts are inconsistent");
    }
    Ok(json!({ "input_tokens": input, "output_tokens": output, "total_tokens": total }))
}

fn nonnegative_integer(value: Option<&Value>) -> Result<u64> {
    value
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("provider token count is malformed"))
}

fn normalize_jev_answer(
    options: &Options,
    raw: &Value,
    out: &mut Map<String, Value>,
) -> Result<()> {
    let answers = required_object(raw, "answers")?;
    if options.kind == Kind::Ask {
        let questions = options.questions.as_deref().unwrap_or_default();
        let expected = questions
            .iter()
            .map(|question| question.name.clone())
            .collect::<BTreeSet<_>>();
        if answers.keys().cloned().collect::<BTreeSet<_>>() != expected {
            bail!("provider answers do not match the requested questions");
        }
        let mut normalized = Map::new();
        for question in questions {
            let answer = answers
                .get(&question.name)
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("provider answer is malformed"))?;
            let fields = jev_answer_fields(
                question.kind,
                answer,
                &question.labels,
                &question.levels,
                question.minimum,
            )?;
            normalized.insert(question.name.clone(), Value::Object(fields));
        }
        out.insert("answers".into(), Value::Object(normalized));
        return Ok(());
    }
    if answers.len() != 1 {
        bail!("provider returned an unexpected answer set");
    }
    let answer = answers
        .get("decision")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("provider answer is malformed"))?;
    let kind = match options.kind {
        Kind::Classify => QuestionKind::Choice,
        Kind::Judge => QuestionKind::Noul,
        Kind::Score => QuestionKind::Score,
        _ => bail!("unsupported Jev operation"),
    };
    let fields = jev_answer_fields(
        kind,
        answer,
        &options.labels,
        &options.levels,
        options.minimum,
    )?;
    out.extend(fields);
    Ok(())
}

/// Validate one Jev answer against the request that produced it and return its
/// normalized fields. `accepted` compares the integer confidence with `minimum`.
fn jev_answer_fields(
    kind: QuestionKind,
    answer: &Map<String, Value>,
    labels: &[String],
    levels: &[String],
    minimum: u8,
) -> Result<Map<String, Value>> {
    if answer.get("type").and_then(Value::as_str) != Some(kind.as_str()) {
        bail!("provider answer type does not match request");
    }
    let mut out = Map::new();
    match kind {
        QuestionKind::Choice => {
            let label = answer
                .get("choice")
                .and_then(Value::as_str)
                .filter(|label| labels.iter().any(|candidate| candidate == label))
                .ok_or_else(|| anyhow!("provider returned an unknown choice"))?;
            let confidence = finite_probability(answer.get("confidence"))?;
            let probabilities = answer
                .get("probabilities")
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("provider probabilities are malformed"))?;
            validate_probabilities(
                probabilities,
                &labels.iter().cloned().collect::<BTreeSet<_>>(),
            )?;
            let confidence_percent = confidence_percent(confidence);
            out.insert("label".into(), json!(label));
            out.insert("confidence_percent".into(), json!(confidence_percent));
            out.insert("accepted".into(), json!(confidence_percent >= minimum));
        }
        QuestionKind::Noul => {
            let p_yes = finite_probability(answer.get("noul"))?;
            let confidence = p_yes.max(1.0 - p_yes);
            let confidence_percent = confidence_percent(confidence);
            out.insert("decision".into(), json!(p_yes >= 0.5));
            out.insert("confidence_percent".into(), json!(confidence_percent));
            out.insert("accepted".into(), json!(confidence_percent >= minimum));
        }
        QuestionKind::Score => {
            let score = answer
                .get("score")
                .and_then(Value::as_f64)
                .filter(|score| {
                    score.is_finite() && *score >= 0.0 && *score <= (levels.len() - 1) as f64
                })
                .ok_or_else(|| anyhow!("provider score is outside the requested rubric"))?;
            let confidence = finite_probability(answer.get("confidence"))?;
            let expected_legend = levels
                .iter()
                .enumerate()
                .map(|(index, label)| (index.to_string(), Value::String(label.clone())))
                .collect::<Map<_, _>>();
            if answer.get("legend").and_then(Value::as_object) != Some(&expected_legend) {
                bail!("provider score legend differs from requested rubric");
            }
            let expected_keys = (0..levels.len()).map(|i| i.to_string()).collect();
            let probabilities = answer
                .get("probabilities")
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("provider probabilities are malformed"))?;
            validate_probabilities(probabilities, &expected_keys)?;
            let confidence_percent = confidence_percent(confidence);
            out.insert("score".into(), json!(score));
            out.insert("confidence_percent".into(), json!(confidence_percent));
            out.insert("accepted".into(), json!(confidence_percent >= minimum));
        }
    }
    Ok(out)
}

fn validate_probabilities(
    probabilities: &Map<String, Value>,
    expected: &BTreeSet<String>,
) -> Result<()> {
    if probabilities.keys().cloned().collect::<BTreeSet<_>>() != *expected {
        bail!("provider probabilities do not match the requested options");
    }
    let mut total = 0.0;
    for value in probabilities.values() {
        total += finite_probability(Some(value))?;
    }
    if !(0.98..=1.02).contains(&total) {
        bail!("provider probabilities do not sum to one");
    }
    Ok(())
}

fn finite_probability(value: Option<&Value>) -> Result<f64> {
    let probability = value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .ok_or_else(|| anyhow!("provider probability is malformed"))?;
    Ok(probability)
}

fn confidence_percent(confidence: f64) -> u8 {
    (confidence * 100.0).floor() as u8
}

fn normalize_openai_answer(
    options: &Options,
    _semantic: &Value,
    raw: &Value,
    out: &mut Map<String, Value>,
) -> Result<()> {
    if options.api == Api::ChatCompletions {
        return normalize_chat_answer(options, raw, out);
    }
    if raw.get("status").and_then(Value::as_str) != Some("completed")
        || raw.get("error").is_some_and(|error| !error.is_null())
        || raw
            .get("incomplete_details")
            .is_some_and(|value| !value.is_null())
    {
        return Err(failure(FailureClass::Incomplete));
    }
    let output = raw
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("provider response output is malformed"))?;
    let mut texts = Vec::new();
    for item in output {
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            continue;
        }
        if item.get("type").and_then(Value::as_str) != Some("message")
            || item.get("role").and_then(Value::as_str) != Some("assistant")
            || item.get("status").and_then(Value::as_str) != Some("completed")
        {
            bail!("provider returned a non-message output item");
        }
        let content = item
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("provider message content is malformed"))?;
        for part in content {
            match part.get("type").and_then(Value::as_str) {
                Some("output_text") => {
                    texts.push(
                        part.get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow!("provider text output is malformed"))?,
                    );
                }
                Some("refusal") => return Err(failure(FailureClass::Refusal)),
                _ => bail!("provider returned unsupported output content"),
            }
        }
    }
    if texts.len() != 1 || texts[0].trim().is_empty() {
        bail!("provider did not return exactly one nonempty text result");
    }
    normalize_text_answer(options, texts[0], out)
}

/// Chat Completions: one choice whose message content is the model text.
/// A refusal, a truncated answer or tool calls fail closed.
fn normalize_chat_answer(
    options: &Options,
    raw: &Value,
    out: &mut Map<String, Value>,
) -> Result<()> {
    let choices = raw
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("provider response choices are malformed"))?;
    let [choice] = choices.as_slice() else {
        bail!("provider did not return exactly one choice");
    };
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("provider message is malformed"))?;
    if message
        .get("refusal")
        .is_some_and(|refusal| !refusal.is_null())
    {
        return Err(failure(FailureClass::Refusal));
    }
    match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") => {}
        Some("length") => return Err(failure(FailureClass::Incomplete)),
        Some("content_filter") => return Err(failure(FailureClass::Refusal)),
        _ => bail!("provider finish reason is unsupported"),
    }
    if message.get("role").is_some_and(|role| role != "assistant") {
        bail!("provider message role is unsupported");
    }
    if message
        .get("tool_calls")
        .is_some_and(|calls| !calls.is_null())
    {
        bail!("provider returned tool calls, which are not supported");
    }
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("provider did not return a nonempty text result"))?;
    normalize_text_answer(options, text, out)
}

fn normalize_text_answer(
    options: &Options,
    text: &str,
    out: &mut Map<String, Value>,
) -> Result<()> {
    match options.kind {
        Kind::Generate => {
            out.insert("text".into(), json!(text));
        }
        Kind::Extract => {
            let value =
                parse_unique_json(text.as_bytes()).map_err(|_| failure(FailureClass::Schema))?;
            let schema = options
                .schema
                .as_ref()
                .ok_or_else(|| anyhow!("missing extraction schema"))?;
            validate_schema_value(&value, schema).map_err(|_| failure(FailureClass::Schema))?;
            out.insert("value".into(), value);
        }
        _ => bail!("unsupported OpenAI operation"),
    }
    Ok(())
}

fn normalize_chat_usage(value: Option<&Value>) -> Result<Value> {
    let usage = value
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("provider response usage is malformed"))?;
    let input = nonnegative_integer(usage.get("prompt_tokens"))?;
    let output = nonnegative_integer(usage.get("completion_tokens"))?;
    let total = match usage.get("total_tokens") {
        Some(value) => nonnegative_integer(Some(value))?,
        None => input
            .checked_add(output)
            .ok_or_else(|| anyhow!("provider token counts overflow"))?,
    };
    if input.checked_add(output) != Some(total) {
        bail!("provider token counts are inconsistent");
    }
    Ok(json!({ "input_tokens": input, "output_tokens": output, "total_tokens": total }))
}

fn validate_schema_definition(schema: &Value) -> Result<()> {
    if !schema.is_object() {
        bail!("schema must be a JSON object");
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") || schema.get("anyOf").is_some()
    {
        bail!("strict extraction schemas need an object root without anyOf");
    }
    validate_schema_keywords(schema)
}

fn validate_schema_keywords(schema: &Value) -> Result<()> {
    const SUPPORTED: &[&str] = &[
        "$schema",
        "$id",
        "$comment",
        "title",
        "description",
        "examples",
        "default",
        "deprecated",
        "readOnly",
        "writeOnly",
        "type",
        "enum",
        "const",
        "required",
        "properties",
        "additionalProperties",
        "items",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minProperties",
        "maxProperties",
        "uniqueItems",
        "anyOf",
    ];
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow!("schema node must be an object"))?;
    if object.keys().any(|key| !SUPPORTED.contains(&key.as_str())) {
        bail!("schema contains an unsupported keyword");
    }
    for key in ["$schema", "$id", "$comment", "title", "description"] {
        if object.get(key).is_some_and(|value| !value.is_string()) {
            bail!("schema text keyword has an invalid shape");
        }
    }
    for key in ["deprecated", "readOnly", "writeOnly", "uniqueItems"] {
        if object.get(key).is_some_and(|value| !value.is_boolean()) {
            bail!("schema boolean keyword has an invalid shape");
        }
    }
    for key in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if object
            .get(key)
            .is_some_and(|value| value.as_u64().is_none())
        {
            bail!("schema count keyword has an invalid shape");
        }
    }
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if object
            .get(key)
            .is_some_and(|value| value.as_f64().is_none())
        {
            bail!("schema numeric keyword has an invalid shape");
        }
    }
    if object.get("multipleOf").is_some_and(|value| {
        !value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number > 0.0)
    }) {
        bail!("schema multipleOf must be a positive finite number");
    }
    let types = match object.get("type") {
        Some(Value::String(kind)) => {
            if !is_supported_schema_type(kind) {
                bail!("schema type is unsupported");
            }
            Some(BTreeSet::from([kind.as_str()]))
        }
        Some(Value::Array(kinds)) => {
            if kinds.is_empty() {
                bail!("schema type union cannot be empty");
            }
            let mut types = BTreeSet::new();
            for kind in kinds {
                let kind = kind
                    .as_str()
                    .filter(|kind| is_supported_schema_type(kind))
                    .ok_or_else(|| anyhow!("schema type is unsupported"))?;
                if !types.insert(kind) {
                    bail!("schema type union entries must be distinct");
                }
            }
            Some(types)
        }
        Some(_) => bail!("schema type must be text or a text array"),
        None => None,
    };
    let has_object_keywords = ["properties", "required", "additionalProperties"]
        .iter()
        .any(|key| object.contains_key(*key));
    let includes_object = types.as_ref().is_some_and(|types| types.contains("object"));
    let includes_array = types.as_ref().is_some_and(|types| types.contains("array"));
    if has_object_keywords && types.is_some() && !includes_object {
        bail!("object keywords require type object");
    }
    if object.contains_key("items") && types.is_some() && !includes_array {
        bail!("items requires type array");
    }
    if let Some(enumeration) = object.get("enum") {
        if !enumeration
            .as_array()
            .is_some_and(|values| !values.is_empty())
        {
            bail!("schema enum must be a nonempty array");
        }
    }
    let object_schema = includes_object || has_object_keywords;
    if object_schema {
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("object schema must declare properties"))?;
        let required = object
            .get("required")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("object schema must declare required properties"))?;
        let mut required_names = BTreeSet::new();
        for name in required {
            let name = name
                .as_str()
                .ok_or_else(|| anyhow!("schema required entries must be text"))?;
            if !required_names.insert(name) {
                bail!("schema required entries must be unique");
            }
        }
        if required_names
            != properties
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
        {
            bail!("strict object schemas must require every declared property");
        }
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            bail!("strict object schemas must set additionalProperties to false");
        }
        for property in properties.values() {
            validate_schema_keywords(property)?;
        }
    }
    if let Some(items) = object.get("items") {
        if !items.is_object() {
            bail!("schema items must be an object schema");
        }
        validate_schema_keywords(items)?;
    }
    if object
        .get("additionalProperties")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        bail!("additionalProperties must be false");
    }
    if let Some(value) = object.get("anyOf") {
        let alternatives = value
            .as_array()
            .ok_or_else(|| anyhow!("schema alternatives must be an array"))?;
        if alternatives.is_empty() {
            bail!("schema alternatives cannot be empty");
        }
        for alternative in alternatives {
            validate_schema_keywords(alternative)?;
        }
    }
    Ok(())
}

fn is_supported_schema_type(kind: &str) -> bool {
    matches!(
        kind,
        "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
    )
}

fn validate_schema_value(value: &Value, schema: &Value) -> Result<()> {
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow!("schema node must be an object"))?;
    if let Some(expected) = object.get("type") {
        let matches = match expected {
            Value::String(kind) => matches_json_type(value, kind),
            Value::Array(kinds) => kinds.iter().any(|kind| {
                kind.as_str()
                    .is_some_and(|kind| matches_json_type(value, kind))
            }),
            _ => false,
        };
        if !matches {
            bail!("extraction value has the wrong JSON type");
        }
    }
    if let Some(constant) = object.get("const") {
        if canonical_value(value)? != canonical_value(constant)? {
            bail!("extraction value does not match const");
        }
    }
    if let Some(enumeration) = object.get("enum") {
        let values = enumeration
            .as_array()
            .ok_or_else(|| anyhow!("schema enum must be an array"))?;
        let identity = canonical_value(value)?;
        let mut matched = false;
        for candidate in values {
            if canonical_value(candidate)? == identity {
                matched = true;
                break;
            }
        }
        if !matched {
            bail!("extraction value is outside enum");
        }
    }
    if let Some(alternatives) = object.get("allOf").and_then(Value::as_array) {
        for schema in alternatives {
            validate_schema_value(value, schema)?;
        }
    }
    if let Some(alternatives) = object.get("anyOf").and_then(Value::as_array) {
        if !alternatives
            .iter()
            .any(|schema| validate_schema_value(value, schema).is_ok())
        {
            bail!("extraction value matches no anyOf option");
        }
    }
    if let Some(alternatives) = object.get("oneOf").and_then(Value::as_array) {
        if alternatives
            .iter()
            .filter(|schema| validate_schema_value(value, schema).is_ok())
            .count()
            != 1
        {
            bail!("extraction value does not match exactly one oneOf option");
        }
    }
    if let Value::Object(values) = value {
        let properties = object.get("properties").and_then(Value::as_object);
        if let Some(required) = object.get("required") {
            let required = required
                .as_array()
                .ok_or_else(|| anyhow!("schema required must be an array"))?;
            for key in required {
                let key = key
                    .as_str()
                    .ok_or_else(|| anyhow!("schema required entries must be text"))?;
                if !values.contains_key(key) {
                    bail!("extraction is missing a required property");
                }
            }
        }
        if let Some(min) = object.get("minProperties").and_then(Value::as_u64) {
            if values.len() < min as usize {
                bail!("extraction has too few properties");
            }
        }
        if let Some(max) = object.get("maxProperties").and_then(Value::as_u64) {
            if values.len() > max as usize {
                bail!("extraction has too many properties");
            }
        }
        for (key, item) in values {
            if let Some(property_schema) = properties.and_then(|properties| properties.get(key)) {
                validate_schema_value(item, property_schema)?;
            } else if object.get("additionalProperties") == Some(&Value::Bool(false)) {
                bail!("extraction has an unexpected property");
            } else if let Some(additional) = object
                .get("additionalProperties")
                .filter(|value| value.is_object())
            {
                validate_schema_value(item, additional)?;
            }
        }
    }
    if let Value::Array(values) = value {
        if let Some(min) = object.get("minItems").and_then(Value::as_u64) {
            if values.len() < min as usize {
                bail!("extraction array is too short");
            }
        }
        if let Some(max) = object.get("maxItems").and_then(Value::as_u64) {
            if values.len() > max as usize {
                bail!("extraction array is too long");
            }
        }
        if object.get("uniqueItems") == Some(&Value::Bool(true)) {
            let mut unique = BTreeSet::new();
            for item in values {
                if !unique.insert(canonical_value(item)?) {
                    bail!("extraction array items are not unique");
                }
            }
        }
        if let Some(items) = object.get("items") {
            for item in values {
                validate_schema_value(item, items)?;
            }
        }
    }
    if let Value::String(text) = value {
        if let Some(min) = object.get("minLength").and_then(Value::as_u64) {
            if text.chars().count() < min as usize {
                bail!("extraction text is too short");
            }
        }
        if let Some(max) = object.get("maxLength").and_then(Value::as_u64) {
            if text.chars().count() > max as usize {
                bail!("extraction text is too long");
            }
        }
    }
    if value.is_number() {
        let number = decimal_value(value).ok_or_else(|| anyhow!("invalid extraction number"))?;
        if let Some(bound) = object.get("minimum") {
            let bound = decimal_value(bound).ok_or_else(|| anyhow!("invalid schema bound"))?;
            if compare_decimals(&number, &bound) == Ordering::Less {
                bail!("extraction number is out of range");
            }
        }
        if let Some(bound) = object.get("maximum") {
            let bound = decimal_value(bound).ok_or_else(|| anyhow!("invalid schema bound"))?;
            if compare_decimals(&number, &bound) == Ordering::Greater {
                bail!("extraction number is out of range");
            }
        }
        if let Some(bound) = object.get("exclusiveMinimum") {
            let bound = decimal_value(bound).ok_or_else(|| anyhow!("invalid schema bound"))?;
            if compare_decimals(&number, &bound) != Ordering::Greater {
                bail!("extraction number is out of range");
            }
        }
        if let Some(bound) = object.get("exclusiveMaximum") {
            let bound = decimal_value(bound).ok_or_else(|| anyhow!("invalid schema bound"))?;
            if compare_decimals(&number, &bound) != Ordering::Less {
                bail!("extraction number is out of range");
            }
        }
        if let Some(multiple) = object.get("multipleOf") {
            if !is_exact_decimal_multiple(value, multiple) {
                bail!("extraction number violates multipleOf");
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct DecimalValue {
    coefficient: u128,
    scale: i32,
    negative: bool,
}

fn compare_decimals(left: &DecimalValue, right: &DecimalValue) -> Ordering {
    match (left.coefficient == 0, right.coefficient == 0) {
        (true, true) => return Ordering::Equal,
        (true, false) => {
            return if right.negative {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, true) => {
            return if left.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, false) => {}
    }
    match (left.negative, right.negative) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => compare_decimal_magnitudes(left, right),
        (true, true) => compare_decimal_magnitudes(left, right).reverse(),
    }
}

fn compare_decimal_magnitudes(left: &DecimalValue, right: &DecimalValue) -> Ordering {
    let left_digits = left.coefficient.to_string();
    let right_digits = right.coefficient.to_string();
    let left_order = left_digits.len() as i64 - i64::from(left.scale);
    let right_order = right_digits.len() as i64 - i64::from(right.scale);
    match left_order.cmp(&right_order) {
        Ordering::Equal => {
            let width = left_digits.len().max(right_digits.len());
            for index in 0..width {
                let left_digit = left_digits.as_bytes().get(index).copied().unwrap_or(b'0');
                let right_digit = right_digits.as_bytes().get(index).copied().unwrap_or(b'0');
                match left_digit.cmp(&right_digit) {
                    Ordering::Equal => {}
                    ordering => return ordering,
                }
            }
            Ordering::Equal
        }
        ordering => ordering,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum CanonicalValue {
    Null,
    Boolean(bool),
    Number(DecimalValue),
    String(String),
    Array(Vec<CanonicalValue>),
    Object(BTreeMap<String, CanonicalValue>),
}

fn canonical_value(value: &Value) -> Result<CanonicalValue> {
    Ok(match value {
        Value::Null => CanonicalValue::Null,
        Value::Bool(value) => CanonicalValue::Boolean(*value),
        Value::Number(_) => CanonicalValue::Number(
            decimal_value(value).ok_or_else(|| anyhow!("number cannot be canonicalized"))?,
        ),
        Value::String(value) => CanonicalValue::String(value.clone()),
        Value::Array(values) => CanonicalValue::Array(
            values
                .iter()
                .map(canonical_value)
                .collect::<Result<Vec<_>>>()?,
        ),
        Value::Object(values) => CanonicalValue::Object(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), canonical_value(value)?)))
                .collect::<Result<BTreeMap<_, _>>>()?,
        ),
    })
}

/// Read serde_json's canonical number spelling as coefficient * 10^-scale.
/// serde_json emits at most 20 integer digits or a shortest round-trip float,
/// so the significant coefficient fits in u128 for the default Number type.
fn decimal_value(value: &Value) -> Option<DecimalValue> {
    let spelling = value.as_number()?.to_string();
    let exponent_at = spelling.find(['e', 'E']);
    let (mantissa, exponent) = match exponent_at {
        Some(index) => (
            &spelling[..index],
            spelling[index + 1..].parse::<i32>().ok()?,
        ),
        None => (spelling.as_str(), 0),
    };
    let negative = mantissa.starts_with('-');
    let unsigned_mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let (whole, fraction) = unsigned_mantissa
        .split_once('.')
        .unwrap_or((unsigned_mantissa, ""));
    let mut digits = String::with_capacity(whole.len() + fraction.len());
    digits.push_str(whole);
    digits.push_str(fraction);
    if digits.is_empty() || !digits.bytes().all(|digit| digit.is_ascii_digit()) {
        return None;
    }
    let mut coefficient = digits.parse::<u128>().ok()?;
    let mut scale = i32::try_from(fraction.len()).ok()?.checked_sub(exponent)?;
    if coefficient == 0 {
        return Some(DecimalValue {
            coefficient: 0,
            scale: 0,
            negative: false,
        });
    }
    while coefficient % 10 == 0 {
        coefficient /= 10;
        scale = scale.checked_sub(1)?;
    }
    Some(DecimalValue {
        coefficient,
        scale,
        negative,
    })
}

/// Check x / divisor using exact decimal divisibility, without floating-point
/// tolerances or constructing powers of ten that could overflow.
fn is_exact_decimal_multiple(value: &Value, divisor: &Value) -> bool {
    let Some(value) = decimal_value(value) else {
        return false;
    };
    let Some(divisor) = decimal_value(divisor) else {
        return false;
    };
    if divisor.coefficient == 0 {
        return false;
    }
    if value.coefficient == 0 {
        return true;
    }

    let common = greatest_common_divisor(value.coefficient, divisor.coefficient);
    let numerator = value.coefficient / common;
    let mut denominator = divisor.coefficient / common;
    let decimal_shift = i64::from(divisor.scale) - i64::from(value.scale);
    if decimal_shift >= 0 {
        let mut twos = 0_i64;
        while denominator.is_multiple_of(2) {
            denominator /= 2;
            twos += 1;
        }
        let mut fives = 0_i64;
        while denominator.is_multiple_of(5) {
            denominator /= 5;
            fives += 1;
        }
        denominator == 1 && twos <= decimal_shift && fives <= decimal_shift
    } else {
        if !numerator.is_multiple_of(denominator) {
            return false;
        }
        let mut quotient = numerator / denominator;
        let mut trailing_zeroes = 0_i64;
        while quotient > 0 && quotient.is_multiple_of(10) {
            quotient /= 10;
            trailing_zeroes += 1;
        }
        trailing_zeroes >= -decimal_shift
    }
}

fn greatest_common_divisor(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn matches_json_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value
                    .as_f64()
                    .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
        }
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn encode_bounded(value: &Value, limit: usize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).context("encoding JSON")?;
    if bytes.len() > limit {
        bail!("encoded JSON exceeds byte limit");
    }
    Ok(bytes)
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn offline_mode_rejects_live_calls_but_allows_fixture_calls() {
        let error = enforce_offline_mode(true, false).unwrap_err();
        assert_eq!(diagnostic(&error), "offline");
        enforce_offline_mode(true, true).unwrap();
        enforce_offline_mode(false, false).unwrap();
    }

    #[test]
    fn endpoint_policy_rejects_credentials_queries_fragments_and_remote_http() {
        for endpoint in [
            "http://example.com/v1",
            "http://127.0.0.1/v1?token=x",
            "https://user:pass@example.com/v1",
            "http://@localhost/v1",
            "https://example.com/v1#fragment",
            "http://localhost.evil/v1",
        ] {
            assert!(validate_endpoint(endpoint).is_err(), "{endpoint}");
        }
        for endpoint in [
            "https://api.example.com/v1",
            "http://127.0.0.1:8000/v1",
            "http://[::1]:8000/v1",
            "http://localhost:8000/v1",
        ] {
            assert!(validate_endpoint(endpoint).is_ok(), "{endpoint}");
        }
    }

    #[test]
    fn jev_semantic_request_has_stable_defaults_and_sorted_label_values() {
        let options = parse_args(&[
            "classify".into(),
            "--question".into(),
            "Is this spam?".into(),
            "--labels".into(),
            "spam".into(),
            "other".into(),
        ])
        .unwrap();
        let semantic = semantic_request(&options, json!("message")).unwrap();
        assert_eq!(semantic["provider"], "jev");
        assert_eq!(semantic["kind"], "classify");
        assert_eq!(semantic["model"], JEV_DEFAULT_MODEL);
        assert_eq!(semantic["minimum"], 0);
        assert_eq!(semantic["labels"], json!(["spam", "other"]));
    }

    #[test]
    fn openai_response_request_has_store_off_and_a_bounded_default() {
        let options = parse_args(&[
            "generate".into(),
            "--provider".into(),
            "openai".into(),
            "--model".into(),
            "gpt-fixture-model".into(),
            "--instruction".into(),
            "Write one line".into(),
        ])
        .unwrap();
        let request = provider_request(&options, Some(&json!("source"))).unwrap();
        assert_eq!(request["store"], false);
        assert_eq!(request["max_output_tokens"], DEFAULT_OPENAI_MAX_TOKENS);
    }

    #[test]
    fn jev_choice_requires_a_requested_label_and_complete_probabilities() {
        let options = parse_args(&[
            "classify".into(),
            "--question".into(),
            "Choose".into(),
            "--labels".into(),
            "urgent".into(),
            "other".into(),
            "--minimum".into(),
            "80".into(),
        ])
        .unwrap();
        let raw = json!({
            "model":"jev-latest",
            "answers":{"decision":{"type":"choice","choice":"urgent","confidence":0.789,"probabilities":{"urgent":0.8,"other":0.2}}},
            "usage":{"input_tokens":11,"output_tokens":4}
        });
        let mut out = Map::new();
        normalize_jev_answer(&options, &raw, &mut out).unwrap();
        assert_eq!(out["label"], "urgent");
        assert_eq!(out["confidence_percent"], 78);
        assert_eq!(out["accepted"], false);
        let mut malformed = raw;
        malformed["answers"]["decision"]["choice"] = json!("unknown");
        assert!(normalize_jev_answer(&options, &malformed, &mut Map::new()).is_err());
    }

    #[test]
    fn extraction_schema_is_checked_locally_and_rejects_unsupported_keywords() {
        let schema = json!({
            "type":"object",
            "properties":{"name":{"type":"string"},"count":{"type":"integer","minimum":1}},
            "required":["name","count"],
            "additionalProperties":false
        });
        validate_schema_definition(&schema).unwrap();
        validate_schema_value(&json!({"name":"item","count":2}), &schema).unwrap();
        validate_schema_value(&json!({"name":"item","count":2.0}), &schema).unwrap();
        assert!(validate_schema_value(&json!({"name":"item","count":2.5}), &schema).is_err());
        assert!(validate_schema_value(&json!({"name":"item"}), &schema).is_err());
        assert!(validate_schema_value(&json!({"name":"item","count":0}), &schema).is_err());
        assert!(
            validate_schema_value(&json!({"name":"item","count":2,"extra":true}), &schema).is_err()
        );
        assert!(validate_schema_definition(&json!({"type":"string","pattern":"a+"})).is_err());
    }

    #[test]
    fn multiple_of_uses_exact_decimal_divisibility() {
        let whole = json!({"type":"number","multipleOf":1});
        validate_schema_value(&parse_unique_json(b"1.0").unwrap(), &whole).unwrap();
        assert!(
            validate_schema_value(&parse_unique_json(b"1.0000000005").unwrap(), &whole).is_err()
        );

        let tenth = json!({"type":"number","multipleOf":0.1});
        validate_schema_value(&parse_unique_json(b"0.3").unwrap(), &tenth).unwrap();
        validate_schema_value(&parse_unique_json(b"-0.3").unwrap(), &tenth).unwrap();

        let fifth = json!({"type":"number","multipleOf":0.2});
        assert!(validate_schema_value(&parse_unique_json(b"0.3").unwrap(), &fifth).is_err());

        let tiny = json!({"type":"number","multipleOf":1e-21});
        validate_schema_value(&parse_unique_json(b"1e-20").unwrap(), &tiny).unwrap();
        assert!(validate_schema_value(
            &parse_unique_json(b"1e-20").unwrap(),
            &json!({
                "type":"number","multipleOf":1e-19
            })
        )
        .is_err());
    }

    #[test]
    fn numeric_bounds_use_exact_decimal_comparison() {
        let maximum = parse_unique_json(b"9223372036854775806").unwrap();
        let max_schema = json!({"type":"integer","maximum":maximum});
        validate_schema_value(
            &parse_unique_json(b"9223372036854775806").unwrap(),
            &max_schema,
        )
        .unwrap();
        assert!(validate_schema_value(
            &parse_unique_json(b"9223372036854775807").unwrap(),
            &max_schema,
        )
        .is_err());

        let minimum = parse_unique_json(b"-9223372036854775807").unwrap();
        let min_schema = json!({"type":"integer","minimum":minimum});
        validate_schema_value(
            &parse_unique_json(b"-9223372036854775807").unwrap(),
            &min_schema,
        )
        .unwrap();
        assert!(validate_schema_value(
            &parse_unique_json(b"-9223372036854775808").unwrap(),
            &min_schema,
        )
        .is_err());

        let fractional = json!({
            "type":"number",
            "minimum":parse_unique_json(b"1e-3").unwrap(),
            "maximum":parse_unique_json(b"0.001").unwrap()
        });
        validate_schema_value(&parse_unique_json(b"0.001").unwrap(), &fractional).unwrap();
        assert!(
            validate_schema_value(&parse_unique_json(b"0.0009").unwrap(), &fractional).is_err()
        );
        assert!(
            validate_schema_value(&parse_unique_json(b"0.0011").unwrap(), &fractional).is_err()
        );

        let negative = json!({
            "type":"number",
            "minimum":parse_unique_json(b"-1.25").unwrap(),
            "maximum":parse_unique_json(b"-1.2").unwrap()
        });
        validate_schema_value(&parse_unique_json(b"-1.25").unwrap(), &negative).unwrap();
        assert!(validate_schema_value(&parse_unique_json(b"-1.26").unwrap(), &negative).is_err());
        assert!(validate_schema_value(&parse_unique_json(b"-1.19").unwrap(), &negative).is_err());

        let exclusive = json!({
            "type":"number",
            "exclusiveMinimum":parse_unique_json(b"1e-1").unwrap(),
            "exclusiveMaximum":parse_unique_json(b"0.2").unwrap()
        });
        validate_schema_value(&parse_unique_json(b"0.15").unwrap(), &exclusive).unwrap();
        assert!(validate_schema_value(&parse_unique_json(b"0.1").unwrap(), &exclusive).is_err());
        assert!(validate_schema_value(&parse_unique_json(b"0.2").unwrap(), &exclusive).is_err());
    }

    #[test]
    fn nullable_type_unions_validate_values_and_container_keywords() {
        let schema = json!({
            "type":"object",
            "properties":{
                "count":{"type":["integer","null"]},
                "amount":{"type":["number","null"]},
                "note":{"type":["string","null"],"minLength":4},
                "items":{"type":["array","null"],"items":{"type":"integer"}},
                "nested":{
                    "type":["object","null"],
                    "properties":{"name":{"type":"string"}},
                    "required":["name"],
                    "additionalProperties":false
                }
            },
            "required":["count","amount","note","items","nested"],
            "additionalProperties":false
        });
        validate_schema_definition(&schema).unwrap();
        validate_schema_value(
            &parse_unique_json(
                br#"{"count":null,"amount":null,"note":null,"items":null,"nested":null}"#,
            )
            .unwrap(),
            &schema,
        )
        .unwrap();
        validate_schema_value(
            &parse_unique_json(
                br#"{"count":2.0,"amount":1.25,"note":"valid","items":[1,2],"nested":{"name":"x"}}"#,
            )
            .unwrap(),
            &schema,
        )
        .unwrap();
        assert!(validate_schema_value(
            &parse_unique_json(
                br#"{"count":2,"amount":1.25,"note":"valid","items":["bad"],"nested":{"name":"x"}}"#,
            )
            .unwrap(),
            &schema,
        )
        .is_err());
        assert!(validate_schema_value(
            &parse_unique_json(
                br#"{"count":2,"amount":1.25,"note":"valid","items":[],"nested":{"other":"x"}}"#,
            )
            .unwrap(),
            &schema,
        )
        .is_err());
        assert!(validate_schema_definition(&json!({
            "type":"object",
            "properties":{"value":{"type":["integer","integer"]}},
            "required":["value"],
            "additionalProperties":false
        }))
        .is_err());
        assert!(validate_schema_definition(&json!({"type":["object","null"]})).is_err());
    }

    #[test]
    fn canonical_numeric_identity_handles_const_enum_and_unique_items() {
        assert_eq!(
            canonical_value(&parse_unique_json(br#"{"b":2.0,"a":1}"#).unwrap()).unwrap(),
            canonical_value(&parse_unique_json(br#"{"a":1.0,"b":2}"#).unwrap()).unwrap()
        );
        let schema = json!({
            "type":"object",
            "properties":{
                "constant":{"const":2.0},
                "choice":{"enum":[-0.0,2.0]},
                "values":{"type":"array","items":{"type":"number"},"uniqueItems":true}
            },
            "required":["constant","choice","values"],
            "additionalProperties":false
        });
        validate_schema_definition(&schema).unwrap();
        validate_schema_value(
            &parse_unique_json(br#"{"constant":2,"choice":0,"values":[2,3]}"#).unwrap(),
            &schema,
        )
        .unwrap();
        assert!(validate_schema_value(
            &parse_unique_json(br#"{"constant":2,"choice":-0.0,"values":[2,2.0]}"#).unwrap(),
            &schema,
        )
        .is_err());
        validate_schema_value(
            &parse_unique_json(
                br#"{"constant":2.0,"choice":2,"values":[-2,2,9007199254740992,9007199254740993]}"#,
            )
            .unwrap(),
            &schema,
        )
        .unwrap();
        assert!(validate_schema_value(
            &parse_unique_json(br#"{"constant":2,"choice":0,"values":[0,-0.0]}"#).unwrap(),
            &schema,
        )
        .is_err());
    }

    #[test]
    fn fixture_requires_a_full_semantic_match_and_returns_raw_response() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fixture.json");
        let expected = json!({"provider":"jev","kind":"judge","model":"jev-latest","state":"text","question":"Is it true?","minimum":0});
        let raw = json!({"model":"jev-latest","answers":{"decision":{"type":"noul","noul":0.9}},"usage":{"input_tokens":1,"output_tokens":2}});
        std::fs::write(
            &path,
            json!({"expected_request":expected,"response":raw}).to_string(),
        )
        .unwrap();
        assert_eq!(
            read_fixture(path.to_str().unwrap(), &expected, 1024).unwrap(),
            raw
        );
        let mismatched = json!({"provider":"jev","kind":"judge","model":"jev-latest","state":"other","question":"Is it true?","minimum":0});
        assert!(read_fixture(path.to_str().unwrap(), &mismatched, 1024).is_err());
    }

    #[test]
    fn unique_json_rejects_duplicate_keys_at_every_depth() {
        assert!(parse_unique_json(br#"{"status":"incomplete","status":"completed"}"#).is_err());
        assert!(parse_unique_json(br#"{"outer":[{"n":0,"n":1}]}"#).is_err());
        assert_eq!(
            parse_unique_json(br#"{"outer":[{"n":1}]}"#).unwrap(),
            json!({"outer":[{"n":1}]})
        );
    }

    #[test]
    fn duplicate_key_state_is_rejected_instead_of_forwarded_as_text() {
        let args = [
            "generate".to_owned(),
            "--provider".to_owned(),
            "openai".to_owned(),
            "--model".to_owned(),
            "gpt-fixture-model".to_owned(),
            "--instruction".to_owned(),
            "summarize".to_owned(),
        ];
        let mut input = std::io::Cursor::new(br#"{"x":1,"x":2}"#.as_slice());
        let mut output = Vec::new();
        let error = run(&args, &mut input, &mut output).unwrap_err();
        assert_eq!(diagnostic(&error), "input");
        assert!(output.is_empty());
    }
}

#[cfg(test)]
mod credential_policy_tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn process(values: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    fn env_file(directory: &tempfile::TempDir, text: &str) -> String {
        let path = directory.path().join("credentials.env");
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn default_openai_key_reaches_only_the_default_host() {
        let keys = process(&[("OPENAI_API_KEY", "default-test-key")]);
        for endpoint in [
            "https://api.openai.com/v1/responses",
            "https://api.openai.com/v1/chat/completions",
            "https://API.openai.com/v1",
            "https://api.openai.com:443/v1",
        ] {
            let key =
                select_credential(Provider::OpenAi, &url(endpoint), None, None, &keys).unwrap();
            assert_eq!(key.as_deref(), Some("default-test-key"), "{endpoint}");
        }
        for endpoint in [
            "https://api.openai.com.evil.test/v1",
            "https://evil.test/v1",
            "https://openai.com/v1",
            "https://api.openai.com:8443/v1",
            "http://api.openai.com/v1",
            "http://127.0.0.1:1/v1",
            "http://localhost:1/v1",
            "https://[::1]:1/v1",
        ] {
            let key =
                select_credential(Provider::OpenAi, &url(endpoint), None, None, &keys).unwrap();
            assert_eq!(key, None, "default key leaked to {endpoint}");
        }
    }

    #[test]
    fn default_jev_key_reaches_only_the_typesafe_host() {
        let keys = process(&[("JEV_API_KEY", "jev-test-key")]);
        let key = select_credential(
            Provider::Jev,
            &url("https://api.typesafe.ai/v1/systemone"),
            None,
            None,
            &keys,
        )
        .unwrap();
        assert_eq!(key.as_deref(), Some("jev-test-key"));
        for endpoint in [
            "https://api.openai.com/v1",
            "https://api.typesafe.ai.evil.test/v1",
            "http://127.0.0.1:1/v1",
        ] {
            assert_eq!(
                select_credential(Provider::Jev, &url(endpoint), None, None, &keys).unwrap(),
                None,
                "{endpoint}"
            );
        }
    }

    #[test]
    fn a_provider_key_never_satisfies_the_other_provider() {
        let openai_only = process(&[("OPENAI_API_KEY", "openai-test-key")]);
        let error = select_credential(
            Provider::Jev,
            &url("https://api.typesafe.ai/v1/systemone"),
            None,
            None,
            &openai_only,
        )
        .unwrap_err();
        assert_eq!(diagnostic(&error), "credentials_missing");
        let jev_only = process(&[("JEV_API_KEY", "jev-test-key")]);
        let error = select_credential(
            Provider::OpenAi,
            &url("https://api.openai.com/v1/responses"),
            None,
            None,
            &jev_only,
        )
        .unwrap_err();
        assert_eq!(diagnostic(&error), "credentials_missing");
    }

    #[test]
    fn default_key_in_an_env_file_reaches_only_the_default_host() {
        let directory = tempfile::tempdir().unwrap();
        let file = env_file(
            &directory,
            "OPENAI_API_KEY=file-default-key\nopenai=alias-key\n",
        );
        let key = select_credential(
            Provider::OpenAi,
            &url("https://api.openai.com/v1/responses"),
            None,
            Some(&file),
            &none,
        )
        .unwrap();
        assert_eq!(key.as_deref(), Some("file-default-key"));
        for endpoint in [
            "http://127.0.0.1:9/v1",
            "https://models.example.test/v1",
            "https://api.openai.com.evil.test/v1",
        ] {
            let key = select_credential(Provider::OpenAi, &url(endpoint), None, Some(&file), &none)
                .unwrap();
            assert_eq!(key, None, "file default key leaked to {endpoint}");
        }
    }

    #[test]
    fn custom_endpoint_never_reads_default_keys_or_the_env_file() {
        // A nonexistent file proves the custom branch never opens the file.
        let missing = "does-not-exist-credentials.env";
        let keys = process(&[
            ("OPENAI_API_KEY", "default-test-key"),
            ("openai", "alias-test-key"),
        ]);
        for endpoint in ["http://127.0.0.1:9/v1", "https://models.example.test/v1"] {
            let key =
                select_credential(Provider::OpenAi, &url(endpoint), None, Some(missing), &keys)
                    .unwrap();
            assert_eq!(key, None, "{endpoint}");
        }
    }

    #[test]
    fn api_key_env_names_its_variable_for_any_endpoint() {
        let keys = process(&[
            ("LOCAL_MODEL_KEY", "local-test-key"),
            ("OPENAI_API_KEY", "default-test-key"),
        ]);
        for endpoint in [
            "http://127.0.0.1:9/v1",
            "https://models.example.test/v1",
            "https://api.openai.com/v1",
        ] {
            let key = select_credential(
                Provider::OpenAi,
                &url(endpoint),
                Some("LOCAL_MODEL_KEY"),
                None,
                &keys,
            )
            .unwrap();
            assert_eq!(key.as_deref(), Some("local-test-key"), "{endpoint}");
        }
    }

    #[test]
    fn api_key_env_reads_the_env_file_and_prefers_the_process_value() {
        let directory = tempfile::tempdir().unwrap();
        let file = env_file(
            &directory,
            "LOCAL_MODEL_KEY=file-local-key\nUNRELATED=ignored\n",
        );
        let from_file = select_credential(
            Provider::OpenAi,
            &url("http://127.0.0.1:9/v1"),
            Some("LOCAL_MODEL_KEY"),
            Some(&file),
            &none,
        )
        .unwrap();
        assert_eq!(from_file.as_deref(), Some("file-local-key"));
        let process_first = process(&[("LOCAL_MODEL_KEY", "process-local-key")]);
        let both = select_credential(
            Provider::OpenAi,
            &url("http://127.0.0.1:9/v1"),
            Some("LOCAL_MODEL_KEY"),
            Some(&file),
            &process_first,
        )
        .unwrap();
        assert_eq!(both.as_deref(), Some("process-local-key"));
    }

    #[test]
    fn missing_api_key_env_value_fails_before_any_connection() {
        let error = select_credential(
            Provider::OpenAi,
            &url("http://127.0.0.1:9/v1"),
            Some("LOCAL_MODEL_KEY"),
            None,
            &none,
        )
        .unwrap_err();
        assert_eq!(diagnostic(&error), "credentials_missing");
    }

    #[test]
    fn default_host_without_any_key_is_credentials_missing() {
        let error = select_credential(
            Provider::OpenAi,
            &url("https://api.openai.com/v1/responses"),
            None,
            None,
            &none,
        )
        .unwrap_err();
        assert_eq!(diagnostic(&error), "credentials_missing");
    }

    #[test]
    fn control_characters_in_a_credential_are_configuration_errors() {
        let keys = process(&[("OPENAI_API_KEY", "bad\u{7}key")]);
        let error = select_credential(
            Provider::OpenAi,
            &url("https://api.openai.com/v1/responses"),
            None,
            None,
            &keys,
        )
        .unwrap_err();
        assert_eq!(diagnostic(&error), "configuration");
    }

    #[test]
    fn api_key_env_rejects_default_names_and_invalid_names() {
        let base = |name: &str| {
            vec![
                "generate".to_owned(),
                "--provider".to_owned(),
                "openai".to_owned(),
                "--model".to_owned(),
                "gpt-6-luna".to_owned(),
                "--instruction".to_owned(),
                "x".to_owned(),
                "--api-key-env".to_owned(),
                name.to_owned(),
            ]
        };
        for name in [
            "OPENAI_API_KEY",
            "openai",
            "OpenAI",
            "openai_api_key",
            "JEV_API_KEY",
            "TYPESAFE_API_KEY",
            "jev",
            "Typesafe_Api_Key",
            "",
            "1BAD",
            "BAD-NAME",
            "HAS SPACE",
        ] {
            assert!(
                parse_args(&base(name)).is_err(),
                "accepted api-key-env {name:?}"
            );
        }
        for name in ["LOCAL_MODEL_KEY", "_LOCAL", "lowercase_key"] {
            let options = parse_args(&base(name))
                .unwrap_or_else(|error| panic!("rejected {name:?}: {error}"));
            assert_eq!(options.api_key_env.as_deref(), Some(name));
        }
    }
}
