"""Run the real Rust custody code in fresh processes and report physical and logical work."""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
TEST = "store::paged::tests::qualification_sample"


def build():
    output = subprocess.check_output(
        ["cargo", "test", "--lib", "--release", "--locked", "--no-run", "--message-format=json"],
        cwd=ROOT, text=True,
    )
    return next(row["executable"] for row in map(json.loads, output.splitlines())
                if row.get("executable") and row["target"]["kind"] == ["lib"])


def sample(binary, directory, size, phase):
    env = dict(os.environ, NAOME_SAMPLE_DIR=str(directory), NAOME_SAMPLE_SIZE=str(size),
               NAOME_SAMPLE_PHASE=phase)
    with tempfile.TemporaryFile() as log:
        start = time.monotonic()
        child = subprocess.Popen([binary, "--exact", TEST, "--ignored", "--nocapture"],
                                 stdout=log, stderr=subprocess.STDOUT, env=env)
        _, status, usage = os.wait4(child.pid, 0)
        child.returncode = os.waitstatus_to_exitcode(status)
        elapsed = time.monotonic() - start
        log.seek(0)
        output = log.read().decode()
    if child.returncode:
        raise RuntimeError(output)
    row = next(json.loads(line[7:]) for line in output.splitlines() if line.startswith("SAMPLE "))
    row["process_ms"] = round(elapsed * 1000, 3)
    row["peak_rss_bytes"] = usage.ru_maxrss * (1 if platform.system() == "Darwin" else 1024)
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--quick", action="store_true")
    parser.add_argument("--repetitions", type=int, default=3)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error("expected positive repetition count")
    binary = build()
    rows = []
    for phases, sizes in [
        (["mixed"], [24]),
        (["inventory"], [100, 10000, 1000000]),
        (["first", "pull-first", "delta", "pull-delta"], [128, 1024, 12800]),
    ]:
        for size in sizes[:1] if args.quick else sizes:
            for trial in range(1, args.repetitions + 1):
                with tempfile.TemporaryDirectory() as directory:
                    for phase in phases:
                        row = sample(binary, directory, size, phase)
                        row["repetition"] = trial
                        rows.append(row)
                        print(size, phase, row["process_ms"], flush=True)
    report = {"platform": platform.platform(), "build": "cargo test --lib --release --locked",
              "rustc": subprocess.check_output(["rustc", "-V"], text=True).strip(),
              "samples": rows}
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
