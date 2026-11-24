"""Build the whitepaper with the pinned TeX engine and bundle, and verify the PDF."""

import argparse
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile

from pinned_tool import executable


ROOT = Path(__file__).resolve().parents[2]
SPEC = ROOT / "spec"
PAPER = "research-evidence-protocol"
TECTONIC_VERSION = "0.17.0"
TECTONIC_ARCHIVES = {
    # Keep the engine independent of the Linux runner's glibc version.
    ("Linux", "x86_64"): (
        "x86_64-unknown-linux-musl",
        "8533d07f9ccbd7a65824b9e0459041bca34af1eb33daba48f59215593753a3b7",
    ),
    ("Darwin", "arm64"): (
        "aarch64-apple-darwin",
        "a3f1cac7c5678f01661a92212f58480ae3b0634115d880dbc59e2953ded45667",
    ),
}
BUNDLE = "https://relay.fullyjustified.net/default_bundle_v33.tar"
BUNDLE_DIGEST = "6ffe055852f8faf66c0acbe1a7fb27f87b869a90bad1204f3bf4d9683f597c7c"
SOURCE_DATE_EPOCH = "1795392000"


def build(write):
    target, checksum = TECTONIC_ARCHIVES[(platform.system(), platform.machine())]
    url = (
        "https://github.com/tectonic-typesetting/tectonic/releases/download/"
        f"tectonic%40{TECTONIC_VERSION}/tectonic-{TECTONIC_VERSION}-{target}.tar.gz"
    )
    cache = ROOT / "target" / "whitepaper-cache"
    cache.mkdir(parents=True, exist_ok=True)
    env = {
        **os.environ,
        "SOURCE_DATE_EPOCH": SOURCE_DATE_EPOCH,
        "TECTONIC_CACHE_DIR": str(cache),
        "TZ": "UTC",
    }
    with executable(url, checksum, "tectonic") as tectonic, tempfile.TemporaryDirectory() as directory:
        subprocess.run(
            [tectonic, "--untrusted", "--bundle", BUNDLE, "--outdir", directory, f"{PAPER}.tex"],
            cwd=SPEC,
            env=env,
            check=True,
            timeout=600,
        )
        # Tectonic stores the bundle's reported digest under its escaped URL.
        bundle_key = "".join(c if c.isalnum() or c in ".-_" else f",{ord(c)}," for c in BUNDLE)
        marker = cache / "bundles" / "hashes" / bundle_key
        if marker.read_text(encoding="ascii").strip() != BUNDLE_DIGEST:
            raise ValueError("unexpected Tectonic bundle digest")
        pdf = Path(directory) / f"{PAPER}.pdf"
        checked_in = SPEC / pdf.name
        if write:
            shutil.copyfile(pdf, checked_in)
        if pdf.read_bytes() != checked_in.read_bytes():
            raise ValueError(
                "whitepaper differs from the checked-in PDF; regenerate with whitepaper.py --write"
            )
        print(f"Verified {pdf.name} ({hashlib.sha256(pdf.read_bytes()).hexdigest()})")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="replace the checked-in PDF with the build")
    build(parser.parse_args().write)
