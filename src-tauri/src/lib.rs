use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::OnceLock;

// ─────────────────────────────────────────────────────────────────────────────
// Data types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LivePhoto {
    pub name: String,
    pub display_name: String,
    pub image_path: String,
    pub video_path: String,
    pub file_size: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Local HTTP file server
//
// Serves arbitrary local files over http://127.0.0.1:<PORT>/…
// so that <img> src attributes can reference frame images extracted into the
// OS temp directory.  Range requests are honoured.
// ─────────────────────────────────────────────────────────────────────────────

static SERVER_PORT: OnceLock<u16> = OnceLock::new();

pub fn start_file_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind file server");
    let port = listener.local_addr().unwrap().port();
    SERVER_PORT.set(port).ok();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                if let Err(e) = serve_http(stream) {
                    if e.kind() != std::io::ErrorKind::BrokenPipe
                        && e.kind() != std::io::ErrorKind::ConnectionReset
                    {
                        eprintln!("[file-server] {e}");
                    }
                }
            });
        }
    });
}

#[tauri::command]
fn get_server_port() -> u16 {
    *SERVER_PORT.get().unwrap_or(&0)
}

fn serve_http(stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut wstream = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let mut req_line = String::new();
    reader.read_line(&mut req_line)?;

    let path_enc = match req_line.trim().split_whitespace().nth(1) {
        Some(p) => p,
        None => {
            write!(wstream, "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
    };

    let mut range_header: Option<String> = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if line.to_ascii_lowercase().starts_with("range:") {
            range_header = line.splitn(2, ':').nth(1).map(|v| v.trim().to_owned());
        }
    }

    let decoded = percent_decode(path_enc);
    let file_path: String = if cfg!(windows)
        && decoded.starts_with('/')
        && decoded.len() > 3
        && decoded.as_bytes().get(2).copied() == Some(b':')
    {
        decoded[1..].to_owned()
    } else {
        decoded
    };

    if file_path.contains("..") {
        write!(wstream, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")?;
        return Ok(());
    }

    let mut file = match std::fs::File::open(&file_path) {
        Ok(f) => f,
        Err(_) => {
            write!(wstream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
    };
    let file_size = file.metadata()?.len();
    let ct = mime_for(&file_path);

    if let Some(range_str) = range_header {
        if let Some(val) = range_str.strip_prefix("bytes=") {
            let mut parts = val.splitn(2, '-');
            let start: u64 = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let end: u64 = parts
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(file_size.saturating_sub(1))
                .min(file_size.saturating_sub(1));
            let length = end - start + 1;
            file.seek(SeekFrom::Start(start))?;
            write!(wstream, "HTTP/1.1 206 Partial Content\r\nContent-Type: {ct}\r\nContent-Range: bytes {start}-{end}/{file_size}\r\nContent-Length: {length}\r\nAccept-Ranges: bytes\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n")?;
            stream_bytes(&mut file, &mut wstream, length)?;
            return Ok(());
        }
    }

    write!(wstream, "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {file_size}\r\nAccept-Ranges: bytes\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n")?;
    stream_bytes(&mut file, &mut wstream, file_size)
}

fn stream_bytes(src: &mut dyn Read, dst: &mut dyn Write, n: u64) -> std::io::Result<()> {
    let mut buf = vec![0u8; 65536];
    let mut rem = n;
    while rem > 0 {
        let chunk = (buf.len() as u64).min(rem) as usize;
        let r = src.read(&mut buf[..chunk])?;
        if r == 0 {
            break;
        }
        dst.write_all(&buf[..r])?;
        rem -= r as u64;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Video frame extraction via gst-launch-1.0 (separate process)
//
// Why gst-launch instead of <video> element?
//
// The video (H.265/HEVC in QuickTime) causes WebKit2GTK's in-process GStreamer
// pipeline to call g_signal_connect_data() on a NULL GObject, printing
// GLib-GObject-CRITICAL warnings and hanging the WebProcess.
//
// Running gst-launch-1.0 as a child process isolates any GStreamer issues.
// Frames are extracted as JPEG files into the OS temp directory, then served
// by our HTTP file server so the browser can display them as <img> elements.
// This gives a smooth "stop-motion" Live Photo animation with zero risk of
// crashing the WebKit process.
// ─────────────────────────────────────────────────────────────────────────────

fn path_hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn frame_cache_dir(video_path: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("livephoto_{:016x}", path_hash(video_path)))
}

/// Collect up to `max` frame paths (0-indexed, gst multifilesink format).
fn collect_frames(dir: &Path, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..(max * 4) {
        let p = dir.join(format!("f{i:05}.jpg"));
        if p.exists() {
            out.push(p.to_string_lossy().into_owned());
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Extract `frame_count` evenly-spaced preview frames from `video_path`.
/// Returns absolute paths to JPEG files (served via the local HTTP server).
/// Results are cached; repeated calls for the same video return instantly.
#[tauri::command]
fn extract_video_frames(video_path: String, frame_count: u32) -> Result<Vec<String>, String> {
    let cache = frame_cache_dir(&video_path);

    // Return cached frames if present
    let cached = collect_frames(&cache, frame_count as usize);
    if !cached.is_empty() {
        return Ok(cached);
    }

    std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;

    let pattern = cache.join("f%05d.jpg");
    let loc_arg = format!("location={}", video_path);
    let sink_arg = format!("location={}", pattern.to_string_lossy());

    // gst-launch-1.0 pipeline:
    //   filesrc → decodebin (handles H.264, H.265, …)
    //          → videorate @ 3 fps → videoconvert → jpegenc → multifilesink
    //
    // Running in a child process: even if this crashes (unsupported codec),
    // the main app and WebKit web process are completely unaffected.
    let gst = if std::path::Path::new("/usr/bin/gst-launch-1.0").exists() {
        "/usr/bin/gst-launch-1.0"
    } else {
        "gst-launch-1.0"
    };

    let output = std::process::Command::new(gst)
        .args([
            "-e",
            "filesrc",
            &loc_arg,
            "!",
            "decodebin",
            "!",
            "videorate",
            "!",
            "video/x-raw,framerate=3/1",
            "!",
            "videoconvert",
            "!",
            "jpegenc",
            "quality=80",
            "!",
            "multifilesink",
            &sink_arg,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("gst-launch-1.0 not found: {e}"))?;

    let frames = collect_frames(&cache, frame_count as usize);
    if frames.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet = &stderr[..stderr.len().min(500)];
        Err(format!("exit={} stderr={}", output.status, snippet))
    } else {
        Ok(frames)
    }
}

/// Open a file with the OS default application (video player, etc.).
#[tauri::command]
fn open_with_system_player(path: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;

    #[cfg(windows)]
    std::process::Command::new("cmd")
        .args(["/c", "start", "", &path])
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Live photo scanner
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
fn list_live_photos(dir: String) -> Result<Vec<LivePhoto>, String> {
    let path = Path::new(&dir);
    if !path.is_dir() {
        return Err(format!("'{dir}' is not a valid directory"));
    }

    let mut photos = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|e| e.to_string())?.flatten() {
        let file_path = entry.path();
        let Some(ext) = file_path.extension() else {
            continue;
        };
        let ext_lower = ext.to_string_lossy().to_lowercase();
        if !matches!(ext_lower.as_str(), "jpg" | "jpeg" | "heic" | "heif" | "png") {
            continue;
        }
        let stem = file_path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        for video_ext in &["mov", "MOV", "mp4", "MP4", "m4v", "M4V"] {
            let video_path = file_path.with_extension(video_ext);
            if video_path.exists() {
                let file_size = std::fs::metadata(&file_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                photos.push(LivePhoto {
                    display_name: stem.replace(['_', '-'], " "),
                    name: stem.clone(),
                    image_path: file_path.to_string_lossy().into_owned(),
                    video_path: video_path.to_string_lossy().into_owned(),
                    file_size,
                });
                break;
            }
        }
    }
    photos.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(photos)
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn mime_for(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("mov") => "video/quicktime",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|_app| {
            start_file_server();
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            list_live_photos,
            get_server_port,
            extract_video_frames,
            open_with_system_player
        ])
        .run(tauri::generate_context!())
        .expect("error while running LivePhoto Viewer");
}
