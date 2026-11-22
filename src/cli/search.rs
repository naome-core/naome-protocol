use super::load;
use crate::{
    search::{
        Embedder, MAX_MEDIA_BYTES, MediaInput, SearchInput, SearchMode, SearchOptions,
        SearchPreparation, SearchReport, text_media,
    },
    store::Archive,
    wire::Id,
};
use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Args, Serialize, Deserialize)]
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
    /// Search this snapshot. Defaults to the active search snapshot.
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
    #[serde(default = "default_limit")]
    limit: usize,
    /// Maximum records to prepare for a first search of this snapshot.
    #[arg(long, default_value_t = SearchOptions::default().index_budget)]
    #[serde(default = "default_index_budget")]
    index_budget: usize,
    /// Maximum full-vector equivalents to spend on semantic comparisons.
    #[arg(long, default_value_t = SearchOptions::default().vector_budget)]
    #[serde(default = "default_vector_budget")]
    vector_budget: usize,
    /// Prepare missing vectors synchronously before responding.
    #[arg(long)]
    #[serde(default)]
    prepare: bool,
    /// Rebuild the selected snapshot's view of the derived cache.
    #[arg(long)]
    #[serde(default)]
    rebuild_index: bool,
    #[command(flatten)]
    #[serde(flatten)]
    model: ModelArguments,
}

#[derive(Args, Serialize, Deserialize)]
pub struct PrepareSearchArguments {
    /// Prepare this snapshot. Defaults to the active search snapshot.
    #[arg(long)]
    head: Option<Id>,
    /// Maximum records and passages to prepare in this invocation.
    #[arg(long, default_value_t = SearchOptions::default().index_budget)]
    #[serde(default = "default_index_budget")]
    index_budget: usize,
    /// Rebuild the selected snapshot's view of the derived cache.
    #[arg(long)]
    #[serde(default)]
    rebuild_index: bool,
    /// Prepare only text and source metadata.
    #[arg(long)]
    #[serde(default)]
    lexical: bool,
    #[command(flatten)]
    #[serde(flatten)]
    model: ModelArguments,
}

#[derive(Args, Serialize, Deserialize)]
struct ModelArguments {
    /// Local EmbeddingGemma 2 checkpoint directory (also NAOME_SEARCH_MODEL).
    #[arg(long)]
    model: Option<PathBuf>,
    /// Python with local media and inference dependencies (also NAOME_SEARCH_PYTHON).
    #[arg(long)]
    python: Option<PathBuf>,
}

impl ModelArguments {
    fn directory(&self) -> Option<PathBuf> {
        self.model
            .clone()
            .or_else(|| std::env::var_os("NAOME_SEARCH_MODEL").map(PathBuf::from))
    }
}

#[cfg(feature = "semantic-search")]
struct LoadedModel {
    directory: PathBuf,
    python: PathBuf,
    worker: crate::search::EmbeddingGemma,
}

/// A local search session. A caller can keep it alive across requests to reuse
/// the model process. Archive access stays with the supplied custody connection.
#[derive(Default)]
pub struct LocalSearch {
    #[cfg(feature = "semantic-search")]
    model: Option<LoadedModel>,
}

impl LocalSearch {
    fn model(&mut self, directory: PathBuf, python: Option<PathBuf>) -> Result<&mut dyn Embedder> {
        #[cfg(feature = "semantic-search")]
        {
            let directory = directory
                .canonicalize()
                .context("locate local EmbeddingGemma 2 weights")?;
            let python = python
                .or_else(|| std::env::var_os("NAOME_SEARCH_PYTHON").map(PathBuf::from))
                .unwrap_or_else(|| {
                    PathBuf::from(if cfg!(windows) {
                        "python.exe"
                    } else {
                        "python3"
                    })
                });
            if self.model.as_ref().is_none_or(|loaded| {
                loaded.directory != directory
                    || loaded.python != python
                    || !loaded.worker.is_available()
            }) {
                self.model = None;
                let worker = crate::search::EmbeddingGemma::open(&directory, &python)?;
                self.model = Some(LoadedModel {
                    directory,
                    python,
                    worker,
                });
            }
            Ok(&mut self
                .model
                .as_mut()
                .context("missing embedding worker")?
                .worker)
        }
        #[cfg(not(feature = "semantic-search"))]
        {
            let _ = (directory, python);
            anyhow::bail!("this build lacks the semantic-search Cargo feature")
        }
    }

    pub fn prepare(
        &mut self,
        archive: &Archive,
        args: PrepareSearchArguments,
    ) -> Result<SearchPreparation> {
        let snapshot = archive.search_head(args.head)?;
        if args.rebuild_index {
            archive.clear_search_index(snapshot.id()?)?;
        }
        let model = if args.lexical {
            None
        } else {
            args.model
                .directory()
                .map(|path| self.model(path, args.model.python))
                .transpose()?
        };
        archive.prepare_search(&snapshot, args.index_budget, model)
    }

