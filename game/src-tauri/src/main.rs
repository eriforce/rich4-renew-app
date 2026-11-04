/**
 * 大富翁4 桌面壳（Windows 便携版），desktop/main.cjs 的 Tauri 版：
 * 游戏从自己的 rich4:// 网址载入（Windows 的 WebView2 里是 http://rich4.localhost/），每个请求
 * 先看在線更新的 overlay 文件夹（docs/rules/29，update.ts），再回 dist/；素材在 exe 旁的 assets/。
 * 存档照旧在 WebView 的 IndexedDB 里，派对模式的 WebRTC 照常（和 Chrome 一样）。
 *
 * 便携版目录：Rich4.exe + dist\（网页构建，几 MB）+ assets\（玩家自解 rich4-assets.tar）。
 */
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use percent_encoding::percent_decode_str;
use tauri::{Manager, State, WebviewUrl, WebviewWindowBuilder};

/// The shell's own folders (desktop/main.cjs 的 DIST / rich4-update / assets).
struct Shell {
    dist: PathBuf,
    assets: PathBuf,
    update: PathBuf,
    overlay: Mutex<Option<PathBuf>>,
}

// ---------------------------------------------------------------- 在線更新 overlay（docs/rules/29）

/** Reads and trims a small text file, None when missing (uRead). */
fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn write_trim(p: &Path, s: &str) {
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = fs::write(p, s);
}

/** "0.9.10" > "0.9.9" (ui/update.ts newer, the same longer-version quirk). */
fn newer(a: &str, b: &str) -> bool {
    let av: Vec<u64> = a.split('.').map(|x| x.parse().unwrap_or(0)).collect();
    let bv: Vec<u64> = b.split('.').map(|x| x.parse().unwrap_or(0)).collect();
    for i in 0..av.len().max(bv.len()) {
        let (p, q) = (av.get(i).copied().unwrap_or(0), bv.get(i).copied().unwrap_or(0));
        if p != q {
            return p > q;
        }
    }
    false
}

/** ^[0-9]+(\.[0-9]+){1,3}$ */
fn version_ok(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    (2..=4).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/** Which downloaded version to start with (main.cjs chooseOverlay): three failed starts drop it. */
fn choose_overlay(update: &Path, dist: &Path) -> Option<PathBuf> {
    let ver = match read_trim(&update.join("current")) {
        Some(v) if version_ok(&v) => v,
        _ => return None,
    };
    let dir = update.join(format!("v{ver}"));
    if !dir.join("index.html").is_file() {
        return None;
    }
    // The dist's own version (update-base.json): a download older than the build is dropped.
    let own = read_trim(&dist.join("update-base.json"))
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|m| m.get("version").and_then(|v| v.as_str()).map(str::to_owned))
        .unwrap_or_default();
    if own.is_empty() || !newer(&ver, &own) {
        let _ = fs::remove_file(update.join("current"));
        return None;
    }
    let tries = read_trim(&update.join("boot"))
        .filter(|b| b.split(' ').next() == Some(ver.as_str()))
        .and_then(|b| b.split(' ').nth(1).and_then(|n| n.parse::<u32>().ok()))
        .unwrap_or(0);
    if tries >= 3 {
        let _ = fs::remove_file(update.join("current"));
        let _ = fs::remove_file(update.join("boot"));
        write_trim(&update.join("bad"), &ver);
        return None;
    }
    write_trim(&update.join("boot"), &format!("{ver} {}", tries + 1));
    Some(dir)
}

/** A name inside rich4-update, no way out of it (main.cjs inside). */
fn inside(root: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.split(['/', '\\']).any(|c| c == "..") {
        return None;
    }
    let f = root.join(name);
    f.starts_with(root).then_some(f)
}

// ---------------------------------------------------------------- the page's update store (preload.cjs)

#[tauri::command]
fn update_read(state: State<'_, Shell>, name: String) -> Option<String> {
    let f = inside(&state.update, &name)?;
    fs::read(f).ok().map(|b| B64.encode(b))
}

#[tauri::command]
fn update_write(state: State<'_, Shell>, name: String, b64: String) -> bool {
    let Some(f) = inside(&state.update, &name) else { return false };
    let Ok(bytes) = B64.decode(b64) else { return false };
    if let Some(p) = f.parent() {
        let _ = fs::create_dir_all(p);
    }
    fs::write(&f, bytes).is_ok()
}

#[tauri::command]
fn update_remove(state: State<'_, Shell>, name: String) -> bool {
    if let Some(f) = inside(&state.update, &name) {
        if f.is_dir() {
            let _ = fs::remove_dir_all(f);
        } else {
            let _ = fs::remove_file(f);
        }
    }
    true
}

