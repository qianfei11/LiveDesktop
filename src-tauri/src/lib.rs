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

/// Metadata returned by get_media_info (via ffprobe).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MediaInfo {
    pub vid_width: Option<u32>,
    pub vid_height: Option<u32>,
    pub img_width: Option<u32>,
    pub img_height: Option<u32>,
    pub duration_secs: Option<f64>,
    pub created_at: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
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
///
/// Strategy:
///   1. ffmpeg  — primary (supports HEVC/H.265, AV1, VP9, etc.)
///   2. gst-launch-1.0 — fallback (requires gstreamer + plugins)
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
    let pattern_str = pattern.to_string_lossy().into_owned();

    // ── Strategy 1: ffmpeg ────────────────────────────────────────────────────
    // Better codec coverage than GStreamer base install (HEVC, AV1, etc.).
    // -y          : overwrite output without prompting
    // fps=3       : output 3 frames per second of source material
    // -q:v 3      : JPEG quality (1=best … 31=worst; 3 ≈ high quality)
    // -start_number 0 : index frames from 0 (f00000.jpg, f00001.jpg, …)
    let ffmpeg = if std::path::Path::new("/usr/bin/ffmpeg").exists() {
        "/usr/bin/ffmpeg"
    } else {
        "ffmpeg"
    };

    if let Ok(_) = std::process::Command::new(ffmpeg)
        .args([
            "-y",
            "-i",
            &video_path,
            "-vf",
            "fps=3",
            "-q:v",
            "3",
            "-start_number",
            "0",
            &pattern_str,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        let frames = collect_frames(&cache, frame_count as usize);
        if !frames.is_empty() {
            return Ok(frames);
        }
    }

    // ── Strategy 2: gst-launch-1.0 fallback ──────────────────────────────────
    let loc_arg = format!("location={}", video_path);
    let sink_arg = format!("location={}", pattern_str);

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
        .map_err(|e| format!("No decoder available (ffmpeg/gst-launch-1.0 not found): {e}"))?;

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
// Single-file (embedded) formats — detection order:
//
// ① XMP APP1 scan (first 64 KB):
//
//   a) Google MicroVideo (old Pixel MVIMG_*.jpg, old Xiaomi)
//      ns: http://ns.google.com/photos/1.0/camera/
//      XMP: GCamera:MicroVideo="1", GCamera:MicroVideoOffset=N  (bytes from EOF)
//      video_start = file_size - N
//
//   b) Google MotionPhoto / Samsung (new Pixel PXL_*.MP.jpg, modern Samsung,
//      new Xiaomi HyperOS 3, vivo)
//      ns: http://ns.google.com/photos/1.0/camera/
//      XMP: GCamera:MotionPhoto="1"
//      Container: <Container:Item Item:Mime="video/mp4" Item:Length="N" Item:Padding="P"/>
//      video_start = file_size - N - P
//
//   c) OPPO / OnePlus / Realme (ColorOS / OxygenOS / realme UI)
//      Standard GCamera:MotionPhoto="1" + Container (same as above)
//      + proprietary ns: http://ns.oplus.com/photos/1.0/camera/
//        OLivePhotoVersion="2", MotionPhotoOwner="oplus"
//      Detected by Container approach; extra oplus namespace → photo_type="oppo"
//
// ② Samsung "MotionPhoto_Data" binary marker (Galaxy S7–S9)
//    ASCII "MotionPhoto_Data" (16 bytes) immediately precedes embedded MP4
//    video_start = offset_after_marker
//
// ③ Huawei / generic ftyp scan (Huawei/Honor pre-GMS-ban, vivo older)
//    No standard XMP offset — video is directly appended after JPEG data.
//    Detect by scanning for a valid ISO Base Media ftyp box (size 12-64,
//    known brand: mp41/mp42/isom/iso2/M4V etc.) from offset 4 KB onward.
//    video_start = ftyp_box_start  (4 bytes before the "ftyp" text)
//
// Separate-file (companion) formats handled in list_live_photos:
//   HEIC/JPG + .mov         → Apple Live Photo
//   HEIC/JPG + .mp4/.m4v   → Generic Android / Huawei pair
//   {stem}_motion.mp4       → Old Samsung
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
    if let Some(result) = find_samsung_marker(jpeg_path, file_size) {
        return Some(result);
    }

    // ── Strategy 3: generic ftyp box scan ────────────────────────────────────
    // Catches Huawei/Honor (older EMUI, HarmonyOS) and other manufacturers
    // that append an MP4 after the JPEG data without writing an XMP offset.
    // Also serves as a fallback when XMP said "MotionPhoto=1" but provided no
    // parseable Container:Item or MotionPhotoOffset (some older Huawei EMUI
    // versions adopted the GCamera MotionPhoto XMP flag but omitted the offset).
    find_ftyp_video_start(jpeg_path, file_size)
}

