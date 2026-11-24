# Working on NAOME Protocol

This repository contains the Rust archive library, the `nao` CLI, the protocol whitepaper and its interoperable examples. Write repository content in English.

## Documentation

- Keep `README.md` focused on installation, first use and navigation. Edit it when a change makes those instructions inaccurate or changes what a new user needs to do.
- Put command options and defaults in CLI help. Put wire formats, compatibility rules and evidence semantics in the existing specification. Regenerate the checked-in PDF with `python3 tools/ci/whitepaper.py --write` when the TeX changes.
- Record run-specific timings, test outcomes and investigation notes in chat, CI logs or an explicitly requested research destination. Keep generated reports, sample databases and scratch files outside tracked sources and release packages. Keep reusable qualification code and required protocol vectors beside their existing counterparts.

## Implementation and verification

- Preserve content identifiers, canonical bytes, access checks and historical evaluator behavior. Exercise old inputs when changing storage or exchange.
- Test through the public library or CLI path affected by the change. Cover interruption and retry for changes to admission or replication.
- Use the pinned Rust toolchain and locked dependencies. The workspace CI runs formatting, Clippy, feature and platform tests. For packaging changes, inspect the built Cargo package and the unpacked native download.
- Distinguish measured resource use from modeled capacity. State the inputs, platform and units for performance comparisons.
