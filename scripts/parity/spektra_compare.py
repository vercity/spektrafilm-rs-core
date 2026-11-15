"""Compare the Python 0.3.4 and Rust CPU f64 pipelines at their stage boundaries."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import numpy as np

REPO = Path(__file__).resolve().parents[2]
STAGE_PROBES = {
    "log_e_film": "SPEKTRAFILM_DUMP_FILM_LOG_RAW",
    "cmy_film": "SPEKTRAFILM_DUMP_FILM_DENSITY",
    "log_e_print": "SPEKTRAFILM_DUMP_PRINT_LOG_RAW",
    "cmy_print": "SPEKTRAFILM_DUMP_PRINT_DENSITY",
    "rgb_out": "SPEKTRAFILM_DUMP_SCAN_LINEAR_RGB",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--input", type=Path, help="Scene-linear RGB float32 TIFF")
    source.add_argument("--synthetic", action="store_true", help="Generate an 8×8 scene-linear fixture")
    parser.add_argument(
        "--input-color-space",
        choices=("ACES2065-1", "ProPhoto RGB", "sRGB"),
        default="ACES2065-1",
        help="Input primaries (default ACES2065-1). The channels must already be linear.",
    )
    destination = parser.add_mutually_exclusive_group(required=True)
    destination.add_argument("--paper", help="Explicit paper profile for print and scan")
    destination.add_argument("--scan-film", action="store_true", help="Scan the film directly")
    parser.add_argument("--film", required=True, help="Explicit film profile")
    parser.add_argument("--out-dir", type=Path, required=True, help="New or empty artifact directory")
    binary = "spektrafilm-f64.exe" if os.name == "nt" else "spektrafilm-f64"
    parser.add_argument("--rust-bin", type=Path, default=REPO / "target/release" / binary)
    parser.add_argument("--data-dir", type=Path, default=REPO / "data")
    parser.add_argument("--pointwise-dir", action="store_true", help="Enable DIR without diffusion")
    parser.add_argument("--enlarger-lut", action="store_true", help="Enable the internal print LUT")
    parser.add_argument("--scanner-lut", action="store_true", help="Enable the internal scanner LUT")
    args = parser.parse_args()
    if args.scan_film and args.enlarger_lut:
        parser.error("--enlarger-lut requires --paper")
    return args


def write_synthetic_tiff(path: Path) -> None:
    import numpy as np
    from spektrafilm.utils.io import save_image_oiio

    levels = np.array([0.0, 0.001, 0.018, 0.09, 0.18, 0.5, 1.0, 4.0], dtype=np.float32)
    gray = np.repeat(levels[:, None], 3, axis=1)
    colors = np.array(
        [
            [0.8, 0.025, 0.025],
            [0.025, 0.8, 0.025],
            [0.025, 0.025, 0.8],
            [0.8, 0.8, 0.025],
            [0.8, 0.025, 0.8],
            [0.025, 0.8, 0.8],
            [0.4, 0.18, 0.07],
            [3.0, 1.5, 0.3],
        ],
        dtype=np.float32,
    )
    image = np.repeat(np.stack((gray, colors)), 4, axis=0)
    save_image_oiio(str(path), image, bit_depth=32)


def read_linear_tiff(path: Path) -> np.ndarray:
    import numpy as np
    import OpenImageIO as oiio

    if path.suffix.lower() not in {".tif", ".tiff"}:
        raise ValueError(f"expected a .tif or .tiff input, received {path}")
    source = oiio.ImageInput.open(str(path))
    if not source:
        raise OSError(f"could not open {path}: {oiio.geterror()}")
    try:
        spec = source.spec()
        pixel_type = oiio.TypeDesc("float")
        if source.format_name() != "tiff" or spec.format != pixel_type or spec.nchannels != 3:
            raise ValueError(f"{path} must be an RGB float32 TIFF")
        pixels = source.read_image(pixel_type)
        if pixels is None:
            raise OSError(f"could not read {path}: {source.geterror()}")
        shape = (spec.height, spec.width, 3)
        image = np.asarray(pixels).reshape(shape)
        if not image.size or not np.isfinite(image).all():
            raise ValueError(f"{path} must contain finite RGB samples")
        return image
    finally:
        source.close()


def render_python(
    args: argparse.Namespace, image: np.ndarray, stages: list[str], config: dict
) -> None:
    import numpy as np
    from spektrafilm import digest_params, init_params
    from spektrafilm.runtime.pipeline import SimulationPipeline

    params = init_params(film_profile=args.film, print_profile=args.paper or args.film)
    for name, value in config["io"].items():
        setattr(params.io, name, value)
    for name, value in config["settings"].items():
        setattr(params.settings, name, value)
    params.camera.auto_exposure = False
    params.debug.deactivate_spatial_effects = True
    params.debug.deactivate_stochastic_effects = True
    params.film_render.halation.boost_ev = 0.0
    params.film_render.dir_couplers.active = args.pointwise_dir
    pipeline = SimulationPipeline(digest_params(params))

    # Carry the same pipeline state through each public tap, including the
    # geometry from preprocess and the black/white references from printing.
    previous_tap = "rgb_in"
    data = image
    for tap in stages:
        data = pipeline.process(data, inject=previous_tap, collect=tap)
        if data.shape != image.shape or not np.isfinite(data).all():
            raise ValueError(
                f"Python stage {tap} must contain finite samples in shape {image.shape}"
            )
        np.save(args.out_dir / f"python_{tap}.npy", data)
        previous_tap = tap


def render_rust(args: argparse.Namespace, source: Path, stages: list[str]) -> None:
    command = [
        str(args.rust_bin.resolve()),
        "process",
        str(source),
        "--output", str(args.out_dir / "rust_output.tif"),
        "--film", args.film,
        "--params", str(args.out_dir / "rust_params.json"),
        "--data-dir", str(args.data_dir),
    ]
    if args.scan_film:
        command.append("--scan-film")
    else:
        command.extend(["--paper", args.paper])

    # The inherited environment can contain probes from another diagnostic run.
    env = os.environ.copy()
    for probe in STAGE_PROBES.values():
        env.pop(probe, None)
    env["SPEKTRAFILM_BACKEND"] = "cpu"
    for stage in stages:
        env[STAGE_PROBES[stage]] = str(args.out_dir / f"rust_{stage}.f64le")
    subprocess.run(command, env=env, check=True)


def measure_stages(out_dir: Path, stages: list[str]) -> dict:
    import numpy as np

    metrics = {}
    for stage in stages:
        python = np.load(out_dir / f"python_{stage}.npy", mmap_mode="r")
        path = out_dir / f"rust_{stage}.f64le"
        dtype = np.dtype("<f8")
        expected_bytes = python.size * dtype.itemsize
        if not path.is_file():
            raise ValueError(f"Rust stage {stage} did not write {path.name}. Use the P0 f64 CLI.")
        if path.stat().st_size != expected_bytes:
            raise ValueError(f"Rust stage {stage} has a size mismatch. Expected {expected_bytes} bytes.")
        rust = np.memmap(path, dtype=dtype, shape=python.shape, mode="r")
        if not np.isfinite(rust).all():
            raise ValueError(f"Rust stage {stage} contains non-finite samples")
        absolute_error = np.abs(python - rust)
        metrics[stage] = {
            "max_abs": float(absolute_error.max()),
            "p99_abs": float(np.percentile(absolute_error, 99)),
            "mean_abs": float(absolute_error.mean()),
        }
    return metrics


def compare(args: argparse.Namespace) -> None:
    if not args.rust_bin.is_file():
        raise ValueError(
            f"f64 CLI not found at {args.rust_bin}. Build it as described in the parity README."
        )
    args.out_dir.mkdir(parents=True, exist_ok=True)
    if any(args.out_dir.iterdir()):
        raise ValueError(f"choose a new or empty artifact directory: {args.out_dir}")

    config = {
        "camera": {"auto_exposure": False},
        "film_render": {
            "dir_couplers": {"active": args.pointwise_dir, "diffusion_size_um": 0.0},
            "grain": {"active": False},
            "glare": {"active": False},
            "halation": {"active": False, "boost_ev": 0.0},
        },
        "print_render": {"glare": {"active": False}},
        "scanner": {"unsharp_mask": [0.0, 0.0]},
        "io": {
            "input_color_space": args.input_color_space,
            "input_cctf_decoding": False,
            "output_color_space": "sRGB",
            "output_cctf_encoding": False,
            "scan_film": args.scan_film,
        },
        "settings": {
            "neutral_print_filters_from_database": not args.scan_film,
            "use_enlarger_lut": args.enlarger_lut,
            "use_scanner_lut": args.scanner_lut,
        },
    }
    with (args.out_dir / "rust_params.json").open("x", encoding="utf-8") as params_file:
        json.dump(config, params_file, indent=2)
        params_file.write("\n")

    source = args.input
    if args.synthetic:
        source = args.out_dir / "input_linear.tif"
        write_synthetic_tiff(source)
    image = read_linear_tiff(source)
    stages = list(STAGE_PROBES)
    if args.scan_film:
        stages.remove("log_e_print")
        stages.remove("cmy_print")
    print(f"Rendering Python stages for {source} ({image.shape[1]}×{image.shape[0]})", flush=True)
    render_python(args, image, stages, config)
    print("Rendering Rust CPU stages", flush=True)
    render_rust(args, source, stages)
    metrics = measure_stages(args.out_dir, stages)
    report = {
        "input": str(source.resolve()),
        "shape": list(image.shape),
        "film": args.film,
        "paper": args.paper,
        "params": config,
        "stages": metrics,
    }
    report_path = args.out_dir / "metrics.json"
    report_path.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    print(f"{'Stage':<14} {'Max abs':>12} {'P99 abs':>12} {'Mean abs':>12}")
    for stage, error in metrics.items():
        print(
            f"{stage:<14} {error['max_abs']:>12.6g} "
            f"{error['p99_abs']:>12.6g} {error['mean_abs']:>12.6g}"
        )
    print(f"Saved {report_path}")


def main() -> None:
    try:
        compare(parse_args())
    except (ImportError, OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"parity comparison: {error}") from error


if __name__ == "__main__":
    main()
