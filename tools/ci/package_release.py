"""Stage and verify the native nao download."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[2]
PUBLISHED_PROFILES = {
    "profile": "1:1:c91fd687d45f55db9c11a22a020a0831921402bc455016fcb876eda782f5e851",
    "measurement_profile": "1:1:96598e90cbf5bdeb629b3034fb6412b0e246c77d518f5012c8433cb2db232a87",
}


def verify(nao, directory, version):
    version_line = subprocess.check_output([nao, "--version"], cwd=directory, text=True, timeout=60)
    help_text = subprocess.check_output([nao, "--help"], cwd=directory, text=True, timeout=60)
    usage = next((line for line in help_text.splitlines() if line.startswith("Usage: ")), "")
    if version_line.strip() != f"nao {version}" or not usage.startswith("Usage: nao "):
        raise ValueError(f"unexpected packaged version or help: {version_line.strip()!r}, {usage!r}")

    def run(*args):
        return json.loads(subprocess.check_output([nao, *args], cwd=directory, timeout=60))

    capabilities = run("capabilities")
    if capabilities["version"] != version:
        raise ValueError("packaged executable and source versions differ")
    for name, expected in PUBLISHED_PROFILES.items():
        if capabilities[name] != expected:
            raise ValueError(f"packaged executable changed the published {name} identifier")

    reading = "Kalibrierung bei 80 µm\n".encode()
    (directory / "Messung µ.txt").write_bytes(reading)
    object_id = run("blob", "Messung µ.txt")
    head = run("snapshot", "release smoke", "--root", object_id)
    for mode, options in [("json", []), ("stream", ["--stream"])]:
        exchange = f"exchange.{mode}"
        receiver = f"{mode} receiver.sqlite"
        resolved = f"{mode} reading.txt"
        run("export", head, exchange, *options)
        run("--db", receiver, "import", exchange, *options)
        subprocess.run([nao, "--db", receiver, "resolve", head, object_id, resolved],
                       cwd=directory, timeout=60, check=True)
        if (directory / resolved).read_bytes() != reading:
            raise ValueError(f"{mode} exchange changed the payload")

    run("keygen", "signing seed")
    if (directory / "signing seed").stat().st_size != 32:
        raise ValueError("key generation did not write a 32-byte seed")
    print(f"Verified {nao.name}: published profiles, both exchange formats, and key generation")


def release_version():
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        return tomllib.load(manifest)["package"]["version"]


def package_path(target, version):
    suffix = "zip" if os.name == "nt" else "tar.gz"
    return ROOT / "target" / "distribution" / f"nao-v{version}-{target}.{suffix}"


def checksum(path):
    with path.open("rb") as contents:
        return f"{hashlib.file_digest(contents, 'sha256').hexdigest()}  {path.name}\n".encode("ascii")


def stage(target):
    rust_version = subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True)
    host = next(line.removeprefix("host: ") for line in rust_version.splitlines()
                if line.startswith("host: "))
    if host != target:
        raise ValueError(f"expected a native {target} runner, found {host}")
    version = release_version()
    download = package_path(target, version)
    name = "nao.exe" if os.name == "nt" else "nao"
    archive_format = "zip" if os.name == "nt" else "gztar"
    build_directory = ROOT / "target" / os.environ.get("CARGO_BUILD_TARGET", "")
    download.parent.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nao-release-") as temporary:
        root = Path(temporary)
        staging = root / "staging"
        staging.mkdir()
        shutil.copy2(build_directory / "release" / name, staging)
        for path in [ROOT / "LICENSE", ROOT / "README.md", *ROOT.glob("requirements-search*.txt")]:
            shutil.copy2(path, staging)
        archive = shutil.make_archive(
            str(root / f"nao-v{version}-{target}"), archive_format, root_dir=staging
        )
        shutil.copy2(archive, download)
    download.with_name(f"{download.name}.sha256").write_bytes(checksum(download))
    print(f"Staged {download.name}")


def verify_download(target, model):
    version = release_version()
    download = package_path(target, version)
    if download.with_name(f"{download.name}.sha256").read_bytes() != checksum(download):
        raise ValueError("release download checksum mismatch")
    with tempfile.TemporaryDirectory(prefix="nao-release-") as temporary:
        unpacked = Path(temporary) / "Test workspace µ"
        if os.name == "nt":
            shutil.unpack_archive(download, unpacked)
            nao = unpacked / "nao.exe"
        else:
            shutil.unpack_archive(download, unpacked, filter="data")
            nao = unpacked / "nao"
        verify(nao, unpacked, version)
        if model:
            from search_qualification import qualify_packaged

            qualify_packaged(nao)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("stage").add_argument("target")
    verify_parser = commands.add_parser("verify")
    verify_parser.add_argument("target")
    verify_parser.add_argument("--model", action="store_true")
    args = parser.parse_args()
    if args.command == "stage":
        stage(args.target)
    else:
        verify_download(args.target, args.model)
