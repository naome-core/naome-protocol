"""Run the offline multilingual and multimodal release cases with pinned inputs."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.request import urlopen

from huggingface_hub import snapshot_download
from PIL import Image, ImageDraw


MODEL = "google/embeddinggemma-2"
REVISION = "f9972ac22e696a105c482202646979e33e4b5a1e"
FIXTURES = {
    "bridge.png": (
        "https://raw.githubusercontent.com/google-gemma/cookbook/"
        "87b4829231c5b783a9a899791f66e41527fc5dc6/apps/sample-data/GoldenGate.png",
        "ff6b23c20b29e03631d5b27590abb7fd89f2998f08227aee21d58a491346b85b",
    ),
    "cat.mp4": (
        "https://raw.githubusercontent.com/bebechien/gemma/"
        "1b50b8d7411109aca433bb48fe2bada11f3c6a4f/videos/cat_dozing.mp4",
        "9ce447d4bbba2e0737d6bcce465c1ccba1ad0a748a7466674e39ecc91a81f66f",
    ),
    "roses.wav": (
        "https://gemma.ai/assets/capabilities/roses-are.wav",
        "9e42e31cbc41cb3c31dc4ddb1dc05fef322c61b98cc03a38c06749b2db94f6b2",
    ),
}


def create_fixtures(directory):
    directory.mkdir()
    for name, (url, expected) in FIXTURES.items():
        with urlopen(url, timeout=60) as response:
            data = response.read(4 * 1024 * 1024)
        if hashlib.sha256(data).hexdigest() != expected:
            raise ValueError(f"fixture digest changed: {name}")
        (directory / name).write_bytes(data)

    page = Image.new("RGB", (500, 400), "#eef4ee")
    draw = ImageDraw.Draw(page)
    draw.text((30, 30), "Hydrological monitoring report: river nitrate concentration", fill="black")
    draw.line([(40, 300), (140, 260), (240, 190), (340, 160), (440, 70)], fill="blue", width=4)
    page.save(directory / "nitrate.pdf")


def verify_packaged_search(nao, root, environment):
    archive = root / "packaged search.sqlite"

    def run(*args):
        return json.loads(subprocess.check_output(
            [nao, "--db", archive, *args], cwd=root, env=environment, timeout=420
        ))

    media = Path(environment["NAOME_MEDIA_FIXTURES"]) / "bridge.png"
    object_id = run("capture", media, "image/png")
    head = run("snapshot", "packaged model search", "--root", object_id)
    for query in [["--prepare", "Golden Gate Brücke in San Francisco"],
                  ["--rebuild-index", "--input", media]]:
        report = run("find", "--head", head, "--mode", "semantic", *query)
        hits = report["hits"]
        if not report["semantic_index_complete"] or not hits or hits[0]["id"] != object_id:
            raise ValueError(f"packaged model search did not retrieve the bridge image: {report}")
    print("Verified multilingual text and image queries through the packaged CLI")


def qualify(nao=None):
    with tempfile.TemporaryDirectory(prefix="nao-search-qualification-") as temporary:
        root = Path(temporary)
        environment = os.environ.copy()
        environment.pop("NAO_ARCHIVE", None)
        if "NAOME_SEARCH_MODEL" not in environment:
            environment["NAOME_SEARCH_MODEL"] = snapshot_download(
                MODEL, revision=REVISION, local_dir=root / "model weights"
            )
        if "NAOME_MEDIA_FIXTURES" not in environment:
            fixtures = root / "media"
            create_fixtures(fixtures)
            environment["NAOME_MEDIA_FIXTURES"] = str(fixtures)
        environment["NAOME_SEARCH_PYTHON"] = sys.executable
        environment["NAOME_SEARCH_DEVICE"] = "cpu"
        environment["HF_HUB_OFFLINE"] = "1"
        subprocess.run(
            ["cargo", "test", "--release", "--locked", "--test", "search_multilingual",
             "--test", "search_media", "--", "--ignored", "--nocapture"],
            check=True,
            env=environment,
        )
        if nao is not None:
            verify_packaged_search(nao, root, environment)


if __name__ == "__main__":
    qualify()
