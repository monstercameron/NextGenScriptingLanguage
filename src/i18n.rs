//! Message catalog lookup with plural rules (#361).
//!
//! A catalog is one JSON document:
//!
//! ```json
//! {
//!   "language": "ru",
//!   "plural_forms": "nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);",
//!   "messages": {
//!     "": { "Saved": "Сохранено", "{count} files": ["{count} файл", "{count} файла", "{count} файлов"] },
//!     "menu": { "Saved": "Сохр." }
//!   }
//! }
//! ```
//!
//! `messages` maps a context (the empty string for none) to message ids. A value is either one
//! string, used for every count, or an array with one string per plural form. Empty strings count
//! as untranslated. Placeholders are written `{name}`; `{{` and `}}` produce literal braces.
//!
//! Plural selection never evaluates the `plural_forms` expression. The header must match one of
//! the rules listed in [`PluralRule`]; anything else is rejected by name.

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;

/// Largest catalog source accepted, in bytes.
pub const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;
/// Largest number of messages (across all contexts) accepted in one catalog.
pub const MAX_CATALOG_ENTRIES: usize = 200_000;

/// The recognised plural rules, keyed by their normalised `Plural-Forms` text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralRule {
    /// `nplurals=1; plural=0;` (for example ja, zh, ko)
    OneForm,
    /// `nplurals=2; plural=(n != 1);` (for example en, de, da)
    NotOne,
    /// `nplurals=2; plural=(n > 1);` (for example fr)
    GreaterThanOne,
    /// `nplurals=3`, the Russian rule shared by Ukrainian, Serbian and Croatian.
    Slavic,
    /// `nplurals=3`, the Polish rule from the gettext manual.
    Polish,
}

const SLAVIC_EXPR: &str = "n%10==1&&n%100!=11?0:n%10>=2&&n%10<=4&&(n%100<10||n%100>=20)?1:2";
const POLISH_EXPR: &str = "n==1?0:n%10>=2&&n%10<=4&&(n%100<10||n%100>=20)?1:2";

impl PluralRule {
    /// Number of plural forms the rule selects between.
    pub fn form_count(self) -> usize {
        match self {
            Self::OneForm => 1,
            Self::NotOne | Self::GreaterThanOne => 2,
            Self::Slavic | Self::Polish => 3,
        }
    }

    /// Index of the plural form used for `n`.
    pub fn form_index(self, n: u64) -> usize {
        match self {
            Self::OneForm => 0,
            Self::NotOne => usize::from(n != 1),
            Self::GreaterThanOne => usize::from(n > 1),
            Self::Slavic => {
                if n % 10 == 1 && n % 100 != 11 {
                    0
                } else if few_forms(n) {
                    1
                } else {
                    2
                }
            }
            Self::Polish => {
                if n == 1 {
                    0
                } else if few_forms(n) {
                    1
                } else {
                    2
                }
            }
        }
    }
}

fn few_forms(n: u64) -> bool {
    (2..=4).contains(&(n % 10)) && (n % 100 < 10 || n % 100 >= 20)
}

/// Recognises a `Plural-Forms` value such as `nplurals=2; plural=(n != 1);`.
///
/// Whitespace is ignored and the trailing semicolon and outer parentheses are optional.
pub fn plural_rule(header: &str) -> Result<PluralRule, I18nError> {
    let unsupported = || I18nError::UnsupportedPluralRule(header.trim().to_string());
    let compact: String = header.chars().filter(|c| !c.is_whitespace()).collect();
    let body = compact.strip_suffix(';').unwrap_or(compact.as_str());
    let (count, expr) = body.split_once(';').ok_or_else(unsupported)?;
    let count = count.strip_prefix("nplurals=").ok_or_else(unsupported)?;
    let expr = expr.strip_prefix("plural=").ok_or_else(unsupported)?;
    match (count, strip_outer_parens(expr)) {
        ("1", "0") => Ok(PluralRule::OneForm),
        ("2", "n!=1") => Ok(PluralRule::NotOne),
        ("2", "n>1") => Ok(PluralRule::GreaterThanOne),
        ("3", SLAVIC_EXPR) => Ok(PluralRule::Slavic),
        ("3", POLISH_EXPR) => Ok(PluralRule::Polish),
        _ => Err(unsupported()),
    }
}