/// Parse XMP string for motion photo markers.
/// Returns (absolute_video_start, photo_type) or None.
fn parse_xmp_motion(xmp: &str, file_size: u64) -> Option<(u64, &'static str)> {
    // ── Google MicroVideo (old format) ────────────────────────────────────────
    // Used by: old Pixel (MVIMG_*.jpg), old Xiaomi (uses same GCamera namespace)
    let is_micro = xmp.contains("MicroVideo=\"1\"") || xmp.contains("MicroVideo='1'");
    if is_micro {
        if let Some(off) = extract_xmp_u64(xmp, "MicroVideoOffset") {
            // Xiaomi uses the same XMP fields as old Google — tag by namespace owner
            let src = if xmp.contains("xiaomi.com") { "xiaomi" } else { "google" };
            return Some((file_size.saturating_sub(off), src));
        }
    }

    // ── MotionPhoto (new Container format) ───────────────────────────────────
    // Used by: new Pixel, modern Samsung, Xiaomi HyperOS 3, vivo, OPPO
    let is_motion = xmp.contains("MotionPhoto=\"1\"") || xmp.contains("MotionPhoto='1'");
    if is_motion {
        // Identify source by checking vendor-specific namespace hints
        let source: &'static str = if xmp.contains("oplus.com") {
            // OPPO / OnePlus / Realme  — ColorOS / OxygenOS / realme UI
            // ns: http://ns.oplus.com/photos/1.0/camera/
            // attributes: OLivePhotoVersion, MotionPhotoOwner="oplus"
            "oppo"
        } else if xmp.contains("samsung.com") {
            "samsung"
        } else if xmp.contains("xiaomi.com") {
            "xiaomi"
        } else {
            "google"
        };

        // Container:Item — <Container:Item Item:Mime="video/mp4" Item:Length="N" Item:Padding="P"/>
        if let Some((length, padding)) = find_container_video_item(xmp) {
            let video_start = file_size.saturating_sub(length + padding);
            return Some((video_start, source));
        }

        // Direct offset attribute fallback
        if let Some(off) = extract_xmp_u64(xmp, "MotionPhotoOffset") {
            return Some((file_size.saturating_sub(off), source));
        }

        // XMP says it is a motion photo but has no parseable offset.
        // Signal the caller to fall through to binary ftyp scan (Huawei, etc.)
        // by returning None here — the outer function will try ftyp next.
    }

    None
}

