//! ffmpeg-based renderer.
//!
//! The [`RenderPlan`] decides every cut. Rendering runs in three deterministic
//! stages:
//!
//! 1. Each video segment is rendered to its own lossless FFV1 file. A segment
//!    owns one ffmpeg process, so there is no shared-input frame loss and the
//!    output frame window is trimmed in time (frame-accurate across mixed
//!    source rates, e.g. 30000/1001 into 25 fps).
//! 2. The segment files are joined with the concat demuxer (stream copy),
//!    which preserves every frame exactly.
//! 3. The single audio master is assembled independently — atrim pieces plus
//!    silence — and muxed with the picture. A camera cut therefore never adds
//!    a second interview audio track.
//!
//! Bit-exact flags and stripped metadata keep re-renders reproducible.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::hash::{sha256_hex, sha256_short};
use crate::json::Json;
use crate::plan::{RenderPlan, RenderSegment};
use crate::project::Project;

#[derive(Debug, Clone)]
pub struct RenderResult {
    pub revision: String,
    pub master_path: PathBuf,
    pub review_path: PathBuf,
    pub plan_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub video_hash: String,
    pub audio_hash: String,
}

#[derive(Debug, Clone)]
pub struct DecodeHashes {
    pub video_hash: String,
    pub audio_hash: String,
    pub frame_count: u64,
}

fn tool(env_var: &str, binary: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| binary.to_string())
}

pub fn ffmpeg_bin() -> String {
    tool("MCEDIT_FFMPEG", "ffmpeg")
}

pub fn ffprobe_bin() -> String {
    tool("MCEDIT_FFPROBE", "ffprobe")
}

fn resolve_clip(base: &Path, clip: &str) -> String {
    let p = Path::new(clip);
    if p.is_absolute() {
        clip.to_string()
    } else {
        base.join(p).to_string_lossy().to_string()
    }
}

fn rate_text(rate: crate::time::Rate) -> String {
    if rate.den == 1 {
        rate.num.to_string()
    } else {
        format!("{}/{}", rate.num, rate.den)
    }
}

/// Actual container duration in seconds, so trims never run past real EOF.
fn clip_actual_duration(path: &str) -> Result<f64, String> {
    let output = Command::new(ffprobe_bin())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            path,
        ])
        .output()
        .map_err(|e| format!("failed to launch {}: {}", ffprobe_bin(), e))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|e| e.to_string())
}

fn run_ffmpeg(args: &[String]) -> Result<String, String> {
    let output = Command::new(ffmpeg_bin())
        .args(args)
        .output()
        .map_err(|e| format!("failed to launch {}: {}", ffmpeg_bin(), e))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stderr).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(25)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        Err(format!("ffmpeg failed ({}):\n{}", output.status, tail))
    }
}

fn common_input_args() -> Vec<String> {
    vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-y".to_string(),
    ]
}

/// Stage 1: render one video segment to a standalone lossless file.
fn render_video_segment(
    seg: &RenderSegment,
    plan: &RenderPlan,
    base: &Path,
    w: u32,
    h: u32,
    out_file: &Path,
) -> Result<(), String> {
    let out_rate = rate_text(plan.output_rate);
    let mut args: Vec<String> = common_input_args().to_vec();
    let vf;
    if seg.black {
        // color is a source filter: feed it through the lavfi virtual input.
        args.push("-f".to_string());
        args.push("lavfi".to_string());
        args.push("-i".to_string());
        args.push(format!(
            "color=c=black:s={}x{}:r={}:d={:.6}",
            w, h, out_rate, seg.duration
        ));
        vf = "format=yuv420p".to_string();
    } else {
        let clip_path = seg.clip_path.as_ref().expect("non-black segment has clip");
        let clip = resolve_clip(base, clip_path);
        let clip_duration = clip_actual_duration(&clip)?;
        args.push("-i".to_string());
        args.push(clip);
        // Time window equals the output frame span; fps picks nearest source
        // frames so mixed-rate sources still yield exactly seg.frames frames.
        // Clamp to the real clip duration: source EOF may sit a few ms before
        // the idealized plan window at tail shortages.
        let trim_start = seg.clip_offset.min(clip_duration);
        // Keep a small epsilon beyond the ideal window so fps can still pick
        // the final output frame; -frames:v caps the result exactly.
        let trim_end = (seg.clip_offset + seg.duration + 0.002).min(clip_duration);
        vf = format!(
            "trim=start={:.6}:end={:.6},setpts=PTS-STARTPTS,\
             fps={}:round=near,\
             scale={}:{}:force_original_aspect_ratio=decrease,\
             pad={}:{}:(ow-iw)/2:(oh-ih)/2,setsar=1,\
             tpad=stop=-1:stop_mode=clone:stop_duration=1,format=yuv420p",
            trim_start, trim_end, out_rate, w, h, w, h
        );
    }
    args.extend([
        "-vf".to_string(),
        vf,
        "-r".to_string(),
        out_rate,
        "-fps_mode".to_string(),
        "cfr".to_string(),
        "-c:v".to_string(),
        "ffv1".to_string(),
        "-level".to_string(),
        "3".to_string(),
        "-an".to_string(),
        "-frames:v".to_string(),
        seg.frames.to_string(),
        out_file.to_string_lossy().to_string(),
    ]);
    run_ffmpeg(&args).map(|_| ())
}

