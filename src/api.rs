use crate::models::{AppleMusicCredentials, Track, TrackSource};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

#[derive(Clone)]
pub struct AppleMusicClient {
    http: reqwest::Client,
}

impl AppleMusicClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .user_agent("AppleMusicGPUI/0.1.0 (Native Rust GPUI Client)")
            .build()
            .unwrap_or_default();
        Self { http }
    }

    /// Search the Apple Music Catalog (`https://api.music.apple.com/v1/catalog/{storefront}/search`)
    /// when a Developer Token is configured, or automatically fall back to the public Apple iTunes
    /// Search API (`https://itunes.apple.com/search`) so 256kbps AAC previews work out of the box!
    pub async fn search_catalog(
        &self,
        query: &str,
        creds: &AppleMusicCredentials,
    ) -> Result<Vec<Track>, String> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        if creds.has_developer_token() {
            match self.search_apple_music_v1(trimmed, creds).await {
                Ok(tracks) if !tracks.is_empty() => return Ok(tracks),
                Ok(_) => {}
                Err(err) => {
                    eprintln!("Apple Music v1 API search notice (falling back to iTunes catalog): {err}");
                }
            }
        }

        self.search_itunes_public(trimmed, creds.effective_storefront())
            .await
    }

    async fn search_apple_music_v1(
        &self,
        query: &str,
        creds: &AppleMusicCredentials,
    ) -> Result<Vec<Track>, String> {
        let storefront = creds.effective_storefront();
        let url = format!("https://amp-api.music.apple.com/v1/catalog/{storefront}/search");

        let mut req = self
            .http
            .get(&url)
            .query(&[
                ("term", query),
                ("types", "songs"),
                ("limit", "20"),
            ])
            .header("Origin", "https://music.apple.com")
            .header("Referer", "https://music.apple.com/")
            .bearer_auth(creds.developer_token.trim());

        if creds.is_authorized() {
            req = req.header("Media-User-Token", creds.user_token.trim());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("Network request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Apple Music API returned HTTP {}", resp.status()));
        }

        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid JSON response: {e}"))?;

        let mut tracks = Vec::new();
        if let Some(songs) = body
            .pointer("/results/songs/data")
            .and_then(|v| v.as_array())
        {
            for item in songs {
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let Some(attrs) = item.get("attributes") else {
                    continue;
                };

                let title = attrs
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Track")
                    .to_string();
                let artist = attrs
                    .get("artistName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Artist")
                    .to_string();
                let album = attrs
                    .get("albumName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Single")
                    .to_string();
                let genre = attrs
                    .get("genreNames")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .unwrap_or("Apple Music")
                    .to_string();
                let duration_ms = attrs
                    .get("durationInMillis")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000);
                let preview_url = attrs
                    .get("previews")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|p| p.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                let bg_hex = attrs
                    .pointer("/artwork/bgColor")
                    .and_then(|v| v.as_str())
                    .and_then(|s| u32::from_str_radix(s, 16).ok())
                    .unwrap_or_else(|| palette_for_string(&title).0);
                let fg_hex = attrs
                    .pointer("/artwork/textColor1")
                    .and_then(|v| v.as_str())
                    .and_then(|s| u32::from_str_radix(s, 16).ok())
                    .unwrap_or_else(|| palette_for_string(&artist).1);

                let release_year = attrs
                    .get("releaseDate")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.get(0..4))
                    .and_then(|y| y.parse::<u16>().ok())
                    .unwrap_or(2025);

                if !id.is_empty() && !preview_url.is_empty() {
                    tracks.push(Track {
                        id: format!("am-{id}"),
                        title,
                        artist,
                        album,
                        genre,
                        duration_secs: (duration_ms as f64 / 1000.0).max(1.0),
                        artwork_color_hex: bg_hex,
                        artwork_secondary_hex: fg_hex,
                        source: TrackSource::StreamUrl(preview_url),
                        is_lossless: false,
                        release_year,
                        catalog_id: Some(id),
                    });
                }
            }
        }

        Ok(tracks)
    }

    async fn search_itunes_public(
        &self,
        query: &str,
        country: &str,
    ) -> Result<Vec<Track>, String> {
        let resp = self
            .http
            .get("https://itunes.apple.com/search")
            .query(&[
                ("term", query),
                ("media", "music"),
                ("entity", "song"),
                ("country", country),
                ("limit", "24"),
            ])
            .send()
            .await
            .map_err(|e| format!("iTunes Catalog request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("iTunes Search returned HTTP {}", resp.status()));
        }

        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse iTunes JSON: {e}"))?;

        let mut tracks = Vec::new();
        if let Some(results) = body.get("results").and_then(|v| v.as_array()) {
            for item in results {
                let Some(preview_url) = item.get("previewUrl").and_then(|v| v.as_str()) else {
                    continue;
                };
                let track_id = item
                    .get("trackId")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let title = item
                    .get("trackName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Track")
                    .to_string();
                let artist = item
                    .get("artistName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Artist")
                    .to_string();
                let album = item
                    .get("collectionName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Single")
                    .to_string();
                let genre = item
                    .get("primaryGenreName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Pop")
                    .to_string();
                let duration_ms = item
                    .get("trackTimeMillis")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000);
                let release_year = item
                    .get("releaseDate")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.get(0..4))
                    .and_then(|y| y.parse::<u16>().ok())
                    .unwrap_or(2024);

                let (c1, c2) = palette_for_string(&format!("{title}-{artist}"));
                tracks.push(Track {
                    id: format!("itunes-{track_id}"),
                    title,
                    artist,
                    album,
                    genre,
                    duration_secs: (duration_ms as f64 / 1000.0).max(1.0),
                    artwork_color_hex: c1,
                    artwork_secondary_hex: c2,
                    source: TrackSource::StreamUrl(preview_url.to_string()),
                    is_lossless: false,
                    release_year,
                    catalog_id: if track_id > 0 {
                        Some(track_id.to_string())
                    } else {
                        None
                    },
                });
            }
        }

        Ok(tracks)
    }

    /// Fetch the authenticated account's authoritative storefront ID (`GET /v1/me/storefront`),
    /// such as `"tw"`, `"us"`, or `"jp"`.
    pub async fn fetch_user_storefront(
        &self,
        creds: &AppleMusicCredentials,
    ) -> Result<String, String> {
        if !creds.has_developer_token() || !creds.is_authorized() {
            return Err("Developer Token and Media-User-Token are required.".to_string());
        }

        let resp = self
            .http
            .get("https://amp-api.music.apple.com/v1/me/storefront")
            .header("Origin", "https://music.apple.com")
            .header("Referer", "https://music.apple.com/")
            .bearer_auth(creds.developer_token.trim())
            .header("Media-User-Token", creds.user_token.trim())
            .send()
            .await
            .map_err(|e| format!("Failed to fetch /v1/me/storefront: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!(
                "Apple Music /v1/me/storefront returned HTTP {}",
                resp.status()
            ));
        }

        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid storefront JSON: {e}"))?;

        body.pointer("/data/0/id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "No storefront id in /v1/me/storefront response".to_string())
    }

    /// Fetch user's personal Apple Music library songs (`GET https://api.music.apple.com/v1/me/library/songs`)
    /// Requires both `developer_token` and `user_token` (obtained via ephemeral `gpui-webview` OAuth).
    pub async fn fetch_user_library_songs(
        &self,
        creds: &AppleMusicCredentials,
    ) -> Result<Vec<Track>, String> {
        if !creds.has_developer_token() || !creds.is_authorized() {
            return Err(
                "Both Developer Token and Media-User-Token are required to fetch personal library."
                    .to_string(),
            );
        }

        let resp = self
            .http
            .get("https://amp-api.music.apple.com/v1/me/library/songs?include=catalog&limit=50")
            .header("Origin", "https://music.apple.com")
            .header("Referer", "https://music.apple.com/")
            .bearer_auth(creds.developer_token.trim())
            .header("Media-User-Token", creds.user_token.trim())
            .send()
            .await
            .map_err(|e| format!("Failed to reach Apple Music library endpoint: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!(
                "Apple Music /v1/me/library/songs returned HTTP {}",
                resp.status()
            ));
        }

        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid library JSON: {e}"))?;

        let mut tracks = Vec::new();
        if let Some(items) = body.get("data").and_then(|v| v.as_array()) {
            for item in items {
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let Some(attrs) = item.get("attributes") else {
                    continue;
                };
                let title = attrs
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let artist = attrs
                    .get("artistName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Artist")
                    .to_string();
                let album = attrs
                    .get("albumName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Library Album")
                    .to_string();
                let duration_ms = attrs
                    .get("durationInMillis")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(180_000);

                let preview_url = attrs
                    .pointer("/previews/0/url")
                    .or_else(|| {
                        item.pointer("/relationships/catalog/data/0/attributes/previews/0/url")
                    })
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                let source = if !preview_url.is_empty() {
                    TrackSource::StreamUrl(preview_url)
                } else {
                    TrackSource::SynthDemo {
                        root_midi: 60,
                        tempo_bpm: 110,
                        style: 0,
                    }
                };

                let catalog_id = attrs
                    .pointer("/playParams/catalogId")
                    .or_else(|| item.pointer("/relationships/catalog/data/0/id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        if !id.is_empty() {
                            Some(id.to_string())
                        } else {
                            None
                        }
                    });

                let (c1, c2) = palette_for_string(&title);
                tracks.push(Track {
                    id: format!("lib-{id}"),
                    title,
                    artist,
                    album,
                    genre: "Apple Music Library".to_string(),
                    duration_secs: (duration_ms as f64 / 1000.0).max(1.0),
                    artwork_color_hex: c1,
                    artwork_secondary_hex: c2,
                    source,
                    is_lossless: true,
                    release_year: 2025,
                    catalog_id,
                });
            }
        }

        Ok(tracks)
    }
}

pub fn palette_for_string(s: &str) -> (u32, u32) {
    const PALETTES: &[(u32, u32)] = &[
        (0xfa2d48, 0x7928ca),
        (0x007aff, 0x00c6ff),
        (0x10b981, 0x047857),
        (0xf59e0b, 0xb45309),
        (0xec4899, 0x8b5cf6),
        (0x6366f1, 0x3b82f6),
        (0x14b8a6, 0x0f766e),
        (0xf43f5e, 0xbe123c),
    ];
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    let idx = (hasher.finish() as usize) % PALETTES.len();
    PALETTES[idx]
}

