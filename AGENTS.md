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
- The `rtsp` feature of `item-ingest` requires `FFMPEG_DIR` (a vendored
  FFmpeg **7.1** shared build — rust-ffmpeg 9.x rejects FFmpeg 8) and
  `LIBCLANG_PATH` (libclang for bindgen). Exact setup: README section
  "RTSP backend". Never assume a bare `cargo build --all-features` works
  without those env vars.
- `vendor/` (FFmpeg, libclang, mediamtx, test media) and `data/` (SQLite)
  are git-ignored; don't commit them, don't delete them to "clean up".

## Project conventions

- Three crates with one-way deps: `item-ingest` / `item-query` → `item-core`.
  Don't introduce a dependency back into `item-core` or between the two
  binaries.
- `Observation` (deduplicated, zone-scoped sightings) is the unit of
  persistence — never store raw per-frame detections in SQLite.
- New optional native backends go behind cargo features, default off, so a
  fresh clone builds and tests green with zero system dependencies.
