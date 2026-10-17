//! Per-element checkpoint records for top-level `for each` and `parallel for each`.
//!
//! A top-level `for each` (a `Loop` step) and a `parallel for each` (a `TaskGroup`
//! step with `each`) are one in-flight node. While such a node runs, each element
//! gets a `begin` line before it starts and a `commit` line once it has succeeded.
//! The lines live in `elements.jsonl`, separate from the completed-node log, and
//! only the in-flight node's log is ever read.
//!
//! Resume replays the log. Committed elements are skipped and their results are
//! restored: a sequential loop continues from the state after its last committed
//! element, and a parallel group takes the recorded results of its committed
//! elements. A committed element whose list entry changed refuses resume. Entries
//! appended after the committed ones run normally. A `begin` without a `commit`
//! is an element that may have started; it needs `--retry-uncertain` when its body
//! can have side effects.

use super::*;
use crate::ast::{LoopKind, Program, StepKind, TaskEach};
use crate::contracts::{EffectCertainty, Outcome, OutcomeKind};
use crate::manifest::Registry;
use crate::runtime::loops::LoopStack;
use crate::runtime::resume::{retry_safe, uncertain_on_failure};
use crate::runtime::tasks::GroupSink;
use crate::runtime::AppletInvocationResult;
use std::collections::BTreeSet;

const ELEMENT_LOG: &str = "elements.jsonl";

/// Where the element log stood at the last journal write. Element commits do not
/// write the journal, so lines after the anchor are covered by the chain alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ElementAnchor {
    pub lines: usize,
    pub chain: String,
}
const ELEMENT_CHAIN_LABEL: &str = "execute-checkpoint-elements/v4";
const ELEMENT_FINGERPRINT_LABEL: &str = "execute-element-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ElementLine {
    Begin {
        index: usize,
        fingerprint: String,
        uncertain: bool,
    },
    Commit {
        index: usize,
        fingerprint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ElementStateDisk>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Box<ElementResultDisk>>,
    },
}

/// A sequential element's state after it. Output is inline, so the commit line is
/// the only record that needs to be durable. Bindings reference blobs, which are
/// reused while the bindings stay the same.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ElementStateDisk {
    cwd: EncodedPath,
    output: String,
    bindings: BTreeMap<String, BlobRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ElementResultDisk {
    status: i32,
    outcome: OutcomeDisk,
    value: Option<Value>,
    uncertain: bool,
    launched_processes: usize,
    output: String,
    stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeDisk {
    status: i32,
    name: String,
    kind: OutcomeKind,
    effect_certainty: EffectCertainty,
}

/// A parallel element's recorded result, as the group reports it.
#[derive(Debug, Clone)]
pub(in crate::runtime) struct ElementOutcome {
    pub output: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: i32,
    pub outcome: Outcome,
    pub value: Option<Value>,
    pub uncertain: bool,
    pub launched_processes: usize,
}

impl ElementOutcome {
    pub(in crate::runtime) fn from_result(
        result: &AppletInvocationResult,
        launched_processes: usize,
    ) -> Self {
        Self {
            output: result.output.clone(),
            stderr: result.stderr.clone(),
            status: result.status,
            outcome: result.outcome.clone(),
            value: result.value.clone(),
            uncertain: result.uncertain,
            launched_processes,
        }
    }

    pub(in crate::runtime) fn into_result(self) -> AppletInvocationResult {
        AppletInvocationResult {
            output: self.output,
            status: self.status,
            outcome: self.outcome,
            value: self.value,
            stderr: self.stderr,
            uncertain: self.uncertain,
            launched_processes: self.launched_processes,
        }
    }
}

struct ElementCommit {
    index: usize,
    fingerprint: String,
    /// Sequential elements: the state after the element, loaded for the last commit.
    state: Option<ExecutionState>,
    /// Parallel elements: the recorded result.
    outcome: Option<ElementOutcome>,
    state_ref: Option<ElementStateDisk>,
}

/// Binding references from the last sequential commit, reused while the bindings and
/// cwd are unchanged.
pub(super) struct ElementStateCache {
    cwd: PathBuf,
    bindings: BTreeMap<String, Value>,
    refs: BTreeMap<String, BlobRef>,
}

struct PendingElement {
    index: usize,
    uncertain: bool,
}

/// The element log of the in-flight node, replayed when the journal opens.
pub(super) struct ElementReplay {
    commits: Vec<ElementCommit>,
    pending: Vec<PendingElement>,
}

/// What a sequential `for each` needs to continue after its committed elements.
pub(in crate::runtime) struct ForEachResume {
    pub binding: String,
    pub outer: BTreeSet<String>,
    pub items: Vec<Value>,
    pub consumed: usize,
    pub state: ExecutionState,
}

/// Records a parallel group's element begins and commits through the journal.
pub(in crate::runtime) struct GroupJournal<'a> {
    journal: &'a mut Journal,
    node_fingerprint: String,
    items: Vec<Value>,
    uncertain: bool,
}

