mod custody;
mod research;
mod search;
#[cfg(windows)]
mod windows_key;
pub use search::{FindArguments, LocalSearch, PrepareSearchArguments, SearchRequest};

use crate::{
    evaluation, executor, fixture, kit, learning,
    profile::{self, Document, Kind, PUBLIC},
    store::{Archive, Bundle},
    wire::{Format, Id, MAX_BYTES, Record},
};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use ed25519_dalek::SigningKey;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

/// Construct argument-heavy subcommands after the top-level parser releases its stack frame.
pub struct DeferredArgs<T>(pub T);
impl<T: clap::Args + 'static> clap::Args for DeferredArgs<T> {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        cmd.defer(T::augment_args)
    }
    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        cmd.defer(T::augment_args_for_update)
    }
}
impl<T: clap::FromArgMatches> clap::FromArgMatches for DeferredArgs<T> {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        T::from_arg_matches(matches).map(Self)
    }
    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(matches)
    }
}
#[derive(Subcommand)]
pub enum ArchiveCommand {
    /// Append, replicate and review bounded archive changes.
    Custody(custody::Arguments),
    /// Show supported formats, evaluators and independently pinned kit digests.
    Capabilities,
    /// Maintenance: copy and upgrade an archive. Ordinary writable opens upgrade in place.
    MigrateArchive {
        source: PathBuf,
        destination: PathBuf,
    },
    /// Maintenance: look up the current identifier of a historical archive head.
    MigratedHead { head: Id },
    /// Run all cases in an externally pinned bootstrap package.
    Kit { file: PathBuf, sha256: String },
    /// Run the full synthetic two-archive trial in a fresh directory.
    Trial { directory: PathBuf },
    /// Run public conformance queries in a clean archive per exchange file.
    Suite { directory: PathBuf },
    /// Compare an external submission with the adjudicated answer key.
    Grade {
        answer_key: PathBuf,
        submission: PathBuf,
    },
    /// Create or open a custody database.
    Init,
    /// Show the active archive, full snapshot and count of deposited objects awaiting a snapshot.
    Status,
    /// Register a complete, readable inventory in the local replica release catalog.
    Publish { head: Id },
    /// List the complete inventories available for replication.
    Heads,
    /// Capture a complete file atomically as a digest-checked descriptor for recover.
    Capture(DeferredArgs<research::CaptureArguments>),
    /// Verify and recover a complete chunked payload into a new file.
    Recover {
        head: Id,
        descriptor: Id,
        output: PathBuf,
        #[arg(long)]
        bytes: u64,
    },
    /// Evaluate the finite-world expected reduction in decision loss for an observation.
    Value { model: PathBuf },
    /// Select an observational plan under an explicit objective and unit-specific ceilings.
    Select { head: Id, request: PathBuf },
    /// Execute a registered local payload-verification attempt and deposit its signed readback.
    Execute {
        head: Id,
        request: PathBuf,
        public_key: String,
        witness_key: PathBuf,
    },
    /// Deposit exact payload bytes.
    Blob {
        file: PathBuf,
        #[arg(long)]
        access: Option<Id>,
    },
    /// Deposit an opaque canonical wire record.
    Wire { file: PathBuf },
    /// Deposit a research-profile JSON document.
    Put {
        file: PathBuf,
        #[arg(long)]
        access: Option<Id>,
    },
    /// Seal the local inventory, or the readable dependencies of selected roots.
    Snapshot {
        #[arg(required_unless_present = "continue_tip")]
        source: Option<String>,
        #[arg(long)]
        parent: Vec<Id>,
        /// Continue the sole full local tip, inferring the source if omitted.
        #[arg(long = "continue", conflicts_with_all = ["parent", "root"])]
        continue_tip: bool,
        /// Select an object and its transitive inputs. Repeat for multiple roots.
        #[arg(long)]
        root: Vec<Id>,
    },
    /// Inspect typed research fields at a fixed snapshot.
    Show {
        head: Id,
        id: Id,
        #[arg(long, default_value = "read")]
        purpose: String,
    },
    /// Inspect a question and the evidence, claims and assessments linked to it.
    Question(DeferredArgs<research::QuestionArguments>),
    /// Write verified, permitted bytes. The output path must be new.
    Resolve {
        head: Id,
        id: Id,
        output: PathBuf,
        #[arg(long, default_value = "read")]
        purpose: String,
    },
    /// List typed records by kind, subject or scope within a bounded inventory page.
    #[command(visible_alias = "search")]
    Discover {
        head: Id,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        after: Option<Id>,
        #[arg(long, default_value_t = 100)]
        budget: usize,
    },
    /// Find authorized content by exact ID, words or meaning.
    Find(DeferredArgs<FindArguments>),
    /// Show or pin the active search snapshot.
    SearchHead {
        head: Option<Id>,
        /// Follow the newest full local snapshot when one exists.
        #[arg(long, conflicts_with = "head")]
        clear: bool,
    },
    /// Prepare archive text, media and embeddings independently of a search.
    PrepareSearch(DeferredArgs<PrepareSearchArguments>),
    /// Reuse a local model for JSON-line find and prepare-search requests on stdin.
    SearchSession,
    /// Trace required input edges and the unresolved frontier.
    Explain {
        head: Id,
        id: Id,
        /// Return a resumable record range. To combine pages, collect dependencies.visited
        /// across the scan and select each notice whose notice_targets intersect that set.
        #[arg(long)]
        compact: bool,
        /// Resume the compact record scan at the same snapshot.
        #[arg(long, requires = "compact")]
        after: Option<Id>,
        #[arg(long, default_value_t = 1000)]
        budget: usize,
        #[arg(long, default_value_t = 32)]
        depth: usize,
        #[arg(long, default_value = "read")]
        purpose: String,
    },
    /// Save an assessment receipt using at most 100,000 evaluation steps and 64 MiB.
    Assess { head: Id, query: PathBuf },
    /// Re-run the recorded evaluator and compare the result with the saved receipt.
    Replay(DeferredArgs<research::ReplayArguments>),
    /// Re-evaluate a saved query at the given snapshot and save a linked successor receipt.
    Reassess(DeferredArgs<research::ReassessArguments>),
    /// Serialize a bounded inventory with its permitted objects for offline exchange.
    Export {
        head: Id,
        /// Disclose selected readable objects with shared membership and access proofs. Repeat for multiple IDs.
        #[arg(long,conflicts_with_all=["after","limit","stream","paged"])]
        member: Vec<Id>,
        #[arg(long)]
        after: Option<Id>,
        #[arg(long)]
        limit: Option<usize>,
        /// Send the next read-only range with witnesses shared across acknowledged pages.
        #[arg(long, conflicts_with = "stream")]
        paged: bool,
        /// Write a page for v0.1.8 recipients, without custody access witnesses.
        #[arg(long, requires = "paged")]
        legacy_page: bool,
        /// Write the inventory once, followed by an ordered stream of packets.
        #[arg(long, conflicts_with_all = ["after", "limit"])]
        stream: bool,
        output: PathBuf,
        #[arg(long, default_value = "read")]
        purpose: String,
    },
    /// Import an exchange file into the archive.
    #[command(
        long_about = "Import an exchange file into the archive.\n\nFor independent senders, use --paged with an independently obtained --expected-head to verify membership and committed access policies. JSON bundles, --stream and --legacy-page rely on the sender for blob access policies."
    )]
    Import {
        file: PathBuf,
        /// Verify and stage the next range, then publish the inventory after the final range.
        #[arg(long, conflicts_with_all = ["stream", "member_head"])]
        paged: bool,
        /// Accept a v0.1.8 page without custody access witnesses from a trusted sender.
        #[arg(long, requires = "paged")]
        legacy_page: bool,
        /// Independently obtained snapshot ID for a paged read exchange.
        #[arg(long, requires = "paged")]
        expected_head: Option<Id>,
        /// Interpret a point disclosure against this independently obtained head.
        #[arg(long, conflicts_with = "stream")]
        member_head: Option<Id>,
        /// Maximum stream input in bytes, including metadata (default 256 MiB).
        #[arg(long, requires = "stream")]
        max_bytes: Option<u64>,
        /// Read a stream from a trusted sender that supplies the blob access policies.
        #[arg(long)]
        stream: bool,
    },
    /// Inspect verified and trusted paged import checkpoints for a snapshot.
    #[command(
        long_about = "Inspect paged import checkpoints for this snapshot without changing the archive. Supplies and omissions describe the last checkpoint through completion. Older imports may have unknown counts. Completion means the inventory was examined; omitted objects may be unavailable."
    )]
    ImportStatus { head: Id },
    /// Sign a minimal scoring-hull disclosure and write its packet to a new file.
    Project {
        head: Id,
        receipt: Id,
        key: PathBuf,
        output: PathBuf,
    },
    /// Produce an auditable native-format mapping and exchange inventory.
    Migrate { head: Id, issuer: String },
    /// Check endpoints and dependency rewrites of a native-format mapping.
    Mapping { head: Id, id: Id },
    /// Trace a correction through labels, manifests and descendants.
    Impact {
        head: Id,
        notice: Id,
        /// Report the unscanned range separately from unresolved dependency IDs.
        #[arg(long)]
        compact: bool,
        #[arg(long)]
        mapping: Vec<Id>,
        /// Check cross-archive identity or input correction authority under a recipient policy.
        #[arg(long)]
        policy: Option<Id>,
        /// Bound the record scan and each proof dependency walk.
        #[arg(long, default_value_t = 10000)]
        budget: usize,
    },
    /// Record a scoped adoption or rejection from a registered, independently witnessed paired trial.
    Adopt {
        head: Id,
        request: PathBuf,
        controller_key: String,
    },
    /// Derive a scoped learning label from an immutable receipt.
    Label {
        head: Id,
        receipt: Id,
        origin: String,
        #[arg(long)]
        check: Option<String>,
        #[arg(long)]
        condition: Option<Id>,
    },
    /// Extract a dated, permission-checked training manifest.
    Train { request: PathBuf },
    /// Create an Ed25519 signing seed in a new private file.
    Keygen { output: PathBuf },
    /// Attest to the exact original record under a role and scope.
    #[command(
        long_about = "Attest to the exact original record under a role and scope. Use role notice-search-exclusion and scope notice-coverage-1 to certify that a readable record is outside direct notice discovery."
    )]
    Sign {
        head: Id,
        id: Id,
        key: PathBuf,
        role: String,
        scope: String,
    },
    /// Verify detached attestation bytes under an explicit trusted public key.
    #[command(
        long_about = "Verify detached attestation bytes under an explicit trusted public key. For a notice-search-exclusion attestation, the target must be in the selected inventory and its bytes may be unavailable."
    )]
    Verify {
        head: Id,
        attestation: Id,
        target: Id,
        role: String,
        scope: String,
        public_key: String,
    },
    /// Define a unit-specific shared resource pool.
    Resource { unit: String, capacity: u64 },
    /// Reserve one authorised attempt before a facility executes it.
    Reserve {
        head: Id,
        request: PathBuf,
        public_key: String,
    },
    /// Reconcile an ambiguous attempt against a signed occurrence witness.
    Readback {
        head: Id,
        attempt: String,
        occurrence: Id,
        signature: Id,
        public_key: String,
    },
    /// Revoke a local executor authority.
    Revoke { authority: Id },
    /// Erase a payload and persist its local withdrawal marker.
    Withdraw { id: Id },
}
fn load(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut b = vec![];
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut b)?;
    ensure!(b.len() <= limit, "file budget exhausted");
    Ok(b)
}
fn input<T: DeserializeOwned>(p: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&load(p, MAX_BYTES)?)?)
}
fn signing_key(path: &Path) -> Result<SigningKey> {
    let seed: [u8; 32] = load(path, 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected raw 32-byte signing seed"))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn unix_time() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

fn print<T: Serialize>(v: T) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout().lock(), &v)?;
    println!();
    Ok(())
}
fn publish<T>(path: &Path, write: impl FnOnce(&mut File) -> Result<T>) -> Result<T> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    let result = write(staged.as_file_mut())?;
    staged.as_file().sync_all()?;
    staged.persist_noclobber(path)?;
    Ok(result)
}
fn output(path: &Path, data: &[u8]) -> Result<()> {
    publish(path, |f| Ok(f.write_all(data)?))
}
fn output_json(path: &Path, data: &impl Serialize) -> Result<()> {
    publish(path, |file| {
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, data)?;
        Ok(writer.flush()?)
    })
}
pub fn run(command: ArchiveCommand, db_path: PathBuf, f: Format) -> Result<()> {
    let public = Id::blob(PUBLIC);
    let open = || Archive::open(&db_path);
    let reader = || Archive::open_for_read(&db_path);
    match command {
        ArchiveCommand::Custody(args) => custody::run(&db_path, args.0),
        ArchiveCommand::MigrateArchive {
            source,
            destination,
        } => print(json!({"converted":Archive::migrate_archive_file(&source,&destination)?})),
        ArchiveCommand::MigratedHead { head } => print(reader()?.active_manifest_id(head)?),
        ArchiveCommand::Capabilities => print(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": ["carrier","archive-custody","profile-reader","evaluator","local-executor-reservations"],
            "kits": {"A":kit::pin(Format::A),"B":kit::pin(Format::B)},
            "profile": Id::blob(profile::PROFILE),
            "measurement_profile": Id::blob(profile::MEASUREMENT_PROFILE),
            "public": public,
            "receipt_evaluator": evaluation::EVALUATOR_ID,
            "calibration_identity_evaluator": evaluation::EVALUATOR_ID,
            "input_correction_evaluator": evaluation::EVALUATOR_ID,
            "notice_coverage": {
                "role": evaluation::notice_coverage::Exclusion::Record.role(),
                "scope": evaluation::notice_coverage::SCOPE,
            },
            "replay_evaluators": evaluation::replay_evaluator_ids().collect::<Vec<_>>(),
            "evaluators": ["simultaneous-bounds","certified-hull","signed-tests","typed-outcome","delegated","recorded-difference","least-zero","domain-inclusion","source-independence","sample-description","survey-identification","unit-grid-map"],
        })),
        ArchiveCommand::Kit { file, sha256 } => {
            let cases = kit::test(&load(&file, MAX_BYTES)?, &sha256)?;
            print(&cases)?;
            ensure!(cases.iter().all(|c| c.matches), "vector mismatch");
            Ok(())
        }
        ArchiveCommand::Suite { directory } => print(crate::conformance::run(
            &directory,
            &input(&directory.join("cases.json"))?,
        )?),
        ArchiveCommand::Grade {
            answer_key,
            submission,
        } => {
            let g = crate::conformance::grade(&input(&answer_key)?, &input(&submission)?)?;
            print(&g)?;
            ensure!(g.correct == g.total, "conformance disagreement");
            Ok(())
        }
        ArchiveCommand::Trial { directory } => print(fixture::trial(&directory)?),
        ArchiveCommand::Keygen { output: path } => {
            let mut seed = [0; 32];
            getrandom::fill(&mut seed)
                .map_err(|e| anyhow::anyhow!("operating system randomness: {e}"))?;
            let key = SigningKey::from_bytes(&seed);
            #[cfg(windows)]
            windows_key::publish_seed(&path, &seed)?;
            #[cfg(not(windows))]
            output(&path, &seed)?;
            print(hex::encode(key.verifying_key().to_bytes()))
        }
        ArchiveCommand::Value { model } => {
            print(input::<crate::investigation::ObservationModel>(&model)?.evaluate()?)
        }
        ArchiveCommand::Capture(DeferredArgs(research::CaptureArguments {
            file,
            media,
            chunk_size,
            access,
            context,
            note,
        })) => {
            let source = File::open(file)?;
            let db = open()?;
            if let Some(target) = context {
                print(db.capture_for(
                    source,
                    &media,
                    chunk_size,
                    crate::payload::CaptureContext {
                        target,
                        note: note.as_deref(),
                        access,
                    },
                    f,
                )?)
            } else {
                print(db.capture(source, &media, chunk_size, access.unwrap_or(public), f)?)
            }
        }
        ArchiveCommand::Recover {
            head,
            descriptor,
            output: path,
            bytes,
        } => {
            let db = reader()?;
            print(publish(&path, |file| {
                db.materialize_at(head, descriptor, bytes, file)
            })?)
        }
        ArchiveCommand::Select { head, request } => {
            let db = open()?;
            print(db.select(&db.active_head(head)?, &input(&request)?, f)?)
        }
        ArchiveCommand::Execute {
            head,
            request,
            public_key,
            witness_key,
        } => {
            let mut db = open()?;
            print(db.execute_payload_check(
                &db.active_head(head)?,
                &input(&request)?,
                &[public_key].into(),
                &signing_key(&witness_key)?,
                unix_time()?,
                f,
            )?)
        }
        ArchiveCommand::Init => {
            open()?;
            print(json!({"profile":Id::blob(profile::PROFILE),"public":public}))
        }
        ArchiveCommand::Status => {
            let path = std::path::absolute(&db_path)?;
            let state = if path.try_exists()? {
                Some(reader()?.status()?)
            } else {
                None
            };
            print(json!({"path":path,"archive":state}))
        }
        ArchiveCommand::Publish { head } => {
            open()?.release(head)?;
            print(head)
        }
        ArchiveCommand::Heads => print(reader()?.releases()?),
        ArchiveCommand::Blob { file, access } => {
            print(open()?.blob(&load(&file, MAX_BYTES)?, access.unwrap_or(public))?)
        }
        ArchiveCommand::Wire { file } => {
            let db = open()?;
            let bytes = load(&file, MAX_BYTES)?;
            let r = Record::decode(&bytes, f)?;
            print(db.record(&r, f)?)
        }
        ArchiveCommand::Put { file, access } => {
            print(open()?.document(&input::<Document>(&file)?, f, access.unwrap_or(public))?)
        }
        ArchiveCommand::Snapshot {
            source,
            parent,
            root,
            continue_tip,
        } => {
            let db = open()?;
            let parents = parent.into_iter().collect();
            let head = if continue_tip {
                db.continue_local_tip(source.as_deref())?
            } else {
                let source = source.as_deref().context("snapshot source is required")?;
                if root.is_empty() {
                    db.snapshot_id(source, parents)?
                } else {
                    db.selected_snapshot(source, parents, root.into_iter().collect())?
                        .id()?
                }
            };
            print(head)
        }
        ArchiveCommand::Show { head, id, purpose } => {
            print(reader()?.read_head(head, id, &purpose)?)
        }
        ArchiveCommand::Question(DeferredArgs(research::QuestionArguments {
            id,
            head,
            budget,
        })) => {
            let db = reader()?;
            print(db.question_at(head, id, budget)?)
        }
        ArchiveCommand::Resolve {
            head,
            id,
            output: path,
            purpose,
        } => {
            let db = reader()?;
            output(&path, &db.resolve_head(head, id, &purpose)?)
        }
        ArchiveCommand::Discover {
            head,
            kind,
            subject,
            scope,
            after,
            budget,
        } => {
            let db = reader()?;
            let kind = kind
                .map(|s| serde_json::from_value::<Kind>(s.into()))
                .transpose()?;
            print(db.discover_head(
                head,
                kind,
                subject.as_deref(),
                scope.as_deref(),
                "read",
                budget,
                after,
            )?)
        }
        ArchiveCommand::Find(DeferredArgs(arguments)) => {
            print(LocalSearch::default().find_path(&db_path, arguments)?)
        }
        ArchiveCommand::SearchHead { head, clear } => {
            let db = if head.is_some() || clear {
                open()?
            } else {
                reader()?
            };
            if head.is_some() || clear {
                db.set_search_head(head)?;
            }
            print(db.search_head_id(None)?)
        }
        ArchiveCommand::PrepareSearch(DeferredArgs(arguments)) => {
            print(LocalSearch::default().prepare(&open()?, arguments)?)
        }
        ArchiveCommand::SearchSession => LocalSearch::default().serve_path(
            &db_path,
            std::io::stdin().lock(),
            std::io::stdout().lock(),
        ),
        ArchiveCommand::Explain {
            head,
            id,
            budget,
            depth,
            purpose,
            compact,
            after,
        } => {
            let db = reader()?;
            let report = if compact {
                db.argument_graph_at(head, id, &purpose, budget, depth, after)?
            } else {
                db.argument_graph(&db.active_head(head)?, id, &purpose, budget, depth)?
            };
            print(report)
        }
        ArchiveCommand::Assess { head, query } => {
            let q = input(&query)?;
            evaluation::check_receipt_budget(&q)?;
            let db = open()?;
            let s = db.active_head(head)?;
            let r = evaluation::assess(&db, &s, &q)?;
            let id = evaluation::deposit_receipt(&db, &s, &q, &r, f)?;
            print(json!({"id":id,"receipt":r}))
        }
        ArchiveCommand::Replay(DeferredArgs(research::ReplayArguments {
            receipt,
            head,
            require_match,
        })) => {
            let db = reader()?;
            let report = match head {
                Some(head) => db.replay_assessment_at(head, receipt)?,
                None => db.replay_local_assessment(receipt)?,
            };
            let matches = report.matches;
            print(report)?;
            ensure!(
                !require_match || matches,
                "saved receipt differs from replay"
            );
            Ok(())
        }
        ArchiveCommand::Reassess(DeferredArgs(research::ReassessArguments {
            receipt,
            head,
            argument,
            at,
        })) => print(open()?.reassess(head, receipt, &argument, at, f)?),
        ArchiveCommand::Export {
            head,
            member,
            output: path,
            purpose,
            after,
            limit,
            stream,
            paged,
            legacy_page,
        } => {
            let db = reader()?;
            let head = db.current_head_id(head)?;
            if paged {
                ensure!(purpose == "read", "incremental pages require read purpose");
                let limit = limit.unwrap_or(64);
                let page = if legacy_page {
                    db.export_legacy_exchange_page(head, after, limit)?
                } else {
                    db.export_exchange_page(head, after, limit)?
                };
                output_json(&path, &page)?;
                return print(page.progress());
            }
            if !member.is_empty() {
                ensure!(
                    purpose == "read",
                    "member proofs are read-purpose disclosures"
                );
                let proof = db.export_members(head, &member)?;
                output_json(&path, &proof)?;
                return print(match proof.requested()? {
                    [id] => json!({"head":head,"member":id}),
                    ids => json!({"head":head,"members":ids}),
                });
            }
            if stream {
                let count = publish(&path, |file| {
                    let mut output = BufWriter::new(file);
                    let count = db.export_stream_head(head, &purpose, &mut output)?;
                    output.flush()?;
                    Ok(count)
                })?;
                return print(json!({"head":head,"continuation":null,"packets":count}));
            }
            let s = db.active_head(head)?;
            let packet = if limit.is_some() || after.is_some() {
                db.export_page(&s, &purpose, after, limit.unwrap_or(usize::MAX))?
            } else {
                db.export(&s, &purpose)?
            };
            output_json(&path, &packet)?;
            print(
                json!({"head":packet.head,"continuation":packet.continuation,"packets":packet.packets.len()}),
            )
        }
        ArchiveCommand::ImportStatus { head } => {
            print(json!({"head": head, "sessions": reader()?.exchange_import_status(head)?}))
        }
        ArchiveCommand::Import {
            file,
            stream,
            max_bytes,
            paged,
            member_head,
            legacy_page,
            expected_head,
        } => {
            let mut db = open()?;
            let file = File::open(file)?;
            if paged {
                let page = crate::store::ExchangePage::read(file)?;
                let head = expected_head.unwrap_or_else(|| page.progress().head);
                let progress = if legacy_page {
                    db.import_legacy_exchange_page(head, &page)?
                } else {
                    db.import_pinned_exchange_page(head, &page)?
                };
                return print(progress);
            }
            if let Some(head) = member_head {
                let ids = db.import_members(head, &crate::store::MemberProof::read(file)?)?;
                match ids.as_slice() {
                    [id] => print(id),
                    _ => print(ids),
                }
            } else if stream {
                let limit = max_bytes.unwrap_or(256 * 1024 * 1024);
                ensure!(
                    file.metadata()?.len() <= limit,
                    "stream exceeds receiver byte limit"
                );
                print(db.import_stream_with_limit(BufReader::new(file), limit)?)
            } else {
                print(db.import(&Bundle::read(file)?)?)
            }
        }
        ArchiveCommand::Project {
            head,
            receipt,
            key,
            output: path,
        } => {
            let db = open()?;
            let disclosure =
                db.project_hull(&db.active_head(head)?, receipt, &signing_key(&key)?, f)?;
            output_json(&path, &disclosure.packet)?;
            print(
                json!({"head":disclosure.packet.head,"queries":disclosure.queries,"private_mapping":disclosure.private_mapping}),
            )
        }
        ArchiveCommand::Migrate { head, issuer } => {
            let db = open()?;
            print(db.migrate(&db.active_head(head)?, f, &issuer)?)
        }
        ArchiveCommand::Mapping { head, id } => {
            let db = reader()?;
            print(db.verify_mapping(&db.active_head(head)?, id)?)
        }
        ArchiveCommand::Impact {
            head,
            notice,
            mapping,
            policy,
            budget,
            compact,
        } => print(reader()?.impact_head(head, notice, &mapping, budget, policy, compact)?),
        ArchiveCommand::Adopt {
            head,
            request,
            controller_key,
        } => {
            let db = open()?;
            print(db.adoption(
                &db.active_head(head)?,
                &input(&request)?,
                &[controller_key].into(),
                f,
            )?)
        }
        ArchiveCommand::Label {
            head,
            receipt,
            origin,
            check,
            condition,
        } => {
            let db = open()?;
            print(db.label(
                &db.active_head(head)?,
                receipt,
                &origin,
                check.as_deref(),
                condition,
                f,
            )?)
        }
        ArchiveCommand::Train { request } => {
            print(open()?.training_manifest(&input::<learning::TrainingRequest>(&request)?, f)?)
        }
        ArchiveCommand::Sign {
            head,
            id,
            key,
            role,
            scope,
        } => {
            let db = open()?;
            let snapshot = db.active_head(head)?;
            if let Some(exclusion) = evaluation::notice_coverage::Exclusion::for_role(&role) {
                ensure!(
                    scope == evaluation::notice_coverage::SCOPE,
                    "invalid notice coverage scope"
                );
                return print(exclusion.certify(&db, &snapshot, id, &signing_key(&key)?, f)?);
            }
            db.resolve(&snapshot, id, "read")?;
            print(db.document(
                &profile::attest(id, &scope, &role, "research-policy-1", &signing_key(&key)?)?,
                f,
                public,
            )?)
        }
        ArchiveCommand::Verify {
            head,
            attestation,
            target,
            role,
            scope,
            public_key,
        } => {
            let db = reader()?;
            if let Some(exclusion) = evaluation::notice_coverage::Exclusion::for_role(&role) {
                ensure!(
                    scope == evaluation::notice_coverage::SCOPE,
                    "invalid notice coverage scope"
                );
                exclusion.verify_at(
                    &db,
                    &db.active_head(head)?,
                    target,
                    attestation,
                    &[public_key].into(),
                )?;
                return print("pass");
            }
            db.resolve_head(head, target, "read")?;
            profile::verify_attestation(
                &db.read_head(head, attestation, "read")?,
                target,
                &scope,
                &role,
                "research-policy-1",
                &[public_key].into(),
            )?;
            print("pass")
        }
        ArchiveCommand::Resource { unit, capacity } => open()?.resource(&unit, capacity),
        ArchiveCommand::Reserve {
            head,
            request,
            public_key,
        } => {
            let mut db = open()?;
            print(db.reserve(
                &db.active_head(head)?,
                &input::<executor::Request>(&request)?,
                &[public_key].into(),
                unix_time()?,
            )?)
        }
        ArchiveCommand::Readback {
            head,
            attempt,
            occurrence,
            signature,
            public_key,
        } => {
            let db = open()?;
            db.reconcile_attempt(
                &db.active_head(head)?,
                &attempt,
                occurrence,
                signature,
                &[public_key].into(),
            )
        }
        ArchiveCommand::Revoke { authority } => open()?.revoke(authority),
        ArchiveCommand::Withdraw { id } => open()?.withdraw(id),
    }
}