fn strip_outer_parens(expr: &str) -> &str {
    if !(expr.starts_with('(') && expr.ends_with(')')) {
        return expr;
    }
    let mut depth = 0usize;
    for (index, byte) in expr.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && index != expr.len() - 1 {
                    return expr;
                }
            }
            _ => {}
        }
    }
    &expr[1..expr.len() - 1]
}

/// A parsed catalog for one language.
#[derive(Debug)]
pub struct Catalog {
    language: Option<String>,
    plural: PluralRule,
    messages: HashMap<String, HashMap<String, Entry>>,
}

#[derive(Debug)]
enum Entry {
    Text(String),
    Forms(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    language: Option<String>,
    plural_forms: Option<String>,
    messages: HashMap<String, HashMap<String, RawEntry>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawEntry {
    Text(String),
    Forms(Vec<String>),
}

impl Catalog {
    /// The `language` field, if the catalog declares one.
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    fn translation(
        &self,
        context: &str,
        msgid: &str,
        form: Option<usize>,
    ) -> Result<Option<&str>, I18nError> {
        let Some(entry) = self
            .messages
            .get(context)
            .and_then(|table| table.get(msgid))
        else {
            return Ok(None);
        };
        match (entry, form) {
            (Entry::Text(text), _) => Ok(non_empty(text)),
            (Entry::Forms(forms), Some(index)) => {
                Ok(forms.get(index).map(String::as_str).and_then(non_empty))
            }
            (Entry::Forms(_), None) => Err(I18nError::PluralFormsNeedCount {
                context: context.to_string(),
                msgid: msgid.to_string(),
            }),
        }
    }
}

fn non_empty(text: &str) -> Option<&str> {
    (!text.is_empty()).then_some(text)
}

/// Parses a catalog, rejecting input over [`MAX_CATALOG_BYTES`] or [`MAX_CATALOG_ENTRIES`].
pub fn parse_catalog(source: &str) -> Result<Catalog, I18nError> {
    parse_catalog_limited(source, MAX_CATALOG_BYTES, MAX_CATALOG_ENTRIES)
}

fn parse_catalog_limited(
    source: &str,
    max_bytes: usize,
    max_entries: usize,
) -> Result<Catalog, I18nError> {
    if source.len() > max_bytes {
        return Err(I18nError::TooLarge {
            bytes: source.len(),
            limit: max_bytes,
        });
    }
    let raw: RawCatalog =
        serde_json::from_str(source).map_err(|error| I18nError::Syntax(error.to_string()))?;
    let plural = match raw.plural_forms {
        Some(header) => plural_rule(&header)?,
        None => PluralRule::NotOne,
    };
    let mut messages = HashMap::with_capacity(raw.messages.len());
    let mut count = 0usize;
    for (context, ids) in raw.messages {
        let mut table = HashMap::with_capacity(ids.len());
        for (msgid, entry) in ids {
            let describe = |reason: String| I18nError::Message {
                context: context.clone(),
                msgid: msgid.clone(),
                reason,
            };
            if msgid.is_empty() {
                return Err(describe("the message id is empty".to_string()));
            }
            count += 1;
            if count > max_entries {
                return Err(I18nError::TooManyEntries { limit: max_entries });
            }
            let entry = match entry {
                RawEntry::Text(text) => Entry::Text(text),
                RawEntry::Forms(forms) if forms.len() == plural.form_count() => Entry::Forms(forms),
                RawEntry::Forms(forms) => {
                    return Err(describe(format!(
                        "has {} plural forms but the plural rule needs {}",
                        forms.len(),
                        plural.form_count()
                    )));
                }
            };
            table.insert(msgid, entry);
        }
        messages.insert(context, table);
    }
    Ok(Catalog {
        language: raw.language,
        plural,
        messages,
    })
}

/// Normalises a locale tag such as `da_DK.UTF-8`, `de-de` or `sr_RS@latin` to `da_DK`, `de_DE`
/// or `sr_RS`. Drops the codeset and modifier, lowercases the language and uppercases the
/// region. Returns `None` for an empty tag, `C`, `POSIX` or a malformed tag.
pub fn normalise_locale(tag: &str) -> Option<String> {
    let base = tag.split(['.', '@']).next()?.replace('-', "_");
    let mut parts = base.split('_');
    let language = parts.next()?;
    let region = parts.next();
    if parts.next().is_some() || !(2..=3).contains(&language.len()) {
        return None;
    }
    if !language.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut normal = language.to_ascii_lowercase();
    if let Some(region) = region {
        if !(2..=3).contains(&region.len()) || !region.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        normal.push('_');
        normal.push_str(&region.to_ascii_uppercase());
    }
    Some(normal)
}

/// Picks the catalogs to consult, in order, for a requested locale.
///
/// `requested` is a LANGUAGE-style colon list. Each entry contributes its exact tag, then its
/// language without region, and `default` comes last. `available` holds the catalog tags, and
/// the result holds indices into it without repeats.
pub fn resolve_catalogs(requested: &str, available: &[&str], default: &str) -> Vec<usize> {
    let mut wanted: Vec<String> = Vec::new();
    for tag in requested.split(':') {
        let Some(normal) = normalise_locale(tag) else {
            continue;
        };
        if let Some((language, _)) = normal.split_once('_') {
            wanted.push(normal.clone());
            wanted.push(language.to_string());
        } else {
            wanted.push(normal);
        }
    }
    if let Some(normal) = normalise_locale(default) {
        wanted.push(normal);
    }
    let normalised: Vec<Option<String>> =
        available.iter().map(|tag| normalise_locale(tag)).collect();
    let mut chain: Vec<usize> = Vec::new();
    for tag in &wanted {
        let found = normalised
            .iter()
            .position(|candidate| candidate.as_deref() == Some(tag.as_str()));
        if let Some(index) = found {
            if !chain.contains(&index) {
                chain.push(index);
            }
        }
    }
    chain
}

/// Looks up a message through `chain` and fills its placeholders.
///
/// The first catalog with a non-empty translation wins. For a plural call, `plural` is the
/// English message id and count, and each catalog picks its own form for the count. When no
/// catalog translates the message, the English source is used: the id for a count of one and
/// the plural id otherwise.
pub fn message(
    chain: &[&Catalog],
    context: &str,
    msgid: &str,
    plural: Option<(&str, u64)>,
    values: &[(&str, &str)],
) -> Result<String, I18nError> {
    for catalog in chain {
        let form = plural.map(|(_, count)| catalog.plural.form_index(count));
        if let Some(text) = catalog.translation(context, msgid, form)? {
            return fill(text, values);
        }
    }
    let source = match plural {
        Some((msgid_plural, count)) if count != 1 => msgid_plural,
        _ => msgid,
    };
    fill(source, values)
}

/// Replaces each `{name}` with its value in a single pass. Values are copied verbatim and never
/// scanned for placeholders. `{{` and `}}` produce literal braces.
pub fn fill(template: &str, values: &[(&str, &str)]) -> Result<String, I18nError> {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut literal_start = 0;
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'{' | b'}' if bytes.get(i + 1) == Some(&byte) => {
                out.push_str(&template[literal_start..i]);
                out.push(char::from(byte));
                i += 2;
                literal_start = i;
            }
            b'{' => {
                let Some(close) = template[i + 1..].find('}').map(|offset| i + 1 + offset) else {
                    return Err(template_error(i, "a `{` is never closed"));
                };
                let name = &template[i + 1..close];
                if !is_placeholder_name(name) {
                    return Err(template_error(
                        i,
                        "placeholder names use letters, digits and underscores",
                    ));
                }
                let value = values
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| *value)
                    .ok_or_else(|| I18nError::MissingPlaceholder(name.to_string()))?;
                out.push_str(&template[literal_start..i]);
                out.push_str(value);
                i = close + 1;
                literal_start = i;
            }
            b'}' => return Err(template_error(i, "a `}` has no matching `{`")),
            _ => i += 1,
        }
    }
    out.push_str(&template[literal_start..]);
    Ok(out)
}

