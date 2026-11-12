//! Small, explicit JSON boundary for the byte-oriented runtime.
use crate::ast::{InputDeclaration, InputType, Value};
use anyhow::{anyhow, bail, Context, Result};
use serde::{
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
    Deserialize, Serialize,
};
use std::{collections::BTreeMap, io::Write};

pub(crate) fn from_json(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(value) => match value.as_i64() {
            Some(integer) => Value::Integer(integer),
            None => Value::Number(value),
        },
        serde_json::Value::String(value) => Value::String(value),
        serde_json::Value::Array(values) => {
            Value::List(values.into_iter().map(from_json).collect())
        }
        serde_json::Value::Object(values) => Value::Record(
            values
                .into_iter()
                .map(|(key, value)| (key, from_json(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
fn old_parse(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes)
        .map(from_json)
        .context("parsing JSON")
}

/// Serialize directly from the typed tree; no second JSON tree or intermediate string.
struct Plain<'a>(&'a Value);
impl Serialize for Plain<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self.0 {
            Value::Null => serializer.serialize_none(),
            Value::String(value) => serializer.serialize_str(value),
            Value::Integer(value) => serializer.serialize_i64(*value),
            Value::Number(value) => value.serialize(serializer),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::List(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(&Plain(value))?;
                }
                seq.end()
            }
            Value::Record(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, &Plain(value))?;
                }
                map.end()
            }
            Value::Reference(_) => Err(serde::ser::Error::custom(
                "cannot encode an unresolved reference",
            )),
        }
    }
}

pub(crate) fn to_json(value: &Value) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(Plain(value))?)
}

pub(crate) fn literal(value: &Value) -> bool {
    match value {
        Value::Reference(_) => false,
        Value::List(values) => values.iter().all(literal),
        Value::Record(values) => values.values().all(literal),
        _ => true,
    }
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice::<JsonDocumentValue>(strip_utf8_bom(bytes))
        .map(|value| value.0)
        .context("parsing JSON")
}

/// Remove one UTF-8 byte-order mark from the beginning of a text document.
pub(crate) fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

/// Parse applet JSON directly into `serde_json::Value`, rejecting duplicate
/// object keys without an intermediate interpreter value.
pub(crate) fn parse_json_value(bytes: &[u8]) -> Result<serde_json::Value> {
    serde_json::from_slice::<JsonValueDocument>(strip_utf8_bom(bytes))
        .map(|document| document.0)
        .context("parsing JSON")
}

struct JsonValueDocument(serde_json::Value);

impl<'de> Deserialize<'de> for JsonValueDocument {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = JsonValueDocument;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(serde_json::Value::Null))
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_unit()
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(serde_json::Value::Number)
                    .map(JsonValueDocument)
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(value) = sequence.next_element::<JsonValueDocument>()? {
                    values.push(value.0);
                }
                Ok(JsonValueDocument(values.into()))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, JsonValueDocument>()? {
                    match values.entry(key) {
                        serde_json::map::Entry::Vacant(entry) => {
                            entry.insert(value.0);
                        }
                        serde_json::map::Entry::Occupied(entry) => {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate key {:?}",
                                entry.key()
                            )));
                        }
                    }
                }
                Ok(JsonValueDocument(values.into()))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

struct JsonDocumentValue(Value);

impl<'de> Deserialize<'de> for JsonDocumentValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = JsonDocumentValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::Null))
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::Null))
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::Integer(value)))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(i64::try_from(value).map_or_else(
                    |_| Value::Number(value.into()),
                    Value::Integer,
                )))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .map(JsonDocumentValue)
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::String(value)))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(value) = sequence.next_element::<JsonDocumentValue>()? {
                    values.push(value.0);
                }
                Ok(JsonDocumentValue(Value::List(values)))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, JsonDocumentValue>()? {
                    match values.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(value.0);
                        }
                        std::collections::btree_map::Entry::Occupied(entry) => {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate key {:?}",
                                entry.key()
                            )));
                        }
                    }
                }
                Ok(JsonDocumentValue(Value::Record(values)))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

pub(crate) fn validate_pointer(pointer: &str) -> Result<()> {
    if !pointer.is_empty() && !pointer.starts_with('/') {
        bail!("JSON pointer must be empty or start with / (for example /items/0/name)");
    }
    let mut chars = pointer.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
            bail!("JSON pointer escape must be ~0 or ~1");
        }
    }
    Ok(())
}

