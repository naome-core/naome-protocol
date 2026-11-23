# Paged custody qualification

## Preregistered method

Baseline is `4f0c3d78f85b30883ef7f65c3df7b53a33bf6e40`. The baseline snapshot seals a sorted JSON set. JSON exchange pages repeat this set, the stream writes it in one bounded header, receipts deposit it as one blob, and a replication release accumulates its members before commit. The evaluation inventory and reverse correction scan may visit every record in the selected flat view.

The proposed paged path uses bounded content-addressed B+ tree pages, immutable per-lab delta heads and explicit admission of a set of tips. Membership, declared reverse edges and raw stream positions share the same persistent tree. A receiver must derive its keys from verified input and compare the resulting root. An unverified index cannot support an absence claim.

Freeze the following checks before collecting results.

- Insert the same small delta into generated trees at three or more sizes spanning at least two orders of magnitude. Count every logical page read, page read byte, new immutable page and byte written by the tree code. For one inserted key at height H require at most H page reads and 2H+1 new pages, including splits. Measure a first admission separately. Verify old roots still resolve their prior values.
- Use real records through the shipping custody API and SQLite with a fixed seed, freshly created files and bounded batches. Compare tagged root and member reads, delta cost, disk growth, elapsed wall time and process peak resident memory. Report OS and build mode. Separate one-time setup, transfers and retained history. Record logical I/O separately from SQLite and OS page-cache activity.
- Compare a first pull and a small incremental pull. Limit in-flight work by batch bytes and members. Interrupt after a committed step, reopen, repeat delivery and finish. A partial or invalid batch must never expose a new head. A lagging receiver must catch up under a finite service budget.
- Exercise two writers on independent files, a burst, unequal service rates, mixed record and raw-block sizes, a fork at one parent and a combined view naming both tips. Audit identity, access restrictions, withdrawals, incomplete evidence, and direct and transitive correction exposure. Missing coverage must produce an open result.
- Replay archived scientific receipts on their existing rules. Verify the release package, paper and legacy wire fixtures. Physical multi-terabyte throughput, WAN transport, failure rates and production concurrency require independent deployment measurements.

These are engineering acceptance criteria chosen for the experiment. Synthetic counts or virtual payload sizes do not qualify as physical bytes processed.

## Follow-up on physical history cost

The first paged run preserved each changed logical page as a full JSON blob. Across three independent 12,800-record runs the origin grew by 358,772,736 bytes for 1,755,290 bytes of input records. This observation motivates a second, predeclared comparison. Keep these runs in `full-page-samples.json`. Preserve every logical page ID, head, wire byte and routing result. Store an insertion or replacement against a retained prior page only when it saves bytes. Bound the physical reconstruction chain to four deltas and record all backing-page reads and bytes separately from logical I/O. Rerun the original workload unchanged, report the full storage and time tradeoff and verify identical logical roots. The follow-up criterion is a lower actual file size at 12,800 records with no more than five backing-page reads per logical read. No runtime improvement is assumed.