impl GroupSink for GroupJournal<'_> {
    fn begin(&mut self, index: usize) -> Result<()> {
        if !self.uncertain {
            return Ok(());
        }
        let fingerprint = element_fingerprint(&self.node_fingerprint, index, &self.items[index])?;
        self.journal.append_element(&ElementLine::Begin {
            index,
            fingerprint,
            uncertain: self.uncertain,
        })
    }

    fn commit(&mut self, index: usize, outcome: &ElementOutcome) -> Result<()> {
        let fingerprint = element_fingerprint(&self.node_fingerprint, index, &self.items[index])?;
        self.journal.commit_outcome(index, fingerprint, outcome)
    }
}

/// Whether the top-level statement at `index` is an element node.
fn is_element_node(program: &Program, index: usize) -> bool {
    matches!(
        program.steps.get(index).map(|step| &step.kind),
        Some(StepKind::Loop {
            kind: LoopKind::ForEach { .. },
            ..
        }) | Some(StepKind::TaskGroup { each: Some(_), .. })
    )
}

/// Whether a node that is about to begin should keep element records instead of
/// a single node-level uncertainty decision.
pub(in crate::runtime) fn tracks_new_node(
    journal: &Journal,
    program: &Program,
    index: usize,
) -> bool {
    journal.element_tracking() && is_element_node(program, index)
}

/// Whether the node in flight at `index` keeps element records.
fn tracks_in_flight(journal: &Journal, program: &Program, index: usize) -> bool {
    tracks_new_node(journal, program, index) && journal.in_flight_start() == Some(index)
}

/// Stable identity for one element: the node, its position and its list entry.
fn element_fingerprint(node_fingerprint: &str, index: usize, item: &Value) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(ELEMENT_FINGERPRINT_LABEL.as_bytes());
    hash.update(b"\n");
    hash.update(node_fingerprint.as_bytes());
    hash.update(b"\n");
    hash.update(index.to_string().as_bytes());
    hash.update(b"\n");
    hash.update(serde_json::to_vec(item).context("encoding element identity")?);
    Ok(format!("{:x}", hash.finalize()))
}

/// Uncertainty of one element body: anything that can leave an effect unknown.
fn element_uncertain(
    program: &Program,
    start: usize,
    end: usize,
    registry: &Registry,
) -> Result<bool> {
    Ok(!retry_safe(program, start, end, registry)?
        || uncertain_on_failure(program, start, end, registry)?)
}

fn list_items(values: &Value, bindings: &BTreeMap<String, Value>) -> Result<Vec<Value>> {
    match crate::runtime::resolve(values, bindings)? {
        Value::List(items) => Ok(items),
        _ => bail!("for each requires a JSON array or list of text"),
    }
}

/// Check every committed element against the list as it resolves now.
fn verify_commits(
    node_fingerprint: &str,
    commits: &[ElementCommit],
    items: &[Value],
    sequential: bool,
) -> Result<()> {
    for (position, commit) in commits.iter().enumerate() {
        if sequential && commit.index != position {
            bail!("checkpoint element log is out of order at element {position}");
        }
        let changed = || {
            anyhow!(
                "for each list changed at element {}; use a fresh checkpoint",
                commit.index
            )
        };
        let item = items.get(commit.index).ok_or_else(changed)?;
        if element_fingerprint(node_fingerprint, commit.index, item)? != commit.fingerprint {
            return Err(anyhow!(
                "for each list changed at element {}; use a fresh checkpoint",
                commit.index
            ));
        }
    }
    Ok(())
}

/// An element that may have started without committing blocks resume unless the
/// operator explicitly allowed retrying uncertain work.
fn check_pending(journal: &Journal, pending: &[PendingElement]) -> Result<()> {
    if let Some(element) = pending.iter().find(|element| element.uncertain) {
        if !journal.retry_uncertain_allowed() {
            bail!(
                "for each element {} may contain an interrupted side effect; pass --retry-uncertain to retry",
                element.index
            );
        }
    }
    Ok(())
}

