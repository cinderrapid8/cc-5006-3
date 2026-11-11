//! Deterministic frame-by-frame render plan.
//!
//! The plan is the single source of truth shared by the ffmpeg renderer and
//! the verification harness: for every output frame it names the source clip,
//! the clip-relative frame and the presentation source time. Black intervals
//! and fallback substitutions are decided here, never inside ffmpeg.

use crate::project::{Project, VisualChoice};
use crate::json::Json;
use crate::time::Rate;

/// A continuous run of output frames served by one clip (or by black).
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSegment {
    pub index: usize,
    pub start: f64,
    pub duration: f64,
    pub frames: u64,
    pub source_id: Option<String>,
    pub clip_path: Option<String>,
    /// Time inside the clip of the first presented frame, seconds.
    pub clip_offset: f64,
    /// Clip-relative frame index of the first presented frame.
    pub first_frame: i64,
    pub black: bool,
    pub fallback: bool,
}

/// A continuous run of program audio served by one source clip, or silence.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSegment {
    pub index: usize,
    pub start: f64,
    pub duration: f64,
    pub clip_path: Option<String>,
    pub clip_offset: f64,
    pub silence: bool,
}

/// Per-frame expectation used by the verifier.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameExpectation {
    pub output_frame: u64,
    pub program_time: f64,
    pub kind: ExpectationKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExpectationKind {
    Black,
    Frame {
        source_id: String,
        source_time: f64,
        clip_offset: f64,
        source_frame: i64,
        fallback: bool,
    },
}

#[derive(Debug, Clone)]
pub struct RenderPlan {
    pub output_rate: Rate,
    pub duration: f64,
    pub total_frames: u64,
    pub segments: Vec<RenderSegment>,
    pub audio_segments: Vec<AudioSegment>,
    pub expectations: Vec<FrameExpectation>,
}

struct PendingSegment {
    start: f64,
    frames: u64,
    source_id: Option<String>,
    clip_path: Option<String>,
    clip_offset: f64,
    first_frame: i64,
    black: bool,
    fallback: bool,
}

impl RenderPlan {
    pub fn build(project: &Project) -> Result<RenderPlan, String> {
        let rate = project.output.rate;
        let frame_dur = rate.frame_duration();
        let total_frames = (project.output.duration * rate.num as f64 / rate.den as f64).round() as u64;

        let mut pending: Option<PendingSegment> = None;
        let mut segments = Vec::new();
        let mut expectations = Vec::new();

        let flush = |pending: &mut Option<PendingSegment>,
                     segments: &mut Vec<RenderSegment>,
                     end_time: f64| {
            if let Some(p) = pending.take() {
                segments.push(RenderSegment {
                    index: segments.len(),
                    start: p.start,
                    duration: (end_time - p.start).max(0.0),
                    frames: p.frames,
                    source_id: p.source_id,
                    clip_path: p.clip_path,
                    clip_offset: p.clip_offset,
                    first_frame: p.first_frame,
                    black: p.black,
                    fallback: p.fallback,
                });
            }
        };

        for output_frame in 0..total_frames {
            let program_time = output_frame as f64 * frame_dur;
            let choice = project.visual_source_at(program_time);

            let (black, source_id, clip_path, clip_offset, first_frame, source_time, fallback) = match &choice {
                VisualChoice::Black => (true, None, None, 0.0, 0, 0.0, false),
                VisualChoice::Source(id) | VisualChoice::Fallback(id, _) => {
                    let source = project.source(id).expect("validated source id");
                    let sample = source
                        .sample_at(program_time)
                        .expect("visual_source_at only returns sources with data");
                    let is_fb = matches!(choice, VisualChoice::Fallback(..));
                    (
                        false,
                        Some(id.to_string()),
                        Some(sample.clip_path.clone()),
                        sample.clip_offset,
                        sample.frame_index,
                        sample.source_time,
                        is_fb,
                    )
                }
            };

            expectations.push(FrameExpectation {
                output_frame,
                program_time,
                kind: if black {
                    ExpectationKind::Black
                } else {
                    ExpectationKind::Frame {
                        source_id: source_id.clone().unwrap(),
                        source_time,
                        clip_offset,
                        source_frame: first_frame,
                        fallback,
                    }
                },
            });

            let same_clip = match &pending {
                Some(p) => {
                    p.black == black
                        && p.clip_path == clip_path
                        && p.fallback == fallback
                        && if !black {
                            // contiguous on the program timeline (frame-accurate
                            // source indices are handled at render trim time)
                            p.start + (p.frames as f64) * frame_dur == program_time
                        } else {
                            true
                        }
                }
                None => false,
            };

            if same_clip {
                pending.as_mut().unwrap().frames += 1;
            } else {
                let end = program_time;
                flush(&mut pending, &mut segments, end);
                pending = Some(PendingSegment {
                    start: program_time,
                    frames: 1,
                    source_id,
                    clip_path,
                    clip_offset,
                    first_frame,
                    black,
                    fallback,
                });
            }
        }
        flush(&mut pending, &mut segments, project.output.duration);

        let audio_segments = build_audio_plan(project, total_frames, frame_dur);

        Ok(RenderPlan {
            output_rate: rate,
            duration: project.output.duration,
            total_frames,
            segments,
            audio_segments,
            expectations,
        })
    }

    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        let mut rate = Json::obj();
        rate.insert("num", Json::Num(self.output_rate.num as f64));
        rate.insert("den", Json::Num(self.output_rate.den as f64));
        o.insert("output_rate", rate);
        o.insert("duration", Json::Num(self.duration));
        o.insert("total_frames", Json::Num(self.total_frames as f64));
        o.insert(
            "segments",
            Json::Arr(self.segments.iter().map(RenderSegment::to_json).collect()),
        );
        o.insert(
            "audio_segments",
            Json::Arr(self.audio_segments.iter().map(AudioSegment::to_json).collect()),
        );
        o.insert(
            "frames",
            Json::Arr(self.expectations.iter().map(FrameExpectation::to_json).collect()),
        );
        o
    }
}

