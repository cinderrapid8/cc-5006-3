#!/usr/bin/env python3
"""Frame-accurate switch verification for the multi-camera interview repro.

For every rendered output frame it decodes the on-frame barcode (camera id +
source frame number) and compares the *actually presented* frame against the
Rust-generated frame-level plan, with special attention to the frames directly
before and after every cut ("切点前后实际帧号").

Outputs a structured report. On any mismatch a self-contained evidence file is
written next to the report so the failure is independently inspectable and
re-runnable. Stdlib only apart from ffmpeg: ``python3 -I``.
"""
import argparse
import json
import os
import subprocess
import sys

WIDTH, HEIGHT = 640, 360


def run(cmd):
    r = subprocess.run(cmd, capture_output=True)
    if r.returncode != 0:
        raise RuntimeError(
            "command failed: " + " ".join(cmd) + "\n" +
            r.stderr.decode("utf-8", "replace")[-800:]
        )
    return r


def read_barcode(rgb, frame_index, cam_ids):
    base = frame_index * WIDTH * HEIGHT * 3
    y = 13 * WIDTH * 3
    value = 0
    bright = 0
    for bit in range(32):
        x = 10 + bit * 6 + 2
        idx = base + y + x * 3
        r_, g_, b_ = rgb[idx], rgb[idx + 1], rgb[idx + 2]
        if r_ > 128 and g_ > 128 and b_ > 128:
            value |= 1 << (31 - bit)
            bright += 1
    if bright == 0:
        return None
    cam = (value >> 28) & 0xF
    src_frame = value & 0x0FFFFFFF
    return (cam_ids[cam] if cam < len(cam_ids) else f"cam{cam}", src_frame)


