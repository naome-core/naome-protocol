# Semantic archive search

## Outcome

An investigator can ask a natural-language question about a named archive snapshot and find the records or passage references worth opening. For example, “Which calibration evidence changed the displacement estimate?” can lead to a claim, a correction and its supporting report even when the query uses different wording. The first release must work across the great majority of widely used languages worldwide. Queries can find relevant material written in a different language and in archives that mix languages. Each hit points to exact archived bytes or a field in a typed record. `show`, `resolve`, `explain` and `assess` provide the follow-through from retrieval to scientific evaluation.

## Baseline

[`Archive::discover`](../src/store.rs) pages through a snapshot in ascending identifier order and filters typed records by kind, subject and scope. It uses the archive's read permission checks and skips blobs. [The CLI](../src/cli.rs) exposes it as `rep discover` with the existing `search` alias. The same commands are embedded by `naome archive`. [The protocol specification](../spec/research-evidence-protocol.tex) describes learned similarity, explicit search coverage and reproducible evidence references.

## Search interface

Offer a separate ranked entry point, `rep find QUERY` or `rep find --input FILE` with optional `--head`, backed by one library query used by archive clients. Accept optional kind, subject and scope filters, a result limit and explicit work budgets. Provide lexical, semantic and hybrid ranking modes. Route exact content identifiers to the archive's verified lookup. Existing structured inventory queries use `discover` and its `search` alias.

Index supported typed records and bounded UTF-8 text, code and data carried by readable archive descriptors. Preserve the original text and byte ranges for citations. EmbeddingGemma 2 places text, images, audio and sampled video in one vector space. Index image contents, audio and video ranges, and rendered PDF pages. Queries can use text, a source/code file, or a native media file. A media hit records its MIME type and a selector into the archived original. Other binary formats expose their descriptor metadata and links. Format-specific extractors can extend content coverage without changing archive IDs.

Each response identifies the snapshot, ranking mode and relevant index and model versions, and reports whether it completed within budget. Return ranked hits with IDs, kind and source metadata when available, and a field path or byte range for matched text. Generate optional snippets from re-resolved content and report incomplete coverage without exposing denied identifiers or counts. A continuation, if added, binds the snapshot, query and index generation so it cannot silently page a different ranking.

## Ownership and ranking

The archive owns immutable objects, snapshot membership and current access enforcement. The search module caches derived passages by source object and section and caches vectors by passage and model. Each snapshot keeps a view of the entries whose complete source dependency sets are readable within that snapshot. A later snapshot can reuse the same entries and embeddings. A source arrival schedules only the records waiting for that source.

Use the same eligible corpus for lexical and semantic ranking. The hybrid results put exact names, titles and codes first, followed by semantic results and lexical matches. Exact vectors are the reference when all comparisons fit. Larger requests project candidates onto normalized prefixes and compare the shortlist at full precision. Report the work and incomplete coverage.

Preparation and retrieval have separate entry points. Preparation resolves and verifies original media before embedding. Retrieval checks the currently allowed entries and returns their source selectors. Recovering an original performs full byte verification. A long-lived local search session keeps its model process warm across requests.

Use a local EmbeddingGemma 2 provider for both query and archive input. Bind the index to the exact weights, tokenizer, decoding implementation and runtime configuration. Validate all vectors and media selectors before persistence. An optional local worker loads the operator's checkpoint offline and receives only approved archive inputs over private process pipes.

## Multilingual coverage

Accept Unicode queries and source text across languages, including mixed-language content. Support same-language and cross-language retrieval without requiring users to specify a language or translate their queries. Lexical ranking must account for different writing systems and word boundaries. Semantic ranking must retrieve and rank relevant passages across languages. Keep snippets in the source language and report language coverage limits when known.

Before model selection, freeze a target set of widely used languages with documented global and regional usage criteria, covering multiple language families and writing systems. The set defines the evaluation scope. Build and judge queries with passages originally written in the target languages, including same-language paraphrases, cross-language matches and mixed-language distractors. Exercise every target language as both query and source, with cross-language cases between several language pairs, including pairs that do not involve English.

## Permission and lifecycle rules

Apply current read permission and snapshot membership before ranking. Denied, erased, missing and out-of-snapshot objects contribute neither hits nor ranking statistics. Resolve authorized sources again before returning excerpts. A derived segment needs current access to every source used in its extraction. A historical head still resolves content under the archive's current custody rules.

