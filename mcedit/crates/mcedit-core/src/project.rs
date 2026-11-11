//! Editing project: sources, cut list, gap policy and the independent audio
//! master. A project is the fixed, revisioned artifact that gets rendered.

use crate::hash::sha256_short;
use crate::json::Json;
use crate::mapping::required_str;
use crate::source::Source;
use crate::time::Rate;

#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    pub width: u32,
    pub height: u32,
    pub rate: Rate,
    pub sample_rate: u32,
    pub duration: f64,
}

/// One switcher cut. The selected source is presented from `start` until the
/// next cut starts; the final cut runs to `output.duration`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cut {
    pub id: String,
    pub start: f64,
    pub source_id: String,
}

/// How missing picture inside a cut is resolved. The operator must choose
/// explicitly; nothing is guessed.
#[derive(Debug, Clone, PartialEq)]
pub enum GapPolicy {
    /// Emit black frames for the missing interval.
    Black,
    /// Re-point the missing interval at another source that still has data.
    Fallback { source_id: String },
}

/// Audio is a single master, chosen independently of picture cuts.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioMaster {
    /// Source whose continuous audio track is used, or an external file id.
    pub source_id: String,
    /// Program time corresponding to the start of the audio media.
    pub program_start: f64,
    pub external_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub name: String,
    pub output: OutputConfig,
    pub sources: Vec<Source>,
    pub cuts: Vec<Cut>,
    pub gap_policy: GapPolicy,
    pub audio: AudioMaster,
}

/// A missing-data interval detected inside a cut.
#[derive(Debug, Clone, PartialEq)]
pub struct Gap {
    pub cut_id: String,
    pub source_id: String,
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub gaps: Vec<Gap>,
}

