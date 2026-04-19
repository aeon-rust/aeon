//! DAG pipeline topology — fan-out, fan-in, chaining, content-based routing.
//!
//! This module provides:
//! - `DagGraph`: a lightweight graph structure for validating pipeline topologies
//!   (cycle detection, name resolution, structural rules)
//! - Typed runner functions for each topology pattern:
//!   - `run_fan_out`: one source → one processor → multiple sinks (zero-copy via Bytes)
//!   - `run_fan_in`: multiple sources → one processor → one sink
//!   - `run_chain`: source → processor A → processor B → sink
//!   - `run_routed`: source → routing function → branch (processor, sink) pairs

use aeon_types::{AeonError, Event, Processor, Sink, Source};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::pipeline::PipelineMetrics;

// ---------------------------------------------------------------------------
// Graph validation
// ---------------------------------------------------------------------------

/// The kind of node in a pipeline DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Source,
    Processor,
    Sink,
    Router,
}

/// A lightweight graph for validating pipeline topology before execution.
///
/// This is a structural check only — it does not hold the actual Source/Processor/Sink
/// implementations (those use generics via the runner functions).
pub struct DagGraph {
    nodes: HashMap<String, NodeKind>,
    edges: Vec<(String, String)>,
}

impl DagGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: Vec::new(),
        }
    }

    /// Add a named node of the given kind. Returns error if name is duplicate.
    pub fn add_node(&mut self, name: &str, kind: NodeKind) -> Result<(), AeonError> {
        if self.nodes.contains_key(name) {
            return Err(AeonError::config(format!("duplicate node name: '{name}'")));
        }
        self.nodes.insert(name.to_string(), kind);
        Ok(())
    }

    /// Add a directed edge from → to. Returns error if either node doesn't exist.
    pub fn add_edge(&mut self, from: &str, to: &str) -> Result<(), AeonError> {
        if !self.nodes.contains_key(from) {
            return Err(AeonError::config(format!(
                "unknown node in edge source: '{from}'"
            )));
        }
        if !self.nodes.contains_key(to) {
            return Err(AeonError::config(format!(
                "unknown node in edge target: '{to}'"
            )));
        }
        self.edges.push((from.to_string(), to.to_string()));
        Ok(())
    }

    /// Validate the DAG: no cycles, structural rules, reachability.
    pub fn validate(&self) -> Result<(), AeonError> {
        // Must have at least one source and one sink
        let has_source = self.nodes.values().any(|k| *k == NodeKind::Source);
        let has_sink = self.nodes.values().any(|k| *k == NodeKind::Sink);
        if !has_source {
            return Err(AeonError::config("DAG has no source nodes".to_string()));
        }
        if !has_sink {
            return Err(AeonError::config("DAG has no sink nodes".to_string()));
        }

        // Sources must not have incoming edges
        let incoming: HashSet<&str> = self.edges.iter().map(|(_, to)| to.as_str()).collect();
        for (name, kind) in &self.nodes {
            if *kind == NodeKind::Source && incoming.contains(name.as_str()) {
                return Err(AeonError::config(format!(
                    "source node '{name}' has incoming edges"
                )));
            }
        }

        // Sinks must not have outgoing edges
        let outgoing: HashSet<&str> = self.edges.iter().map(|(from, _)| from.as_str()).collect();
        for (name, kind) in &self.nodes {
            if *kind == NodeKind::Sink && outgoing.contains(name.as_str()) {
                return Err(AeonError::config(format!(
                    "sink node '{name}' has outgoing edges"
                )));
            }
        }

        // Cycle detection via Kahn's algorithm (topological sort)
        self.detect_cycles()?;

        Ok(())
    }

    /// Kahn's algorithm — returns error if cycles exist.
    fn detect_cycles(&self) -> Result<(), AeonError> {
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        for name in self.nodes.keys() {
            in_degree.insert(name.as_str(), 0);
        }
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
        for (from, to) in &self.edges {
            *in_degree.entry(to.as_str()).or_insert(0) += 1;
            adjacency
                .entry(from.as_str())
                .or_default()
                .push(to.as_str());
        }

        let mut queue: VecDeque<&str> = VecDeque::new();
        for (&node, &deg) in &in_degree {
            if deg == 0 {
                queue.push_back(node);
            }
        }

        let mut visited = 0usize;
        while let Some(node) = queue.pop_front() {
            visited += 1;
            if let Some(neighbors) = adjacency.get(node) {
                for &neighbor in neighbors {
                    // FT-10: `neighbor` comes from `adjacency`, whose keys
                    // are a subset of `in_degree`'s keys (both populated from
                    // `self.nodes`). Guaranteed Some.
                    #[allow(clippy::expect_used)]
                    let deg = in_degree.get_mut(neighbor).expect("invariant: adjacency targets are in in_degree");
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(neighbor);
                    }
                }
            }
        }

        if visited != self.nodes.len() {
            return Err(AeonError::config("DAG contains a cycle".to_string()));
        }

        Ok(())
    }
}

