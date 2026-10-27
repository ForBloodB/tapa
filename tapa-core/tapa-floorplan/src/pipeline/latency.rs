//! AutoBridge-style integer latency balancing for stream FIFOs.
// Adapted from UCLA-VAST AutoBridge, Copyright (c) 2021 Licheng Guo.
// Licensed under MIT; see LICENSE-AUTOBRIDGE.md.

use std::collections::BTreeMap;

use tapa_ir::{PipelineRoute, RoutedChannel};

use crate::graph::FloorGraph;
use crate::solver::{
    Comparison, LinExpr, LpModel, LpStatus, Sense, SolveOpts, Solver, SolverError,
};
use crate::ExactInt;

/// Why stream latency balancing could not produce FIFO depth overrides.
#[derive(Debug, thiserror::Error)]
pub enum LatencyBalanceError {
    /// A directed cycle contains positive pipeline latency and cannot satisfy
    /// the original AutoBridge difference constraints.
    #[error("latency balancing found a positive-latency stream dependency loop")]
    PositiveLatencyCycle,
    /// A route or graph value exceeded the integer range used by the model.
    #[error("latency balancing arithmetic overflow for stream `{0}`")]
    Arithmetic(String),
    /// CBC returned no usable solution after the positive-cycle check passed.
    #[error("latency balancing ILP returned {0:?}")]
    NoSolution(LpStatus),
    /// The solver failed or returned an invalid incumbent.
    #[error(transparent)]
    Solver(#[from] SolverError),
}

#[derive(Clone)]
struct Edge {
    fifo: usize,
    fifo_name: String,
    src: usize,
    dst: usize,
    width: u32,
    latency: u32,
}

/// Return the larger logical FIFO depths selected by weighted latency
/// balancing. The map omits streams whose source depth remains unchanged.
pub(crate) fn balance_stream_latencies(
    graph: &FloorGraph,
    routes: &[PipelineRoute],
    solver: &dyn Solver,
    opts: &SolveOpts,
) -> Result<BTreeMap<String, u32>, LatencyBalanceError> {
    let edges = graph
        .streams()
        .iter()
        .enumerate()
        .map(|(fifo, stream)| {
            let body_level = routes.iter().find_map(|route| match &route.channel {
                RoutedChannel::Stream { fifo } if fifo == &stream.link => {
                    Some(route.reg_regions.len())
                }
                RoutedChannel::Stream { .. }
                | RoutedChannel::Axi { .. }
                | RoutedChannel::Control { .. } => None,
            });
            let latency = match body_level {
                Some(body) => body
                    .checked_add(1)
                    .ok_or_else(|| LatencyBalanceError::Arithmetic(stream.link.clone()))?,
                None => 0,
            };
            let latency = u32::try_from(latency)
                .map_err(|_| LatencyBalanceError::Arithmetic(stream.link.clone()))?;
            Ok(Edge {
                fifo,
                fifo_name: stream.link.clone(),
                src: stream.src,
                dst: stream.dst,
                width: stream.data_width,
                latency,
            })
        })
        .collect::<Result<Vec<_>, LatencyBalanceError>>()?;

    let added_depths = solve_balance(graph.vertices().len(), &edges, solver, opts)?;
    let mut depths = BTreeMap::new();
    for (stream, added) in graph.streams().iter().zip(added_depths) {
        if added == 0 {
            continue;
        }
        let depth = stream
            .depth
            .checked_add(added)
            .ok_or_else(|| LatencyBalanceError::Arithmetic(stream.link.clone()))?;
        depths.insert(stream.link.clone(), depth);
    }
    Ok(depths)
}

fn solve_balance(
    vertex_count: usize,
    edges: &[Edge],
    solver: &dyn Solver,
    opts: &SolveOpts,
) -> Result<Vec<u32>, LatencyBalanceError> {
    if edges.is_empty() || edges.iter().all(|edge| edge.latency == 0) {
        return Ok(vec![0; edges.len()]);
    }
    reject_positive_latency_cycles(vertex_count, edges)?;

    let max_potential = edges.iter().try_fold(0_u64, |sum, edge| {
        sum.checked_add(u64::from(edge.latency))
            .ok_or_else(|| LatencyBalanceError::Arithmetic(edge.fifo_name.clone()))
    })?;
    let bound = exact_u64_as_f64(max_potential);
    let mut model = LpModel::new(Sense::Minimize);
    let potentials = (0..vertex_count)
        .map(|vertex| model.add_integer(format!("potential_{vertex}"), -bound, bound))
        .collect::<Vec<_>>();

    let mut objective_coefficients = BTreeMap::<usize, i64>::new();
    for edge in edges {
        let width = i64::from(edge.width);
        model.add_constraint(
            format!("stream_latency_{}", edge.fifo),
            LinExpr::sum([(1.0, potentials[edge.src]), (-1.0, potentials[edge.dst])]),
            Comparison::Ge,
            f64::from(edge.latency),
        );
        for (vertex, coefficient) in [(edge.src, width), (edge.dst, -width)] {
            let current = objective_coefficients.entry(vertex).or_default();
            *current = current
                .checked_add(coefficient)
                .ok_or_else(|| LatencyBalanceError::Arithmetic(edge.fifo_name.clone()))?;
        }
    }
    model.set_objective(LinExpr::sum(
        objective_coefficients
            .into_iter()
            .filter(|(_, coefficient)| *coefficient != 0)
            .map(|(vertex, coefficient)| (coefficient.as_f64(), potentials[vertex])),
    ));

    let solution = solver.solve(&model, opts)?;
    solution.validate_for(&model)?;
    if !solution.is_found() {
        return Err(LatencyBalanceError::NoSolution(solution.status));
    }

    edges
        .iter()
        .map(|edge| {
            let slack = solution.value(potentials[edge.src])
                - solution.value(potentials[edge.dst])
                - f64::from(edge.latency);
            let added = slack.round();
            if !slack.is_finite() || (slack - added).abs() > 1e-6 || added < 0.0 {
                return Err(LatencyBalanceError::Arithmetic(edge.fifo_name.clone()));
            }
            exact_f64_as_u32(added)
                .ok_or_else(|| LatencyBalanceError::Arithmetic(edge.fifo_name.clone()))
        })
        .collect()
}

/// The inequalities are `potential[src] >= potential[dst] + latency`.
/// Longest-path relaxation detects exactly the positive-weight cycles that
/// make those constraints infeasible; zero-latency cycles remain legal.
fn reject_positive_latency_cycles(
    vertex_count: usize,
    edges: &[Edge],
) -> Result<(), LatencyBalanceError> {
    let mut distance = vec![0_u64; vertex_count];
    for pass in 0..vertex_count {
        let mut changed = false;
        for edge in edges {
            let candidate = distance[edge.dst]
                .checked_add(u64::from(edge.latency))
                .ok_or_else(|| LatencyBalanceError::Arithmetic(edge.fifo_name.clone()))?;
            if candidate > distance[edge.src] {
                distance[edge.src] = candidate;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        if pass + 1 == vertex_count {
            return Err(LatencyBalanceError::PositiveLatencyCycle);
        }
    }
    Ok(())
}

#[allow(
    clippy::cast_precision_loss,
    reason = "stream count and pipeline depths are bounded far below 2^53"
)]
fn exact_u64_as_f64(value: u64) -> f64 {
    value as f64
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the caller verifies the value is a finite, nonnegative integral u32"
)]
fn exact_f64_as_u32(value: f64) -> Option<u32> {
    (value <= f64::from(u32::MAX)).then_some(value as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::device::select::select_device;
    use crate::graph::{Stream, Vertex};
    use crate::pipeline::plan::plan_routes;
    use crate::solver::{CbcSolver, SolveOpts};
    use tapa_ir::{Area, PipelineScheme};

    fn solve(edges: &[(usize, usize, u32, u32)], vertices: usize) -> Vec<u32> {
        let edges = edges
            .iter()
            .enumerate()
            .map(|(fifo, &(src, dst, width, latency))| Edge {
                fifo,
                fifo_name: format!("fifo_{fifo}"),
                src,
                dst,
                width,
                latency,
            })
            .collect::<Vec<_>>();
        solve_balance(
            vertices,
            &edges,
            &CbcSolver::new(),
            &SolveOpts {
                time_limit: Some(std::time::Duration::from_secs(10)),
                threads: Some(1),
                mip_gap_abs: None,
            },
        )
        .expect("latency balance")
    }

    #[test]
    fn balances_short_diamond_path_on_the_narrower_fifo() {
        let added = solve(&[(0, 1, 8, 1), (1, 3, 8, 0), (0, 2, 9, 0), (2, 3, 1, 0)], 4);
        assert_eq!(added, [0, 0, 0, 1]);
    }

    #[test]
    fn v80_balances_latency_over_slr1_slr2_routes() {
        let vertices = (0..4)
            .map(|index| Vertex {
                name: format!("task_{index}"),
                area: Area::default(),
                required_tag: None,
                materialize: true,
            })
            .collect();
        // V80's SLASH RM can use SLR1 and SLR2, but not SLR0. The direct
        // edge crosses once; the alternate path crosses three times.
        let streams = [(0, 3), (0, 1), (1, 2), (2, 3)]
            .into_iter()
            .enumerate()
            .map(|(index, (src, dst))| Stream {
                link: format!("fifo_{index}"),
                src,
                dst,
                width: 34,
                data_width: 32,
                depth: 16,
            })
            .collect();
        let graph = FloorGraph {
            vertices,
            streams,
            placement_edges: Vec::new(),
            axi_nets: Vec::new(),
            control_nets: Vec::new(),
            index: std::collections::HashMap::new(),
            co_located: Vec::new(),
        };
        let device = select_device("v80").expect("v80 device model");
        let regions = BTreeMap::from([
            ("task_0".to_string(), "SLOT_X0Y1_TO_SLOT_X0Y1".to_string()),
            ("task_1".to_string(), "SLOT_X0Y2_TO_SLOT_X0Y2".to_string()),
            ("task_2".to_string(), "SLOT_X0Y1_TO_SLOT_X0Y1".to_string()),
            ("task_3".to_string(), "SLOT_X0Y2_TO_SLOT_X0Y2".to_string()),
        ]);
        let solver = CbcSolver::new();
        let opts = SolveOpts {
            time_limit: Some(std::time::Duration::from_secs(10)),
            threads: Some(1),
            mip_gap_abs: None,
        };
        let routes = plan_routes(
            &graph,
            &regions,
            &BTreeMap::new(),
            &device,
            PipelineScheme::Double,
            1.0,
            &solver,
            &opts,
        )
        .expect("route streams across the V80 RM boundary");
        assert_eq!(routes.len(), 4);
        for (fifo, src_slr, dst_slr) in [
            ("fifo_0", 1, 2),
            ("fifo_1", 1, 2),
            ("fifo_2", 2, 1),
            ("fifo_3", 1, 2),
        ] {
            let route = routes
                .iter()
                .find(|route| {
                    matches!(&route.channel, RoutedChannel::Stream { fifo: link } if link == fifo)
                })
                .expect("routed stream");
            let expected = vec![format!("SLOT_X0Y{src_slr}"), format!("SLOT_X0Y{dst_slr}")];
            assert_eq!(route.route, expected);
            assert_eq!(route.reg_regions, expected);
            assert!(!route.route.iter().any(|region| region == "SLOT_X0Y0"));
        }

        let depths = balance_stream_latencies(&graph, &routes, &solver, &opts)
            .expect("balance generated route latency");

        assert_eq!(depths, BTreeMap::from([("fifo_0".to_string(), 22)]));
    }

    #[test]
    fn accepts_zero_latency_cycles() {
        assert_eq!(solve(&[(0, 1, 32, 0), (1, 0, 32, 0)], 2), [0, 0]);
    }

    #[test]
    fn rejects_positive_latency_cycles_before_solving() {
        let edges = [
            Edge {
                fifo: 0,
                fifo_name: "fifo_0".to_string(),
                src: 0,
                dst: 1,
                width: 32,
                latency: 1,
            },
            Edge {
                fifo: 1,
                fifo_name: "fifo_1".to_string(),
                src: 1,
                dst: 0,
                width: 32,
                latency: 0,
            },
        ];
        assert!(matches!(
            reject_positive_latency_cycles(2, &edges),
            Err(LatencyBalanceError::PositiveLatencyCycle)
        ));
    }
}
