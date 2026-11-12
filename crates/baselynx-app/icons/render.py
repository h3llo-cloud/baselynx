"""Render the app icon from baselynx.svg (the only hand-edited file here).

Writes, next to this script:
  baselynx.png   512 px: the window icon on Linux (X11), the Dock icon on macOS,
                 the icon of the desktop entry on Linux; embedded in the binary
  baselynx.ico   16-256 px: the Windows executable, taskbar, Start menu and MSI icon
and docs/images/logo.png (256 px) for the README.

Needs Chrome or Edge (renders the SVG) and Pillow (`pip install pillow`):
    python crates/baselynx-app/icons/render.py
"""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
BROWSERS = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "google-chrome",
    "chromium",
]


def browser() -> str:
    for b in BROWSERS:
        if Path(b).exists() or shutil.which(b):
            return b
    sys.exit("Chrome, Edge or Chromium is needed to render the SVG")


def render_master(size: int = 1024) -> Image.Image:
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        shutil.copy(HERE / "baselynx.svg", tmp / "icon.svg")
        (tmp / "page.html").write_text(
            "<!DOCTYPE html><html><head><style>html,body{margin:0;background:transparent}"
            f"img{{display:block}}</style></head><body><img src='icon.svg' width='{size}' height='{size}'>"
            "</body></html>",
            encoding="utf-8",
        )
        out = tmp / "master.png"
        subprocess.run(
            [browser(), "--headless=new", "--disable-gpu", "--hide-scrollbars",
             "--allow-file-access-from-files", "--default-background-color=00000000",
             f"--window-size={size},{size}", f"--screenshot={out}", (tmp / "page.html").as_uri()],
            check=True, capture_output=True,
        )
        img = Image.open(out).convert("RGBA")
        img.load()
        return img


def main() -> None:
    master = render_master()
    assert master.size == (1024, 1024), master.size
    master.resize((512, 512), Image.LANCZOS).save(HERE / "baselynx.png", optimize=True)
    master.save(HERE / "baselynx.ico", sizes=[(s, s) for s in (16, 20, 24, 32, 40, 48, 64, 128, 256)])
    (ROOT / "docs" / "images").mkdir(parents=True, exist_ok=True)
    master.resize((256, 256), Image.LANCZOS).save(ROOT / "docs" / "images" / "logo.png", optimize=True)
    for f in ("baselynx.png", "baselynx.ico"):
        print(f, (HERE / f).stat().st_size, "bytes")


if __name__ == "__main__":
    main()
