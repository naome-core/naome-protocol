# Semantic archive search

## Outcome

An investigator can ask a natural-language question about a named archive snapshot and find the records or passage references worth opening. For example, “Which calibration evidence changed the displacement estimate?” can lead to a claim, a correction and its supporting report even when the query uses different wording. Each hit points to exact archived bytes or a field in a typed record. `show`, `resolve`, `explain` and `assess` provide the follow-through from retrieval to scientific evaluation.

## Baseline

[`Archive::discover`](../src/store.rs) pages through a snapshot in ascending identifier order and filters typed records by kind, subject and scope. It uses the archive's read permission checks and skips blobs. [The CLI](../src/cli.rs) exposes it as `rep discover` with the existing `search` alias. The same commands are embedded by `naome archive`. [The protocol specification](../spec/research-evidence-protocol.tex) describes learned similarity, explicit search coverage and reproducible evidence references.

## Proposed first version

Offer a separate ranked entry point, provisionally `rep find HEAD QUERY`, backed by one library query used by archive clients. Accept optional kind, subject and scope filters, a result limit and explicit work budgets. Provide lexical, semantic and hybrid ranking modes. Route exact content identifiers to the archive's verified lookup. Existing structured inventory queries use `discover` and its `search` alias.

Start with supported typed records and bounded UTF-8 text carried by readable archive descriptors. Extract a documented, versioned set of text fields for supported profiles. Segments of text payloads carry byte ranges. Treat unsupported profiles and other media as coverage gaps when they limit the requested search. Structured metadata can still help a user find their descriptors. Later slices can add profile-specific extractors, OCR, non-text modalities and multi-archive queries.

Each response identifies the snapshot, ranking mode and relevant index and model versions, and reports whether it completed within budget. Return ranked hits with IDs, kind and source metadata when available, and a field path or byte range for matched text. Generate optional snippets from re-resolved content and report incomplete coverage without exposing denied identifiers or counts. A continuation, if added, binds the snapshot, query and index generation so it cannot silently page a different ranking.

## Ownership and ranking

The archive owns immutable objects, snapshot membership and current access enforcement. A small search module owns deterministic text extraction, ranking and a rebuildable local index. Store index data under the same local custody as its sources, with a schema or generation identifier. Keep search state outside the content-addressed record and exchange formats. Every entry carries the source IDs actually used to extract its text. Typed fields can be resolved through their record and profile. Payload passages also track the descriptor and chunk IDs they used.

Use the same eligible corpus for lexical and semantic ranking. Evaluate SQLite FTS5 for lexical candidate lookup and verify that ranking statistics cannot incorporate out-of-snapshot or unreadable records. The exact vector baseline scores eligible embeddings locally. Use a bounded, deterministic rank fusion for hybrid results and a stable identifier and segment tie-break. Record ranking parameters and distinguish budgeted results from a complete scan. Test exact ranking before considering an approximate vector index.

Define a narrow local embedding interface for indexed text and queries. Pin model identity, output dimension, normalization and extraction version for each generation. Reject incompatible or invalid vectors. Compare candidate local models on the same evaluation set before choosing a default. If a requested semantic mode lacks its configured model or coverage, report that explicitly. Lexical search is available as its own mode. The first version runs offline with operator-supplied model files.

## Permission and lifecycle rules

Apply current read permission and snapshot membership before ranking. Denied, erased, missing and out-of-snapshot objects contribute neither hits nor ranking statistics. Resolve authorized sources again before returning excerpts. A derived segment needs current access to every source used in its extraction. A historical head still resolves content under the archive's current custody rules.

Exercise indexing through existing deposit, import and withdrawal operations. A local payload withdrawal removes the associated derived text, vectors and postings from search. Validate purge behavior for the chosen storage layout, including index internals that retain deleted entries. Imported or partially available snapshots expose the searchable subset with coverage recorded. Make rebuild and resumption idempotent. Compute expensive extraction and embeddings outside SQLite write transactions and recheck source eligibility before committing their index entries.

## Delivery sequence

1. **Fix the evaluation set and contract.** Build synthetic archives and judged queries covering exact IDs, metadata, paraphrases, calibration reports, text passages, corrections, historical heads and withheld data. Freeze the judgments before tuning ranking. Define the response shape and coverage states.
2. **Deliver lexical retrieval and source navigation.** Connect the library and CLI to canonical archive reads. Add indexing and invalidation, explicit budgets and stable ordering. Validate results against the existing archived identifiers and permissions.
3. **Add semantic and hybrid retrieval.** Evaluate local models with the same judgments, select a versioned configuration and implement reproducible generation and rank fusion. Keep a lexical comparison and a semantic-only comparison.
4. **Validate archive operations and scale.** Test rebuild and restart, incremental indexing, snapshot isolation, withdrawal and partially imported bundles. Profile realistic large local corpora before adding an approximate vector backend or changing index placement.

## Acceptance

- The judged set includes English and German queries, exact terms and paraphrases. Report Recall@10 and nDCG@10 for structured discovery where applicable, lexical, semantic and hybrid ranking on the same eligible corpus. Hybrid should improve paraphrase retrieval over lexical, while exact identifier lookup continues to return the correct permitted object.
- Every hit resolves to its cited record or passage. Fixtures with unreadable, withdrawn and out-of-snapshot content return zero such hits. Adding excluded content leaves visible results and scoring unchanged. Cover both native encodings, partial imports, rebuild after restart, a withdrawn chunk in a searchable descriptor and missing model configuration.
- Measure cold index build time, warm query median and p95, memory and index disk size at multiple corpus sizes. Establish operating limits from those measurements. Track query budget exhaustion separately from an empty completed search.

Model choice, maximum supported corpus size and performance targets are decisions for implementation after baseline measurements. The first version's success is a local search flow with checkable references and measured retrieval quality.