impl ValidationReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl Project {
    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }

    pub fn validate(&self) -> ValidationReport {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut gaps = Vec::new();

        if self.output.duration <= 0.0 {
            errors.push("output.duration must be positive".to_string());
        }
        if self.cuts.is_empty() {
            errors.push("timeline has no cuts".to_string());
        }

        for source in &self.sources {
            if !source.anchored() {
                warnings.push(format!(
                    "source '{}' has no human anchor and cannot be aligned",
                    source.id
                ));
            } else if let Some(m) = source.mapping() {
                if m.is_uncertain() {
                    warnings.push(format!(
                        "source '{}' anchor mapping is uncertain within +/- {:.3}s; a comparable range is shown",
                        source.id,
                        m.range_half_width()
                    ));
                }
            }
        }

        let mut last_start = f64::NEG_INFINITY;
        for (i, cut) in self.cuts.iter().enumerate() {
            if cut.start < 0.0 || cut.start >= self.output.duration {
                errors.push(format!("cut {} starts outside program duration", cut.id));
            }
            if i > 0 && cut.start <= last_start + 1e-9 {
                errors.push(format!("cut {} is not strictly after the previous cut", cut.id));
            }
            last_start = cut.start;
            let source = match self.source(&cut.source_id) {
                Some(s) => s,
                None => {
                    errors.push(format!("cut {} references unknown source '{}'", cut.id, cut.source_id));
                    continue;
                }
            };
            if !source.anchored() {
                errors.push(format!(
                    "cut {} uses unanchored source '{}'",
                    cut.id, cut.source_id
                ));
            }
        }

        if let Err(msg) = self.check_gap_policy() {
            errors.push(msg);
        }
        if self.source(&self.audio.source_id).is_none() && self.audio.external_path.is_none() {
            errors.push(format!(
                "audio master references unknown source '{}' and no external file",
                self.audio.source_id
            ));
        }

        for gap in self.scan_gaps() {
            gaps.push(gap);
        }

        ValidationReport {
            errors,
            warnings,
            gaps,
        }
    }

    fn check_gap_policy(&self) -> Result<(), String> {
        if let GapPolicy::Fallback { source_id } = &self.gap_policy {
            let fallback = self
                .source(source_id)
                .ok_or_else(|| format!("gap fallback references unknown source '{}'", source_id))?;
            if !fallback.anchored() {
                return Err(format!("gap fallback source '{}' has no anchor", source_id));
            }
        }
        Ok(())
    }

    /// Walk every cut and report intervals where its selected source has no
    /// recorded media.
    pub fn scan_gaps(&self) -> Vec<Gap> {
        let mut out = Vec::new();
        for (i, cut) in self.cuts.iter().enumerate() {
            let end = self
                .cuts
                .get(i + 1)
                .map(|c| c.start)
                .unwrap_or(self.output.duration);
            let source = match self.source(&cut.source_id) {
                Some(s) => s,
                None => continue,
            };
            for (g_start, g_end) in source.gaps_in(cut.start, end) {
                if g_end - g_start > 1e-6 {
                    out.push(Gap {
                        cut_id: cut.id.clone(),
                        source_id: cut.source_id.clone(),
                        start: g_start,
                        end: g_end,
                    });
                }
            }
        }
        out
    }

    /// Source selected at a given program time after applying the gap policy.
    /// Returns None only when a fallback source itself lacks data.
    pub fn visual_source_at(&self, program_time: f64) -> VisualChoice {
        let cut = match self.cut_at(program_time) {
            Some(c) => c,
            None => return VisualChoice::Black,
        };
        let source = match self.source(&cut.source_id) {
            Some(s) => s,
            None => return VisualChoice::Black,
        };
        if source.sample_at(program_time).is_some() {
            return VisualChoice::Source(source.id.clone());
        }
        match &self.gap_policy {
            GapPolicy::Black => VisualChoice::Black,
            GapPolicy::Fallback { source_id } => {
                if let Some(fb) = self.source(source_id) {
                    if fb.sample_at(program_time).is_some() {
                        return VisualChoice::Fallback(fb.id.clone(), source.id.clone());
                    }
                }
                VisualChoice::Black
            }
        }
    }

    pub fn cut_at(&self, program_time: f64) -> Option<&Cut> {
        let mut found: Option<&Cut> = None;
        for cut in &self.cuts {
            if cut.start <= program_time + 1e-9 {
                found = Some(cut);
            } else {
                break;
            }
        }
        found
    }

    /// Stable content revision: canonical JSON of the editable content,
    /// independent of file system timestamps.
    pub fn revision(&self) -> String {
        sha256_short(self.content_json().to_canonical_string().as_bytes())
    }

    pub fn content_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("name", Json::Str(self.name.clone()));
        o.insert("output", self.output.to_json());
        o.insert(
            "sources",
            Json::Arr(self.sources.iter().map(|s| s.to_json()).collect()),
        );
        o.insert("cuts", Json::Arr(self.cuts.iter().map(Cut::to_json).collect()));
        o.insert("gap_policy", self.gap_policy.to_json());
        o.insert("audio", self.audio.to_json());
        o
    }

    pub fn from_json(v: &Json) -> Result<Project, String> {
        let output = v
            .get("output")
            .map(OutputConfig::from_json)
            .transpose()?
            .ok_or("missing 'output'")?;
        let sources = v
            .get("sources")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().map(Source::from_json).collect::<Result<Vec<_>, _>>())
            .transpose()?
            .unwrap_or_default();
        let cuts = v
            .get("cuts")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().map(Cut::from_json).collect::<Result<Vec<_>, _>>())
            .transpose()?
            .unwrap_or_default();
        let gap_policy = match v.get("gap_policy") {
            Some(g) => GapPolicy::from_json(g)?,
            None => GapPolicy::Black,
        };
        let audio = match v.get("audio") {
            Some(a) => AudioMaster::from_json(a)?,
            None => {
                return Err("missing 'audio' master definition".to_string());
            }
        };
        Ok(Project {
            name: v.get("name").and_then(|x| x.as_str()).unwrap_or("untitled").to_string(),
            output,
            sources,
            cuts,
            gap_policy,
            audio,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VisualChoice {
    Source(String),
    Fallback(String, String),
    Black,
}

impl OutputConfig {
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("width", Json::Num(self.width as f64));
        o.insert("height", Json::Num(self.height as f64));
        let mut rate = Json::obj();
        rate.insert("num", Json::Num(self.rate.num as f64));
        rate.insert("den", Json::Num(self.rate.den as f64));
        o.insert("rate", rate);
        o.insert("sample_rate", Json::Num(self.sample_rate as f64));
        o.insert("duration", Json::Num(self.duration));
        o
    }

    pub fn from_json(v: &Json) -> Result<Self, String> {
        let rate = match v.get("rate") {
            Some(Json::Obj(fields)) => Rate {
                num: fields
                    .iter()
                    .find(|(k, _)| k == "num")
                    .and_then(|(_, x)| x.as_u64())
                    .ok_or("rate.num missing")? as u32,
                den: fields
                    .iter()
                    .find(|(k, _)| k == "den")
                    .and_then(|(_, x)| x.as_u64())
                    .ok_or("rate.den missing")? as u32,
            },
            Some(Json::Num(n)) => Rate::fps(*n as u32),
            _ => return Err("output.rate missing".to_string()),
        };
        Ok(OutputConfig {
            width: required_num(v, "width")? as u32,
            height: required_num(v, "height")? as u32,
            rate,
            sample_rate: v
                .get("sample_rate")
                .and_then(|x| x.as_u64())
                .unwrap_or(48000) as u32,
            duration: required_num(v, "duration")?,
        })
    }
}

fn required_num(v: &Json, key: &str) -> Result<f64, String> {
    v.get(key)
        .and_then(|x| x.as_f64())
        .ok_or_else(|| format!("missing field '{}'", key))
}

impl Cut {
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("id", Json::Str(self.id.clone()));
        o.insert("start", Json::Num(self.start));
        o.insert("source_id", Json::Str(self.source_id.clone()));
        o
    }

    pub fn from_json(v: &Json) -> Result<Self, String> {
        Ok(Cut {
            id: required_str(v, "id")?,
            start: v.get("start").and_then(|x| x.as_f64()).ok_or("cut.start missing")?,
            source_id: required_str(v, "source_id")?,
        })
    }
}

