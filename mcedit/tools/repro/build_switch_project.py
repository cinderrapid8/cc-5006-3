#!/usr/bin/env python3
"""Build the multi-camera interview project used to verify camera-switch
frame accuracy on B-frame / different-start / non-uniform-timestamp media.

Cuts are deliberately placed both on and off output frame boundaries and inside
the short camera's coverage, so the verifier can compare the *actual* decoded
source frame number against the planned presentation frame on each side of
every switch. Stdlib only: ``python3 -I``.
"""
import argparse
import json
import os


def load_manifest(data_dir):
    with open(os.path.join(data_dir, "repro-media-manifest.json"), encoding="utf-8") as f:
        return json.load(f)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    args = ap.parse_args()
    data_dir = os.path.abspath(args.data)
    manifest = load_manifest(data_dir)

    sources = []
    for cam_id, cam in manifest["cameras"].items():
        offset = cam["true_offset_source_minus_program"]
        # Two exact human anchors per camera; the editor only ever sees these.
        anchors = [
            {"id": f"{cam_id}-a1", "label": "共同动作一", "program_time": 1.0,
             "source_time": round(1.0 + offset, 6), "uncertainty": 0.0},
            {"id": f"{cam_id}-a2", "label": "共同动作二", "program_time": 4.0,
             "source_time": round(4.0 + offset, 6), "uncertainty": 0.0},
        ]
        sources.append({
            "id": cam_id,
            "name": cam["name"],
            "role": cam["role"],
            "rate": cam["rate"],
            "clips": cam["clips"],
            "anchors": anchors,
        })

    # Output is 25 fps, 8 s. Cuts alternate cameras; several are intentionally
    # NOT on a frame boundary and several cross the short cam_c coverage end.
    cuts = [
        (0.0, "cam_a"),
        (1.5, "cam_b"),      # frame-aligned
        (2.002, "cam_a"),    # off-grid cut between frames 50 and 51
        (3.0, "cam_c"),      # NTSC short camera
        (4.003, "cam_v"),    # VFR source, off-grid cut
        (5.0, "cam_a"),
        (5.5, "cam_b"),
        (6.0, "cam_a"),      # cam_b ends at program 6.0; cam_c long gone
    ]
    project = {
        "name": "interview-switch-repro",
        "output": {
            "width": 640,
            "height": 360,
            "rate": {"num": 25, "den": 1},
            "sample_rate": 48000,
            "duration": 8.0,
        },
        "sources": sources,
        "cuts": [
            {"id": f"cut-{i + 1}", "start": round(start, 4), "source_id": sid}
            for i, (start, sid) in enumerate(cuts)
        ],
        # Missing frames follow the existing policy: black, never a guessed cam.
        "gap_policy": {"mode": "black"},
        "audio": {"source_id": "cam_a", "program_start": 0.0, "external_path": None},
    }

    projects_dir = os.path.join(data_dir, "projects")
    os.makedirs(projects_dir, exist_ok=True)
    path = os.path.join(projects_dir, "interview-switch-repro.json")
    with open(path, "w", encoding="utf-8") as f:
        json.dump(project, f, indent=2, ensure_ascii=False)
    print(json.dumps({"wrote": path, "cuts": len(cuts)}, ensure_ascii=False))


if __name__ == "__main__":
    main()
