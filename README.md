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

## Search and upgrade from v0.1.2

Open the existing SQLite archive with v0.1.3. The first ranked search creates its
search tables transactionally and builds the index for the selected snapshot.
Record IDs, payloads, permissions, snapshot manifests and exchange files keep
their existing format. `rep discover HEAD` and its `rep search HEAD` alias
continue to serve structured inventory queries.

```sh
rep --db evidence.sqlite find "interferometer drift"
rep --db evidence.sqlite find "why was the displacement estimate corrected?" --head "$head_id"
```

`find` defaults to the most recently saved or imported snapshot. It can resolve a
content ID directly and accepts `--kind`, `--subject` and `--scope` to narrow
ranked results. Hits include an archived ID, a selector into the source and any
permitted evidence links. A capture can be recovered with
`rep recover HEAD ID OUTPUT --bytes BUDGET`, which verifies its original bytes.

### Search by content or by example

Configure EmbeddingGemma 2 using an operator-installed Python environment and
[the full local checkpoint](https://huggingface.co/google/embeddinggemma-2/tree/f9972ac22e696a105c482202646979e33e4b5a1e).
The worker expects the checkpoint files in one directory. A tested macOS
environment used Python 3.12, PyTorch 2.15, Transformers 5.24 and Sentence
Transformers 6.3, with Pillow, PyAV and pypdfium2. On Linux, install the CPU
PyTorch wheel if the system has no supported accelerator.

```sh
python3 -m venv .search-venv
.search-venv/bin/python -m pip install 'torch==2.15.0' 'transformers==5.24.0' \
  'sentence-transformers==6.3.0' 'torchvision==0.29.2' pillow av pypdfium2
export NAOME_SEARCH_PYTHON="$PWD/.search-venv/bin/python"
export NAOME_SEARCH_MODEL=/path/to/embeddinggemma-2
rep --db evidence.sqlite prepare-search
rep --db evidence.sqlite find "where was the calibration repeated?"
rep --db evidence.sqlite find --input example.jpg
rep --db evidence.sqlite find --input recording.m4a
rep --db evidence.sqlite find --input example.mp4
rep --db evidence.sqlite find --input dataset.json
```

A local worker processes verified bytes through private pipes. Model
initialization is offline. Queries and artifacts never leave the archive
operator's machine through search. EmbeddingGemma 2 shares a vector space
across text, images, audio and video frames. PDFs are indexed page by page.
Audio in a video can independently match the query. UTF-8 documents, code and
datasets such as CSV and JSON share the typed text index. Other binary formats
can be found through archive metadata and linked records.

Text captures are bounded to 256 KiB. Native media are bounded to 32 MiB per
capture or example file, with a five-minute audio/video window and a 24-page
PDF window. Audio selectors name time ranges, video selectors name ranges of
sampled frames or audio, and PDF selectors name pages. Images and source
documents remain available at their original resolution via `recover`.

With a configured model, hybrid is the default. The query phase uses
prepared embeddings. Text and metadata are populated in bounded batches
on first use, so an upgraded archive can be searched immediately.
`prepare-search` advances the embedding cache independently. Repeat it
while `semantic_index_complete` is false to cover additional batches.
`find --prepare` combines preparation and search for one-off debugging.
An individual media failure leaves other results available and is reported
as limited coverage.

Run `rep search-session` to reuse one local model across requests. It accepts
one JSON object per line and returns an `ok` or `error` response for each:

```json
{"op":"prepare_search"}
{"op":"find","query":"calibration drift"}
{"op":"find","input":"example.jpg"}
```

Use `--index-budget` to permit more work per preparation call or
`--rebuild-index` to rebuild the selected snapshot's view of verified
source entries. Shared passages and embeddings are reused across overlapping
snapshots. Withdrawals remove dependent passages and vectors in the same
custody transaction. The response reports snapshot coverage and the
number of vector-component comparisons. The vector budget is a limit on
full-vector equivalents. Larger scans use projected candidates followed by
exact cosine scoring within that limit.


## Bootstrap and exchange

The two pinned kits include canonical decoding rules, reference handling and six vectors each. Obtain the expected SHA-256 digest through a trusted channel, then run `rep kit FILE DIGEST`. `rep capabilities` prints the supported formats, kit digests, active receipt evaluator and accepted replay identifiers. [Format A](spec/kits/A.json) · [Format B](spec/kits/B.json) · [Research profile](spec/kits/research-profile.txt) · [Paired measurement profile](spec/kits/measurement-profile-v1.txt)

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
