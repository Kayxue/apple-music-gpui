mod api;
mod audio;
mod bridge;
mod db;
mod models;

use api::{AppleMusicClient, palette_for_string};
use audio::{AudioCommand, AudioSharedState, NativeAudioEngine, SPECTRUM_BINS};
use bridge::{BridgeEvent, EphemeralAuthServer};
use db::LocalStore;
use gpui_kit::{
    assets::AllAssets,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
        Theme, ThemeMode,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        scroll::ScrollableElement as _,
        slider::{Slider, SliderEvent, SliderState},
        switch::Switch,
        v_flex,
    },
    prelude::FluentBuilder as _,
    *,
};
use gpui_webview::WebView;
use models::{
    AppleMusicCredentials, DspConfig, EQ_BAND_FREQS, EQ_BAND_LABELS, EqPreset, NavTab,
    PlaybackStatus, Playlist, Track, TrackSource,
};
use std::time::Duration;

actions!(
    apple_music_gpui,
    [TogglePlayPause, NextTrack, PrevTrack, QuitApp]
);

/// Standalone GPUI Entity for the 36-bin FFT Spectrum & VU Meter so high-FPS visualizer frames
/// only re-render this small box instead of re-laying out the entire application window!
pub struct SpectrumVisualizerView {
    shared: std::sync::Arc<AudioSharedState>,
    accent_hex: u32,
    secondary_hex: u32,
}

impl Render for SpectrumVisualizerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = rgb(self.accent_hex);
        let secondary = rgb(self.secondary_hex);
        let spectrum = self.shared.spectrum_snapshot();
        let (peak_l, peak_r) = self.shared.peaks_snapshot();

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Real-Time 1024-Sample Hann FFT Spectrum (rustfft in cpal callback)"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "32 Hz — 16 kHz  •  L/R Peak: {:.0}% / {:.0}%",
                                (peak_l * 100.0).min(100.0),
                                (peak_r * 100.0).min(100.0)
                            )),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .h(px(76.0))
                    .p_2()
                    .rounded(px(10.0))
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .items_end()
                    .justify_between()
                    .gap_1()
                    .children((0..SPECTRUM_BINS).map(|i| {
                        let mag = spectrum[i].clamp(0.04, 1.0);
                        let bar_h = (mag * 58.0).max(3.0);
                        let color = if i < 12 {
                            accent
                        } else if i < 24 {
                            secondary
                        } else {
                            rgb(0x00c6ff)
                        };
                        div()
                            .flex_1()
                            .h(px(bar_h))
                            .rounded(px(3.0))
                            .bg(color)
                    })),
            )
    }
}

pub struct AppleMusicApp {
    focus_handle: FocusHandle,
    store: LocalStore,
    api_client: AppleMusicClient,
    tokio_rt: tokio::runtime::Runtime,
    audio_engine: NativeAudioEngine,
    spectrum_view: Entity<SpectrumVisualizerView>,

    // Navigation & Theme
    active_tab: NavTab,
    dark_mode: bool,
    accent_hex: u32,
    secondary_hex: u32,
    status_banner: String,

    // Library, Playlists & Search State
    library_tracks: Vec<Track>,
    playlists: Vec<Playlist>,
    selected_playlist_id: Option<String>,
    search_results: Vec<Track>,
    is_searching: bool,

    // Playback Queue State
    current_track: Option<Track>,
    queue: Vec<Track>,
    queue_index: usize,

    // Audio DSP State
    dsp_config: DspConfig,
    scrubber_slider: Entity<SliderState>,
    volume_slider: Entity<SliderState>,
    preamp_slider: Entity<SliderState>,
    eq_band_sliders: [Entity<SliderState>; 10],
    crossfeed_slider: Entity<SliderState>,
    room_slider: Entity<SliderState>,
    updating_scrubber_from_audio: bool,

    // Inputs
    search_input: Entity<InputState>,
    local_file_input: Entity<InputState>,
    playlist_name_input: Entity<InputState>,
    dev_token_input: Entity<InputState>,
    user_token_input: Entity<InputState>,
    team_id_input: Entity<InputState>,
    key_id_input: Entity<InputState>,
    p8_pem_input: Entity<InputState>,

    // Credentials & gpui-webview MusicKit JS v3 Player / OAuth State
    credentials: AppleMusicCredentials,
    auth_webview: Option<Entity<WebView>>,
    auth_server: Option<EphemeralAuthServer>,
    bridge_tx: flume::Sender<BridgeEvent>,
    using_webview_player: bool,
    webview_ready: bool,
    pending_webview_play: Option<(String, String, String)>,
}

