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

/// photo_type values:
///   "live"    — Apple Live Photo (HEIC/JPG + MOV separate files)
///   "google"  — Google Motion Photo (embedded MP4, MicroVideo or MotionPhoto XMP)
///   "samsung" — Samsung Motion Photo (embedded MP4 via Container XMP or MotionPhoto_Data marker)
///   "motion"  — Generic Android pair (JPG + MP4/M4V, separate files, e.g. Xiaomi/OnePlus/older Samsung)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LivePhoto {
    pub name: String,
    pub display_name: String,
    pub image_path: String,
    pub video_path: String,
    pub file_size: u64,
    pub photo_type: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Local HTTP file server
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
// Embedded motion photo detection and extraction
//
// Supports three single-file formats:
//
// 1. Google MicroVideo (old Pixel, MVIMG_*.jpg)
//    XMP: GCamera:MicroVideo="1", GCamera:MicroVideoOffset=N (from EOF)
//    Video starts at: file_size - N
//
// 2. Google/Samsung MotionPhoto (new Pixel PXL_*.MP.jpg, modern Samsung)
//    XMP: GCamera:MotionPhoto="1" or Samsung:MotionPhoto="1"
//    Container XMP: <Container:Item Item:Mime="video/mp4" Item:Length="N" Item:Padding="P"/>
//    Video starts at: file_size - N - P
//
// 3. Samsung older (Galaxy S7/S8/S9, MotionPhoto_Data marker)
//    Binary marker "MotionPhoto_Data" (16 bytes) appended after JPEG FF D9
//    Video starts immediately after the 16-byte marker
// ─────────────────────────────────────────────────────────────────────────────

/// Try to detect an embedded video in a JPEG file.
/// Returns (absolute_video_start_offset, photo_type_str) on success.
fn jpeg_motion_photo_offset(jpeg_path: &Path) -> Option<(u64, &'static str)> {
    let file_size = std::fs::metadata(jpeg_path).ok()?.len();
    if file_size < 4 {
        return None;
    }

    // ── Strategy 1: scan JPEG APP1 XMP marker (first 64 KB) ─────────────────
    let scan_size = file_size.min(65536) as usize;
    let mut buf = vec![0u8; scan_size];
    let mut f = std::fs::File::open(jpeg_path).ok()?;
    let n = f.read(&mut buf).ok()?;
    let buf = &buf[..n];

    // Verify JPEG magic
    if buf.len() < 2 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return None;
    }

    let mut pos = 2usize;
    while pos + 3 < buf.len() {
        if buf[pos] != 0xFF {
            break;
        }
        let marker = buf[pos + 1];
        // Stop at SOS (compressed data begins) or EOI
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        // Skip standalone markers (no length field)
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) {
            pos += 2;
            continue;
        }
        if pos + 3 >= buf.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        if seg_len < 2 {
            break;
        }
        let data_start = pos + 4;
        let data_end = (pos + 2 + seg_len).min(buf.len());

        // APP1 marker (0xE1) may contain XMP
        if marker == 0xE1 && data_start < data_end {
            const XMP_SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
            let seg = &buf[data_start..data_end];
            if seg.len() > XMP_SIG.len() && seg.starts_with(XMP_SIG) {
                let xmp = std::str::from_utf8(&seg[XMP_SIG.len()..]).unwrap_or("");
                if let Some(result) = parse_xmp_motion(xmp, file_size) {
                    return Some(result);
                }
            }
        }

        pos = pos + 2 + seg_len;
    }

    // ── Strategy 2: binary scan for Samsung "MotionPhoto_Data" marker ────────
    find_samsung_marker(jpeg_path, file_size)
}

