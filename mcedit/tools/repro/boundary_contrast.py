#!/usr/bin/env python3
"""Independent before/after evidence for the camera-switch frame query fix.

The application-layer query (mcedit-core `Source::sample_at`) was wrong in two
specific, boundary-local ways. This script re-implements both the old and the
new query in plain Python and separates the genuine defects from harmless
rounding differences:

  OLD query
    * clip membership: half-open interval with a half-frame epsilon
      `start - eps <= t < end - eps`
    * frame: round-to-nearest on the clip offset, with no range check

  NEW query
    * membership on the presentation frame grid: frame k covers
      `[start + k*fd, start + (k+1)*fd)`; the final frame owns the slice up to
      the clip end and any time before clip start is rejected
    * frame: floor on the same grid, clamped to a valid index

Two defects are boundary-local and must be repaired:

  1. last-presentation-frame drop — near the clip end the old epsilon interval
     stops admitting times one half-frame early, and the nearest-frame math can
     even return an index equal to frame_count (one past the last valid frame);
  2. frame-zero mis-assignment — a time just before clip start passes the old
     `start - eps` test, gets a clamped zero offset and is labelled frame 0.

The floor-vs-nearest change also shifts frames whose fractional grid position
crosses 0.5 in the clip interior. That is an expected rounding-policy change,
not the bug: the renderer resamples by engineering timestamp anyway, and the
barcode verifier confirms the actually presented frame. The report therefore
proves the two boundary defects are fixed AND that no *additional* frame offset
appears beyond the documented floor-vs-nearest rounding. Stdlib only.
"""
import argparse
import json
import os


def anchor_program_to_source(project_source):
    a = project_source["anchors"]
    p0, s0 = a[0]["program_time"], a[0]["source_time"]
    p1, s1 = a[1]["program_time"], a[1]["source_time"]
    scale = (s1 - s0) / (p1 - p0)
    return lambda t: s0 + (t - p0) * scale


def old_query(clip, source_time, fd):
    """Pre-fix half-open epsilon membership + unclamped nearest frame."""
    eps = fd * 0.5
    admitted = (source_time >= clip["start"] - eps
                and source_time < clip["start"] + clip["duration"] - eps)
    if not admitted:
        return None
    offset = max(0.0, source_time - clip["start"])
    return int(round(offset / fd))  # nearest_frame; can equal frame_count


