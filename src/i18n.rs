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
//! string, used for every count, an array with one string per plural form, or an object keyed by
//! CLDR category name (`{"one": "...", "other": "..."}`). The object form is mapped onto the same
//! forms by [`PluralRule`]; the array form keeps its order. Empty strings count as untranslated. Placeholders are written `{name}`; `{{` and `}}` produce literal braces.
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
///
/// Each rule's form indices correspond to CLDR categories as follows. The object form of a
/// catalog entry (`{"one": ..., "other": ...}`) is mapped through the same indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralRule {
    /// `nplurals=1; plural=0;` (for example ja, zh, ko). Form 0 is `other`.
    OneForm,
    /// `nplurals=2; plural=(n != 1);` (for example en, de, da). Form 0 is `one`, form 1 is `other`.
    NotOne,
    /// `nplurals=2; plural=(n > 1);` (for example fr). Form 0 is `one`, form 1 is `other`.
    GreaterThanOne,
    /// `nplurals=3`, the Russian rule shared by Ukrainian, Serbian and Croatian. Form 0 is `one`,
    /// form 1 is `few`, form 2 is `many`. `other` is accepted as another name for form 2, since
    /// CLDR uses it for the same slot in these languages when the count is not an integer.
    Slavic,
    /// `nplurals=3`, the Polish rule from the gettext manual. The category mapping is the same as
    /// [`PluralRule::Slavic`].
    Polish,
}

/// The CLDR plural category names, in the order the specification lists them.
const CLDR_CATEGORIES: [&str; 6] = ["zero", "one", "two", "few", "many", "other"];

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
    /// The form index that a CLDR category name selects, or `None` when the rule has no such form.
    fn category_slot(self, category: &str) -> Option<usize> {
        match (self, category) {
            (Self::OneForm, "other") => Some(0),
            (Self::NotOne | Self::GreaterThanOne, "one") => Some(0),
            (Self::NotOne | Self::GreaterThanOne, "other") => Some(1),
            (Self::Slavic | Self::Polish, "one") => Some(0),
            (Self::Slavic | Self::Polish, "few") => Some(1),
            (Self::Slavic | Self::Polish, "many" | "other") => Some(2),
            _ => None,
        }
    }

    /// The CLDR category name of form `index`, for error messages.
    fn form_name(self, index: usize) -> &'static str {
        match (self, index) {
            (Self::OneForm, _) => "other",
            (Self::NotOne | Self::GreaterThanOne, 0) => "one",
            (Self::NotOne | Self::GreaterThanOne, _) => "other",
            (Self::Slavic | Self::Polish, 0) => "one",
            (Self::Slavic | Self::Polish, 1) => "few",
            (Self::Slavic | Self::Polish, _) => "many",
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
    Categories(HashMap<String, String>),
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

/// Converts the object form `{"one": ..., "other": ...}` into one string per plural form.
///
/// Each category must be a CLDR name that the rule has, each form may be named once, and every
/// form must be present (an empty string is allowed and means untranslated). Errors name the
/// category at fault.
fn categories_to_forms(
    rule: PluralRule,
    categories: &HashMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut named: Vec<(&str, &str)> = categories
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_str()))
        .collect();
    named.sort_unstable();
    let mut slots: Vec<Option<(&str, &str)>> = vec![None; rule.form_count()];
    for (name, text) in named {
        if !CLDR_CATEGORIES.contains(&name) {
            return Err(format!(
                "`{name}` is not a CLDR plural category (zero, one, two, few, many, other)"
            ));
        }
        let Some(index) = rule.category_slot(name) else {
            let forms: Vec<&str> = (0..rule.form_count())
                .map(|index| rule.form_name(index))
                .collect();
            return Err(format!(
                "`{name}` is not a form of the catalog's plural rule, which has {}",
                forms.join(", ")
            ));
        };
        let slot = slots
            .get_mut(index)
            .ok_or_else(|| format!("`{name}` has no form slot"))?;
        if let Some((earlier, _)) = *slot {
            return Err(format!(
                "`{earlier}` and `{name}` both give form {index} of the plural rule"
            ));
        }
        *slot = Some((name, text));
    }
    let mut forms = Vec::with_capacity(slots.len());
    for (index, slot) in slots.into_iter().enumerate() {
        match slot {
            Some((_, text)) => forms.push(text.to_string()),
            None => return Err(format!("has no `{}` form", rule.form_name(index))),
        }
    }
    Ok(forms)
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
                RawEntry::Categories(categories) => {
                    Entry::Forms(categories_to_forms(plural, &categories).map_err(describe)?)
                }
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
    InvalidNumber(String),
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
            Self::InvalidNumber(text) => write!(
                f,
                "`{text}` is not a plain decimal number: an optional sign, digits, and an optional fraction after a period"
            ),
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