/// Sequential `for each`: when the in-flight loop has committed elements, the
/// state to continue from and the iterator position. `None` runs the loop normally.
pub(in crate::runtime) fn for_each_resume(
    journal: &mut Journal,
    program: &Program,
    index: usize,
    start: &ExecutionState,
    registry: &Registry,
) -> Result<Option<ForEachResume>> {
    if !tracks_in_flight(journal, program, index) {
        return Ok(None);
    }
    // Only a sequential for each resumes here. A parallel group takes its replay in
    // group_journal, so this must not consume it.
    let StepKind::Loop {
        kind: LoopKind::ForEach { binding, values },
        ..
    } = &program.steps[index].kind
    else {
        return Ok(None);
    };
    let Some(replay) = journal.take_element_replay() else {
        return Ok(None);
    };
    if replay.commits.is_empty() && replay.pending.is_empty() {
        return Ok(None);
    }
    let items = list_items(values, &start.bindings)?;
    let node_fingerprint = journal.in_flight_fingerprint()?;
    verify_commits(&node_fingerprint, &replay.commits, &items, true)?;
    check_pending(journal, &replay.pending)?;
    let _ = registry;
    let consumed = replay.commits.len();
    if consumed == 0 {
        return Ok(None);
    }
    let state = replay
        .commits
        .last()
        .and_then(|commit| commit.state.clone())
        .context("checkpoint element log has no state for its last element")?;
    Ok(Some(ForEachResume {
        binding: binding.clone(),
        outer: start.bindings.keys().cloned().collect(),
        items,
        consumed,
        state,
    }))
}

/// Record the begin of the element the loop header just yielded.
pub(in crate::runtime) fn for_each_element_begin(
    journal: &mut Journal,
    program: &Program,
    registry: &Registry,
    loops: &LoopStack,
    header: usize,
) -> Result<()> {
    if !tracks_in_flight(journal, program, header) {
        return Ok(());
    }
    let Some((element, item)) = loops.current_element(header) else {
        return Ok(());
    };
    let StepKind::Loop { end, .. } = &program.steps[header].kind else {
        return Ok(());
    };
    let uncertain = element_uncertain(program, header + 1, header + end, registry)?;
    // Retry-safe elements need no begin record: a crashed one simply runs again.
    if !uncertain {
        return Ok(());
    }
    let fingerprint = element_fingerprint(&journal.in_flight_fingerprint()?, element, item)?;
    journal.append_element(&ElementLine::Begin {
        index: element,
        fingerprint,
        uncertain,
    })
}

/// Whether the loop end at `header` closes a tracked element, so the caller can
/// materialize the output the element leaves behind.
pub(in crate::runtime) fn for_each_element_due(
    journal: &Journal,
    program: &Program,
    header: usize,
) -> bool {
    tracks_in_flight(journal, program, header)
}

/// Commit the element that just ended at the loop end for `header`.
pub(in crate::runtime) fn for_each_element_commit(
    journal: &mut Journal,
    loops: &LoopStack,
    header: usize,
    cwd: &Path,
    output: &[u8],
    bindings: &BTreeMap<String, Value>,
) -> Result<()> {
    let Some((element, item)) = loops.current_element(header) else {
        return Ok(());
    };
    let fingerprint = element_fingerprint(&journal.in_flight_fingerprint()?, element, item)?;
    journal.commit_state(element, fingerprint, cwd, output, bindings)
}

/// Parallel `parallel for each`: the journal sink for this group, and the
/// results of elements that already committed.
pub(in crate::runtime) fn group_journal<'a>(
    journal: Option<&'a mut Journal>,
    program: &Program,
    index: usize,
    each: &TaskEach,
    bindings: &BTreeMap<String, Value>,
    registry: &Registry,
    sensitivities: &crate::security::SensitivitySet,
) -> Result<Option<(GroupJournal<'a>, BTreeMap<usize, ElementOutcome>)>> {
    let Some(journal) = journal else {
        return Ok(None);
    };
    if !tracks_in_flight(journal, program, index) {
        return Ok(None);
    }
    // The floor is fixed for the run, so refusing here is the same refusal the first
    // element commit would get, and it happens before any element writes a line.
    crate::runtime::authorize_checkpoint_persistence(registry, sensitivities)?;
    let items = list_items(&each.values, bindings)?;
    let node_fingerprint = journal.in_flight_fingerprint()?;
    let mut skip = BTreeMap::new();
    if let Some(replay) = journal.take_element_replay() {
        verify_commits(&node_fingerprint, &replay.commits, &items, false)?;
        check_pending(journal, &replay.pending)?;
        for commit in replay.commits {
            let outcome = commit
                .outcome
                .context("checkpoint element log has no result for a parallel element")?;
            skip.insert(commit.index, outcome);
        }
    }
    let uncertain = element_uncertain(program, index, index + 1, registry)?;
    Ok(Some((
        GroupJournal {
            journal,
            node_fingerprint,
            items,
            uncertain,
        },
        skip,
    )))
}