pub(crate) fn take<'a>(
    pointer: &str,
    value: &'a Value,
    kind: InputType,
    line: usize,
) -> Result<&'a Value> {
    validate_pointer(pointer)?;
    let mut selected = value;
    if !pointer.is_empty() {
        for part in pointer[1..].split('/') {
            let key = part.replace("~1", "/").replace("~0", "~");
            selected = match selected {
                Value::Record(values) => values.get(&key),
                Value::List(values) => {
                    if key.is_empty()
                        || (key.len() > 1 && key.starts_with('0'))
                        || !key.bytes().all(|b| b.is_ascii_digit())
                    {
                        None
                    } else {
                        key.parse::<usize>()
                            .ok()
                            .and_then(|index| values.get(index))
                    }
                }
                _ => None,
            }
            .ok_or_else(|| anyhow!("JSON pointer {pointer:?} does not exist"))?;
        }
    }
    InputDeclaration {
        name: format!("JSON pointer {pointer}"),
        kind,
        default: None,
        source: None,
        public: false,
        line,
        description: None,
        choices: None,
    }
    .validate_value(selected)?;
    Ok(selected)
}

pub(crate) fn source<'a>(
    value: &'a Value,
    bindings: &'a BTreeMap<String, Value>,
) -> Result<&'a Value> {
    match value {
        Value::Reference(name) => bindings
            .get(name)
            .ok_or_else(|| anyhow!("undefined result {name:?}")),
        value => Ok(value),
    }
}

pub(crate) fn encode(value: &Value, limit: usize) -> Result<Vec<u8>> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other(
                    "encoded JSON exceeds maximum output bytes",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut output, &Plain(value)).context("encoding JSON")?;
    Ok(output.bytes)
}

/// What one JSON pointer identified in a document parsed by [`walk_pointer`].
// Not wired: the runtime has no single-use liveness for JSON bindings (see NEEDS).
#[allow(dead_code)]
pub(crate) enum PointerSelection {
    Absent,
    Found(serde_json::Value),
    /// The pointer cannot select a value; the text is the applet error message.
    Failed(String),
}

/// The result of [`walk_pointer`]: the whole document's measured size and depth, plus the
/// pointer's selection. The size is the compact length that `applets` checks against its limit.
// Not wired: the runtime has no single-use liveness for JSON bindings (see NEEDS).
#[allow(dead_code)]
pub(crate) struct PointerWalk {
    pub(crate) encoded_bytes: usize,
    pub(crate) max_depth: usize,
    pub(crate) selected: PointerSelection,
}

/// What a node does during the parse. `Skip` validates and measures a node that cannot
/// affect the selection, `Path(n)` is the node that segment `n` must match, and `Build`
/// materializes the selected value.
#[derive(Clone, Copy)]
enum WalkMode {
    Skip,
    Path(usize),
    Build,
}

struct Walk<'p> {
    segments: &'p [String],
    encoded_bytes: usize,
    max_depth: usize,
    selected: Option<PointerSelection>,
}

impl Walk<'_> {
    /// A pointer identifies at most one node, so the first outcome written is the only one.
    fn select(&mut self, outcome: PointerSelection) {
        if self.selected.is_none() {
            self.selected = Some(outcome);
        }
    }
}

struct Node<'w, 'p> {
    walk: &'w mut Walk<'p>,
    mode: WalkMode,
    depth: usize,
}

fn descend(segment_count: usize, position: usize) -> WalkMode {
    if position + 1 == segment_count {
        WalkMode::Build
    } else {
        WalkMode::Path(position + 1)
    }
}

enum IndexStatus {
    Absent,
    At(usize),
    /// The pointer cannot select a value; the text is the applet error message.
    Failed(String),
}

/// Mirrors the list branch of the applet pointer lookup: `-`, names and empty segments
/// are absent; digit strings with a leading zero, or too large for `usize`, are errors.
fn index_status(segment: &str) -> IndexStatus {
    if segment == "-" || segment.is_empty() || !segment.bytes().all(|byte| byte.is_ascii_digit()) {
        return IndexStatus::Absent;
    }
    if segment.len() > 1 && segment.starts_with('0') {
        return IndexStatus::Failed(format!("invalid JSON Pointer array index {segment:?}"));
    }
    match segment.parse::<usize>() {
        Ok(index) => IndexStatus::At(index),
        Err(_) => IndexStatus::Failed(format!(
            "JSON Pointer array index is too large: {segment:?}"
        )),
    }
}

