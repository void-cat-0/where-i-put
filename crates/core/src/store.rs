//! SQLite persistence: region config + observation log.
//!
//! Vector embeddings (sqlite-vec) are deliberately NOT wired in yet: at
//! personal scale a LIKE/label lookup over observations carries the query
//! layer, and the embedding tables should be a rebuildable cache anyway.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use thiserror::Error;

use crate::model::{Observation, Region};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Two sightings of the same label+zone within this window are merged into one
/// observation instead of inserting a new row.
pub const DEFAULT_DEDUP_WINDOW: Duration = Duration::from_secs(300);

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Open an existing database strictly read-only (no migrate, no WAL
    /// change) -- the web server reads the loop's data while the ingest
    /// daemon writes it; SQLite WAL makes concurrent readers safe.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { conn })
    }

    fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS regions (
                 id         INTEGER PRIMARY KEY,
                 camera_id  TEXT NOT NULL,
                 name       TEXT NOT NULL,
                 x0 REAL NOT NULL, y0 REAL NOT NULL, x1 REAL NOT NULL, y1 REAL NOT NULL,
                 UNIQUE(camera_id, name)
             );
             CREATE TABLE IF NOT EXISTS observations (
                 id              INTEGER PRIMARY KEY,
                 camera_id       TEXT NOT NULL,
                 zone            TEXT NOT NULL,
                 label           TEXT NOT NULL,
                 first_seen      TEXT NOT NULL,
                 last_seen       TEXT NOT NULL,
                 hit_count       INTEGER NOT NULL DEFAULT 1,
                 sample_snapshot TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_obs_lookup
                 ON observations (label, zone, camera_id, last_seen DESC);",
        )?;
        Ok(())
    }

    // ---- regions -----------------------------------------------------------

    pub fn upsert_region(&self, camera_id: &str, name: &str, rect: [f32; 4]) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO regions (camera_id, name, x0, y0, x1, y1)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(camera_id, name) DO UPDATE SET
                 x0 = excluded.x0, y0 = excluded.y0, x1 = excluded.x1, y1 = excluded.y1",
            params![camera_id, name, rect[0], rect[1], rect[2], rect[3]],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM regions WHERE camera_id = ?1 AND name = ?2",
            params![camera_id, name],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn regions_for(&self, camera_id: &str) -> Result<Vec<Region>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, camera_id, name, x0, y0, x1, y1 FROM regions WHERE camera_id = ?1",
        )?;
        let rows = stmt.query_map(params![camera_id], |r| {
            Ok(Region {
                id: r.get(0)?,
                camera_id: r.get(1)?,
                name: r.get(2)?,
                rect: [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?],
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// First region whose rect contains `point`, if any.
    pub fn zone_for_point(&self, camera_id: &str, point: (f32, f32)) -> Result<String> {
        Ok(self
            .regions_for(camera_id)?
            .into_iter()
            .find(|rg| rg.contains(point))
            .map(|rg| rg.name)
            .unwrap_or_else(|| "frame".into()))
    }

    // ---- observations ------------------------------------------------------

    /// Merge with the latest observation of (camera, zone, label) if it is
    /// still open, otherwise insert a fresh row. Returns (row id, is_new):
    /// the caller writes a snapshot exactly when is_new, so every
    /// observation gets one representative image without re-storing on
    /// every merge.
    pub fn record_sighting(
        &self,
        camera_id: &str,
        zone: &str,
        label: &str,
        seen_at: DateTime<Utc>,
        snapshot: Option<&str>,
        window: Duration,
    ) -> Result<(i64, bool)> {
        let cutoff =
            seen_at - chrono::Duration::from_std(window).expect("window fits chrono range");
        let existing: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, last_seen FROM observations
                 WHERE camera_id = ?1 AND zone = ?2 AND label = ?3 AND last_seen >= ?4
                 ORDER BY last_seen DESC LIMIT 1",
                params![camera_id, zone, label, cutoff.to_rfc3339()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();

        if let Some((id, _)) = existing {
            self.conn.execute(
                "UPDATE observations
                 SET last_seen = ?2, hit_count = hit_count + 1,
                     sample_snapshot = COALESCE(sample_snapshot, ?3)
                 WHERE id = ?1",
                params![id, seen_at.to_rfc3339(), snapshot],
            )?;
            return Ok((id, false));
        }

        self.conn.execute(
            "INSERT INTO observations
                 (camera_id, zone, label, first_seen, last_seen, hit_count, sample_snapshot)
             VALUES (?1, ?2, ?3, ?4, ?4, 1, ?5)",
            params![camera_id, zone, label, seen_at.to_rfc3339(), snapshot],
        )?;
        Ok((self.conn.last_insert_rowid(), true))
    }

    /// Attach (or replace) an observation's snapshot path after the fact.
    pub fn set_sample_snapshot(&self, id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE observations SET sample_snapshot = ?2 WHERE id = ?1",
            params![id, path],
        )?;
        Ok(())
    }

    /// The stored snapshot path of one observation, if any. The web layer
    /// resolves it to a file; `frigate://` refs come back verbatim.
    pub fn snapshot_path(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sample_snapshot FROM observations WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
    }

    /// Most recent observations, optionally filtered by a label substring
    /// (case-insensitive) and/or zone. This is the query layer's data source.
    pub fn recent(&self, label_like: Option<&str>, limit: i64) -> Result<Vec<Observation>> {
        let sql =
            format!(
            "SELECT id, camera_id, zone, label, first_seen, last_seen, hit_count, sample_snapshot
             FROM observations
             {}
             ORDER BY last_seen DESC LIMIT ?1",
            if label_like.is_some() { "WHERE label LIKE '%' || ?2 || '%'" } else { "" }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row| -> rusqlite::Result<Observation> {
            Ok(Observation {
                id: r.get(0)?,
                camera_id: r.get(1)?,
                zone: r.get(2)?,
                label: r.get(3)?,
                first_seen: parse_ts(&r.get::<_, String>(4)?)?,
                last_seen: parse_ts(&r.get::<_, String>(5)?)?,
                hit_count: r.get(6)?,
                sample_snapshot: r.get(7)?,
            })
        };
        let rows = match label_like {
            Some(l) => stmt.query_map(params![limit, l], &map),
            None => stmt.query_map(params![limit], &map),
        }?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        return Ok(out);

        fn parse_ts(s: &str) -> rusqlite::Result<DateTime<Utc>> {
            DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(mins: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 10, mins, 0).unwrap()
    }

    #[test]
    fn sightings_merge_within_window_then_split_after() {
        let s = Store::in_memory().unwrap();
        let (id1, new1) = s
            .record_sighting(
                "cam1",
                "entrance",
                "keys",
                ts(0),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert!(new1);
        // +1 min: merged into the same observation
        let (id2, new2) = s
            .record_sighting(
                "cam1",
                "entrance",
                "keys",
                ts(1),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert!(!new2);
        assert_eq!(id1, id2);
        // +10 min: outside the 5-min window -> a new observation
        let (id3, new3) = s
            .record_sighting(
                "cam1",
                "entrance",
                "keys",
                ts(11),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert!(new3);
        assert_ne!(id1, id3);

        let obs = s.recent(Some("keys"), 10).unwrap();
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].hit_count, 1);
        assert_eq!(obs[1].hit_count, 2); // merged row, newest first
    }

    #[test]
    fn zones_from_region_rects() {
        let s = Store::in_memory().unwrap();
        s.upsert_region("cam1", "entrance", [0.0, 0.0, 200.0, 400.0])
            .unwrap();
        assert_eq!(
            s.zone_for_point("cam1", (100.0, 300.0)).unwrap(),
            "entrance"
        );
        assert_eq!(s.zone_for_point("cam1", (999.0, 999.0)).unwrap(), "frame");
    }

    #[test]
    fn snapshot_attached_after_insert() {
        let s = Store::in_memory().unwrap();
        let (id, _) = s
            .record_sighting("cam1", "desk", "keys", ts(0), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        assert!(
            s.recent(Some("keys"), 1).unwrap()[0]
                .sample_snapshot
                .is_none()
        );
        s.set_sample_snapshot(id, "snapshots/1.jpg").unwrap();
        assert_eq!(
            s.recent(Some("keys"), 1).unwrap()[0]
                .sample_snapshot
                .as_deref(),
            Some("snapshots/1.jpg")
        );
    }
}