impl GapPolicy {
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        match self {
            GapPolicy::Black => {
                o.insert("mode", Json::Str("black".to_string()));
            }
            GapPolicy::Fallback { source_id } => {
                o.insert("mode", Json::Str("fallback".to_string()));
                o.insert("source_id", Json::Str(source_id.clone()));
            }
        }
        o
    }

    pub fn from_json(v: &Json) -> Result<Self, String> {
        match v.get("mode").and_then(|x| x.as_str()) {
            Some("black") | None => Ok(GapPolicy::Black),
            Some("fallback") => Ok(GapPolicy::Fallback {
                source_id: required_str(v, "source_id")?,
            }),
            Some(other) => Err(format!("unknown gap policy '{}'", other)),
        }
    }
}

impl AudioMaster {
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("source_id", Json::Str(self.source_id.clone()));
        o.insert("program_start", Json::Num(self.program_start));
        match &self.external_path {
            Some(p) => o.insert("external_path", Json::Str(p.clone())),
            None => o.insert("external_path", Json::Null),
        }
        o
    }

    pub fn from_json(v: &Json) -> Result<Self, String> {
        Ok(AudioMaster {
            source_id: required_str(v, "source_id")?,
            program_start: v.get("program_start").and_then(|x| x.as_f64()).unwrap_or(0.0),
            external_path: v.get("external_path").and_then(|x| x.as_str()).map(String::from),
        })
    }
}

impl Gap {
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("cut_id", Json::Str(self.cut_id.clone()));
        o.insert("source_id", Json::Str(self.source_id.clone()));
        o.insert("start", Json::Num(self.start));
        o.insert("end", Json::Num(self.end));
        o.insert("duration", Json::Num(self.end - self.start));
        o
    }
}