Exercise indexing through existing deposit, import and withdrawal operations. A local payload withdrawal removes the associated derived text, vectors and postings from search. Validate purge behavior for the chosen storage layout, including index internals that retain deleted entries. Imported or partially available snapshots expose the searchable subset with coverage recorded. Make rebuild and resumption idempotent. Compute expensive extraction and embeddings outside SQLite write transactions and recheck source eligibility before committing their index entries.

## Delivery sequence

1. **Fix the evaluation set and contract.** Build synthetic archives and judged queries covering exact IDs, metadata, paraphrases, calibration reports, text passages, corrections, historical heads and withheld data. Freeze the target languages, relevance judgments and minimum retrieval quality before tuning ranking. Define the response shape and coverage states.
2. **Deliver lexical retrieval and source navigation.** Connect the library and CLI to canonical archive reads. Add indexing and invalidation, explicit budgets and stable ordering. Validate results against the existing archived identifiers and permissions.
3. **Deliver multilingual and multimodal retrieval.** Evaluate the selected EmbeddingGemma 2 configuration with lexical, semantic and hybrid search. Exercise text and example-file queries across modalities, including audio/video ranges, image results and PDF pages.
4. **Validate archive operations and scale.** Test rebuild and restart, incremental indexing, snapshot isolation, withdrawal and partially imported bundles. Profile realistic large local corpora before adding an approximate vector backend or changing index placement.

## Acceptance

- Report Recall@10 and nDCG@10 by language for same-language retrieval and by tested pair and direction for cross-language retrieval. Compare lexical, semantic and hybrid ranking on the same eligible corpus. Count a language toward coverage only if it meets the predeclared quality floor for same-language retrieval and for cross-language retrieval as both query and source. The proposed release threshold is at least 90% of the frozen target languages. Report which languages and directions missed the quality floor.
- Hybrid should improve paraphrase retrieval over lexical. Exact identifier lookup continues to return the correct permitted object.
- Every hit resolves to its cited record or passage. Fixtures with unreadable, withdrawn and out-of-snapshot content return zero such hits. Adding excluded content leaves visible results and scoring unchanged. Cover both native encodings, partial imports, rebuild after restart, a withdrawn chunk in a searchable descriptor and missing model configuration.
- Measure cold index build time, warm query median and p95, memory and index disk size at multiple corpus sizes. Establish operating limits from those measurements. Track query budget exhaustion separately from an empty completed search.

Operating limits beyond the documented per-artifact and per-query budgets depend on corpus and host measurements. The first version's success is a local search flow with checkable references and measured retrieval quality.

## Reproducing the model checks

Use the EmbeddingGemma 2 checkpoint and Python environment described in the
[README](../README.md#search-by-content-or-by-example). The release-profile
language check uses a fixed synthetic archive with 26 languages and 52 judged
queries. Each language appears in same-language retrieval and in both query
and source roles for cross-language retrieval. Seven unmatched queries test
false positives.

```sh
cargo test --release --locked --test search_multilingual -- --ignored --nocapture
cargo test --release --locked --test search_media -- --ignored --nocapture
```

For the media check, set `NAOME_MEDIA_FIXTURES` to a directory containing the
[Golden Gate image](https://raw.githubusercontent.com/google-gemma/cookbook/refs/heads/main/apps/sample-data/GoldenGate.png)
as `bridge.png`, the [speech recording](https://gemma.ai/assets/capabilities/roses-are.wav)
as `roses.wav`, and the [cat clip](https://github.com/bebechien/gemma/raw/refs/heads/main/videos/cat_dozing.mp4)
as `cat.mp4`. Create `nitrate.pdf` using Pillow in that directory.

```python
from PIL import Image, ImageDraw

page = Image.new("RGB", (500, 400), "#eef4ee")
draw = ImageDraw.Draw(page)
draw.text((30, 30), "Hydrological monitoring report: river nitrate concentration", fill="black")
draw.line([(40, 300), (140, 260), (240, 190), (340, 160), (440, 70)], fill="blue", width=4)
page.save("nitrate.pdf")
```

The media check exercises text-to-artifact and file-to-artifact retrieval,
validates each result selector, and reconstructs the archived original byte
for byte. These trials measure behavior on their fixed corpora. Throughput and
retrieval quality at production archive sizes remain dependent on the corpus
and host.
