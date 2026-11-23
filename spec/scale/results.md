# Paged custody qualification results

## Reproduction

Acceptance criteria were committed at 12f572a before measurement. The full-page implementation, its measurements and the follow-up physical-storage criteria were committed at f2ecdfb. The follow-up preserves the logical pages, head IDs and transfer format. From this revision, run:

    cargo test --locked --lib store::paged::tests
    python3 tools/scale/qualify.py new-samples.json
    python3 tools/scale/qualify.py spec/scale/local-samples.json --verify-saved

The driver compiles an optimized Rust library test and measures each phase in a fresh process with wait4 on macOS or Linux. Each trial uses fresh on-disk SQLite files. Seed, first pull, one-record delta and delta pull run in order. Each layout has three repetitions of each configuration and 48 observations. The comparison checks head IDs, logical counters, receipt sizes, transfer bytes, fork IDs and deterministic results. Both layouts ran sequentially on one macOS 27.3 arm64 host using rustc 1.97.1. OS caches were uncontrolled. Ranges below are the observed minimum and maximum. Logical reads are B+ tree reads. Backing reads are SQLite row fetches for reconstructing pages. Physical device reads were unmeasured. Wire bytes include all actual frame lengths and ancestry proofs.

## Inventory-only experiment

The generator produced ID-shaped keys in batches of 256 and used the real tree code, page serialization, hashing and SQLite storage. Payloads for these hypothetical IDs were never generated or read. Each size received the same new key. The old root continued to exclude it.

| Indexed keys | Path height | Logical reads | New pages | Backing reads | Logical bytes read / created | New backing bytes |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 2 | 2 | 2 | 2 | 4,256 / 4,402 | 284 |
| 10,000 | 3 | 3 | 3 | 11 | 9,670 / 9,816 | 7,423 |
| 1,000,000 | 5 | 5 | 6 | 14 | 13,636 / 13,938 | 2,886 |

All three met the preregistered H-read and 2H+1-write bounds. The million-key inventory actually grew the database by 183,644,160 bytes and had median process peak RSS of 7.61 MiB.

## Physical records and portable frames

Origin batches contained 64 individually encoded canonical occurrences. The same 203-byte claim declared an evidence link to the same first occurrence at every size. The delta process admits it, checks the old membership, resolves it on the new head, queries reverse dependencies and sizes a receipt. Each compact receipt occupied 465 bytes. The receiver independently decoded the actual portable frame and rederived the same head.

| Records before delta | Admission logical reads / new pages | Admission backing reads | Delta file growth | Frame including payload | Delta process median (min–max), ms | Median peak RSS, MiB | Receiver median, ms |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 128 | 16 / 8 | 32 | 12,288 B | 824 B | 6.43 (6.16–6.47) | 5.39 | 5.67 |
| 1,024 | 25 / 12 | 86 | 28,672 B | 1,038 B | 8.47 (8.12–8.84) | 6.84 | 8.14 |
| 12,800 | 34 / 18 | 121 | 49,152 B | 1,252 B | 10.74 (10.01–10.91) | 7.69 | 9.59 |

Member resolution read 4, 6 and 8 logical pages, and reverse review read 6, 9 and 12. The largest incremental receiver peaked at a median 8.11 MiB. A duplicate completed local pull transferred zero frames.

| First admission | Origin file growth, bytes | Payload bytes | First-pull frame bytes | Origin / receiver median, ms | Origin / receiver median peak RSS, MiB |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 128 | 331,776 | 17,298 | 52,194 | 17.79 / 28.69 | 6.03 / 6.17 |
| 1,024 | 7,122,944 | 139,178 | 703,444 | 264.12 / 481.86 | 8.34 / 10.05 |
| 12,800 | 155,996,160 | 1,755,290 | 12,935,140 | 10,250.81 / 14,293.96 | 8.36 / 11.09 |

The largest origin created 57,962 immutable logical pages. Its 116,526 logical reads required 348,447 backing reads returning 777,237,336 bytes. New backing-page data occupied 97,348,011 bytes before SQLite layout and other tables. The first receiver replayed 200 commits. Ancestry proof and frame metadata contributed 11,179,850 bytes.

## Physical history tradeoff

Preserving complete physical pages grew the largest origin by 358,772,736 bytes. Bounded splices lowered growth by 56.5% to 155,996,160. Median origin time rose from 8.74 to 10.25 s and cold pull from 11.30 to 14.29 s. The fixed delta grew the file by 49,152 bytes, down from 118,784. Across both layouts the logical roots and wire bytes matched. Every counted phase stayed within five backing reads per logical read. Approximately 89 database bytes per initial payload byte in this tiny-record, history-retaining workload is a material remaining cost.

## Concurrency, coverage and replay

Two actual threads owned independent writer shards. Each submitted twelve mixed record/blob batches and announced a tip after each three-batch burst. A queue held two tip notifications. One receiving worker reopened its per-source replica after each committed step and paused for 3 ms. It caught up after 24 batches, 1,595,726 payload bytes and 37,790 frame metadata bytes. The largest batch carried 262,329 payload bytes. Each run encountered a full queue five times. Eight duplicate announcements transferred zero further members. The two-tip view found the expected stream and occurrence. The scheduling experiment used one host and local files.

Tests cover sibling tips, cross-lab premises, direct and transitive routing, an independent control, missing sources and pages, insufficient budgets, restrictions and withdrawal. They interrupt transfers, replay frames, retract a grant between steps and retain historical receipts after a withdrawal. A correctly hashed descriptor that omits the index changes for a notice fails independent reconstruction; staged members and the tip roll back. The native package smoke uses ordinary CLI commands for capture, frame and direct pulls, range reading and receipt retrieval.

The first hosted Windows debug run exposed stack exhaustion during eager CLI construction, including standalone commands. The paged command group now initializes on demand. With a 1 MiB main-stack limit on macOS the debug executable passed version, capabilities, paged help, capture, both transfer paths, range and receipt checks. The custody algorithms and measured data paths are unchanged.

Historical scientific replay reproduced the rules-2, rules-3, rules-4 and rules-5 training manifests. The current trial matched the independent rules-6 writer control across eight conformance files and all 24 graded answers. A structural review supplies candidates for scientific assessment. Its closed search covers declared relationships in the named sources.

## Capacity model and remaining limits

Three TiB of raw data use 12,582,912 positions at 256 KiB per block. At 15 blocks per capture batch this takes 838,861 data commits. At 10 decimal Gbit/s, the payload alone takes about 2,639 seconds before storage, proofs and contention. These figures are arithmetic rather than observed throughput.

Two example labs producing 800 MiB/s each for ten seconds against a 600 MiB/s receiver create a 10,000 MiB backlog. If aggregate arrivals then fall to 200 MiB/s, ideal drain time is 25 seconds. In this model committed origin history holds the backlog. Sustained production above receiver capacity grows the durable backlog even with bounded RAM. Field-record amplification, compaction, multi-terabyte recovery, WAN transport and physical multi-lab throughput remain unqualified. The v1 scientific evaluator, search and network release commands retain their existing limits.
