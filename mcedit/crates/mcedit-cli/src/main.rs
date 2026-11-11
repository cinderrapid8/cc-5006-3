use std::path::PathBuf;
use std::process::ExitCode;

use mcedit_core::json::str_val;
use mcedit_core::plan::RenderPlan;
use mcedit_core::render::{decode_hashes, render, verify_reproducible};
use mcedit_core::{load_project, Json};
use mcedit_server::{describe_sources, parse_rate, serve, verify_file};

fn usage() -> &'static str {
    "mcedit - multi-camera interview editor\n\n\
USAGE:\n  \
mcedit validate <project.json>\n  \
mcedit plan <project.json>\n  \
mcedit render <project.json> [--base DIR] [--out DIR] [--repeat]\n  \
mcedit verify <project.json> <master.mkv>\n  \
mcedit sources <project.json>\n  \
mcedit serve --base DIR [--addr 127.0.0.1:8088]\n"
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprint!("{}", usage());
        return ExitCode::from(2);
    }
    let cmd = args[1].as_str();
    let result = match cmd {
        "validate" => cmd_validate(&args[2..]),
        "plan" => cmd_plan(&args[2..]),
        "render" => cmd_render(&args[2..]),
        "verify" => cmd_verify(&args[2..]),
        "sources" => cmd_sources(&args[2..]),
        "serve" => cmd_serve(&args[2..]),
        "-h" | "--help" | "help" => {
            print!("{}", usage());
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command '{}'", other)),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::FAILURE
        }
    }
}

type CmdResult = Result<ExitCode, String>;

fn cmd_validate(args: &[String]) -> CmdResult {
    let path = first_path(args)?;
    let (project, _) = load_project(&path)?;
    let report = project.validate();
    let mut out = Json::obj();
    out.insert("project", str_val(project.name.clone()));
    out.insert("revision", str_val(project.revision()));
    out.insert("ok", Json::Bool(report.ok()));
    out.insert(
        "errors",
        Json::Arr(report.errors.iter().map(|e| Json::Str(e.clone())).collect()),
    );
    out.insert(
        "warnings",
        Json::Arr(report.warnings.iter().map(|e| Json::Str(e.clone())).collect()),
    );
    out.insert("gaps", Json::Arr(report.gaps.iter().map(|g| g.to_json()).collect()));
    println!("{}", out.to_string_pretty());
    Ok(if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    })
}

fn cmd_plan(args: &[String]) -> CmdResult {
    let path = first_path(args)?;
    let (project, _) = load_project(&path)?;
    let plan = RenderPlan::build(&project)?;
    println!("{}", plan.to_json().to_string_pretty());
    Ok(ExitCode::SUCCESS)
}

fn cmd_render(args: &[String]) -> CmdResult {
    let project_path = first_path(args)?;
    let (project, _) = load_project(&project_path)?;
    let report = project.validate();
    if !report.ok() {
        return Err(format!("project invalid: {}", report.errors.join("; ")));
    }
    let plan = RenderPlan::build(&project)?;
    let base = flag_value(args, "--base")
        .map(PathBuf::from)
        .or_else(|| project_path.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let out_dir = flag_value(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| base.join("renders"));

    if args.iter().any(|a| a == "--repeat") {
        let (same, a, b) = verify_reproducible(&project, &plan, &base, &out_dir)?;
        println!(
            "{}",
            serde_like_summary(
                &project,
                &a.video_hash,
                &a.audio_hash,
                a.frame_count,
                same,
                &b.video_hash,
                &b.audio_hash,
            )
        );
        return Ok(if same {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(4)
        });
    }

    let result = render(&project, &plan, &base, &out_dir)?;
    let hashes = decode_hashes(&result.master_path, &project)?;
    println!(
        "{}",
        serde_like_summary(
            &project,
            &result.video_hash,
            &result.audio_hash,
            hashes.frame_count,
            true,
            "",
            "",
        )
    );
    println!("master: {}", result.master_path.display());
    println!("review: {}", result.review_path.display());
    println!("plan:   {}", result.plan_path.display());
    println!("sidecar:{}", result.sidecar_path.display());
    Ok(ExitCode::SUCCESS)
}

fn serde_like_summary(
    project: &mcedit_core::Project,
    video_hash: &str,
    audio_hash: &str,
    frame_count: u64,
    reproducible: bool,
    second_video: &str,
    second_audio: &str,
) -> String {
    let mut o = Json::obj();
    o.insert("project", Json::Str(project.name.clone()));
    o.insert("revision", Json::Str(project.revision()));
    o.insert("video_decoded_sha256", Json::Str(video_hash.to_string()));
    o.insert("audio_decoded_sha256", Json::Str(audio_hash.to_string()));
    o.insert("frame_count", Json::Num(frame_count as f64));
    o.insert("reproducible", Json::Bool(reproducible));
    if !second_video.is_empty() {
        o.insert("second_video_sha256", Json::Str(second_video.to_string()));
        o.insert("second_audio_sha256", Json::Str(second_audio.to_string()));
    }
    o.to_string_pretty()
}

fn cmd_verify(args: &[String]) -> CmdResult {
    let project_path = first_path(args)?;
    let master = args
        .get(1)
        .map(PathBuf::from)
        .ok_or("verify requires <project.json> <master.mkv>")?;
    let (project, _) = load_project(&project_path)?;
    let (video, audio, frames) = verify_file(&project, &master)?;
    let mut o = Json::obj();
    o.insert("video_decoded_sha256", Json::Str(video));
    o.insert("audio_decoded_sha256", Json::Str(audio));
    o.insert("frame_count", Json::Num(frames as f64));
    o.insert("expected_frames", Json::Num(
        (project.output.duration * project.output.rate.num as f64 / project.output.rate.den as f64).round() as f64,
    ));
    println!("{}", o.to_string_pretty());
    Ok(ExitCode::SUCCESS)
}

fn cmd_sources(args: &[String]) -> CmdResult {
    let path = first_path(args)?;
    let (project, _) = load_project(&path)?;
    println!("{}", describe_sources(&project).to_string_pretty());
    Ok(ExitCode::SUCCESS)
}

fn cmd_serve(args: &[String]) -> CmdResult {
    let base = flag_value(args, "--base")
        .map(PathBuf::from)
        .ok_or("serve requires --base DIR")?;
    let addr = flag_value(args, "--addr").unwrap_or("127.0.0.1:8088".to_string());
    let _ = parse_rate("25")?;
    serve(base, &addr).map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

fn first_path(args: &[String]) -> Result<PathBuf, String> {
    args.first()
        .filter(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .ok_or("missing project file argument".to_string())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
