# nao

`nao` is the command-line tool in the `naome-protocol` Rust package. It stores, exchanges, and evaluates research evidence. It uses local SQLite archives. Object identifiers are derived from content. Evaluation uses the selected snapshot and the supplied policy.

## Install

On Windows x64, install or upgrade through the WinGet community source.

```powershell
winget install --id NAOME.nao --exact --source winget
winget upgrade --id NAOME.nao --exact --source winget
```

Download the archive for Windows x64, Apple silicon macOS, or Linux x64 (glibc 2.35 or later) from [Releases](https://github.com/naome-core/naome-protocol/releases). Verify it using the matching SHA-256 file, then extract it. Run the executable by its path or add its directory to your PATH. On Windows, use `.\nao.exe` from the extracted directory.

To build from a checkout, run `cargo install --locked --path .` with Rust and a C toolchain for SQLite. For Rust library use, set `default-features = false` to omit the CLI and the bundled model integration.

## Use an archive

In a new directory, create a file named `reading.txt` containing the word `reading`, then run these commands.

```sh
nao capture reading.txt text/plain
nao snapshot readings
nao find reading
```

The default database is `evidence.sqlite`. Use `--db FILE` to choose another file. Existing archives and exchange files can be used directly. `nao snapshot` prints the snapshot ID. Use that ID to export the snapshot, then import it into another archive.

```sh
nao export SNAPSHOT_ID exchange.json
nao --db receiver.sqlite import exchange.json
```

Add `--stream` to both commands for streaming exchange. Use `nao --help` and `nao COMMAND --help` for command options. Keep archive files and signing keys in a private directory.

## Search with a local model

Keyword search runs in the CLI. Semantic and hybrid search use a local EmbeddingGemma 2 checkpoint and Python 3.12. Activate a Python 3.12 virtual environment before running these commands. On Windows and Linux, install the CPU PyTorch packages first.

```sh
python -m pip install --index-url https://download.pytorch.org/whl/cpu -r requirements-search-torch.txt
```

Then run the following on all three platforms.

```sh
python -m pip install -r requirements-search.txt
hf download google/embeddinggemma-2 --revision f9972ac22e696a105c482202646979e33e4b5a1e --local-dir model
nao find reading --prepare --model model --python python
```

Once set up, inference runs offline. Use `--input FILE` for a query from a supported media or text file.

## Protocol reference

`nao capabilities` reports the supported formats and bootstrap digests.

- [Whitepaper](spec/research-evidence-protocol.pdf)
- [Bootstrap kit A](spec/kits/A.json) and [bootstrap kit B](spec/kits/B.json)
- [Example exchange file](spec/examples/exchange-v1.json)