/// Commits are group-synced: at most one sync per interval while elements run, and
/// always before the journal is written and when the journal is dropped.
const ELEMENT_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

fn hex_decode(text: &str) -> Result<Vec<u8>> {
    if !text.is_ascii() || !text.len().is_multiple_of(2) {
        bail!("checkpoint element output is malformed");
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16)
                .context("checkpoint element output is malformed")
        })
        .collect()
}

impl Journal {
    fn element_chain_start(fingerprint: &str, start: usize, end: usize) -> String {
        hex_digest(format!("{ELEMENT_CHAIN_LABEL}\n{fingerprint}\n{start}\n{end}").as_bytes())
    }

    /// Whether this journal keeps element records. Services have their own event
    /// lifecycle, so only finite runs track elements.
    pub(super) fn element_tracking(&self) -> bool {
        self.data.identity.mode == "finite"
    }

    fn in_flight_start(&self) -> Option<usize> {
        self.data.in_flight.as_ref().map(|node| node.start)
    }

    fn in_flight_fingerprint(&self) -> Result<String> {
        self.data
            .in_flight
            .as_ref()
            .map(|node| node.fingerprint.clone())
            .context("no checkpoint node is in flight")
    }

    fn retry_uncertain_allowed(&self) -> bool {
        self.retry_uncertain
    }

    /// Start an empty element log for a new node. A missing log stays missing.
    pub(super) fn reset_elements(
        &mut self,
        fingerprint: &str,
        start: usize,
        end: usize,
    ) -> Result<()> {
        self.elements_file = None;
        let path = self.directory.join(ELEMENT_LOG);
        if path
            .try_exists()
            .context("checking checkpoint element log")?
        {
            let file = open_regular(&path, "checkpoint element log", true)?;
            file.set_len(0).context("clearing checkpoint element log")?;
            file.sync_data().context("syncing checkpoint element log")?;
        }
        self.elements_chain = Self::element_chain_start(fingerprint, start, end);
        self.elements_bytes = 0;
        self.elements_lines = 0;
        self.elements_dirty = false;
        self.elements_synced_at = std::time::Instant::now();
        self.element_state_cache = None;
        self.element_replay = None;
        Ok(())
    }