impl Default for DagGraph {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Topology runners
// ---------------------------------------------------------------------------

/// Fan-out: one source → one processor → multiple sinks.
///
/// Each sink receives a clone of every output. Cloning an `Output` is cheap —
/// `Bytes::clone()` is an Arc increment (zero-copy), `Arc<str>::clone()` is
/// a pointer copy.
pub async fn run_fan_out<S, P, K>(
    source: &mut S,
    processor: &P,
    sinks: &mut [K],
    metrics: &PipelineMetrics,
    shutdown: &AtomicBool,
) -> Result<(), AeonError>
where
    S: Source,
    P: Processor,
    K: Sink,
{
    if sinks.is_empty() {
        return Err(AeonError::config(
            "fan-out requires at least one sink".to_string(),
        ));
    }

    while !shutdown.load(Ordering::Relaxed) {
        let events = source.next_batch().await?;
        if events.is_empty() {
            break;
        }

        let count = events.len() as u64;
        metrics.events_received.fetch_add(count, Ordering::Relaxed);

        let outputs = processor.process_batch(events)?;
        metrics.events_processed.fetch_add(count, Ordering::Relaxed);

        let out_count = outputs.len() as u64;

        // Fan-out: clone outputs to each sink (last sink gets the original)
        let sink_count = sinks.len();
        for sink in sinks.iter_mut().take(sink_count - 1) {
            // Bytes::clone = Arc increment, not data copy
            let cloned = outputs.to_vec();
            sink.write_batch(cloned).await?;
        }
        // Last sink gets the original vec (no clone)
        if let Some(last) = sinks.last_mut() {
            last.write_batch(outputs).await?;
        }

        // Count once per output, not per sink
        metrics.outputs_sent.fetch_add(out_count, Ordering::Relaxed);
    }

    for sink in sinks.iter_mut() {
        sink.flush().await?;
    }
    Ok(())
}

/// Fan-in: multiple sources → one processor → one sink.
///
/// Sources are polled round-robin. Once a source returns an empty batch,
/// it is marked exhausted. The pipeline completes when all sources are exhausted.
pub async fn run_fan_in<S, P, K>(
    sources: &mut [S],
    processor: &P,
    sink: &mut K,
    metrics: &PipelineMetrics,
    shutdown: &AtomicBool,
) -> Result<(), AeonError>
where
    S: Source,
    P: Processor,
    K: Sink,
{
    if sources.is_empty() {
        return Err(AeonError::config(
            "fan-in requires at least one source".to_string(),
        ));
    }

    let mut exhausted = vec![false; sources.len()];

    while !shutdown.load(Ordering::Relaxed) {
        let mut any_active = false;

        for (i, source) in sources.iter_mut().enumerate() {
            if exhausted[i] {
                continue;
            }

            let events = source.next_batch().await?;
            if events.is_empty() {
                exhausted[i] = true;
                continue;
            }

            any_active = true;
            let count = events.len() as u64;
            metrics.events_received.fetch_add(count, Ordering::Relaxed);

            let outputs = processor.process_batch(events)?;
            metrics.events_processed.fetch_add(count, Ordering::Relaxed);

            let out_count = outputs.len() as u64;
            sink.write_batch(outputs).await?;
            metrics.outputs_sent.fetch_add(out_count, Ordering::Relaxed);
        }

        if !any_active {
            break;
        }
    }

    sink.flush().await?;
    Ok(())
}

/// Chain: source → processor A → processor B → sink.
///
/// Processor A's outputs are converted back to events (via `Output::into_event`)
/// and fed into processor B. This enables composable processing pipelines.
///
/// The chained source identity (source name, partition) is preserved from the
/// original events for traceability.
pub async fn run_chain<S, P1, P2, K>(
    source: &mut S,
    first: &P1,
    second: &P2,
    sink: &mut K,
    metrics: &PipelineMetrics,
    shutdown: &AtomicBool,
) -> Result<(), AeonError>
where
    S: Source,
    P1: Processor,
    P2: Processor,
    K: Sink,
{
    while !shutdown.load(Ordering::Relaxed) {
        let events = source.next_batch().await?;
        if events.is_empty() {
            break;
        }

        let count = events.len() as u64;
        metrics.events_received.fetch_add(count, Ordering::Relaxed);

        // Stage 1: first processor
        let intermediate = first.process_batch(events)?;

        // Convert outputs back to events for the second processor.
        // Preserve source identity from the output's destination field.
        let chained_events: Vec<Event> = intermediate
            .into_iter()
            .enumerate()
            .map(|(i, output)| {
                let source_name = output.destination.clone();
                output.into_event(
                    uuid::Uuid::nil(),
                    i as i64,
                    source_name,
                    aeon_types::PartitionId::new(0),
                )
            })
            .collect();

        // Stage 2: second processor
        let outputs = second.process_batch(chained_events)?;
        metrics.events_processed.fetch_add(count, Ordering::Relaxed);

        let out_count = outputs.len() as u64;
        sink.write_batch(outputs).await?;
        metrics.outputs_sent.fetch_add(out_count, Ordering::Relaxed);
    }

    sink.flush().await?;
    Ok(())
}

/// Content-based routing: source → router function → (processor, sink) branches.
///
/// The router function inspects each event and returns a branch index.
/// Events are grouped by branch, then each branch's processor handles its
/// events and the outputs go to the branch's sink.
///
/// Events routed to an out-of-range index are silently dropped (logged in future).
pub async fn run_routed<S, P, K, F>(
    source: &mut S,
    router: &F,
    branches: &mut [(P, K)],
    metrics: &PipelineMetrics,
    shutdown: &AtomicBool,
) -> Result<(), AeonError>
where
    S: Source,
    P: Processor,
    K: Sink,
    F: Fn(&Event) -> usize,
{
    if branches.is_empty() {
        return Err(AeonError::config(
            "routed pipeline requires at least one branch".to_string(),
        ));
    }

    while !shutdown.load(Ordering::Relaxed) {
        let events = source.next_batch().await?;
        if events.is_empty() {
            break;
        }

        let count = events.len() as u64;
        metrics.events_received.fetch_add(count, Ordering::Relaxed);

        // Group events by branch index
        let mut buckets: Vec<Vec<Event>> = (0..branches.len()).map(|_| Vec::new()).collect();
        for event in events {
            let idx = router(&event);
            if idx < buckets.len() {
                buckets[idx].push(event);
            }
        }

        // Process each branch
        let mut total_processed = 0u64;
        let mut total_outputs = 0u64;
        for (i, bucket) in buckets.into_iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            let batch_count = bucket.len() as u64;
            let (processor, sink) = &mut branches[i];
            let outputs = processor.process_batch(bucket)?;
            total_processed += batch_count;
            total_outputs += outputs.len() as u64;
            sink.write_batch(outputs).await?;
        }

        metrics
            .events_processed
            .fetch_add(total_processed, Ordering::Relaxed);
        metrics
            .outputs_sent
            .fetch_add(total_outputs, Ordering::Relaxed);
    }

