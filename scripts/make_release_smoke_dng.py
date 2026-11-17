#!/usr/bin/env python3
"""Write a small Bayer DNG with synthetic sensor data for release smoke tests."""

import argparse
import struct
from pathlib import Path


def write_dng(path: Path) -> None:
    width, height = 96, 72
    samples = [
        512 + (x * 97 + y * 181) // 4 + (1 - (x & 1) + (y & 1)) * 180
        for y in range(height)
        for x in range(width)
    ]
    pixels = struct.pack(f"<{len(samples)}H", *samples)
    tags = {}

    def tag(number: int, kind: int, count: int, data: bytes) -> None:
        tags[number] = (kind, count, data)

    def short(number: int, *values: int) -> None:
        tag(number, 3, len(values), struct.pack(f"<{len(values)}H", *values))

    def long(number: int, *values: int) -> None:
        tag(number, 4, len(values), struct.pack(f"<{len(values)}I", *values))

    def unsigned_bytes(number: int, *values: int) -> None:
        tag(number, 1, len(values), bytes(values))

    def ascii_tag(number: int, value: str) -> None:
        data = value.encode("ascii") + b"\0"
        tag(number, 2, len(data), data)

    long(256, width)
    long(257, height)
    short(258, 16)  # BitsPerSample
    short(259, 1)  # Compression: uncompressed
    short(262, 32803)  # PhotometricInterpretation: CFA
    ascii_tag(271, "Spektrafilm")
    ascii_tag(272, "Release smoke")
    long(273, 0)  # StripOffsets, filled in below
    short(274, 1)  # Orientation
    short(277, 1)  # SamplesPerPixel
    long(278, height)
    long(279, len(pixels))

    short(33421, 2, 2)  # CFARepeatPatternDim
    unsigned_bytes(33422, 0, 1, 1, 2)  # CFAPattern: RGGB
    unsigned_bytes(50706, 1, 4, 0, 0)  # DNGVersion
    unsigned_bytes(50707, 1, 1, 0, 0)  # DNGBackwardVersion
    ascii_tag(50708, "Spektrafilm Release smoke")  # UniqueCameraModel
    unsigned_bytes(50710, 0, 1, 2)  # CFAPlaneColor
    short(50711, 1)  # CFALayout: rectangular
    short(50714, 128)  # BlackLevel
    long(50717, 4095)  # WhiteLevel
    long(50719, 0, 0)  # DefaultCropOrigin
    long(50720, width, height)  # DefaultCropSize
    # ColorMatrix1: XYZ to camera RGB, here approximating linear sRGB.
    matrix = (3241, -1537, -499, -969, 1876, 42, 56, -204, 1057)
    tag(50721, 10, 9, b"".join(struct.pack("<ii", value, 1000) for value in matrix))
    tag(50728, 5, 3, struct.pack("<6I", 2, 3, 1, 1, 3, 4))  # AsShotNeutral
    short(50778, 21)  # CalibrationIlluminant1: D65
    long(50829, 0, 0, height, width)  # ActiveArea

    entries = bytearray()
    extra = bytearray()
    extra_start = 8 + 2 + 12 * len(tags) + 4
    pixel_offset = extra_start + sum(
        (len(data) + 1) & ~1 for _, _, data in tags.values() if len(data) > 4
    )
    long(273, pixel_offset)
    for number, (kind, count, data) in sorted(tags.items()):
        entries += struct.pack("<HHI", number, kind, count)
        if len(data) <= 4:
            entries += data.ljust(4, b"\0")
        else:
            entries += struct.pack("<I", extra_start + len(extra))
            extra += data.ljust((len(data) + 1) & ~1, b"\0")

    path.write_bytes(
        b"II" + struct.pack("<HIH", 42, 8, len(tags)) + entries + b"\0" * 4 + extra + pixels
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    write_dng(parser.parse_args().output)
