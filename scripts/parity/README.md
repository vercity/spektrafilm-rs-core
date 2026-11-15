# Python ↔ Rust parity harness

`spektra_compare.py` compares Python 0.3.4 and the Rust CPU f64 pipeline using a
shared scene-linear RGB float32 TIFF. It reports maximum, 99th-percentile, and
mean absolute errors at each stage. Both pipelines run with autoexposure,
spatial effects, random effects, and output transfer encoding disabled.

## Setup

From the Rust repository root, build the f64 CLI and create a Python 3.13
environment. The dependency bounds below follow the 0.3.4 reference checkout's
`pyproject.toml`. Set `PYTHONPATH` to that checkout's `src` directory.

```bash
cargo build --locked --release -p spektrafilm-cli --bin spektrafilm-f64 --features precision-f64

uv venv --python 3.13 /tmp/spektrafilm-parity-env
parity_python=/tmp/spektrafilm-parity-env/bin/python
uv pip install --python "$parity_python" \
    'numpy~=2.4' 'scipy~=1.17' 'colour-science~=0.4.6' 'scikit-image~=0.26' \
    'matplotlib~=3.10' 'opt-einsum~=3.4.0' 'numba~=0.64' 'OpenImageIO~=3.1.11' \
    'pyfftw~=0.15.0' 'rawpy~=0.26.1' 'exiv2~=0.18.1' 'lensfunpy~=1.18.0'
export PYTHONPATH=/path/to/python/spektrafilm/src
```

## Compare a fixture

```bash
"$parity_python" scripts/parity/spektra_compare.py \
    --synthetic --film kodak_portra_400 --paper kodak_portra_endura \
    --out-dir "$(mktemp -d)"

"$parity_python" scripts/parity/spektra_compare.py \
    --synthetic --film fujifilm_provia_100f --scan-film \
    --out-dir "$(mktemp -d)"
```

The synthetic fixture contains a gray ramp through 4.0, saturated color patches,
and mixed highlights. A direct scan omits the print exposure and print CMY taps.
Use `--pointwise-dir` to enable DIR without diffusion. Use `--enlarger-lut` and
`--scanner-lut` to compare the internal spectral LUT paths independently.
`--rust-bin` and `--data-dir` accept alternate paths.

For an existing image, replace `--synthetic` with `--input /path/to/frame.tif`.
The default input primaries are ACES2065-1. Add `--input-color-space 'ProPhoto RGB'`
or `--input-color-space sRGB` for a TIFF already linear in those primaries. The
earlier positional RAW workflow now starts with decoding the RAW to a scene-linear
float32 RGB TIFF separately. Both implementations read that same TIFF.

Use a new or empty output directory for each run. It receives the exact Rust JSON
parameters, a 16-bit convenience TIFF from the Rust CLI, and `metrics.json`.
`python_<tap>.npy` and `rust_<tap>.f64le` contain the compared values in interleaved
H×W×3 order. Rust probes contain headerless little-endian float64 samples. The
`rgb_out` tap is linear sRGB after scan-stage processing and before the terminal
clamp or transfer encoding.

`py_stages.py`, `py_bisect.py`, and `rs_bisect.rs` contain earlier diagnostic probes.

## Historical findings from 2026-05-23

Tested on `IMG_8096.dng` downsampled to 1500 px, kodak_gold_200 +
fujifilm_crystal_archive_typeii, all effects off, AE off:

| Stage                                | Match status                  |
|--------------------------------------|-------------------------------|
| `build_rgb_to_adapted_xyz` matrix    | bit-identical to 7 decimals   |
| Hanatos2025 spectral upsampling       | bit-identical to 6 decimals   |
| log10 + film density curve interp    | bit-identical                 |
| Scan-film mode (skip print + scan)   | 1/255 quantization only       |
| Full chain with printing             | mean 5/255 drift, max 14/255  |

**Conclusion:** the drift is entirely in the **print stage** (between
the film density CMY output and the print density CMY output). The most
likely culprits are:

- `apply_database_neutral_print_filters` neutral filter values
- `compute_exposure_factor` print exposure normalization
- The spectral integration through the film dyes + enlarger illuminant
  + print sensitivities
- The print density curve interpolation

Verified the c/m/y neutral filter values match Python exactly:
`0.0 / 76.95409743164727 / 82.69937618751322` for kodak_gold_200 +
fujifilm_crystal_archive_typeii + TH-KG3.