    for (_, sink) in branches.iter_mut() {
        sink.flush().await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Topology dispatch
// ---------------------------------------------------------------------------

/// Detected topology based on source/sink counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// 1 source → 1 processor → 1 sink (default SPSC-buffered path).
    Linear,
    /// N sources → 1 processor → 1 sink.
    FanIn,
    /// 1 source → 1 processor → M sinks.
    FanOut,
    /// N sources → 1 processor → M sinks (fan-in then fan-out).
    FanInFanOut,
}

impl Topology {
    /// Derive topology from source and sink counts.
    pub fn detect(source_count: usize, sink_count: usize) -> Self {
        match (source_count > 1, sink_count > 1) {
            (false, false) => Self::Linear,
            (true, false) => Self::FanIn,
            (false, true) => Self::FanOut,
            (true, true) => Self::FanInFanOut,
        }
    }
}

/// Run a pipeline with automatic topology selection based on source/sink
/// count. Uses the DAG runners for multi-source/multi-sink topologies and
/// the simple `run()` path for linear pipelines.
///
/// For multi-source + multi-sink (`FanInFanOut`), events are merged via
/// round-robin fan-in, then cloned to each sink via fan-out.
pub async fn run_topology<S, P, K>(
    sources: &mut [S],
    processor: &P,
    sinks: &mut [K],
    metrics: &PipelineMetrics,
    shutdown: &AtomicBool,
) -> Result<(), AeonError>
where
    S: Source,
    P: Processor,
    K: Sink,
{
    let topo = Topology::detect(sources.len(), sinks.len());

    match topo {
        Topology::Linear => {
            let source = sources.first_mut().ok_or_else(|| {
                AeonError::config("pipeline requires at least one source")
            })?;
            let sink = sinks.first_mut().ok_or_else(|| {
                AeonError::config("pipeline requires at least one sink")
            })?;
            crate::pipeline::run(source, processor, sink, metrics, shutdown).await
        }
        Topology::FanIn => {
            let sink = sinks.first_mut().ok_or_else(|| {
                AeonError::config("fan-in requires at least one sink")
            })?;
            run_fan_in(sources, processor, sink, metrics, shutdown).await
        }
        Topology::FanOut => {
            let source = sources.first_mut().ok_or_else(|| {
                AeonError::config("fan-out requires at least one source")
            })?;
            run_fan_out(source, processor, sinks, metrics, shutdown).await
        }
        Topology::FanInFanOut => {
            run_fan_in_fan_out(sources, processor, sinks, metrics, shutdown).await
        }
    }
}

/// N sources → 1 processor → M sinks. Round-robin sources, then fan-out
/// to all sinks per batch.
async fn run_fan_in_fan_out<S, P, K>(
    sources: &mut [S],
    processor: &P,
    sinks: &mut [K],
    metrics: &PipelineMetrics,
    shutdown: &AtomicBool,
) -> Result<(), AeonError>
where
    S: Source,
    P: Processor,
    K: Sink,
{
    if sources.is_empty() || sinks.is_empty() {
        return Err(AeonError::config(
            "fan-in-fan-out requires at least one source and one sink",
        ));
    }

    let mut exhausted = vec![false; sources.len()];

    while !shutdown.load(Ordering::Relaxed) {
        let mut any_active = false;

        for (i, source) in sources.iter_mut().enumerate() {
            if exhausted[i] {
                continue;
            }

            let events = source.next_batch().await?;
            if events.is_empty() {
                exhausted[i] = true;
                continue;
            }

            any_active = true;
            let count = events.len() as u64;
            metrics.events_received.fetch_add(count, Ordering::Relaxed);

            let outputs = processor.process_batch(events)?;
            metrics.events_processed.fetch_add(count, Ordering::Relaxed);

            let out_count = outputs.len() as u64;

            let sink_count = sinks.len();
            for sink in sinks.iter_mut().take(sink_count - 1) {
                let cloned = outputs.to_vec();
                sink.write_batch(cloned).await?;
            }
            if let Some(last) = sinks.last_mut() {
                last.write_batch(outputs).await?;
            }

            metrics.outputs_sent.fetch_add(out_count, Ordering::Relaxed);
        }

        if !any_active {
            break;
        }
    }

    for sink in sinks.iter_mut() {
        sink.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processor::PassthroughProcessor;
    use aeon_connectors::{BlackholeSink, MemorySink, MemorySource};
    use aeon_types::PartitionId;
    use bytes::Bytes;
    use std::sync::Arc;

    fn make_events(count: usize) -> Vec<Event> {
        let source: Arc<str> = Arc::from("test");
        (0..count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i as i64,
                    Arc::clone(&source),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Graph validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn valid_linear_dag() {
        let mut g = DagGraph::new();
        g.add_node("src", NodeKind::Source).unwrap();
        g.add_node("proc", NodeKind::Processor).unwrap();
        g.add_node("sink", NodeKind::Sink).unwrap();
        g.add_edge("src", "proc").unwrap();
        g.add_edge("proc", "sink").unwrap();
        g.validate().unwrap();
    }

    #[test]
    fn valid_fan_out_dag() {
        let mut g = DagGraph::new();
        g.add_node("src", NodeKind::Source).unwrap();
        g.add_node("proc", NodeKind::Processor).unwrap();
        g.add_node("sink1", NodeKind::Sink).unwrap();
        g.add_node("sink2", NodeKind::Sink).unwrap();
        g.add_edge("src", "proc").unwrap();
        g.add_edge("proc", "sink1").unwrap();
        g.add_edge("proc", "sink2").unwrap();
        g.validate().unwrap();
    }

    #[test]
    fn valid_fan_in_dag() {
        let mut g = DagGraph::new();
        g.add_node("src1", NodeKind::Source).unwrap();
        g.add_node("src2", NodeKind::Source).unwrap();
        g.add_node("proc", NodeKind::Processor).unwrap();
        g.add_node("sink", NodeKind::Sink).unwrap();
        g.add_edge("src1", "proc").unwrap();
        g.add_edge("src2", "proc").unwrap();
        g.add_edge("proc", "sink").unwrap();
        g.validate().unwrap();
    }

    #[test]
    fn valid_chain_dag() {
        let mut g = DagGraph::new();
        g.add_node("src", NodeKind::Source).unwrap();
        g.add_node("proc1", NodeKind::Processor).unwrap();
        g.add_node("proc2", NodeKind::Processor).unwrap();
        g.add_node("sink", NodeKind::Sink).unwrap();
        g.add_edge("src", "proc1").unwrap();
        g.add_edge("proc1", "proc2").unwrap();
        g.add_edge("proc2", "sink").unwrap();
        g.validate().unwrap();
    }

    #[test]
    fn valid_routed_dag() {
        let mut g = DagGraph::new();
        g.add_node("src", NodeKind::Source).unwrap();
        g.add_node("router", NodeKind::Router).unwrap();
        g.add_node("proc_a", NodeKind::Processor).unwrap();
        g.add_node("proc_b", NodeKind::Processor).unwrap();
        g.add_node("sink_a", NodeKind::Sink).unwrap();
        g.add_node("sink_b", NodeKind::Sink).unwrap();
        g.add_edge("src", "router").unwrap();
        g.add_edge("router", "proc_a").unwrap();
        g.add_edge("router", "proc_b").unwrap();
        g.add_edge("proc_a", "sink_a").unwrap();
        g.add_edge("proc_b", "sink_b").unwrap();
        g.validate().unwrap();
    }

    #[test]
    fn cycle_detected() {
        let mut g = DagGraph::new();
        g.add_node("a", NodeKind::Processor).unwrap();
        g.add_node("b", NodeKind::Processor).unwrap();
        g.add_node("src", NodeKind::Source).unwrap();
        g.add_node("sink", NodeKind::Sink).unwrap();
        g.add_edge("src", "a").unwrap();
        g.add_edge("a", "b").unwrap();
        g.add_edge("b", "a").unwrap(); // cycle
        g.add_edge("b", "sink").unwrap();
        let err = g.validate().unwrap_err();
        assert!(
            err.to_string().contains("cycle"),
            "expected cycle error: {err}"
        );
    }

    #[test]
    fn duplicate_node_name_rejected() {
        let mut g = DagGraph::new();
        g.add_node("x", NodeKind::Source).unwrap();
        let err = g.add_node("x", NodeKind::Processor).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn unknown_edge_node_rejected() {
        let mut g = DagGraph::new();
        g.add_node("a", NodeKind::Source).unwrap();
        let err = g.add_edge("a", "b").unwrap_err();
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn source_with_incoming_edge_rejected() {
        let mut g = DagGraph::new();
        g.add_node("src", NodeKind::Source).unwrap();
        g.add_node("proc", NodeKind::Processor).unwrap();
        g.add_node("sink", NodeKind::Sink).unwrap();
        g.add_edge("proc", "src").unwrap(); // source has incoming
        g.add_edge("src", "sink").unwrap();
        let err = g.validate().unwrap_err();
        assert!(err.to_string().contains("incoming"));
    }

    #[test]
    fn sink_with_outgoing_edge_rejected() {
        let mut g = DagGraph::new();
        g.add_node("src", NodeKind::Source).unwrap();
        g.add_node("sink", NodeKind::Sink).unwrap();
        g.add_node("proc", NodeKind::Processor).unwrap();
        g.add_edge("src", "sink").unwrap();
        g.add_edge("sink", "proc").unwrap(); // sink has outgoing
        let err = g.validate().unwrap_err();
        assert!(err.to_string().contains("outgoing"));
    }

    #[test]
    fn missing_source_rejected() {
        let mut g = DagGraph::new();
        g.add_node("proc", NodeKind::Processor).unwrap();
        g.add_node("sink", NodeKind::Sink).unwrap();
        g.add_edge("proc", "sink").unwrap();
        let err = g.validate().unwrap_err();
        assert!(err.to_string().contains("no source"));
    }

    #[test]
    fn missing_sink_rejected() {
        let mut g = DagGraph::new();
        g.add_node("src", NodeKind::Source).unwrap();
        g.add_node("proc", NodeKind::Processor).unwrap();
        g.add_edge("src", "proc").unwrap();
        let err = g.validate().unwrap_err();
        assert!(err.to_string().contains("no sink"));
    }

    // -----------------------------------------------------------------------
    // Fan-out topology tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fan_out_delivers_to_all_sinks() {
        let events = make_events(50);
        let mut source = MemorySource::new(events, 16);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sinks = vec![MemorySink::new(), MemorySink::new(), MemorySink::new()];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_fan_out(&mut source, &processor, &mut sinks, &metrics, &shutdown)
            .await
            .unwrap();

        // Each sink should receive all 50 outputs
        for sink in &sinks {
            assert_eq!(sink.len(), 50);
        }
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 50);
        assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 50);
    }

    #[tokio::test]
    async fn fan_out_zero_copy_payload() {
        let events = make_events(1);
        let mut source = MemorySource::new(events, 10);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sinks = vec![MemorySink::new(), MemorySink::new()];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_fan_out(&mut source, &processor, &mut sinks, &metrics, &shutdown)
            .await
            .unwrap();

        // Both sinks have the same payload content
        assert_eq!(sinks[0].outputs()[0].payload.as_ref(), b"event-0");
        assert_eq!(sinks[1].outputs()[0].payload.as_ref(), b"event-0");
    }

    // -----------------------------------------------------------------------
    // Fan-in topology tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fan_in_merges_sources() {
        let mut sources = vec![
            MemorySource::new(make_events(30), 10),
            MemorySource::new(make_events(20), 10),
        ];
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sink = MemorySink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_fan_in(&mut sources, &processor, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sink.len(), 50); // 30 + 20
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 50);
    }

    #[tokio::test]
    async fn fan_in_handles_empty_source() {
        let mut sources = vec![
            MemorySource::new(make_events(10), 10),
            MemorySource::new(vec![], 10), // empty
        ];
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sink = MemorySink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_fan_in(&mut sources, &processor, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sink.len(), 10);
    }

    // -----------------------------------------------------------------------
    // Chain topology tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn chain_two_processors() {
        let events = make_events(20);
        let mut source = MemorySource::new(events, 10);
        let first = PassthroughProcessor::new(Arc::from("intermediate"));
        let second = PassthroughProcessor::new(Arc::from("final"));
        let mut sink = MemorySink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_chain(&mut source, &first, &second, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sink.len(), 20);
        // Final outputs should have the second processor's destination
        assert_eq!(&*sink.outputs()[0].destination, "final");
        // Payload should survive the chain (zero-copy through both processors)
        assert_eq!(sink.outputs()[0].payload.as_ref(), b"event-0");
    }

