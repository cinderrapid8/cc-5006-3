#!/usr/bin/env python3
"""End-to-end acceptance verification for mcedit.

For every rendered output frame it:
  * decodes the 32-bit barcode (camera id + source frame number),
  * compares against the Rust-generated frame-level plan,
  * checks visual continuity (no unexpected jumps beyond planned cuts),
  * verifies black intervals and fallback substitutions,
  * verifies the audio is a single continuous master with one blip per second,
  * re-renders and compares decoded content hashes for reproducibility.

Runs with the standard library only: ``python3 -I verify_render.py ...``.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys

WIDTH, HEIGHT = 640, 360
CAM_IDS = ["host", "guest", "wide"]


def run(cmd):
    r = subprocess.run(cmd, capture_output=True)
    if r.returncode != 0:
        raise RuntimeError(
            f"command failed: {' '.join(cmd)}\n{r.stderr[-800:].decode('utf-8', 'replace')}"
        )
    return r


def decode_raw_yuv(master, frames):
    # RGB is easier for barcode sampling; ffmpeg converts the master.
    r = run([
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-i", master,
        "-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "rgb24", "-",
    ])
    frame_size = WIDTH * HEIGHT * 3
    if len(r.stdout) < frames * frame_size:
        raise RuntimeError(
            f"decoded {len(r.stdout) // frame_size} frames, expected {frames}"
        )
    return r.stdout


def decode_audio_pcm(master, sample_rate):
    r = run([
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-i", master,
        "-map", "0:a:0", "-ac", "2", "-ar", str(sample_rate), "-f", "s16le", "-",
    ])
    return r.stdout

def read_barcode(rgb, frame_index):
    """Return (camera_id, source_frame) or None for a black frame."""
    base = frame_index * WIDTH * HEIGHT * 3
    # Barcode occupies y=8..18, 32 bits starting at x=10, each bit 6px wide.
    # Sample the center of each bit cell.
    y = 13 * WIDTH * 3
    value = 0
    bright_count = 0
    for bit in range(32):
        x = 10 + bit * 6 + 2
        idx = base + y + x * 3
        r_, g_, b_ = rgb[idx], rgb[idx + 1], rgb[idx + 2]
        on = r_ > 128 and g_ > 128 and b_ > 128
        if on:
            value |= 1 << (31 - bit)
            bright_count += 1
    if bright_count == 0:
        return None
    cam = (value >> 28) & 0xF
    src_frame = value & 0x0FFFFFFF
    return CAM_IDS[cam] if cam < len(CAM_IDS) else f"cam{cam}", src_frame


def is_black_frame(rgb, frame_index):
    base = frame_index * WIDTH * HEIGHT * 3
    # Sample a grid away from the (black) barcode strip.
    total = 0
    samples = 0
    for y in range(60, HEIGHT, 40):
        for x in range(20, WIDTH, 60):
            idx = base + (y * WIDTH + x) * 3
            total += rgb[idx] + rgb[idx + 1] + rgb[idx + 2]
            samples += 3
    return total / max(1, samples) < 12


def count_blips(pcm, sample_rate):
    # Stereo s16le. Count distinct loud bursts on the left channel: a blip
    # exists when a sliding 20 ms window's peak crosses a threshold, and we
    # count rising edges separated by at least 300 ms.
    import array
    samples = array.array("h")
    samples.frombytes(pcm)
    left = samples[0::2]
    window = sample_rate // 50
    threshold = 9000
    blips = 0
    in_blip = False
    quiet_for = 0
    i = 0
    while i + window < len(left):
        peak = max(abs(v) for v in left[i:i + window])
        if peak > threshold:
            if not in_blip:
                blips += 1
                in_blip = True
            quiet_for = 0
        else:
            quiet_for += window
            if quiet_for > sample_rate * 0.3:
                in_blip = False
        i += window
    return blips


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--project", required=True)
    ap.add_argument("--mcedit", required=True, help="path to mcedit binary")
    args = ap.parse_args()

    data = os.path.abspath(args.data)
    project_path = os.path.join(data, "projects", args.project + ".json")
    project = json.load(open(project_path, encoding="utf-8"))
    renders = os.path.join(data, "renders")

    # Re-render twice into isolated directories and compare decoded hashes.
    run([args.mcedit, "render", project_path, "--base", data, "--out", renders])
    sidecars = sorted(
        f for f in os.listdir(renders)
        if f.startswith(args.project + ".") and f.endswith(".sidecar.json")
    )
    if not sidecars:
        raise RuntimeError("no sidecar produced")
    sidecar = json.load(open(os.path.join(renders, sidecars[-1]), encoding="utf-8"))
    revision = sidecar["revision"]
    master = os.path.join(renders, sidecar["master_file"])
    plan = json.load(open(
        os.path.join(renders, f"{args.project}.{revision}.plan.json"), encoding="utf-8"
    ))

    failures = []
    total_frames = plan["total_frames"]
    rgb = decode_raw_yuv(master, total_frames)
    pcm = decode_audio_pcm(master, project["output"]["sample_rate"])

    # 1. Frame-by-frame agreement with the plan.
    black_seen = 0
    fallback_seen = 0
    source_frames = {sid: [] for sid in CAM_IDS}
    prev_by_cam = {}
    discontinuities = []

    for exp in plan["frames"]:
        n = exp["output_frame"]
        if exp["kind"] == "black":
            if not is_black_frame(rgb, n):
                failures.append(f"frame {n}: expected black, got picture")
            else:
                black_seen += 1
            continue
        decoded = read_barcode(rgb, n)
        if decoded is None:
            failures.append(f"frame {n}: expected {exp['source_id']} barcode, got black")
            continue
        cam, src_frame = decoded
        if cam != exp["source_id"]:
            failures.append(
                f"frame {n}: barcode camera {cam} != plan {exp['source_id']}"
            )
            continue
        # Compare by source time, which is independent of whether the plan's
        # source_frame is clip-relative while the barcode is absolute.
        source = next(s for s in project["sources"] if s["id"] == cam)
        rate = source["rate"]
        decoded_time = src_frame * rate["den"] / rate["num"]
        tolerance = 1.5 * rate["den"] / rate["num"]
        if abs(decoded_time - exp["source_time"]) > tolerance:
            failures.append(
                f"frame {n}: {cam} barcode time {decoded_time:.3f} "
                f"!= plan source time {exp['source_time']:.3f}"
            )
        if exp.get("fallback"):
            fallback_seen += 1
        source_frames[cam].append(src_frame)

        # Continuity within one camera: consecutive used frames must advance by
        # at most 2 source frames (rate conversion) and never go backwards.
        if cam in prev_by_cam:
            delta = src_frame - prev_by_cam[cam]
            if delta < 0:
                discontinuities.append((n, cam, delta))
            elif delta > 2:
                # A large jump is fine only across a planned cut; detect that
                # by checking the plan changed source at this output frame.
                prev_exp = plan["frames"][n - 1]
                same_cut = (
                    prev_exp["kind"] != "black"
                    and prev_exp.get("source_id") == cam
                    and exp.get("source_id") == cam
                )
                if same_cut:
                    discontinuities.append((n, cam, delta))
        prev_by_cam[cam] = src_frame

    # 2. Every expected black/fallback interval actually occurred.
    expected_black = sum(1 for f in plan["frames"] if f["kind"] == "black")
    expected_fallback = sum(1 for f in plan["frames"] if f.get("fallback"))
    if black_seen != expected_black:
        failures.append(f"black frames {black_seen} != expected {expected_black}")
    if fallback_seen != expected_fallback:
        failures.append(f"fallback frames {fallback_seen} != expected {expected_fallback}")
    if discontinuities:
        failures.append(f"non-monotonic/jumpy source frames: {discontinuities[:5]}")

    # 3. Audio: single continuous master, one blip per program second.
    expected_samples = project["output"]["sample_rate"] * int(project["output"]["duration"]) * 2
    if len(pcm) < expected_samples * 0.95:
        failures.append(f"audio too short: {len(pcm)} bytes vs {expected_samples}")
    blips = count_blips(pcm, project["output"]["sample_rate"])
    # Blips are expected only where the single master actually carries sound;
    # silence gaps (e.g. the host-camera hole) correctly have none.
    expected_blips = 0
    for seg in plan["audio_segments"]:
        if not seg["silence"]:
            expected_blips += int(seg["duration"])
    if not (expected_blips - 1 <= blips <= expected_blips + 1):
        failures.append(
            f"audio blip count {blips} outside {expected_blips}±1 "
            "(single master continuity broken)"
        )

    # 4. Reproducibility: second render must hash identically (decoded content).
    repro_dir = os.path.join(renders, f"repro-{revision}")
    run([args.mcedit, "render", project_path, "--base", data, "--out", repro_dir])
    second_master = os.path.join(repro_dir, sidecar["master_file"])
    h1 = hashlib.sha256(rgb).hexdigest()
    rgb2 = decode_raw_yuv(second_master, total_frames)
    h2 = hashlib.sha256(rgb2).hexdigest()
    pcm2 = decode_audio_pcm(second_master, project["output"]["sample_rate"])
    ah1 = hashlib.sha256(pcm).hexdigest()
    ah2 = hashlib.sha256(pcm2).hexdigest()
    reproducible = h1 == h2 and ah1 == ah2
    if not reproducible:
        failures.append("re-render decoded hashes differ")

    result = {
        "project": args.project,
        "revision": revision,
        "total_frames": total_frames,
        "black_frames": black_seen,
        "fallback_frames": fallback_seen,
        "audio_blips": blips,
        "video_decoded_sha256": h1,
        "audio_decoded_sha256": ah1,
        "reproducible": reproducible,
        "failures": failures,
        "pass": not failures,
    }
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
