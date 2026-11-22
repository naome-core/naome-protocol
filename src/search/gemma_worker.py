"""Private pipe protocol for local, offline EmbeddingGemma 2 inference."""
import base64
from contextlib import closing, redirect_stdout
from itertools import chain
import hashlib
import io
import json
import os
from pathlib import Path
import sys
import warnings

import av
import numpy as np
from PIL import Image, ImageOps
import pypdfium2 as pdfium
import sentence_transformers
import torch
import transformers

DIMENSIONS = 768
MAX_PIXELS = 32_000_000
MAX_SECONDS = 300
AUDIO_SECONDS = 20
VIDEO_SECONDS = 16
TARGET_SAMPLE_RATE = 16_000
PDF_PAGES = 24

Image.MAX_IMAGE_PIXELS = MAX_PIXELS
warnings.simplefilter("error", Image.DecompressionBombWarning)

IMAGE_FORMATS = {
    "image/png": "PNG", "image/jpeg": "JPEG", "image/webp": "WEBP",
    "image/gif": "GIF", "image/tiff": "TIFF",
}
AUDIO_FORMATS = {
    "audio/wav": "wav", "audio/x-wav": "wav", "audio/wave": "wav",
    "audio/mpeg": "mp3", "audio/flac": "flac", "audio/ogg": "ogg",
    "audio/mp4": "mov", "audio/aac": "aac", "audio/webm": "matroska",
}
VIDEO_FORMATS = {
    "video/mp4": "mov", "video/quicktime": "mov",
    "video/webm": "matroska", "video/x-matroska": "matroska",
}


def fingerprint(directory, device):
    digest = hashlib.sha256()
    versions = (
        torch.__version__, transformers.__version__, sentence_transformers.__version__,
        av.__version__, Image.__version__, device,
    )
    digest.update(b"embeddinggemma2-media-v1")
    digest.update(repr(versions).encode())
    for path in sorted(directory.rglob("*")):
        relative = path.relative_to(directory)
        if not path.is_file() or any(part.startswith(".") for part in relative.parts):
            continue
        if path.suffix not in {".json", ".safetensors", ".model", ".jinja", ".txt"}:
            continue
        digest.update(relative.as_posix().encode() + b"\0")
        with path.open("rb") as source:
            digest.update(hashlib.file_digest(source, "sha256").digest())
    return digest.hexdigest()


def whole():
    return {"type": "whole"}


def interval(start_ms, end_ms):
    return {"type": "time", "start_ms": start_ms, "end_ms": max(start_ms + 1, end_ms)}


def bounded_image(image, bound):
    if image.width * image.height > MAX_PIXELS:
        raise ValueError("image exceeds pixel budget")
    image.thumbnail((bound, bound))
    return image.convert("RGB")


