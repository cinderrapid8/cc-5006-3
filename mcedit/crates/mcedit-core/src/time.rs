//! Time and frame-rate primitives.
//!
//! All timing inside a project is expressed in seconds on the shared program
//! timeline (double precision). Frame rates are exact rationals so that, for
//! example, NTSC 30000/1001 is never rounded to 30.

use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub num: u32,
    pub den: u32,
}

impl Rate {
    pub fn fps(num: u32) -> Self {
        Rate { num, den: 1 }
    }

    pub fn ntsc_30() -> Self {
        Rate {
            num: 30000,
            den: 1001,
        }
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    pub fn frame_duration(self) -> f64 {
        self.den as f64 / self.num as f64
    }
}

/// Convert a time in seconds to the frame index whose presentation time is the
/// nearest one to it.
pub fn nearest_frame(time_sec: f64, rate: Rate) -> i64 {
    (time_sec * rate.num as f64 / rate.den as f64).round() as i64
}

/// Presentation time of a frame in seconds.
pub fn frame_time(frame: i64, rate: Rate) -> f64 {
    frame as f64 * rate.den as f64 / rate.num as f64
}

/// Format a source time as SMPTE-style timecode HH:MM:SS:FF using the source's
/// own frame rate. Negative values are prefixed with '-'.
pub fn format_timecode(time_sec: f64, rate: Rate) -> String {
    let negative = time_sec < 0.0;
    let total_frames = nearest_frame(time_sec.abs(), rate);
    let fps = (rate.num as f64 / rate.den as f64).round() as u32;
    let fps = if fps == 0 { 25 } else { fps };
    let frames = total_frames.rem_euclid(fps as i64);
    let total_seconds = total_frames.div_euclid(fps as i64);
    let seconds = total_seconds.rem_euclid(60);
    let total_minutes = total_seconds.div_euclid(60);
    let minutes = total_minutes.rem_euclid(60);
    let hours = total_minutes.div_euclid(60);
    format!(
        "{}{:02}:{:02}:{:02}:{:02}",
        if negative { "-" } else { "" },
        hours,
        minutes,
        seconds,
        frames
    )
}

/// Program-time clock format MM:SS.mmm, always positive.
pub fn format_program_time(time_sec: f64) -> String {
    let t = time_sec.max(0.0);
    let minutes = (t / 60.0).floor() as u64;
    let seconds = t % 60.0;
    format!("{:02}:{:06.3}", minutes, seconds)
}

pub fn approx_cmp(a: f64, b: f64, eps: f64) -> Ordering {
    if (a - b).abs() <= eps {
        Ordering::Equal
    } else if a < b {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}
