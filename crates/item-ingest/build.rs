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
            // On Linux, source builds put .so files in lib/, not bin/.
            // Emit rpath so the binary finds them at runtime.
            #[cfg(target_family = "unix")]
            {
                let lib = Path::new(&ffmpeg_dir).join("lib");
                if lib.is_dir() {
                    println!("cargo:rustc-link-search=native={}", lib.display());
                    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
                }
            }
            return;
        }
    };
    // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out -- and, on toolchains
    // with the newer build-directory layout, target/<profile>/build/<pkg>/
    // <hash>/out. The profile directory is therefore whichever ancestor cargo
    // names after PROFILE, not a fixed number of hops up (a custom
    // `--profile dist` lands there too). If it cannot be found, skip the
    // staging step rather than fail a build over a convenience copy.
    let profile = env::var("PROFILE").unwrap_or_default();
    let out_dir = env::var_os("OUT_DIR").map(PathBuf::from).expect("OUT_DIR");
    let Some(profile_dir) = out_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == profile.as_str()))
    else {
        println!(
            "cargo:warning=could not place the {} profile directory above {}; \
             FFmpeg DLLs were not staged next to the binary",
            profile,
            out_dir.display()
        );
        return;
    };
    let mut copied = 0usize;
    for dll in &dlls {
        let dst = profile_dir.join(dll.file_name().unwrap());
        if is_fresh(dll, &dst) {
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

/// Already staged, and not older than the source. Size alone is not enough: a
/// same-size replacement (or a hand-deleted copy) would otherwise be treated as
/// up to date, and the exe would fail to start for a missing DLL.
fn is_fresh(src: &Path, dst: &Path) -> bool {
    let (Ok(src), Ok(dst)) = (src.metadata(), dst.metadata()) else {
        return false;
    };
    dst.len() == src.len()
        && match (src.modified(), dst.modified()) {
            (Ok(src), Ok(dst)) => dst >= src,
            // No usable mtime: copy, it is cheaper than a broken binary.
            _ => false,
        }
}