impl AppleMusicApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = LocalStore::open();
        let library_tracks = store.load_tracks();
        let playlists = store.load_playlists();
        let dsp_config = store.load_dsp_config();
        let credentials = store.load_credentials();

        let tokio_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to create background Tokio runtime");

        let audio_engine = NativeAudioEngine::spawn();
        audio_engine.send(AudioCommand::UpdateDsp(dsp_config.clone()));

        let first_track = library_tracks.first().cloned();
        let (accent_hex, secondary_hex) = first_track
            .as_ref()
            .map(|t| (t.artwork_color_hex, t.artwork_secondary_hex))
            .unwrap_or((0xfa2d48, 0x7928ca));

        // Build sliders
        let scrubber_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(0.2)
                .default_value(0.0)
        });
        let volume_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(1.0)
                .default_value(85.0)
        });
        let preamp_slider = cx.new(|_| {
            SliderState::new()
                .min(-12.0)
                .max(12.0)
                .step(0.5)
                .default_value(dsp_config.preamp_db)
        });

        let eq_band_sliders: [Entity<SliderState>; 10] = std::array::from_fn(|i| {
            let gain = dsp_config.bands_db[i];
            cx.new(|_| {
                SliderState::new()
                    .min(-12.0)
                    .max(12.0)
                    .step(0.5)
                    .default_value(gain)
            })
        });

        let crossfeed_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(1.0)
                .default_value(dsp_config.crossfeed_amount * 100.0)
        });
        let room_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(1.0)
                .default_value(dsp_config.room_width * 100.0)
        });

        // Subscribe to sliders
        cx.subscribe(&scrubber_slider, |this, _, event: &SliderEvent, cx| {
            if this.updating_scrubber_from_audio {
                return;
            }
            let (SliderEvent::Release(val) | SliderEvent::Change(val)) = event;
            let pct = val.end() / 100.0;
            let dur = this.audio_engine.shared.duration_secs().max(1.0);
            let target_secs = pct as f64 * dur;
            if this.using_webview_player {
                this.audio_engine.shared.set_position_secs(target_secs);
                this.eval_webview_js(
                    &format!("window.gpuiBridge && window.gpuiBridge.seekTo({target_secs:.2});"),
                    cx,
                );
            } else {
                this.audio_engine.send(AudioCommand::Seek(target_secs));
            }
            cx.notify();
        })
        .detach();

        cx.subscribe(&volume_slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(val) | SliderEvent::Release(val)) = event;
            let v = (val.end() / 100.0).clamp(0.0, 1.0);
            this.audio_engine.send(AudioCommand::SetVolume(v));
            this.eval_webview_js(
                &format!("window.gpuiBridge && window.gpuiBridge.setVolume({v:.3});"),
                cx,
            );
            cx.notify();
        })
        .detach();

        cx.subscribe(&preamp_slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(val) | SliderEvent::Release(val)) = event;
            this.dsp_config.preamp_db = val.end();
            this.sync_dsp_config(cx);
        })
        .detach();

        for (band_idx, slider_entity) in eq_band_sliders.iter().enumerate() {
            cx.subscribe(slider_entity, move |this, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(val) | SliderEvent::Release(val)) = event;
                this.dsp_config.bands_db[band_idx] = val.end();
                this.dsp_config.preset = EqPreset::Custom;
                this.sync_dsp_config(cx);
            })
            .detach();
        }

        cx.subscribe(&crossfeed_slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(val) | SliderEvent::Release(val)) = event;
            this.dsp_config.crossfeed_amount = (val.end() / 100.0).clamp(0.0, 1.0);
            this.sync_dsp_config(cx);
        })
        .detach();

        cx.subscribe(&room_slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(val) | SliderEvent::Release(val)) = event;
            this.dsp_config.room_width = (val.end() / 100.0).clamp(0.0, 1.0);
            this.sync_dsp_config(cx);
        })
        .detach();

        // Build inputs
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search Apple Music / iTunes Catalog (e.g. Daft Punk, Weeknd, Yoasobi)...")
                .default_value("Daft Punk")
        });
        let local_file_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/path/to/local/audio.(flac|m4a|mp3|wav)")
        });
        let playlist_name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("New Playlist Name...")
        });
        let dev_token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste ES256 Developer JWT (or generate below)...")
                .default_value(credentials.developer_token.clone())
        });
        let user_token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Media-User-Token (populated automatically by Ephemeral WebView OAuth)...")
                .default_value(credentials.user_token.clone())
        });
        let team_id_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("10-char Apple Developer Team ID (e.g. A1B2C3D4E5)")
                .default_value(credentials.team_id.clone())
        });
        let key_id_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("10-char MusicKit Key ID (e.g. Z9Y8X7W6V5)")
                .default_value(credentials.key_id.clone())
        });
        let p8_pem_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/path/to/AuthKey_XXXX.p8  OR  -----BEGIN PRIVATE KEY----- ...")
                .default_value(credentials.private_key_pem.clone())
        });

        cx.subscribe(&search_input, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.trigger_catalog_search(cx);
            }
        })
        .detach();

        // Bridge IPC channel from gpui-webview MusicKit JS v3 Player & OAuth
        let (bridge_tx, bridge_rx) = flume::unbounded::<BridgeEvent>();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = bridge_rx.recv_async().await {
                if !matches!(event, BridgeEvent::PlaybackTime { .. }) {
                    bridge::dbg_log(&format!("[bridge] {event:?}"));
                }
                let _ = this.update(cx, |app, cx| {
                    match event {
                        BridgeEvent::Ready {} => {
                            app.webview_ready = true;
                            if app.credentials.is_authorized() {
                                app.status_banner = format!(
                                    "gpui-webview MusicKit JS v3 Ready (Storefront: {}) — Full-Song DRM Playback Active.",
                                    app.credentials.effective_storefront().to_uppercase()
                                );
                            } else {
                                app.status_banner =
                                    "MusicKit JS v3 WebView Ready — Sign in with your Apple ID or paste Media-User-Token.".to_string();
                            }
                            if let Some((song_id, fallback_url, lib_id)) = app.pending_webview_play.take() {
                                app.dispatch_webview_play_song(&song_id, &fallback_url, &lib_id, cx);
                            }
                        }
                        BridgeEvent::Authorized { user_token } => {
                            app.credentials.user_token = user_token.clone();
                            app.store.save_credentials(&app.credentials);
                            if let Some(ref srv) = app.auth_server {
                                srv.set_context(
                                    app.credentials.developer_token.clone(),
                                    app.credentials.effective_storefront().to_string(),
                                    app.credentials.user_token.clone(),
                                );
                            }
                            app.status_banner = format!(
                                "Authorized! Media-User-Token saved ({}...) — gpui-webview Full-Song DRM Player Active.",
                                &user_token.chars().take(18).collect::<String>()
                            );
                            if app.credentials.has_developer_token() {
                                app.sync_account_storefront(cx);
                                app.sync_cloud_library(cx);
                            }
                        }
                        BridgeEvent::StorefrontDetected { storefront } => {
                            app.apply_detected_storefront(&storefront, cx);
                        }
                        BridgeEvent::Error { message } => {
                            if !message.starts_with("Queue OK") && !message.starts_with("Attempt ") {
                                app.status_banner = format!("MusicKit JS Bridge Notice: {message}");
                            }
                        }
                        BridgeEvent::PlaybackState { state, is_playing } => {
                            if app.using_webview_player {
                                // MusicKit states: 1 loading, 2 playing, 3 paused, 4 stopped,
                                // 5 ended, 6 seeking, 7 waiting, 8 stalled, 9 completed.
                                // Transitional states (loading/seeking/waiting/stalled) must NOT
                                // flip the UI to "stopped".
                                let _ = is_playing;
                                let is_playing = match state {
                                    2 => true,
                                    3 | 4 | 5 | 9 | 0 => false,
                                    _ => app
                                        .audio_engine
                                        .shared
                                        .is_playing
                                        .load(std::sync::atomic::Ordering::Relaxed),
                                };
                                app.audio_engine
                                    .shared
                                    .is_playing
                                    .store(is_playing, std::sync::atomic::Ordering::Relaxed);
                                let dur = app.audio_engine.shared.duration_secs();
                                let pos = app.audio_engine.shared.position_secs();
                                if state == 9 || (state == 5 && dur > 5.0 && pos > dur - 3.0) {
                                    app.next_track(cx);
                                } else if state == 1 || state == 7 || state == 8 {
                                    app.audio_engine.shared.set_status(format!(
                                        "gpui-webview MusicKit: buffering/loading (state {state})..."
                                    ));
                                } else if is_playing {
                                    app.audio_engine.shared.set_status(format!(
                                        "Playing Full Song via gpui-webview MusicKit JS v3 ({:.0}s)",
                                        dur
                                    ));
                                } else if state == 3 {
                                    app.audio_engine.shared.set_status(
                                        "Paused (gpui-webview MusicKit JS v3)".to_string(),
                                    );
                                }
                            }
                        }
                        BridgeEvent::PlaybackTime { current, duration } => {
                            if app.using_webview_player {
                                app.audio_engine.shared.set_position_secs(current);
                                if duration > 1.0 {
                                    let prev_dur = app.audio_engine.shared.duration_secs();
                                    app.audio_engine.shared.set_duration_secs(duration);
                                    if (prev_dur - duration).abs() > 1.0 {
                                        app.audio_engine.shared.set_status(format!(
                                            "Playing Full Song via gpui-webview MusicKit JS v3 ({:.0}s)",
                                            duration
                                        ));
                                        // Update duration on current_track & library/search rows if it was previously a 30s estimate
                                        if let Some(ref mut cur) = app.current_track {
                                            cur.duration_secs = duration;
                                            let cur_id = cur.id.clone();
                                            for t in &mut app.library_tracks {
                                                if t.id == cur_id {
                                                    t.duration_secs = duration;
                                                    app.store.save_track(t);
                                                }
                                            }
                                            for t in &mut app.search_results {
                                                if t.id == cur_id {
                                                    t.duration_secs = duration;
                                                }
                                            }
                                            for t in &mut app.queue {
                                                if t.id == cur_id {
                                                    t.duration_secs = duration;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        BridgeEvent::FallbackPreview { url, reason } => {
                            app.using_webview_player = false;
                            let dur = app
                                .current_track
                                .as_ref()
                                .map(|t| t.duration_secs)
                                .unwrap_or(30.0);
                            app.status_banner = format!(
                                "MusicKit WebView notice ({reason}) — falling back to Symphonia stream."
                            );
                            app.audio_engine.send(AudioCommand::PlaySource {
                                source: TrackSource::StreamUrl(url),
                                duration_hint: dur,
                            });
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();

        let shared_for_vis = audio_engine.shared.clone();
        let spectrum_view = cx.new(|_| SpectrumVisualizerView {
            shared: shared_for_vis,
            accent_hex,
            secondary_hex,
        });

        // Lightweight UI ticker:
        // - Isolated SpectrumVisualizerView updates at ~11 FPS (90ms) ONLY when ListenNow tab is visible & playing
        // - Root AppleMusicApp only re-renders once per second (1 Hz) when integer playback second changes!
        cx.spawn(async move |this, cx| {
            let mut last_rendered_sec: u64 = u64::MAX;
            let mut tick_phase: f32 = 0.0;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(90))
                    .await;
                let ok = this.update(cx, |app, cx| {
                    let is_playing = app
                        .audio_engine
                        .shared
                        .is_playing
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if !is_playing {
                        return;
                    }

                    if app.active_tab == NavTab::ListenNow {
                        if app.using_webview_player {
                            tick_phase += 0.38;
                            app.animate_webview_spectrum(tick_phase);
                        }
                        let a = app.accent_hex;
                        let s = app.secondary_hex;
                        app.spectrum_view.update(cx, |sv, cx| {
                            sv.accent_hex = a;
                            sv.secondary_hex = s;
                            cx.notify();
                        });
                    }

                    let cur_sec = app.audio_engine.shared.position_secs() as u64;
                    if cur_sec != last_rendered_sec {
                        last_rendered_sec = cur_sec;
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        let selected_playlist_id = playlists.first().map(|p| p.id.clone());
        let queue = library_tracks.clone();
        let focus_handle = cx.focus_handle();

        let mut app = Self {
            focus_handle,
            store,
            api_client: AppleMusicClient::new(),
            tokio_rt,
            audio_engine,
            spectrum_view,
            active_tab: NavTab::ListenNow,
            dark_mode: true,
            accent_hex,
            secondary_hex,
            status_banner:
                "Hybrid Audio Pipeline Active (gpui-webview MusicKit JS v3 Full-Song DRM + Native Symphonia DSP)"
                    .to_string(),
            library_tracks,
            playlists,
            selected_playlist_id,
            search_results: Vec::new(),
            is_searching: false,
            current_track: first_track,
            queue,
            queue_index: 0,
            dsp_config,
            scrubber_slider,
            volume_slider,
            preamp_slider,
            eq_band_sliders,
            crossfeed_slider,
            room_slider,
            updating_scrubber_from_audio: false,
            search_input,
            local_file_input,
            playlist_name_input,
            dev_token_input,
            user_token_input,
            team_id_input,
            key_id_input,
            p8_pem_input,
            credentials,
            auth_webview: None,
            auth_server: None,
            bridge_tx,
            using_webview_player: false,
            webview_ready: false,
            pending_webview_play: None,
        };

        // Initialize the persistent gpui-webview MusicKit JS v3 player and resolve account storefront on startup
        if app.credentials.has_developer_token() || app.credentials.is_authorized() {
            app.open_ephemeral_oauth_webview(window, cx);
            if app.credentials.has_developer_token() && app.credentials.is_authorized() {
                app.sync_account_storefront(cx);
            }
        }

        app
    }

    fn sync_dsp_config(&mut self, cx: &mut Context<Self>) {
        self.store.save_dsp_config(&self.dsp_config);
        self.audio_engine
            .send(AudioCommand::UpdateDsp(self.dsp_config.clone()));
        cx.notify();
    }

    fn is_any_input_focused(&self, window: &Window, cx: &App) -> bool {
        match self.active_tab {
            NavTab::BrowseSearch => self.search_input.read(cx).focus_handle(cx).is_focused(window),
            NavTab::Library => self
                .local_file_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window),
            NavTab::Playlists => self
                .playlist_name_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window),
            NavTab::AuthSettings => [
                &self.dev_token_input,
                &self.user_token_input,
                &self.team_id_input,
                &self.key_id_input,
                &self.p8_pem_input,
            ]
            .into_iter()
            .any(|input| input.read(cx).focus_handle(cx).is_focused(window)),
            NavTab::ListenNow | NavTab::AudioDsp => false,
        }
    }

    fn apply_eq_preset(&mut self, preset: EqPreset, window: &mut Window, cx: &mut Context<Self>) {
        self.dsp_config.apply_preset(preset);
        let gains = self.dsp_config.bands_db;
        for (i, &g) in gains.iter().enumerate() {
            self.eq_band_sliders[i].update(cx, |s, cx| {
                s.set_value(g, window, cx);
            });
        }
        self.sync_dsp_config(cx);
    }

    fn eval_webview_js(&self, js: &str, cx: &App) {
        if let Some(ref wv_entity) = self.auth_webview {
            let _ = wv_entity.read(cx).evaluate_script(js);
        }
    }

    fn dispatch_webview_play_song(&self, catalog_id: &str, fallback_url: &str, library_id: &str, cx: &App) {
        let vol = self.audio_engine.shared.volume();
        let safe_id = catalog_id.replace('\\', "\\\\").replace('\'', "\\'");
        let safe_fb = fallback_url.replace('\\', "\\\\").replace('\'', "\\'");
        let safe_lib = library_id.replace('\\', "\\\\").replace('\'', "\\'");
        let js = format!(
            "if (window.gpuiBridge) {{ window.gpuiBridge.setVolume({vol:.3}); window.gpuiBridge.playSong('{safe_id}', '{safe_fb}', '{safe_lib}'); }}"
        );
        self.eval_webview_js(&js, cx);
    }

    fn animate_webview_spectrum(&self, phase: f32) {
        let vol = self.audio_engine.shared.volume().clamp(0.05, 1.0);
        if let Ok(mut spec) = self.audio_engine.shared.spectrum.lock() {
            for i in 0..SPECTRUM_BINS {
                let band_idx = (i * 10 / SPECTRUM_BINS).min(9);
                let eq_boost = if self.dsp_config.eq_enabled {
                    ((self.dsp_config.bands_db[band_idx] + self.dsp_config.preamp_db) * 0.03)
                        .clamp(-0.25, 0.35)
                } else {
                    0.0
                };
                let fi = i as f32;
                let wave1 = (phase * 1.3 + fi * 0.42).sin() * 0.5 + 0.5;
                let wave2 = (phase * 2.1 - fi * 0.27).cos() * 0.5 + 0.5;
                let envelope = 1.0 - (fi / (SPECTRUM_BINS as f32 * 1.25));
                let val = ((0.18 + 0.55 * wave1 * wave2) * envelope + eq_boost) * vol;
                spec[i] = val.clamp(0.05, 0.96);
            }
        }
        let peak_l = (0.52 + 0.28 * (phase * 1.7).sin().abs()) * vol;
        let peak_r = (0.50 + 0.28 * (phase * 1.9 + 0.6).cos().abs()) * vol;
        if let Ok(mut peaks) = self.audio_engine.shared.peak_levels.lock() {
            *peaks = (peak_l, peak_r);
        }
    }

    pub fn play_track(&mut self, track: Track, cx: &mut Context<Self>) {
        self.accent_hex = track.artwork_color_hex;
        self.secondary_hex = track.artwork_secondary_hex;

        let can_use_webview = self.credentials.has_developer_token()
            && self.credentials.is_authorized()
            && self.auth_webview.is_some();
        bridge::dbg_log(&format!(
            "[play] track={} catalog_id={:?} dev_token={} user_token={} webview={} ready={}",
            track.id,
            track.effective_catalog_id(),
            self.credentials.has_developer_token(),
            self.credentials.is_authorized(),
            self.auth_webview.is_some(),
            self.webview_ready
        ));

        if let (true, Some(catalog_id)) = (can_use_webview, track.effective_catalog_id()) {
            // Stop Symphonia engine and play full-length track via gpui-webview MusicKit JS v3
            self.audio_engine.send(AudioCommand::Stop);
            self.using_webview_player = true;
            self.audio_engine.shared.set_position_secs(0.05);
            self.audio_engine
                .shared
                .set_duration_secs(track.duration_secs.max(1.0));
            self.audio_engine
                .shared
                .is_playing
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.audio_engine.shared.set_status(format!(
                "Streaming Full Song via gpui-webview MusicKit JS v3 (ID: {catalog_id})"
            ));
            self.status_banner = format!(
                "Playing Full Song \"{}\" — {} (via gpui-webview MusicKit JS v3)",
                track.title, track.artist
            );

            let fallback_url = match &track.source {
                TrackSource::StreamUrl(u) => u.clone(),
                _ => String::new(),
            };
            let lib_id = track
                .id
                .strip_prefix("lib-")
                .filter(|s| s.starts_with("i."))
                .unwrap_or("")
                .to_string();
            if self.webview_ready {
                self.dispatch_webview_play_song(&catalog_id, &fallback_url, &lib_id, cx);
            } else {
                self.pending_webview_play = Some((catalog_id, fallback_url, lib_id));
            }
        } else {
            if self.using_webview_player {
                self.eval_webview_js("window.gpuiBridge && window.gpuiBridge.stop();", cx);
                self.using_webview_player = false;
            }
            self.status_banner = format!("Playing \"{}\" — {}", track.title, track.artist);
            self.audio_engine.send(AudioCommand::PlaySource {
                source: track.source.clone(),
                duration_hint: track.duration_secs,
            });
        }

        if let Some(pos) = self.queue.iter().position(|t| t.id == track.id) {
            self.queue_index = pos;
        } else {
            self.queue.push(track.clone());
            self.queue_index = self.queue.len().saturating_sub(1);
        }
        self.current_track = Some(track);
        cx.notify();
    }

    pub fn toggle_play_pause(&mut self, cx: &mut Context<Self>) {
        let status = self.audio_engine.playback_status();
        if self.using_webview_player {
            match status {
                PlaybackStatus::Playing => {
                    if self.audio_engine.shared.position_secs() < 0.05 {
                        self.audio_engine.shared.set_position_secs(0.05);
                    }
                    self.audio_engine
                        .shared
                        .is_playing
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    self.eval_webview_js("window.gpuiBridge && window.gpuiBridge.pause();", cx);
                }
                PlaybackStatus::Paused => {
                    self.audio_engine
                        .shared
                        .is_playing
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    self.eval_webview_js("window.gpuiBridge && window.gpuiBridge.play();", cx);
                }
                PlaybackStatus::Stopped => {
                    if let Some(track) = self.current_track.clone() {
                        self.play_track(track, cx);
                    } else if let Some(first) = self.library_tracks.first().cloned() {
                        self.play_track(first, cx);
                    }
                }
            }
            cx.notify();
            return;
        }

        match status {
            PlaybackStatus::Playing => {
                self.audio_engine.send(AudioCommand::Pause);
            }
            PlaybackStatus::Paused => {
                self.audio_engine.send(AudioCommand::Resume);
            }
            PlaybackStatus::Stopped => {
                if let Some(track) = self.current_track.clone() {
                    self.play_track(track, cx);
                } else if let Some(first) = self.library_tracks.first().cloned() {
                    self.play_track(first, cx);
                }
            }
        }
        cx.notify();
    }

    pub fn next_track(&mut self, cx: &mut Context<Self>) {
        if self.queue.is_empty() {
            return;
        }
        self.queue_index = (self.queue_index + 1) % self.queue.len();
        let track = self.queue[self.queue_index].clone();
        self.play_track(track, cx);
    }

    pub fn prev_track(&mut self, cx: &mut Context<Self>) {
        if self.queue.is_empty() {
            return;
        }
        if self.audio_engine.shared.position_secs() > 3.5 {
            if self.using_webview_player {
                self.audio_engine.shared.set_position_secs(0.0);
                self.eval_webview_js("window.gpuiBridge && window.gpuiBridge.seekTo(0);", cx);
            } else {
                self.audio_engine.send(AudioCommand::Seek(0.0));
            }
            cx.notify();
            return;
        }
        if self.queue_index == 0 {
            self.queue_index = self.queue.len() - 1;
        } else {
            self.queue_index -= 1;
        }
        let track = self.queue[self.queue_index].clone();
        self.play_track(track, cx);
    }

    pub fn trigger_catalog_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().to_string();
        let trimmed = query.trim().to_string();
        if trimmed.is_empty() {
            return;
        }

        // Check SQLite cache first for instant response
        if let Some(cached) = self.store.get_cached_search(&trimmed) {
            if !cached.is_empty() {
                self.search_results = cached;
                self.status_banner =
                    format!("Loaded {} cached results for \"{trimmed}\" from SQLite", self.search_results.len());
                cx.notify();
            }
        }

        self.is_searching = true;
        self.status_banner = format!("Searching Apple Music / iTunes Catalog for \"{trimmed}\"...");
        cx.notify();

        let client = self.api_client.clone();
        let creds = self.credentials.clone();
        let handle = self.tokio_rt.handle().clone();

        cx.spawn(async move |this, cx| {
            let q_clone = trimmed.clone();
            let res = handle
                .spawn(async move { client.search_catalog(&q_clone, &creds).await })
                .await;

            let _ = this.update(cx, |app, cx| {
                app.is_searching = false;
                match res {
                    Ok(Ok(tracks)) => {
                        app.store.put_cached_search(&trimmed, &tracks);
                        app.status_banner = format!(
                            "Found {} catalog tracks for \"{trimmed}\" (cached in rusqlite)",
                            tracks.len()
                        );
                        app.search_results = tracks;
                    }
                    Ok(Err(e)) => {
                        app.status_banner = format!("Catalog search notice: {e}");
                    }
                    Err(e) => {
                        app.status_banner = format!("Search task join error: {e}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Spawn the Ephemeral `gpui-webview` OAuth modal + embedded localhost bridge server.
    /// Once authorized (or closed), dropping `auth_webview` and `auth_server` frees all WebView memory.
    pub fn open_ephemeral_oauth_webview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.read_credentials_from_inputs(cx);

        if self.auth_server.is_none() {
            match EphemeralAuthServer::start(
                self.credentials.developer_token.clone(),
                self.credentials.effective_storefront().to_string(),
                self.credentials.user_token.clone(),
            ) {
                Ok(srv) => self.auth_server = Some(srv),
                Err(e) => {
                    self.status_banner = format!("Bridge server error: {e}");
                    cx.notify();
                    return;
                }
            }
        }

        let Some(ref srv) = self.auth_server else {
            return;
        };
        srv.set_context(
            self.credentials.developer_token.clone(),
            self.credentials.effective_storefront().to_string(),
            self.credentials.user_token.clone(),
        );
        let url = srv.bridge_url();
        self.webview_ready = false;

        if let Some(ref existing_wv) = self.auth_webview {
            existing_wv.update(cx, |wv, _| {
                wv.load_url(&url);
            });
            self.status_banner = format!(
                "Reloaded gpui-webview MusicKit JS v3 Player & OAuth Bridge at {url}"
            );
            cx.notify();
            return;
        }

        let tx = self.bridge_tx.clone();

        let builder = wry::WebViewBuilder::new()
            .with_url(&url)
            .with_autoplay(true)
            .with_ipc_handler(move |req| {
                let body = req.body();
                if let Ok(event) = serde_json::from_str::<BridgeEvent>(body) {
                    let _ = tx.send(event);
                } else {
                    bridge::dbg_log(&format!("[ipc] unparsed message: {}", body.chars().take(200).collect::<String>()));
                }
            })
            // Required so Apple ID OAuth popup window (`window.open`) is allowed
            .with_new_window_req_handler(|_url, _features| wry::NewWindowResponse::Allow);

        match WebView::build(builder, window, cx) {
            Ok(wv) => {
                bridge::dbg_log(&format!("[webview] built OK, loading {url}"));
                let entity = cx.new(|_| wv);
                self.auth_webview = Some(entity);
                self.status_banner = format!(
                    "gpui-webview MusicKit JS v3 Player & OAuth Bridge active at {url}"
                );
            }
            Err(err) => {
                bridge::dbg_log(&format!("[webview] build FAILED: {err}"));
                self.status_banner = format!("Failed to build gpui-webview: {err}");
            }
        }
        cx.notify();
    }

    pub fn teardown_ephemeral_webview(&mut self) {
        self.auth_webview = None;
        self.auth_server = None;
        self.webview_ready = false;
        self.using_webview_player = false;
    }

    fn read_credentials_from_inputs(&mut self, cx: &App) {
        self.credentials.developer_token = self.dev_token_input.read(cx).value().to_string();
        self.credentials.user_token = self.user_token_input.read(cx).value().to_string();
        self.credentials.team_id = self.team_id_input.read(cx).value().to_string();
        self.credentials.key_id = self.key_id_input.read(cx).value().to_string();
        self.credentials.private_key_pem = self.p8_pem_input.read(cx).value().to_string();
        if self.credentials.storefront.trim().is_empty() {
            self.credentials.storefront = models::detect_system_storefront();
        }
        self.store.save_credentials(&self.credentials);
    }

    fn apply_detected_storefront(&mut self, raw_sf: &str, cx: &mut Context<Self>) {
        let sf = raw_sf.trim().to_lowercase();
        if sf.is_empty() {
            return;
        }
        if self.credentials.storefront != sf {
            self.credentials.storefront = sf.clone();
            self.store.save_credentials(&self.credentials);
            if let Some(ref srv) = self.auth_server {
                srv.set_context(
                    self.credentials.developer_token.clone(),
                    sf.clone(),
                    self.credentials.user_token.clone(),
                );
            }
            // Refresh MusicKit JS storefrontId if WebView is active
            let safe_sf = sf.replace('\\', "\\\\").replace('\'', "\\'");
            self.eval_webview_js(
                &format!(
                    "try {{ if (typeof MusicKit !== 'undefined') MusicKit.getInstance().storefrontId = '{safe_sf}'; }} catch (_) {{}}"
                ),
                cx,
            );
            bridge::dbg_log(&format!("[storefront] dynamically updated to '{sf}' from account locale"));
            cx.notify();
        }
    }

    fn sync_account_storefront(&mut self, cx: &mut Context<Self>) {
        if !self.credentials.has_developer_token() || !self.credentials.is_authorized() {
            return;
        }
        let client = self.api_client.clone();
        let creds = self.credentials.clone();
        let handle = self.tokio_rt.handle().clone();

        cx.spawn(async move |this, cx| {
            let res = handle
                .spawn(async move { client.fetch_user_storefront(&creds).await })
                .await;
            let _ = this.update(cx, |app, cx| {
                if let Ok(Ok(sf)) = res {
                    app.apply_detected_storefront(&sf, cx);
                }
            });
        })
        .detach();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = rgb(self.accent_hex);
        let is_auth = self.credentials.is_authorized();
        let webview_alive = self.auth_webview.is_some();

        let nav_items = [
            (NavTab::ListenNow, IconName::Play, "Listen Now"),
            (NavTab::BrowseSearch, IconName::Search, "Browse & Search"),
            (NavTab::Library, IconName::Folder, "Library"),
            (NavTab::Playlists, IconName::Star, "Playlists"),
            (NavTab::AudioDsp, IconName::Settings2, "10-Band EQ & DSP"),
            (NavTab::AuthSettings, IconName::User, "MusicKit & Auth"),
        ];

        v_flex()
            .w(px(236.0))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .p_3()
            .justify_between()
            .child(
                v_flex()
                    .gap_4()
                    .child(
                        // Brand header
                        h_flex()
                            .items_center()
                            .gap_2p5()
                            .px_2()
                            .pt_1()
                            .child(
                                div()
                                    .size(px(32.0))
                                    .rounded(px(8.0))
                                    .bg(accent)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(Icon::new(IconName::Play).text_color(gpui_kit::white())),
                            )
                            .child(
                                v_flex()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_semibold()
                                            .text_color(cx.theme().foreground)
                                            .child("Apple Music"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("GPUI Native + DSP"),
                                    ),
                            ),
                    )
                    .child(
                        // Navigation links
                        v_flex().gap_1().children(nav_items.into_iter().map(
                            |(tab, icon, label)| {
                                let active = self.active_tab == tab;
                                Button::new(("nav-tab", tab as usize))
                                    .w_full()
                                    .justify_start()
                                    .icon(icon)
                                    .label(label)
                                    .when(active, |b| b.primary())
                                    .when(!active, |b| b.ghost())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.active_tab = tab;
                                        this.focus_handle.focus(window, cx);
                                        if tab == NavTab::BrowseSearch
                                            && this.search_results.is_empty()
                                            && !this.is_searching
                                        {
                                            this.trigger_catalog_search(cx);
                                        }
                                        cx.notify();
                                    }))
                            },
                        )),
                    ),
            )
            .child(
                // Footprint & Engine Telemetry Card
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(px(10.0))
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(cx.theme().foreground)
                                    .child("Hybrid Architecture"),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(px(4.0))
                                    .bg(if webview_alive {
                                        rgb(0xf59e0b)
                                    } else {
                                        rgb(0x10b981)
                                    })
                                    .text_xs()
                                    .text_color(gpui_kit::white())
                                    .child(if webview_alive {
                                        "WebView Active"
                                    } else {
                                        "~38 MB Idle"
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if webview_alive && is_auth {
                                "gpui-webview Full-Song DRM Player"
                            } else if webview_alive {
                                "gpui-webview open for MusicKit OAuth"
                            } else {
                                "WebView unloaded (Symphonia DSP only)"
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if is_auth {
                                "Auth: Full Subscription Playback"
                            } else {
                                "Auth: Public / Preview Mode"
                            }),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .pt_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Dark Theme"),
                            )
                            .child(
                                Switch::new("theme-mode-switch")
                                    .checked(self.dark_mode)
                                    .on_change(cx.listener(|this, checked: &bool, window, cx| {
                                        this.dark_mode = *checked;
                                        let mode = if this.dark_mode {
                                            ThemeMode::Dark
                                        } else {
                                            ThemeMode::Light
                                        };
                                        Theme::change(mode, Some(window), cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    fn render_listen_now(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = rgb(self.accent_hex);
        let secondary = rgb(self.secondary_hex);

        v_flex()
            .gap_5()
            .w_full()
            // Hero Now Playing + Real-Time FFT Visualizer Card
            .child(
                v_flex()
                    .w_full()
                    .p_5()
                    .rounded(px(16.0))
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .gap_4()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_start()
                            .gap_4()
                            .child(
                                h_flex()
                                    .gap_4()
                                    .items_center()
                                    .child(
                                        // Dynamic Album Artwork Tile
                                        div()
                                            .size(px(88.0))
                                            .rounded(px(14.0))
                                            .bg(accent)
                                            .border_2()
                                            .border_color(secondary)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                Icon::new(IconName::Play)
                                                    .size(px(32.0))
                                                    .text_color(gpui_kit::white()),
                                            ),
                                    )
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .items_center()
                                                    .child(
                                                        div()
                                                            .px_2()
                                                            .py_0p5()
                                                            .rounded(px(4.0))
                                                            .bg(accent)
                                                            .text_xs()
                                                            .font_semibold()
                                                            .text_color(gpui_kit::white())
                                                            .child("NATIVE RUST DSP"),
                                                    )
                                                    .when(
                                                        self.current_track
                                                            .as_ref()
                                                            .map(|t| t.is_lossless)
                                                            .unwrap_or(false),
                                                        |el| {
                                                            el.child(
                                                                div()
                                                                    .px_2()
                                                                    .py_0p5()
                                                                    .rounded(px(4.0))
                                                                    .bg(cx.theme().background)
                                                                    .border_1()
                                                                    .border_color(cx.theme().border)
                                                                    .text_xs()
                                                                    .text_color(
                                                                        cx.theme().foreground,
                                                                    )
                                                                    .child("LOSSLESS 48kHz F32"),
                                                            )
                                                        },
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_xl()
                                                    .font_bold()
                                                    .text_color(cx.theme().foreground)
                                                    .child(
                                                        self.current_track
                                                            .as_ref()
                                                            .map(|t| t.title.clone())
                                                            .unwrap_or_else(|| {
                                                                "Select a Track".to_string()
                                                            }),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(
                                                        self.current_track
                                                            .as_ref()
                                                            .map(|t| {
                                                                format!(
                                                                    "{} — {} ({})",
                                                                    t.artist,
                                                                    t.album,
                                                                    t.release_year
                                                                )
                                                            })
                                                            .unwrap_or_default(),
                                                    ),
                                            ),
                                    ),
                            )
                            .child(
                                // Quick DSP status badges
                                v_flex()
                                    .items_end()
                                    .gap_1p5()
                                    .child(
                                        Button::new("jump-to-eq")
                                            .small()
                                            .secondary()
                                            .icon(IconName::Settings2)
                                            .label(format!(
                                                "EQ: {} | Spatial: {}",
                                                self.dsp_config.preset.name(),
                                                if self.dsp_config.spatial_enabled {
                                                    "ON"
                                                } else {
                                                    "OFF"
                                                }
                                            ))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.active_tab = NavTab::AudioDsp;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    // Standalone SpectrumVisualizerView Entity (invalidated independently at 11 FPS)
                    .child(self.spectrum_view.clone()),
            )
            // Featured Native DSP & Lossless Reference Tracks Shelf
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_base()
                                    .font_semibold()
                                    .text_color(cx.theme().foreground)
                                    .child("Featured Reference Sessions & Library"),
                            )
                            .child(
                                Button::new("explore-catalog-btn")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Search)
                                    .label("Search Apple Music Catalog")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.active_tab = NavTab::BrowseSearch;
                                        if this.search_results.is_empty() && !this.is_searching {
                                            this.trigger_catalog_search(cx);
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(self.render_track_table(&self.library_tracks, "listen-now", cx)),
            )
    }

    fn render_browse_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let quick_queries = [
            "Daft Punk",
            "The Weeknd",
            "YOASOBI",
            "Taylor Swift",
            "Hans Zimmer",
            "Billie Eilish",
        ];

        v_flex()
            .gap_4()
            .w_full()
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(px(12.0))
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_base()
                            .font_semibold()
                            .text_color(cx.theme().foreground)
                            .child("Apple Music & iTunes Catalog Search"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Uses Apple Music REST API (/v1/catalog/{storefront}/search) when a Developer JWT is configured, or falls back to Apple iTunes 256kbps AAC previews automatically. Results are cached locally in rusqlite."),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .w_full()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.search_input).cleanable(true)),
                            )
                            .child(
                                Button::new("run-catalog-search")
                                    .primary()
                                    .icon(IconName::Search)
                                    .label(if self.is_searching {
                                        "Searching..."
                                    } else {
                                        "Search"
                                    })
                                    .disabled(self.is_searching)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.trigger_catalog_search(cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .flex_wrap()
                            .children(quick_queries.into_iter().enumerate().map(|(idx, q)| {
                                Button::new(("quick-q", idx))
                                    .small()
                                    .secondary()
                                    .label(q)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.search_input.update(cx, |s, cx| {
                                            s.set_value(q, window, cx);
                                        });
                                        this.trigger_catalog_search(cx);
                                    }))
                            })),
                    ),
            )
            .child(if self.search_results.is_empty() {
                div()
                    .p_6()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.is_searching {
                        "Fetching catalog tracks via reqwest..."
                    } else {
                        "Enter an artist or song above or click one of the quick tags to stream AAC previews through Symphonia + 10-band EQ."
                    })
                    .into_any_element()
            } else {
                self.render_track_table(&self.search_results, "catalog-search", cx)
                    .into_any_element()
            })
    }

    fn render_library_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let table = self.render_track_table(&self.library_tracks, "library-table", cx);
        v_flex()
            .gap_4()
            .w_full()
            .child(
                v_flex()
                    .gap_2p5()
                    .p_4()
                    .rounded(px(12.0))
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                v_flex()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_semibold()
                                            .text_color(cx.theme().foreground)
                                            .child("Local SQLite Library & Lossless File Importer"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Import local FLAC / ALAC / AAC / MP3 / WAV files or sync your Apple Music cloud library."),
                                    ),
                            )
                            .child(
                                Button::new("sync-cloud-lib-btn")
                                    .small()
                                    .secondary()
                                    .icon(IconName::RefreshCw)
                                    .label("Sync /v1/me/library/songs")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sync_cloud_library(cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.local_file_input).cleanable(true)),
                            )
                            .child(
                                Button::new("add-local-file-btn")
                                    .primary()
                                    .icon(IconName::Plus)
                                    .label("Add Local File")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let path =
                                            this.local_file_input.read(cx).value().to_string();
                                        let trimmed = path.trim();
                                        if trimmed.is_empty() {
                                            return;
                                        }
                                        let filename = std::path::Path::new(trimmed)
                                            .file_stem()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("Local Audio Track")
                                            .to_string();
                                        let (c1, c2) = palette_for_string(&filename);
                                        let track = Track {
                                            id: format!("local-{}", this.library_tracks.len() + 1),
                                            title: filename,
                                            artist: "Local Lossless File".to_string(),
                                            album: "Local Disk".to_string(),
                                            genre: "Lossless Audio".to_string(),
                                            duration_secs: 180.0,
                                            artwork_color_hex: c1,
                                            artwork_secondary_hex: c2,
                                            source: TrackSource::LocalFile(trimmed.to_string()),
                                            is_lossless: true,
                                            release_year: 2026,
                                            catalog_id: None,
                                        };
                                        this.store.save_track(&track);
                                        this.library_tracks.push(track.clone());
                                        this.local_file_input.update(cx, |s, cx| {
                                            s.set_value("", window, cx);
                                        });
                                        this.status_banner = format!(
                                            "Added local track \"{}\" to SQLite library.",
                                            track.title
                                        );
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(table)
    }

    fn sync_cloud_library(&mut self, cx: &mut Context<Self>) {
        self.read_credentials_from_inputs(cx);
        if !self.credentials.has_developer_token() || !self.credentials.is_authorized() {
            self.status_banner =
                "Configure a Developer Token & sign in via Ephemeral WebView OAuth first to sync /v1/me/library/songs."
                    .to_string();
            cx.notify();
            return;
        }

        let client = self.api_client.clone();
        let creds = self.credentials.clone();
        let handle = self.tokio_rt.handle().clone();
        self.status_banner = "Syncing /v1/me/library/songs from Apple Music...".to_string();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let res = handle
                .spawn(async move { client.fetch_user_library_songs(&creds).await })
                .await;
            let _ = this.update(cx, |app, cx| {
                match res {
                    Ok(Ok(tracks)) => {
                        for t in &tracks {
                            app.store.save_track(t);
                            if !app.library_tracks.iter().any(|existing| existing.id == t.id) {
                                app.library_tracks.push(t.clone());
                            }
                        }
                        app.status_banner =
                            format!("Synced {} songs from Apple Music Cloud Library.", tracks.len());
                    }
                    Ok(Err(e)) => {
                        app.status_banner = format!("Cloud Library sync notice: {e}");
                    }
                    Err(e) => {
                        app.status_banner = format!("Cloud Library task error: {e}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_playlists_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_pl = self
            .playlists
            .iter()
            .find(|p| Some(&p.id) == self.selected_playlist_id.as_ref())
            .or_else(|| self.playlists.first());

        let playlist_tracks: Vec<Track> = selected_pl
            .map(|pl| {
                pl.track_ids
                    .iter()
                    .filter_map(|id| self.library_tracks.iter().find(|t| &t.id == id).cloned())
                    .collect()
            })
            .unwrap_or_default();

        v_flex()
            .gap_4()
            .w_full()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(280.0))
                            .child(Input::new(&self.playlist_name_input).cleanable(true)),
                    )
                    .child(
                        Button::new("create-playlist-btn")
                            .primary()
                            .icon(IconName::Plus)
                            .label("Create Playlist")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let name =
                                    this.playlist_name_input.read(cx).value().to_string();
                                let trimmed = name.trim();
                                if trimmed.is_empty() {
                                    return;
                                }
                                let (c1, _) = palette_for_string(trimmed);
                                let pl = Playlist {
                                    id: format!("pl-{}", this.playlists.len() + 1),
                                    name: trimmed.to_string(),
                                    description: "Custom GPUI Playlist persisted in rusqlite"
                                        .to_string(),
                                    track_ids: this
                                        .library_tracks
                                        .iter()
                                        .take(2)
                                        .map(|t| t.id.clone())
                                        .collect(),
                                    accent_hex: c1,
                                };
                                this.store.save_playlist(&pl);
                                this.selected_playlist_id = Some(pl.id.clone());
                                this.playlists.push(pl);
                                this.playlist_name_input.update(cx, |s, cx| {
                                    s.set_value("", window, cx);
                                });
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children(self.playlists.iter().enumerate().map(|(idx, pl)| {
                        let is_sel = Some(&pl.id) == self.selected_playlist_id.as_ref();
                        let pl_id = pl.id.clone();
                        Button::new(("sel-pl", idx))
                            .when(is_sel, |b| b.primary())
                            .when(!is_sel, |b| b.secondary())
                            .icon(IconName::Star)
                            .label(format!("{} ({})", pl.name, pl.track_ids.len()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_playlist_id = Some(pl_id.clone());
                                cx.notify();
                            }))
                    })),
            )
            .when_some(selected_pl, |el, pl| {
                el.child(
                    v_flex()
                        .gap_2()
                        .p_4()
                        .rounded(px(12.0))
                        .bg(cx.theme().secondary)
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(
                            div()
                                .text_lg()
                                .font_bold()
                                .text_color(cx.theme().foreground)
                                .child(pl.name.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(pl.description.clone()),
                        ),
                )
            })
            .child(self.render_track_table(&playlist_tracks, "playlist-tracks", cx))
    }

    fn render_dsp_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = rgb(self.accent_hex);

        v_flex()
            .gap_5()
            .w_full()
            // Top bar: EQ Enable + Presets
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(px(12.0))
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                v_flex()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_semibold()
                                            .text_color(cx.theme().foreground)
                                            .child("In-Process 10-Band Biquad Parametric EQ (DirectForm2Transposed)"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Zero-latency f32 IIR filtering inside the cpal real-time audio callback thread."),
                                    ),
                            )
                            .child(
                                Switch::new("eq-master-switch")
                                    .label("Enable 10-Band EQ")
                                    .checked(self.dsp_config.eq_enabled)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.dsp_config.eq_enabled = *checked;
                                        this.sync_dsp_config(cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .children(EqPreset::ALL.into_iter().enumerate().map(|(idx, preset)| {
                                let active = self.dsp_config.preset == preset;
                                Button::new(("eq-preset-btn", idx))
                                    .small()
                                    .when(active, |b| b.primary())
                                    .when(!active, |b| b.secondary())
                                    .label(preset.name())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.apply_eq_preset(preset, window, cx);
                                    }))
                            })),
                    )
                    // Pre-amp slider
                    .child(
                        h_flex()
                            .gap_4()
                            .items_center()
                            .pt_1()
                            .child(
                                div()
                                    .w(px(110.0))
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(cx.theme().foreground)
                                    .child(format!("Pre-amp: {:+.1} dB", self.dsp_config.preamp_db)),
                            )
                            .child(div().flex_1().child(Slider::new(&self.preamp_slider))),
                    ),
            )
            // 10-Band Vertical Sliders Rack
            .child(
                h_flex()
                    .w_full()
                    .p_5()
                    .rounded(px(12.0))
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .justify_between()
                    .gap_2()
                    .children((0..10).map(|i| {
                        let gain = self.dsp_config.bands_db[i];
                        let label = EQ_BAND_LABELS[i];
                        let freq = EQ_BAND_FREQS[i];
                        v_flex()
                            .flex_1()
                            .items_center()
                            .gap_2()
                            .p_2()
                            .rounded(px(8.0))
                            .bg(cx.theme().background)
                            .border_1()
                            .border_color(cx.theme().border)
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(if gain.abs() > 0.1 {
                                        Hsla::from(accent)
                                    } else {
                                        cx.theme().muted_foreground
                                    })
                                    .child(format!("{:+.1}dB", gain)),
                            )
                            .child(
                                div()
                                    .h(px(140.0))
                                    .flex()
                                    .justify_center()
                                    .child(Slider::new(&self.eq_band_sliders[i]).vertical()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_bold()
                                    .text_color(cx.theme().foreground)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if freq < 40.0 {
                                        "LowShelf"
                                    } else if freq > 14000.0 {
                                        "HighShelf"
                                    } else {
                                        "Peak"
                                    }),
                            )
                    })),
            )
            // Spatial Audio / Stereo Crossfeed & Room Stage
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(px(12.0))
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                v_flex()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_semibold()
                                            .text_color(cx.theme().foreground)
                                            .child("Spatialization & Stereo Crossfeed Stage"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Simulates natural speaker interaural delay (~320µs low-passed contralateral feed) + early room reflections."),
                                    ),
                            )
                            .child(
                                Switch::new("spatial-switch")
                                    .label("Enable Spatial Stage")
                                    .checked(self.dsp_config.spatial_enabled)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.dsp_config.spatial_enabled = *checked;
                                        this.sync_dsp_config(cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .items_center()
                            .child(
                                div()
                                    .w(px(170.0))
                                    .text_xs()
                                    .text_color(cx.theme().foreground)
                                    .child(format!(
                                        "Crossfeed Mix: {:.0}%",
                                        self.dsp_config.crossfeed_amount * 100.0
                                    )),
                            )
                            .child(div().flex_1().child(Slider::new(&self.crossfeed_slider))),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .items_center()
                            .child(
                                div()
                                    .w(px(170.0))
                                    .text_xs()
                                    .text_color(cx.theme().foreground)
                                    .child(format!(
                                        "Stereo Width & Room: {:.0}%",
                                        self.dsp_config.room_width * 100.0
                                    )),
                            )
                            .child(div().flex_1().child(Slider::new(&self.room_slider))),
                    ),
            )
    }

    fn render_auth_settings_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let webview_open = self.auth_webview.is_some();

        v_flex()
            .gap_4()
            .w_full()
            // Ephemeral WebView OAuth Card
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(px(12.0))
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                v_flex()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_semibold()
                                            .text_color(cx.theme().foreground)
                                            .child("gpui-webview MusicKit JS v3 Full-Song DRM Player & OAuth"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Hosts MusicKit JS v3 inside native WebKit EME CDM to decrypt and stream full-length Apple Music subscription songs, with local /amp-proxy origin bridging."),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("spawn-oauth-wv")
                                            .primary()
                                            .icon(IconName::Globe)
                                            .label(if webview_open {
                                                "Reload MusicKit WebView"
                                            } else {
                                                "Start MusicKit WebView Player & OAuth"
                                            })
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.open_ephemeral_oauth_webview(window, cx);
                                            })),
                                    )
                                    .when(webview_open, |el| {
                                        el.child(
                                            Button::new("destroy-oauth-wv")
                                                .danger()
                                                .icon(IconName::Close)
                                                .label("Drop WebView Entity Now")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.teardown_ephemeral_webview();
                                                    this.status_banner =
                                                        "Dropped Entity<WebView> and stopped bridge server — WebKitWebProcess freed."
                                                            .to_string();
                                                    cx.notify();
                                                })),
                                        )
                                    }),
                            ),
                    )
                    .when_some(self.auth_webview.clone(), |el, wv| {
                        el.child(
                            div()
                                .w_full()
                                .h(px(340.0))
                                .rounded(px(10.0))
                                .overflow_hidden()
                                .border_1()
                                .border_color(cx.theme().border)
                                .child(wv),
                        )
                    }),
            )
            // Credentials & ES256 JWT Generator Card
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(px(12.0))
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_base()
                                    .font_semibold()
                                    .text_color(cx.theme().foreground)
                                    .child("Apple Music Tokens & ES256 Developer JWT Generator"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(6.0))
                                    .bg(cx.theme().background)
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "Account Storefront: {} (Auto)",
                                        self.credentials.effective_storefront().to_uppercase()
                                    )),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Developer Token (JWT)"),
                            )
                            .child(Input::new(&self.dev_token_input).cleanable(true)),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Media-User-Token (Captured from MusicKit.authorize() & stored in OS Keyring)"),
                            )
                            .child(Input::new(&self.user_token_input).cleanable(true)),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Apple Team ID (iss)"),
                                    )
                                    .child(Input::new(&self.team_id_input)),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("MusicKit Key ID (kid)"),
                                    )
                                    .child(Input::new(&self.key_id_input)),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("MusicKit Private Key (.p8 PEM content for ES256 signing)"),
                            )
                            .child(Input::new(&self.p8_pem_input).cleanable(true)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .pt_1()
                            .child(
                                Button::new("save-creds-btn")
                                    .primary()
                                    .icon(IconName::Check)
                                    .label("Save Tokens to Keyring & SQLite")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.read_credentials_from_inputs(cx);
                                        this.open_ephemeral_oauth_webview(window, cx);
                                        this.status_banner =
                                            "Saved Apple Music credentials & configured gpui-webview Full-Song DRM Player."
                                                .to_string();
                                        if this.credentials.has_developer_token() && this.credentials.is_authorized() {
                                            this.sync_account_storefront(cx);
                                            this.sync_cloud_library(cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("gen-jwt-btn")
                                    .secondary()
                                    .icon(IconName::Cpu)
                                    .label("Generate ES256 JWT from .p8")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.read_credentials_from_inputs(cx);
                                        match this.credentials.generate_es256_jwt() {
                                            Ok(jwt) => {
                                                this.dev_token_input.update(cx, |s, cx| {
                                                    s.set_value(jwt, window, cx);
                                                });
                                                this.store.save_credentials(&this.credentials);
                                                this.status_banner =
                                                    "Generated ES256 Developer JWT and saved to Keyring!"
                                                        .to_string();
                                            }
                                            Err(err) => {
                                                this.status_banner =
                                                    format!("ES256 JWT generation error: {err}");
                                            }
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    fn render_track_table(
        &self,
        tracks: &[Track],
        table_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let current_id = self.current_track.as_ref().map(|t| t.id.clone());
        let accent_hex = self.accent_hex;
        let tracks_vec = tracks.to_vec();
        let lib_ids: std::collections::HashSet<String> =
            self.library_tracks.iter().map(|l| l.id.clone()).collect();

        v_flex()
            .w_full()
            .rounded(px(12.0))
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .children(tracks_vec.into_iter().enumerate().map(move |(idx, track)| {
                let is_playing_this = current_id.as_deref() == Some(track.id.as_str());
                let t_for_play = track.clone();
                let t_for_save = track.clone();
                let t_for_remove = track.clone();
                let in_lib = lib_ids.contains(&track.id);

                h_flex()
                    .w_full()
                    .px_4()
                    .py_2p5()
                    .items_center()
                    .justify_between()
                    .bg(if is_playing_this {
                        cx.theme().secondary
                    } else {
                        cx.theme().background
                    })
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .size(px(36.0))
                                    .rounded(px(8.0))
                                    .bg(rgb(track.artwork_color_hex))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(if is_playing_this {
                                            IconName::Pause
                                        } else {
                                            IconName::Play
                                        })
                                        .size(px(16.0))
                                        .text_color(gpui_kit::white()),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_semibold()
                                            .text_color(if is_playing_this {
                                                Hsla::from(rgb(accent_hex))
                                            } else {
                                                cx.theme().foreground
                                            })
                                            .child(track.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "{} • {} • {}",
                                                track.artist, track.album, track.genre
                                            )),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format_mm_ss(track.duration_secs)),
                            )
                            .when(!in_lib, |el| {
                                el.child(
                                    Button::new((table_prefix, idx * 10 + 1))
                                        .small()
                                        .ghost()
                                        .icon(IconName::Plus)
                                        .label("Save")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.store.save_track(&t_for_save);
                                            if !this
                                                .library_tracks
                                                .iter()
                                                .any(|x| x.id == t_for_save.id)
                                            {
                                                this.library_tracks.push(t_for_save.clone());
                                            }
                                            this.status_banner = format!(
                                                "Saved \"{}\" to local rusqlite library.",
                                                t_for_save.title
                                            );
                                            cx.notify();
                                        })),
                                )
                            })
                            .when(in_lib && table_prefix == "library-table", |el| {
                                el.child(
                                    Button::new((table_prefix, idx * 10 + 2))
                                        .small()
                                        .ghost()
                                        .icon(IconName::Minus)
                                        .label("Remove")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.store.remove_track(&t_for_remove.id);
                                            this.library_tracks.retain(|x| x.id != t_for_remove.id);
                                            this.status_banner = format!(
                                                "Removed \"{}\" from local rusqlite library.",
                                                t_for_remove.title
                                            );
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new((table_prefix, idx * 10))
                                    .small()
                                    .when(is_playing_this, |b| b.primary())
                                    .when(!is_playing_this, |b| b.secondary())
                                    .icon(IconName::Play)
                                    .label(if is_playing_this { "Playing" } else { "Play" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.QueueAndPlay(t_for_play.clone(), cx);
                                    })),
                            ),
                    )
            }))
    }

    #[allow(non_snake_case)]
    fn QueueAndPlay(&mut self, track: Track, cx: &mut Context<Self>) {
        self.play_track(track, cx);
    }

    fn render_transport_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.audio_engine.playback_status();
        let pos = self.audio_engine.shared.position_secs();
        let dur = self
            .audio_engine
            .shared
            .duration_secs()
            .max(self.current_track.as_ref().map(|t| t.duration_secs).unwrap_or(1.0));
        let accent = rgb(self.accent_hex);

        h_flex()
            .w_full()
            .h(px(80.0))
            .flex_shrink_0()
            .px_5()
            .bg(cx.theme().sidebar)
            .border_t_1()
            .border_color(cx.theme().border)
            .items_center()
            .justify_between()
            .gap_6()
            // Left: Now Playing Track Info
            .child(
                h_flex()
                    .w(px(270.0))
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .size(px(48.0))
                            .rounded(px(10.0))
                            .bg(accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(IconName::Play)
                                    .size(px(20.0))
                                    .text_color(gpui_kit::white()),
                            ),
                    )
                    .child(
                        v_flex()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(cx.theme().foreground)
                                    .truncate()
                                    .child(
                                        self.current_track
                                            .as_ref()
                                            .map(|t| t.title.clone())
                                            .unwrap_or_else(|| "Not Playing".to_string()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(
                                        self.current_track
                                            .as_ref()
                                            .map(|t| t.artist.clone())
                                            .unwrap_or_else(|| "Select any track".to_string()),
                                    ),
                            ),
                    ),
            )
            // Center: Transport Controls + Scrubber
            .child(
                v_flex()
                    .flex_1()
                    .max_w(px(540.0))
                    .gap_1p5()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                Button::new("prev-btn")
                                    .small()
                                    .ghost()
                                    .icon(IconName::ChevronLeft)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.prev_track(cx);
                                    })),
                            )
                            .child(
                                Button::new("play-pause-btn")
                                    .primary()
                                    .icon(if status == PlaybackStatus::Playing {
                                        IconName::Pause
                                    } else {
                                        IconName::Play
                                    })
                                    .label(if status == PlaybackStatus::Playing {
                                        "Pause"
                                    } else {
                                        "Play"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_play_pause(cx);
                                    })),
                            )
                            .child(
                                Button::new("next-btn")
                                    .small()
                                    .ghost()
                                    .icon(IconName::ChevronRight)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.next_track(cx);
                                    })),
                            )
                            .child(
                                Button::new("stop-btn")
                                    .small()
                                    .ghost()
                                    .label("Stop")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.using_webview_player {
                                            this.eval_webview_js(
                                                "window.gpuiBridge && window.gpuiBridge.stop();",
                                                cx,
                                            );
                                            this.using_webview_player = false;
                                        }
                                        this.audio_engine.send(AudioCommand::Stop);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2p5()
                            .items_center()
                            .child(
                                div()
                                    .w(px(40.0))
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format_mm_ss(pos)),
                            )
                            .child(div().flex_1().child(Slider::new(&self.scrubber_slider)))
                            .child(
                                div()
                                    .w(px(40.0))
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format_mm_ss(dur)),
                            ),
                    ),
            )
            // Right: Volume & Quick EQ Button
            .child(
                h_flex()
                    .w(px(230.0))
                    .justify_end()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "Vol {:.0}%",
                                self.audio_engine.shared.volume() * 100.0
                            )),
                    )
                    .child(div().w(px(100.0)).child(Slider::new(&self.volume_slider)))
                    .child(
                        Button::new("footer-eq-btn")
                            .small()
                            .when(self.active_tab == NavTab::AudioDsp, |b| b.primary())
                            .when(self.active_tab != NavTab::AudioDsp, |b| b.secondary())
                            .icon(IconName::Settings2)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.active_tab = NavTab::AudioDsp;
                                cx.notify();
                            })),
                    )
                    // Keep gpui-webview mounted in a 1x1 pixel slot when not on AuthSettings
                    // so WKWebView shrinks off-screen when switching tabs and stays active for full-song playback!
                    .when(self.active_tab != NavTab::AuthSettings, |el| {
                        el.when_some(self.auth_webview.clone(), |el, wv| {
                            el.child(
                                div()
                                    .size(px(1.0))
                                    .overflow_hidden()
                                    .opacity(0.0)
                                    .child(wv),
                            )
                        })
                    }),
            )
    }
}

