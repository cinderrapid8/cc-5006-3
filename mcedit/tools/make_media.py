#!/usr/bin/env python3
"""Generate the three-camera interview test corpus for mcedit.

Runs with the standard library only (``python3 -I make_media.py``); ffmpeg is
shelled out for encoding. Every produced frame carries:

  * a human-readable counter whose start number differs per camera,
  * a machine-readable barcode encoding camera id and source frame number,
  * a camera-specific background tint,
  * a matching timecode line.

Cameras have different start times, frame rates and deliberate missing
sections. A sidecar manifest records start timecode, rate, clips and the true
offsets used; the editor never trusts file creation time.
"""

import argparse
import json
import math
import os
import struct
import subprocess
import sys
import wave

WIDTH, HEIGHT = 640, 360
# 5x7 uppercase/digit font, one byte per row, low 5 bits used.
FONT = {
    "0": [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
    "1": [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
    "2": [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
    "3": [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
    "4": [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
    "5": [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
    "6": [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
    "7": [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
    "8": [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
    "9": [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
    "A": [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
    "B": [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
    "C": [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
    "D": [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
    "E": [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
    "F": [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
    "G": [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
    "H": [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
    "I": [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
    "J": [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
    "K": [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
    "L": [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
    "M": [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
    "N": [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
    "O": [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
    "P": [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
    "Q": [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
    "R": [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
    "S": [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
    "T": [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
    "U": [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
    "V": [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
    "W": [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
    "X": [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
    "Y": [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
    "Z": [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
    ":": [0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00],
    "-": [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
    ".": [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
    " ": [0x00] * 7,
}

CAMERAS = {
    "host": {
        "name": "HOST CU",
        "tone": (28, 42, 64),
        "tone_freq": 330.0,
        "counter_start": 1000,
        "rate": (25, 1),
        # source-time (s) start/end for each recorded clip
        "clips": [(1.0, 12.0), (15.0, 31.0)],
    },
    "guest": {
        "name": "GUEST CU",
        "tone": (48, 30, 52),
        "tone_freq": 392.0,
        "counter_start": 5000,
        "rate": (25, 1),
        "clips": [(-2.0, 10.0)],
    },
    "wide": {
        "name": "WIDE SHOT",
        "tone": (30, 48, 36),
        "tone_freq": 262.0,
        "counter_start": 9000,
        "rate": (30000, 1001),
        "clips": [(0.5, 28.5)],
    },
}

# True offsets (source = program + offset), recorded in the manifest only.
TRUE_OFFSETS = {"host": 1.0, "guest": -2.0, "wide": 0.5}
SAMPLE_RATE = 48000


def put_pixel(buf, x, y, color):
    if 0 <= x < WIDTH and 0 <= y < HEIGHT:
        i = (y * WIDTH + x) * 3
        buf[i], buf[i + 1], buf[i + 2] = color


def fill_rect(buf, x0, y0, w, h, color):
    for y in range(max(0, y0), min(HEIGHT, y0 + h)):
        base = (y * WIDTH + max(0, x0)) * 3
        row = bytes(color) * min(w, WIDTH - x0)
        buf[base:base + len(row)] = row


def draw_text(buf, text, x, y, scale, color):
    cursor = x
    for ch in text.upper():
        glyph = FONT.get(ch, FONT[" "])
        for row_idx, bits in enumerate(glyph):
            for col in range(5):
                if bits & (1 << (4 - col)):
                    fill_rect(buf, cursor + col * scale, y + row_idx * scale, scale, scale, color)
        cursor += 6 * scale


def draw_barcode(buf, camera_index, source_frame, y=8):
    # 4-bit camera id (top nibble) + 28-bit frame number, white=1, black=0.
    value = ((camera_index & 0xF) << 28) | (source_frame & 0x0FFFFFFF)
    x0 = 10
    fill_rect(buf, x0 - 2, y - 2, 34 * 6 + 4, 18, (0, 0, 0))
    for bit in range(32):
        on = (value >> (31 - bit)) & 1
        color = (255, 255, 255) if on else (20, 20, 20)
        fill_rect(buf, x0 + bit * 6, y, 5, 10, color)


def timecode_text(source_seconds, rate_num, rate_den):
    fps = round(rate_num / rate_den)
    total = round(source_seconds * rate_num / rate_den)
    sign = "-" if total < 0 else ""
    total = abs(total)
    f = total % fps
    secs = total // fps
    s, m, h = secs % 60, (secs // 60) % 60, secs // 3600
    return f"{sign}{h:02d}:{m:02d}:{s:02d}:{f:02d}"


def render_clip_frames(cam_id, cam, clip_idx, clip_start, clip_end, out_rgb):
    rate_num, rate_den = cam["rate"]
    frame_dur = rate_den / rate_num
    count = round((clip_end - clip_start) * rate_num / rate_den)
    base = cam["tone"]
    # Slight moving band so consecutive frames are visibly distinct beyond the
    # counter, while encoding stays cheap.
    for f in range(count):
        buf = bytearray(WIDTH * HEIGHT * 3)
        # Background: base tone with a vertical gradient.
        for y in range(HEIGHT):
            shade = 1.0 - 0.35 * y / HEIGHT
            row = bytes(int(c * shade) for c in base)
            off = y * WIDTH * 3
            buf[off:off + WIDTH * 3] = row * WIDTH
        sweep = (f * 7) % WIDTH
        fill_rect(buf, sweep, 0, 14, HEIGHT, tuple(min(255, c + 26) for c in base))

        source_time = clip_start + f * frame_dur
        source_frame = round(source_time * rate_num / rate_den)
        counter = cam["counter_start"] + source_frame

        draw_barcode(buf, list(CAMERAS).index(cam_id), source_frame & 0x0FFFFFFF)
        draw_text(buf, cam["name"], 10, 34, 3, (255, 255, 255))
        draw_text(buf, f"CNT {counter}", 10, 70, 4, (255, 230, 120))
        draw_text(buf, f"SRCF {source_frame}", 10, 118, 3, (210, 230, 255))
        draw_text(buf, timecode_text(source_time, rate_num, rate_den), 10, 152, 3, (210, 230, 255))
        draw_text(buf, f"CLIP {clip_idx + 1}", 10, 190, 2, (180, 180, 180))
        out_rgb.write(bytes(buf))
    return count


def synth_audio(cam, clip_start, clip_end, path):
    # Continuous tone + a louder blip at each integer source second, so the
    # verifier can count blips and prove one single, continuous master.
    rate_num, rate_den = cam["rate"]
    duration = clip_end - clip_start
    n = int(round(duration * SAMPLE_RATE))
    freq = cam["tone_freq"]
    with wave.open(path, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(SAMPLE_RATE)
        chunk = bytearray()
        for i in range(n):
            t = i / SAMPLE_RATE
            source_t = clip_start + t
            blip = 0.0
            phase_in_second = source_t - math.floor(source_t)
            if phase_in_second < 0.08:
                blip = 0.35 * math.sin(2 * math.pi * 880.0 * t)
            sample = 0.18 * math.sin(2 * math.pi * freq * t) + blip
            chunk.extend(struct.pack("<h", int(max(-1.0, min(1.0, sample)) * 30000)))
            if len(chunk) >= SAMPLE_RATE * 2:
                wav.writeframes(chunk)
                chunk.clear()
        if chunk:
            wav.writeframes(chunk)


def encode_clip(rgb_path, wav_path, out_path, rate_num, rate_den, frames):
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        "-f", "rawvideo", "-pix_fmt", "rgb24",
        "-s", f"{WIDTH}x{HEIGHT}",
        "-r", f"{rate_num}/{rate_den}",
        "-i", rgb_path,
        "-i", wav_path,
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "18",
        "-pix_fmt", "yuv420p",
        "-c:a", "aac", "-b:a", "96k",
        "-map_metadata", "-1",
        "-frames:v", str(frames),
        out_path,
    ]
    subprocess.run(cmd, check=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True, help="data directory")
    args = ap.parse_args()
    out = os.path.abspath(args.out)
    media_dir = os.path.join(out, "media")
    tmp_dir = os.path.join(out, "tmp")
    os.makedirs(media_dir, exist_ok=True)
    os.makedirs(tmp_dir, exist_ok=True)

    manifest = {"schema": "mcedit-media/1", "cameras": {}}
    for cam_id, cam in CAMERAS.items():
        cam_dir = os.path.join(media_dir, cam_id)
        os.makedirs(cam_dir, exist_ok=True)
        rate_num, rate_den = cam["rate"]
        clip_entries = []
        for idx, (start, end) in enumerate(cam["clips"]):
            tag = f"{cam_id}-{idx + 1}"
            rgb_path = os.path.join(tmp_dir, f"{tag}.rgb")
            wav_path = os.path.join(tmp_dir, f"{tag}.wav")
            out_path = os.path.join(cam_dir, f"{tag}.mp4")
            with open(rgb_path, "wb") as rgb:
                frames = render_clip_frames(cam_id, cam, idx, start, end, rgb)
            synth_audio(cam, start, end, wav_path)
            encode_clip(rgb_path, wav_path, out_path, rate_num, rate_den, frames)
            os.remove(rgb_path)
            os.remove(wav_path)
            clip_entries.append({
                "path": f"media/{cam_id}/{tag}.mp4",
                "start": start,
                "duration": round(end - start, 6),
                "frame_count": frames,
                "has_audio": True,
            })
        manifest["cameras"][cam_id] = {
            "name": cam["name"],
            "role": {"host": "host_closeup", "guest": "guest_closeup", "wide": "wide"}[cam_id],
            "rate": {"num": rate_num, "den": rate_den},
            "true_offset_source_minus_program": TRUE_OFFSETS[cam_id],
            "counter_start": cam["counter_start"],
            "clips": clip_entries,
        }

    with open(os.path.join(out, "media-manifest.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=2, ensure_ascii=False)
    print(json.dumps({"ok": True, "out": out, "cameras": list(CAMERAS)}, ensure_ascii=False))


if __name__ == "__main__":
    sys.exit(main())