/// Counts bytes written, without allocating the text.
fn encoded_str_len(text: &str) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    // Writing to a counter cannot fail.
    let _ = serde_json::to_writer(&mut count, text);
    count.0
}

/// The length of a number's display form, as `applets` measures numbers.
fn display_len(value: impl std::fmt::Display) -> usize {
    use std::fmt::Write;
    struct Count(usize);
    impl Write for Count {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0 = self.0.saturating_add(text.len());
            Ok(())
        }
    }
    let mut count = Count(0);
    // Writing to a counter cannot fail.
    let _ = write!(count, "{value}");
    count.0
}

impl Node<'_, '_> {
    fn enter(&mut self, bytes: usize) {
        self.walk.max_depth = self.walk.max_depth.max(self.depth);
        self.walk.encoded_bytes = self.walk.encoded_bytes.saturating_add(bytes);
    }

    fn scalar<E: serde::de::Error>(
        mut self,
        length: usize,
        build: impl FnOnce() -> serde_json::Value,
    ) -> Result<serde_json::Value, E> {
        self.enter(length);
        match self.mode {
            WalkMode::Skip => Ok(serde_json::Value::Null),
            WalkMode::Build => Ok(build()),
            // A scalar has no children, so a pointer with segments left cannot reach a value.
            WalkMode::Path(_) => {
                self.walk.select(PointerSelection::Absent);
                Ok(serde_json::Value::Null)
            }
        }
    }

    fn sequence<'de, A: SeqAccess<'de>>(
        mut self,
        mut seq: A,
    ) -> Result<serde_json::Value, A::Error> {
        self.enter(2);
        let mode = self.mode;
        let target = match mode {
            WalkMode::Path(position) => {
                match index_status(self.walk.segments.get(position).map_or("", String::as_str)) {
                    IndexStatus::At(index) => Some(index),
                    IndexStatus::Absent => {
                        self.walk.select(PointerSelection::Absent);
                        None
                    }
                    IndexStatus::Failed(error) => {
                        self.walk.select(PointerSelection::Failed(error));
                        None
                    }
                }
            }
            WalkMode::Skip | WalkMode::Build => None,
        };
        let child_depth = self.depth + 1;
        let mut items = Vec::new();
        let mut index = 0usize;
        loop {
            let child_mode = match mode {
                WalkMode::Skip => WalkMode::Skip,
                WalkMode::Build => WalkMode::Build,
                WalkMode::Path(position) if target == Some(index) => {
                    descend(self.walk.segments.len(), position)
                }
                WalkMode::Path(_) => WalkMode::Skip,
            };
            let Some(value) = seq.next_element_seed(Node {
                walk: &mut *self.walk,
                mode: child_mode,
                depth: child_depth,
            })?
            else {
                break;
            };
            if index > 0 {
                self.enter(1);
            }
            match mode {
                WalkMode::Build => items.push(value),
                WalkMode::Path(_) if matches!(child_mode, WalkMode::Build) => {
                    self.walk.select(PointerSelection::Found(value));
                }
                WalkMode::Path(_) | WalkMode::Skip => {}
            }
            index += 1;
        }
        if let Some(target) = target {
            if target >= index {
                self.walk.select(PointerSelection::Absent);
            }
        }
        Ok(match mode {
            WalkMode::Build => serde_json::Value::Array(items),
            WalkMode::Skip | WalkMode::Path(_) => serde_json::Value::Null,
        })
    }

    fn object<'de, A: MapAccess<'de>>(mut self, mut map: A) -> Result<serde_json::Value, A::Error> {
        self.enter(2);
        let mode = self.mode;
        let child_depth = self.depth + 1;
        let mut fields = serde_json::Map::new();
        // Duplicate keys are checked after each value, as the typed parser does.
        let mut seen = std::collections::HashSet::new();
        let mut count = 0usize;
        while let Some(key) = map.next_key::<String>()? {
            if count > 0 {
                self.enter(1);
            }
            count += 1;
            self.enter(encoded_str_len(&key) + 1);
            let child_mode = match mode {
                WalkMode::Skip => WalkMode::Skip,
                WalkMode::Build => WalkMode::Build,
                WalkMode::Path(position)
                    if self
                        .walk
                        .segments
                        .get(position)
                        .is_some_and(|segment| *segment == key) =>
                {
                    descend(self.walk.segments.len(), position)
                }
                WalkMode::Path(_) => WalkMode::Skip,
            };
            let value = map.next_value_seed(Node {
                walk: &mut *self.walk,
                mode: child_mode,
                depth: child_depth,
            })?;
            if !seen.insert(key.clone()) {
                return Err(<A::Error as serde::de::Error>::custom(format!(
                    "duplicate key {key:?}"
                )));
            }
            match mode {
                WalkMode::Build => {
                    fields.insert(key, value);
                }
                WalkMode::Path(_) if matches!(child_mode, WalkMode::Build) => {
                    self.walk.select(PointerSelection::Found(value));
                }
                WalkMode::Path(_) | WalkMode::Skip => {}
            }
        }
        if let WalkMode::Path(_) = mode {
            // Reached only when no matching child selected anything.
            self.walk.select(PointerSelection::Absent);
        }
        Ok(match mode {
            WalkMode::Build => serde_json::Value::Object(fields),
            WalkMode::Skip | WalkMode::Path(_) => serde_json::Value::Null,
        })
    }
}

