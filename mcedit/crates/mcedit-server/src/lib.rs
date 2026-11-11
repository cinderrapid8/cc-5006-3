//! Small std-only HTTP server: static web UI, project persistence, frame-level
//! state queries and render jobs. No external crates, so it builds offline.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use mcedit_core::json::{num_val, str_val, Json};
use mcedit_core::plan::RenderPlan;
use mcedit_core::project::{Project, ValidationReport};
use mcedit_core::render::{decode_hashes, render, RenderResult};
use mcedit_core::time::Rate;

struct JobState {
    status: String,
    message: String,
    result: Option<RenderResult>,
}

struct AppState {
    base: PathBuf,
    jobs: Mutex<HashMap<String, JobState>>,
}

struct Request {
    method: String,
    path: String,
    body: String,
}

const INDEX_HTML: &str = include_str!("../../../web/index.html");
const APP_JS: &str = include_str!("../../../web/app.js");
const STYLES_CSS: &str = include_str!("../../../web/styles.css");

static STATE_FIXED: OnceLock<&'static AppState> = OnceLock::new();

fn state() -> &'static AppState {
    STATE_FIXED.get().expect("server started before serving requests")
}

pub fn serve(base: PathBuf, addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    eprintln!("mcedit serving {} on http://{}", base.display(), bound);
    let app: &'static AppState = Box::leak(Box::new(AppState {
        base,
        jobs: Mutex::new(HashMap::new()),
    }));
    // If two servers are started in one process the second is ignored; the
    // CLI only starts one.
    let _ = STATE_FIXED.set(app);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(e) = handle(stream) {
                        eprintln!("connection error: {}", e);
                    }
                });
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream) -> Result<(), String> {
    let mut reader_buf = Vec::new();
    let request = read_request(&mut stream, &mut reader_buf)?;
    let response = route(&request);
    write_response(&mut stream, &response)
}

fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Request, String> {
    let mut header_end = None;
    let mut tmp = [0u8; 4096];
    while header_end.is_none() {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("client closed".to_string());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
        }
        if buf.len() > 64 * 1024 * 1024 {
            return Err("request too large".to_string());
        }
    }
    let header_end = header_end.unwrap();
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().ok_or("empty request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("bad request line")?.to_string();
    let raw_path = parts.next().ok_or("bad request line")?.to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let lower = l.to_ascii_lowercase();
            lower.strip_prefix("content-length:").map(|v| v.trim().to_string())
        })
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string();
    let path = raw_path.split('?').next().unwrap_or("/").to_string();
    Ok(Request { method, path, body })
}

struct Response {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

fn ok_json(value: Json) -> Response {
    Response {
        status: 200,
        content_type: "application/json; charset=utf-8".to_string(),
        body: value.to_string_pretty().into_bytes(),
    }
}

fn error_json(status: u16, message: &str) -> Response {
    let mut o = Json::obj();
    o.insert("error", str_val(message));
    Response {
        status,
        content_type: "application/json; charset=utf-8".to_string(),
        body: o.to_string_pretty().into_bytes(),
    }
}

fn route(req: &Request) -> Response {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => static_asset("text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        ("GET", "/app.js") => static_asset("application/javascript; charset=utf-8", APP_JS.as_bytes()),
        ("GET", "/styles.css") => static_asset("text/css; charset=utf-8", STYLES_CSS.as_bytes()),
        ("GET", "/api/health") => ok_json(health()),
        ("POST", "/api/projects") => save_project(req),
        ("GET", path) if path.starts_with("/api/projects/") => get_project(path),
        ("POST", path) if path.ends_with("/validate") => with_project(path, |p| ok_json(validation_json(&p.validate()))),
        ("POST", path) if path.ends_with("/plan") => with_project(path, |p| match RenderPlan::build(&p) {
            Ok(plan) => ok_json(plan.to_json()),
            Err(e) => error_json(400, &e),
        }),
        ("POST", path) if path.ends_with("/render") => start_render(path),
        ("GET", path) if path.starts_with("/api/jobs/") => get_job(path),
        ("GET", path) if path.starts_with("/raw/") => serve_raw(path),
        ("GET", path) if path.starts_with("/renders/") => serve_render_file(path),
        _ => error_json(404, "not found"),
    }
}

fn static_asset(content_type: &str, body: &[u8]) -> Response {
    Response {
        status: 200,
        content_type: content_type.to_string(),
        body: body.to_vec(),
    }
}

fn health() -> Json {
    let mut o = Json::obj();
    o.insert("status", str_val("ok"));
    o.insert("service", str_val("mcedit"));
    o.insert("base", str_val(state().base.to_string_lossy()));
    o
}

fn project_file(name: &str) -> PathBuf {
    state().base.join("projects").join(format!("{}.json", name))
}

fn save_project(req: &Request) -> Response {
    let value = match Json::from_str(&req.body) {
        Ok(v) => v,
        Err(e) => return error_json(400, &format!("invalid JSON: {}", e)),
    };
    let project = match Project::from_json(&value) {
        Ok(p) => p,
        Err(e) => return error_json(400, &e),
    };
    let dir = state().base.join("projects");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return error_json(500, &e.to_string());
    }
    let path = project_file(&project.name);
    if let Err(e) = std::fs::write(&path, project.content_json().to_string_pretty()) {
        return error_json(500, &e.to_string());
    }
    let mut o = Json::obj();
    o.insert("saved", Json::Bool(true));
    o.insert("name", str_val(project.name.clone()));
    o.insert("revision", str_val(project.revision()));
    o.insert("path", str_val(path.to_string_lossy()));
    ok_json(o)
}

