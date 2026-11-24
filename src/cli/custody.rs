//! Bounded custody commands for independently pinned lab lineages.
use super::*;
use crate::store::custody::{BATCH_BYTES, BLOCK, Federation, Item, MEMBERS, Member, Position};
use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeSet;

// Build this command group after the top-level builder returns, including in debug builds
// on hosts with a 1 MiB main-thread stack.
pub struct Arguments(pub Command);
impl clap::Args for Arguments {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        cmd.subcommand_required(true)
            .defer(Command::augment_subcommands)
    }
    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        cmd.defer(Command::augment_subcommands_for_update)
    }
}
impl clap::FromArgMatches for Arguments {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        Ok(Self(Command::from_arg_matches(matches)?))
    }
    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(matches)
    }
}
#[derive(Subcommand)]
pub enum Command {
    /// Admit up to 64 members from a JSON array of {id,access,position,file}.
    Append {
        source: String,
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        manifest: Option<PathBuf>,
        /// Admit one file. Use --record-format for an encoded scientific record.
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, requires = "file")]
        record_format: Option<Format>,
        #[arg(long, requires = "file")]
        expected: Option<Id>,
        #[arg(long, requires = "file")]
        access: Option<Id>,
        #[arg(long)]
        parent: Option<Id>,
    },
    /// Append a file to a named observation in committed batches for range reads.
    Capture {
        source: String,
        stream: Id,
        file: PathBuf,
        #[arg(long)]
        parent: Id,
        #[arg(long)]
        access: Option<Id>,
    },
    /// Export the next portable delta and its verified path to the selected tip.
    ExportStep {
        head: Id,
        output: PathBuf,
        #[arg(long)]
        after: Option<Id>,
    },
    /// Atomically admit one portable delta for an independently pinned target.
    ImportStep { head: Id, file: PathBuf },
    /// List private branch tips for one local source.
    Tips {
        source: String,
        #[arg(long)]
        after: Option<Id>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Inspect a locally retained structural review receipt.
    Receipt { id: Id },
    /// Inspect the immutable tip descriptor and its bounded delta.
    Head { head: Id },
    /// Permit replication of this exact tip, including its verified ancestry.
    Share { head: Id },
    /// Retract local permission for further pulls of this tip.
    Retract { head: Id },
    /// Advance by a bounded number of verified deltas from a local custody file.
    Pull {
        origin: PathBuf,
        head: Id,
        #[arg(long, default_value_t = 1)]
        batches: usize,
        /// Pull to this exact target, committing each batch and retaining a resumable cursor.
        #[arg(long, conflicts_with = "batches")]
        until_complete: bool,
    },
    /// Read exactly the selected byte range into a new file.
    Range {
        head: Id,
        stream: Id,
        offset: u64,
        length: usize,
        output: PathBuf,
    },
    /// Trace declared dependents within a pinned federation specified as [{head,db}].
    Review {
        sources: PathBuf,
        /// Look for notices on the target and its premises.
        #[arg(long)]
        corrections: bool,
        /// Deposit a compact routing receipt in the selected local database.
        #[arg(long)]
        receipt: bool,
        target: Id,
        #[arg(long, default_value_t = 1000)]
        budget: usize,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    id: Id,
    access: Id,
    position: Option<Position>,
    file: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Locator {
    head: Id,
    db: PathBuf,
}

fn capture(
    db: &Archive,
    source: &str,
    initial: Id,
    stream: Id,
    file: &Path,
    access: Id,
) -> Result<()> {
    let mut reader = BufReader::new(File::open(file)?);
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    let mut parent = Some(initial);
    db.custody_resolve(initial, stream, "read")?;
    let mut block = 0_u64;
    let mut items = Vec::new();
    loop {
        let mut data = vec![0; BLOCK];
        let used = crate::payload::read_block(&mut reader, &mut data)?;
        data.truncate(used);
        let eof = data.len() < BLOCK;
        if !data.is_empty() {
            digest.update(&data);
            length = length
                .checked_add(data.len() as u64)
                .context("capture length overflow")?;
            let mut item = Item::blob(data, access);
            item.member.position = Some(Position { stream, block });
            items.push(item);
            block += 1;
        }
        if !items.is_empty() && (eof || items.len() == BATCH_BYTES / BLOCK) {
            let admitted = db.custody_append(source, parent, &items)?;
            parent = Some(admitted.head);
            items.clear();
        }
        if eof {
            ensure!(block != 0, "empty raw stream");
            return print(
                json!({"head":parent,"stream":stream,"length":length,"sha256":hex::encode(digest.finalize()),"blocks":block}),
            );
        }
    }
}
pub(super) fn run(db_path: &Path, command: Command) -> Result<()> {
    if let Command::Review {
        sources,
        corrections,
        receipt,
        target,
        budget,
    } = command
    {
        let locators: Vec<Locator> = serde_json::from_slice(&load(&sources, 64 * 1024)?)?;
        ensure!(
            (1..=64).contains(&locators.len()),
            "invalid federation size"
        );
        let heads: BTreeSet<_> = locators.iter().map(|l| l.head).collect();
        ensure!(heads.len() == locators.len(), "duplicate federation head");
        let federation = Federation::new(heads)?;
        let mut stores = Vec::new();
        for loc in locators {
            if let Ok(db) = Archive::custody_reader(loc.db) {
                stores.push((db, loc.head));
            }
        }
        let pins: Vec<_> = stores.iter().map(|(a, id)| (a, *id)).collect();
        let report = if corrections {
            federation.corrections(&pins, target, budget)?
        } else {
            federation.review(&pins, target, budget)?
        };
        if receipt {
            let db = Archive::open(db_path)?;
            let receipt = db.custody_save_receipt(&report)?;
            return print(json!({"receipt":receipt,"report":report}));
        }
        return print(report);
    }
    let db = match &command {
        Command::Head { .. }
        | Command::Receipt { .. }
        | Command::Tips { .. }
        | Command::Range { .. }
        | Command::ExportStep { .. } => Archive::custody_reader(db_path)?,
        _ => Archive::open(db_path)?,
    };
    match command {
        Command::Review { .. } => unreachable!(),
        Command::Append {
            source,
            parent,
            manifest,
            file,
            record_format,
            expected,
            access,
        } => {
            if let Some(path) = file {
                let bytes = load(&path, MAX_BYTES)?;
                let item = if let Some(format) =
                    record_format.or_else(|| expected.and_then(|id| id.format()))
                {
                    let record = Record::decode(&bytes, format)?;
                    ensure!(
                        access.is_none_or(|a| a == record.access),
                        "record access mismatch"
                    );
                    Item {
                        member: Member {
                            id: Id::record(format, &bytes),
                            access: record.access,
                            position: None,
                            absent: false,
                        },
                        bytes,
                    }
                } else {
                    Item::blob(bytes, access.unwrap_or_else(|| Id::blob(PUBLIC)))
                };
                ensure!(
                    expected.is_none_or(|id| id == item.member.id),
                    "unexpected member identifier"
                );
                return print(db.custody_append(&source, parent, &[item])?);
            }
            let manifest = manifest.context("missing batch manifest")?;
            let specs: Vec<Input> = serde_json::from_slice(&load(&manifest, 64 * 1024)?)?;
            ensure!((1..=MEMBERS).contains(&specs.len()), "invalid batch size");
            let mut remaining = if specs.len() == 1 {
                crate::wire::MAX_BYTES
            } else {
                BATCH_BYTES
            };
            let mut items = Vec::new();
            let directory = manifest.parent().context("manifest path")?;
            for spec in specs {
                let bytes = load(&directory.join(spec.file), remaining)?;
                remaining -= bytes.len();
                items.push(Item {
                    member: Member {
                        id: spec.id,
                        access: spec.access,
                        position: spec.position,
                        absent: false,
                    },
                    bytes,
                });
            }
            print(db.custody_append(&source, parent, &items)?)
        }
        Command::Capture {
            source,
            parent,
            stream,
            file,
            access,
        } => capture(
            &db,
            &source,
            parent,
            stream,
            &file,
            access.unwrap_or_else(|| Id::blob(PUBLIC)),
        ),
        Command::ExportStep {
            head,
            after,
            output,
        } => print(publish(&output, |f| {
            db.custody_export_step(head, after, f)
        })?),
        Command::ImportStep { head, file } => {
            print(db.custody_import_step(head, BufReader::new(File::open(file)?))?)
        }
        Command::Tips {
            source,
            after,
            limit,
        } => print(db.custody_tips(&source, after, limit)?),
        Command::Receipt { id } => print(serde_json::from_slice::<serde_json::Value>(
            &db.custody_receipt(id)?,
        )?),
        Command::Head { head } => print(db.custody_head(head)?),
        Command::Share { head } => db.custody_share(head, true),
        Command::Retract { head } => db.custody_share(head, false),
        Command::Pull {
            origin,
            head,
            batches,
            until_complete,
        } => {
            ensure!(origin.is_file(), "missing origin archive");
            let origin = Archive::custody_reader(origin)?;
            print(if until_complete {
                db.custody_pull_until_complete(&origin, head)?
            } else {
                db.custody_pull(&origin, head, batches)?
            })
        }
        Command::Range {
            head,
            stream,
            offset,
            length,
            output: path,
        } => output(&path, &db.custody_range(head, stream, offset, length)?),
    }
}
