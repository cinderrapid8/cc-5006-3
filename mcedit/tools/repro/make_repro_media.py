#!/usr/bin/env python3
"""Minimal reproduction corpus for the camera-switch wrong-frame investigation.

Like tools/make_media.py but deliberately stresses the decoder/container layer:

  * every clip is encoded with B-frames so decode order differs from
    presentation order (the manifest records both orderings as evidence),
  * cameras start at different source times and different frame rates,
  * one camera is a short camera (a single short clip, ending mid-program),
  * one clip carries non-uniform (VFR) presentation timestamps,
  * every frame still carries the machine-readable barcode so the verifier can
    read the *presented* source frame number back from any rendered output.

Stdlib only apart from the ffmpeg shell-out. Run with ``python3 -I``.
"""
import argparse
import json
import os
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
from make_media import (  # noqa: E402
    WIDTH,
    HEIGHT,
    SAMPLE_RATE,
    draw_barcode,
    draw_text,
    timecode_text,
)

# camera id -> (rate, counter start, source-time clips, encode mode)
# encode mode: "bframe" (CFR, IPB reordering), "vfr" (non-uniform PTS + B),
#              "short" (short camera with one tiny clip)
CAMERAS = {
    "cam_a": {
        "name": "CAM A",
        "tone": (28, 42, 64),
        "counter_start": 1000,
        "rate": (25, 1),
        "clips": [(0.0, 8.0)],
        "mode": "bframe",
    },
    "cam_b": {
        "name": "CAM B",
        "tone": (52, 30, 34),
        "counter_start": 3000,
        "rate": (25, 1),
        # started two source seconds before program zero
        "clips": [(-2.0, 6.0)],
        "mode": "bframe",
    },
    "cam_c": {
        "name": "CAM C SHORT",
        "tone": (30, 48, 36),
        "counter_start": 7000,
        "rate": (30000, 1001),
        # short camera: only covers the first four program seconds
        "clips": [(0.5, 4.5)],
        "mode": "bframe",
    },
    "cam_v": {
        "name": "CAM V VFR",
        "tone": (46, 40, 24),
        "counter_start": 500,
        "rate": (25, 1),
        "clips": [(0.0, 6.0)],
        "mode": "vfr",
    },
}

TRUE_OFFSETS = {"cam_a": 0.0, "cam_b": -2.0, "cam_c": 0.5, "cam_v": 0.0}

# Barcodes carry a 28-bit unsigned field. Cameras that started before program
# zero have negative source frame numbers, so bias them into the unsigned field
# at encode time; the verifier subtracts the same constant back.
FRAME_BIAS = 8192


def run(cmd):
    r = subprocess.run(cmd, capture_output=True)
    if r.returncode != 0:
        raise RuntimeError(
            "command failed: " + " ".join(cmd) + "\n" + r.stderr.decode("utf-8", "replace")[-800:]
        )
    return r


def render_frames(cam_id, cam, clip_idx, clip_start, clip_end):
    rate_num, rate_den = cam["rate"]
    frame_dur = rate_den / rate_num
    count = round((clip_end - clip_start) * rate_num / rate_den)
    buf = bytearray()
    for f in range(count):
        frame = bytearray(WIDTH * HEIGHT * 3)
        for y in range(HEIGHT):
            shade = 1.0 - 0.35 * y / HEIGHT
            row = bytes(int(c * shade) for c in cam["tone"])
            off = y * WIDTH * 3
            frame[off:off + WIDTH * 3] = row * WIDTH
        source_time = clip_start + f * frame_dur
        source_frame = round(source_time * rate_num / rate_den)
        coded_frame = (source_frame + FRAME_BIAS) & 0x0FFFFFFF
        draw_barcode(frame, list(CAMERAS).index(cam_id), coded_frame)
        draw_text(frame, cam["name"], 10, 34, 3, (255, 255, 255))
        draw_text(frame, f"CNT {cam['counter_start'] + source_frame}", 10, 70, 4, (255, 230, 120))
        draw_text(frame, f"SRCF {source_frame}", 10, 118, 3, (210, 230, 255))
        draw_text(frame, timecode_text(source_time, rate_num, rate_den), 10, 152, 3, (210, 230, 255))
        draw_text(frame, f"CLIP {clip_idx + 1}", 10, 190, 2, (180, 180, 180))
        buf.extend(frame)
    return bytes(buf), count


def silent_wav(duration, path):
    # silent placeholder audio keeps the muxer happy; picture is what matters
    import wave
    n = int(round(duration * SAMPLE_RATE))
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(b"\x00\x00" * n)


