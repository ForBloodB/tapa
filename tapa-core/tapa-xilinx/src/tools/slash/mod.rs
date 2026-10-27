//! TAPA XO to SLASH IP-XACT adaptation and V80 hardware linking.

use camino::Utf8PathBuf;
use regex::Regex;
use std::path::Path;

use crate::tools::vitis::timing::{parse_kernel_timing_for_clock, KernelTiming};

use crate::error::{Result, XilinxError};
use crate::runtime::process::{ToolInvocation, ToolRunner};

#[derive(Debug)]
pub struct SlashLinkJob {
    pub xo: Utf8PathBuf,
    pub config: Utf8PathBuf,
    pub output: Utf8PathBuf,
    pub work_dir: Utf8PathBuf,
    pub slashkit: String,
    pub vivado: String,
    pub jobs: u32,
    pub clock_hz: u64,
    pub floorplan_xdc: Option<Utf8PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlashLinkOutput {
    pub timing_report: Utf8PathBuf,
    pub timing: KernelTiming,
    pub stdout: String,
    pub stderr: String,
}

/// Link a single TAPA kernel for the service shell. A missed target is an
/// error even if SLASH emitted a VBIN with a derated clock.
pub fn link(runner: &dyn ToolRunner, job: &SlashLinkJob) -> Result<SlashLinkOutput> {
    if job.output.extension() != Some("vbin") || job.clock_hz == 0 || job.jobs == 0 {
        return Err(XilinxError::SlashLink(
            "link requires a .vbin output, a positive clock and a positive job count".into(),
        ));
    }
    let xo = crate::util::absolutize(&job.xo);
    let config = crate::util::absolutize(&job.config);
    let output = crate::util::absolutize(&job.output);
    let work = crate::util::absolutize(&job.work_dir);
    let ip = work.join("slash-ip");
    let slashkit = resolve_executable(&job.slashkit)?;
    let vivado = resolve_executable(&job.vivado)?;
    if output.exists() || ip.exists() {
        return Err(XilinxError::SlashLink(
            "SLASH output or export directory already exists; use a fresh link work directory"
                .into(),
        ));
    }
    fs_err::create_dir_all(&work)?;
    let adapter = work.join("slash_export.py");
    fs_err::write(&adapter, include_str!("export.py"))?;
    let mut export = ToolInvocation::new("python3")
        .arg(adapter.as_str())
        .arg("export")
        .arg(xo.as_str())
        .arg(ip.as_str());
    export.uploads = vec![adapter.clone(), xo];
    export.downloads = vec![ip.clone()];
    run(runner, &export)?;

    let mut invocation = ToolInvocation::new(slashkit.as_str())
        .arg("link")
        .arg("--vivado")
        .arg(vivado.as_str())
        .arg("--jobs")
        .arg(job.jobs.to_string())
        .arg("-c")
        .arg(config.as_str())
        .arg("-k")
        .arg(ip.join("impl/ip/component.xml").as_str())
        .arg("-o")
        .arg(output.as_str())
        .arg("-p")
        .arg("hw")
        .arg("--clock-hz")
        .arg(job.clock_hz.to_string());
    invocation.uploads = vec![ip, config];
    // The implementation project is retained on the tool host. Verification
    // runs there too, so remote linking need not download the Vivado project.
    invocation.downloads = vec![output.clone(), Utf8PathBuf::from(format!("{output}.prj"))];
    let xdc = job
        .floorplan_xdc
        .as_ref()
        .map(|xdc| crate::util::absolutize(xdc));
    let hook = work.join("slash_pre_synth.tcl");
    let placement_hook = work.join("slash_pre_place.tcl");
    fs_err::write(&placement_hook, pre_place_hook(xdc.as_ref()))?;
    fs_err::write(&hook, pre_synth_hook(&placement_hook))?;
    invocation
        .args
        .extend(["--pre-synth-tcls".into(), hook.to_string()]);
    invocation.uploads.push(hook);
    invocation.uploads.push(placement_hook);
    if let Some(xdc) = xdc {
        invocation.uploads.push(xdc);
    }
    let link_output = run_output(runner, &invocation)?;

    // SLASH's BD address assignment drives the hardware. Keep the exported
    // system map in sync so VRT's BAR lookup names that same AXI-Lite window.
    let project = Utf8PathBuf::from(format!("{output}.prj"));
    let mut finalize = ToolInvocation::new("python3")
        .arg(adapter.as_str())
        .arg("finalize")
        .arg(output.as_str());
    finalize.uploads = vec![adapter.clone(), output.clone(), project.clone()];
    finalize.downloads = vec![output.clone(), project.clone()];
    run(runner, &finalize)?;

    let mut verify = ToolInvocation::new("python3")
        .arg(adapter.as_str())
        .arg("verify")
        .arg(output.as_str())
        .arg(job.clock_hz.to_string());
    verify.uploads = vec![adapter, output.clone(), project];
    run(runner, &verify)?;
    let timing_report = find_timing_report(&Utf8PathBuf::from(format!("{output}.prj")))?;
    let timing_text = fs_err::read_to_string(&timing_report)?;
    let timing = parse_timing(&timing_text, job.clock_hz)?;
    Ok(SlashLinkOutput {
        timing_report,
        timing,
        stdout: link_output.stdout,
        stderr: link_output.stderr,
    })
}

fn pre_place_hook(floorplan_xdc: Option<&Utf8PathBuf>) -> String {
    let mut hook = include_str!("pre_place.tcl").to_owned();
    if let Some(xdc) = floorplan_xdc {
        // Floorplan diagnostics use Tcl control flow, not plain XDC syntax.
        hook.push_str("\nsource {");
        hook.push_str(xdc.as_str());
        hook.push_str("}\n");
    }
    hook
}

fn pre_synth_hook(placement_hook: &Utf8PathBuf) -> String {
    format!(
        "# Apply netlist constraints after the RM is linked, before placement.\n\
         set_property STEPS.PLACE_DESIGN.TCL.PRE [list {{{placement_hook}}}] [get_runs impl_1]\n\
         # Keep the standard place_design invocation used to validate the\n\
         # V80 hierarchy constraints; retain the strategy's other steps.\n\
         set_property STEPS.PLACE_DESIGN.ARGS.SUBDIRECTIVE {{}} [get_runs impl_1]\n\
         # VRT reserves the first 64 KiB of the service-shell control aperture.\n\
         set tapa_kernel_cells [get_bd_cells -hier -filter {{VLNV =~ \"tapa:xrtl:*\"}}]\n\
         if {{[llength $tapa_kernel_cells] != 1}} {{ error \"TAPA V80 link requires exactly one TAPA kernel, found [llength $tapa_kernel_cells]\" }}\n\
         set tapa_kernel_cell [lindex $tapa_kernel_cells 0]\n\
         set tapa_control_seg [get_bd_addr_segs -quiet \"$tapa_kernel_cell/s_axi_control/reg0\"]\n\
         if {{[llength $tapa_control_seg] != 1}} {{ error \"TAPA V80 link could not resolve the kernel AXI-Lite register segment\" }}\n\
         set tapa_control_range [get_property RANGE $tapa_control_seg]\n\
         set tapa_control_space [get_bd_addr_spaces S_AXILITE_INI]\n\
         if {{[llength $tapa_control_space] != 1}} {{ error \"TAPA V80 link could not resolve the service-shell control address space\" }}\n\
         assign_bd_address -offset 0x020200010000 -range $tapa_control_range -target_address_space $tapa_control_space $tapa_control_seg -force\n\
         set tapa_mapped_name \"SEG_[get_property NAME $tapa_kernel_cell]_reg0\"\n\
         set tapa_mapped_segments [list]\n\
         foreach tapa_mapped_candidate [get_bd_addr_segs -quiet -of_objects $tapa_control_space] {{\n\
             if {{[get_property NAME $tapa_mapped_candidate] eq $tapa_mapped_name}} {{ lappend tapa_mapped_segments $tapa_mapped_candidate }}\n\
         }}\n\
         if {{[llength $tapa_mapped_segments] != 1}} {{ error \"TAPA V80 link could not resolve the mapped kernel AXI-Lite segment\" }}\n\
         set tapa_actual_offset [get_property OFFSET [lindex $tapa_mapped_segments 0]]\n\
         if {{$tapa_actual_offset != 0x020200010000}} {{ error \"TAPA V80 kernel AXI-Lite segment did not move past the service-shell reservation (got $tapa_actual_offset)\" }}\n"
    )
}

fn resolve_executable(program: &str) -> Result<Utf8PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() || program.contains(std::path::MAIN_SEPARATOR) {
        let path = crate::util::absolutize(camino::Utf8Path::new(program));
        if path.is_file() {
            return Ok(path);
        }
    } else if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Ok(crate::util::utf8(candidate));
            }
        }
    }
    Err(XilinxError::SlashLink(format!(
        "SLASH tool `{program}` is not an executable file or PATH entry"
    )))
}

