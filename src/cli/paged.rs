//! Bounded custody commands for independently pinned lab lineages.
use super::*;
use crate::store::paged::{BATCH_BYTES, BLOCK, Federation, Item, MEMBERS, Member, Position};
use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Subcommand)]
pub enum Command {
    /// Admit up to 64 members from a JSON array of {id,access,position,file}.
    Append {
        source: String,
        manifest: PathBuf,
        #[arg(long)]
        parent: Option<Id>,
    },
    /// Stream a file into fixed blocks. The stream ID names a caller-defined observation.
    Capture {
        source: String,
        stream: Id,
        file: PathBuf,
        #[arg(long)]
        parent: Option<Id>,
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
    },
    Resolve {
        head: Id,
        id: Id,
        output: PathBuf,
        #[arg(long, default_value = "read")]
        purpose: String,
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
    mut parent: Option<Id>,
    stream: Id,
    file: &Path,
    access: Id,
) -> Result<()> {
    let mut reader = BufReader::new(File::open(file)?);
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    let initial = parent.context("capture requires a parent containing the stream descriptor")?;
    db.paged_resolve(initial, stream, "read")?;
    let mut block = 0_u64;
    let mut items = Vec::new();
    loop {
        let mut data = Vec::new();
        (&mut reader).take(BLOCK as u64).read_to_end(&mut data)?;
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
        // Leave room for the default access descriptors on the first batch.
        if !items.is_empty() && (eof || items.len() == BATCH_BYTES / BLOCK - 1) {
            let admitted = db.paged_append(source, parent, &items)?;
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
            if let Ok(db) = Archive::paged_reader(loc.db) {
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
            db.init_paged()?;
            let receipt = db.paged_save_receipt(&report)?;
            return print(json!({"receipt":receipt,"report":report}));
        }
        return print(report);
    }
    let db = Archive::open(db_path)?;
    db.init_paged()?;
    match command {
        Command::Review { .. } => unreachable!(),
        Command::Append {
            source,
            parent,
            manifest,
        } => {
            let specs: Vec<Input> = serde_json::from_slice(&load(&manifest, 64 * 1024)?)?;
            ensure!((1..=MEMBERS).contains(&specs.len()), "invalid batch size");
            let mut remaining = BATCH_BYTES;
            let mut items = Vec::new();
            let directory = manifest.parent().context("manifest path")?;
            for spec in specs {
                let bytes = load(&directory.join(spec.file), remaining.min(1024 * 1024))?;
                remaining -= bytes.len();
                items.push(Item {
                    member: Member {
                        id: spec.id,
                        access: spec.access,
                        position: spec.position,
                    },
                    bytes,
                });
            }
            print(db.paged_append(&source, parent, &items)?)
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
        } => print(publish(&output, |f| db.paged_export_step(head, after, f))?),
        Command::ImportStep { head, file } => {
            print(db.paged_import_step(head, BufReader::new(File::open(file)?))?)
        }
        Command::Tips {
            source,
            after,
            limit,
        } => print(db.paged_tips(&source, after, limit)?),
        Command::Receipt { id } => print(serde_json::from_slice::<serde_json::Value>(
            &db.paged_receipt(id)?,
        )?),
        Command::Head { head } => print(db.paged_head(head)?),
        Command::Share { head } => db.paged_share(head, true),
        Command::Retract { head } => db.paged_share(head, false),
        Command::Pull {
            origin,
            head,
            batches,
        } => {
            ensure!(origin.is_file(), "missing origin archive");
            print(db.paged_pull(&Archive::paged_reader(origin)?, head, batches)?)
        }
        Command::Resolve {
            head,
            id,
            output: path,
            purpose,
        } => output(&path, &db.paged_resolve(head, id, &purpose)?),
        Command::Range {
            head,
            stream,
            offset,
            length,
            output: path,
        } => output(&path, &db.paged_range(head, stream, offset, length)?),
    }
}
