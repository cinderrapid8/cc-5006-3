//! mcedit-core: domain model, anchor mapping, frame-accurate render plan and
//! deterministic ffmpeg rendering for the multi-camera interview editor.

pub mod hash;
pub mod json;
pub mod mapping;
pub mod plan;
pub mod project;
pub mod render;
pub mod source;
pub mod time;

pub use json::Json;
pub use mapping::{Anchor, Mapping};
pub use plan::{AudioSegment, ExpectationKind, FrameExpectation, RenderPlan, RenderSegment};
pub use project::{AudioMaster, Cut, Gap, GapPolicy, OutputConfig, Project, ValidationReport, VisualChoice};
pub use render::{decode_hashes, render, verify_reproducible, DecodeHashes, RenderResult};
pub use source::{Clip, Source, SourceSample};
pub use time::{format_program_time, format_timecode, nearest_frame, Rate};

use std::path::Path;

/// Load a project file and parse it. File system timestamps are never read.
pub fn load_project(path: &Path) -> Result<(Project, String), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let value = Json::from_str(&text)?;
    let project = Project::from_json(&value)?;
    Ok((project, text))
}
