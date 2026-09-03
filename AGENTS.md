# AGENTS.md

Notes for AI coding agents (and humans) working in this repo.

## Version control: jj (Jujutsu)

This repo is managed with **jujutsu (`jj`)**, colocated with git
(`jj git init --colocate` was used; `.git/` still exists for remotes and
interop). Default to jj for all version-control operations:

- **Do not** run `git commit`, `git add`, `git checkout`, or `git reset`
  here. jj is the source of truth for the working copy; mixing raw git
  writes into a colocated repo causes conflicts and surprising divergent
  heads.
- `git push` / `git fetch` remain fine when the user explicitly asks to
  talk to a remote (colocated mode syncs jj automatically afterwards).
  Otherwise prefer `jj git push` / `jj git fetch`.

Common commands an agent will need:

| Task | Command |
| --- | --- |
| See current state + changes | `jj status` |
| History | `jj log -r 'main::@'` (or plain `jj log`) |
| Describe ("commit") the working copy | `jj describe -m "..."` |
| New change on top ("commit + start next") | `jj new` |
| Squash a change into its parent | `jj squash -r <change>` |
| Abandon a change | `jj abandon -r <change>` |
| Diff a change | `jj diff --change <change>` / `obslog` for history |

jj concepts differ from git: every snapshot of the working copy **is** a
commit (change); there is no index/staging area. `jj describe` replaces
"commit with message". A change left without a description is normal
mid-work state, not an error to "fix" by committing prematurely.

Before ending a work session, make sure `@` has a meaningful description
(via `jj describe`) so the user's `jj log` reads cleanly.

## Toolchain / build

- Rust workspace, edition 2024 (rustc ≥ 1.85). `cargo check` / `cargo test`
  work with **no native deps** by default.
- The `rtsp` feature of `item-ingest` needs FFmpeg 7.1 + libclang. Setup is
  automated: run `cargo xtask setup` (downloads verified archives into
  `target/vendor/`), then `cargo build --features rtsp` — `.cargo/config.toml`
  injects `FFMPEG_DIR`/`LIBCLANG_PATH`/runtime `PATH`, so agents must **not**
  export these manually. `cargo xtask status` reports cache state.
- Pinning policy: all artifact URLs + sha256s live in `crates/xtask/src/main.rs`
  module `pins`. FFmpeg must stay on **7.1** (rust-ffmpeg 9.x rejects FFmpeg 8;
  BtbN's rolling `latest` tag ships 8 — never pin it). Each `target/vendor/*`
  dir carries a `manifest.json` recording url/sha/platform (plus flavor
  zip|source); a mismatch or missing manifest means stale cache →
  `cargo xtask setup [--force]`.
- `cargo xtask setup --from-source` builds the pinned upstream FFmpeg tarball
  (configure whitelist: rtsp/tcp/udp + h264/hevc/mjpeg only) into the same
  `target/vendor/ffmpeg` layout. Requires sh/perl/make/nasm (+ cl.exe via MSVC
  Developer shell on Windows) — on this Windows dev machine `make` is absent,
  so the preflight fails with guidance; treat that as the designed behavior.
- `cargo clean` wipes the toolchain cache (it lives under `target/`); that is
  by design, re-run setup after cleaning.
- `vendor/` (dev-only test gear: mediamtx, test.mp4) and `data/` (SQLite) are
  git-ignored; don't commit them, don't delete them to "clean up".

## Project conventions

- Three crates with one-way deps: `item-ingest` / `item-query` → `item-core`.
  Don't introduce a dependency back into `item-core` or between the two
  binaries.
- `Observation` (deduplicated, zone-scoped sightings) is the unit of
  persistence — never store raw per-frame detections in SQLite.
- New optional native backends go behind cargo features, default off, so a
  fresh clone builds and tests green with zero system dependencies.
