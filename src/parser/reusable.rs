//! Same-file finite programs lower into scoped instructions in the shared engine.
use super::*;
use crate::ast::ReusableProgram;
use std::sync::Arc;

const MAX_CALL_DEPTH: usize = 16;
const MAX_EXPANDED_STEPS: usize = 100_000;
const MAX_EXPANDED_BYTES: usize = 16 * 1024 * 1024;

struct Budget {
    steps: usize,
    bytes: usize,
}
impl Budget {
    fn charge(&mut self, value: &impl serde::Serialize) -> Result<()> {
        struct Counter(usize);
        impl std::io::Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
                    std::io::Error::other("reusable program expansion exceeds 16 MiB")
                })?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Padding covers adjusted jump digits and internal instruction tags.
        let mut counter = Counter(
            self.bytes
                .checked_sub(16)
                .context("reusable program expansion exceeds 16 MiB")?,
        );
        serde_json::to_writer(&mut counter, value)?;
        self.bytes = counter.0;
        Ok(())
    }
    fn instruction(&mut self) -> Result<()> {
        self.steps = self.steps.checked_sub(1).ok_or_else(|| {
            anyhow!("expanded reusable programs exceed {MAX_EXPANDED_STEPS} instructions")
        })?;
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct BorrowedStep<'a> {
    line: usize,
    kind: BorrowedCall<'a>,
}
#[derive(serde::Serialize)]
enum BorrowedCall<'a> {
    CallProgram {
        call: &'a Invocation,
        inputs: &'a [InputDeclaration],
        end: usize,
        connection: Connection,
    },
}

pub(super) fn parse_definition<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
) -> Result<(String, ReusableProgram)> {
    let header = line_at(*cursor).expect("definition header");
    for (index, expected) in ["define", "a", "program", "named"].iter().enumerate() {
        if !word_is(header.tokens.get(index), expected) {
            bail!(
                "line {}: expected `define a program named <name>`",
                header.number
            );
        }
    }
    let name = parse_binding_identifier(header.tokens.get(4), "program name")?;
    ensure_end(header.tokens, 5).with_context(|| format!("line {}", header.number))?;
    *cursor += 1;
    let mut inputs = Vec::new();
    let mut steps = Vec::new();
    while let Some(line) = line_at(*cursor).filter(|line| line.indent > 0) {
        if line.indent != 2 {
            bail!(
                "line {}: reusable program body needs two spaces",
                line.number
            );
        }
        if word_is(line.tokens.first(), "accept") {
            if !steps.is_empty() {
                bail!(
                    "line {}: program inputs must precede its executable statements",
                    line.number
                );
            }
            inputs.push(
                parse_input(line.tokens, line.number)
                    .with_context(|| format!("line {}", line.number))?,
            );
            *cursor += 1;
        } else {
            parse_statement(line_at, cursor, 2, &mut steps, 0)?;
        }
    }
    if steps.is_empty() {
        bail!(
            "line {}: reusable program body cannot be empty",
            header.number
        );
    }
    if let Some(step) = steps
        .iter()
        .skip(1)
        .find(|step| matches!(step.kind, StepKind::ReadStdin { .. }))
    {
        bail!(
            "line {}: reusable stdin must be read at the start of its body",
            step.line
        );
    }
    crate::inputs::validate(&inputs)?;
    if let Some(input) = inputs.iter().find(|input| input.source.is_some()) {
        bail!("line {}: reusable inputs must be passed explicitly; read environment or arguments in the caller", input.line);
    }
    Ok((
        name,
        ReusableProgram {
            inputs,
            steps,
            line: header.number,
        },
    ))
}