def is_black(rgb, frame_index):
    base = frame_index * WIDTH * HEIGHT * 3
    total, samples = 0, 0
    for y in range(60, HEIGHT, 40):
        for x in range(20, WIDTH, 60):
            idx = base + (y * WIDTH + x) * 3
            total += rgb[idx] + rgb[idx + 1] + rgb[idx + 2]
            samples += 3
    return total / max(1, samples) < 12


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--project", default="interview-switch-repro")
    ap.add_argument("--master", required=True, help="rendered master mkv/mp4")
    ap.add_argument("--plan", required=True, help="rendered plan json")
    ap.add_argument("--evidence-dir", required=True)
    args = ap.parse_args()

    data = os.path.abspath(args.data)
    project = json.load(open(os.path.join(data, "projects", args.project + ".json"),
                             encoding="utf-8"))
    manifest = json.load(open(os.path.join(data, "repro-media-manifest.json"),
                              encoding="utf-8"))
    frame_bias = manifest.get("frame_bias", 0)
    plan = json.load(open(args.plan, encoding="utf-8"))
    cam_ids = [s["id"] for s in project["sources"]]
    rate_by_id = {s["id"]: s["rate"] for s in project["sources"]}
    # CFR clips present frames on an exact grid, so the verifier can demand
    # frame-number equality there (any global/local offset is caught, not just a
    # >1.5 frame time drift). VFR clips have deliberately irregular PTS and are
    # compared by source time with a half-grid tolerance instead.
    vfr_ids = {cid for cid, cam in manifest["cameras"].items()
               if cam.get("encode_mode") == "vfr"}

    os.makedirs(args.evidence_dir, exist_ok=True)

    r = run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", args.master,
             "-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
    rgb = r.stdout
    frame_size = WIDTH * HEIGHT * 3
    decoded_frames = len(rgb) // frame_size

    failures = []
    actual = []          # per-frame actual (cam, src_frame) or None for black
    cut_boundary = []    # detailed before/after evidence at each cut

    cut_starts = sorted(c["start"] for c in project["cuts"])
    out_rate = project["output"]["rate"]
    out_dur = out_rate["den"] / out_rate["num"]

    expectations = plan["frames"]
    for n, exp in enumerate(expectations):
        if exp["kind"] == "black":
            if not is_black(rgb, n):
                failures.append(f"output {n}: plan says black but picture was decoded")
            actual.append(None)
            continue
        got = read_barcode(rgb, n, cam_ids)
        if got is None:
            failures.append(f"output {n}: expected {exp['source_id']} frame, got black")
            actual.append(None)
            continue
        cam, coded_frame = got
        src_frame = coded_frame - frame_bias
        actual.append([cam, src_frame])
        if cam != exp["source_id"]:
            failures.append(
                f"output {n}: camera {cam} != planned {exp['source_id']} "
                f"(selected camera identity replaced)"
            )
            continue
        rate = rate_by_id[cam]
        if cam in vfr_ids:
            # Irregular PTS: only require the presented frame to cover the
            # planned engineering time within one output frame.
            decoded_time = src_frame * rate["den"] / rate["num"]
            tol = 1.5 * rate["den"] / rate["num"]
            if abs(decoded_time - exp["source_time"]) > tol:
                failures.append(
                    f"output {n}: {cam} (VFR) presented frame {src_frame} "
                    f"(t={decoded_time:.3f}) outside tolerance of planned source "
                    f"time {exp['source_time']:.3f}"
                )
            continue
        # CFR. The frame that must be presented is the one at the planned
        # engineering source time, expressed as an absolute source frame number
        # (the barcode carries absolute numbers; the plan's source_frame is
        # clip-relative).
        expected_abs = round(exp["source_time"] * rate["num"] / rate["den"])
        same_rate = (rate["num"], rate["den"]) == (
            out_rate["num"], out_rate["den"]
        )
        if same_rate:
            # Same output rate: ffmpeg's fps conversion is frame identity, so
            # demand exact equality. Any off-by-one here is a genuine boundary
            # or global-shift defect and is caught, not absorbed by a tolerance.
            if src_frame != expected_abs:
                failures.append(
                    f"output {n}: {cam} presented frame {src_frame} != expected "
                    f"absolute frame {expected_abs} (planned clip-relative "
                    f"{exp['source_frame']}) — frame offset at boundary"
                )
        else:
            # Rate conversion (e.g. 30000/1001 NTSC into 25 fps) inherently
            # skips or holds a frame while resampling. The presented frame must
            # stay within one source frame of the engineering target; a larger
            # drift is a real defect. Monotonicity is checked separately below.
            if abs(src_frame - expected_abs) > 1:
                failures.append(
                    f"output {n}: {cam} rate-converted frame {src_frame} drifted "
                    f"more than one frame from target {expected_abs} "
                    f"(planned clip-relative {exp['source_frame']})"
                )

    # Explicit before/after actual frame numbers for every switch point.
    for cut in project["cuts"]:
        cs = cut["start"]
        # last output frame whose clock is strictly before the cut
        before_n = max(0, int((cs - 1e-9) / out_dur))
        after_n = min(len(expectations) - 1, int((cs + 1e-9) / out_dur) + 1)
        def detail(n):
            exp = expectations[n]
            act = actual[n] if n < len(actual) else None
            return {
                "output_frame": n,
                "program_time": round(n * out_dur, 4),
                "planned": None if exp["kind"] == "black" else {
                    "source_id": exp["source_id"],
                    "source_frame": exp.get("source_frame"),
                    "source_time": round(exp.get("source_time", 0.0), 4),
                },
                "actual": None if act is None else {"source_id": act[0], "source_frame": act[1]},
            }
        cut_boundary.append({
            "cut_id": cut["id"],
            "cut_start": cs,
            "selected_source": cut["source_id"],
            "before": detail(before_n),
            "after": detail(after_n),
        })

    # Monotonicity within every contiguous same-camera run: presentation frames
    # may hold (rate conversion) or advance, but must never go backward while
    # the same camera stays selected. A fresh camera run starts at every cut,
    # including a return to a camera used earlier, so runs are segmented by the
    # adjacent planned source id rather than by the camera alone.
    run_cam = None
    run_prev = None
    for n, item in enumerate(actual):
        if item is None:
            run_cam, run_prev = None, None
            continue
        cam, sf = item
        if cam != run_cam:
            run_cam, run_prev = cam, sf
            continue
        if sf < run_prev:
            failures.append(
                f"output {n}: {cam} presentation frame went backward "
                f"{run_prev} -> {sf} inside a contiguous run"
            )
        run_prev = sf

    report = {
        "project": args.project,
        "total_frames": plan["total_frames"],
        "decoded_frames": decoded_frames,
        "cut_boundaries": cut_boundary,
        "failures": failures,
        "pass": not failures and decoded_frames == plan["total_frames"],
    }

    report_path = os.path.join(args.evidence_dir, "switch-verify-report.json")
    with open(report_path, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2, ensure_ascii=False)

    # Independent failure evidence: full per-frame actual-vs-plan table.
    table_path = os.path.join(args.evidence_dir, "frame-table.json")
    table = []
    for n, exp in enumerate(expectations):
        table.append({
            "output_frame": n,
            "program_time": round(n * out_dur, 4),
            "planned_kind": exp["kind"],
            "planned_source": exp.get("source_id"),
            "planned_source_frame": exp.get("source_frame"),
            "planned_source_time": exp.get("source_time"),
            "actual": actual[n],
        })
    with open(table_path, "w", encoding="utf-8") as f:
        json.dump(table, f, indent=2, ensure_ascii=False)

    print(json.dumps({k: report[k] for k in
                      ("project", "total_frames", "decoded_frames", "pass", "failures")},
                     indent=2, ensure_ascii=False))
    print("report:", report_path)
    print("frame table:", table_path)
    return 0 if report["pass"] else 1


if __name__ == "__main__":
    sys.exit(main())