/// Parse Container:Item elements from XMP to find the video/mp4 item's length and padding.
/// Returns (Item:Length, Item:Padding).
fn find_container_video_item(xmp: &str) -> Option<(u64, u64)> {
    // Walk every <Container:Item …/> tag and return the length/padding of the
    // first one whose Mime attribute contains "video/mp4".
    //
    // The naive window-search approach was wrong: a preceding image Container:Item
    //   <Container:Item Item:Mime="image/jpeg" … Item:Length="0" …/>
    // could fall inside the backward window and poison the length result.
    let mut rest = xmp;
    while let Some(tag_start) = rest.find("<Container:Item") {
        let tag_body = &rest[tag_start..];
        // Self-closing tags end with "/>"; fall back to ">" for malformed XMP.
        let tag_end = tag_body
            .find("/>")
            .map(|p| p + 2)
            .or_else(|| tag_body.find(">").map(|p| p + 1))?;
        let tag = &tag_body[..tag_end];
        if tag.contains("video/mp4") {
            let length = extract_xmp_u64(tag, "Item:Length")?;
            let padding = extract_xmp_u64(tag, "Item:Padding").unwrap_or(0);
            return Some((length, padding));
        }
        rest = &tag_body[tag_end..];
    }
    None
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

/// Scan the file for a valid ISO Base Media ftyp box (MP4/MOV container header).
///
/// Used as a last-resort fallback for manufacturers that append video directly
/// after the JPEG data without writing standard XMP offset metadata.
/// Primary targets: Huawei/Honor (pre-GMS and HarmonyOS older devices).
///
/// Scans from 4 KB (past JPEG APP marker area) up to 20 MB.
/// Returns the absolute offset where the ftyp box starts (video_start).
fn find_ftyp_video_start(path: &Path, file_size: u64) -> Option<(u64, &'static str)> {
    // A phone camera JPEG is always several hundred KB; skip small files.
    if file_size < 64 * 1024 {
        return None;
    }

    let scan_size = file_size.min(20 * 1024 * 1024) as usize;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; scan_size];
    let n = f.read(&mut buf).ok()?;
    let buf = &buf[..n];

    // Skip the first 4 KB to avoid false positives inside JPEG APP segments.
    // JPEG compressed image data (after SOS) can be large, so the embedded
    // video realistically starts well past the header.
    let search_start = 4096usize;

    for i in search_start..buf.len().saturating_sub(8) {
        if &buf[i..i + 4] != b"ftyp" {
            continue;
        }
        // The 4 bytes before "ftyp" encode the box size (big-endian u32).
        // A valid ftyp box is 12–64 bytes (type + version + compatible brands).
        if i < 4 {
            continue;
        }
        let box_size =
            u32::from_be_bytes([buf[i - 4], buf[i - 3], buf[i - 2], buf[i - 1]]) as usize;
        if box_size < 12 || box_size > 64 {
            continue;
        }
        // The 4 bytes immediately after "ftyp" are the major brand.
        let brand = &buf[i + 4..i + 8];
        if !is_known_mp4_brand(brand) {
            continue;
        }
        // Require the box to be reasonably deep into the file — at least 32 KB
        // — to avoid matching a thumbnail JPEG embedded within EXIF.
        let box_start = (i - 4) as u64;
        if box_start < 32 * 1024 {
            continue;
        }
        return Some((box_start, "huawei"));
    }
    None
}

/// Return true if the 4-byte slice matches a known ISO Base Media ftyp major brand.
fn is_known_mp4_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"mp41" | b"mp42" | b"isom" | b"iso2" | b"iso4" | b"iso5" | b"iso6"
            | b"M4V " | b"M4A " | b"avc1" | b"qt  " | b"MSNV" | b"f4v " | b"3gp4"
            | b"3gp5" | b"3gp6" | b"mmp4" | b"hvc1" | b"HEVC" | b"heic" | b"mif1"
    )
}

/// Cache path for the extracted embedded video for a given JPEG.
fn embedded_video_cache_path(jpeg_path: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("livephoto_{:016x}", path_hash(jpeg_path)))
        .join("embedded.mp4")
}

/// Extract the embedded video from a motion photo JPEG to a temp cache file.
/// Returns the absolute path to the extracted MP4.
///
/// Cache validity rules:
///   - A 0-byte cached file means a previous extraction failed — re-extract.
///   - If the source JPEG is newer than the cached file, re-extract (handles
///     test-file regeneration and in-place photo edits).
fn extract_embedded_video(jpeg_path: &str, video_start: u64) -> Result<String, String> {
    let dst_path = embedded_video_cache_path(jpeg_path);

    if dst_path.exists() {
        let dst_size = std::fs::metadata(&dst_path).map(|m| m.len()).unwrap_or(0);
        let src_newer = {
            let src_mt = std::fs::metadata(jpeg_path).and_then(|m| m.modified()).ok();
            let dst_mt = std::fs::metadata(&dst_path).and_then(|m| m.modified()).ok();
            match (src_mt, dst_mt) {
                (Some(s), Some(d)) => s > d,
                _ => false,
            }
        };
        if dst_size > 0 && !src_newer {
            return Ok(dst_path.to_string_lossy().into_owned());
        }
        // Stale or empty cache — remove and re-extract below
        let _ = std::fs::remove_file(&dst_path);
    }

    std::fs::create_dir_all(dst_path.parent().unwrap()).map_err(|e| e.to_string())?;

    let file_size = std::fs::metadata(jpeg_path)
        .map_err(|e| e.to_string())?
        .len();
    let video_size = file_size.saturating_sub(video_start);

    if video_size == 0 {
        return Err(format!(
            "invalid video offset {video_start} >= file size {file_size}"
        ));
    }

    let mut src = std::fs::File::open(jpeg_path).map_err(|e| e.to_string())?;
    src.seek(SeekFrom::Start(video_start))
        .map_err(|e| e.to_string())?;

    let mut dst = std::fs::File::create(&dst_path).map_err(|e| e.to_string())?;
    if let Err(e) = stream_bytes(&mut src, &mut dst, video_size) {
        // Remove the partial file so it is not permanently cached as valid
        let _ = std::fs::remove_file(&dst_path);
        return Err(e.to_string());
    }

    Ok(dst_path.to_string_lossy().into_owned())
}

