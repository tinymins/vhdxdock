"""Export the app PNG and multi-resolution Windows ICO without altering artwork.

Requires Pillow: python -m pip install Pillow
Usage: python scripts/export-icon.py [source.png]
"""

import argparse
from pathlib import Path

from PIL import Image


def main() -> None:
    assets = Path(__file__).resolve().parents[1] / "assets"
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", nargs="?", type=Path, default=assets / "icon-source.png")
    args = parser.parse_args()
    with Image.open(args.source) as source:
        if source.width != source.height:
            parser.error("The source icon must be square")
        icon = source.convert("RGBA").resize((512, 512), Image.Resampling.LANCZOS)
    icon.save(assets / "icon.png", optimize=True)
    sizes = [(size, size) for size in (16, 20, 24, 32, 40, 48, 64, 128, 256)]
    icon.save(assets / "icon.ico", format="ICO", sizes=sizes)
    with Image.open(assets / "icon.ico") as exported:
        if exported.ico.sizes() != set(sizes):
            raise RuntimeError("Exported ICO is missing a resolution")
    print("Exported assets/icon.png (512px) and assets/icon.ico (16-256px)")


if __name__ == "__main__":
    main()
