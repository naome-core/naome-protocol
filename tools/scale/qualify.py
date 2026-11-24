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


# The earlier writer produced these roots from generated IDs without storing payloads.
INVENTORY_ROOTS = {
    100: "1:1:099a070fe4dbf60cec1a0cce433c011d4a8455bd11bfd605c1f2d282f6eeaa50",
    10000: "1:1:1e0ddde841382f83eb81de6ef24dc93b323d88d16d953326408816131306e205",
    1000000: "1:1:61e7c7c285c4af1abf86c2e700ee97c0a1799f04ba2f6ab4af839104e8207ab7",
}
PAYLOAD_BYTES = {128: 17298, 1024: 139178, 12800: 1755290}
WORKLOADS = [
    (["mixed"], [24]),
    (["inventory"], list(INVENTORY_ROOTS)),
    (["first", "point", "pull-first", "delta", "pull-delta"], list(PAYLOAD_BYTES)),
]


def deterministic_fields(data):
    if not isinstance(data, dict):
        return data
    if "reads" in data:
        assert data["reads"] <= data["backing_reads"] <= 5 * data["reads"]
    variable = {"elapsed_ms", "writer_finish_us", "queue_full_events", "receiver_growth"}
    return {k: deterministic_fields(v) for k, v in data.items() if k not in variable}


def verify(report):
    if report.get("archive_format") != "manifest":
        raise ValueError("use the qualification driver from the measured revision for older layouts")
    samples = report["samples"]
    by_trial = {}
    first = {}
    full = {(p, n) for phases, sizes in WORKLOADS for p in phases for n in sizes}
    quick = {(p, sizes[0]) for phases, sizes in WORKLOADS for p in phases}
    if report.get("qualification_version", 1) == 1:
        full = {key for key in full if key[0] != "point"}
        quick = {key for key in quick if key[0] != "point"}
    for row in samples:
        phase, size = key = row["phase"], row["size"]
        seen = by_trial.setdefault(row["repetition"], set())
        assert key in full and key not in seen, key
        seen.add(key)
        result = row["result"]
        if phase == "inventory":
            assert result["root"] == INVENTORY_ROOTS[size]
            assert result["delta_io"]["reads"] <= result["height"]
            assert result["delta_io"]["writes"] <= 2 * result["height"] + 1
        elif phase == "point":
            assert result["payload_bytes"] > 0 and result["point_json_bytes"] < 256 * 1024
            if size >= 1024:
                assert result["point_json_bytes"] < result["full_json_bytes"]
        elif phase == "mixed":
            assert result["writers"] == len(set(result["federated_tips"])) == 2
            assert result["batches"] == result["receiver_reopens"] == size
            assert result["payload_bytes"] == 1595726 and result["queue_capacity"] == 2
            assert result["duplicate_deliveries"] == 8 and result["review_standing"] == "review"
        elif phase in {"first", "pull-first"}:
            assert result["payload_bytes"] == PAYLOAD_BYTES[size]
            if phase == "pull-first":
                assert result["batches"] == size // 64
        else:
            assert result["payload_bytes"] == 203
            if phase == "pull-delta":
                assert result["batches"] == 1 and result["descriptor_bytes"] <= 32 * 1024
        observed = deterministic_fields(result)
        assert observed == first.setdefault(key, observed), key
    assert by_trial and set(by_trial) == set(range(1, len(by_trial) + 1))
    expected = full if set(first) == full else quick
    assert all(seen == expected for seen in by_trial.values()), "incomplete workload"
    print(f"Verified archive workloads and deterministic observations in {len(samples)} samples.")


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
    for phases, sizes in WORKLOADS:
        for size in sizes[:1] if args.quick else sizes:
            for trial in range(1, args.repetitions + 1):
                with tempfile.TemporaryDirectory() as directory:
                    for phase in phases:
                        row = sample(binary, directory, size, phase)
                        row["repetition"] = trial
                        rows.append(row)
                        print(size, phase, row["process_ms"], flush=True)
    report = {"qualification_version": 2, "archive_format": "manifest", "platform": platform.platform(), "build": "cargo test --lib --release --locked",
              "rustc": subprocess.check_output(["rustc", "-V"], text=True).strip(),
              "samples": rows}
    verify(report)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
