# NAOME Research Evidence Protocol

NAOME Protocol records the evidence and assumptions behind scientific claims. A recipient can assess a claim against a specific archive snapshot and revisit that assessment when a premise changes. The Rust library and its command-line tool, `nao`, store and exchange content-addressed records.

## Install

Download the archive and matching SHA-256 file for your platform from [Releases](https://github.com/naome-core/naome-protocol/releases). Verify the checksum, extract the archive and add its directory to your PATH. To install from a source checkout, run `cargo install --locked --path .` with Rust and a C toolchain available.

## Exchange a reading

In an empty directory, create a text file named `reading.txt` containing the word `reading`. Capture it in a local archive and take a snapshot.

```sh
nao capture reading.txt text/plain
nao snapshot readings
```

Replace `SNAPSHOT_ID` below with the identifier printed by the second command. Export the snapshot, import it into a separate archive and search the received evidence.

```sh
nao export SNAPSHOT_ID exchange.json
nao --db receiver.sqlite import exchange.json
nao --db receiver.sqlite find reading --head SNAPSHOT_ID
```

The search result names the snapshot and the captured record. Run `nao --help` to list commands or `nao COMMAND --help` for a command’s options.

The [whitepaper](spec/research-evidence-protocol.pdf) describes claim evaluation, archive manifests, transfer and access rules. A [complete exchange file](spec/examples/exchange.json) and [format A](spec/kits/A.json) and [format B](spec/kits/B.json) bootstrap kits provide machine-readable examples.
