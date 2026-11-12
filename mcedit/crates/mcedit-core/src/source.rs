//! Camera sources: frame rate, human anchors, recorded clips and availability.
//!
//! A source may be split into several clip files (a camera that stopped and
//! restarted, or a damaged/corrupt section removed at ingest). Each clip keeps
//! its own start timecode on the source timeline. The source timeline itself
//! is mapped onto the program timeline only through [`Anchor`]s.

use crate::json::Json;
use crate::mapping::{required_num, required_str, Anchor, Mapping};
use crate::time::Rate;

/// One physical media file belonging to a source. Times are in seconds on the
/// source timeline, not in program time and not file-system time.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub path: String,
    pub start: f64,
    pub duration: f64,
    pub frame_count: u64,
    pub has_audio: bool,
}

impl Clip {
    pub fn end(&self) -> f64 {
        self.start + self.duration
    }

    /// Time-based half-open membership with a half-frame epsilon. Kept for the
    /// coarse availability-interval math; frame-accurate lookup must go through
    /// [`Clip::frame_at`] instead because the last presentation frame extends to
    /// the clip end even though its timestamp sits one frame duration earlier.
    pub fn contains(&self, source_time: f64, eps: f64) -> bool {
        source_time >= self.start - eps && source_time < self.end() - eps
    }

    /// Number of frames the clip actually presents. The manifest carries an
    /// explicit count; fall back to the duration-derived grid when absent.
    pub fn effective_frame_count(&self, rate: Rate) -> u64 {
        if self.frame_count > 0 {
            self.frame_count
        } else {
            crate::time::nearest_frame(self.duration, rate).max(0) as u64
        }
    }

    /// Locate the clip-relative presentation frame covering `source_time`.
    ///
    /// Frame `k` is presented over the half-open interval
    /// `[start + k*frame_dur, start + (k+1)*frame_dur)`, so the clip's final
    /// frame owns the slice up to `end()`. Membership is decided on this frame
    /// grid — never on decode order, which B-frame streams reorder.
    /// Returns the clip-relative frame index, or None when no frame covers it.
    pub fn frame_at(&self, source_time: f64, rate: Rate) -> Option<i64> {
        let frame_dur = rate.frame_duration();
        let count = self.effective_frame_count(rate) as i64;
        if count <= 0 {
            return None;
        }
        let rel = source_time - self.start;
        // Floor to the frame grid; a time inside the last frame's presentation
        // slice (>= its PTS, < clip end) must still map to that last frame.
        let idx = (rel / frame_dur).floor() as i64;
        let idx = idx.max(0);
        if idx >= count {
            return None;
        }
        // The first presentation frame starts exactly at clip.start; an earlier
        // time belongs to a preceding ingest gap (or another clip), never here.
        if source_time < self.start {
            return None;
        }
        Some(idx)
    }

    pub fn from_json(v: &Json) -> Result<Clip, String> {
        Ok(Clip {
            path: required_str(v, "path")?,
            start: required_num(v, "start")?,
            duration: required_num(v, "duration")?,
            frame_count: v.get("frame_count").and_then(|x| x.as_u64()).unwrap_or(0),
            has_audio: v.get("has_audio").and_then(|x| x.as_bool()).unwrap_or(true),
        })
    }

    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("path", Json::Str(self.path.clone()));
        o.insert("start", Json::Num(self.start));
        o.insert("duration", Json::Num(self.duration));
        o.insert("frame_count", Json::Num(self.frame_count as f64));
        o.insert("has_audio", Json::Bool(self.has_audio));
        o
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub role: String,
    pub rate: Rate,
    pub clips: Vec<Clip>,
    pub anchors: Vec<Anchor>,
}

/// Result of asking a source for the media covering a program position.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSample {
    pub source_time: f64,
    pub clip_path: String,
    pub clip_offset: f64,
    pub frame_index: i64,
}

impl Source {
    pub fn mapping(&self) -> Option<Mapping> {
        Mapping::from_anchors(&self.anchors)
    }

    pub fn anchored(&self) -> bool {
        !self.anchors.is_empty()
    }

    /// Merged availability intervals in source time, sorted and coalesced.
    pub fn source_intervals(&self) -> Vec<(f64, f64)> {
        let mut spans: Vec<(f64, f64)> = self.clips.iter().map(|c| (c.start, c.end())).collect();
        spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for span in spans {
            if let Some(last) = merged.last_mut() {
                if span.0 <= last.1 + 1e-6 {
                    last.1 = last.1.max(span.1);
                    continue;
                }
            }
            merged.push(span);
        }
        merged
    }

