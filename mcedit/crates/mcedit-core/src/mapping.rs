//! Source-to-program timeline mapping based exclusively on human anchor
//! points. File creation/modification timestamps are deliberately never used
//! for alignment.
//!
//! With one anchor the mapping is a pure offset (`program = source - offset`).
//! With several anchors a least-squares fit of slope and offset is computed;
//! if the fitted slope disagrees with the frame-rate ratio the residual spread
//! is surfaced as an uncertainty, and the UI shows a comparable range instead
//! of pretending the mapping is exact.

use crate::json::Json;

/// One human-identified correspondence: the same visible event located on the
/// program timeline and on the source timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub id: String,
    pub label: String,
    pub program_time: f64,
    pub source_time: f64,
    /// Human estimate of how far either side of the point the match could be
    /// off, in seconds. Zero means an exact, confident anchor.
    pub uncertainty: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mapping {
    /// source_time = slope * program_time + intercept
    pub slope: f64,
    pub intercept: f64,
    /// RMS residual of the anchor fit in seconds (0 for a single anchor).
    pub fit_residual: f64,
    /// Largest declared anchor uncertainty in seconds.
    pub anchor_uncertainty: f64,
    pub anchor_count: usize,
}

impl Mapping {
    pub fn from_anchors(anchors: &[Anchor]) -> Option<Mapping> {
        match anchors.len() {
            0 => None,
            1 => {
                let a = &anchors[0];
                Some(Mapping {
                    slope: 1.0,
                    intercept: a.source_time - a.program_time,
                    fit_residual: 0.0,
                    anchor_uncertainty: a.uncertainty.max(0.0),
                    anchor_count: 1,
                })
            }
            _ => {
                let n = anchors.len() as f64;
                let mean_p = anchors.iter().map(|a| a.program_time).sum::<f64>() / n;
                let mean_s = anchors.iter().map(|a| a.source_time).sum::<f64>() / n;
                let mut var_p = 0.0;
                let mut cov = 0.0;
                for a in anchors {
                    let dp = a.program_time - mean_p;
                    let ds = a.source_time - mean_s;
                    var_p += dp * dp;
                    cov += dp * ds;
                }
                let slope = if var_p > 1e-12 { cov / var_p } else { 1.0 };
                let intercept = mean_s - slope * mean_p;
                let residual = (anchors
                    .iter()
                    .map(|a| {
                        let predicted = slope * a.program_time + intercept;
                        let e = predicted - a.source_time;
                        e * e
                    })
                    .sum::<f64>()
                    / n)
                    .sqrt();
                let anchor_uncertainty = anchors.iter().map(|a| a.uncertainty).fold(0.0, f64::max);
                Some(Mapping {
                    slope,
                    intercept,
                    fit_residual: residual,
                    anchor_uncertainty,
                    anchor_count: anchors.len(),
                })
            }
        }
    }

    pub fn program_to_source(&self, program_time: f64) -> f64 {
        self.slope * program_time + self.intercept
    }

    pub fn source_to_program(&self, source_time: f64) -> f64 {
        (source_time - self.intercept) / self.slope
    }

    /// Total half-width of the comparable range around a mapped point, in
    /// seconds. Combines fit residual with the operator's own uncertainty.
    pub fn range_half_width(&self) -> f64 {
        self.fit_residual + self.anchor_uncertainty
    }

    /// Inclusive (low, high) program-time range consistent with the anchors
    /// for a requested program position.
    pub fn comparable_range(&self, program_time: f64) -> (f64, f64) {
        let w = self.range_half_width() * self.slope.abs();
        (program_time - w, program_time + w)
    }

    /// True when the mapping is too uncertain to claim frame-accurate cuts.
    pub fn is_uncertain(&self) -> bool {
        self.range_half_width() > 0.04
    }

    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("slope", Json::Num(self.slope));
        o.insert("intercept", Json::Num(self.intercept));
        o.insert("fit_residual", Json::Num(self.fit_residual));
        o.insert("anchor_uncertainty", Json::Num(self.anchor_uncertainty));
        o.insert("range_half_width", Json::Num(self.range_half_width()));
        o.insert("anchor_count", Json::Num(self.anchor_count as f64));
        o.insert("uncertain", Json::Bool(self.is_uncertain()));
        o
    }
}

impl Anchor {
    pub fn from_json(v: &Json) -> Result<Anchor, String> {
        Ok(Anchor {
            id: required_str(v, "id")?,
            label: v.get("label").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            program_time: required_num(v, "program_time")?,
            source_time: required_num(v, "source_time")?,
            uncertainty: v.get("uncertainty").and_then(|x| x.as_f64()).unwrap_or(0.0),
        })
    }

    pub fn to_json(&self) -> Json {
        let mut o = Json::obj();
        o.insert("id", Json::Str(self.id.clone()));
        o.insert("label", Json::Str(self.label.clone()));
        o.insert("program_time", Json::Num(self.program_time));
        o.insert("source_time", Json::Num(self.source_time));
        o.insert("uncertainty", Json::Num(self.uncertainty));
        o
    }
}

pub(crate) fn required_str(v: &Json, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing or invalid field '{}'", key))
}

pub(crate) fn required_num(v: &Json, key: &str) -> Result<f64, String> {
    v.get(key)
        .and_then(|x| x.as_f64())
        .ok_or_else(|| format!("missing or invalid field '{}'", key))
}
