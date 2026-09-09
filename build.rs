//! Build script: build the embedded web console frontend automatically.
//!
//! `webui/` is a Vite + Lit SPA whose `dist/` output is compiled into the
//! binary via rust-embed (release builds only — debug builds read from disk
//! at runtime). On every `cargo build` this script decides what to do:
//!
//! 1. Debug builds (`PROFILE=debug`) skip the frontend toolchain by default:
//!    no pnpm invocation, so Rust iteration stays fast. Whatever `dist/` is
//!    on disk gets used (rust-embed serves it live from disk in debug); if
//!    dist is missing a placeholder entry page is written so the server has
//!    something to serve. `XKEEPER_WEBUI_BUILD=force` opts back into the
//!    automatic build.
//! 2. Release builds (or debug + `force`) keep the bundle current: when any
//!    webui input (`src/`, configs, lockfile) is newer than the bundle — or
//!    the bundle is missing, or a previous run left the placeholder marker —
//!    run the frontend build: `pnpm install` when dependencies are missing
//!    or the lockfile changed, then `pnpm build`.
//! 3. When the Node toolchain is unavailable (or the build fails), fall back
//!    to a placeholder entry page so `cargo test` and docs builds stay green
//!    on machines without Node; a `cargo:warning` says so the first time.
//! 4. A stamp over the dist contents is emitted as a `rustc-env`, so a
//!    changed bundle recompiles the crate (and is re-embedded) even when no
//!    Rust source changed.
//!
//! Set `XKEEPER_WEBUI_BUILD=skip` to never invoke the frontend toolchain and
//! use whatever `dist/` is already on disk (used by CI jobs without a Node
//! setup that don't serve the real UI). The legacy `XKEEPER_FRONTEND_BUILD`
//! variable is still honored with lower precedence.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let webui = manifest.join("webui");
    let dist = webui.join("dist");

    // Watch exactly the webui inputs plus the output directory. Writing dist
    // does re-trigger this script (that is how a manually-run `pnpm build`
    // gets picked up), but the freshness check and the stamp keep those
    // reruns cheap and make the rebuild settle after one pass.
    for watched in [
        "webui/src",
        "webui/index.html",
        "webui/package.json",
        "webui/pnpm-lock.yaml",
        "webui/vite.config.ts",
        "webui/tsconfig.json",
        "webui/dist",
    ] {
        println!("cargo:rerun-if-changed={watched}");
    }

    let index = dist.join("index.html");
    let marker = dist.join(".xkeeper-placeholder");

    match build_mode() {
        BuildMode::Skip => {
            if !index.exists() {
                write_placeholder(&dist);
            }
            println!("cargo:rustc-env=XKEEPER_WEBUI_STAMP=skip");
            return;
        }
        BuildMode::Auto | BuildMode::Force => {}
    }

    let inputs = [
        webui.join("src"),
        webui.join("index.html"),
        webui.join("package.json"),
        webui.join("pnpm-lock.yaml"),
        webui.join("vite.config.ts"),
        webui.join("tsconfig.json"),
    ];
    let newest_input = newest_mtime(&inputs);
    let stale = marker.exists()
        || newest_input
            .is_none_or(|newest| !index.exists() || mtime(&index).is_some_and(|t| t < newest));

    if stale && !build_webui(&webui, &dist) && !index.exists() {
        // No usable bundle and no toolchain (or the build failed): embed a
        // placeholder page rather than failing the Rust build.
        write_placeholder(&dist);
        println!(
            "cargo:warning=webui bundle missing and Node toolchain unavailable \
             (pnpm/corepack not found) — embedded a placeholder page; install \
             Node >= 22 + pnpm and rebuild for the real UI"
        );
    }

    println!("cargo:rustc-env=XKEEPER_WEBUI_STAMP={}", stamp(&dist));
}

enum BuildMode {
    /// Never invoke the frontend toolchain; use disk as-is.
    Skip,
    /// Release behavior: build when inputs are newer than the bundle.
    Auto,
    /// Like Auto, but honored in debug builds too.
    Force,
}