    /// Availability mapped onto program time. None until the source has at
    /// least one human anchor.
    pub fn program_intervals(&self) -> Option<Vec<(f64, f64)>> {
        let mapping = self.mapping()?;
        let mut spans: Vec<(f64, f64)> = self
            .source_intervals()
            .iter()
            .map(|(s, e)| {
                let p0 = mapping.source_to_program(*s);
                let p1 = mapping.source_to_program(*e);
                (p0.min(p1), p0.max(p1))
            })
            .collect();
        spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for span in spans {
            if let Some(last) = merged.last_mut() {
                if span.0 <= last.1 + 1e-6 {
                    last.1 = last.1.max(span.1);
                    continue;
                }
            }
            merged.push(span);
        }
        Some(merged)
    }

    /// Locate the clip and presentation frame for a program position. Returns
    /// None when the source is unanchored or has no recorded frame there.
    ///
    /// Membership is decided on each clip's presentation frame grid (see
    /// [`Clip::frame_at`]), so the answer is always a *presentation* frame keyed
    /// by its engineering timestamp. B-frame decode order never enters the
    /// decision. Clips are checked by ascending source start, so at an ingest
    /// boundary the earlier clip deterministically wins; this is a local
    /// boundary decision and never a global shift of frame numbers.
    pub fn sample_at(&self, program_time: f64) -> Option<SourceSample> {
        let mapping = self.mapping()?;
        let source_time = mapping.program_to_source(program_time);
        let mut order: Vec<usize> = (0..self.clips.len()).collect();
        order.sort_by(|&a, &b| {
            self.clips[a]
                .start
                .partial_cmp(&self.clips[b].start)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        for clip_idx in order {
            let clip = &self.clips[clip_idx];
            if let Some(frame_index) = clip.frame_at(source_time, self.rate) {
                let clip_offset = (source_time - clip.start).max(0.0);
                return Some(SourceSample {
                    source_time,
                    clip_path: clip.path.clone(),
                    clip_offset,
                    frame_index,
                });
            }
        }
        None
    }

    /// Is recorded data available across a half-open program interval?
    pub fn covers(&self, start: f64, end: f64) -> bool {
        let step = 0.01;
        let mut t = start;
        while t < end - 1e-9 {
            if self.sample_at(t).is_none() {
                return false;
            }
            t += step;
        }
        true
    }

    /// Program-time sub-intervals of [start, end) with no recorded data.
    pub fn gaps_in(&self, start: f64, end: f64) -> Vec<(f64, f64)> {
        let available = match self.program_intervals() {
            Some(v) => v,
            None => return vec![(start, end)],
        };
        let mut gaps = Vec::new();
        let mut cursor = start;
        for (a, b) in available {
            if b <= cursor {
                continue;
            }
            if a >= end {
                break;
            }
            if a > cursor {
                gaps.push((cursor, a.min(end)));
            }
            cursor = cursor.max(b);
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            gaps.push((cursor, end));
        }
        gaps
    }

    pub fn from_json(v: &Json) -> Result<Source, String> {
        let rate = match v.get("rate") {
            Some(Json::Obj(fields)) => {
                let num = fields
                    .iter()
                    .find(|(k, _)| k == "num")
                    .and_then(|(_, x)| x.as_u64())
                    .ok_or("rate.num missing")? as u32;
                let den = fields
                    .iter()
                    .find(|(k, _)| k == "den")
                    .and_then(|(_, x)| x.as_u64())
                    .ok_or("rate.den missing")? as u32;
                Rate { num, den }
            }
            Some(Json::Num(n)) => Rate::fps(*n as u32),
            _ => return Err("missing field 'rate'".to_string()),
        };
        let clips = v
            .get("clips")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().map(Clip::from_json).collect::<Result<Vec<_>, _>>())
            .transpose()?
            .unwrap_or_default();
        let anchors = v
            .get("anchors")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().map(Anchor::from_json).collect::<Result<Vec<_>, _>>())
            .transpose()?
            .unwrap_or_default();
        Ok(Source {
            id: required_str(v, "id")?,
            name: v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            role: v.get("role").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            rate,
            clips,
            anchors,
        })
    }

    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("id", Json::Str(self.id.clone()));
        o.insert("name", Json::Str(self.name.clone()));
        o.insert("role", Json::Str(self.role.clone()));
        let mut rate = Json::obj();
        rate.insert("num", Json::Num(self.rate.num as f64));
        rate.insert("den", Json::Num(self.rate.den as f64));
        o.insert("rate", rate);
        o.insert(
            "clips",
            Json::Arr(self.clips.iter().map(Clip::to_json).collect()),
        );
        o.insert(
            "anchors",
            Json::Arr(self.anchors.iter().map(Anchor::to_json).collect()),
        );
        o
    }
}