impl<'de> DeserializeSeed<'de> for Node<'_, '_> {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Node<'_, '_> {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.scalar(4, || serde_json::Value::Null)
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_unit()
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.scalar(if value { 4 } else { 5 }, || serde_json::Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.scalar(display_len(value), || serde_json::Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.scalar(display_len(value), || serde_json::Value::from(value))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let number =
            serde_json::Number::from_f64(value).ok_or_else(|| E::custom("invalid JSON number"))?;
        self.scalar(display_len(&number), move || {
            serde_json::Value::Number(number)
        })
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.scalar(encoded_str_len(value), || {
            serde_json::Value::String(value.to_owned())
        })
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let length = encoded_str_len(&value);
        self.scalar(length, move || serde_json::Value::String(value))
    }

    fn visit_seq<A>(self, seq: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.sequence(seq)
    }

    fn visit_map<A>(self, map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.object(map)
    }
}

/// Parses `bytes` exactly as [`parse`] does (same syntax, duplicate-key and trailing-input
/// errors, same error text), but materializes only the value that `segments` identifies.
/// Every other node is still validated and measured, so the result reports the same
/// encoded size and depth as the typed tree would. The applet layer applies the limits
/// and the default, in the same order as `get_typed`.
// Not wired: the runtime has no single-use liveness for JSON bindings (see NEEDS).
#[allow(dead_code)]
pub(crate) fn walk_pointer(bytes: &[u8], segments: &[String]) -> Result<PointerWalk> {
    let mut walk = Walk {
        segments,
        encoded_bytes: 0,
        max_depth: 0,
        selected: None,
    };
    let mode = if segments.is_empty() {
        WalkMode::Build
    } else {
        WalkMode::Path(0)
    };
    let mut deserializer = serde_json::Deserializer::from_slice(strip_utf8_bom(bytes));
    let root = Node {
        walk: &mut walk,
        mode,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .context("parsing JSON")?;
    deserializer.end().context("parsing JSON")?;
    if segments.is_empty() {
        walk.select(PointerSelection::Found(root));
    }
    Ok(PointerWalk {
        encoded_bytes: walk.encoded_bytes,
        max_depth: walk.max_depth,
        selected: walk.selected.unwrap_or(PointerSelection::Absent),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_parser_matches_serde_reference() {
        let documents = [
            r#"null"#,
            r#"{"unicode":"雪 café","escaped":"line\nquote\"slash\\","nested":[{},[],{"x":true}]}"#,
            r#"[-9223372036854775808,9223372036854775807,9223372036854775808,18446744073709551615,-1,0,1,1.0,-2.5,1e20]"#,
            r#"{"same":1,"empty_object":{},"empty_array":[]}"#,
        ];
        for document in documents {
            let bytes = document.as_bytes();
            assert_eq!(
                parse(bytes).unwrap(),
                old_parse(bytes).unwrap(),
                "{document}"
            );
        }

        for bytes in [&b""[..], b"null trailing", b"{"] {
            assert_eq!(
                format!("{:#}", parse(bytes).unwrap_err()),
                format!("{:#}", old_parse(bytes).unwrap_err()),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }

        assert!(to_json(&Value::Reference("missing".into()))
            .unwrap_err()
            .to_string()
            .contains("cannot encode an unresolved reference"));
    }

    #[test]
    fn duplicate_json_object_keys_are_rejected_with_source_position() {
        let error = parse(br#"{"a":1,"a":2}"#).expect_err("duplicate key must fail");
        let message = format!("{error:#}");
        assert!(message.contains("duplicate key \"a\""), "{message}");
        assert!(message.contains("line 1 column"), "{message}");
    }
}