def encode(rgb, wav, out, rate, frames, mode):
    num, den = rate
    common = [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        "-f", "rawvideo", "-pix_fmt", "rgb24",
        "-s", f"{WIDTH}x{HEIGHT}", "-r", f"{num}/{den}",
        "-i", rgb, "-i", wav,
        "-c:v", "libx264", "-preset", "medium", "-crf", "18",
        "-g", "6", "-bf", "2", "-pix_fmt", "yuv420p",
        "-c:a", "aac", "-b:a", "64k", "-map_metadata", "-1",
        "-frames:v", str(frames),
    ]
    if mode == "vfr":
        # Perturb PTS (small sinusoidal jitter) and stop the muxer from
        # collapsing it back to CFR. Matroska stores the irregular timestamps.
        out_mkv = out.rsplit(".", 1)[0] + ".mkv"
        # common ends with "-frames:v", str(frames); insert filter + fps mode
        # ahead of those output controls and replace the output path.
        cmd = common[:-2] + [
            "-vf", "setpts=PTS+0.015*sin(N)",
            "-fps_mode", "passthrough",
            "-frames:v", str(frames),
            out_mkv,
        ]
        run(cmd)
        return os.path.basename(out_mkv)
    run(common + [out])
    return os.path.basename(out)


def frame_ordering_evidence(path, rate):
    """Record presentation order and decode order of every frame."""
    num, den = rate
    # presentation order: pts_time + coded_picture_number via ffprobe frames
    r = run([
        "ffprobe", "-v", "error", "-select_streams", "v",
        "-show_entries", "frame=pts_time,pict_type,coded_picture_number",
        "-of", "json", path,
    ])
    info = json.loads(r.stdout.decode("utf-8"))
    presentation = []
    for fr in info.get("frames", []):
        presentation.append({
            "pts_time": round(float(fr.get("pts_time", 0.0)), 6),
            "pict_type": fr.get("pict_type"),
            "coded_picture_number": fr.get("coded_picture_number"),
        })
    # decode order: coded_picture_number reflects decode slot; derive the
    # permutation that turns presentation order into decode order.
    coded = [f["coded_picture_number"] for f in presentation]
    decode_perm = sorted(range(len(coded)), key=lambda i: coded[i])
    return {
        "frame_count": len(presentation),
        "presentation_order": presentation,
        "decode_order_is_presentation_indices": decode_perm,
        "reordered": decode_perm != list(range(len(decode_perm))),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    out = os.path.abspath(args.out)
    media_dir = os.path.join(out, "media")
    tmp_dir = os.path.join(out, "tmp")
    os.makedirs(media_dir, exist_ok=True)
    os.makedirs(tmp_dir, exist_ok=True)

    manifest = {"schema": "mcedit-repro-media/1", "cameras": {}}
    evidence = {"schema": "mcedit-repro-evidence/1", "clips": {}}

    for cam_id, cam in CAMERAS.items():
        cam_dir = os.path.join(media_dir, cam_id)
        os.makedirs(cam_dir, exist_ok=True)
        rate = cam["rate"]
        clip_entries = []
        for idx, (start, end) in enumerate(cam["clips"]):
            tag = f"{cam_id}-{idx + 1}"
            rgb_path = os.path.join(tmp_dir, f"{tag}.rgb")
            wav_path = os.path.join(tmp_dir, f"{tag}.wav")
            with open(rgb_path, "wb") as f:
                raw, frames = render_frames(cam_id, cam, idx, start, end)
                f.write(raw)
            silent_wav(end - start, wav_path)
            out_name = encode(rgb_path, wav_path, os.path.join(cam_dir, f"{tag}.mp4"),
                              rate, frames, cam["mode"])
            produced = os.path.join(cam_dir, out_name)
            ev = frame_ordering_evidence(produced, rate)
            evidence["clips"][f"media/{cam_id}/{out_name}"] = ev
            clip_entries.append({
                "path": f"media/{cam_id}/{out_name}",
                "start": start,
                "duration": round(end - start, 6),
                "frame_count": frames,
                "has_audio": True,
            })
            os.remove(rgb_path)
            os.remove(wav_path)
        manifest["cameras"][cam_id] = {
            "name": cam["name"],
            "role": cam_id,
            "rate": {"num": rate[0], "den": rate[1]},
            "true_offset_source_minus_program": TRUE_OFFSETS[cam_id],
            "counter_start": cam["counter_start"],
            "encode_mode": cam["mode"],
            "clips": clip_entries,
        }

    manifest["frame_bias"] = FRAME_BIAS
    with open(os.path.join(out, "repro-media-manifest.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=2, ensure_ascii=False)
    with open(os.path.join(out, "decode-evidence.json"), "w", encoding="utf-8") as f:
        json.dump(evidence, f, indent=2, ensure_ascii=False)
    reordered = {k: v["reordered"] for k, v in evidence["clips"].items()}
    print(json.dumps({"ok": True, "out": out, "bframe_reordered": reordered}, ensure_ascii=False))


if __name__ == "__main__":
    sys.exit(main())