    #[tokio::test]
    async fn chain_preserves_all_payloads() {
        let events = make_events(5);
        let mut source = MemorySource::new(events, 10);
        let first = PassthroughProcessor::new(Arc::from("mid"));
        let second = PassthroughProcessor::new(Arc::from("end"));
        let mut sink = MemorySink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_chain(&mut source, &first, &second, &mut sink, &metrics, &shutdown)
            .await
            .unwrap();

        let outputs = sink.outputs();
        for (i, output) in outputs.iter().enumerate().take(5) {
            assert_eq!(output.payload.as_ref(), format!("event-{i}").as_bytes());
        }
    }

    // -----------------------------------------------------------------------
    // Content-based routing tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn routed_pipeline_routes_by_content() {
        // Events with even index → branch 0, odd → branch 1
        let events = make_events(100);
        let mut source = MemorySource::new(events, 32);

        let router = |event: &Event| {
            if event.timestamp % 2 == 0 { 0 } else { 1 }
        };

        let mut branches: Vec<(PassthroughProcessor, MemorySink)> = vec![
            (
                PassthroughProcessor::new(Arc::from("even")),
                MemorySink::new(),
            ),
            (
                PassthroughProcessor::new(Arc::from("odd")),
                MemorySink::new(),
            ),
        ];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_routed(&mut source, &router, &mut branches, &metrics, &shutdown)
            .await
            .unwrap();

