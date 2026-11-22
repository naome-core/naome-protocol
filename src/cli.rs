use crate::{
    evaluation, executor, fixture, kit, learning,
    profile::{self, Document, Kind, PUBLIC},
    search::{MAX_MEDIA_BYTES, MediaInput, SearchInput, SearchMode, SearchOptions, text_media},
    store::{Archive, Bundle},
    wire::{Format, Id, MAX_BYTES, Record},
};
use anyhow::{Result, ensure};
use clap::{Args, Subcommand};
use ed25519_dalek::SigningKey;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Subcommand)]
pub enum ArchiveCommand {
    /// Show supported formats, evaluators and independently pinned kit digests.
    Capabilities,
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
    /// Trace required input edges and the unresolved frontier.
    Explain {
        head: Id,
        id: Id,
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
        output: PathBuf,
        #[arg(long, default_value = "read")]
        purpose: String,
    },
    /// Verify and reconcile an exchange file atomically.
    Import { file: PathBuf },
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
        #[arg(long)]
        mapping: Vec<Id>,
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
#[derive(Args)]
pub struct FindArguments {
    /// A content identifier, words, or a natural-language query.
    #[arg(required_unless_present = "input", conflicts_with = "input")]
    query: Option<String>,
    /// Use a local image, audio, video, PDF, code or data file as the query.
    #[arg(long, value_name = "FILE")]
    input: Option<PathBuf>,
    /// MIME type of --input. Defaults to the filename extension or UTF-8 content.
    #[arg(long, requires = "input")]
    media_type: Option<String>,
    /// Search this snapshot. Defaults to the most recently saved or imported snapshot.
    #[arg(long)]
    head: Option<Id>,
    /// Defaults to hybrid when a local model is configured, otherwise lexical.
    #[arg(long, value_enum)]
    mode: Option<SearchMode>,
    #[arg(long)]
    kind: Option<String>,
    #[arg(long)]
    subject: Option<String>,
    #[arg(long)]
    scope: Option<String>,
    #[arg(long, default_value_t = SearchOptions::default().limit)]
    limit: usize,
    /// Maximum records and passages to index per invocation.
    #[arg(long, default_value_t = SearchOptions::default().index_budget)]
    index_budget: usize,
    /// Maximum eligible vectors to score against the query.
    #[arg(long, default_value_t = SearchOptions::default().vector_budget)]
    vector_budget: usize,
    /// Rebuild the selected snapshot's derived search index.
    #[arg(long)]
    rebuild_index: bool,
    /// Local EmbeddingGemma 2 checkpoint directory (also NAOME_SEARCH_MODEL).
    #[arg(long)]
    model: Option<PathBuf>,
    /// Python executable with the local media and inference dependencies (also NAOME_SEARCH_PYTHON).
    #[arg(long)]
    python: Option<PathBuf>,
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
    match &command {
        ArchiveCommand::Capabilities => {
            return print(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "capabilities": ["carrier","profile-reader","evaluator","local-executor-reservations"],
                "kits": {"A":kit::pin(Format::A),"B":kit::pin(Format::B)},
                "profile": Id::blob(profile::PROFILE),
                "measurement_profile": Id::blob(profile::MEASUREMENT_PROFILE),
                "public": public,
                "receipt_evaluator": evaluation::EVALUATOR_ID,
                "replay_evaluators": evaluation::replay_evaluator_ids().collect::<Vec<_>>(),
                "evaluators": ["simultaneous-bounds","certified-hull","signed-tests","typed-outcome","delegated","recorded-difference","least-zero","domain-inclusion","source-independence","sample-description","survey-identification","unit-grid-map"],
            }));
        }
        ArchiveCommand::Kit { file, sha256 } => {
            let cases = kit::test(&load(file, MAX_BYTES)?, sha256)?;
            print(&cases)?;
            ensure!(cases.iter().all(|c| c.matches), "vector mismatch");
            return Ok(());
        }
        ArchiveCommand::Suite { directory } => {
            return print(crate::conformance::run(
                directory,
                &input(&directory.join("cases.json"))?,
            )?);
        }
        ArchiveCommand::Grade {
            answer_key,
            submission,
        } => {
            let g = crate::conformance::grade(&input(answer_key)?, &input(submission)?)?;
            print(&g)?;
            ensure!(g.correct == g.total, "conformance disagreement");
            return Ok(());
        }
        ArchiveCommand::Trial { directory } => return print(fixture::trial(directory)?),
        ArchiveCommand::Keygen { output: path } => {
            let mut seed = [0; 32];
            getrandom::fill(&mut seed)
                .map_err(|e| anyhow::anyhow!("operating system randomness: {e}"))?;
            let key = SigningKey::from_bytes(&seed);
            output(path, &seed)?;
            return print(hex::encode(key.verifying_key().to_bytes()));
        }
        ArchiveCommand::Value { model } => {
            return print(input::<crate::investigation::ObservationModel>(model)?.evaluate()?);
        }
        _ => (),
    }
    let mut db = Archive::open(db_path)?;
    match command {
        ArchiveCommand::Capture {
            file,
            media,
            chunk_size,
            access,
        } => print(db.capture(
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
            let s = db.head(head)?;
            print(publish(&path, |file| {
                db.materialize(&s, descriptor, "read", bytes, file)
            })?)
        }
        ArchiveCommand::Select { head, request } => {
            print(db.select(&db.head(head)?, &input(&request)?, f)?)
        }
        ArchiveCommand::Execute {
            head,
            request,
            public_key,
            witness_key,
        } => print(db.execute_payload_check(
            &db.head(head)?,
            &input(&request)?,
            &[public_key].into(),
            &signing_key(&witness_key)?,
            unix_time()?,
            f,
        )?),
        ArchiveCommand::Init => {
            print(json!({"profile":Id::blob(profile::PROFILE),"public":public}))
        }
        ArchiveCommand::Publish { head } => {
            db.release(head)?;
            print(head)
        }
        ArchiveCommand::Heads => print(db.releases()?),
        ArchiveCommand::Blob { file, access } => {
            print(db.blob(&load(&file, MAX_BYTES)?, access.unwrap_or(public))?)
        }
        ArchiveCommand::Wire { file } => {
            let bytes = load(&file, MAX_BYTES)?;
            let r = Record::decode(&bytes, f)?;
            print(db.record(&r, f)?)
        }
        ArchiveCommand::Put { file, access } => {
            print(db.document(&input::<Document>(&file)?, f, access.unwrap_or(public))?)
        }
        ArchiveCommand::Snapshot {
            source,
            parent,
            root,
        } => {
            let parents = parent.into_iter().collect();
            let snapshot = if root.is_empty() {
                db.snapshot(&source, parents)?
            } else {
                db.selected_snapshot(&source, parents, root.into_iter().collect())?
            };
            print(snapshot.id()?)
        }
        ArchiveCommand::Show { head, id, purpose } => {
            print(db.read(&db.head(head)?, id, &purpose)?)
        }
        ArchiveCommand::Resolve {
            head,
            id,
            output: path,
            purpose,
        } => output(&path, &db.resolve(&db.head(head)?, id, &purpose)?),
        ArchiveCommand::Discover {
            head,
            kind,
            subject,
            scope,
            after,
            budget,
        } => {
            let kind = kind
                .map(|s| serde_json::from_value::<Kind>(s.into()))
                .transpose()?;
            print(db.discover(
                &db.head(head)?,
                kind,
                subject.as_deref(),
                scope.as_deref(),
                "read",
                budget,
                after,
            )?)
        }
        ArchiveCommand::Find(arguments) => find(&db, arguments),
        ArchiveCommand::Explain {
            head,
            id,
            budget,
            depth,
            purpose,
        } => print(db.argument_graph(&db.head(head)?, id, &purpose, budget, depth)?),
        ArchiveCommand::Assess { head, query } => {
            let s = db.head(head)?;
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
        } => {
            let s = db.head(head)?;
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
        ArchiveCommand::Import { file } => print(db.import(&Bundle::read(File::open(file)?)?)?),
        ArchiveCommand::Project {
            head,
            receipt,
            key,
            output: path,
        } => {
            let disclosure = db.project_hull(&db.head(head)?, receipt, &signing_key(&key)?, f)?;
            output_json(&path, &disclosure.packet)?;
            print(
                json!({"head":disclosure.packet.head,"queries":disclosure.queries,"private_mapping":disclosure.private_mapping}),
            )
        }
        ArchiveCommand::Migrate { head, issuer } => {
            print(db.migrate(&db.head(head)?, f, &issuer)?)
        }
        ArchiveCommand::Mapping { head, id } => print(db.verify_mapping(&db.head(head)?, id)?),
        ArchiveCommand::Impact {
            head,
            notice,
            mapping,
            budget,
        } => print(db.impact(&db.head(head)?, notice, &mapping, budget)?),
        ArchiveCommand::Adopt {
            head,
            request,
            controller_key,
        } => print(db.adoption(
            &db.head(head)?,
            &input(&request)?,
            &[controller_key].into(),
            f,
        )?),
        ArchiveCommand::Label {
            head,
            receipt,
            origin,
            check,
            condition,
        } => print(db.label(
            &db.head(head)?,
            receipt,
            &origin,
            check.as_deref(),
            condition,
            f,
        )?),
        ArchiveCommand::Train { request } => {
            print(db.training_manifest(&input::<learning::TrainingRequest>(&request)?, f)?)
        }
        ArchiveCommand::Sign {
            head,
            id,
            key,
            role,
            scope,
        } => {
            db.resolve(&db.head(head)?, id, "read")?;
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
            let s = db.head(head)?;
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
        ArchiveCommand::Resource { unit, capacity } => db.resource(&unit, capacity),
        ArchiveCommand::Reserve {
            head,
            request,
            public_key,
        } => print(db.reserve(
            &db.head(head)?,
            &input::<executor::Request>(&request)?,
            &[public_key].into(),
            unix_time()?,
        )?),
        ArchiveCommand::Readback {
            head,
            attempt,
            occurrence,
            signature,
            public_key,
        } => db.reconcile_attempt(
            &db.head(head)?,
            &attempt,
            occurrence,
            signature,
            &[public_key].into(),
        ),
        ArchiveCommand::Revoke { authority } => db.revoke(authority),
        ArchiveCommand::Withdraw { id } => db.withdraw(id),
        ArchiveCommand::Capabilities
        | ArchiveCommand::Kit { .. }
        | ArchiveCommand::Suite { .. }
        | ArchiveCommand::Grade { .. }
        | ArchiveCommand::Trial { .. }
        | ArchiveCommand::Keygen { .. }
        | ArchiveCommand::Value { .. } => unreachable!("handled before opening the archive"),
    }
}

fn find(archive: &Archive, arguments: FindArguments) -> Result<()> {
    use anyhow::Context;
    let snapshot = archive.search_head(arguments.head)?;
    let bytes = arguments
        .input
        .as_deref()
        .map(|path| load(path, MAX_MEDIA_BYTES as usize))
        .transpose()?;
    let input = match (
        arguments.query.as_deref(),
        arguments.input.as_deref(),
        bytes.as_deref(),
    ) {
        (Some(query), None, None) => SearchInput::Text(query),
        (None, Some(path), Some(bytes)) => {
            let media_type = arguments
                .media_type
                .as_deref()
                .or_else(|| infer_media_type(path));
            if media_type.is_some_and(text_media) || media_type.is_none() {
                let text = std::str::from_utf8(bytes)
                    .context("query file requires supported media or UTF-8 text")?;
                ensure!(
                    !text.contains('\0'),
                    "query file requires supported media or UTF-8 text"
                );
                SearchInput::TextFile(text)
            } else {
                SearchInput::Media(MediaInput {
                    media_type: media_type.context("missing query media type")?,
                    bytes,
                })
            }
        }
        _ => anyhow::bail!("expected a search query or --input FILE"),
    };

    let model = arguments
        .model
        .or_else(|| std::env::var_os("NAOME_SEARCH_MODEL").map(PathBuf::from));
    let mode = arguments.mode.unwrap_or(if model.is_some() {
        SearchMode::Hybrid
    } else {
        SearchMode::Lexical
    });
    let options = SearchOptions {
        mode,
        kind: arguments
            .kind
            .map(|kind| serde_json::from_value(kind.into()))
            .transpose()?,
        subject: arguments.subject.as_deref(),
        scope: arguments.scope.as_deref(),
        limit: arguments.limit,
        index_budget: arguments.index_budget,
        vector_budget: arguments.vector_budget,
    };
    if arguments.rebuild_index {
        archive.clear_search_index(snapshot.id()?)?;
    }
    if mode == SearchMode::Lexical
        || matches!(input, SearchInput::Text(text) if text.trim().parse::<Id>().is_ok())
    {
        return print(archive.find_input(&snapshot, input, options, None)?);
    }
    #[cfg(feature = "semantic-search")]
    {
        let directory =
            model.context("configure --model or NAOME_SEARCH_MODEL for semantic search")?;
        let python = arguments
            .python
            .or_else(|| std::env::var_os("NAOME_SEARCH_PYTHON").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("python3"));
        let mut model = crate::search::EmbeddingGemma::open(&directory, &python)?;
        print(archive.find_input(&snapshot, input, options, Some(&mut model))?)
    }
    #[cfg(not(feature = "semantic-search"))]
    {
        let _ = (model, arguments.python);
        anyhow::bail!("this build lacks the semantic-search Cargo feature")
    }
}

fn infer_media_type(path: &Path) -> Option<&'static str> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "png" => "image/png",
            "jpeg" | "jpg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "tiff" | "tif" => "image/tiff",
            "wav" => "audio/wav",
            "flac" => "audio/flac",
            "ogg" => "audio/ogg",
            "mp3" => "audio/mpeg",
            "m4a" => "audio/mp4",
            "aac" => "audio/aac",
            "mp4" => "video/mp4",
            "mov" => "video/quicktime",
            "webm" => "video/webm",
            "mkv" => "video/x-matroska",
            "pdf" => "application/pdf",
            "json" | "jsonl" | "ndjson" => "application/json",
            "csv" => "text/csv",
            "tsv" => "text/tab-separated-values",
            _ => return None,
        },
    )
}