    /// Replay the element log of the in-flight node. A torn final line from an
    /// interrupted append is not committed and is dropped by the next append.
    pub(super) fn load_elements(&mut self) -> Result<()> {
        let Some(node) = self.data.in_flight.clone() else {
            self.element_replay = None;
            return Ok(());
        };
        self.elements_file = None;
        self.elements_chain = Self::element_chain_start(&node.fingerprint, node.start, node.end);
        self.elements_bytes = 0;
        self.elements_lines = 0;
        self.elements_dirty = false;
        // The anchor is verified once the count reaches it. A log that ends before
        // the anchor was truncated below a state the journal already acknowledged.
        let anchor = node.elements.clone();
        let anchor_lines = anchor.as_ref().map_or(0, |anchor| anchor.lines);
        let mut anchor_seen = anchor_lines == 0;
        let mut lines = 0usize;
        let mut replay = ElementReplay {
            commits: Vec::new(),
            pending: Vec::new(),
        };
        let path = self.directory.join(ELEMENT_LOG);
        if path
            .try_exists()
            .context("checking checkpoint element log")?
        {
            let file = open_regular(&path, "checkpoint element log", false)?;
            restrict_file(&file)?;
            let mut reader = io::BufReader::new(file);
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = reader.read_until(b'\n', &mut line)?;
                if read == 0 || line.last() != Some(&b'\n') {
                    break;
                }
                line.pop();
                self.elements_chain = chain_step(&self.elements_chain, &line);
                self.elements_bytes += read as u64;
                lines += 1;
                if let Some(anchor) = anchor.as_ref().filter(|anchor| anchor.lines == lines) {
                    if anchor.chain != self.elements_chain {
                        bail!(
                            "checkpoint element log does not match its journal anchor; it was rewritten"
                        );
                    }
                    anchor_seen = true;
                }
                let entry: ElementLine = serde_json::from_slice(&line)
                    .context("checkpoint element log is malformed or corrupt")?;
                match entry {
                    ElementLine::Begin {
                        index, uncertain, ..
                    } => {
                        replay.pending.retain(|element| element.index != index);
                        replay.pending.push(PendingElement { index, uncertain });
                    }
                    ElementLine::Commit {
                        index,
                        fingerprint,
                        state,
                        result,
                    } => {
                        replay.pending.retain(|element| element.index != index);
                        let outcome = match result {
                            Some(disk) => Some(self.load_element_outcome(&disk)?),
                            None => None,
                        };
                        replay.commits.push(ElementCommit {
                            index,
                            fingerprint,
                            state: None,
                            outcome,
                            state_ref: state,
                        });
                    }
                }
            }
        }
        if !anchor_seen {
            bail!("checkpoint element log is shorter than its journal anchor; it was truncated");
        }
        self.elements_lines = lines;
        if let Some(last) = replay.commits.last_mut() {
            if let Some(disk) = last.state_ref.clone() {
                last.state = Some(self.load_element_state(&disk)?);
            }
        }
        self.element_replay = Some(replay);
        Ok(())
    }

    pub(super) fn take_element_replay(&mut self) -> Option<ElementReplay> {
        self.element_replay.take()
    }

    fn load_element_state(&self, disk: &ElementStateDisk) -> Result<ExecutionState> {
        let output = hex_decode(&disk.output)?;
        if output.len() > self.max_output_bytes {
            bail!(
                "checkpoint element output exceeds maximum of {} bytes",
                self.max_output_bytes
            );
        }
        let encoded_limit = self.max_output_bytes.saturating_mul(6).saturating_add(16);
        let mut bindings = BTreeMap::new();
        for (name, reference) in &disk.bindings {
            let bytes = load_blob(&self.blobs_directory, reference, encoded_limit)
                .with_context(|| format!("loading checkpoint binding {name:?}"))?;
            let value: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding checkpoint binding {name:?}"))?;
            if value_size(&value) > self.max_output_bytes {
                bail!(
                    "checkpoint binding {name:?} exceeds maximum of {} bytes",
                    self.max_output_bytes
                );
            }
            bindings.insert(name.clone(), value);
        }
        Ok(ExecutionState {
            cwd: disk.cwd.to_path(),
            output,
            bindings,
        })
    }

    fn load_element_outcome(&self, disk: &ElementResultDisk) -> Result<ElementOutcome> {
        let output = hex_decode(&disk.output)?;
        let stderr = hex_decode(&disk.stderr)?;
        if output.len() > self.max_output_bytes || stderr.len() > self.max_output_bytes {
            bail!(
                "checkpoint element output exceeds maximum of {} bytes",
                self.max_output_bytes
            );
        }
        Ok(ElementOutcome {
            output,
            stderr,
            status: disk.status,
            outcome: Outcome {
                status: disk.outcome.status,
                name: disk.outcome.name.clone(),
                kind: disk.outcome.kind,
                effect_certainty: disk.outcome.effect_certainty,
            },
            value: disk.value.clone(),
            uncertain: disk.uncertain,
            launched_processes: disk.launched_processes,
        })
    }

    /// Write one element line and sync it before returning. Used for begin records,
    /// which must be durable before their element starts.
    fn append_element(&mut self, entry: &ElementLine) -> Result<()> {
        self.write_element_line(entry, true)
    }

    /// Write one element line without an immediate sync. Commits use this: they are
    /// group-synced, and a lost commit only makes its element run again.
    fn append_element_buffered(&mut self, entry: &ElementLine) -> Result<()> {
        self.write_element_line(entry, false)
    }

    fn write_element_line(&mut self, entry: &ElementLine, durable: bool) -> Result<()> {
        let line = serde_json::to_vec(entry).context("encoding checkpoint element")?;
        if let Err(error) = self.write_element_bytes(&line, durable) {
            // The element may have run; keep the durable in-flight record for resume.
            self.persistence_failed = true;
            return Err(error);
        }
        Ok(())
    }

    fn write_element_bytes(&mut self, line: &[u8], durable: bool) -> Result<()> {
        if self.elements_file.is_none() {
            let path = self.directory.join(ELEMENT_LOG);
            let mut file = open_regular(&path, "checkpoint element log", true)?;
            restrict_file(&file)?;
            // Bytes past the verified prefix, left by an interrupted append, go first.
            file.set_len(self.elements_bytes)
                .context("truncating checkpoint element log")?;
            file.seek(SeekFrom::Start(self.elements_bytes))
                .context("seeking checkpoint element log")?;
            self.elements_file = Some(file);
        }
        let file = self
            .elements_file
            .as_mut()
            .context("checkpoint element log is not open")?;
        let mut record = Vec::with_capacity(line.len() + 1);
        record.extend_from_slice(line);
        record.push(b'\n');
        file.write_all(&record)
            .context("appending checkpoint element")?;
        self.elements_chain = chain_step(&self.elements_chain, line);
        self.elements_bytes += record.len() as u64;
        self.elements_lines += 1;
        if durable || self.elements_synced_at.elapsed() >= ELEMENT_SYNC_INTERVAL {
            self.sync_open_element_file()?;
        } else {
            self.elements_dirty = true;
        }
        Ok(())
    }

    fn sync_open_element_file(&mut self) -> Result<()> {
        if let Some(file) = self.elements_file.as_ref() {
            file.sync_data().context("syncing checkpoint element log")?;
        }
        self.elements_dirty = false;
        self.elements_synced_at = std::time::Instant::now();
        Ok(())
    }

    /// Make every buffered element line durable. Called before the journal is
    /// written, so the anchor never names lines that could still be lost.
    pub(super) fn sync_elements(&mut self) -> Result<()> {
        if self.elements_dirty {
            self.sync_open_element_file()?;
        }
        Ok(())
    }

    /// Binding references for `bindings`. Unchanged bindings reuse the previous
    /// references, so a steady loop writes no blobs. Changed values are stored, and
    /// synced by the blob write, before the commit that names them.
    fn element_state_refs(
        &mut self,
        cwd: &Path,
        bindings: &BTreeMap<String, Value>,
    ) -> Result<BTreeMap<String, BlobRef>> {
        if let Some(cache) = self.element_state_cache.as_ref() {
            if cache.cwd == cwd && cache.bindings == *bindings {
                return Ok(cache.refs.clone());
            }
        }
        let mut refs = BTreeMap::new();
        for (name, value) in bindings {
            if value_size(value) > self.max_output_bytes {
                bail!(
                    "checkpoint binding {name:?} exceeds maximum of {} bytes",
                    self.max_output_bytes
                );
            }
            let bytes = serde_json::to_vec(value)
                .with_context(|| format!("encoding checkpoint binding {name:?}"))?;
            let encoded_limit = self.max_output_bytes.saturating_mul(6).saturating_add(16);
            refs.insert(
                name.clone(),
                store_blob(&self.blobs_directory, &bytes, encoded_limit)?,
            );
        }
        self.element_state_cache = Some(ElementStateCache {
            cwd: cwd.to_path_buf(),
            bindings: bindings.clone(),
            refs: refs.clone(),
        });
        Ok(refs)
    }

    /// Commit a sequential element: the state after it. The output is inline.
    fn commit_state(
        &mut self,
        index: usize,
        fingerprint: String,
        cwd: &Path,
        output: &[u8],
        bindings: &BTreeMap<String, Value>,
    ) -> Result<()> {
        if output.len() > self.max_output_bytes {
            self.persistence_failed = true;
            bail!(
                "checkpoint element output exceeds maximum of {} bytes",
                self.max_output_bytes
            );
        }
        let bindings = match self.element_state_refs(cwd, bindings) {
            Ok(bindings) => bindings,
            Err(error) => {
                self.persistence_failed = true;
                return Err(error);
            }
        };
        self.append_element_buffered(&ElementLine::Commit {
            index,
            fingerprint,
            state: Some(ElementStateDisk {
                cwd: EncodedPath::from_path(cwd),
                output: hex_encode(output),
                bindings,
            }),
            result: None,
        })
    }

    /// Commit a parallel element: its recorded result, output and stderr inline.
    fn commit_outcome(
        &mut self,
        index: usize,
        fingerprint: String,
        outcome: &ElementOutcome,
    ) -> Result<()> {
        if outcome.output.len() > self.max_output_bytes
            || outcome.stderr.len() > self.max_output_bytes
        {
            self.persistence_failed = true;
            bail!(
                "checkpoint element output exceeds maximum of {} bytes",
                self.max_output_bytes
            );
        }
        self.append_element_buffered(&ElementLine::Commit {
            index,
            fingerprint,
            state: None,
            result: Some(Box::new(ElementResultDisk {
                status: outcome.status,
                outcome: OutcomeDisk {
                    status: outcome.outcome.status,
                    name: outcome.outcome.name.clone(),
                    kind: outcome.outcome.kind,
                    effect_certainty: outcome.outcome.effect_certainty,
                },
                value: outcome.value.clone(),
                uncertain: outcome.uncertain,
                launched_processes: outcome.launched_processes,
                output: hex_encode(&outcome.output),
                stderr: hex_encode(&outcome.stderr),
            })),
        })
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        // A normal exit makes the buffered element commits durable.
        let _ = self.sync_elements();
    }
}

