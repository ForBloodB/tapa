//! Floorplan-consumption adapter for the FIFO pass: every `FloorplanResult`
//! read the fifo wiring needs, reading `tapa_ir::floorplan` types only.

use tapa_ir::{FloorplanResult, RoutedChannel};

use crate::error::CodegenError;

/// The Body-cell count if `fifo_name` has a floorplanned cross-slot route.
///
/// `reg_regions` is the authoritative per-cell handoff to XDC emission. Using
/// its length here guarantees the generated Body hierarchy and its placement
/// constraints cannot silently disagree.
pub(super) fn stream_crossing_body_level(
    floorplan: Option<&FloorplanResult>,
    fifo_name: &str,
) -> Option<u32> {
    floorplan?
        .routes
        .iter()
        .find_map(|route| match &route.channel {
            RoutedChannel::Stream { fifo } if fifo == fifo_name => {
                Some(u32::try_from(route.reg_regions.len()).unwrap_or(u32::MAX))
            }
            RoutedChannel::Stream { .. }
            | RoutedChannel::Axi { .. }
            | RoutedChannel::Control { .. } => None,
        })
}

/// Return a balanced logical depth for `fifo_name`, validating the persisted
/// override against the source FIFO depth.
pub(super) fn stream_fifo_depth(
    floorplan: Option<&FloorplanResult>,
    fifo_name: &str,
    source_depth: u32,
) -> Result<u32, CodegenError> {
    let Some(floorplan) = floorplan else {
        return Ok(source_depth);
    };
    let Some(depth) = floorplan.stream_fifo_depths.get(fifo_name).copied() else {
        return Ok(source_depth);
    };
    if depth < source_depth {
        return Err(CodegenError::InvalidFloorplan(format!(
            "balanced depth {depth} for stream FIFO `{fifo_name}` is smaller than its source depth {source_depth}"
        )));
    }
    Ok(depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_body_count_comes_from_xdc_region_list() {
        use std::collections::BTreeMap;
        use tapa_ir::{PipelineRoute, PipelineScheme};

        let floorplan = FloorplanResult {
            device: "u280".to_string(),
            grid: (2, 3),
            regions: BTreeMap::new(),
            routes: vec![PipelineRoute {
                channel: RoutedChannel::Stream {
                    fifo: "data_q".to_string(),
                },
                route: vec!["SLOT_X0Y0".to_string(), "SLOT_X0Y1".to_string()],
                scheme: PipelineScheme::Double,
                reg_regions: vec!["SLOT_X0Y0".to_string(), "SLOT_X0Y1".to_string()],
            }],
            slot_usage: BTreeMap::new(),
            ..FloorplanResult::default()
        };
        assert_eq!(
            stream_crossing_body_level(Some(&floorplan), "data_q"),
            Some(2)
        );
    }

    #[test]
    fn balanced_depth_defaults_to_source_and_rejects_shrinking() {
        use std::collections::BTreeMap;

        let floorplan = FloorplanResult {
            stream_fifo_depths: BTreeMap::from([("stream".to_string(), 24)]),
            ..FloorplanResult::default()
        };
        assert_eq!(stream_fifo_depth(None, "stream", 16).expect("no plan"), 16);
        assert_eq!(
            stream_fifo_depth(Some(&floorplan), "stream", 16).expect("balanced"),
            24
        );
        let error =
            stream_fifo_depth(Some(&floorplan), "stream", 32).expect_err("cannot shrink depth");
        assert!(matches!(error, CodegenError::InvalidFloorplan(_)));
    }
}