fn run(runner: &dyn ToolRunner, invocation: &ToolInvocation) -> Result<()> {
    let output = run_output(runner, invocation)?;
    if output.exit_code != 0 {
        return Err(super::tool_failure(&invocation.program, output));
    }
    Ok(())
}

fn run_output(
    runner: &dyn ToolRunner,
    invocation: &ToolInvocation,
) -> Result<crate::runtime::process::ToolOutput> {
    let output = runner.run(invocation)?;
    if output.exit_code != 0 {
        return Err(super::tool_failure(&invocation.program, output));
    }
    Ok(output)
}

fn find_timing_report(project: &Utf8PathBuf) -> Result<Utf8PathBuf> {
    let root = Path::new(project.as_str()).join("slash_rm");
    let mut reports = fs_err::read_dir(&root)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("report_timing_") && name.ends_with(".txt"))
        })
        .map(crate::util::utf8)
        .collect::<Vec<_>>();
    reports.sort();
    match reports.as_slice() {
        [report] => Ok(report.clone()),
        [] => Err(XilinxError::SlashLink(format!(
            "SLASH produced no routed timing report under `{project}`"
        ))),
        _ => Err(XilinxError::SlashLink(format!(
            "SLASH produced multiple routed timing reports under `{project}`"
        ))),
    }
}