#[tauri::command]
fn update_rename(state: State<'_, Shell>, r#from: String, to: String) -> bool {
    let (Some(a), Some(b)) = (inside(&state.update, &r#from), inside(&state.update, &to)) else { return false };
    if b.is_dir() {
        let _ = fs::remove_dir_all(&b);
    } else {
        let _ = fs::remove_file(&b);
    }
    fs::rename(a, b).is_ok()
}

#[tauri::command]
fn update_list(state: State<'_, Shell>) -> Vec<String> {
    fs::read_dir(&state.update)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- rich4:// = overlay first, then dist / assets

fn mime_of(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webp" => "image/webp",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn resolve(request: &tauri::http::Request<Vec<u8>>, shell: &Shell) -> (u16, &'static str, Vec<u8>) {
    let path = percent_decode_str(request.uri().path()).decode_utf8_lossy();
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.split(['/', '\\']).any(|c| c == "..") {
        return (403, "text/plain", b"forbidden".to_vec());
    }
    // The overlay first (every request, like main.cjs), then the assets folder, then dist.
    let overlay = shell.overlay.lock().unwrap().clone();
    if let Some(dir) = overlay {
        let f = dir.join(rel);
        if f.is_file() {
            return fs::read(&f).map(|b| (200, mime_of(rel), b)).unwrap_or((404, "text/plain", b"not found".to_vec()));
        }
    }
    let file = match rel.strip_prefix("assets/") {
        Some(rest) => shell.assets.join(rest),
        None => shell.dist.join(rel),
    };
    match fs::read(&file) {
        Ok(b) => (200, mime_of(rel), b),
        Err(_) => (404, "text/plain", b"not found".to_vec()),
    }
}

// ---------------------------------------------------------------- the window

/** The page's update store (preload.cjs rich4Desktop), backed by the commands above. */
const SHIM: &str = r#"(function () {
  function make(invoke) {
    const b64 = function (u) { let s = ''; for (let i = 0; i < u.length; i += 0x8000) s += String.fromCharCode.apply(null, u.subarray(i, i + 0x8000)); return btoa(s); };
    const unb64 = function (s) { return Uint8Array.from(atob(s), function (c) { return c.charCodeAt(0); }); };
    return { update: {
      read: async function (n) { const r = await invoke('update_read', { name: n }); return r == null ? null : unb64(r); },
      write: async function (n, d) { const b = typeof d === 'string' ? new TextEncoder().encode(d) : d; await invoke('update_write', { name: n, b64: b64(b) }); },
      remove: async function (n) { await invoke('update_remove', { name: n }); },
      rename: async function (a, b2) { await invoke('update_rename', { from: a, to: b2 }); },
      list: function () { return invoke('update_list'); },
    } };
  }
  if (window.__TAURI_INTERNALS__) window.rich4Desktop = make(window.__TAURI_INTERNALS__.invoke);
  else { var t = setInterval(function () { if (window.__TAURI_INTERNALS__) { clearInterval(t); window.rich4Desktop = make(window.__TAURI_INTERNALS__.invoke); } }, 0); }
})();"#;

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![update_read, update_write, update_remove, update_rename, update_list])
        .register_uri_scheme_protocol("rich4", |ctx, request| {
            let (code, mime, body) = match ctx.app_handle().try_state::<Shell>() {
                Some(s) => resolve(&request, &s),
                None => (404, "text/plain", b"not ready".to_vec()),
            };
            tauri::http::Response::builder()
                .status(code)
                .header("Content-Type", mime)
                .body(Cow::Owned(body))
                .map_err(Into::into)
        })
        .setup(|app| {
            // dist and assets sit next to the exe (portable); RICH4_DIST / RICH4_ASSETS override (dev).
            let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
            let dist = std::env::var("RICH4_DIST").map(PathBuf::from).ok().or_else(|| {
                exe_dir.as_ref().map(|d| d.join("dist")).filter(|p| p.join("index.html").is_file())
            }).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dist"));
            let assets = std::env::var("RICH4_ASSETS").map(PathBuf::from).ok().or_else(|| {
                exe_dir.as_ref().map(|d| d.join("assets")).filter(|p| p.is_dir())
            }).unwrap_or_else(|| dist.parent().unwrap_or(Path::new(".")).join("public/assets"));
            let update = app.path().app_data_dir()?.join("rich4-update");
            let overlay = choose_overlay(&update, &dist);
            app.manage(Shell { dist, assets, update, overlay: Mutex::new(overlay) });

            // Windows' WebView2 carries custom schemes as http://<scheme>.localhost; elsewhere the scheme itself.
            let url: tauri::Url = if cfg!(windows) {
                "http://rich4.localhost/index.html"
            } else {
                "rich4://localhost/index.html"
            }
            .parse()?;
            let mut win = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("大富翁4")
                .inner_size(1280.0, 800.0)
                .min_inner_size(800.0, 500.0)
                .initialization_script(SHIM);
            #[cfg(windows)]
            {
                win = win.additional_browser_args(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required",
                );
            }
            win.build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("大富翁4 桌面壳启动失败");
}
