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
TEST = "store::custody::tests::qualification_sample"


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


def verify(report):
    if report.get("archive_format") == "manifest":
        return verify_manifest(report)
    reference = json.loads((ROOT / "spec/scale/full-page-samples.json").read_text())
    baseline = {(r["phase"], r["size"]): r for r in reference["samples"]}
    def check(original, measured):
        for name, value in original.items():
            if isinstance(value, dict):
                check(value, measured[name])
            elif name not in {"elapsed_ms", "queue_full_events", "writer_finish_us", "receiver_growth"}:
                assert measured[name] == value, (name, measured[name], value)
        if "reads" in original:
            assert measured["reads"] <= measured["backing_reads"] <= 5 * measured["reads"]
    for row in report["samples"]:
        other = baseline[row["phase"], row["size"]]
        check(other["result"], row["result"])
        if row["phase"] == "first" and row["size"] == 12800 and report["platform"] == reference["platform"]:
            assert row["database_growth"] < other["database_growth"]
    print(f"Verified logical identities, work and transfer fields for {len(report['samples'])} observations.")


def verify_manifest(report):
    """Check invariant outcomes and within-format determinism; timings are observations."""
    reference = json.loads((ROOT / "spec/scale/local-samples.json").read_text())
    old = {(r["phase"], r["size"]): r["result"] for r in reference["samples"]}
    first = {}
    def stable(data):
        if isinstance(data, dict):
            if "reads" in data:
                assert data["reads"] <= data["backing_reads"] <= 5 * data["reads"]
            return {k: stable(v) for k, v in data.items()
                    if k not in {"elapsed_ms", "writer_finish_us", "queue_full_events", "receiver_growth"}}
        return data
    for row in report["samples"]:
        key = row["phase"], row["size"]
        result = row["result"]
        original = old[key]
        for field in ("payload_bytes", "batches", "duplicate_deliveries", "review_standing", "queue_capacity"):
            if field in original:
                assert result[field] == original[field], (key, field)
        if key[0] == "inventory":
            assert result["root"] == original["root"]
            assert result["delta_io"]["reads"] <= result["height"]
            assert result["delta_io"]["writes"] <= 2 * result["height"] + 1
        if key[0] == "pull-delta":
            assert result["batches"] == 1 and result["payload_bytes"] == 203
            assert result["descriptor_bytes"] <= 32 * 1024
        observed = stable(result)
        assert observed == first.setdefault(key, observed), key
    print(f"Verified archive workloads and deterministic observations in {len(report['samples'])} samples.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--quick", action="store_true")
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--verify-saved", action="store_true")
    args = parser.parse_args()
    if args.verify_saved:
        verify(json.loads(args.output.read_text()))
        return
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
    report = {"archive_format": "manifest", "platform": platform.platform(), "build": "cargo test --lib --release --locked",
              "rustc": subprocess.check_output(["rustc", "-V"], text=True).strip(),
              "samples": rows}
    verify(report)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
