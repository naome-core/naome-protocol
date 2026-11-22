"""Run the offline multilingual and multimodal release cases with pinned inputs."""

import argparse
import hashlib
import json
import os
import shutil
import stat
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[2]
TEST_DIRECTORY = ROOT / "target" / "search-test-binaries"
TEST_SUITES = ("search_media", "search_multilingual")
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
    from PIL import Image, ImageDraw

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
    def run(*args):
        return json.loads(subprocess.check_output([nao, *args], cwd=root, env=environment, timeout=420))

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


def test_binary(suite):
    return TEST_DIRECTORY / (suite + (".exe" if os.name == "nt" else ""))


def stage_tests():
    command = ["cargo", "test", "--release", "--locked", "--no-run", "--message-format=json"]
    for suite in TEST_SUITES:
        command.extend(["--test", suite])
    result = subprocess.run(command, check=True, cwd=ROOT, text=True, stdout=subprocess.PIPE)
    executables = {}
    for line in result.stdout.splitlines():
        event = json.loads(line)
        if event["reason"] != "compiler-artifact" or not event.get("executable"):
            continue
        target = event["target"]
        if target["kind"] == ["test"] and target["name"] in TEST_SUITES:
            executables[target["name"]] = event["executable"]
    if executables.keys() != set(TEST_SUITES):
        raise ValueError(f"missing Cargo test executables: {set(TEST_SUITES) - executables.keys()}")
    TEST_DIRECTORY.mkdir(exist_ok=True)
    for suite, executable in executables.items():
        shutil.copy2(executable, test_binary(suite))


def model_environment(root):
    environment = os.environ.copy()
    if "NAOME_SEARCH_MODEL" not in environment:
        from huggingface_hub import snapshot_download

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
    return environment


def run_tests(suite):
    executable = test_binary(suite)
    if os.name != "nt":
        executable.chmod(executable.stat().st_mode | stat.S_IXUSR)
    with tempfile.TemporaryDirectory(prefix="nao-search-qualification-") as temporary:
        subprocess.run(
            [executable, "--ignored", "--nocapture"],
            check=True,
            cwd=ROOT,
            env=model_environment(Path(temporary)),
        )


def qualify_packaged(nao):
    with tempfile.TemporaryDirectory(prefix="nao-search-qualification-") as temporary:
        root = Path(temporary)
        verify_packaged_search(nao, root, model_environment(root))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("stage")
    commands.add_parser("run").add_argument("suite", choices=TEST_SUITES)
    args = parser.parse_args()
    if args.command == "stage":
        stage_tests()
    else:
        run_tests(args.suite)