fn get_project(path: &str) -> Response {
    let name = path.trim_start_matches("/api/projects/");
    let file = project_file(name);
    match std::fs::read_to_string(&file) {
        Ok(text) => match Json::from_str(&text) {
            Ok(v) => ok_json(v),
            Err(e) => error_json(500, &e),
        },
        Err(_) => error_json(404, "project not found"),
    }
}

fn with_project(path: &str, f: impl FnOnce(Project) -> Response) -> Response {
    let name = path
        .trim_start_matches("/api/projects/")
        .split('/')
        .next()
        .unwrap_or("");
    let file = project_file(name);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(_) => return error_json(404, "project not found"),
    };
    let value = match Json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return error_json(500, &e),
    };
    match Project::from_json(&value) {
        Ok(p) => f(p),
        Err(e) => error_json(400, &e),
    }
}

fn validation_json(report: &ValidationReport) -> Json {
    let mut o = Json::obj();
    o.insert("ok", Json::Bool(report.ok()));
    o.insert(
        "errors",
        Json::Arr(report.errors.iter().map(|e| str_val(e.clone())).collect()),
    );
    o.insert(
        "warnings",
        Json::Arr(report.warnings.iter().map(|e| str_val(e.clone())).collect()),
    );
    o.insert(
        "gaps",
        Json::Arr(report.gaps.iter().map(|g| g.to_json()).collect()),
    );
    o
}

fn start_render(path: &str) -> Response {
    let name = path
        .trim_start_matches("/api/projects/")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    let file = project_file(&name);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(_) => return error_json(404, "project not found"),
    };
    let value = match Json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return error_json(500, &e),
    };
    let project = match Project::from_json(&value) {
        Ok(p) => p,
        Err(e) => return error_json(400, &e),
    };
    let report = project.validate();
    if !report.ok() {
        return error_json(409, &format!("project invalid: {}", report.errors.join("; ")));
    }
    let plan = match RenderPlan::build(&project) {
        Ok(p) => p,
        Err(e) => return error_json(400, &e),
    };

    let job_id = format!(
        "job-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        &project.revision()[..8]
    );
    state().jobs.lock().unwrap().insert(
        job_id.clone(),
        JobState {
            status: "running".to_string(),
            message: "render started".to_string(),
            result: None,
        },
    );

    let base = state().base.clone();
    let job_for_thread = job_id.clone();
    thread::spawn(move || {
        let out_dir = base.join("renders");
        let outcome = render(&project, &plan, &base, &out_dir);
        let mut jobs = state().jobs.lock().unwrap();
        match outcome {
            Ok(result) => {
                jobs.insert(
                    job_for_thread,
                    JobState {
                        status: "done".to_string(),
                        message: "render complete".to_string(),
                        result: Some(result),
                    },
                );
            }
            Err(e) => {
                jobs.insert(
                    job_for_thread,
                    JobState {
                        status: "error".to_string(),
                        message: e,
                        result: None,
                    },
                );
            }
        }
    });

    let mut o = Json::obj();
    o.insert("job_id", str_val(job_id));
    o.insert("status", str_val("running"));
    ok_json(o)
}

