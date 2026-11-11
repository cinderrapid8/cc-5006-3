#!/usr/bin/env python3
"""Build mcedit project JSON files from the generated media manifest.

Projects use only human anchors for alignment (the manifest's true offsets are
used to pick anchor values an operator would enter, but the editor itself only
sees the anchors). Produces the acceptance variants:

  news-main        normal edit with black chosen for the host-camera gap
  news-fallback    same edit with an explicit wide fallback camera
  news-rapid       rapid back-to-back cuts including single-frame switches
  news-damaged     guest camera damaged: its clips are removed from the project
  news-uncertain   anchors carry uncertainty, exercising comparable ranges
"""

import argparse
import json
import os

DURATION = 30.0
OUT_RATE = {"num": 25, "den": 1}


def load_manifest(data_dir):
    with open(os.path.join(data_dir, "media-manifest.json"), encoding="utf-8") as f:
        return json.load(f)


def make_sources(manifest, uncertain=False, damaged=False):
    sources = []
    for cam_id, cam in manifest["cameras"].items():
        offset = cam["true_offset_source_minus_program"]
        anchors = [
            {
                "id": f"{cam_id}-anc-1",
                "label": "共同动作：主持人抬手",
                "program_time": 5.0,
                "source_time": 5.0 + offset,
                "uncertainty": 0.0,
            },
            {
                "id": f"{cam_id}-anc-2",
                "label": "共同动作：嘉宾翻页",
                "program_time": 9.0,
                "source_time": 9.0 + offset,
                "uncertainty": 0.08 if uncertain else 0.0,
            },
        ]
        clips = [] if (damaged and cam_id == "guest") else cam["clips"]
        sources.append(
            {
                "id": cam_id,
                "name": cam["name"],
                "role": cam["role"],
                "rate": cam["rate"],
                "clips": clips,
                "anchors": anchors,
            }
        )
    return sources


def base_project(name, manifest, cuts, gap_policy, uncertain=False, damaged=False):
    return {
        "name": name,
        "output": {
            "width": 640,
            "height": 360,
            "rate": OUT_RATE,
            "sample_rate": 48000,
            "duration": DURATION,
        },
        "sources": make_sources(manifest, uncertain=uncertain, damaged=damaged),
        "cuts": cuts,
        "gap_policy": gap_policy,
        # Single audio master independent of picture cuts. Host audio starts
        # at source time 1.0, which maps to program 0.0.
        "audio": {"source_id": "host", "program_start": 0.0, "external_path": None},
    }


def numbered(cuts):
    return [
        {"id": f"cut-{i + 1}", "start": round(start, 4), "source_id": sid}
        for i, (start, sid) in enumerate(cuts)
    ]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    args = ap.parse_args()
    data_dir = os.path.abspath(args.data)
    manifest = load_manifest(data_dir)
    projects_dir = os.path.join(data_dir, "projects")
    os.makedirs(projects_dir, exist_ok=True)

    main_cuts = numbered(
        [
            (0.0, "host"),
            (6.0, "guest"),
            (10.5, "host"),   # host hole program 11-14 lies inside this cut
            (15.0, "wide"),
            (20.0, "host"),
            (24.0, "wide"),   # wide dies at program 28.0 -> tail shortage
        ]
    )

    rapid = [
        (0.0, "host"),
        (3.0, "wide"),
        (3.5, "host"),
        (4.0, "guest"),
        (4.5, "wide"),
        (5.0, "host"),
        # Frame-accurate rapid fire at 25 fps: alternating every 2-3 frames.
        (6.0, "wide"),
        (6.08, "host"),
        (6.16, "wide"),
        (6.24, "host"),
        (6.32, "wide"),
        (6.40, "host"),
        (6.48, "wide"),
        (6.56, "host"),
        (7.0, "guest"),
        (9.0, "host"),
        (12.0, "wide"),
        (16.0, "host"),
        (20.0, "wide"),
        (24.0, "host"),
    ]

    projects = {
        "news-main": base_project("news-main", manifest, main_cuts, {"mode": "black"}),
        "news-fallback": base_project(
            "news-fallback",
            manifest,
            main_cuts,
            {"mode": "fallback", "source_id": "wide"},
        ),
        "news-rapid": base_project(
            "news-rapid",
            manifest,
            numbered(rapid),
            {"mode": "fallback", "source_id": "host"},
        ),
        "news-damaged": base_project(
            "news-damaged",
            manifest,
            main_cuts,
            {"mode": "fallback", "source_id": "wide"},
            damaged=True,
        ),
        "news-uncertain": base_project(
            "news-uncertain",
            manifest,
            main_cuts,
            {"mode": "black"},
            uncertain=True,
        ),
    }

    for name, project in projects.items():
        path = os.path.join(projects_dir, f"{name}.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(project, f, indent=2, ensure_ascii=False)
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