/// Stage 2: concat demuxer stream-copies the per-segment files losslessly.
fn concat_segments(segment_files: &[PathBuf], video_out: &Path) -> Result<(), String> {
    let mut list = String::new();
    for f in segment_files {
        // Absolute paths keep the concat demuxer independent of its own cwd.
        let abs = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
        let p = abs.to_string_lossy().replace('\'', "'\\''");
        list.push_str(&format!("file '{}'\n", p));
    }
    let list_path = video_out.with_extension("concat.txt");
    std::fs::write(&list_path, list).map_err(|e| e.to_string())?;
    let args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-y".to_string(),
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        list_path.to_string_lossy().to_string(),
        "-c".to_string(),
        "copy".to_string(),
        video_out.to_string_lossy().to_string(),
    ];
    run_ffmpeg(&args).map(|_| ())
}

/// Stage 3: build the independent audio master as a stereo s16le WAV of exact
/// program duration. Each piece is its own ffmpeg pass, then concatenated.
fn build_audio_wav(
    project: &Project,
    plan: &RenderPlan,
    base: &Path,
    tmp_dir: &Path,
) -> Result<PathBuf, String> {
    let sr = project.output.sample_rate;
    let mut piece_files = Vec::new();
    for seg in &plan.audio_segments {
        let piece = tmp_dir.join(format!("audio-{}.wav", seg.index));
        let mut args: Vec<String> = common_input_args().to_vec();
        let af;
        if seg.silence {
            args.push("-f".to_string());
            args.push("lavfi".to_string());
            args.push("-i".to_string());
            args.push(format!(
                "anullsrc=channel_layout=stereo:sample_rate={}:d={:.6}",
                sr, seg.duration
            ));
            af = format!("aformat=sample_rates={}:channel_layouts=stereo:sample_fmts=s16", sr);
        } else {
            let clip = resolve_clip(base, seg.clip_path.as_ref().expect("audio segment has clip"));
            args.push("-i".to_string());
            args.push(clip);
            af = format!(
                "atrim=start={:.6}:end={:.6},asetpts=PTS-STARTPTS,\
                 aformat=sample_rates={}:channel_layouts=stereo:sample_fmts=s16",
                seg.clip_offset,
                seg.clip_offset + seg.duration,
                sr
            );
        }
        args.extend([
            "-af".to_string(),
            af,
            "-t".to_string(),
            format!("{:.6}", seg.duration),
            "-c:a".to_string(),
            "pcm_s16le".to_string(),
            piece.to_string_lossy().to_string(),
        ]);
        run_ffmpeg(&args)?;
        piece_files.push(piece);
    }

    let list_path = tmp_dir.join("audio.concat.txt");
    let mut list = String::new();
    for f in &piece_files {
        let abs = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
        let p = abs.to_string_lossy().replace('\'', "'\\''");
        list.push_str(&format!("file '{}'\n", p));
    }
    std::fs::write(&list_path, list).map_err(|e| e.to_string())?;
    let wav_out = tmp_dir.join("audio-master.wav");
    let args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-y".to_string(),
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        list_path.to_string_lossy().to_string(),
        "-c".to_string(),
        "copy".to_string(),
        wav_out.to_string_lossy().to_string(),
    ];
    run_ffmpeg(&args)?;
    Ok(wav_out)
}