def audio_parts(data, format_name, allow_missing=False):
    resampler = av.AudioResampler(format="fltp", layout="mono", rate=TARGET_SAMPLE_RATE)
    limit = MAX_SECONDS * TARGET_SAMPLE_RATE
    chunks = []
    count = 0
    limited = False
    with av.open(io.BytesIO(data), mode="r", format=format_name, options={"protocol_whitelist": "pipe"}) as source:
        if not source.streams.audio:
            if allow_missing:
                return [], False
            raise ValueError("no audio track")
        for decoded in chain(source.decode(source.streams.audio[0]), [None]):
            for frame in resampler.resample(decoded):
                waveform = frame.to_ndarray().reshape(-1)
                remaining = limit - count
                chunks.append(waveform[:remaining])
                count += min(len(waveform), remaining)
                limited |= len(waveform) > remaining
            if limited:
                break
    if not count:
        return [], limited
    waveform = np.concatenate(chunks)
    width = AUDIO_SECONDS * TARGET_SAMPLE_RATE
    parts = []
    for start in range(0, len(waveform), width):
        end = min(len(waveform), start + width)
        selector = interval(start * 1000 // TARGET_SAMPLE_RATE, end * 1000 // TARGET_SAMPLE_RATE)
        parts.append((selector, {"audio": waveform[start:end]}))
    return parts, limited


def video_parts(data, format_name):
    parts = []
    frames = []
    start_ms = last_ms = next_sample = 0
    limited = False
    with av.open(io.BytesIO(data), mode="r", format=format_name, options={"protocol_whitelist": "pipe"}) as video:
        if not video.streams.video:
            raise ValueError("no video track")
        stream = video.streams.video[0]
        rate = float(stream.average_rate or 25)
        origin = None
        for index, frame in enumerate(video.decode(stream)):
            timestamp = frame.time if frame.time is not None else index / rate
            if origin is None:
                origin = timestamp
            time_ms = max(0, round((timestamp - origin) * 1000))
            if time_ms >= MAX_SECONDS * 1000 or index >= 18_000:
                limited = True
                break
            last_ms = time_ms
            if time_ms < next_sample:
                continue
            if frames and time_ms >= start_ms + VIDEO_SECONDS * 1000:
                parts.append((interval(start_ms, time_ms), {"video": frames}))
                frames = []
            if not frames:
                start_ms = time_ms
            if frame.width * frame.height > MAX_PIXELS:
                raise ValueError("video frame exceeds pixel budget")
            frames.append(bounded_image(frame.to_image(), 480))
            next_sample = time_ms + 1000
        if frames:
            parts.append((interval(start_ms, last_ms + max(1, round(1000 / rate))), {"video": frames}))
    return parts, limited


def pdf_parts(data):
    parts = []
    with closing(pdfium.PdfDocument(data)) as document:
        for index in range(min(len(document), PDF_PAGES)):
            with closing(document[index]) as page:
                width, height = page.get_size()
                with closing(page.render(scale=min(2.0, 1536.0 / max(width, height)))) as bitmap:
                    image = bounded_image(bitmap.to_pil(), 1536)
            parts.append(({"type": "page", "number": index + 1}, {"image": image}))
        return parts, len(document) > PDF_PAGES


def decode_media(media_type, data):
    if media_type in IMAGE_FORMATS:
        with Image.open(io.BytesIO(data), formats=[IMAGE_FORMATS[media_type]]) as image:
            limited = getattr(image, "n_frames", 1) > 1
            return [(whole(), {"image": bounded_image(ImageOps.exif_transpose(image), 1536)})], limited
    if media_type in AUDIO_FORMATS:
        return audio_parts(data, AUDIO_FORMATS[media_type])
    if media_type in VIDEO_FORMATS:
        format_name = VIDEO_FORMATS[media_type]
        video, limited = video_parts(data, format_name)
        audio, audio_limited = audio_parts(data, format_name, allow_missing=True)
        return video + audio, limited or audio_limited
    if media_type == "application/pdf":
        return pdf_parts(data)
    return [], True


def respond(model, message):
    if message["kind"] == "text":
        return model.encode(
            message["text"], prompt_name=message["prompt"], normalize_embeddings=True,
            show_progress_bar=False,
        ).tolist()
    data = base64.b64decode(message["data"], validate=True)
    try:
        parts, limited = decode_media(message["media_type"], data)
    except (ValueError, OSError, av.FFmpegError, pdfium.PdfiumError,
            Image.DecompressionBombError, Image.DecompressionBombWarning):
        return {"parts": [], "limited": True}
    vectors = []
    for selector, sample in parts:
        options = {}
        if "video" in sample:
            options = {"processing_kwargs": {"video": {"fps": None, "max_frames": VIDEO_SECONDS}}}
        vector = model.encode(sample, normalize_embeddings=True, show_progress_bar=False, **options)
        vectors.append({"span": selector, "vector": vector.tolist()})
    return {"parts": vectors, "limited": limited or not vectors}


def main():
    directory = Path(sys.argv[1])
    device = os.environ.get("NAOME_SEARCH_DEVICE", "cpu")
    configuration = json.loads((directory / "config.json").read_text())
    if "EmbeddingGemma2Model" not in configuration.get("architectures", []):
        raise ValueError("the supplied checkpoint is not EmbeddingGemma 2")
    torch.set_num_threads(min(4, os.cpu_count() or 1))
    key = fingerprint(directory, device)
    with redirect_stdout(sys.stderr):
        model = sentence_transformers.SentenceTransformer(
            str(directory), device=device, local_files_only=True, trust_remote_code=False,
            model_kwargs={"use_safetensors": True, "dtype": torch.float32},
        )
    if model.get_embedding_dimension() != DIMENSIONS:
        raise ValueError("unexpected EmbeddingGemma 2 projection size")
    print(json.dumps({"fingerprint": key, "dimensions": DIMENSIONS}), flush=True)
    for line in sys.stdin:
        try:
            with redirect_stdout(sys.stderr), torch.inference_mode():
                response = respond(model, json.loads(line))
            print(json.dumps(response, allow_nan=False), flush=True)
        except Exception as error:
            print(json.dumps({"error": f"{type(error).__name__}: {error}"}), flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"error": f"{type(error).__name__}: {error}"}), flush=True)
        sys.exit(1)
