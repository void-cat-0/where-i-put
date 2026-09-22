# Deployment templates

Three platforms, three service managers, three files. They are **templates to
copy and edit**, not installers; the paths in them are placeholders.

| Platform | File | Service manager | Stop signal |
| --- | --- | --- | --- |
| Linux | `item-ingest.service` | systemd | `SIGTERM` |
| macOS | `com.whereiput.ingest.plist` | launchd | `SIGTERM` |
| Windows | `install-windows-task.ps1` | Task Scheduler | console events |

The daemon handles each of those, plus `SIGINT`/`SIGHUP` on Unix and
`Ctrl-Break`/console-close/logoff/shutdown on Windows — see
`wait_for_signal` in `crates/item-ingest/src/daemon.rs`. It drains camera
threads, writes a final health snapshot and checkpoints the WAL before exiting 0,
within a 30-second budget the templates give it.

## Before installing, on every platform

1. **Absolute paths.** The daemon resolves relative paths in `config.toml`
   against the **config file's directory**, not the working directory
   (`docs/resident-ingest.md` §3). A config in `/etc/where-i-put` that says
   `db = "data/items.db"` writes into `/etc/where-i-put/data` — almost never
   what you want. Either keep the config and its data tree together, or write
   absolute paths in the config.
2. **The log directory must exist.** launchd does not create it; a
   `StandardErrorPath` in a missing directory is where a crash's only evidence
   goes.
3. **Camera permissions.** The account the service runs as must be able to read
   the device (Linux: usually the `video` group).
4. **Models are not vendored.** If the config uses `detector = "yolo"`, the
   model path in the config must point at a real file; a missing model is a
   fatal per-camera error at startup (reported as `failed` in `health.json`,
   not retried forever).

## Restart semantics, and why they now work

Both systemd (`Restart=on-failure`) and Task Scheduler (restart three times, one
minute apart) assume a crashed daemon can be restarted. It can, because a lock
left behind by a kill stops beating: the lock file's mtime is refreshed every
15s while the daemon runs, and a start that finds a lock silent for 45s takes it
over (`crates/item-ingest/src/lock.rs`).

A fresh `health.json` outvotes that: if the health file was written within §5's
90-second window, the lock's new owner is refused even when the heartbeat has
stopped, because a wedged-but-live daemon must not have its lock stolen. Look
for `previous run died without shutdown` in the log to confirm a takeover
happened.

## Verifying an install

```sh
# Is it alive? updated_at older than 90s means dead or hung (§5).
cat <snapshots' sibling>/health.json

# Did it survive a hard kill? Kill it, wait a minute, check the log for:
#   "previous run died without shutdown; taking over the lock"
```

For Windows specifically, `Stop-ScheduledTask` ends the process **without** a
console event, so the daemon cannot drain and the next start takes over its
stale lock. That is the designed recovery path, not a failure — but to stop it
cleanly, close its console window or send Ctrl-C from the session that started
it.