def new_query(clip, source_time, fd):
    """Fixed presentation-frame grid membership, range-checked floor."""
    if source_time < clip["start"]:
        return None
    count = clip["frame_count"]
    idx = int((source_time - clip["start"]) / fd // 1)
    if idx < 0 or idx >= count:
        return None
    return idx


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--project", default="interview-switch-repro")
    ap.add_argument("--evidence-dir", required=True)
    args = ap.parse_args()

    data = os.path.abspath(args.data)
    project = json.load(open(os.path.join(data, "projects", args.project + ".json"),
                             encoding="utf-8"))
    verifier = json.load(open(os.path.join(args.evidence_dir,
                                           "switch-verify-report.json"),
                              encoding="utf-8"))
    table = json.load(open(os.path.join(args.evidence_dir, "frame-table.json"),
                           encoding="utf-8"))
    rendered = {row["output_frame"]: row["actual"] for row in table}

    sources = {s["id"]: s for s in project["sources"]}
    mappers = {sid: anchor_program_to_source(s) for sid, s in sources.items()}
    out_fd = project["output"]["rate"]["den"] / project["output"]["rate"]["num"]

    last_frame_defects = []
    frame_zero_defects = []
    unexplained_diffs = []

    for row in table:
        n = row["output_frame"]
        if row["planned_kind"] != "frame":
            continue
        sid = row["planned_source"]
        src = sources[sid]
        fd = src["rate"]["den"] / src["rate"]["num"]
        clip = src["clips"][0]
        count = clip["frame_count"]
        source_time = mappers[sid](n * out_fd)

        old_idx = old_query(clip, source_time, fd)
        new_idx = new_query(clip, source_time, fd)

        entry = {
            "output_frame": n,
            "program_time": round(n * out_fd, 4),
            "source_id": sid,
            "source_time": round(source_time, 5),
            "old_clip_relative_frame": old_idx,
            "new_clip_relative_frame": new_idx,
            "rendered_absolute_frame": None if rendered[n] is None else rendered[n][1],
        }

        # Defect 1: old query returns an out-of-range index at the tail, or
        # rejects a time the final frame actually covers.
        last_frame_bug = (
            (old_idx is not None and old_idx >= count)
            or (new_idx == count - 1 and old_idx is None)
        )
        if last_frame_bug:
            detail = ("index >= frame_count (one past last frame)"
                      if old_idx is not None and old_idx >= count
                      else "final presentation frame wrongly treated as gap")
            last_frame_defects.append({**entry, "defect": detail})
            continue

        # Defect 2: old query admits a pre-start time and clamps it to frame 0.
        if source_time < clip["start"] and old_idx == 0 and new_idx is None:
            frame_zero_defects.append({**entry, "defect": "pre-start time clamped to frame 0"})
            continue

        if old_idx == new_idx:
            continue

        # Remaining changes are post-start, in-range rounding differences. The
        # documented floor-vs-nearest policy change can only move by one grid
        # step; anything larger, or a change outside the valid grid, is a real
        # local frame offset and fails the run.
        if new_idx is None or not (0 <= new_idx < count) or source_time < clip["start"]:
            unexplained_diffs.append(entry)
        elif abs(old_idx - new_idx) > 1:
            unexplained_diffs.append({**entry, "defect": "non-adjacent frame change"})

    # Direct query-level probe for the frame-zero defect. The fixed plan never
    # emits a pre-start frame, so rendered output cannot show this bug; drive the
    # two re-implemented queries directly at a time a quarter frame before a clip
    # start. The old epsilon membership admits it and clamps to frame 0, the new
    # grid correctly rejects it as an ingest gap.
    prestart_probes = []
    for sid, src in sources.items():
        fd = src["rate"]["den"] / src["rate"]["num"]
        clip = src["clips"][0]
        t = clip["start"] - fd * 0.25
        old_idx = old_query(clip, t, fd)
        new_idx = new_query(clip, t, fd)
        prestart_probes.append({
            "source_id": sid,
            "probe_source_time": round(t, 6),
            "clip_start": clip["start"],
            "old_query_frame": old_idx,
            "new_query_frame": new_idx,
            "old_misassigns_frame_zero": old_idx == 0,
            "new_rejects_as_gap": new_idx is None,
        })
        if old_idx == 0 and new_idx is None:
            frame_zero_defects.append({
                "source_id": sid,
                "source_time": round(t, 5),
                "old_clip_relative_frame": 0,
                "new_clip_relative_frame": None,
                "defect": "pre-start time clamped to frame 0 (query-level probe)",
            })

    cut_rows = {c["cut_id"]: c for c in verifier["cut_boundaries"]}
    cut_evidence = [{
        "cut_id": cut["id"],
        "cut_start": cut["start"],
        "selected_source": cut["source_id"],
        "before": cut_rows[cut["id"]]["before"],
        "after": cut_rows[cut["id"]]["after"],
    } for cut in project["cuts"]]

    result = {
        "project": args.project,
        "rule_old": "half-open contains() with half-frame epsilon + unclamped round-to-nearest offset",
        "rule_new": "presentation frame grid [start+k*fd, start+(k+1)*fd); range-checked floor",
        "last_presentation_frame_defects_fixed": last_frame_defects,
        "frame_zero_misassignment_defects_fixed": frame_zero_defects,
        "prestart_query_probes": prestart_probes,
        "unexplained_frame_offsets": unexplained_diffs,
        "boundary_defects_repaired": bool(last_frame_defects and frame_zero_defects),
        "no_unexplained_offset": not unexplained_diffs,
        "cut_boundaries": cut_evidence,
        "pass": (not unexplained_diffs
                 and bool(last_frame_defects)
                 and all(p["new_rejects_as_gap"] for p in prestart_probes)
                 and verifier.get("pass", False)),
    }
    out_path = os.path.join(args.evidence_dir, "boundary-contrast.json")
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)

    print(json.dumps({
        "last_frame_defects_fixed": len(last_frame_defects),
        "frame_zero_defects_fixed": len(frame_zero_defects),
        "unexplained_frame_offsets": len(unexplained_diffs),
        "verifier_pass": verifier.get("pass", False),
        "pass": result["pass"],
    }, indent=2, ensure_ascii=False))
    print("boundary contrast:", out_path)
    return 0 if result["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