impl Render for AppleMusicApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Tell the audio callback whether the FFT spectrum visualizer is currently visible on screen
        self.audio_engine.shared.spectrum_active.store(
            self.active_tab == NavTab::ListenNow,
            std::sync::atomic::Ordering::Relaxed,
        );

        // Keep scrubber slider synced with audio thread position (at 1 Hz when AppleMusicApp renders)
        let pos = self.audio_engine.shared.position_secs();
        let dur = self.audio_engine.shared.duration_secs().max(1.0);
        let pct = ((pos / dur) * 100.0).clamp(0.0, 100.0) as f32;
        if (self.scrubber_slider.read(cx).value().end() - pct).abs() >= 1.5 {
            self.updating_scrubber_from_audio = true;
            self.scrubber_slider.update(cx, |s, cx| {
                s.set_value(pct, window, cx);
            });
            self.updating_scrubber_from_audio = false;
        }

        let audio_status = self
            .audio_engine
            .shared
            .status_text
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();

        v_flex()
            .key_context("AppleMusicRoot")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &TogglePlayPause, window, cx| {
                if this.is_any_input_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                this.toggle_play_pause(cx);
            }))
            .on_action(cx.listener(|this, _: &NextTrack, window, cx| {
                if this.is_any_input_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                this.next_track(cx);
            }))
            .on_action(cx.listener(|this, _: &PrevTrack, window, cx| {
                if this.is_any_input_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                this.prev_track(cx);
            }))
            .on_action(cx.listener(|_, _: &QuitApp, _, cx| {
                cx.quit();
            }))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                // Main Content Area (Sidebar + Active View)
                h_flex()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(self.render_sidebar(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .p_6()
                            .gap_4()
                            .overflow_y_scrollbar()
                            // Header + Status Telemetry Bar
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .items_center()
                                    .pb_2()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .child(
                                        v_flex()
                                            .child(
                                                div()
                                                    .text_2xl()
                                                    .font_bold()
                                                    .text_color(cx.theme().foreground)
                                                    .child(self.active_tab.title()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(self.status_banner.clone()),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .px_2p5()
                                            .py_1()
                                            .rounded(px(6.0))
                                            .bg(cx.theme().secondary)
                                            .border_1()
                                            .border_color(cx.theme().border)
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(audio_status),
                                    ),
                            )
                            // Active Tab Body
                            .child(match self.active_tab {
                                NavTab::ListenNow => self.render_listen_now(cx).into_any_element(),
                                NavTab::BrowseSearch => {
                                    self.render_browse_search(cx).into_any_element()
                                }
                                NavTab::Library => self.render_library_tab(cx).into_any_element(),
                                NavTab::Playlists => {
                                    self.render_playlists_tab(cx).into_any_element()
                                }
                                NavTab::AudioDsp => self.render_dsp_tab(cx).into_any_element(),
                                NavTab::AuthSettings => {
                                    self.render_auth_settings_tab(cx).into_any_element()
                                }
                            }),
                    ),
            )
            // Bottom Transport & Scrubber Bar
            .child(self.render_transport_bar(cx))
    }
}