    pub fn find(&mut self, archive: &Archive, args: FindArguments) -> Result<SearchReport> {
        let snapshot = archive.search_head(args.head)?;
        let bytes = args
            .input
            .as_deref()
            .map(|path| load(path, MAX_MEDIA_BYTES as usize))
            .transpose()?;
        let input = match (
            args.query.as_deref(),
            args.input.as_deref(),
            bytes.as_deref(),
        ) {
            (Some(query), None, None) => SearchInput::Text(query),
            (None, Some(path), Some(bytes)) => {
                let media = args
                    .media_type
                    .as_deref()
                    .or_else(|| infer_media_type(path));
                if media.is_some_and(text_media) || media.is_none() {
                    let text = std::str::from_utf8(bytes)
                        .context("query file requires supported media or UTF-8 text")?;
                    ensure!(
                        !text.contains('\0'),
                        "query file requires supported media or UTF-8 text"
                    );
                    SearchInput::TextFile(text)
                } else {
                    SearchInput::Media(MediaInput {
                        media_type: media.context("missing query media type")?,
                        bytes,
                    })
                }
            }
            _ => anyhow::bail!("expected a search query or --input FILE"),
        };
        let directory = args.model.directory();
        let mode = args.mode.unwrap_or(if directory.is_some() {
            SearchMode::Hybrid
        } else {
            SearchMode::Lexical
        });
        let options = SearchOptions {
            mode,
            kind: args
                .kind
                .map(|kind| serde_json::from_value(kind.into()))
                .transpose()?,
            subject: args.subject.as_deref(),
            scope: args.scope.as_deref(),
            limit: args.limit,
            index_budget: args.index_budget,
            vector_budget: args.vector_budget,
        };
        if args.rebuild_index {
            archive.clear_search_index(snapshot.id()?)?;
        }
        if matches!(input, SearchInput::Text(text) if text.trim().parse::<Id>().is_ok()) {
            return archive.search_prepared(&snapshot, input, options, None);
        }
        let model = if mode == SearchMode::Lexical {
            None
        } else {
            let directory =
                directory.context("configure --model or NAOME_SEARCH_MODEL for semantic search")?;
            Some(self.model(directory, args.model.python)?)
        };
        if args.prepare {
            return archive.find_input(&snapshot, input, options, model);
        }
        if !matches!(input, SearchInput::Media(_)) {
            archive.prepare_search(&snapshot, args.index_budget, None)?;
        }
        archive.search_prepared(&snapshot, input, options, model)
    }

    /// Serve one typed search request and response per JSON line.
    pub fn serve(
        &mut self,
        archive: &Archive,
        mut input: impl BufRead,
        mut output: impl Write,
    ) -> Result<()> {
        const LINE_LIMIT: u64 = 64 * 1024;
        loop {
            let mut line = String::new();
            if (&mut input).take(LINE_LIMIT + 1).read_line(&mut line)? == 0 {
                break;
            }
            ensure!(
                line.len() as u64 <= LINE_LIMIT,
                "search session request exceeds line budget"
            );
            let response = self.execute_line(archive, &line);
            let response = match response {
                Ok(value) => json!({"ok":value}),
                Err(error) => json!({"error":error.to_string()}),
            };
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        Ok(())
    }

    fn execute_line(&mut self, archive: &Archive, line: &str) -> Result<Value> {
        match serde_json::from_str(line)? {
            SearchRequest::Find(args) => Ok(serde_json::to_value(self.find(archive, args)?)?),
            SearchRequest::PrepareSearch(args) => {
                Ok(serde_json::to_value(self.prepare(archive, args)?)?)
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SearchRequest {
    Find(FindArguments),
    PrepareSearch(PrepareSearchArguments),
}

impl SearchRequest {
    /// Resolve paths before sending a request to an existing local process.
    pub fn with_client_paths(mut self) -> Result<Self> {
        let model = match &mut self {
            Self::Find(args) => {
                args.input = args.input.take().map(std::path::absolute).transpose()?;
                &mut args.model
            }
            Self::PrepareSearch(args) => &mut args.model,
        };
        model.model = model.directory().map(std::path::absolute).transpose()?;
        let python = model
            .python
            .take()
            .or_else(|| std::env::var_os("NAOME_SEARCH_PYTHON").map(PathBuf::from));
        model.python = match python {
            Some(python) if python.components().count() > 1 => Some(std::path::absolute(python)?),
            other => other,
        };
        Ok(self)
    }
}

fn default_limit() -> usize {
    SearchOptions::default().limit
}
fn default_index_budget() -> usize {
    SearchOptions::default().index_budget
}
fn default_vector_budget() -> usize {
    SearchOptions::default().vector_budget
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
