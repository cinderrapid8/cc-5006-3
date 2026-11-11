//! Unit tests for the core timeline rules. Run with `cargo test`.

use mcedit_core::mapping::{Anchor, Mapping};
use mcedit_core::plan::{ExpectationKind, RenderPlan};
use mcedit_core::project::{AudioMaster, Cut, GapPolicy, OutputConfig, Project};
use mcedit_core::source::{Clip, Source};
use mcedit_core::time::Rate;

fn anchor(id: &str, p: f64, s: f64, u: f64) -> Anchor {
    Anchor {
        id: id.to_string(),
        label: id.to_string(),
        program_time: p,
        source_time: s,
        uncertainty: u,
    }
}

fn clip(path: &str, start: f64, duration: f64) -> Clip {
    Clip {
        path: path.to_string(),
        start,
        duration,
        frame_count: (duration * 25.0) as u64,
        has_audio: true,
    }
}

fn source(id: &str, anchors: Vec<Anchor>, clips: Vec<Clip>, rate: Rate) -> Source {
    Source {
        id: id.to_string(),
        name: id.to_string(),
        role: id.to_string(),
        rate,
        clips,
        anchors,
    }
}

#[test]
fn single_anchor_is_pure_offset() {
    let m = Mapping::from_anchors(&[anchor("a", 10.0, 12.5, 0.0)]).unwrap();
    assert!((m.slope - 1.0).abs() < 1e-12);
    assert!((m.intercept - 2.5).abs() < 1e-12);
    assert!((m.program_to_source(10.0) - 12.5).abs() < 1e-12);
    assert!((m.source_to_program(12.5) - 10.0).abs() < 1e-12);
    assert!(!m.is_uncertain());
}

#[test]
fn no_anchor_means_no_mapping_never_file_time() {
    assert!(Mapping::from_anchors(&[]).is_none());
    let s = source("cam", vec![], vec![clip("c.mp4", 0.0, 5.0)], Rate::fps(25));
    assert!(!s.anchored());
    assert!(s.program_intervals().is_none());
    assert!(s.sample_at(1.0).is_none());
}

#[test]
fn uncertainty_surfaces_comparable_range() {
    let m = Mapping::from_anchors(&[anchor("a", 5.0, 6.0, 0.1)]).unwrap();
    assert!(m.is_uncertain());
    let (lo, hi) = m.comparable_range(5.0);
    assert!((lo - 4.9).abs() < 1e-9);
    assert!((hi - 5.1).abs() < 1e-9);
}

#[test]
fn multi_anchor_least_squares_detects_residual() {
    // Consistent anchors: source runs 2% slow relative to program clock.
    let anchors = vec![
        anchor("a", 0.0, 1.0, 0.0),
        anchor("b", 10.0, 11.02, 0.0),
        anchor("c", 20.0, 21.04, 0.0),
    ];
    let m = Mapping::from_anchors(&anchors).unwrap();
    assert!((m.slope - 1.002).abs() < 1e-9);
    assert!(m.fit_residual.abs() < 1e-9);

    // Inconsistent anchor: residual must become visible.
    let bad = vec![
        anchor("a", 0.0, 1.0, 0.0),
        anchor("b", 10.0, 11.02, 0.0),
        anchor("c", 20.0, 22.5, 0.0),
    ];
    let m2 = Mapping::from_anchors(&bad).unwrap();
    assert!(m2.fit_residual > 0.3);
    assert!(m2.is_uncertain());
}

fn demo_project(gap_policy: GapPolicy) -> Project {
    // host:   offset +1s, full coverage, a hole at program 12-14s
    let host = source(
        "host",
        vec![anchor("h", 0.0, 1.0, 0.0)],
        vec![clip("host-a.mp4", 1.0, 11.0), clip("host-b.mp4", 15.0, 16.0)],
        Rate::fps(25),
    );
    // guest:  offset -2s (started early), only covers source time -2..10
    let guest = source(
        "guest",
        vec![anchor("g", 0.0, -2.0, 0.0)],
        vec![clip("guest.mp4", -2.0, 12.0)],
        Rate::fps(25),
    );
    // wide:   NTSC rate, offset +0.5, tail shortage ending inside the program
    let wide = source(
        "wide",
        vec![anchor("w", 0.0, 0.5, 0.0)],
        vec![clip("wide.mp4", 0.5, 28.0)],
        Rate::ntsc_30(),
    );
    Project {
        name: "demo".to_string(),
        output: OutputConfig {
            width: 640,
            height: 360,
            rate: Rate::fps(25),
            sample_rate: 48000,
            duration: 30.0,
        },
        sources: vec![host, guest, wide],
        cuts: vec![
            Cut { id: "cut-1".to_string(), start: 0.0, source_id: "host".to_string() },
            Cut { id: "cut-2".to_string(), start: 8.0, source_id: "guest".to_string() },
            Cut { id: "cut-3".to_string(), start: 11.0, source_id: "host".to_string() },
            Cut { id: "cut-4".to_string(), start: 16.0, source_id: "wide".to_string() },
        ],
        gap_policy,
        audio: AudioMaster {
            source_id: "host".to_string(),
            program_start: 0.0,
            external_path: None,
        },
    }
}