// ─────────────────────────────────────────────────────────────────────────────
// Live photo scanner
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Feature 5 — Live photo scanner (with optional recursive traversal)
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
fn list_live_photos(dir: String, recursive: bool) -> Result<Vec<LivePhoto>, String> {
    let path = Path::new(&dir);
    if !path.is_dir() {
        return Err(format!("'{dir}' is not a valid directory"));
    }

    let mut photos = Vec::new();

    for file_path in collect_image_files(path, recursive) {
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

        // ── 1a. Apple Live Photo: HEIC/JPEG + MOV (separate files) ───────────
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

        // ── 1b. Huawei/Honor HEIC + MP4 (separate files) ─────────────────────
        // Huawei devices save HEIC images; their motion video is an .mp4 (not
        // .mov). Also covers Honor, which shares the same camera software.
        // We tag these specifically so they display with the right badge.
        if matches!(ext_lower.as_str(), "heic" | "heif") {
            let huawei_found = ["mp4", "MP4"].iter().any(|ve| {
                let vp = file_path.with_extension(ve);
                if vp.exists() {
                    photos.push(LivePhoto {
                        display_name: stem.replace(['_', '-'], " "),
                        name: stem.clone(),
                        image_path: image_path.clone(),
                        video_path: vp.to_string_lossy().into_owned(),
                        file_size,
                        photo_type: "huawei".to_string(),
                    });
                    true
                } else {
                    false
                }
            });
            if huawei_found {
                continue;
            }
        }

        // ── 2. Generic Android pair: JPG/PNG + MP4/M4V (separate files) ──────
        // Catches: Xiaomi, OnePlus, Huawei (JPG+MP4), OPPO, vivo, and other
        // manufacturers that write a companion MP4 with the same stem.
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

/// Cache path for the JPEG thumbnail of a HEIC image.
fn thumbnail_cache_path(image_path: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("livephoto_{:016x}", path_hash(image_path)))
        .join("thumbnail.jpg")
}

/// Collect all image file paths under `dir`, optionally recursing into sub-directories.
fn collect_image_files(dir: &Path, recursive: bool) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && recursive {
            out.extend(collect_image_files(&path, true));
        } else if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if matches!(ext.as_str(), "jpg" | "jpeg" | "heic" | "heif" | "png") {
                out.push(path);
            }
        }
    }
    out
}

fn ffmpeg_bin() -> &'static str {
    if std::path::Path::new("/usr/bin/ffmpeg").exists() {
        "/usr/bin/ffmpeg"
    } else {
        "ffmpeg"
    }
}