fn template_error(offset: usize, reason: &'static str) -> I18nError {
    I18nError::Template { offset, reason }
}

fn is_placeholder_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Failures while loading a catalog, selecting a plural rule or filling a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum I18nError {
    TooLarge {
        bytes: usize,
        limit: usize,
    },
    TooManyEntries {
        limit: usize,
    },
    Syntax(String),
    Message {
        context: String,
        msgid: String,
        reason: String,
    },
    UnsupportedPluralRule(String),
    PluralFormsNeedCount {
        context: String,
        msgid: String,
    },
    MissingPlaceholder(String),
    Template {
        offset: usize,
        reason: &'static str,
    },
}

impl fmt::Display for I18nError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, limit } => {
                write!(f, "catalog is {bytes} bytes, the limit is {limit}")
            }
            Self::TooManyEntries { limit } => {
                write!(f, "catalog has more than {limit} messages")
            }
            Self::Syntax(message) => write!(f, "invalid catalog: {message}"),
            Self::Message {
                context,
                msgid,
                reason,
            } => write!(f, "{}: {reason}", describe_message(context, msgid)),
            Self::UnsupportedPluralRule(rule) => write!(
                f,
                "unsupported plural rule `{rule}`: only the one-form, n != 1, n > 1, Slavic and Polish rules are recognised"
            ),
            Self::PluralFormsNeedCount { context, msgid } => write!(
                f,
                "{} has plural forms and must be looked up with a count",
                describe_message(context, msgid)
            ),
            Self::MissingPlaceholder(name) => write!(f, "no value for placeholder `{name}`"),
            Self::Template { offset, reason } => {
                write!(f, "malformed template at byte {offset}: {reason}")
            }
        }
    }
}