#[test]
fn gaps_are_detected_inside_cuts() {
    let p = demo_project(GapPolicy::Black);
    let report = p.validate();
    assert!(report.ok());
    // host hole maps to program 11..14 (source 12..14 minus offset 1), cut-3
    // starts at 11 so the gap inside cut-3 is 11..14.
    let cut3_gap: Vec<_> = report.gaps.iter().filter(|g| g.cut_id == "cut-3").collect();
    assert_eq!(cut3_gap.len(), 1);
    assert!((cut3_gap[0].start - 11.0).abs() < 1e-9);
    assert!((cut3_gap[0].end - 14.0).abs() < 1e-9);
    // wide tail ends at source 28.5 -> program 28.0, leaving a real shortage.
    assert!(report.gaps.iter().any(|g| g.source_id == "wide"));
}

#[test]
fn black_policy_emits_black_segments() {
    let p = demo_project(GapPolicy::Black);
    let plan = RenderPlan::build(&p).unwrap();
    assert_eq!(plan.total_frames, 750);
    let black: Vec<_> = plan.segments.iter().filter(|s| s.black).collect();
    assert!(!black.is_empty());
    let covered: u64 = plan.segments.iter().map(|s| s.frames).sum();
    assert_eq!(covered, 750);
    // expectation at program 12s (frame 300) must be black.
    let frame300 = &plan.expectations[300];
    assert!(matches!(frame300.kind, ExpectationKind::Black));
}

#[test]
fn fallback_policy_repoints_gap_without_touching_audio() {
    let p = demo_project(GapPolicy::Fallback { source_id: "wide".to_string() });
    let plan = RenderPlan::build(&p).unwrap();
    let frame300 = &plan.expectations[300];
    match &frame300.kind {
        ExpectationKind::Frame { source_id, fallback, .. } => {
            assert_eq!(source_id, "wide");
            assert!(*fallback);
        }
        other => panic!("expected fallback frame, got {:?}", other),
    }
    // Audio stays a single master: first audio segment is host, never guest.
    assert_eq!(plan.audio_segments[0].clip_path.as_deref(), Some("host-a.mp4"));
    let audio_sources: Vec<&str> = plan
        .audio_segments
        .iter()
        .filter_map(|s| s.clip_path.as_deref())
        .collect();
    assert!(audio_sources.iter().all(|p| p.starts_with("host")));
}

#[test]
fn rapid_cuts_produce_one_frame_segments() {
    let mut p = demo_project(GapPolicy::Fallback { source_id: "wide".to_string() });
    // Thirteen cuts inside three output seconds, all on frame boundaries.
    p.cuts = (0..13)
        .map(|i| Cut {
            id: format!("cut-{}", i + 1),
            start: i as f64 / 25.0,
            source_id: if i % 2 == 0 { "host".to_string() } else { "wide".to_string() },
        })
        .collect();
    let plan = RenderPlan::build(&p).unwrap();
    let tiny: Vec<_> = plan.segments.iter().filter(|s| s.frames == 1).collect();
    assert!(tiny.len() >= 10, "expected many single-frame segments, got {}", tiny.len());
    let total: u64 = plan.segments.iter().map(|s| s.frames).sum();
    assert_eq!(total, plan.total_frames);
}

#[test]
fn damaged_source_missing_entirely_forces_black_or_fallback() {
    let mut p = demo_project(GapPolicy::Black);
    // Simulate a dead camera: guest clips removed after ingest.
    for s in p.sources.iter_mut() {
        if s.id == "guest" {
            s.clips.clear();
        }
    }
    let report = p.validate();
    assert!(report.gaps.iter().any(|g| g.source_id == "guest"));
    let plan = RenderPlan::build(&p).unwrap();
    // cut-2 (8..11) must now be entirely black.
    let all_black = plan.expectations[200..275]
        .iter()
        .all(|f| matches!(f.kind, ExpectationKind::Black));
    assert!(all_black);
}

#[test]
fn revision_is_stable_and_content_sensitive() {
    let p1 = demo_project(GapPolicy::Black);
    let p2 = demo_project(GapPolicy::Black);
    assert_eq!(p1.revision(), p2.revision());
    let mut p3 = demo_project(GapPolicy::Black);
    p3.cuts[0].start = 0.5;
    assert_ne!(p1.revision(), p3.revision());
}

#[test]
fn audio_plan_inserts_silence_for_master_gaps() {
    let p = demo_project(GapPolicy::Black);
    let plan = RenderPlan::build(&p).unwrap();
    // host has no data for program 11..14 (its hole) -> silence segment there.
    let silence = plan
        .audio_segments
        .iter()
        .find(|s| s.silence && s.start <= 11.0 && s.start + s.duration >= 14.0);
    assert!(silence.is_some(), "expected silence covering 11..14");
    let total: f64 = plan.audio_segments.iter().map(|s| s.duration).sum();
    assert!((total - 30.0).abs() < 1e-6);
}