fn get_job(path: &str) -> Response {
    let job_id = path.trim_start_matches("/api/jobs/");
    let jobs = state().jobs.lock().unwrap();
    match jobs.get(job_id) {
        Some(job) => {
            let mut o = Json::obj();
            o.insert("job_id", str_val(job_id));
            o.insert("status", str_val(job.status.clone()));
            o.insert("message", str_val(job.message.clone()));
            if let Some(r) = &job.result {
                let mut res = Json::obj();
                res.insert("revision", str_val(r.revision.clone()));
                res.insert("video_hash", str_val(r.video_hash.clone()));
                res.insert("audio_hash", str_val(r.audio_hash.clone()));
                res.insert(
                    "master_url",
                    str_val(format!("/renders/{}", r.master_path.file_name().unwrap().to_string_lossy())),
                );
                res.insert(
                    "review_url",
                    str_val(format!("/renders/{}", r.review_path.file_name().unwrap().to_string_lossy())),
                );
                res.insert(
                    "plan_url",
                    str_val(format!("/renders/{}", r.plan_path.file_name().unwrap().to_string_lossy())),
                );
                res.insert(
                    "sidecar_url",
                    str_val(format!("/renders/{}", r.sidecar_path.file_name().unwrap().to_string_lossy())),
                );
                o.insert("result", res);
            }
            ok_json(o)
        }
        None => error_json(404, "unknown job"),
    }
}

/// Resolve a relative media path under the data base, refusing traversal.
fn safe_base_path(prefix: &str, subdir: &str, url_path: &str) -> Option<PathBuf> {
    let rel = url_path.strip_prefix(prefix)?;
    let candidate = if subdir.is_empty() {
        state().base.join(rel)
    } else {
        state().base.join(subdir).join(rel)
    };
    let normalized = normalize(&candidate);
    if normalized.starts_with(&state().base) {
        Some(normalized)
    } else {
        None
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn serve_raw(path: &str) -> Response {
    match safe_base_path("/raw/", "", path) {
        Some(file) => serve_file(&file),
        None => error_json(403, "path outside data directory"),
    }
}

fn serve_render_file(path: &str) -> Response {
    match safe_base_path("/renders/", "renders", path) {
        Some(file) => serve_file(&file),
        None => error_json(403, "path outside data directory"),
    }
}

fn serve_file(file: &Path) -> Response {
    match std::fs::read(file) {
        Ok(bytes) => Response {
            status: 200,
            content_type: guess_content_type(file).to_string(),
            body: bytes,
        },
        Err(_) => error_json(404, "file not found"),
    }
}

fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "wav" => "audio/wav",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

fn write_response(stream: &mut TcpStream, resp: &Response) -> Result<(), String> {
    let reason = match resp.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        resp.status,
        reason,
        resp.content_type,
        resp.body.len()
    );
    stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(&resp.body).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

/// State helper used by the CLI to inspect a project over the same code path
/// as the HTTP layer.
pub fn describe_sources(project: &Project) -> Json {
    let mut arr = Json::Arr(Vec::new());
    for source in &project.sources {
        let mut o = Json::obj();
        o.insert("id", str_val(source.id.clone()));
        o.insert("name", str_val(source.name.clone()));
        o.insert("rate", num_val(source.rate.as_f64()));
        match source.mapping() {
            Some(mapping) => {
                o.insert("anchored", Json::Bool(true));
                o.insert("mapping", mapping.to_json());
                if let Some(intervals) = source.program_intervals() {
                    o.insert(
                        "program_intervals",
                        Json::Arr(
                            intervals
                                .iter()
                                .map(|(a, b)| {
                                    let mut e = Json::obj();
                                    e.insert("start", Json::Num(*a));
                                    e.insert("end", Json::Num(*b));
                                    e
                                })
                                .collect(),
                        ),
                    );
                }
            }
            None => o.insert("anchored", Json::Bool(false)),
        }
        if let Json::Arr(items) = &mut arr {
            items.push(o);
        }
    }
    arr
}

/// Verify a rendered master against a project; exposed for CLI reuse.
pub fn verify_file(project: &Project, file: &Path) -> Result<(String, String, u64), String> {
    let hashes = decode_hashes(file, project)?;
    Ok((hashes.video_hash, hashes.audio_hash, hashes.frame_count))
}

pub fn parse_rate(text: &str) -> Result<Rate, String> {
    if let Some((n, d)) = text.split_once('/') {
        Ok(Rate {
            num: n.parse().map_err(|e: std::num::ParseIntError| e.to_string())?,
            den: d.parse().map_err(|e: std::num::ParseIntError| e.to_string())?,
        })
    } else {
        Ok(Rate::fps(text.parse().map_err(|e: std::num::ParseIntError| e.to_string())?))
    }
}