/// Fuzz entry: replay `bytes` as the element log of an in-flight node, in a fresh
/// `scratch` directory. Only the log's own reader runs, so a bad line must fail there.
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_replay_element_log(scratch: &Path, bytes: &[u8]) -> Result<()> {
    let options = CheckpointOptions {
        directory: scratch.join("checkpoint"),
        program_path: scratch.join("flow.txt"),
        resume: false,
        retry_uncertain: false,
    };
    fs::write(&options.program_path, "workflow")?;
    let cwd = scratch.join("work");
    fs::create_dir_all(&cwd)?;
    let mut journal = Journal::open_with_inputs(&options, &cwd, "finite", 1024, None)?;
    journal.begin_node_record(NodeBegin {
        fingerprint: "fuzz-node",
        start: 0,
        end: 2,
        line: 1,
        cwd_before: &cwd,
        retry_safe: true,
        step_id: None,
    })?;
    fs::write(options.directory.join(ELEMENT_LOG), bytes)?;
    journal.load_elements()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn options(directory: &Path) -> CheckpointOptions {
        let program_path = directory.join("flow.txt");
        fs::write(&program_path, "workflow").unwrap();
        CheckpointOptions {
            directory: directory.join("checkpoint"),
            program_path,
            resume: false,
            retry_uncertain: false,
        }
    }

    fn open(options: &CheckpointOptions, cwd: &Path) -> Journal {
        Journal::open_with_inputs(options, cwd, "finite", 1024, None).unwrap()
    }

    fn start_node(journal: &mut Journal, cwd: &Path, fingerprint: &str) {
        journal.begin_node(fingerprint, 0, 2, 1, cwd, true).unwrap();
    }

    #[test]
    fn element_commits_replay_in_order_with_their_state() {
        let temp = tempdir().unwrap();
        let options = options(temp.path());
        let cwd = temp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let mut journal = open(&options, &cwd);
        start_node(&mut journal, &cwd, "loop-node");
        let first = element_fingerprint("loop-node", 0, &Value::String("a".into())).unwrap();
        journal
            .append_element(&ElementLine::Begin {
                index: 0,
                fingerprint: first.clone(),
                uncertain: false,
            })
            .unwrap();
        journal
            .commit_state(0, first, &cwd, b"one", &BTreeMap::new())
            .unwrap();
        let second = element_fingerprint("loop-node", 1, &Value::String("b".into())).unwrap();
        journal
            .append_element(&ElementLine::Begin {
                index: 1,
                fingerprint: second.clone(),
                uncertain: true,
            })
            .unwrap();
        drop(journal);

        let mut reopened = open(&resuming(&options), &cwd);
        let replay = reopened.take_element_replay().unwrap();
        assert_eq!(replay.commits.len(), 1);
        assert_eq!(replay.commits[0].index, 0);
        assert_eq!(replay.commits[0].state.as_ref().unwrap().output, b"one");
        assert_eq!(replay.pending.len(), 1);
        assert_eq!(replay.pending[0].index, 1);
        assert!(replay.pending[0].uncertain);
    }

    #[test]
    fn torn_final_element_line_is_not_committed_and_is_truncated_on_append() {
        let temp = tempdir().unwrap();
        let options = options(temp.path());
        let cwd = temp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let mut journal = open(&options, &cwd);
        start_node(&mut journal, &cwd, "loop-node");
        journal
            .append_element(&ElementLine::Begin {
                index: 0,
                fingerprint: "f0".into(),
                uncertain: false,
            })
            .unwrap();
        drop(journal);
        let log = options.directory.join(ELEMENT_LOG);
        let mut bytes = fs::read(&log).unwrap();
        bytes.extend_from_slice(b"{\"kind\":\"commit\",\"index\":0");
        fs::write(&log, bytes).unwrap();

        let mut reopened = open(&resuming(&options), &cwd);
        let replay = reopened.take_element_replay().unwrap();
        assert!(replay.commits.is_empty());
        assert_eq!(replay.pending.len(), 1);
        reopened
            .append_element(&ElementLine::Begin {
                index: 0,
                fingerprint: "f0".into(),
                uncertain: false,
            })
            .unwrap();
        drop(reopened);
        let text = fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text
            .lines()
            .all(|line| line.starts_with("{\"kind\":\"begin\"")));
    }

    #[test]
    fn uncertain_pending_element_requires_explicit_retry() {
        let temp = tempdir().unwrap();
        let options = options(temp.path());
        let cwd = temp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let mut journal = open(&options, &cwd);
        start_node(&mut journal, &cwd, "loop-node");
        journal
            .append_element(&ElementLine::Begin {
                index: 0,
                fingerprint: "f0".into(),
                uncertain: true,
            })
            .unwrap();
        drop(journal);

        let reopened = open(&resuming(&options), &cwd);
        assert!(check_pending(
            &reopened,
            &[PendingElement {
                index: 0,
                uncertain: true
            }]
        )
        .is_err());
        drop(reopened);
        let mut retry = options.clone();
        retry.resume = true;
        retry.retry_uncertain = true;
        let allowed = Journal::open_with_inputs(&retry, &cwd, "finite", 1024, None).unwrap();
        assert!(check_pending(
            &allowed,
            &[PendingElement {
                index: 0,
                uncertain: true
            }]
        )
        .is_ok());
    }

    /// Two element lines are covered by a journal write; a third is only covered by
    /// the chain. Returns the element log path after the write.
    fn anchored_log(options: &CheckpointOptions, cwd: &Path) -> PathBuf {
        let mut journal = open(options, cwd);
        start_node(&mut journal, cwd, "loop-node");
        journal
            .append_element(&ElementLine::Begin {
                index: 0,
                fingerprint: "f0".into(),
                uncertain: false,
            })
            .unwrap();
        journal
            .append_element(&ElementLine::Commit {
                index: 0,
                fingerprint: "f0".into(),
                state: None,
                result: None,
            })
            .unwrap();
        journal.persist().unwrap();
        journal
            .append_element(&ElementLine::Begin {
                index: 1,
                fingerprint: "f1".into(),
                uncertain: false,
            })
            .unwrap();
        drop(journal);
        options.directory.join(ELEMENT_LOG)
    }

    fn resuming(options: &CheckpointOptions) -> CheckpointOptions {
        let mut resume = options.clone();
        resume.resume = true;
        resume
    }

    #[test]
    fn a_log_shorter_than_its_anchor_is_refused_as_truncated() {
        let temp = tempdir().unwrap();
        let options = options(temp.path());
        let cwd = temp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let log = anchored_log(&options, &cwd);
        let text = fs::read_to_string(&log).unwrap();
        let first = text.lines().next().unwrap();
        fs::write(&log, format!("{first}\n")).unwrap();

        let error = Journal::open_with_inputs(&resuming(&options), &cwd, "finite", 1024, None)
            .err()
            .expect("a log below its anchor must be refused");
        assert!(
            error
                .to_string()
                .contains("shorter than its journal anchor"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_rewritten_anchored_prefix_is_refused_by_its_chain() {
        let temp = tempdir().unwrap();
        let options = options(temp.path());
        let cwd = temp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let log = anchored_log(&options, &cwd);
        let text = fs::read_to_string(&log).unwrap();
        fs::write(&log, text.replacen("\"f0\"", "\"fx\"", 1)).unwrap();

        let error = Journal::open_with_inputs(&resuming(&options), &cwd, "finite", 1024, None)
            .err()
            .expect("a rewritten prefix must be refused");
        assert!(
            error
                .to_string()
                .contains("does not match its journal anchor"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn lines_after_the_anchor_replay_when_the_chain_holds() {
        let temp = tempdir().unwrap();
        let options = options(temp.path());
        let cwd = temp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        anchored_log(&options, &cwd);
        let mut reopened =
            Journal::open_with_inputs(&resuming(&options), &cwd, "finite", 1024, None).unwrap();
        let replay = reopened.take_element_replay().unwrap();
        assert_eq!(replay.commits.len(), 1);
        assert_eq!(replay.pending.len(), 1);
        assert_eq!(replay.pending[0].index, 1);
    }

    #[test]
    fn a_new_node_starts_with_an_empty_element_log() {
        let temp = tempdir().unwrap();
        let options = options(temp.path());
        let cwd = temp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let mut journal = open(&options, &cwd);
        start_node(&mut journal, &cwd, "loop-node");
        journal
            .append_element(&ElementLine::Begin {
                index: 0,
                fingerprint: "f0".into(),
                uncertain: false,
            })
            .unwrap();
        journal
            .complete_node_parts(&cwd, b"", std::iter::empty::<(&str, &Value)>())
            .unwrap();
        journal
            .begin_node("next-node", 2, 3, 2, &cwd, true)
            .unwrap();
        drop(journal);
        let text = fs::read_to_string(options.directory.join(ELEMENT_LOG)).unwrap_or_default();
        assert!(
            text.is_empty(),
            "stale element records survived a new node: {text}"
        );
    }
}
