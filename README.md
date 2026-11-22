# Research Evidence Protocol

`naome-protocol` stores, exchanges and evaluates content-addressed research evidence. The library and its `rep` CLI use local SQLite archives and portable exchange files. Each judgment binds a claim, its evidence, the recipient's policy and the inventory searched for corrections. [Whitepaper](spec/research-evidence-protocol.pdf) · [LaTeX source](spec/research-evidence-protocol.tex)

## Use an offline archive

From this crate's directory, run `cargo install --locked --path .`. Set a database with `rep --db FILE`. The default is `evidence.sqlite`. The following example uses a fresh directory, a C compiler for bundled SQLite and `jq` for reading JSON output.

```sh
export archive_dir="$(mktemp -d)"
cd "$archive_dir"
printf 'instrument readback\n' > reading.txt
id=$(rep blob reading.txt | jq -r .)
head_id=$(rep snapshot readback --root "$id" | jq -r .)
rep export "$head_id" exchange.json
rep --db receiver.sqlite import exchange.json
rep --db receiver.sqlite resolve "$head_id" "$id" received.txt
cmp reading.txt received.txt
```

Use `rep put` for typed research documents and `rep assess` for a policy-bound receipt. `rep --help` lists the archive, investigation, correction, migration and learning commands. The `naome archive` frontend accepts the same operations in a node directory. To embed the Rust library alone, select `naome-protocol` with `default-features = false`. The default `cli` feature adds argument parsing, command I/O, synthetic trials and the grader.

## Bootstrap and exchange

The two pinned kits include canonical decoding rules, reference handling and six vectors each. Obtain the expected SHA-256 digest through a trusted channel, then run `rep kit FILE DIGEST`. `rep capabilities` prints the versions and digests built into this implementation. [Format A](spec/kits/A.json) · [Format B](spec/kits/B.json) · [Research profile](spec/kits/research-profile.txt) · [Paired measurement profile](spec/kits/measurement-profile-v1.txt)

A stores the typed envelope as deterministic CBOR. B uses a canonical tagged JSON encoding. Re-encoding creates a new record identifier. A verified migration binds the exact old and new records and their dependencies. The migration exchange includes the paired inventories and mapping evidence. Detached signatures continue to name their original bytes. Legacy JSON-body records and typed measurements can coexist in the same archive.

A selected snapshot includes its roots, forward dependencies, profiles and access descriptors. Include any detached attestations or notices the recipient must examine as additional roots. Export can omit restricted payloads and discloses the inventory's membership. For larger exchanges use `export --limit N` and resume with `--after ID`. Network replication limits apply when publishing a snapshot. Import verifies the packet and reconciles it atomically. The whitepaper's offline exchange section specifies the JSON fields and snapshot digest. [Complete two-object exchange](spec/examples/exchange-v1.json)

## Reproduce the calibration trial

Run these commands from an empty working directory. The grading command compares independently supplied answers to a separate answer key.

```sh
rep trial calibration > trial.json
rep suite calibration/conformance > submission.json
rep grade calibration/conformance/answers.json submission.json
```

The trial exercises 24 queries across six exchanged inventories. For the original calibration the threshold and band tests pass, and the physical claims are supported. With the accepted 80 µm correction both tests fail and both claims are refuted. Competing accepted corrections leave the physical claims contested. A recipient who receives only the certified hull can score the planned tests while the physical claims remain unresolved.

## Permissions and learning

Every read, export or training use is evaluated against the object's access descriptor. Configure trust roots and policies independently of imported evidence. The trial uses public synthetic keys. The operator controls database files and signing seeds.

A training request binds a policy, a feature-cutoff inventory, a label inventory, examples and partitions. Use `independent_roles` for shared origins such as a specimen or patient. Prospective use requires a signed release witness for the concealed outcomes. A scoped training grant requires its project identity and a valid time window. Current-standing extraction withholds a label affected by a correction. Historical extraction replays the receipt with its recorded evaluator and checks current use permission. Evaluator identifiers track assessment rules independently of the package version. [Executable learning cases](tests/learning.rs)

Measurement readings, thresholds and calibration biases use a micrometre grid. The evaluator converts integer guarantee radii in `um` or `mm` exactly. An unsupported guarantee unit leaves the physical claim open.

The two native codecs share one evaluator. The paper's independent-implementation comparison, blinded baselines and prospective instrument trials require external evidence.

## Source provenance

Version 0.1.0 was extracted from `crates/naome-protocol` at commit `8eadd8fdd8fcf5bdf3af26a77f0e20faacdfef9a` in the original NAOME repository (now `naome-core/naome-network`). The source subtree was `ae7f157cca2acc157ca0e92fafc41bc855df2988`. At extraction, the Rust sources, tests, specification and bootstrap kits retained their original bytes. The repository metadata, lockfile and workflows support a standalone build. [Source commit](https://github.com/naome-core/naome-network/commit/8eadd8fdd8fcf5bdf3af26a77f0e20faacdfef9a)