impl std::error::Error for I18nError {}

fn describe_message(context: &str, msgid: &str) -> String {
    if context.is_empty() {
        format!("message {msgid:?}")
    } else {
        format!("message {msgid:?} in context {context:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGLISH: &str = r#"{"language":"en","messages":{"":{"Saved":"Saved","{count} file":["{count} file","{count} files"]}}}"#;

    const DANISH: &str = r#"{
      "language": "da",
      "messages": {
        "": {"Saved": "Gemt", "Only danish": "Kun dansk", "Blank": "", "Hello {name}": "Hej {name}"},
        "menu": {"Saved": "Gem"}
      }
    }"#;

    const RUSSIAN: &str = r#"{
      "language": "ru",
      "plural_forms": "nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);",
      "messages": {"": {"{count} files": ["{count} файл", "{count} файла", "{count} файлов"]}}
    }"#;

    const POLISH: &str = r#"{"plural_forms": "nplurals=3; plural=(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);",
      "messages": {"": {"{count} files": ["{count} plik", "{count} pliki", "{count} plików"]}}}"#;

    fn form_for(rule: PluralRule, numbers: &[u64]) -> Vec<usize> {
        numbers.iter().map(|&n| rule.form_index(n)).collect()
    }
    const BOUNDARY: &[u64] = &[0, 1, 2, 4, 5, 11, 12, 21, 22, 25];

    #[test]
    fn recognises_the_published_rules_with_spacing_variants() {
        assert_eq!(
            plural_rule("nplurals=1; plural=0;"),
            Ok(PluralRule::OneForm)
        );
        assert_eq!(
            plural_rule("nplurals=2; plural=(n != 1);"),
            Ok(PluralRule::NotOne)
        );
        assert_eq!(
            plural_rule("nplurals=2; plural=n!=1"),
            Ok(PluralRule::NotOne)
        );
        assert_eq!(
            plural_rule("nplurals=2; plural=(n > 1);"),
            Ok(PluralRule::GreaterThanOne)
        );
        assert_eq!(
            plural_rule("nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);"),
            Ok(PluralRule::Slavic)
        );
        assert_eq!(
            plural_rule("nplurals=3; plural=(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);"),
            Ok(PluralRule::Polish)
        );
    }

    #[test]
    fn unrecognised_rule_is_an_error_that_names_it() {
        let error = plural_rule("nplurals=2; plural=(n == 0 || n == 1);").unwrap_err();
        assert!(matches!(error, I18nError::UnsupportedPluralRule(_)));
        assert!(error.to_string().contains("(n == 0 || n == 1)"), "{error}");
        assert!(plural_rule("nplurals=2; plural=(n != 1); charset=UTF-8").is_err());
        assert!(plural_rule("nplurals=3; plural=(n != 1);").is_err());
    }

    #[test]
    fn one_form_and_two_form_rules_select_expected_indices() {
        assert_eq!(form_for(PluralRule::OneForm, BOUNDARY), [0; 10]);
        assert_eq!(
            form_for(PluralRule::NotOne, &[0, 1, 2, 5, 21]),
            [1, 0, 1, 1, 1]
        );
        assert_eq!(
            form_for(PluralRule::GreaterThanOne, &[0, 1, 2, 5]),
            [0, 0, 1, 1]
        );
    }

    #[test]
    fn three_form_slavic_rule_matches_boundary_numbers() {
        assert_eq!(
            form_for(PluralRule::Slavic, BOUNDARY),
            [2, 0, 1, 1, 2, 2, 2, 0, 1, 2]
        );
    }

    #[test]
    fn three_form_polish_rule_matches_boundary_numbers() {
        assert_eq!(
            form_for(PluralRule::Polish, BOUNDARY),
            [2, 0, 1, 1, 2, 2, 2, 2, 1, 2]
        );
    }

    #[test]
    fn form_counts_match_the_rules() {
        assert_eq!(PluralRule::OneForm.form_count(), 1);
        assert_eq!(PluralRule::NotOne.form_count(), 2);
        assert_eq!(PluralRule::GreaterThanOne.form_count(), 2);
        assert_eq!(PluralRule::Slavic.form_count(), 3);
        assert_eq!(PluralRule::Polish.form_count(), 3);
    }

    #[test]
    fn russian_plural_array_is_selected_by_count() {
        let catalog = parse_catalog(RUSSIAN).unwrap();
        let chain = [&catalog];
        let render = |n: u64| {
            let count = n.to_string();
            message(
                &chain,
                "",
                "{count} files",
                Some(("{count} files", n)),
                &[("count", count.as_str())],
            )
            .unwrap()
        };
        assert_eq!(render(21), "21 файл");
        assert_eq!(render(22), "22 файла");
        assert_eq!(render(5), "5 файлов");
        assert_eq!(render(11), "11 файлов");
    }

    #[test]
    fn plural_array_length_must_match_the_rule() {
        let source = r#"{"plural_forms":"nplurals=3; plural=(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);","messages":{"":{"x":["a","b"]}}}"#;
        let error = parse_catalog(source).unwrap_err();
        assert!(error.to_string().contains("has 2 plural forms"), "{error}");
        assert!(error.to_string().contains("needs 3"), "{error}");
    }

    #[test]
    fn polish_plural_array_is_selected_by_count() {
        let catalog = parse_catalog(POLISH).unwrap();
        let chain = [&catalog];
        let render = |n: u64| {
            let count = n.to_string();
            message(
                &chain,
                "",
                "{count} files",
                Some(("{count} files", n)),
                &[("count", count.as_str())],
            )
            .unwrap()
        };
        assert_eq!(render(1), "1 plik");
        assert_eq!(render(22), "22 pliki");
        assert_eq!(render(21), "21 plików");
    }

    #[test]
    fn translated_message_wins_and_untranslated_falls_back_to_msgid() {
        let danish = parse_catalog(DANISH).unwrap();
        let chain = [&danish];
        assert_eq!(message(&chain, "", "Saved", None, &[]).unwrap(), "Gemt");
        assert_eq!(
            message(&chain, "", "Missing key", None, &[]).unwrap(),
            "Missing key"
        );
        assert_eq!(message(&chain, "", "Blank", None, &[]).unwrap(), "Blank");
    }

    #[test]
    fn context_separates_identical_message_ids() {
        let danish = parse_catalog(DANISH).unwrap();
        let chain = [&danish];
        assert_eq!(message(&chain, "menu", "Saved", None, &[]).unwrap(), "Gem");
        assert_eq!(
            message(&chain, "other", "Saved", None, &[]).unwrap(),
            "Saved"
        );
    }

    #[test]
    fn chain_falls_through_to_later_catalogs_then_to_msgid() {
        let danish = parse_catalog(DANISH).unwrap();
        let english = parse_catalog(ENGLISH).unwrap();
        let chain = [&danish, &english];
        assert_eq!(
            message(&chain, "", "Only danish", None, &[]).unwrap(),
            "Kun dansk"
        );
        assert!(message(&chain, "", "{count} file", None, &[]).is_err());
        assert_eq!(message(&chain, "", "Gone", None, &[]).unwrap(), "Gone");
        assert_eq!(
            message(&[&english], "", "Saved", None, &[]).unwrap(),
            "Saved"
        );
    }

    #[test]
    fn plural_call_without_translation_uses_the_english_source() {
        let chain: [&Catalog; 0] = [];
        assert_eq!(
            message(
                &chain,
                "",
                "{count} file",
                Some(("{count} files", 1)),
                &[("count", "1")]
            )
            .unwrap(),
            "1 file"
        );
        assert_eq!(
            message(
                &chain,
                "",
                "{count} file",
                Some(("{count} files", 2)),
                &[("count", "2")]
            )
            .unwrap(),
            "2 files"
        );
    }

    #[test]
    fn singular_call_on_plural_entry_is_an_error() {
        let english = parse_catalog(ENGLISH).unwrap();
        let error = message(&[&english], "", "{count} file", None, &[]).unwrap_err();
        assert!(matches!(error, I18nError::PluralFormsNeedCount { .. }));
    }

    #[test]
    fn placeholders_are_filled_by_name_in_any_order() {
        let danish = parse_catalog(DANISH).unwrap();
        let chain = [&danish];
        assert_eq!(
            message(&chain, "", "Hello {name}", None, &[("name", "Ada")]).unwrap(),
            "Hej Ada"
        );
        assert_eq!(
            fill("{a} and {b}", &[("b", "2"), ("a", "1")]).unwrap(),
            "1 and 2"
        );
    }

    #[test]
    fn missing_placeholder_is_an_error_naming_it() {
        let error = fill("Hi {who}", &[]).unwrap_err();
        assert_eq!(error, I18nError::MissingPlaceholder("who".to_string()));
    }

    #[test]
    fn substituted_values_are_never_rescanned() {
        assert_eq!(fill("{a}", &[("a", "{b}"), ("b", "X")]).unwrap(), "{b}");
        assert_eq!(fill("{a}!", &[("a", "}}{{")]).unwrap(), "}}{{!");
    }

    #[test]
    fn doubled_braces_are_literal_and_bad_braces_are_errors() {
        assert_eq!(fill("{{x}} {v}", &[("v", "1")]).unwrap(), "{x} 1");
        assert!(matches!(
            fill("oops }", &[]),
            Err(I18nError::Template { .. })
        ));
        assert!(matches!(
            fill("open {name", &[("name", "x")]),
            Err(I18nError::Template { .. })
        ));
        assert!(matches!(
            fill("{a b}", &[]),
            Err(I18nError::Template { .. })
        ));
        assert!(matches!(fill("{}", &[]), Err(I18nError::Template { .. })));
    }

    #[test]
    fn locale_tags_are_normalised() {
        assert_eq!(normalise_locale("da_DK.UTF-8").as_deref(), Some("da_DK"));
        assert_eq!(normalise_locale("de_DE@euro").as_deref(), Some("de_DE"));
        assert_eq!(normalise_locale("da-dk").as_deref(), Some("da_DK"));
        assert_eq!(normalise_locale("EN_us").as_deref(), Some("en_US"));
        assert_eq!(normalise_locale("fr").as_deref(), Some("fr"));
        assert_eq!(normalise_locale("C"), None);
        assert_eq!(normalise_locale("POSIX"), None);
        assert_eq!(normalise_locale(""), None);
        assert_eq!(normalise_locale("en_US_x"), None);
    }

    #[test]
    fn resolution_tries_exact_then_language_then_default() {
        let available = ["de", "da", "en", "da_DK"];
        assert_eq!(
            resolve_catalogs("da_DK.UTF-8:de", &available, "en"),
            vec![3, 1, 0, 2]
        );
        let regional = ["pt", "en"];
        assert_eq!(resolve_catalogs("pt_BR", &regional, "en"), vec![0, 1]);
        assert_eq!(resolve_catalogs("fr_CA", &["en"], "en"), vec![0]);
        assert_eq!(resolve_catalogs("C", &["en", "da"], "en"), vec![0]);
        assert_eq!(resolve_catalogs("", &["en", "da"], "da"), vec![1]);
        assert_eq!(resolve_catalogs("en_US:en", &["en"], "en"), vec![0]);
    }

    #[test]
    fn catalog_syntax_error_reports_a_line_number() {
        let source = "{\n\n\n  \"messages\": 7\n}";
        let error = parse_catalog(source).unwrap_err();
        assert!(matches!(error, I18nError::Syntax(_)));
        assert!(error.to_string().contains("line 4"), "{error}");
    }

    #[test]
    fn unknown_top_level_keys_are_rejected() {
        let error = parse_catalog(r#"{"messages":{},"extra":1}"#).unwrap_err();
        assert!(error.to_string().contains("extra"), "{error}");
    }

    #[test]
    fn empty_message_id_is_rejected() {
        let error = parse_catalog(r#"{"messages":{"":{"":"x"}}}"#).unwrap_err();
        assert!(error.to_string().contains("message id is empty"), "{error}");
    }

    #[test]
    fn size_and_entry_limits_are_enforced() {
        let error = parse_catalog_limited(ENGLISH, 10, 100).unwrap_err();
        assert!(matches!(error, I18nError::TooLarge { limit: 10, .. }));
        let error = parse_catalog_limited(ENGLISH, 10_000, 1).unwrap_err();
        assert_eq!(error, I18nError::TooManyEntries { limit: 1 });
        assert!(parse_catalog_limited(ENGLISH, 10_000, 2).is_ok());
    }
}
