use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Standard 10-band graphic equalizer center frequencies (Hz)
pub const EQ_BAND_FREQS: [f32; 10] = [
    32.0, 64.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

pub const EQ_BAND_LABELS: [&str; 10] = [
    "32", "64", "125", "250", "500", "1K", "2K", "4K", "8K", "16K",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EqPreset {
    Flat,
    BassBoost,
    VocalBooster,
    Acoustic,
    Electronic,
    LateNight,
    Custom,
}

impl EqPreset {
    pub const ALL: [EqPreset; 6] = [
        EqPreset::Flat,
        EqPreset::BassBoost,
        EqPreset::VocalBooster,
        EqPreset::Acoustic,
        EqPreset::Electronic,
        EqPreset::LateNight,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            EqPreset::Flat => "Flat",
            EqPreset::BassBoost => "Bass Boost",
            EqPreset::VocalBooster => "Vocal Booster",
            EqPreset::Acoustic => "Acoustic",
            EqPreset::Electronic => "Electronic",
            EqPreset::LateNight => "Late Night",
            EqPreset::Custom => "Custom",
        }
    }

    pub fn gains_db(&self) -> [f32; 10] {
        match self {
            EqPreset::Flat | EqPreset::Custom => [0.0; 10],
            EqPreset::BassBoost => [6.5, 5.5, 4.0, 2.0, 0.5, 0.0, 0.0, 0.5, 1.0, 1.5],
            EqPreset::VocalBooster => [-1.5, -1.0, 0.0, 2.0, 4.5, 5.0, 4.0, 2.0, 0.5, 0.0],
            EqPreset::Acoustic => [3.5, 3.0, 2.0, 1.0, 1.5, 1.5, 3.0, 3.5, 3.0, 2.0],
            EqPreset::Electronic => [5.0, 4.5, 2.0, 0.0, -1.5, 1.0, 2.5, 4.0, 5.0, 5.5],
            EqPreset::LateNight => [3.0, 2.5, 1.5, 0.5, -0.5, -1.0, -1.5, -2.5, -3.5, -4.5],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DspConfig {
    pub eq_enabled: bool,
    pub preamp_db: f32,
    pub bands_db: [f32; 10],
    pub preset: EqPreset,
    pub spatial_enabled: bool,
    pub crossfeed_amount: f32, // 0.0 to 1.0
    pub room_width: f32,       // 0.0 to 1.0
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            eq_enabled: true,
            preamp_db: 0.0,
            bands_db: [0.0; 10],
            preset: EqPreset::Flat,
            spatial_enabled: false,
            crossfeed_amount: 0.35,
            room_width: 0.40,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackSource {
    /// Apple Music Catalog / iTunes AAC/M4A stream or preview URL
    StreamUrl(String),
    /// Local audio file on disk (FLAC, ALAC, AAC, MP3, WAV)
    LocalFile(String),
    /// Built-in polyphonic synth chord progression for instant zero-network testing
    SynthDemo {
        root_midi: u8,
        tempo_bpm: u16,
        style: u8,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub duration_secs: f64,
    pub artwork_color_hex: u32,
    pub artwork_secondary_hex: u32,
    pub source: TrackSource,
    pub is_lossless: bool,
    pub release_year: u16,
    #[serde(default)]
    pub catalog_id: Option<String>,
}

impl Track {
    /// Returns the Apple Music Catalog Song ID (or Cloud Library Song ID `i.xxx`) if this track
    /// can be streamed in full length via `gpui-webview` + MusicKit JS v3.
    pub fn effective_catalog_id(&self) -> Option<String> {
        if let Some(ref cid) = self.catalog_id {
            let trimmed = cid.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(rest) = self.id.strip_prefix("am-") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
        if let Some(rest) = self.id.strip_prefix("itunes-") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
        if let Some(rest) = self.id.strip_prefix("lib-") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub description: String,
    pub track_ids: Vec<String>,
    pub accent_hex: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    ListenNow,
    BrowseSearch,
    Library,
    Playlists,
    AudioDsp,
    AuthSettings,
}

impl NavTab {
    pub fn title(&self) -> &'static str {
        match self {
            NavTab::ListenNow => "Listen Now",
            NavTab::BrowseSearch => "Browse & Search",
            NavTab::Library => "Library",
            NavTab::Playlists => "Playlists",
            NavTab::AudioDsp => "10-Band EQ & DSP",
            NavTab::AuthSettings => "MusicKit Auth & Settings",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppleMusicCredentials {
    pub developer_token: String,
    pub user_token: String,
    pub team_id: String,
    pub key_id: String,
    pub private_key_pem: String,
    pub storefront: String,
}

/// Extract a 2-letter lowercase country/storefront code from a locale string
/// such as `zh_TW.UTF-8`, `en-US`, or macOS `en_US@rg=twzzzz`.
pub fn parse_locale_country_code(locale: &str) -> Option<String> {
    let trimmed = locale.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Check macOS region override first, e.g. `en_US@rg=twzzzz`
    if let Some((_, after_rg)) = trimmed.split_once("@rg=") {
        let code: String = after_rg.chars().take(2).collect();
        if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
            return Some(code.to_lowercase());
        }
    }
    let base = trimmed.split(['.', '@']).next().unwrap_or(trimmed);
    let region = base.split(['_', '-']).nth(1)?;
    if region.len() == 2 && region.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(region.to_lowercase());
    }
    None
}

/// Detect the default storefront code from OS locale / environment variables, defaulting to `"us"`.
pub fn detect_system_storefront() -> String {
    if let Ok(explicit) = std::env::var("APPLE_MUSIC_STOREFRONT") {
        let trimmed = explicit.trim().to_lowercase();
        if trimmed.len() == 2 && trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
            return trimmed;
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(code) = parse_locale_country_code(&s) {
                return code;
            }
        }
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            if let Some(code) = parse_locale_country_code(&val) {
                return code;
            }
        }
    }
    "us".to_string()
}

impl AppleMusicCredentials {
    pub fn effective_storefront(&self) -> &str {
        if self.storefront.trim().is_empty() {
            "us"
        } else {
            self.storefront.trim()
        }
    }

    pub fn has_developer_token(&self) -> bool {
        !self.developer_token.trim().is_empty()
    }

    pub fn is_authorized(&self) -> bool {
        !self.user_token.trim().is_empty()
    }

    /// Generate an ES256-signed Apple Music Developer JWT from Team ID, Key ID, and .p8 PEM (or .p8 file path)
    pub fn generate_es256_jwt(&mut self) -> Result<String, String> {
        let team_id = self.team_id.trim();
        let key_id = self.key_id.trim();
        let raw_pem_or_path = self.private_key_pem.trim();

        if team_id.is_empty() || key_id.is_empty() || raw_pem_or_path.is_empty() {
            return Err(
                "Team ID, Key ID, and Private Key (.p8 file path or PEM) are required".to_string(),
            );
        }

        // Support either a path to a .p8 file on disk or pasted single/multi-line PEM content
        let loaded_pem = if !raw_pem_or_path.contains("PRIVATE KEY")
            && (raw_pem_or_path.ends_with(".p8")
                || raw_pem_or_path.starts_with('/')
                || raw_pem_or_path.starts_with("~/"))
        {
            let expanded = if let Some(rest) = raw_pem_or_path.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/{rest}")
            } else {
                raw_pem_or_path.to_string()
            };
            std::fs::read_to_string(&expanded)
                .map_err(|e| format!("Failed to read .p8 file at {expanded}: {e}"))?
        } else {
            raw_pem_or_path.to_string()
        };

        // Normalize single-line pasted PEM into standard multi-line PEM format
        let body: String = loaded_pem
            .replace("-----BEGIN PRIVATE KEY-----", "")
            .replace("-----END PRIVATE KEY-----", "")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let mut normalized_pem = String::from("-----BEGIN PRIVATE KEY-----\n");
        for chunk in body.as_bytes().chunks(64) {
            if let Ok(s) = std::str::from_utf8(chunk) {
                normalized_pem.push_str(s);
                normalized_pem.push('\n');
            }
        }
        normalized_pem.push_str("-----END PRIVATE KEY-----\n");

        #[derive(Serialize)]
        struct Claims<'a> {
            iss: &'a str,
            iat: u64,
            exp: u64,
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let claims = Claims {
            iss: team_id,
            iat: now,
            // Valid for 30 days (Apple allows up to 6 months)
            exp: now + 30 * 24 * 3600,
        };

        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(key_id.to_string());

        let encoding_key = jsonwebtoken::EncodingKey::from_ec_pem(normalized_pem.as_bytes())
            .map_err(|e| format!("Invalid ES256 .p8 key: {e}"))?;

        let token = jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| format!("Failed to sign JWT: {e}"))?;

        self.developer_token = token.clone();
        Ok(token)
    }
}

/// Curated default tracks so the player has rich shelves and immediate audio playback
pub fn default_demo_tracks() -> Vec<Track> {
    vec![
        Track {
            id: "demo-synth-1".to_string(),
            title: "Midnight Horizon (Spatial Synth)".to_string(),
            artist: "GPUI Audio Lab".to_string(),
            album: "Native Rust DSP Sessions".to_string(),
            genre: "Electronic / Synthwave".to_string(),
            duration_secs: 96.0,
            artwork_color_hex: 0xfa2d48,
            artwork_secondary_hex: 0x7928ca,
            source: TrackSource::SynthDemo {
                root_midi: 60, // C4
                tempo_bpm: 112,
                style: 0,
            },
            is_lossless: true,
            release_year: 2026,
            catalog_id: None,
        },
        Track {
            id: "demo-synth-2".to_string(),
            title: "Sub-Bass Cathedral (32Hz–16kHz Test)".to_string(),
            artist: "Biquad & RustFFT".to_string(),
            album: "Native Rust DSP Sessions".to_string(),
            genre: "Ambient / Bass".to_string(),
            duration_secs: 120.0,
            artwork_color_hex: 0x007aff,
            artwork_secondary_hex: 0x00c6ff,
            source: TrackSource::SynthDemo {
                root_midi: 53, // F3
                tempo_bpm: 92,
                style: 1,
            },
            is_lossless: true,
            release_year: 2026,
            catalog_id: None,
        },
        Track {
            id: "demo-synth-3".to_string(),
            title: "Crystal Arpeggio (Crossfeed Stage)".to_string(),
            artist: "Symphonia Ensemble".to_string(),
            album: "Zero Browser Overhead".to_string(),
            genre: "IDM / Chillwave".to_string(),
            duration_secs: 84.0,
            artwork_color_hex: 0x10b981,
            artwork_secondary_hex: 0x047857,
            source: TrackSource::SynthDemo {
                root_midi: 64, // E4
                tempo_bpm: 124,
                style: 2,
            },
            is_lossless: true,
            release_year: 2026,
            catalog_id: None,
        },
        Track {
            id: "demo-synth-4".to_string(),
            title: "Velvet Nocturne (Lo-Fi Rhodes)".to_string(),
            artist: "CoreAudio & CPAL".to_string(),
            album: "Late Night Coding".to_string(),
            genre: "Lo-Fi / Jazz Hop".to_string(),
            duration_secs: 108.0,
            artwork_color_hex: 0xf59e0b,
            artwork_secondary_hex: 0xb45309,
            source: TrackSource::SynthDemo {
                root_midi: 57, // A3
                tempo_bpm: 80,
                style: 3,
            },
            is_lossless: true,
            release_year: 2026,
            catalog_id: None,
        },
    ]
}

pub fn default_playlists() -> Vec<Playlist> {
    vec![
        Playlist {
            id: "pl-spatial-dsp".to_string(),
            name: "Spatial & EQ Reference".to_string(),
            description: "Test the 10-band Biquad EQ, stereo crossfeed, and real-time FFT spectrum analyzer.".to_string(),
            track_ids: vec![
                "demo-synth-1".to_string(),
                "demo-synth-2".to_string(),
                "demo-synth-3".to_string(),
                "demo-synth-4".to_string(),
            ],
            accent_hex: 0xfa2d48,
        },
        Playlist {
            id: "pl-favorites".to_string(),
            name: "Favorite Tracks".to_string(),
            description: "Your saved Apple Music catalog tracks and local lossless files.".to_string(),
            track_ids: vec![
                "demo-synth-1".to_string(),
                "demo-synth-3".to_string(),
            ],
            accent_hex: 0x8b5cf6,
        },
    ]
}

impl DspConfig {
    pub fn apply_preset(&mut self, preset: EqPreset) {
        self.preset = preset;
        if preset != EqPreset::Custom {
            self.bands_db = preset.gains_db();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eq_presets_and_dsp_config() {
        let mut cfg = DspConfig::default();
        assert_eq!(cfg.preset, EqPreset::Flat);
        assert_eq!(cfg.bands_db, [0.0; 10]);

        cfg.apply_preset(EqPreset::BassBoost);
        assert_eq!(cfg.preset, EqPreset::BassBoost);
        assert!(cfg.bands_db[0] > 5.0);
        assert!(cfg.bands_db[1] > 4.0);

        cfg.apply_preset(EqPreset::LateNight);
        assert_eq!(cfg.preset, EqPreset::LateNight);
        assert!(cfg.bands_db[9] < 0.0);
    }

    #[test]
    fn test_credentials_validation() {
        let mut creds = AppleMusicCredentials::default();
        assert_eq!(creds.effective_storefront(), "us");
        assert!(!creds.has_developer_token());
        assert!(!creds.is_authorized());
        assert!(creds.generate_es256_jwt().is_err());

        assert_eq!(
            parse_locale_country_code("zh_TW.UTF-8").as_deref(),
            Some("tw")
        );
        assert_eq!(
            parse_locale_country_code("en_US@rg=twzzzz").as_deref(),
            Some("tw")
        );
        assert_eq!(parse_locale_country_code("ja-JP").as_deref(), Some("jp"));
    }
}