fn mux(video: &Path, audio: &Path, out: &Path, master: bool) -> Result<(), String> {
    let mut args: Vec<String> = common_input_args().to_vec();
    args.extend(["-i".to_string(), video.to_string_lossy().to_string()]);
    args.extend(["-i".to_string(), audio.to_string_lossy().to_string()]);
    args.extend([
        "-map".to_string(),
        "0:v:0".to_string(),
        "-map".to_string(),
        "1:a:0".to_string(),
        "-map_metadata".to_string(),
        "-1".to_string(),
        "-fflags".to_string(),
        "+bitexact".to_string(),
        "-flags".to_string(),
        "+bitexact".to_string(),
    ]);
    if master {
        args.extend([
            "-c:v".to_string(),
            "copy".to_string(),
            "-c:a".to_string(),
            "flac".to_string(),
            out.to_string_lossy().to_string(),
        ]);
    } else {
        args.extend([
            "-c:v".to_string(),
            "libx264".to_string(),
            "-preset".to_string(),
            "medium".to_string(),
            "-crf".to_string(),
            "20".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            "128k".to_string(),
            "-movflags".to_string(),
            "+faststart".to_string(),
            out.to_string_lossy().to_string(),
        ]);
    }
    run_ffmpeg(&args).map(|_| ())
}

/// Render the project. `base` resolves clip-relative paths; outputs are written
/// next to `out_dir`.
pub fn render(project: &Project, plan: &RenderPlan, base: &Path, out_dir: &Path) -> Result<RenderResult, String> {
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let revision = project.revision();
    let stem = sanitize_stem(&project.name);
    let master_path = out_dir.join(format!("{}.{}.master.mkv", stem, revision));
    let review_path = out_dir.join(format!("{}.{}.review.mp4", stem, revision));
    let plan_path = out_dir.join(format!("{}.{}.plan.json", stem, revision));
    let sidecar_path = out_dir.join(format!("{}.{}.sidecar.json", stem, revision));

    let tmp_dir = out_dir.join(format!("tmp-{}", revision));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
    let w = project.output.width;
    let h = project.output.height;

    // Stage 1: per-segment lossless files.
    let mut segment_files = Vec::new();
    for seg in &plan.segments {
        let seg_file = tmp_dir.join(format!("seg-{:04}.mkv", seg.index));
        render_video_segment(seg, plan, base, w, h, &seg_file)?;
        segment_files.push(seg_file);
    }

    // Stage 2: exact concat.
    let video_only = tmp_dir.join("video.mkv");
    concat_segments(&segment_files, &video_only)?;

    // Stage 3: independent audio master and mux.
    let audio_wav = build_audio_wav(project, plan, base, &tmp_dir)?;
    mux(&video_only, &audio_wav, &master_path, true)?;
    mux(&video_only, &audio_wav, &review_path, false)?;

    let hashes = decode_hashes(&master_path, project)?;
    let input_hashes = input_file_hashes(plan, base, project);
    let sidecar = build_sidecar(project, plan, &revision, &master_path, &review_path, &hashes, input_hashes);
    std::fs::write(&sidecar_path, sidecar.to_string_pretty()).map_err(|e| e.to_string())?;
    std::fs::write(&plan_path, plan.to_json().to_string_pretty()).map_err(|e| e.to_string())?;

    std::fs::remove_dir_all(&tmp_dir).ok();

    Ok(RenderResult {
        revision,
        master_path,
        review_path,
        plan_path,
        sidecar_path,
        video_hash: hashes.video_hash,
        audio_hash: hashes.audio_hash,
    })
}

fn input_file_hashes(plan: &RenderPlan, base: &Path, project: &Project) -> Vec<(String, String)> {
    let mut paths: Vec<String> = Vec::new();
    for seg in &plan.segments {
        if let Some(p) = &seg.clip_path {
            paths.push(resolve_clip(base, p));
        }
    }
    for seg in &plan.audio_segments {
        if let Some(p) = &seg.clip_path {
            paths.push(resolve_clip(base, p));
        }
    }
    if let Some(ext) = &project.audio.external_path {
        paths.push(resolve_clip(base, ext));
    }
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|p| {
            let data = std::fs::read(&p).unwrap_or_default();
            (p, sha256_hex(&data))
        })
        .collect()
}

