//! One iterative bounded snapshot-DAG walk shared by history display,
//! message reachability, and integrator assignment checks.
//!
//! This module replaces three duplicated reachability walks (previously in
//! `history.rs`, `messages.rs`, and `integrator.rs`) with a single iterative
//! walk that enforces explicit node/parent/depth/byte budgets and reports a
//! typed exhaustion result instead of collapsing every bound into a bare
//! error string.
//!
//! The walk decides *which* snapshots are visited and in which deterministic
//! order. Message decoding, history rendering, and integrator ranking stay
//! with the callers: this module only loads snapshots through a
//! [`SnapshotLoader`] and hands each visited snapshot to a small
//! [`TraversalVisitor`].
//!
//! Ordering is deterministic and caller-selectable: the root is visited
//! first, then parents are pushed in declaration order (or the reverse) so a
//! stack pop always yields the same traversal for the same DAG.
//!
//! Budget exhaustion is reported as [`TraversalOutcome::Exhausted`] with the
//! exact budget that failed; callers translate it into their own errors or
//! fail-closed behavior. Loader errors (for example a corrupt or missing
//! parent snapshot) propagate unchanged with context.

use crate::snapshot::SnapshotEngine;
use anyhow::{Context, Result};
use feanorfs_common::Snapshot;

/// Explicit budgets bounding one DAG walk.
///
/// Every budget is a strict upper bound: exceeding it produces
/// [`TraversalExhaustion`]. Callers that do not care about a bound pass
/// [`TraversalBudgets::unlimited`] for that field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraversalBudgets {
    /// Maximum number of distinct snapshot ids touched (visited or queued on
    /// the frontier). This is the memory bound: it caps both the visited set
    /// and the pending stack combined. A linear chain therefore visits at
    /// most `node_budget` nodes before exhausting.
    pub node_budget: usize,
    /// Maximum number of parent edges loaded across all visited snapshots.
    pub parent_budget: usize,
    /// Maximum depth below the root (the root is depth 0). A snapshot at
    /// depth `depth_budget + 1` is never visited: the walk exhausts first.
    pub depth_budget: usize,
    /// Maximum accumulated bytes of visited snapshot ids. This bounds the
    /// memory retained for the visited set.
    pub byte_budget: usize,
}

impl TraversalBudgets {
    /// Budgets that never exhaust; callers override the fields they need.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            node_budget: usize::MAX,
            parent_budget: usize::MAX,
            depth_budget: usize::MAX,
            byte_budget: usize::MAX,
        }
    }
}

/// Why a bounded walk stopped before visiting every reachable snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum TraversalExhaustion {
    /// `node_budget` (visited + queued ids) was exceeded.
    NodeBudget,
    /// `parent_budget` (loaded parent edges) was exceeded.
    ParentBudget,
    /// `depth_budget` was exceeded.
    DepthBudget,
    /// `byte_budget` (accumulated visited-id bytes) was exceeded.
    ByteBudget,
}

impl std::fmt::Display for TraversalExhaustion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodeBudget => f.write_str("node budget"),
            Self::ParentBudget => f.write_str("parent budget"),
            Self::DepthBudget => f.write_str("depth budget"),
            Self::ByteBudget => f.write_str("byte budget"),
        }
    }
}

/// Outcome of one bounded walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraversalOutcome {
    /// The visitor asked the walk to stop (for example the target was found
    /// or the caller's output limit was reached).
    Stopped { visited: usize },
    /// Every reachable snapshot was visited within the budgets.
    Complete { visited: usize },
    /// A budget was exhausted before the walk finished; `visited` is the
    /// number of snapshots actually handed to the visitor.
    Exhausted {
        reason: TraversalExhaustion,
        visited: usize,
    },
}

/// Parent visit order (deterministic per DAG).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentOrder {
    /// First-declared parent is visited next (parents pushed reversed so the
    /// stack pops the first parent first). The history log uses this so the
    /// primary parent lineage is reported before merge parents.
    FirstFirst,
    /// Last-declared parent is visited next (parents pushed as declared).
    /// Reachability scans use this to stay cheap on single-parent chains.
    LastFirst,
}

/// How the visitor wants the walk to proceed after one snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisitControl {
    /// Keep walking: visit this snapshot's parents.
    Continue,
    /// Stop the whole walk. The current snapshot's parents are not visited.
    Stop,
}

/// Loads one snapshot by id during a walk.
///
/// Implementations are usually thin adapters over [`SnapshotEngine`] (see
/// [`EngineLoader`]); tests and mocks can serve from memory. A load failure
/// (for example a corrupt or missing parent snapshot) aborts the walk with an
/// error annotated by the offending id.
pub trait SnapshotLoader {
    /// Loads and decodes the snapshot with `id`.
    async fn load(&mut self, id: &str) -> Result<Snapshot>;
}