fn parse_timing(report: &str, clock_hz: u64) -> Result<KernelTiming> {
    let requested_mhz = clock_hz / 1_000_000;
    if requested_mhz == 0 || requested_mhz > u64::from(u32::MAX) {
        return Err(XilinxError::InvalidFrequency(format!(
            "invalid SLASH clock `{clock_hz}` Hz"
        )));
    }
    let requested_mhz = f64::from(u32::try_from(requested_mhz).expect("checked above"));
    let row = Regex::new(
        r"(?m)WNS\(ns\)\s+TNS\(ns\).*?\n[ \t-]+\r?\n\s*([-+0-9.]+)\s+([-+0-9.]+)\s+\d+\s+\d+\s+([-+0-9.]+)\s+([-+0-9.]+)",
    )
    .expect("static SLASH timing expression");
    let captures = row.captures(report).ok_or_else(|| {
        XilinxError::TimingSummaryParse(
            "SLASH routed timing report has no WNS/TNS/WHS/THS summary".into(),
        )
    })?;
    let parse = |index: usize, name: &str| -> Result<f64> {
        let value = captures[index]
            .parse::<f64>()
            .map_err(|_| XilinxError::TimingSummaryParse(format!("invalid SLASH {name} value")))?;
        if value.is_finite() {
            Ok(value)
        } else {
            Err(XilinxError::TimingSummaryParse(format!(
                "SLASH {name} value is not finite"
            )))
        }
    };
    let wns = parse(1, "WNS")?;
    let tns = parse(2, "TNS")?;
    let whs = parse(3, "WHS")?;
    let ths = parse(4, "THS")?;
    if [wns, tns, whs, ths].iter().any(|value| *value < 0.0) {
        return Err(XilinxError::TimingSummaryParse(format!(
            "SLASH timing failed at {requested_mhz:.0} MHz: WNS={wns}, TNS={tns}, WHS={whs}, THS={ths}"
        )));
    }
    parse_kernel_timing_for_clock(report, "user_clk")
}

#[cfg(test)]
mod tests {
    use super::{
        link, parse_timing, pre_place_hook, pre_synth_hook, resolve_executable, SlashLinkJob,
    };
    use crate::runtime::process::{MockToolRunner, ToolOutput};
    use camino::Utf8PathBuf;

    #[test]
    fn routed_timing_uses_user_clock_and_rejects_platform_setup_or_hold_failures() {
        // Unmodified summary/clock-table excerpt from the Vivado 2025.1
        // V80 vadd implementation; platform clocks also run at 100 MHz.
        let report = include_str!("../../../../testdata/golden/v80-timing.rpt");
        let timing = parse_timing(report, 100_000_000).expect("real routed report");
        assert!((timing.reported_target_period_ns - 10.0).abs() < 1e-9);
        assert!((timing.reported_target_mhz - 100.0).abs() < 1e-9);
        assert!((timing.wns_ns - 3.135).abs() < 1e-9);
        assert!((timing.fmax_mhz - 1000.0 / 6.865).abs() < 1e-9);
        for (from, to) in [("0.006", "-0.006"), ("0.021", "-0.021")] {
            let failing = report.replacen(from, to, 1);
            let error = parse_timing(&failing, 100_000_000).expect_err("platform timing failure");
            assert!(error.to_string().contains("SLASH timing failed"));
        }
    }

