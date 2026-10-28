//! Typed applet graphs with bounded, revisioned event transport.
//!
//! Graph contracts reuse the canonical [`crate::contracts::ValueSchema`].
//! A graph edge never changes a payload's representation: callers must declare
//! a matching schema or insert an applet that performs an explicit conversion.

mod checkpoint;
mod plan;
mod syntax;
mod transport;

pub use crate::contracts::{OutputType, PortDelivery, ValueSchema};
pub use checkpoint::{
    checkpoint_identity, open_checkpoint_directory, CompletedNode, GraphCheckpointState,
    GraphCheckpointStore, InterruptedNode, MemoryGraphStore, RecordedOutput,
};
pub use plan::{
    BufferPolicy, EdgeSpec, GraphDiagnostic, GraphNode, GraphNodeKind, GraphPlan, PortDirection,
    PortRef, PortSchema, PortValue,
};
pub use syntax::{is_graph, parse};
pub use transport::{
    graph_event_line, graph_event_line_json, graph_event_payload_json, run, run_checkpointed,
    start_session, stream_event_byte_limit, ChannelStdinSource, EventEnvelope, GraphContext,
    GraphExecutor, GraphRunOptions, GraphRunResult, GraphSession, GraphStdinSource, NodeIo,
    PortEmitReport, RemoteEventMetadata, RuntimeGraphExecutor, MAX_GRAPH_LISTENER_READERS,
    MAX_LISTENER_PORTS_PER_SOURCE,
};

use crate::ast::Invocation;

/// Every applet invocation in the plan, in source order. CLI policy preflight
/// uses this before starting any graph workers or external effects.
impl GraphPlan {
    pub fn invocations(&self) -> impl Iterator<Item = &Invocation> {
        self.nodes().iter().flat_map(GraphNode::invocations)
    }
}
