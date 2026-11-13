use crate::models::{
    AppleMusicCredentials, DspConfig, Playlist, Track, default_demo_tracks, default_playlists,
};
use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const KEYRING_SERVICE: &str = "com.gpui.apple-music-client";
const KEYRING_USER_TOKEN: &str = "music-user-token";
const KEYRING_DEV_TOKEN: &str = "developer-token";

pub struct LocalStore {
    conn: Connection,
}

impl LocalStore {
    pub fn open() -> Self {
        let db_path = Self::db_file_path();
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(&db_path)
            .or_else(|_| Connection::open_in_memory())
            .expect("Failed to initialize rusqlite database");

        let store = Self { conn };
        store.init_schema();
        store
    }

    fn db_file_path() -> PathBuf {
        if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("apple-music-gpui")
                .join("library_cache.sqlite3")
        } else {
            PathBuf::from("library_cache.sqlite3")
        }
    }

    fn init_schema(&self) {
        let _ = self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS kv_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS library_tracks (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL,
                added_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS playlists (
                id TEXT PRIMARY KEY,
                json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS search_cache (
                query TEXT PRIMARY KEY,
                json TEXT NOT NULL,
                cached_at INTEGER NOT NULL
            );
            DELETE FROM search_cache WHERE json LIKE '%\"duration_secs\":30.0%';
            ",
        );

        // Seed default tracks and playlists if empty
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM library_tracks", [], |r| r.get(0))
            .unwrap_or(0);

        if count == 0 {
            for track in default_demo_tracks() {
                self.save_track(&track);
            }
        }

        let pl_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM playlists", [], |r| r.get(0))
            .unwrap_or(0);

        if pl_count == 0 {
            for pl in default_playlists() {
                self.save_playlist(&pl);
            }
        }
    }

    pub fn load_tracks(&self) -> Vec<Track> {
        let mut stmt = match self
            .conn
            .prepare("SELECT json FROM library_tracks ORDER BY added_at ASC")
        {
            Ok(s) => s,
            Err(_) => return default_demo_tracks(),
        };

        let rows = stmt
            .query_map([], |row| {
                let s: String = row.get(0)?;
                Ok(s)
            })
            .ok();

        let mut tracks = Vec::new();
        if let Some(rows) = rows {
            for r in rows.flatten() {
                if let Ok(t) = serde_json::from_str::<Track>(&r) {
                    tracks.push(t);
                }
            }
        }

        if tracks.is_empty() {
            default_demo_tracks()
        } else {
            tracks
        }
    }

    pub fn save_track(&self, track: &Track) {
        if let Ok(json) = serde_json::to_string(track) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let _ = self.conn.execute(
                "INSERT OR REPLACE INTO library_tracks (id, json, added_at) VALUES (?1, ?2, ?3)",
                params![track.id, json, now],
            );
        }
    }

    pub fn remove_track(&self, track_id: &str) {
        let _ = self.conn.execute(
            "DELETE FROM library_tracks WHERE id = ?1",
            params![track_id],
        );
    }

    pub fn load_playlists(&self) -> Vec<Playlist> {
        let mut stmt = match self.conn.prepare("SELECT json FROM playlists") {
            Ok(s) => s,
            Err(_) => return default_playlists(),
        };

        let rows = stmt
            .query_map([], |row| {
                let s: String = row.get(0)?;
                Ok(s)
            })
            .ok();

        let mut list = Vec::new();
        if let Some(rows) = rows {
            for r in rows.flatten() {
                if let Ok(p) = serde_json::from_str::<Playlist>(&r) {
                    list.push(p);
                }
            }
        }

        if list.is_empty() {
            default_playlists()
        } else {
            list
        }
    }

    pub fn save_playlist(&self, playlist: &Playlist) {
        if let Ok(json) = serde_json::to_string(playlist) {
            let _ = self.conn.execute(
                "INSERT OR REPLACE INTO playlists (id, json) VALUES (?1, ?2)",
                params![playlist.id, json],
            );
        }
    }

    pub fn get_cached_search(&self, query: &str) -> Option<Vec<Track>> {
        let key = query.trim().to_lowercase();
        let json: String = self
            .conn
            .query_row(
                "SELECT json FROM search_cache WHERE query = ?1",
                params![key],
                |r| r.get(0),
            )
            .ok()?;
        serde_json::from_str(&json).ok()
    }

    pub fn put_cached_search(&self, query: &str, results: &[Track]) {
        let key = query.trim().to_lowercase();
        if let Ok(json) = serde_json::to_string(results) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let _ = self.conn.execute(
                "INSERT OR REPLACE INTO search_cache (query, json, cached_at) VALUES (?1, ?2, ?3)",
                params![key, json, now],
            );
        }
    }

    pub fn load_dsp_config(&self) -> DspConfig {
        let json: Result<String, _> = self.conn.query_row(
            "SELECT value FROM kv_settings WHERE key = 'dsp_config'",
            [],
            |r| r.get(0),
        );
        json.ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save_dsp_config(&self, config: &DspConfig) {
        if let Ok(json) = serde_json::to_string(config) {
            let _ = self.conn.execute(
                "INSERT OR REPLACE INTO kv_settings (key, value) VALUES ('dsp_config', ?1)",
                params![json],
            );
        }
    }

    pub fn load_credentials(&self) -> AppleMusicCredentials {
        let json: Result<String, _> = self.conn.query_row(
            "SELECT value FROM kv_settings WHERE key = 'credentials'",
            [],
            |r| r.get(0),
        );
        let mut creds: AppleMusicCredentials = json
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        // Try loading tokens from OS Keyring first if available
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER_TOKEN) {
            if let Ok(token) = entry.get_password() {
                if !token.is_empty() {
                    creds.user_token = token;
                }
            }
        }
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_DEV_TOKEN) {
            if let Ok(token) = entry.get_password() {
                if !token.is_empty() {
                    creds.developer_token = token;
                }
            }
        }

        // Also check environment variables
        if creds.developer_token.is_empty() {
            if let Ok(env_dev) = std::env::var("APPLE_MUSIC_DEV_TOKEN") {
                creds.developer_token = env_dev;
            }
        }
        if creds.user_token.is_empty() {
            if let Ok(env_user) = std::env::var("APPLE_MUSIC_USER_TOKEN") {
                creds.user_token = env_user;
            }
        }

        if creds.storefront.is_empty() {
            creds.storefront = crate::models::detect_system_storefront();
        }
        creds
    }

    pub fn save_credentials(&self, creds: &AppleMusicCredentials) {
        // Persist tokens in OS keyring (best effort; SQLite fallback keeps it working in sandboxed environments)
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER_TOKEN) {
            if creds.user_token.is_empty() {
                let _ = entry.delete_credential();
            } else {
                let _ = entry.set_password(&creds.user_token);
            }
        }
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_DEV_TOKEN) {
            if creds.developer_token.is_empty() {
                let _ = entry.delete_credential();
            } else {
                let _ = entry.set_password(&creds.developer_token);
            }
        }

        if let Ok(json) = serde_json::to_string(creds) {
            let _ = self.conn.execute(
                "INSERT OR REPLACE INTO kv_settings (key, value) VALUES ('credentials', ?1)",
                params![json],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EqPreset, TrackSource};

    #[test]
    fn test_sqlite_store_roundtrip() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = LocalStore { conn };
        store.init_schema();

        let track = Track {
            id: "test-101".to_string(),
            title: "Veridis Quo".to_string(),
            artist: "Daft Punk".to_string(),
            album: "Discovery".to_string(),
            genre: "Electronic".to_string(),
            duration_secs: 345.0,
            artwork_color_hex: 0xfa2d48,
            artwork_secondary_hex: 0x007aff,
            source: TrackSource::SynthDemo {
                root_midi: 60,
                tempo_bpm: 110,
                style: 0,
            },
            is_lossless: true,
            release_year: 2001,
            catalog_id: None,
        };

        let initial_count = store.load_tracks().len();
        assert_eq!(initial_count, 4);

        store.save_track(&track);
        let loaded = store.load_tracks();
        assert_eq!(loaded.len(), initial_count + 1);
        assert!(loaded.iter().any(|t| t.title == "Veridis Quo"));

        store.remove_track("test-101");
        let after_remove = store.load_tracks();
        assert_eq!(after_remove.len(), initial_count);
        assert!(!after_remove.iter().any(|t| t.id == "test-101"));

        // Search cache test
        store.put_cached_search("Daft Punk", std::slice::from_ref(&track));
        let cached = store.get_cached_search("  daft punk ").expect("cached results");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].artist, "Daft Punk");

        // DSP config test
        let mut dsp = DspConfig::default();
        dsp.apply_preset(EqPreset::VocalBooster);
        store.save_dsp_config(&dsp);
        let loaded_dsp = store.load_dsp_config();
        assert_eq!(loaded_dsp.preset, EqPreset::VocalBooster);
        assert_eq!(loaded_dsp.bands_db, dsp.bands_db);
    }
}
