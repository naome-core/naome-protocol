use super::{EmbeddedMedia, Embedder, EmbeddingInput, MediaInput, media_type};
use crate::wire::Id;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, sync_channel},
    thread,
    time::{Duration, Instant},
};

const DIMENSIONS: usize = 768;
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const WORKER_TIMEOUT: Duration = Duration::from_secs(180);

/// An offline EmbeddingGemma 2 worker using explicitly installed model files.
/// It keeps the encoders warm for the lifetime of this handle.
pub struct EmbeddingGemma {
    child: Child,
    input: Option<BufWriter<ChildStdin>>,
    responses: Receiver<Result<Value>>,
    model: Id,
}

impl EmbeddingGemma {
    pub fn open(directory: &Path, python: &Path) -> Result<Self> {
        let directory = directory
            .canonicalize()
            .context("locate local EmbeddingGemma 2 weights")?;
        ensure!(
            directory.is_dir(),
            "expected a local EmbeddingGemma 2 model directory"
        );
        let mut child = Command::new(python)
            .args(["-I", "-u", "-c", include_str!("gemma_worker.py")])
            .arg(directory)
            .env("HF_HUB_OFFLINE", "1")
            .env("TRANSFORMERS_OFFLINE", "1")
            .env("HF_HUB_DISABLE_TELEMETRY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start the local EmbeddingGemma 2 Python worker")?;
        let input = BufWriter::new(child.stdin.take().context("worker stdin unavailable")?);
        let mut output = BufReader::new(child.stdout.take().context("worker stdout unavailable")?);
        let (send, responses) = sync_channel(1);
        thread::spawn(move || {
            loop {
                let response = read_response(&mut output);
                let finished = response.is_err();
                if send.send(response).is_err() || finished {
                    break;
                }
            }
        });
        let mut worker = Self {
            child,
            input: Some(input),
            responses,
            model: Id::blob(&[]),
        };
        let hello: Value = worker.response()?;
        let fingerprint = hello["fingerprint"]
            .as_str()
            .context("missing EmbeddingGemma 2 fingerprint")?;
        let fingerprint = hex::decode(fingerprint)?;
        ensure!(
            fingerprint.len() == 32,
            "invalid EmbeddingGemma 2 fingerprint"
        );
        ensure!(
            hello["dimensions"].as_u64() == Some(DIMENSIONS as u64),
            "unexpected EmbeddingGemma 2 dimensions"
        );
        worker.model =
            Id::blob(&[fingerprint.as_slice(), include_bytes!("gemma_worker.py")].concat());
        Ok(worker)
    }

    fn response<T: DeserializeOwned>(&mut self) -> Result<T> {
        let value = self
            .responses
            .recv_timeout(WORKER_TIMEOUT)
            .context("EmbeddingGemma 2 worker timed out or closed")??;
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            bail!("EmbeddingGemma 2 worker: {error}");
        }
        Ok(serde_json::from_value(value)?)
    }

    fn request<T: DeserializeOwned>(&mut self, request: &Value) -> Result<T> {
        let input = self.input.as_mut().context("worker input closed")?;
        serde_json::to_writer(&mut *input, request)?;
        input.write_all(b"\n")?;
        input.flush()?;
        self.response()
    }
}

fn read_response(mut output: impl BufRead) -> Result<Value> {
    let mut line = String::new();
    let bytes = (&mut output)
        .take(MAX_RESPONSE_BYTES)
        .read_line(&mut line)?;
    ensure!(
        bytes != 0 && line.ends_with('\n'),
        "EmbeddingGemma 2 worker closed or exceeded its response budget"
    );
    serde_json::from_str(&line).context("invalid EmbeddingGemma 2 worker response")
}

impl Drop for EmbeddingGemma {
    fn drop(&mut self) {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Embedder for EmbeddingGemma {
    fn id(&self) -> Id {
        self.model
    }

    fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    fn minimum_similarity(&self) -> f32 {
        0.595
    }

    fn embed(&mut self, input: EmbeddingInput, text: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prompt = match input {
            EmbeddingInput::Query => "SearchQuery",
            EmbeddingInput::Passage => "Document",
        };
        self.request(&json!({"kind": "text", "prompt": prompt, "text": text}))
    }

    fn embed_media(
        &mut self,
        _input: EmbeddingInput,
        input: MediaInput<'_>,
    ) -> Result<EmbeddedMedia> {
        self.request(&json!({
            "kind": "media",
            "media_type": media_type(input.media_type),
            "data": STANDARD.encode(input.bytes),
        }))
    }
}