/// Longest locale variable value, in bytes, that [`default_language`] reads.
const MAX_ENV_LOCALE_BYTES: usize = 64;

/// The language the environment asks for, by POSIX precedence, as a normalised tag such as `da_DK`.
///
/// `env` looks a variable up by name and returns `None` when it is unset, so tests can pass a
/// closure. The effective locale is the first non-empty of `LC_ALL`, `LC_MESSAGES` and `LANG`.
/// When it is `C` or `POSIX`, with or without a codeset, the result is `None` and `LANGUAGE` is
/// ignored, as gettext does. Otherwise `LANGUAGE` is read as a colon list: the first entry that is
/// a plausible tag is the answer, and a `C` or `POSIX` entry ends the list with `None`. Without a
/// usable entry, the effective locale is used. The codeset and modifier are dropped, so
/// `da_DK.UTF-8@euro` gives `da_DK`.
///
/// A value is read only if it is at most 64 bytes of ASCII letters, digits, `_`, `-`, `.` and `@`,
/// and then only its language and region are kept. Anything else is ignored, so no environment
/// value can become a path component.
pub fn default_language(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    let effective = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|name| non_empty_variable(&env, name))?;
    if is_c_or_posix(&effective) {
        return None;
    }
    if let Some(list) = non_empty_variable(&env, "LANGUAGE") {
        for entry in list.split(':') {
            if is_c_or_posix(entry) {
                return None;
            }
            if let Some(tag) = checked_tag(entry) {
                return Some(tag);
            }
        }
    }
    checked_tag(&effective)
}

fn non_empty_variable(env: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name).filter(|value| !value.is_empty())
}

fn is_c_or_posix(tag: &str) -> bool {
    matches!(tag.split(['.', '@']).next(), Some("C" | "POSIX"))
}

fn checked_tag(raw: &str) -> Option<String> {
    let plausible = raw.len() <= MAX_ENV_LOCALE_BYTES
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@'));
    if plausible {
        normalise_locale(raw)
    } else {
        None
    }
}

/// Decimal separator and digit-group separator for each language in [`format_number`]'s table.
const NUMBER_FORMATS: &[(&str, char, Option<char>)] = &[
    ("en", '.', Some(',')),
    ("da", ',', Some('.')),
    ("de", ',', Some('.')),
    ("fr", ',', Some('\u{202F}')),
    ("sv", ',', Some('\u{00A0}')),
    ("nb", ',', Some('\u{00A0}')),
];