/// Small visitor applied to every snapshot the walk reaches.
///
/// The visitor decides whether the walk continues and collects whatever the
/// caller needs (entries, matches, a found target). Return
/// [`VisitControl::Stop`] to end the walk early; return an error to abort it.
pub trait TraversalVisitor {
    /// Handles one reached snapshot at `depth` (root is 0).
    async fn visit(&mut self, snapshot: &Snapshot, id: &str, depth: usize) -> Result<VisitControl>;
}

/// Standard [`SnapshotLoader`] over a [`SnapshotEngine`].
pub struct EngineLoader<'ctx, 'a>(pub &'ctx SnapshotEngine<'ctx, 'a>);

impl<'ctx, 'a> SnapshotLoader for EngineLoader<'ctx, 'a> {
    async fn load(&mut self, id: &str) -> Result<Snapshot> {
        self.0.load_snapshot(id).await
    }
}
/// Collection policy that stops as soon as a target snapshot id is reached.
///
/// Used by reachability checks: the walk outcome distinguishes the found
/// (`Stopped`) case from a bounded scan (`Exhausted`) and an unreachable DAG
/// (`Complete`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFinder {
    target: String,
}

impl TargetFinder {
    /// Creates a finder for `target`.
    #[must_use]
    pub fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
        }
    }
}

impl TraversalVisitor for TargetFinder {
    async fn visit(
        &mut self,
        _snapshot: &Snapshot,
        id: &str,
        _depth: usize,
    ) -> Result<VisitControl> {
        if id == self.target {
            Ok(VisitControl::Stop)
        } else {
            Ok(VisitControl::Continue)
        }
    }
}