fn build_sidecar(
    project: &Project,
    plan: &RenderPlan,
    revision: &str,
    master: &Path,
    review: &Path,
    hashes: &DecodeHashes,
    input_hashes: Vec<(String, String)>,
) -> Json {
    let mut o = Json::obj();
    o.insert("schema", Json::Str("mcedit-sidecar/1".to_string()));
    o.insert("project_name", Json::Str(project.name.clone()));
    o.insert("revision", Json::Str(revision.to_string()));
    o.insert("ffmpeg", Json::Str(ffmpeg_version().unwrap_or_else(|| "unknown".to_string())));
    o.insert("total_frames", Json::Num(plan.total_frames as f64));
    o.insert("duration", Json::Num(plan.duration));
    o.insert("video_decoded_sha256", Json::Str(hashes.video_hash.clone()));
    o.insert("audio_decoded_sha256", Json::Str(hashes.audio_hash.clone()));
    o.insert("frame_count", Json::Num(hashes.frame_count as f64));
    o.insert(
        "master_file",
        Json::Str(master.file_name().unwrap_or_default().to_string_lossy().to_string()),
    );
    o.insert(
        "review_file",
        Json::Str(review.file_name().unwrap_or_default().to_string_lossy().to_string()),
    );
    let inputs = Json::Arr(
        input_hashes
            .into_iter()
            .map(|(path, hash)| {
                let mut e = Json::obj();
                e.insert("path", Json::Str(path));
                e.insert("sha256", Json::Str(hash));
                e
            })
            .collect(),
    );
    o.insert("inputs", inputs);
    o.insert(
        "video_segments",
        Json::Arr(plan.segments.iter().map(|s| s.to_json()).collect()),
    );
    o.insert(
        "audio_segments",
        Json::Arr(plan.audio_segments.iter().map(|s| s.to_json()).collect()),
    );
    o
}

fn ffmpeg_version() -> Option<String> {
    let out = Command::new(ffmpeg_bin()).arg("-version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(|s| s.to_string())
}

/// Decode a rendered file to raw YUV420p and PCM s16le and hash each stream.
/// These hashes are content hashes, independent of container/encoder headers.
pub fn decode_hashes(file: &Path, project: &Project) -> Result<DecodeHashes, String> {
    let w = project.output.width;
    let h = project.output.height;
    let frame_size = (w * h * 3 / 2) as usize;

    let output = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            file.to_string_lossy().as_ref(),
            "-map",
            "0:v:0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "video decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let raw = &output.stdout;
    let frame_count = raw.len() / frame_size;
    let video_hash = sha256_hex(raw);

    let output = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            file.to_string_lossy().as_ref(),
            "-map",
            "0:a:0",
            "-ac",
            "2",
            "-ar",
            &project.output.sample_rate.to_string(),
            "-f",
            "s16le",
            "-",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "audio decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let audio_hash = sha256_hex(&output.stdout);

    Ok(DecodeHashes {
        video_hash,
        audio_hash,
        frame_count: frame_count as u64,
    })
}

fn sanitize_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "project".to_string()
    } else {
        cleaned
    }
}

/// Re-render into a temporary directory and confirm identical decoded content.
pub fn verify_reproducible(
    project: &Project,
    plan: &RenderPlan,
    base: &Path,
    out_dir: &Path,
) -> Result<(bool, DecodeHashes, DecodeHashes), String> {
    let first = render(project, plan, base, out_dir)?;
    let tmp = out_dir.join(format!("repro-{}", sha256_short(first.revision.as_bytes())));
    let second = render(project, plan, base, &tmp)?;
    let a = decode_hashes(&first.master_path, project)?;
    let b = decode_hashes(&second.master_path, project)?;
    std::fs::remove_dir_all(&tmp).ok();
    Ok((a.video_hash == b.video_hash && a.audio_hash == b.audio_hash, a, b))
}