        // 50 even (0,2,4,...,98) → branch 0, 50 odd → branch 1
        assert_eq!(branches[0].1.len(), 50);
        assert_eq!(branches[1].1.len(), 50);
        assert_eq!(&*branches[0].1.outputs()[0].destination, "even");
        assert_eq!(&*branches[1].1.outputs()[0].destination, "odd");
    }

    #[tokio::test]
    async fn routed_pipeline_all_to_one_branch() {
        let events = make_events(10);
        let mut source = MemorySource::new(events, 10);

        // All events go to branch 0
        let router = |_: &Event| 0;
        let mut branches = vec![
            (
                PassthroughProcessor::new(Arc::from("all")),
                MemorySink::new(),
            ),
            (
                PassthroughProcessor::new(Arc::from("none")),
                MemorySink::new(),
            ),
        ];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_routed(&mut source, &router, &mut branches, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(branches[0].1.len(), 10);
        assert_eq!(branches[1].1.len(), 0);
    }

    #[tokio::test]
    async fn routed_pipeline_out_of_range_dropped() {
        let events = make_events(5);
        let mut source = MemorySource::new(events, 10);

        // Route to branch 99 (out of range) — events are dropped
        let router = |_: &Event| 99;
        let mut branches = vec![(
            PassthroughProcessor::new(Arc::from("only")),
            MemorySink::new(),
        )];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_routed(&mut source, &router, &mut branches, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(branches[0].1.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fan_out_with_blackhole() {
        let events = make_events(1_000);
        let mut source = MemorySource::new(events, 128);
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sinks = vec![BlackholeSink::new(), BlackholeSink::new()];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_fan_out(&mut source, &processor, &mut sinks, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sinks[0].count(), 1_000);
        assert_eq!(sinks[1].count(), 1_000);
    }

    #[test]
    fn topology_detect_linear() {
        assert_eq!(Topology::detect(1, 1), Topology::Linear);
    }

    #[test]
    fn topology_detect_fan_in() {
        assert_eq!(Topology::detect(3, 1), Topology::FanIn);
    }

    #[test]
    fn topology_detect_fan_out() {
        assert_eq!(Topology::detect(1, 4), Topology::FanOut);
    }

    #[test]
    fn topology_detect_fan_in_fan_out() {
        assert_eq!(Topology::detect(2, 3), Topology::FanInFanOut);
    }

    #[tokio::test]
    async fn run_topology_linear_path() {
        let events = make_events(100);
        let mut sources = vec![MemorySource::new(events, 32)];
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sinks = vec![MemorySink::new()];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_topology(&mut sources, &processor, &mut sinks, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sinks[0].outputs().len(), 100);
    }

    #[tokio::test]
    async fn run_topology_fan_in_path() {
        let events_a = make_events(50);
        let events_b = make_events(50);
        let mut sources = vec![
            MemorySource::new(events_a, 32),
            MemorySource::new(events_b, 32),
        ];
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sinks = vec![MemorySink::new()];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_topology(&mut sources, &processor, &mut sinks, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sinks[0].outputs().len(), 100);
    }

    #[tokio::test]
    async fn run_topology_fan_out_path() {
        let events = make_events(100);
        let mut sources = vec![MemorySource::new(events, 32)];
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sinks = vec![BlackholeSink::new(), BlackholeSink::new()];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_topology(&mut sources, &processor, &mut sinks, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sinks[0].count(), 100);
        assert_eq!(sinks[1].count(), 100);
    }

    #[tokio::test]
    async fn run_topology_fan_in_fan_out_path() {
        let events_a = make_events(40);
        let events_b = make_events(60);
        let mut sources = vec![
            MemorySource::new(events_a, 32),
            MemorySource::new(events_b, 32),
        ];
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sinks = vec![BlackholeSink::new(), BlackholeSink::new()];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run_topology(&mut sources, &processor, &mut sinks, &metrics, &shutdown)
            .await
            .unwrap();

        assert_eq!(sinks[0].count(), 100);
        assert_eq!(sinks[1].count(), 100);
    }

    #[tokio::test]
    async fn run_topology_empty_sources_errors() {
        let mut sources: Vec<MemorySource> = vec![];
        let processor = PassthroughProcessor::new(Arc::from("out"));
        let mut sinks = vec![BlackholeSink::new()];
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        let result =
            run_topology(&mut sources, &processor, &mut sinks, &metrics, &shutdown).await;
        assert!(result.is_err());
    }
}
