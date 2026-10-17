//! The graph journal (mode `graph`, journal version 5). It keeps the completed nodes
//! of a finite applet graph and the set of nodes in flight.
//!
//! `journal.json` names the committed prefix of `graph.jsonl` by its line count and
//! chain digest, and lists the in-flight nodes. A node enters that list before its
//! first applet invocation, and leaves it only when its record is committed. A record
//! line past the committed prefix is ignored on open, and the next append truncates it.

use super::{
    atomic_write, encode_envelope, hex_digest, lock_directory, open_regular, real_directory,
    restrict_directory, restrict_file, serialized_digest, sync_directory, JournalEnvelope,
    JOURNAL_VERSION,
};
use crate::graph::{CompletedNode, GraphCheckpointState, GraphCheckpointStore, InterruptedNode};
use crate::security::SensitivitySet;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// The `kind` every graph journal body carries. Program readers refuse it.
const GRAPH_KIND: &str = "graph";
const GRAPH_LOG: &str = "graph.jsonl";
const GRAPH_CHAIN_START: &str = "execute-graph-completed-log/v5";
const MAX_JOURNAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// Checks that a completed node's payloads may be written. It receives the union of
/// their sensitivity labels and applies the same persistence policy as a program
/// checkpoint. Payloads are written only after it accepts.
pub(crate) type PersistenceAuthorizer = Arc<dyn Fn(&SensitivitySet) -> Result<()> + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphJournalDisk {
    version: u32,
    kind: String,
    identity: String,
    completed_count: usize,
    completed_chain: String,
    in_flight: Vec<InterruptedNode>,
}

/// Whether a journal body belongs to a graph run.
pub(super) fn is_graph_body(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("kind")
                .and_then(|kind| kind.as_str())
                .map(|kind| kind == GRAPH_KIND)
        })
        .unwrap_or(false)
}

struct GraphState {
    identity: String,
    completed: Vec<CompletedNode>,
    /// Chain digest over the committed lines of `graph.jsonl`.
    chain: String,
    /// Bytes of `graph.jsonl` covered by the chain.
    bytes: u64,
    in_flight: Vec<InterruptedNode>,
}

/// An open graph journal. It holds the directory lock for its whole life.
pub(crate) struct GraphJournal {
    directory: PathBuf,
    authorize: PersistenceAuthorizer,
    state: Mutex<GraphState>,
    _lock: File,
}

/// Open the graph journal in `directory`. `identity` names the graph, its inputs and
/// its working directory, and a resume must match it. With `resume` false the
/// journal must not exist yet, and a fresh one is created.
pub(crate) fn open_graph_journal(
    directory: &Path,
    resume: bool,
    identity: &str,
    authorize: PersistenceAuthorizer,
) -> Result<GraphJournal> {
    let requested: PathBuf = directory.components().collect();
    let existed = fs::symlink_metadata(&requested).is_ok();
    real_directory(&requested, "checkpoint", !resume)?;
    let directory = fs::canonicalize(&requested)
        .with_context(|| format!("resolving checkpoint directory {}", requested.display()))?;
    restrict_directory(&directory)?;
    if !resume && !existed {
        if let Some(parent) = directory.parent() {
            sync_directory(parent)?;
        }
    }
    let lock = lock_directory(&directory)?;
    let journal_path = directory.join("journal.json");
    if resume {
        let disk = read_disk(&journal_path)?;
        if disk.identity != identity {
            bail!("graph checkpoint belongs to a different graph, directory, or inputs");
        }
        let (completed, log) = read_log(&directory, disk.completed_count, &disk.completed_chain)?;
        return Ok(GraphJournal {
            directory,
            authorize,
            state: Mutex::new(GraphState {
                identity: identity.to_owned(),
                completed,
                chain: log.chain,
                bytes: log.bytes,
                in_flight: disk.in_flight,
            }),
            _lock: lock,
        });
    }
    if fs::symlink_metadata(&journal_path).is_ok() {
        bail!("checkpoint already exists; use resume explicitly");
    }
    let journal = GraphJournal {
        directory,
        authorize,
        state: Mutex::new(GraphState {
            identity: identity.to_owned(),
            completed: Vec::new(),
            chain: chain_start(),
            bytes: 0,
            in_flight: Vec::new(),
        }),
        _lock: lock,
    };
    {
        let state = journal.locked();
        write_disk(
            &journal.directory,
            &journal.disk(&state, state.in_flight.clone()),
        )?;
    }
    Ok(journal)
}

