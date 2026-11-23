mod custody;
mod search;
pub use search::{FindArguments, LocalSearch, PrepareSearchArguments, SearchRequest};

use crate::{
    evaluation, executor, fixture, kit, learning,
    profile::{self, Document, Kind, PUBLIC},
    store::{Archive, Bundle},
    wire::{Format, Id, MAX_BYTES, Record},
};
use anyhow::{Result, ensure};
use clap::Subcommand;
use ed25519_dalek::SigningKey;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Subcommand)]
pub enum ArchiveCommand {
    /// Append, replicate and review bounded archive changes.
    Custody(custody::Arguments),
    /// Show supported formats, evaluators and independently pinned kit digests.
    Capabilities,
    /// Convert an existing archive into a new file and retain its historical evidence.
    MigrateArchive {
        source: PathBuf,
        destination: PathBuf,
    },
    /// Resolve the active head corresponding to an earlier project identifier.
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
    /// Release a complete, readable inventory to the configured replicas.
    Publish { head: Id },
    /// List the complete inventories available for replication.
    Heads,
    /// Capture an arbitrary stream as an ordered, digest-checked chunk manifest.
    Capture {
        file: PathBuf,
        media: String,
        #[arg(long, default_value_t = 1048576)]
        chunk_size: usize,
        #[arg(long)]
        access: Option<Id>,
    },
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
        source: String,
        #[arg(long)]
        parent: Vec<Id>,
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
    /// Write verified, permitted bytes. The output path must be new.
    Resolve {
        head: Id,
        id: Id,
        output: PathBuf,
        #[arg(long, default_value = "read")]
        purpose: String,
    },
    /// Discover typed records within a bounded inventory page.
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
    /// Search authorized archive records and text passages by word or meaning.
    Find(FindArguments),
    /// Show or pin the active search snapshot.
    SearchHead {
        head: Option<Id>,
        /// Use the newest local snapshot, or the first import if there is no local snapshot.
        #[arg(long, conflicts_with = "head")]
        clear: bool,
    },
    /// Prepare archive text, media and embeddings independently of a search.
    PrepareSearch(PrepareSearchArguments),
    /// Reuse a local model for JSON-line find and prepare-search requests on stdin.
    SearchSession,
    /// Trace required input edges and the unresolved frontier.
    Explain {
        head: Id,
        id: Id,
        /// Return the unscanned records as a resumable range.
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
    /// Evaluate a query and persist its immutable receipt.
    Assess { head: Id, query: PathBuf },
    /// Serialize a bounded inventory with its permitted objects for offline exchange.
    Export {
        head: Id,
        #[arg(long)]
        after: Option<Id>,
        #[arg(long)]
        limit: Option<usize>,
        /// Write the inventory once, followed by an ordered stream of packets.
        #[arg(long, conflicts_with_all = ["after", "limit"])]
        stream: bool,
        output: PathBuf,
        #[arg(long, default_value = "read")]
        purpose: String,
    },
    /// Verify and reconcile an exchange file atomically.
    Import {
        file: PathBuf,
        /// Read the streaming exchange format.
        #[arg(long)]
        stream: bool,
    },
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
    Sign {
        head: Id,
        id: Id,
        key: PathBuf,
        role: String,
        scope: String,
    },
    /// Verify detached attestation bytes under an explicit trusted public key.
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
    match command {
        ArchiveCommand::Custody(args) => custody::run(&db_path, args.0),
        ArchiveCommand::MigrateArchive {
            source,
            destination,
        } => print(json!({"converted":Archive::migrate_archive_file(&source,&destination)?})),
        ArchiveCommand::MigratedHead { head } => {
            let db = open()?;
            print(db.active_head(head)?.id()?)
        }
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
            output(&path, &seed)?;
            print(hex::encode(key.verifying_key().to_bytes()))
        }
        ArchiveCommand::Value { model } => {
            print(input::<crate::investigation::ObservationModel>(&model)?.evaluate()?)
        }
        ArchiveCommand::Capture {
            file,
            media,
            chunk_size,
            access,
        } => print(open()?.capture(
            File::open(file)?,
            &media,
            chunk_size,
            access.unwrap_or(public),
            f,
        )?),
        ArchiveCommand::Recover {
            head,
            descriptor,
            output: path,
            bytes,
        } => {
            let db = open()?;
            let s = db.active_head(head)?;
            print(publish(&path, |file| {
                db.materialize(&s, descriptor, "read", bytes, file)
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
        ArchiveCommand::Publish { head } => {
            open()?.release(head)?;
            print(head)
        }
        ArchiveCommand::Heads => print(open()?.releases()?),
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
        } => {
            let db = open()?;
            let parents = parent.into_iter().collect();
            let head = if root.is_empty() {
                db.snapshot_id(&source, parents)?
            } else {
                db.selected_snapshot(&source, parents, root.into_iter().collect())?
                    .id()?
            };
            print(head)
        }
        ArchiveCommand::Show { head, id, purpose } => {
            let db = open()?;
            print(db.read(&db.active_head(head)?, id, &purpose)?)
        }
        ArchiveCommand::Resolve {
            head,
            id,
            output: path,
            purpose,
        } => {
            let db = open()?;
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
            let db = open()?;
            let kind = kind
                .map(|s| serde_json::from_value::<Kind>(s.into()))
                .transpose()?;
            print(db.discover(
                &db.active_head(head)?,
                kind,
                subject.as_deref(),
                scope.as_deref(),
                "read",
                budget,
                after,
            )?)
        }
        ArchiveCommand::Find(arguments) => print(LocalSearch::default().find(&open()?, arguments)?),
        ArchiveCommand::SearchHead { head, clear } => {
            let db = open()?;
            if head.is_some() || clear {
                db.set_search_head(head)?;
            }
            print(db.search_head(None)?.id()?)
        }
        ArchiveCommand::PrepareSearch(arguments) => {
            print(LocalSearch::default().prepare(&open()?, arguments)?)
        }
        ArchiveCommand::SearchSession => LocalSearch::default().serve(
            &open()?,
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
            let db = open()?;
            let snapshot = db.active_head(head)?;
            let report = if compact {
                db.argument_graph_page(&snapshot, id, &purpose, budget, depth, after)?
            } else {
                db.argument_graph(&snapshot, id, &purpose, budget, depth)?
            };
            print(report)
        }
        ArchiveCommand::Assess { head, query } => {
            let db = open()?;
            let s = db.active_head(head)?;
            let q = input(&query)?;
            let r = evaluation::assess(&db, &s, &q)?;
            let id = evaluation::deposit_receipt(&db, &s, &q, &r, f)?;
            print(json!({"id":id,"receipt":r}))
        }
        ArchiveCommand::Export {
            head,
            output: path,
            purpose,
            after,
            limit,
            stream,
        } => {
            let db = open()?;
            let head = db.current_head_id(head)?;
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
        ArchiveCommand::Import { file, stream } => {
            let mut db = open()?;
            let file = File::open(file)?;
            if stream {
                print(db.import_stream(BufReader::new(file))?)
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
            let db = open()?;
            print(db.verify_mapping(&db.active_head(head)?, id)?)
        }
        ArchiveCommand::Impact {
            head,
            notice,
            mapping,
            policy,
            budget,
            compact,
        } => {
            let db = open()?;
            let snapshot = db.active_head(head)?;
            if compact {
                print(db.impact_compact(&snapshot, notice, &mapping, budget, policy)?)
            } else {
                print(db.impact_with_policy(&snapshot, notice, &mapping, budget, policy)?)
            }
        }
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
            db.resolve(&db.active_head(head)?, id, "read")?;
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
            let db = open()?;
            let s = db.active_head(head)?;
            db.resolve(&s, target, "read")?;
            profile::verify_attestation(
                &db.read(&s, attestation, "read")?,
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