/// Formats a decimal number, written as text, with the separators of a locale.
///
/// `value_text` is an optional sign (`-` or `+`), one or more digits, and an optional fraction
/// after a period, such as `-1234.50`. It is never converted to a float, so every digit and the
/// whole fraction are kept. Anything else is an [`I18nError::InvalidNumber`].
///
/// `locale` is a locale tag such as `da_DK`. This is a fixed table, not a CLDR implementation.
/// It covers these languages, and a region falls back to its language:
///
/// - `en`: `.` decimal, `,` grouping (`1,234.5`)
/// - `da`, `de`: `,` decimal, `.` grouping (`1.234,5`)
/// - `fr`: `,` decimal, U+202F narrow no-break space grouping (`1 234,5`)
/// - `sv`, `nb`: `,` decimal, U+00A0 no-break space grouping (`1 234,5`)
///
/// Every listed language groups the integer part in threes from the right, including four-digit
/// integer parts. CLDR has minimum-grouping rules for some of these locales that are not applied
/// here. The minus sign is always an ASCII hyphen-minus. A locale that is unknown or malformed
/// formats as plain text with a period and no grouping. That is not an error.
pub fn format_number(value_text: &str, locale: &str) -> Result<String, I18nError> {
    let (negative, unsigned) = match value_text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value_text.strip_prefix('+').unwrap_or(value_text)),
    };
    let (integer, fraction) = match unsigned.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (unsigned, None),
    };
    if !is_digits(integer) || fraction.is_some_and(|digits| !is_digits(digits)) {
        return Err(I18nError::InvalidNumber(value_text.to_string()));
    }
    let (decimal, group) = number_convention(locale);
    let mut out = String::with_capacity(value_text.len() + integer.len() / 3 + 1);
    if negative {
        out.push('-');
    }
    match group {
        Some(separator) => push_grouped(&mut out, integer, separator),
        None => out.push_str(integer),
    }
    if let Some(fraction) = fraction {
        out.push(decimal);
        out.push_str(fraction);
    }
    Ok(out)
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The decimal separator and group separator for `locale`, by language, or plain text.
fn number_convention(locale: &str) -> (char, Option<char>) {
    let Some(normal) = normalise_locale(locale) else {
        return ('.', None);
    };
    let language = normal
        .split_once('_')
        .map_or(normal.as_str(), |(language, _)| language);
    NUMBER_FORMATS
        .iter()
        .find(|(tag, _, _)| *tag == language)
        .map_or(('.', None), |&(_, decimal, group)| (decimal, group))
}