/// Parse XMP string for motion photo markers.
/// Returns (absolute_video_start, photo_type) or None.
fn parse_xmp_motion(xmp: &str, file_size: u64) -> Option<(u64, &'static str)> {
    // ── Google MicroVideo (old format) ────────────────────────────────────────
    // XMP: GCamera:MicroVideo="1", GCamera:MicroVideoOffset=N (bytes from EOF)
    let is_micro = xmp.contains("MicroVideo=\"1\"") || xmp.contains("MicroVideo='1'");
    if is_micro {
        if let Some(off) = extract_xmp_u64(xmp, "MicroVideoOffset") {
            return Some((file_size.saturating_sub(off), "google"));
        }
    }

    // ── Google MotionPhoto / Samsung MotionPhoto (new Container format) ───────
    // XMP: GCamera:MotionPhoto="1" or Samsung:MotionPhoto="1"
    // Container: <Container:Item Item:Mime="video/mp4" Item:Length="N" Item:Padding="P"/>
    let is_motion = xmp.contains("MotionPhoto=\"1\"") || xmp.contains("MotionPhoto='1'");
    if is_motion {
        let source: &'static str = if xmp.contains("samsung.com") {
            "samsung"
        } else {
            "google"
        };

        // Try Container:Item approach (length of video item from EOF)
        if let Some((length, padding)) = find_container_video_item(xmp) {
            let video_start = file_size.saturating_sub(length + padding);
            return Some((video_start, source));
        }

        // Fallback: try MotionPhotoOffset attribute directly
        if let Some(off) = extract_xmp_u64(xmp, "MotionPhotoOffset") {
            return Some((file_size.saturating_sub(off), source));
        }
    }

    None
}

/// Parse Container:Item elements from XMP to find the video/mp4 item's length and padding.
/// Returns (Item:Length, Item:Padding).
fn find_container_video_item(xmp: &str) -> Option<(u64, u64)> {
    // Find the position of "video/mp4" mime type in a Container:Item element.
    // XMP looks like:
    //   <Container:Item Item:Mime="video/mp4" Item:Semantic="MotionPhoto"
    //                   Item:Length="2929880" Item:Padding="0"/>
    let mp4_pos = xmp.find("video/mp4")?;

    // Search for Item:Length in a window around the "video/mp4" hit.
    // The attributes may appear before or after the mime, so use a ±500 char window.
    let win_start = mp4_pos.saturating_sub(300);
    let win_end = (mp4_pos + 400).min(xmp.len());
    let window = &xmp[win_start..win_end];

    let length = extract_xmp_u64(window, "Item:Length")?;
    let padding = extract_xmp_u64(window, "Item:Padding").unwrap_or(0);
    Some((length, padding))
}