/// Open a graph checkpoint as the store that `run_checkpointed` records into.
pub(crate) fn open_graph_checkpoint(
    directory: &Path,
    resume: bool,
    identity: &str,
    authorize: PersistenceAuthorizer,
) -> Result<Arc<dyn GraphCheckpointStore>> {
    Ok(Arc::new(open_graph_journal(
        directory, resume, identity, authorize,
    )?))
}

impl GraphJournal {
    fn locked(&self) -> MutexGuard<'_, GraphState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn disk(&self, state: &GraphState, in_flight: Vec<InterruptedNode>) -> GraphJournalDisk {
        GraphJournalDisk {
            version: JOURNAL_VERSION,
            kind: GRAPH_KIND.to_owned(),
            identity: state.identity.clone(),
            completed_count: state.completed.len(),
            completed_chain: state.chain.clone(),
            in_flight,
        }
    }
}

impl GraphCheckpointStore for GraphJournal {
    fn load(&self) -> Result<GraphCheckpointState> {
        let state = self.locked();
        Ok(GraphCheckpointState {
            completed: state.completed.clone(),
            interrupted: state.in_flight.clone(),
        })
    }

    fn begin(&self, node: &str, fingerprint: &str, retry_safe: bool) -> Result<()> {
        let mut state = self.locked();
        let mut in_flight: Vec<InterruptedNode> = state
            .in_flight
            .iter()
            .filter(|entry| entry.node != node)
            .cloned()
            .collect();
        in_flight.push(InterruptedNode {
            node: node.to_owned(),
            fingerprint: fingerprint.to_owned(),
            retry_safe,
        });
        // Durable before the node's first effect, so an interruption after this point
        // leaves the node in flight.
        write_disk(&self.directory, &self.disk(&state, in_flight.clone()))?;
        state.in_flight = in_flight;
        Ok(())
    }

    fn complete(&self, record: CompletedNode) -> Result<()> {
        let floor = SensitivitySet::from_labels(
            record
                .outputs
                .iter()
                .flat_map(|output| output.sensitivity.iter()),
        );
        (self.authorize)(&floor)?;
        let line = serde_json::to_vec(&record).context("encoding graph completed node")?;
        if line.len() > MAX_RECORD_BYTES {
            bail!(
                "completed graph node {:?} exceeds the 16 MiB record limit",
                record.node
            );
        }
        let mut state = self.locked();
        let chain = chain_step(&state.chain, &line);
        let bytes = state.bytes + line.len() as u64 + 1;
        // The line is durable before the journal names it, and a crash in between leaves
        // the node in flight, so it is rerun.
        append_line(&self.directory, state.bytes, &line)?;
        let in_flight: Vec<InterruptedNode> = state
            .in_flight
            .iter()
            .filter(|entry| entry.node != record.node)
            .cloned()
            .collect();
        let disk = GraphJournalDisk {
            version: JOURNAL_VERSION,
            kind: GRAPH_KIND.to_owned(),
            identity: state.identity.clone(),
            completed_count: state.completed.len() + 1,
            completed_chain: chain.clone(),
            in_flight: in_flight.clone(),
        };
        write_disk(&self.directory, &disk)?;
        state.completed.push(record);
        state.chain = chain;
        state.bytes = bytes;
        state.in_flight = in_flight;
        Ok(())
    }
}

