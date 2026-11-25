"""Stage a native nao download and exercise the executable from the archive."""

import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[2]
PUBLISHED_PROFILES = {
    "profile": "1:1:c91fd687d45f55db9c11a22a020a0831921402bc455016fcb876eda782f5e851",
    "measurement_profile": "1:1:96598e90cbf5bdeb629b3034fb6412b0e246c77d518f5012c8433cb2db232a87",
}


def transfer_paged(run, head, prefix, receiver, *, sender=None, limit=16):
    origin = ["--db", sender] if sender else []
    receive = ["--db", receiver, "import"]
    pin = ["--paged", "--expected-head", head]
    cursor = None
    for index in range(64):
        page = f"{prefix}-{index}.json"
        after = ["--after", cursor] if cursor else []
        sent = run(*origin, "export", head, page, "--paged", "--limit", str(limit), *after)
        accepted = run(*receive, page, *pin)
        if accepted != sent:
            raise ValueError("receiver acknowledged a different exchange range")
        if index == 1:
            retry = run(*receive, f"{prefix}-0.json", *pin)
            if (retry["complete"], retry["continuation"]) != (accepted["complete"], accepted["continuation"]):
                raise ValueError("repeated delivery changed the exchange checkpoint")
        if accepted["complete"]:
            return
        cursor = accepted["continuation"]
    raise ValueError("paged transfer never completed")


def verify_research_handoff(run):
    study = run("trial", "research trial")
    fixture = study["fixture"]
    origin = Path("research trial") / "east.sqlite"
    receiver = "research receiver.sqlite"
    initial = fixture["published"]
    transfer_paged(run, initial, "initial-research", receiver, sender=origin)
    receipt = fixture["receipts"][2]
    replay = ["--db", receiver, "replay", receipt, "--require-match"]
    label = ["--db", receiver, "label", initial, receipt, "synthetic"]
    historical = run(*replay, "--head", initial)
    if not historical["matches"] or historical["head"] != fixture["s0"]:
        raise ValueError("received archive did not replay the historical receipt")
    if run(*label) != fixture["labels"][2]:
        raise ValueError("received archive derived a different historical label")

    current = run("--db", origin, "snapshot", "release correction review")
    transfer_paged(run, current, "corrected-research", receiver, sender=origin)
    impact = run("--db", receiver, "impact", current, study["N"], "--mapping", study["mapping"])
    affected = {item for ids in impact["by_kind"].values() for item in ids}
    if not set(fixture["labels"]) <= affected:
        raise ValueError("received correction did not route original labels")
    if run(*replay, "--head", current) != historical or run(*label) != fixture["labels"][2]:
        raise ValueError("received correction changed historical replay or label derivation")