/// Extract a u64 value from an XMP attribute like `AttrName="12345"` or `AttrName='12345'`.
fn extract_xmp_u64(xmp: &str, attr: &str) -> Option<u64> {
    for &q in &['"', '\''] {
        let pattern = format!("{attr}={q}");
        if let Some(pos) = xmp.find(&pattern) {
            let after = &xmp[pos + pattern.len()..];
            if let Some(end) = after.find(q) {
                if let Ok(val) = after[..end].trim().parse::<u64>() {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// Scan file for Samsung's binary `MotionPhoto_Data` marker.
/// The 16-byte ASCII marker is written immediately before the embedded MP4.
/// Returns the absolute offset where the MP4 data begins (right after the marker).
fn find_samsung_marker(path: &Path, file_size: u64) -> Option<(u64, &'static str)> {
    const MARKER: &[u8] = b"MotionPhoto_Data";
    // Scan up to 20 MB (covers typical JPEG + Samsung header area)
    let scan_size = file_size.min(20 * 1024 * 1024) as usize;

    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; scan_size];
    let n = f.read(&mut buf).ok()?;
    let buf = &buf[..n];

    // Simple linear scan — fast enough for ≤20 MB in Rust
    for i in 0..buf.len().saturating_sub(MARKER.len()) {
        if buf[i..].starts_with(MARKER) {
            let video_start = (i + MARKER.len()) as u64;
            // Sanity check: make sure we found real MP4 data (ftyp or mdat box)
            let after = &buf[i + MARKER.len()..];
            if after.len() >= 8 && (&after[4..8] == b"ftyp" || &after[4..8] == b"mdat") {
                return Some((video_start, "samsung"));
            }
        }
    }
    None
}

/// Cache path for the extracted embedded video for a given JPEG.
fn embedded_video_cache_path(jpeg_path: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("livephoto_{:016x}", path_hash(jpeg_path)))
        .join("embedded.mp4")
}

/// Extract the embedded video from a motion photo JPEG to a temp cache file.
/// Returns the absolute path to the extracted MP4.
fn extract_embedded_video(jpeg_path: &str, video_start: u64) -> Result<String, String> {
    let dst_path = embedded_video_cache_path(jpeg_path);

    if dst_path.exists() {
        return Ok(dst_path.to_string_lossy().into_owned());
    }

    std::fs::create_dir_all(dst_path.parent().unwrap()).map_err(|e| e.to_string())?;

    let file_size = std::fs::metadata(jpeg_path)
        .map_err(|e| e.to_string())?
        .len();
    let video_size = file_size.saturating_sub(video_start);

    let mut src = std::fs::File::open(jpeg_path).map_err(|e| e.to_string())?;
    src.seek(SeekFrom::Start(video_start))
        .map_err(|e| e.to_string())?;

    let mut dst = std::fs::File::create(&dst_path).map_err(|e| e.to_string())?;
    stream_bytes(&mut src, &mut dst, video_size).map_err(|e| e.to_string())?;

    Ok(dst_path.to_string_lossy().into_owned())
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
        let file_size = std::fs::metadata(&file_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let image_path = file_path.to_string_lossy().into_owned();

        // ── 1. Apple Live Photo: HEIC/JPEG + MOV (separate files) ────────────
        let apple_found = ["mov", "MOV"].iter().any(|ve| {
            let vp = file_path.with_extension(ve);
            if vp.exists() {
                photos.push(LivePhoto {
                    display_name: stem.replace(['_', '-'], " "),
                    name: stem.clone(),
                    image_path: image_path.clone(),
                    video_path: vp.to_string_lossy().into_owned(),
                    file_size,
                    photo_type: "live".to_string(),
                });
                true
            } else {
                false
            }
        });
        if apple_found {
            continue;
        }

        // ── 2. Generic Android pair: JPG + MP4/M4V (separate files) ──────────
        // Catches: Xiaomi, OnePlus, Huawei, and other manufacturers that write
        // a companion MP4 with the same stem.
        let pair_found = ["mp4", "MP4", "m4v", "M4V"].iter().any(|ve| {
            let vp = file_path.with_extension(ve);
            if vp.exists() {
                photos.push(LivePhoto {
                    display_name: stem.replace(['_', '-'], " "),
                    name: stem.clone(),
                    image_path: image_path.clone(),
                    video_path: vp.to_string_lossy().into_owned(),
                    file_size,
                    photo_type: "motion".to_string(),
                });
                true
            } else {
                false
            }
        });
        if pair_found {
            continue;
        }

        // ── 3. Samsung older separate-file pair: {stem}_motion.mp4 ───────────
        // Galaxy S7/S8 era: companion file named "{stem}_motion.mp4"
        let samsung_pair_found = ["_motion.mp4", "_motion.MP4"].iter().any(|suffix| {
            let vp = file_path.with_file_name(format!("{stem}{suffix}"));
            if vp.exists() {
                photos.push(LivePhoto {
                    display_name: stem.replace(['_', '-'], " "),
                    name: stem.clone(),
                    image_path: image_path.clone(),
                    video_path: vp.to_string_lossy().into_owned(),
                    file_size,
                    photo_type: "samsung".to_string(),
                });
                true
            } else {
                false
            }
        });
        if samsung_pair_found {
            continue;
        }

        // ── 4. Embedded motion photo (Google Pixel & modern Samsung) ──────────
        // Only JPEG files can embed a video this way; HEIC/PNG cannot.
        if matches!(ext_lower.as_str(), "jpg" | "jpeg") {
            if let Some((video_start, ptype)) = jpeg_motion_photo_offset(&file_path) {
                match extract_embedded_video(&image_path, video_start) {
                    Ok(video_path) => {
                        photos.push(LivePhoto {
                            display_name: stem.replace(['_', '-'], " "),
                            name: stem.clone(),
                            image_path,
                            video_path,
                            file_size,
                            photo_type: ptype.to_string(),
                        });
                    }
                    Err(e) => {
                        eprintln!("[motion-photo] failed to extract '{stem}': {e}");
                    }
                }
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
