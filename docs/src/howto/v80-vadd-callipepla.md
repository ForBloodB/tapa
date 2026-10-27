# V80 Validation with vadd and Callipepla

This guide records the end-to-end TAPA flow used to validate the experimental
V80 backend with the `vadd` and Callipepla examples.

Use the checked-in application sources unchanged when validating backend
fixes. Record the source revision, HLS target, implementation clocks, and
software or RTL comparisons with each hardware result. Results obtained by
rewriting an application do not establish support for the original source.

## Requirements

Use Vitis HLS and Vivado 2025.1, CBC on `PATH`, and a SLASH `slashkit` build
that supports V80 hardware linking and the service shell. The flow was
validated with `slashkit` 0.1.0. Source the Xilinx settings and make sure the
V80 board is available to `v80-smi` and VRT before starting.

Set the tool paths for the environment:

```bash
export TAPA=/path/to/tapa
export SLASHKIT=/path/to/slashkit
export VIVADO=/opt/Xilinx/2025.1/Vivado/bin/vivado
```

## Compile and Link

For each example, run the normal TAPA stages in order. `--platform
slash_v80_service` selects the V80 target, and the connectivity file maps the
kernel memory ports to the service shell:

```bash
"$TAPA" -w build/vadd analyze --top VecAdd \
  --input tests/apps/vadd/vadd.cpp
"$TAPA" -w build/vadd synth --platform slash_v80_service \
  --clock-period 10 --jobs 8
"$TAPA" -w build/vadd floorplan --connectivity vadd.cfg \
  --partition-strategy multi-level --pp-scheme double
"$TAPA" -w build/vadd pack --output build/vadd/kernel.xo \
  --connectivity vadd.cfg
"$TAPA" -w build/vadd link --xo build/vadd/kernel.xo \
  --connectivity vadd.cfg --output build/vadd/kernel.vbin \
  --clock-hz 100000000 --jobs 8 --vivado "$VIVADO" \
  --slashkit "$SLASHKIT" --floorplan-xdc build/vadd/floorplan.xdc
```

Repeat the same stages for Callipepla, using top `Callipepla`, source
`tests/regression/callipepla/tapa/src/callipepla.cpp`, and a connectivity file
that maps its memory ports to the V80 service shell. Keep each example's
working directory and output paths separate.

Use the same SLASH connectivity file for planning, packing, and linking. For
example, `vadd.cfg` declares `nk=VecAdd:1:VecAdd_0` and maps the physical port
with `sp=VecAdd_0.m_axi_a:HBM0`. See the
[connectivity reference](../reference/cli.md#connectivity-file) for a complete example.

The final link output is a SLASH `.vbin`. Check the Vivado timing report and
the produced image before programming the board.

Before placement, the linker checks optimized RM URAM use against the shell's
actual sites and assigns the RM's HBM SmartConnect bridges to SLR2, next to
their fixed NoC terminals. This also applies without a TAPA floorplan. The
optional floorplan constraints are applied in the same placement hook; neither
hook changes the static shell or relaxes its clocks.

V80 SLR assignments target each task or pipeline-stage hierarchy root once.
They do not repeat the assignment on internal RAM modules: nested assignments
can make Vivado report twice the actual URAM demand during placement. Crossing
pipeline registers retain their separate `USER_SLL_REG` constraints. A passing
resource check or placement is not a timing result; compare setup and hold for
all clocks after routing, using the original clock periods and timing exceptions.

## RTL Cosimulation

Run the XO with the same FPGA part used for HLS. XO cosimulation does not
recover the part number from the package; without an override, xsim uses
`xc7a100tcsg324-1`. This matters for V80 floating-point primitive models.

```bash
./vadd --bitstream build/vadd/kernel.xo -cosim_simulator xsim \
  -xsim_part_num xcv80-lsva4737-2MHP-e-S \
  -cosim_work_dir build/vadd/cosim 4097
```

Use the same part override for the Callipepla host. Bazel's xsim tests set
`FRT_XSIM_PART_NUM` through their test environment.

## Board Validation

Program the board with the generated image, then run the VRT host for that
kernel:

```bash
v80-smi program -d 0000:a1:00 build/vadd/kernel.vbin
vadd-vrt 0000:a1:00 build/vadd/kernel.vbin

v80-smi program -d 0000:a1:00 build/callipepla/kernel.vbin
callipepla-vrt 0000:a1:00 build/callipepla/kernel.vbin \
  tests/regression/testdata/spd-14.mtx 16
```

The vadd validation checks lengths 1, 7, 16, 4097, 65537, and 13; each run
should report `VADD_V80_PASS`. Callipepla should report
`CALLIPEPLA_V80_PASS` after checking the residuals against the CPU result.

The V80 vadd image passed on hardware. Callipepla run3 also produced the
expected result on hardware, but its 333 MHz clock domain did not meet timing
(WNS -0.186 ns); the user clock WNS was +0.522 ns. The Callipepla result is
functional validation, not timing closure.