/// Iterative bounded DAG walk from `root`.
///
/// Visits `root` first (depth 0), then its parents in [`ParentOrder`], and so
/// on, loading each snapshot through `loader`. Every visited snapshot is
/// handed to `visitor` with its id and depth. The walk never revisits an id.
/// Budget exhaustion yields [`TraversalOutcome::Exhausted`]; a visitor
/// returning [`VisitControl::Stop`] yields [`TraversalOutcome::Stopped`]; a
/// fully drained reachable DAG yields [`TraversalOutcome::Complete`].
///
/// # Errors
/// Returns an error when `loader` fails (for example a corrupt or missing
/// parent snapshot) or when `visitor` fails.
pub async fn walk<L, V>(
    root: &str,
    budgets: TraversalBudgets,
    parent_order: ParentOrder,
    loader: &mut L,
    visitor: &mut V,
) -> Result<TraversalOutcome>
where
    L: SnapshotLoader,
    V: TraversalVisitor,
{
    let mut pending = vec![(root.to_string(), 0_usize)];
    let mut seen = std::collections::HashSet::new();
    let mut visited = 0_usize;
    let mut loaded_edges = 0_usize;
    let mut id_bytes = 0_usize;

    while let Some((id, depth)) = pending.pop() {
        // Node budget: visited ids plus queued ids stay under the cap. The +1
        // accounts for the popped id, which is no longer on the frontier but
        // will be visited unless a cheaper budget trips first.
        if seen.len().saturating_add(pending.len()).saturating_add(1) > budgets.node_budget {
            return Ok(TraversalOutcome::Exhausted {
                reason: TraversalExhaustion::NodeBudget,
                visited,
            });
        }
        if depth > budgets.depth_budget {
            return Ok(TraversalOutcome::Exhausted {
                reason: TraversalExhaustion::DepthBudget,
                visited,
            });
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        id_bytes = id_bytes.saturating_add(id.len());
        if id_bytes > budgets.byte_budget {
            return Ok(TraversalOutcome::Exhausted {
                reason: TraversalExhaustion::ByteBudget,
                visited,
            });
        }
        let snapshot = loader
            .load(&id)
            .await
            .with_context(|| format!("load snapshot {id} during traversal"))?;
        loaded_edges = loaded_edges.saturating_add(snapshot.parents.len());
        if loaded_edges > budgets.parent_budget {
            return Ok(TraversalOutcome::Exhausted {
                reason: TraversalExhaustion::ParentBudget,
                visited,
            });
        }
        match visitor.visit(&snapshot, &id, depth).await? {
            VisitControl::Continue => {}
            VisitControl::Stop => {
                return Ok(TraversalOutcome::Stopped {
                    visited: visited.saturating_add(1),
                });
            }
        }
        visited = visited.saturating_add(1);
        let child_depth = depth.saturating_add(1);
        match parent_order {
            ParentOrder::FirstFirst => {
                pending.extend(
                    snapshot
                        .parents
                        .iter()
                        .rev()
                        .map(|id| (id.clone(), child_depth)),
                );
            }
            ParentOrder::LastFirst => {
                pending.extend(snapshot.parents.iter().map(|id| (id.clone(), child_depth)));
            }
        }
    }
    Ok(TraversalOutcome::Complete { visited })
}

#[cfg(test)]
mod tests {
    use super::{
        walk, ParentOrder, SnapshotLoader, TraversalBudgets, TraversalExhaustion, TraversalOutcome,
        TraversalVisitor, VisitControl,
    };
    use anyhow::Result;
    use feanorfs_common::Snapshot;
    use std::collections::HashMap;

    fn snapshot(parents: &[&str]) -> Snapshot {
        Snapshot {
            root: "0".repeat(64),
            parents: parents.iter().map(|id| (*id).to_string()).collect(),
            author: "tester".to_string(),
            created_at_ms: 1,
            message: None,
        }
    }

    /// Builds a chain `a00 -> a01 -> a02 -> ...` with three-byte ids so byte
    /// accounting is predictable; the last node has no parents.
    fn chain(len: usize) -> HashMap<String, Snapshot> {
        let mut dag = HashMap::new();
        for index in 0..len {
            let id = format!("a{index:02x}");
            let parent = format!("a{:02x}", index + 1);
            let parents: Vec<&str> = if index + 1 < len {
                vec![&parent]
            } else {
                Vec::new()
            };
            dag.insert(id, snapshot(&parents));
        }
        dag
    }

    /// In-memory loader serving snapshots out of a map; a missing id fails
    /// exactly like a corrupt/missing parent would through the engine.
    struct MemoryLoader {
        dag: HashMap<String, Snapshot>,
    }

    impl SnapshotLoader for MemoryLoader {
        async fn load(&mut self, id: &str) -> Result<Snapshot> {
            self.dag
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing parent snapshot {id}"))
        }
    }

    /// Collects every visited id in visit order.
    struct IdCollector {
        seen: Vec<String>,
        stop_at: Option<String>,
    }

    impl TraversalVisitor for IdCollector {
        async fn visit(
            &mut self,
            _snapshot: &Snapshot,
            id: &str,
            _depth: usize,
        ) -> Result<VisitControl> {
            self.seen.push(id.to_string());
            if self.stop_at.as_deref() == Some(id) {
                Ok(VisitControl::Stop)
            } else {
                Ok(VisitControl::Continue)
            }
        }
    }

    async fn collect(
        dag: &HashMap<String, Snapshot>,
        root: &str,
        budgets: TraversalBudgets,
        order: ParentOrder,
    ) -> Result<(TraversalOutcome, Vec<String>)> {
        let mut loader = MemoryLoader { dag: dag.clone() };
        let mut visitor = IdCollector {
            seen: Vec::new(),
            stop_at: None,
        };
        let outcome = walk(root, budgets, order, &mut loader, &mut visitor).await?;
        Ok((outcome, visitor.seen))
    }

    #[tokio::test]
    async fn linear_chain_completes_in_deterministic_order() {
        let dag = chain(4);
        let (outcome, seen) = collect(
            &dag,
            "a00",
            TraversalBudgets::unlimited(),
            ParentOrder::FirstFirst,
        )
        .await
        .unwrap();
        assert_eq!(outcome, TraversalOutcome::Complete { visited: 4 });
        assert_eq!(seen, vec!["a00", "a01", "a02", "a03"]);
    }

    #[tokio::test]
    async fn merge_dag_order_depends_on_parent_order_policy() {
        // a0 has parents [a1, a2]; a1 has parent a2 (diamond).
        let mut dag = HashMap::new();
        dag.insert("a0".to_string(), snapshot(&["a1", "a2"]));
        dag.insert("a1".to_string(), snapshot(&["a2"]));
        dag.insert("a2".to_string(), snapshot(&[]));
        let (outcome, seen) = collect(
            &dag,
            "a0",
            TraversalBudgets::unlimited(),
            ParentOrder::FirstFirst,
        )
        .await
        .unwrap();
        assert_eq!(outcome, TraversalOutcome::Complete { visited: 3 });
        // First-declared parent (a1) is visited next, and each id once.
        assert_eq!(seen, vec!["a0", "a1", "a2"]);

        let (_, last_first) = collect(
            &dag,
            "a0",
            TraversalBudgets::unlimited(),
            ParentOrder::LastFirst,
        )
        .await
        .unwrap();
        assert_eq!(last_first, vec!["a0", "a2", "a1"]);
    }

    #[tokio::test]
    async fn node_budget_exhaustion_reports_typed_reason() {
        let dag = chain(5);
        let budgets = TraversalBudgets {
            node_budget: 3,
            ..TraversalBudgets::unlimited()
        };
        let (outcome, seen) = collect(&dag, "a00", budgets, ParentOrder::FirstFirst)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            TraversalOutcome::Exhausted {
                reason: TraversalExhaustion::NodeBudget,
                visited: 3
            }
        );
        assert_eq!(seen, vec!["a00", "a01", "a02"]);
    }

    #[tokio::test]
    async fn depth_budget_exhaustion_reports_typed_reason() {
        let dag = chain(4);
        let budgets = TraversalBudgets {
            depth_budget: 1,
            ..TraversalBudgets::unlimited()
        };
        let (outcome, seen) = collect(&dag, "a00", budgets, ParentOrder::FirstFirst)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            TraversalOutcome::Exhausted {
                reason: TraversalExhaustion::DepthBudget,
                visited: 2
            }
        );
        assert_eq!(seen, vec!["a00", "a01"]);
    }

    #[tokio::test]
    async fn byte_budget_exhaustion_reports_typed_reason() {
        // Three-byte ids: two visits accumulate 6 bytes, the third would make 9.
        let dag = chain(4);
        let budgets = TraversalBudgets {
            byte_budget: 6,
            ..TraversalBudgets::unlimited()
        };
        let (outcome, seen) = collect(&dag, "a00", budgets, ParentOrder::FirstFirst)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            TraversalOutcome::Exhausted {
                reason: TraversalExhaustion::ByteBudget,
                visited: 2
            }
        );
        assert_eq!(seen, vec!["a00", "a01"]);
    }

    #[tokio::test]
    async fn parent_budget_exhaustion_reports_typed_reason() {
        let mut dag = HashMap::new();
        dag.insert("a0".to_string(), snapshot(&["a1", "a2", "a3"]));
        dag.insert("a1".to_string(), snapshot(&[]));
        dag.insert("a2".to_string(), snapshot(&[]));
        dag.insert("a3".to_string(), snapshot(&[]));
        let budgets = TraversalBudgets {
            parent_budget: 2,
            ..TraversalBudgets::unlimited()
        };
        let (outcome, seen) = collect(&dag, "a0", budgets, ParentOrder::FirstFirst)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            TraversalOutcome::Exhausted {
                reason: TraversalExhaustion::ParentBudget,
                visited: 0
            }
        );
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn corrupt_or_missing_parent_propagates_an_error() {
        // a00 -> a01 -> missing.
        let mut dag = HashMap::new();
        dag.insert("a00".to_string(), snapshot(&["a01"]));
        dag.insert("a01".to_string(), snapshot(&["dead"]));
        let error = collect(
            &dag,
            "a00",
            TraversalBudgets::unlimited(),
            ParentOrder::FirstFirst,
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("dead"),
            "error should name the missing parent: {message}"
        );
    }

    #[tokio::test]
    async fn target_finder_stops_at_the_target() {
        let dag = chain(4);
        let mut loader = MemoryLoader { dag };
        let mut visitor = super::TargetFinder::new("a02");
        let outcome = walk(
            "a00",
            TraversalBudgets::unlimited(),
            ParentOrder::FirstFirst,
            &mut loader,
            &mut visitor,
        )
        .await
        .unwrap();
        assert_eq!(outcome, TraversalOutcome::Stopped { visited: 3 });
    }

    #[tokio::test]
    async fn visitor_stop_short_circuits_before_children() {
        let dag = chain(4);
        let mut loader = MemoryLoader { dag: dag.clone() };
        let mut visitor = IdCollector {
            seen: Vec::new(),
            stop_at: Some("a01".to_string()),
        };
        let outcome = walk(
            "a00",
            TraversalBudgets::unlimited(),
            ParentOrder::FirstFirst,
            &mut loader,
            &mut visitor,
        )
        .await
        .unwrap();
        assert_eq!(outcome, TraversalOutcome::Stopped { visited: 2 });
        assert_eq!(visitor.seen, vec!["a00", "a01"]);
    }

    #[tokio::test]
    async fn cycles_and_duplicate_parents_are_visited_once() {
        // a0 -> [a1, a2]; a1 -> [a0]; a2 -> [a1].
        let mut dag = HashMap::new();
        dag.insert("a0".to_string(), snapshot(&["a1", "a2"]));
        dag.insert("a1".to_string(), snapshot(&["a0"]));
        dag.insert("a2".to_string(), snapshot(&["a1"]));
        let (outcome, seen) = collect(
            &dag,
            "a0",
            TraversalBudgets::unlimited(),
            ParentOrder::FirstFirst,
        )
        .await
        .unwrap();
        assert_eq!(outcome, TraversalOutcome::Complete { visited: 3 });
        assert_eq!(seen, vec!["a0", "a1", "a2"]);
    }
}