fn ffprobe_bin() -> &'static str {
    if std::path::Path::new("/usr/bin/ffprobe").exists() {
        "/usr/bin/ffprobe"
    } else {
        "ffprobe"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Feature 2 — HEIC thumbnail generation
// ─────────────────────────────────────────────────────────────────────────────

/// Convert a HEIC/HEIF image to a cached JPEG thumbnail via ffmpeg.
/// Returns the original path unchanged for JPEG/PNG (browser-native formats).
#[tauri::command]
fn get_thumbnail(image_path: String) -> Result<String, String> {
    let ext = Path::new(&image_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if !matches!(ext.as_str(), "heic" | "heif") {
        return Ok(image_path);
    }

    let dst = thumbnail_cache_path(&image_path);
    if dst.exists() && std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0) > 0 {
        return Ok(dst.to_string_lossy().into_owned());
    }

    std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e| e.to_string())?;

    std::process::Command::new(ffmpeg_bin())
        .args([
            "-y",
            "-i",
            &image_path,
            "-vframes",
            "1",
            "-q:v",
            "3",
            dst.to_str().unwrap_or(""),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;

    if dst.exists() && std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0) > 0 {
        Ok(dst.to_string_lossy().into_owned())
    } else {
        Err(format!("HEIC thumbnail conversion failed for '{image_path}'"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Feature 3 — Export embedded video
// ─────────────────────────────────────────────────────────────────────────────

/// Copy the video file to a user-chosen destination path.
#[tauri::command]
fn save_video(src: String, dst: String) -> Result<(), String> {
    std::fs::copy(&src, &dst)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Feature 4 — EXIF / media metadata via ffprobe
// ─────────────────────────────────────────────────────────────────────────────

fn ffprobe_json(path: &str) -> Option<serde_json::Value> {
    let out = std::process::Command::new(ffprobe_bin())
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            path,
        ])
        .output()
        .ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

fn first_tag<'a>(tags: &'a serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|&k| tags[k].as_str())
        .map(String::from)
}

/// Return media metadata for the given image and video files.
#[tauri::command]
fn get_media_info(image_path: String, video_path: String) -> Result<MediaInfo, String> {
    let mut info = MediaInfo::default();

    // ── Video: dimensions, duration, tags ────────────────────────────────────
    if !video_path.is_empty() {
        if let Some(json) = ffprobe_json(&video_path) {
            if let Some(streams) = json["streams"].as_array() {
                if let Some(vs) = streams
                    .iter()
                    .find(|s| s["codec_type"].as_str() == Some("video"))
                {
                    info.vid_width = vs["width"].as_u64().map(|v| v as u32);
                    info.vid_height = vs["height"].as_u64().map(|v| v as u32);
                    // Some containers store duration per-stream
                    if info.duration_secs.is_none() {
                        info.duration_secs = vs["duration"]
                            .as_str()
                            .and_then(|d| d.parse::<f64>().ok());
                    }
                }
            }
            info.duration_secs = info.duration_secs.or_else(|| {
                json["format"]["duration"]
                    .as_str()
                    .and_then(|d| d.parse::<f64>().ok())
            });
            let tags = &json["format"]["tags"];
            info.created_at = first_tag(
                tags,
                &[
                    "creation_time",
                    "date_time_original",
                    "DateTimeOriginal",
                    "com.apple.quicktime.creationdate",
                ],
            );
            info.make = first_tag(
                tags,
                &["com.apple.quicktime.make", "make", "Make"],
            );
            info.model = first_tag(
                tags,
                &["com.apple.quicktime.model", "model", "Model"],
            );
        }
    }

    // ── Image: dimensions + fill missing EXIF tags ────────────────────────────
    if !image_path.is_empty() {
        if let Some(json) = ffprobe_json(&image_path) {
            if let Some(streams) = json["streams"].as_array() {
                if let Some(vs) = streams
                    .iter()
                    .find(|s| s["codec_type"].as_str() == Some("video"))
                {
                    info.img_width = vs["width"].as_u64().map(|v| v as u32);
                    info.img_height = vs["height"].as_u64().map(|v| v as u32);
                }
            }
            let tags = &json["format"]["tags"];
            if info.created_at.is_none() {
                info.created_at =
                    first_tag(tags, &["creation_time", "DateTimeOriginal", "date"]);
            }
            if info.make.is_none() {
                info.make = first_tag(tags, &["make", "Make"]);
            }
            if info.model.is_none() {
                info.model = first_tag(tags, &["model", "Model"]);
            }
        }
    }

    Ok(info)
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
            open_with_system_player,
            get_thumbnail,
            save_video,
            get_media_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running LivePhoto Viewer");
}
