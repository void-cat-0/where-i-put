//! Runtime staging for the `rtsp` feature: copies the FFmpeg DLLs from
//! FFMPEG_DIR/bin next to the built exe (target/<profile>/), so the binary
//! is self-contained no matter how it's launched — `cargo run`, double-
//! clicked, or shipped as a folder. Windows loads DLLs from the exe's own
//! directory first, so no PATH games are needed.
//!
//! Skipped entirely without the rtsp feature or FFMPEG_DIR (default builds
//! and CI without the native toolchain stay clean). Copy is idempotent by
//! (size) equality to keep incremental builds cheap.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_RTSP");
    if env::var_os("CARGO_FEATURE_RTSP").is_none() {
        return;
    }
    let Some(ffmpeg_dir) = env::var_os("FFMPEG_DIR") else {
        println!(
            "cargo:warning=rtsp feature is on but FFMPEG_DIR is unset; \
             run `cargo xtask setup` and build via cargo so .cargo/config.toml applies"
        );
        return;
    };
    let bin = Path::new(&ffmpeg_dir).join("bin");
    let dlls: Vec<PathBuf> = match fs::read_dir(&bin) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")))
            .collect(),
        Err(e) => {
            println!("cargo:warning=reading {}: {e}", bin.display());
            return;
        }
    };
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out -> walk up to <profile>.
    let out_dir = env::var_os("OUT_DIR").map(PathBuf::from).expect("OUT_DIR");
    let profile_dir = out_dir
        .ancestors()
        .find(|p| matches!(p.file_name().and_then(|n| n.to_str()), Some("debug") | Some("release")))
        .expect("OUT_DIR under target/<profile>/");
    let mut copied = 0usize;
    for dll in &dlls {
        let dst = profile_dir.join(dll.file_name().unwrap());
        let fresh = dst.metadata().is_ok_and(|m| {
            m.len() == dll.metadata().map(|s| s.len()).unwrap_or(u64::MAX)
        });
        if fresh {
            continue;
        }
        match fs::copy(dll, &dst) {
            Ok(_) => copied += 1,
            Err(e) => println!(
                "cargo:warning=failed to stage {}: {e}",
                dll.file_name().unwrap().to_string_lossy()
            ),
        }
    }
    if copied > 0 {
        println!(
            "cargo:warning=staged {copied} FFmpeg DLL(s) next to the binary \
             (rtsp feature; re-runs only when they are missing or changed)"
        );
    }
}