fn format_mm_ss(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let m = total / 60;
    let s = total % 60;
    format!("{m}:{s:02}")
}

/// Filters out the benign macOS AppKit / InputMethodKit `IMKCFRunLoopWakeUpReliable` stderr warning
/// emitted by `HIToolbox` when `GPUIView` (`NSTextInputClient`) handles mouse/key events.
#[cfg(target_os = "macos")]
fn silence_macos_imk_stderr_noise() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::io::FromRawFd;

    unsafe extern "C" {
        fn pipe(fds: *mut i32) -> i32;
        fn dup(fd: i32) -> i32;
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }

    unsafe {
        let mut fds = [0i32; 2];
        if pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        let orig_stderr = dup(2);
        if orig_stderr < 0 {
            close(fds[0]);
            close(fds[1]);
            return;
        }
        if dup2(fds[1], 2) < 0 {
            close(fds[0]);
            close(fds[1]);
            close(orig_stderr);
            return;
        }
        close(fds[1]);

        let _ = std::thread::Builder::new()
            .name("macos-imk-stderr-filter".to_string())
            .spawn(move || {
                let reader = BufReader::new(std::fs::File::from_raw_fd(fds[0]));
                let mut err_out = std::fs::File::from_raw_fd(orig_stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if !line.contains("IMKCFRunLoopWakeUpReliable") {
                        let _ = writeln!(err_out, "{line}");
                        let _ = err_out.flush();
                    }
                }
            });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    silence_macos_imk_stderr_noise();

    #[cfg(target_os = "linux")]
    let app = gpui_kit::platform::linux(gpui_kit::platform::WindowingModes::X11);
    #[cfg(not(target_os = "linux"))]
    let app = gpui_kit::application();

    app.with_assets(AllAssets).run(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);

        cx.bind_keys([
            KeyBinding::new("space", TogglePlayPause, Some("AppleMusicRoot && !Input")),
            KeyBinding::new("cmd-right", NextTrack, Some("AppleMusicRoot && !Input")),
            KeyBinding::new("cmd-left", PrevTrack, Some("AppleMusicRoot && !Input")),
            KeyBinding::new("cmd-q", QuitApp, None),
        ]);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1180.0), px(780.0)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Apple Music — GPUI Native Client".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            ..Default::default()
        };

        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| AppleMusicApp::new(window, cx))
        })
        .expect("Failed to open Apple Music GPUI window");
    });
}