/// Read and verify the journal file: its checksum, its kind, and its version.
fn read_disk(path: &Path) -> Result<GraphJournalDisk> {
    let file = open_regular(path, "checkpoint journal", false)
        .context("graph checkpoint journal is missing")?;
    restrict_file(&file)?;
    let mut bytes = Vec::new();
    file.take(MAX_JOURNAL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_JOURNAL_BYTES {
        bail!("checkpoint journal is too large");
    }
    let envelope: JournalEnvelope =
        serde_json::from_slice(&bytes).context("checkpoint journal is malformed or corrupt")?;
    let body = envelope.data.get();
    if !is_graph_body(body) {
        bail!("this checkpoint holds a program run, not a graph run");
    }
    let disk: GraphJournalDisk =
        serde_json::from_str(body).context("checkpoint journal is malformed or corrupt")?;
    if disk.version != JOURNAL_VERSION {
        bail!("unsupported graph checkpoint version {}", disk.version);
    }
    if serialized_digest(&disk)? != envelope.checksum {
        bail!("checkpoint journal checksum mismatch");
    }
    Ok(disk)
}

/// Write the journal atomically, with a checksum over its body.
fn write_disk(directory: &Path, disk: &GraphJournalDisk) -> Result<()> {
    let data = serde_json::to_vec(disk).context("encoding graph checkpoint journal")?;
    let bytes = encode_envelope(&hex_digest(&data), &data);
    if bytes.len() > MAX_JOURNAL_BYTES {
        bail!("graph checkpoint journal exceeds 16 MiB");
    }
    atomic_write(directory, "journal", "json", &bytes)
        .context("persisting graph checkpoint journal")
}

struct GraphLog {
    chain: String,
    bytes: u64,
}

fn chain_start() -> String {
    hex_digest(GRAPH_CHAIN_START.as_bytes())
}

/// Each line extends the chain with its own bytes.
fn chain_step(previous: &str, line: &[u8]) -> String {
    hex_digest(format!("{previous}\n{}", hex_digest(line)).as_bytes())
}

/// Read the committed prefix of `graph.jsonl`. Lines past `count` are uncommitted and
/// ignored. The prefix must match the journal's chain exactly.
fn read_log(
    directory: &Path,
    count: usize,
    expected_chain: &str,
) -> Result<(Vec<CompletedNode>, GraphLog)> {
    if count == 0 {
        if expected_chain != chain_start() {
            bail!("graph completed log checksum mismatch");
        }
        return Ok((
            Vec::new(),
            GraphLog {
                chain: chain_start(),
                bytes: 0,
            },
        ));
    }
    let file = open_regular(&directory.join(GRAPH_LOG), "graph completed log", false)
        .context("graph completed log is missing")?;
    let mut reader = io::BufReader::new(file);
    let mut nodes = Vec::with_capacity(count.min(4096));
    let mut chain = chain_start();
    let mut bytes = 0u64;
    let mut line = Vec::new();
    for _ in 0..count {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
            bail!("graph completed log is shorter than its journal records");
        }
        line.pop();
        chain = chain_step(&chain, &line);
        let node: CompletedNode =
            serde_json::from_slice(&line).context("graph completed log is malformed or corrupt")?;
        nodes.push(node);
        bytes += line.len() as u64 + 1;
    }
    if chain != expected_chain {
        bail!("graph completed log checksum mismatch");
    }
    Ok((nodes, GraphLog { chain, bytes }))
}

/// Append one committed line, first dropping any uncommitted tail beyond the chain.
fn append_line(directory: &Path, committed_bytes: u64, line: &[u8]) -> Result<()> {
    #[cfg(feature = "failpoints")]
    crate::failpoint::hit("checkpoint.append")?;
    let mut file = open_regular(&directory.join(GRAPH_LOG), "graph completed log", true)?;
    restrict_file(&file)?;
    file.set_len(committed_bytes)
        .context("truncating graph completed log")?;
    file.seek(SeekFrom::Start(committed_bytes))
        .context("seeking graph completed log")?;
    file.write_all(line)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_data())
        .context("appending graph completed node")
}