/// Appends `digits` with `separator` before every group of three from the right. The input is
/// ASCII digits, checked by the caller.
fn push_grouped(out: &mut String, digits: &str, separator: char) {
    let len = digits.len();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (len - index).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(digit);
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

    const ENGLISH_OBJECT: &str =
        r#"{"messages":{"":{"{count} file":{"one":"{count} file","other":"{count} files"}}}}"#;

    const RUSSIAN_OBJECT: &str = r#"{
      "plural_forms": "nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);",
      "messages": {"": {"{count} files": {"one": "{count} файл", "few": "{count} файла", "many": "{count} файлов"}}}
    }"#;

    const RUSSIAN_OTHER: &str = r#"{
      "plural_forms": "nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);",
      "messages": {"": {"{count} files": {"one": "{count} файл", "few": "{count} файла", "other": "{count} файлов"}}}
    }"#;

    fn render_count(catalog: &Catalog, msgid: &str, plural: &str, n: u64) -> String {
        let count = n.to_string();
        message(
            &[catalog],
            "",
            msgid,
            Some((plural, n)),
            &[("count", count.as_str())],
        )
        .unwrap()
    }

    #[test]
    fn plural_object_form_and_array_form_give_the_same_result() {
        let english_array = parse_catalog(ENGLISH).unwrap();
        let english_object = parse_catalog(ENGLISH_OBJECT).unwrap();
        for n in [0, 1, 2, 5, 11, 21, 100] {
            assert_eq!(
                render_count(&english_object, "{count} file", "{count} files", n),
                render_count(&english_array, "{count} file", "{count} files", n),
                "n = {n}"
            );
        }
        let russian_array = parse_catalog(RUSSIAN).unwrap();
        let russian_object = parse_catalog(RUSSIAN_OBJECT).unwrap();
        let russian_other = parse_catalog(RUSSIAN_OTHER).unwrap();
        for n in [1, 2, 5, 11, 12, 21, 22, 25, 101, 111, 112, 1000] {
            let expected = render_count(&russian_array, "{count} files", "{count} files", n);
            assert_eq!(
                render_count(&russian_object, "{count} files", "{count} files", n),
                expected,
                "n = {n}"
            );
            assert_eq!(
                render_count(&russian_other, "{count} files", "{count} files", n),
                expected,
                "n = {n}"
            );
        }
        assert_eq!(
            render_count(&russian_object, "{count} files", "{count} files", 21),
            "21 файл"
        );
    }

    #[test]
    fn plural_object_form_rejects_categories_the_rule_does_not_have() {
        let two = r#"{"messages":{"":{"x":{"one":"a","two":"b","other":"c"}}}}"#;
        let error = parse_catalog(two).unwrap_err();
        assert!(error.to_string().contains("`two`"), "{error}");

        let japanese = r#"{"plural_forms":"nplurals=1; plural=0;","messages":{"":{"x":{"one":"a","other":"b"}}}}"#;
        let error = parse_catalog(japanese).unwrap_err();
        assert!(error.to_string().contains("`one`"), "{error}");

        let unknown = r#"{"messages":{"":{"x":{"one":"a","other":"b","tiny":"c"}}}}"#;
        let error = parse_catalog(unknown).unwrap_err();
        assert!(
            error.to_string().contains("`tiny` is not a CLDR"),
            "{error}"
        );

        let missing = r#"{"messages":{"":{"x":{"other":"b"}}}}"#;
        let error = parse_catalog(missing).unwrap_err();
        assert!(error.to_string().contains("no `one` form"), "{error}");

        let duplicate = r#"{"plural_forms":"nplurals=3; plural=(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);","messages":{"":{"x":{"one":"a","few":"b","many":"c","other":"d"}}}}"#;
        let error = parse_catalog(duplicate).unwrap_err();
        assert!(error.to_string().contains("both give form 2"), "{error}");
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(key, _)| key.as_str() == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn default_language_follows_posix_precedence() {
        assert_eq!(
            default_language(env_of(&[
                ("LANG", "en_US.UTF-8"),
                ("LC_MESSAGES", "da_DK.UTF-8")
            ]))
            .as_deref(),
            Some("da_DK")
        );
        assert_eq!(
            default_language(env_of(&[
                ("LANG", "en_US.UTF-8"),
                ("LC_ALL", "de_DE.UTF-8"),
                ("LC_MESSAGES", "da_DK"),
            ]))
            .as_deref(),
            Some("de_DE")
        );
        assert_eq!(
            default_language(env_of(&[("LANG", "fr_FR.UTF-8")])).as_deref(),
            Some("fr_FR")
        );
        assert_eq!(default_language(env_of(&[])), None);
    }

    #[test]
    fn default_language_strips_codeset_and_modifier() {
        assert_eq!(
            default_language(env_of(&[("LANG", "da_DK.UTF-8@euro")])).as_deref(),
            Some("da_DK")
        );
        assert_eq!(
            default_language(env_of(&[("LANG", "da-dk")])).as_deref(),
            Some("da_DK")
        );
    }

    #[test]
    fn default_language_maps_c_and_posix_to_none() {
        for locale in ["C", "POSIX", "C.UTF-8", "POSIX.UTF-8"] {
            assert_eq!(
                default_language(env_of(&[("LANG", locale)])),
                None,
                "{locale}"
            );
        }
        // LC_ALL is the effective locale, so C there hides the da_DK in LANG.
        assert_eq!(
            default_language(env_of(&[("LC_ALL", "C"), ("LANG", "da_DK.UTF-8")])),
            None
        );
    }

    #[test]
    fn language_list_is_read_only_under_a_real_locale() {
        assert_eq!(
            default_language(env_of(&[("LANG", "C"), ("LANGUAGE", "da:de")])),
            None
        );
        assert_eq!(
            default_language(env_of(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "da:de")])).as_deref(),
            Some("da")
        );
        assert_eq!(
            default_language(env_of(&[
                ("LANG", "en_US.UTF-8"),
                ("LANGUAGE", "::de_DE.UTF-8"),
            ]))
            .as_deref(),
            Some("de_DE")
        );
        // A C entry in the list ends it, as it does in gettext.
        assert_eq!(
            default_language(env_of(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "C:da")])),
            None
        );
    }

    #[test]
    fn empty_locale_variables_count_as_unset() {
        assert_eq!(
            default_language(env_of(&[("LC_ALL", ""), ("LANG", "da_DK")])).as_deref(),
            Some("da_DK")
        );
        assert_eq!(
            default_language(env_of(&[("LANG", ""), ("LANGUAGE", "da")])),
            None
        );
    }

    #[test]
    fn hostile_environment_values_never_become_a_tag() {
        let long = "a".repeat(200);
        let hostile = [
            "../../etc/passwd",
            "da_DK/../../x",
            "da\nDK",
            "$(rm -rf ~)",
            "de_DE; rm",
            "..",
            "da_DK\\..\\x",
            "\u{0}",
            long.as_str(),
            "d",
            "da_DK_extra",
            "é_DK",
        ];
        for value in hostile {
            for name in ["LANG", "LC_ALL", "LC_MESSAGES"] {
                assert_eq!(
                    default_language(env_of(&[(name, value)])),
                    None,
                    "{name}={value:?}"
                );
            }
            // A hostile LANGUAGE entry is skipped and the usable locale is used.
            assert_eq!(
                default_language(env_of(&[("LANG", "da_DK"), ("LANGUAGE", value)])).as_deref(),
                Some("da_DK"),
                "LANGUAGE={value:?}"
            );
        }
    }

    #[test]
    fn danish_renders_13_1_with_a_decimal_comma() {
        assert_eq!(format_number("13.1", "da").unwrap(), "13,1");
        assert_eq!(format_number("13.1", "da_DK.UTF-8").unwrap(), "13,1");
    }

    #[test]
    fn listed_locales_use_their_separators() {
        assert_eq!(format_number("1234.5", "en_US").unwrap(), "1,234.5");
        assert_eq!(format_number("1234.5", "de").unwrap(), "1.234,5");
        assert_eq!(format_number("1234.5", "fr_FR").unwrap(), "1\u{202F}234,5");
        assert_eq!(format_number("1234.5", "sv").unwrap(), "1\u{A0}234,5");
        assert_eq!(format_number("1234.5", "nb_NO").unwrap(), "1\u{A0}234,5");
    }

    #[test]
    fn negative_and_signed_numbers_keep_exact_digits() {
        assert_eq!(format_number("-1234.50", "da").unwrap(), "-1.234,50");
        assert_eq!(format_number("-0.5", "en").unwrap(), "-0.5");
        assert_eq!(format_number("+1234", "en").unwrap(), "1,234");
        assert_eq!(format_number("007", "da").unwrap(), "007");
        assert_eq!(
            format_number("0.10000000000000000001", "de").unwrap(),
            "0,10000000000000000001"
        );
    }

    #[test]
    fn long_integer_parts_are_grouped_without_float_rounding() {
        assert_eq!(
            format_number("123456789012345678901234567890.5", "de").unwrap(),
            "123.456.789.012.345.678.901.234.567.890,5"
        );
        assert_eq!(format_number("12345", "en").unwrap(), "12,345");
        assert_eq!(format_number("123", "en").unwrap(), "123");
    }

    #[test]
    fn invalid_numbers_are_errors() {
        for text in [
            "", "-", "+", ".5", "5.", "1e3", "1,5", "12a", "1.2.3", "--1", "+-1", " 1", "1 ",
            "0x1F", "NaN", "\u{0661}",
        ] {
            assert_eq!(
                format_number(text, "en"),
                Err(I18nError::InvalidNumber(text.to_string())),
                "{text:?}"
            );
        }
    }

    #[test]
    fn unknown_locales_format_as_plain_text_not_an_error() {
        assert_eq!(format_number("1234.5", "xx").unwrap(), "1234.5");
        assert_eq!(format_number("1234.5", "").unwrap(), "1234.5");
        assert_eq!(format_number("1234.5", "../../etc").unwrap(), "1234.5");
        assert_eq!(format_number("-1234.5", "C").unwrap(), "-1234.5");
        assert_eq!(format_number("1234.5", "pt_BR").unwrap(), "1234.5");
        assert_eq!(format_number("1234.5", "fr_CA").unwrap(), "1\u{202F}234,5");
    }
}