def verify(nao, directory, version):
    readme = (directory / "README.md").read_text(encoding="utf-8")
    for link in re.findall(r"\]\((spec/[^)]+)\)", readme):
        expected = ROOT / link
        packaged = directory / link
        if not packaged.is_file() or packaged.read_bytes() != expected.read_bytes():
            raise ValueError(f"missing or altered README target in release download: {link}")

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
    for mode, options in [("json", []), ("stream", ["--stream"]), ("paged", ["--paged"])]:
        exchange = f"exchange.{mode}"
        receiver = f"{mode} receiver.sqlite"
        resolved = f"{mode} reading.txt"
        run("export", head, exchange, *options)
        pin = ["--expected-head", head] if mode == "paged" else []
        run("--db", receiver, "import", exchange, *options, *pin)
        subprocess.run([nao, "--db", receiver, "resolve", head, object_id, resolved],
                       cwd=directory, timeout=60, check=True)
        if (directory / resolved).read_bytes() != reading:
            raise ValueError(f"{mode} exchange changed the payload")

    retired = run("blob", "README.md")
    transfer_head = run("snapshot", "paged release qualification", "--root", object_id, "--root", retired)
    subprocess.run([nao, "withdraw", retired], cwd=directory, check=True, timeout=60)
    transfer_paged(run, transfer_head, "transfer", "continued receiver.sqlite", limit=1)
    subprocess.run([nao, "--db", "continued receiver.sqlite", "resolve", transfer_head, object_id,
                    "continued-reading.txt"], cwd=directory, check=True, timeout=60)
    if (directory / "continued-reading.txt").read_bytes() != reading:
        raise ValueError("continued transfer changed the available payload")
    unavailable = subprocess.run([nao, "--db", "continued receiver.sqlite", "resolve", transfer_head,
                                  retired, "withdrawn.txt"], cwd=directory, capture_output=True, timeout=60)
    if unavailable.returncode == 0 or b"unavailable" not in unavailable.stderr:
        raise ValueError("finished inventory incorrectly supplied withdrawn evidence")

    verify_research_handoff(run)

    (directory / "stream.txt").write_bytes(b"instrument capture stream")
    stream = run("blob", "stream.txt")
    manifest = [{"id": stream, "access": capabilities["public"], "file": "stream.txt"}]
    (directory / "members.json").write_text(json.dumps(manifest), encoding="utf-8")
    origin = ["--db", "origin.sqlite", "custody"]
    receiver = ["--db", "receiver.sqlite", "custody"]
    first = run(*origin, "append", "lab", "members.json")["head"]
    captured = run(*origin, "capture", "lab", stream, "Messung µ.txt", "--parent", first)
    tip = captured["head"]
    subprocess.run([nao, *origin, "share", tip], cwd=directory, check=True, timeout=60)
    run(*origin, "export-step", tip, "first.frame")
    assert run(*receiver, "import-step", tip, "first.frame")["head"] == first
    transfer = run(*receiver, "pull", "origin.sqlite", tip)
    assert transfer["complete"] and transfer["batches"] == 1
    subprocess.run([nao, *receiver, "range", tip, stream, "0", str(len(reading)), "range.bin"],
                   cwd=directory, check=True, timeout=60)
    assert (directory / "range.bin").read_bytes() == reading
    (directory / "sources.json").write_text(json.dumps([{"head": tip, "db": "receiver.sqlite"}]))
    result = run(*receiver, "review", "sources.json", object_id, "--corrections", "--receipt")
    assert result["report"]["standing"] == "outside"
    assert run(*receiver, "receipt", result["receipt"])["heads"] == [tip]

    source_bytes = (directory / "evidence.sqlite").read_bytes()
    run("migrate-archive", "evidence.sqlite", "converted.sqlite")
    assert (directory / "evidence.sqlite").read_bytes() == source_bytes
    assert run("--db", "converted.sqlite", "migrated-head", head) == head

    key_directory = directory / "key check"
    key_directory.mkdir()
    if os.name == "nt":
        subprocess.run(["icacls", str(key_directory), "/grant", "*S-1-1-0:(OI)(CI)R"], check=True)
    seed = key_directory / "signing seed"
    run("keygen", str(seed))
    if os.name == "nt":
        subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", """
            $ErrorActionPreference = 'Stop'
            $acl = Get-Acl -LiteralPath $env:NAO_KEY_CHECK_PATH
            $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
            $rules = @($acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier]))
            if (-not $acl.AreAccessRulesProtected -or $rules.Count -ne 1 -or
                $rules[0].IdentityReference -ne $sid -or $rules[0].IsInherited -or
                $rules[0].AccessControlType -ne 'Allow' -or
                $rules[0].FileSystemRights -ne 'FullControl') { throw 'Unexpected key permissions' }
        """], env={**os.environ, "NAO_KEY_CHECK_PATH": str(seed)}, check=True)
    elif seed.stat().st_mode & 0o077:
        raise ValueError("group or other permissions on signing seed")
    if seed.stat().st_size != 32:
        raise ValueError("key generation did not write a 32-byte seed")
    print(f"Verified {nao.name}: published profiles, archive exchange, custody transfer and review, and key generation")


def source_package():
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    name = f"naome-protocol-{version}"
    archive = ROOT / "target/package" / f"{name}.crate"
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    with tarfile.open(archive, "r:gz") as package:
        with package.extractfile(f"{name}/.cargo_vcs_info.json") as info:
            git = json.load(info)["git"]
    if git.get("sha1") != commit or git.get("dirty"):
        raise ValueError(f"source package does not match clean checkout {commit}")
    output = ROOT / "target/source-distribution"
    output.mkdir(exist_ok=True)
    shutil.copy2(archive, output)
    shutil.copy2(ROOT / "spec/research-evidence-protocol.pdf", output)
    print(f"Staged {archive.name} from {commit}")


def package(target, model):
    rust_version = subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True)
    host = next(line.removeprefix("host: ") for line in rust_version.splitlines()
                if line.startswith("host: "))
    if host != target:
        raise ValueError(f"expected a native {target} runner, found {host}")
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        metadata = tomllib.load(manifest)["package"]
    version = metadata["version"]
    name = "nao.exe" if os.name == "nt" else "nao"
    archive_format = "zip" if os.name == "nt" else "gztar"
    target_directory = ROOT / "target"
    build_directory = target_directory / os.environ.get("CARGO_BUILD_TARGET", "")
    output = target_directory / "distribution"
    output.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nao-release-") as temporary:
        root = Path(temporary)
        staging = root / "staging"
        staging.mkdir()
        shutil.copy2(build_directory / "release" / name, staging)
        for path in [ROOT / "LICENSE", ROOT / "README.md", *ROOT.glob("requirements-search*.txt")]:
            shutil.copy2(path, staging)
        for pattern in metadata["include"]:
            if pattern.startswith("spec/"):
                for path in ROOT.glob(pattern):
                    destination = staging / path.relative_to(ROOT)
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(path, destination)

        package_path = Path(shutil.make_archive(
            str(root / f"nao-v{version}-{target}"), archive_format, root_dir=staging
        ))
        unpacked = root / "Test workspace µ"
        if archive_format == "gztar":
            shutil.unpack_archive(package_path, unpacked, filter="data")
        else:
            shutil.unpack_archive(package_path, unpacked)
        nao = unpacked / name
        verify(nao, unpacked, version)
        if model:
            from search_qualification import qualify

            qualify(nao)
        with package_path.open("rb") as contents:
            digest = hashlib.file_digest(contents, "sha256").hexdigest()
        (output / f"{package_path.name}.sha256").write_bytes(
            f"{digest}  {package_path.name}\n".encode("ascii")
        )
        shutil.copy2(package_path, output)
        print(f"Staged {package_path.name} ({digest})")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    platform = parser.add_mutually_exclusive_group(required=True)
    platform.add_argument("target", nargs="?")
    platform.add_argument("--source", action="store_true")
    parser.add_argument("--model", action="store_true")
    args = parser.parse_args()
    if args.source:
        source_package()
    else:
        package(args.target, args.model)