fn build_audio_plan(project: &Project, total_frames: u64, frame_dur: f64) -> Vec<AudioSegment> {
    let audio = &project.audio;
    let source = match project.source(&audio.source_id) {
        Some(s) => s,
        None => {
            return vec![AudioSegment {
                index: 0,
                start: 0.0,
                duration: total_frames as f64 * frame_dur,
                clip_path: audio.external_path.clone(),
                clip_offset: 0.0,
                silence: audio.external_path.is_none(),
            }];
        }
    };
    let mapping = match source.mapping() {
        Some(m) => m,
        None => {
            return vec![AudioSegment {
                index: 0,
                start: 0.0,
                duration: total_frames as f64 * frame_dur,
                clip_path: None,
                clip_offset: 0.0,
                silence: true,
            }];
        }
    };

    let program_end = total_frames as f64 * frame_dur;
    let mut segments = Vec::new();
    let mut cursor = 0.0_f64;
    for clip in &source.clips {
        let clip_p_start = mapping.source_to_program(clip.start);
        let clip_p_end = mapping.source_to_program(clip.end());
        let overlap_start = cursor.max(clip_p_start.max(audio.program_start));
        let overlap_end = clip_p_end.min(program_end);
        if overlap_end > overlap_start + 1e-9 {
            if overlap_start > cursor + 1e-9 {
                segments.push(AudioSegment {
                    index: segments.len(),
                    start: cursor,
                    duration: overlap_start - cursor,
                    clip_path: None,
                    clip_offset: 0.0,
                    silence: true,
                });
            }
            let clip_offset = mapping.program_to_source(overlap_start) - clip.start;
            segments.push(AudioSegment {
                index: segments.len(),
                start: overlap_start,
                duration: overlap_end - overlap_start,
                clip_path: Some(clip.path.clone()),
                clip_offset: clip_offset.max(0.0),
                silence: false,
            });
            cursor = overlap_end;
        }
    }
    if cursor < program_end - 1e-9 {
        segments.push(AudioSegment {
            index: segments.len(),
            start: cursor,
            duration: program_end - cursor,
            clip_path: None,
            clip_offset: 0.0,
            silence: true,
        });
    }
    if segments.is_empty() {
        segments.push(AudioSegment {
            index: 0,
            start: 0.0,
            duration: program_end,
            clip_path: None,
            clip_offset: 0.0,
            silence: true,
        });
    }
    segments
}

impl RenderSegment {
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("index", Json::Num(self.index as f64));
        o.insert("start", Json::Num(self.start));
        o.insert("duration", Json::Num(self.duration));
        o.insert("frames", Json::Num(self.frames as f64));
        match &self.source_id {
            Some(id) => o.insert("source_id", Json::Str(id.clone())),
            None => o.insert("source_id", Json::Null),
        }
        match &self.clip_path {
            Some(p) => o.insert("clip_path", Json::Str(p.clone())),
            None => o.insert("clip_path", Json::Null),
        }
        o.insert("clip_offset", Json::Num(self.clip_offset));
        o.insert("first_frame", Json::Num(self.first_frame as f64));
        o.insert("black", Json::Bool(self.black));
        o.insert("fallback", Json::Bool(self.fallback));
        o
    }
}

impl AudioSegment {
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("index", Json::Num(self.index as f64));
        o.insert("start", Json::Num(self.start));
        o.insert("duration", Json::Num(self.duration));
        match &self.clip_path {
            Some(p) => o.insert("clip_path", Json::Str(p.clone())),
            None => o.insert("clip_path", Json::Null),
        }
        o.insert("clip_offset", Json::Num(self.clip_offset));
        o.insert("silence", Json::Bool(self.silence));
        o
    }
}

impl FrameExpectation {
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("output_frame", Json::Num(self.output_frame as f64));
        o.insert("program_time", Json::Num(self.program_time));
        match &self.kind {
            ExpectationKind::Black => {
                o.insert("kind", Json::Str("black".to_string()));
            }
            ExpectationKind::Frame {
                source_id,
                source_time,
                clip_offset,
                source_frame,
                fallback,
            } => {
                o.insert("kind", Json::Str("frame".to_string()));
                o.insert("source_id", Json::Str(source_id.clone()));
                o.insert("source_time", Json::Num(*source_time));
                o.insert("clip_offset", Json::Num(*clip_offset));
                o.insert("source_frame", Json::Num(*source_frame as f64));
                o.insert("fallback", Json::Bool(*fallback));
            }
        }
        o
    }
}