    #[test]
    fn v80_pre_synth_hook_reassigns_the_real_kernel_control_segment() {
        let hook = pre_synth_hook(&Utf8PathBuf::from("/tmp/slash_pre_place.tcl"));
        assert!(hook.contains("STEPS.PLACE_DESIGN.TCL.PRE"));
        assert!(hook.contains("STEPS.PLACE_DESIGN.ARGS.SUBDIRECTIVE {} [get_runs impl_1]"));
        assert!(!hook.contains("STEPS.OPT_DESIGN.TCL.PRE"));
        assert!(hook.contains("get_bd_cells -hier -filter {VLNV =~ \"tapa:xrtl:*\"}"));
        assert!(hook.contains("get_property RANGE $tapa_control_seg"));
        assert!(
            hook.contains("assign_bd_address -offset 0x020200010000 -range $tapa_control_range")
        );
        assert!(hook.contains("get_bd_addr_segs -quiet -of_objects $tapa_control_space"));
        assert!(hook.contains("SEG_[get_property NAME $tapa_kernel_cell]_reg0"));
        assert!(hook.contains("get_property OFFSET [lindex $tapa_mapped_segments 0]"));
        assert!(hook.contains("if {$tapa_actual_offset != 0x020200010000}"));
        let placement = pre_place_hook(Some(&Utf8PathBuf::from("/tmp/floorplan.xdc")));
        assert!(placement.ends_with("source {/tmp/floorplan.xdc}\n"));
        assert!(placement.contains("set_property USER_SLR_ASSIGNMENT SLR2 $tapa_hbm_bridges"));
    }

    #[test]
    fn link_without_floorplan_still_reserves_service_shell_control_addresses() {
        let dir = tempfile::tempdir().expect("temporary link directory");
        let work = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("UTF-8 path");
        let runner = MockToolRunner::new();
        runner.push_ok("python3", ToolOutput::default());
        runner.push_failure("/bin/sh", 1, "stop after inspecting SLASH invocation");
        let job = SlashLinkJob {
            xo: work.join("kernel.xo"),
            config: work.join("connectivity.cfg"),
            output: work.join("kernel.vbin"),
            work_dir: work.join("link"),
            slashkit: "/bin/sh".to_string(),
            vivado: "/bin/sh".to_string(),
            jobs: 1,
            clock_hz: 100_000_000,
            floorplan_xdc: None,
        };
        let error = link(&runner, &job).expect_err("mock stops before implementation");
        assert!(error
            .to_string()
            .contains("stop after inspecting SLASH invocation"));
        let calls = runner.calls();
        let invocation = &calls[1];
        let hook_path = invocation
            .args
            .windows(2)
            .find(|args| args[0] == "--pre-synth-tcls")
            .expect("SLASH always receives the service-shell ABI hook");
        let hook = std::fs::read_to_string(&hook_path[1]).expect("generated hook");
        assert!(invocation.uploads.iter().any(|path| path == &hook_path[1]));
        assert!(hook.contains("assign_bd_address -offset 0x020200010000"));
        assert!(hook.contains("if {$tapa_actual_offset != 0x020200010000}"));
        assert!(hook.contains("STEPS.PLACE_DESIGN.TCL.PRE"));
        let placement_path = job.work_dir.join("slash_pre_place.tcl");
        assert!(invocation.uploads.contains(&placement_path));
        let placement = std::fs::read_to_string(placement_path).expect("placement hook");
        assert!(!placement.contains("\nsource {"));
        assert!(placement.contains("set_property USER_SLR_ASSIGNMENT SLR2 $tapa_hbm_bridges"));
        assert!(placement.contains("if {$tapa_rm_uram_used > $tapa_rm_uram_capacity}"));
    }

    #[test]
    fn resolves_absolute_executable() {
        let resolved = resolve_executable("/bin/sh").expect("shell must exist");
        assert_eq!(resolved, "/bin/sh");
    }

    #[test]
    fn rejects_missing_executable() {
        let error = resolve_executable("tapa-slash-missing-tool").expect_err("must fail");
        assert!(error.to_string().contains("not an executable file"));
    }
}
