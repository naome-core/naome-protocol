"""Check two disjoint audio segments against the original container clock."""
from fractions import Fraction
import importlib.util
from pathlib import Path
import tempfile

import av
import numpy as np

root = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("archive_worker", root / "src/search/gemma_worker.py")
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)

with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "delayed-audio.mkv"
    with av.open(str(path), "w") as output:
        rate = 16000
        video = output.add_stream("mpeg4", rate=1)
        video.width = video.height = 32
        video.pix_fmt = "yuv420p"
        audio = output.add_stream("pcm_s16le", rate=rate)
        audio.layout = "mono"
        for second in range(8):
            frame = av.VideoFrame.from_ndarray(np.full((32, 32, 3), second, dtype="uint8"), format="rgb24")
            frame.pts = second
            frame.time_base = Fraction(1)
            for packet in video.encode(frame):
                output.mux(packet)
            if second in (3, 6):
                frame = av.AudioFrame.from_ndarray(np.full((1, rate), 700, dtype="int16"), format="s16", layout="mono")
                frame.pts = second * rate
                frame.sample_rate = rate
                frame.time_base = Fraction(1, rate)
                for packet in audio.encode(frame):
                    output.mux(packet)
        for stream in (video, audio):
            for packet in stream.encode():
                output.mux(packet)
    data = path.read_bytes()
    visual, _ = worker.video_parts(data, "matroska")
    sound, limited = worker.audio_parts(data, "matroska")
    assert not limited, sound
    assert visual[0][0]["start_ms"] == 0, visual[0][0]
    selectors = [(span["start_ms"], span["end_ms"]) for span, _ in sound]
    assert selectors == [(3000, 4000), (6000, 7000)], selectors
    print(selectors)
