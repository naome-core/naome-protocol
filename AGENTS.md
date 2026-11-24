# Working on NAOME Protocol

This repository contains the Rust archive library, the `nao` CLI, the protocol whitepaper and its interoperable examples. Write repository content in English.

## Documentation

- Keep `README.md` to the protocol’s purpose, installation, one working first-use example and pointers to the specification and CLI help. Keep version-specific results, benchmarks, release notes and operational procedures out of the README. Edit it when a change makes those instructions inaccurate or changes what a new user needs to do.
- Put command options and defaults in CLI help. Put wire formats, compatibility rules and evidence semantics in the existing specification.
- Keep the whitepaper a standalone research paper about the protocol. Include the problem, model, assumptions, analytical properties, worked examples, related work, limitations and proposed independent evaluation where relevant. Keep benchmark tables, machine-specific measurements, local experiment reports, CI results, release history and troubleshooting out of the paper. Present worked examples and proposed studies as such.
- Regenerate the checked-in PDF with `python3 tools/ci/whitepaper.py --write` when the TeX changes.
- Do not add standalone Markdown documentation, implementation notes, handoffs, test reports, benchmark results, logs, sample databases or other run artifacts to the repository or release packages. Put run results in chat, CI logs or an explicitly requested external research record.
- Keep test fixtures to the minimum bytes that an automated test actually needs. Generate disposable inputs during the test. Check in fixed inputs or expected bytes only when the test depends on that exact historical, compatibility or reference vector. Keep test-only fixtures out of release downloads.

## Implementation and verification

- Preserve content identifiers, canonical bytes, access checks and historical evaluator behavior. Exercise old inputs when changing storage or exchange.
- Test through the public library or CLI path affected by the change. Cover interruption and retry for changes to admission or replication.
- Use the pinned Rust toolchain and locked dependencies. The workspace CI runs formatting, Clippy, feature and platform tests. For packaging changes, inspect the built Cargo package and the unpacked native download.
- Distinguish measured resource use from modeled capacity. State the inputs, platform and units for performance comparisons.
