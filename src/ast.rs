use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    String(String),
    Integer(i64),
    Bool(bool),
    List(Vec<Value>),
    Record(BTreeMap<String, Value>),
    Null,
    /// Unsigned or fractional JSON numbers; never implicitly coerced to an integer.
    Number(serde_json::Number),
    Reference(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Connection {
    Start,
    Then,
    Pipe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub operation: String,
    /// A quoted executable name bypasses the named applet catalog.
    #[serde(default, skip_serializing_if = "is_false")]
    pub direct: bool,
    pub parameters: BTreeMap<String, Value>,
    pub binding: Option<String>,
    pub cache: bool,
    /// Explicitly retain a native exit code, including ordinary nonzero exits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_binding: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Scalar equality; there is no shell expansion or implicit truthiness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub left: Value,
    pub right: Value,
    pub negated: bool,
}

/// A bounded loop; values resolve in the enclosing lexical scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LoopKind {
    Repeat { count: Value },
    ForEach { binding: String, values: Value },
    While { condition: Condition },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StepKind {
    /// `end` is relative to the header and points just beyond EndLoop.
    Loop {
        kind: LoopKind,
        end: usize,
    },
    /// `back` is a backward offset from this marker to its Loop header.
    EndLoop {
        back: usize,
    },
    /// Parser-lowered forward offsets, relative to this statement. `end` points
    /// just beyond EndIf; `otherwise` points to its body, or equals `end`.
    If {
        condition: Condition,
        otherwise: usize,
        end: usize,
    },
    Otherwise {
        end: usize,
    },
    EndIf,
    /// Expanded finite program body; end points just beyond its EndCall.
    CallProgram {
        call: Invocation,
        inputs: Vec<InputDeclaration>,
        end: usize,
        connection: Connection,
    },
    EndCall,
    /// A reusable body's first `read from stdin` reads its call input.
    ReadCallInput {
        binding: Option<String>,
    },
    UseFolder {
        path: Value,
    },
    Read {
        path: Value,
        binding: Option<String>,
    },
    ReadStdin {
        binding: Option<String>,
    },
    ParseJson {
        binding: String,
    },
    TakeJson {
        pointer: String,
        source: Value,
        kind: InputType,
        binding: String,
    },
    EncodeJson {
        source: Value,
    },
    Invoke {
        call: Invocation,
        connection: Connection,
    },
    Save {
        path: Value,
        connection: Connection,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub line: usize,
    pub kind: StepKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    #[serde(default, skip_serializing_if = "definitions_empty")]
    pub definitions: Arc<BTreeMap<String, ReusableProgram>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputDeclaration>,
    pub steps: Vec<Step>,
    pub trigger: Option<Trigger>,
}

fn definitions_empty(definitions: &Arc<BTreeMap<String, ReusableProgram>>) -> bool {
    definitions.is_empty()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReusableProgram {
    pub inputs: Vec<InputDeclaration>,
    pub steps: Vec<Step>,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputType {
    Text,
    Integer,
    Boolean,
    TextList,
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputDeclaration {
    pub name: String,
    pub kind: InputType,
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<InputSource>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputSource {
    Environment(String),
    Argument(usize),
    Arguments,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Trigger {
    Every {
        interval_ms: u64,
        line: usize,
    },
    Cron {
        expression: String,
        line: usize,
    },
    Listen {
        source: ListenSource,
        binding: String,
        line: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ListenSource {
    Stdin,
    Applet(Invocation),
}