/// Debug builds skip the frontend toolchain by default; `skip` forces that
/// off-switch everywhere and `force` opts debug builds into the automatic
/// build. The legacy `XKEEPER_FRONTEND_BUILD` variable is honored when the
/// new one is unset.
fn build_mode() -> BuildMode {
    let raw = std::env::var("XKEEPER_WEBUI_BUILD")
        .or_else(|_| std::env::var("XKEEPER_FRONTEND_BUILD"))
        .unwrap_or_default();
    match raw.as_str() {
        "skip" | "0" | "false" => BuildMode::Skip,
        "force" => BuildMode::Force,
        _ => {
            let debug = matches!(std::env::var("PROFILE").as_deref(), Ok("debug") | Err(_));
            if debug {
                BuildMode::Skip
            } else {
                BuildMode::Auto
            }
        }
    }
}

/// Run the frontend toolchain: install deps when needed, then build. The
/// output directory is wiped first so a failed build cannot leave a stale
/// bundle that looks fresh. Returns false on any failure (dist may be gone).
fn build_webui(webui: &Path, dist: &Path) -> bool {
    let Some(pnpm) = discover_pnpm() else {
        return false;
    };

    let lock = webui.join("pnpm-lock.yaml");
    let modules = webui.join("node_modules/.pnpm");
    let need_install = !modules.exists()
        || mtime(&lock).is_some_and(|lock_t| mtime(&modules).is_none_or(|m_t| lock_t > m_t));

    let _ = fs::remove_dir_all(dist);
    if need_install
        && !run(
            &pnpm,
            webui,
            &["install", "--frozen-lockfile"],
            "pnpm install",
        )
    {
        return false;
    }
    run(&pnpm, webui, &["build"], "pnpm build")
}

/// Locate pnpm: a `pnpm` on PATH first, then `corepack pnpm` (Node >= 22
/// ships corepack and package.json pins the version via `packageManager`).
fn discover_pnpm() -> Option<Vec<String>> {
    for prefix in [
        vec!["pnpm".to_string()],
        vec!["corepack".to_string(), "pnpm".to_string()],
    ] {
        let ok = Command::new(&prefix[0])
            .args(&prefix[1..])
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(prefix);
        }
    }
    None
}

fn run(cmd: &[String], dir: &Path, args: &[&str], what: &str) -> bool {
    let output = Command::new(&cmd[0])
        .args(&cmd[1..])
        .args(args)
        .current_dir(dir)
        .output();
    match output {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            eprintln!(
                "{what} failed:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            println!("cargo:warning={what} failed — see output above");
            false
        }
        Err(err) => {
            eprintln!("{what} could not start: {err}");
            println!("cargo:warning={what} failed — see output above");
            false
        }
    }
}

fn write_placeholder(dist: &Path) {
    fs::create_dir_all(dist).expect("create webui dist directory");
    fs::write(
        dist.join("index.html"),
        "<!doctype html><html><body><h1>xkeeper webui</h1><p>Frontend bundle not built \
         yet. Install Node >= 22 + pnpm and run <code>cargo build</code> — the \
         frontend is built automatically for release builds (debug: \
         <code>XKEEPER_WEBUI_BUILD=force</code> or <code>pnpm build</code>).</p>\
         </body></html>\n",
    )
    .expect("write placeholder index.html");
    fs::write(dist.join(".xkeeper-placeholder"), "placeholder\n")
        .expect("write placeholder marker");
}

/// Newest mtime across the given paths, recursing into directories.
fn newest_mtime(paths: &[PathBuf]) -> Option<SystemTime> {
    paths.iter().filter_map(|p| walk_mtime(p)).max()
}

fn walk_mtime(path: &Path) -> Option<SystemTime> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return meta.modified().ok();
    }
    let mut best = meta.modified().ok();
    for entry in fs::read_dir(path).ok()?.flatten() {
        best = best.max(walk_mtime(&entry.path()));
    }
    best
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A stable hash over the dist contents (path, size, mtime). Emitted as a
/// `rustc-env`: when the bundle changes, the build output changes, which
/// makes cargo recompile the crate so rust-embed re-reads it.
fn stamp(dist: &Path) -> String {
    let mut files: Vec<(String, u64, u128)> = Vec::new();
    collect(dist, dist, &mut files);
    files.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (path, len, mtime) in files {
        for byte in path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        for value in [u128::from(len), mtime] {
            for byte in value.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    format!("{hash:016x}")
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, u64, u128)>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let meta = entry.metadata();
            out.push((
                rel,
                meta.as_ref().map(|m| m.len()).unwrap_or(0),
                meta.ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            ));
        }
    }
}