pub(super) fn finish(
    inputs: Vec<InputDeclaration>,
    steps: Vec<Step>,
    trigger: Option<Trigger>,
    definitions: BTreeMap<String, ReusableProgram>,
) -> Result<Program> {
    if definitions.is_empty()
        && !steps
            .iter()
            .any(|step| matches!(step.kind, StepKind::CallProgram { .. }))
    {
        return Ok(Program {
            inputs,
            steps,
            trigger,
            definitions: Arc::default(),
        });
    }
    let mut budget = Budget {
        steps: MAX_EXPANDED_STEPS,
        bytes: MAX_EXPANDED_BYTES,
    };
    let mut compiled = BTreeMap::new();
    // Even unused definitions are expanded and semantically checked by the runtime.
    for (name, definition) in &definitions {
        budget.charge(&(name, &definition.inputs, definition.line))?;
        let mut active = vec![name.as_str()];
        let body = expand(
            &definition.steps,
            &definitions,
            &mut active,
            &mut budget,
            false,
        )?;
        compiled.insert(
            name.clone(),
            ReusableProgram {
                inputs: definition.inputs.clone(),
                steps: body,
                line: definition.line,
            },
        );
    }
    let steps = expand(&steps, &definitions, &mut Vec::new(), &mut budget, false)?;
    Ok(Program {
        inputs,
        steps,
        trigger,
        definitions: Arc::new(compiled),
    })
}

fn expand<'a>(
    raw: &[Step],
    definitions: &'a BTreeMap<String, ReusableProgram>,
    active: &mut Vec<&'a str>,
    budget: &mut Budget,
    call_body: bool,
) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut positions = Vec::with_capacity(raw.len() + 1);
    for (index, step) in raw.iter().enumerate() {
        positions.push(steps.len());
        budget.instruction()?;
        if let StepKind::CallProgram {
            call, connection, ..
        } = &step.kind
        {
            let (name, definition) =
                definitions.get_key_value(&call.operation).ok_or_else(|| {
                    anyhow!("line {}: undefined program {:?}", step.line, call.operation)
                })?;
            if active.contains(&name.as_str()) {
                bail!(
                    "line {}: recursive program call {:?} is unsupported",
                    step.line,
                    name
                );
            }
            if active.len() >= MAX_CALL_DEPTH {
                bail!(
                    "line {}: program calls exceed {MAX_CALL_DEPTH} levels",
                    step.line
                );
            }
            budget.charge(&BorrowedStep {
                line: step.line,
                kind: BorrowedCall::CallProgram {
                    call,
                    inputs: &definition.inputs,
                    end: MAX_EXPANDED_STEPS,
                    connection: *connection,
                },
            })?;
            active.push(name);
            let body = expand(&definition.steps, definitions, active, budget, true)?;
            active.pop();
            let end = body
                .len()
                .checked_add(2)
                .context("program call length overflow")?;
            steps.push(Step {
                line: step.line,
                kind: StepKind::CallProgram {
                    call: call.clone(),
                    inputs: definition.inputs.clone(),
                    end,
                    connection: *connection,
                },
            });
            steps.extend(body);
            budget.instruction()?;
            let end = Step {
                line: step.line,
                kind: StepKind::EndCall,
            };
            budget.charge(&end)?;
            steps.push(end);
        } else if call_body && matches!(step.kind, StepKind::ReadStdin { .. }) {
            budget.charge(step)?;
            if index != 0 {
                bail!(
                    "line {}: reusable stdin must be read at the start of its body",
                    step.line
                );
            }
            let StepKind::ReadStdin { binding } = &step.kind else {
                unreachable!()
            };
            steps.push(Step {
                line: step.line,
                kind: StepKind::ReadCallInput {
                    binding: binding.clone(),
                },
            });
        } else {
            budget.charge(step)?;
            steps.push(step.clone());
        }
    }
    positions.push(steps.len());
    for (index, original) in raw.iter().enumerate() {
        let lowered = &mut steps[positions[index]].kind;
        match (&original.kind, lowered) {
            (
                StepKind::If { otherwise, end, .. },
                StepKind::If {
                    otherwise: new_otherwise,
                    end: new_end,
                    ..
                },
            ) => {
                *new_otherwise = positions[index + otherwise] - positions[index];
                *new_end = positions[index + end] - positions[index];
            }
            (StepKind::Otherwise { end }, StepKind::Otherwise { end: new_end }) => {
                *new_end = positions[index + end] - positions[index]
            }
            _ => {}
        }
    }
    Ok(steps)
}
