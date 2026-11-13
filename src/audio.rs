use crate::models::{DspConfig, EQ_BAND_FREQS, PlaybackStatus, TrackSource};
use biquad::{Biquad, Coefficients, DirectForm2Transposed, ToHertz, Type as FilterType};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use rustfft::{FftPlanner, num_complex::Complex};
use std::io::Cursor;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub const SPECTRUM_BINS: usize = 36;
const FFT_WINDOW_SIZE: usize = 1024;

pub enum AudioCommand {
    PlaySource {
        source: TrackSource,
        duration_hint: f64,
    },
    Pause,
    Resume,
    Stop,
    Seek(f64),
    SetVolume(f32),
    UpdateDsp(DspConfig),
}

/// Shared lock-free / low-contention state read by GPUI's render loop at 60–120 FPS
pub struct AudioSharedState {
    pub is_playing: AtomicBool,
    pub flush_requested: AtomicBool,
    pub spectrum_active: AtomicBool,
    pub position_bits: AtomicU32, // f32 seconds bitcast
    pub duration_bits: AtomicU32, // f32 seconds bitcast
    pub volume_bits: AtomicU32,   // f32 0.0..1.0 bitcast
    pub sample_rate: AtomicU32,
    pub spectrum: Mutex<[f32; SPECTRUM_BINS]>,
    pub peak_levels: Mutex<(f32, f32)>, // Left, Right VU meter levels
    pub status_text: Mutex<String>,
}

impl AudioSharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            is_playing: AtomicBool::new(false),
            flush_requested: AtomicBool::new(false),
            spectrum_active: AtomicBool::new(true),
            position_bits: AtomicU32::new(0.0_f32.to_bits()),
            duration_bits: AtomicU32::new(0.0_f32.to_bits()),
            volume_bits: AtomicU32::new(0.85_f32.to_bits()),
            sample_rate: AtomicU32::new(48_000),
            spectrum: Mutex::new([0.0; SPECTRUM_BINS]),
            peak_levels: Mutex::new((0.0, 0.0)),
            status_text: Mutex::new("Native Audio Pipeline Ready (cpal + symphonia)".to_string()),
        })
    }

    pub fn position_secs(&self) -> f64 {
        f32::from_bits(self.position_bits.load(Ordering::Relaxed)) as f64
    }

    pub fn set_position_secs(&self, secs: f64) {
        self.position_bits
            .store((secs.max(0.0) as f32).to_bits(), Ordering::Relaxed);
    }

    pub fn duration_secs(&self) -> f64 {
        f32::from_bits(self.duration_bits.load(Ordering::Relaxed)) as f64
    }

    pub fn set_duration_secs(&self, secs: f64) {
        self.duration_bits
            .store((secs.max(0.0) as f32).to_bits(), Ordering::Relaxed);
    }

    pub fn set_status(&self, status: String) {
        if let Ok(mut g) = self.status_text.lock() {
            *g = status;
        }
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    pub fn spectrum_snapshot(&self) -> [f32; SPECTRUM_BINS] {
        self.spectrum.lock().map(|g| *g).unwrap_or([0.0; SPECTRUM_BINS])
    }

    pub fn peaks_snapshot(&self) -> (f32, f32) {
        self.peak_levels.lock().map(|g| *g).unwrap_or((0.0, 0.0))
    }
}

/// Real-time DSP processor running inside the `cpal` audio callback thread:
/// 1. Pre-amp & 10-Band Biquad EQ (`biquad::DirectForm2Transposed<f32>`)
/// 2. Spatial / Crossfeed Stage (delayed/attenuated contralateral channel mixing + subtle room reverb)
/// 3. FFT Tap (1024-sample Hann window -> `rustfft` -> logarithmic spectrum bars)
struct RealtimeDspProcessor {
    sample_rate: f32,
    config: DspConfig,
    preamp_linear: f32,
    any_eq_active: bool,
    filters_l: [DirectForm2Transposed<f32>; 10],
    filters_r: [DirectForm2Transposed<f32>; 10],
    // Delay ring buffer for Bauer-style headphone crossfeed & room width (~30ms max at 48kHz)
    delay_l: Vec<f32>,
    delay_r: Vec<f32>,
    delay_idx: usize,
    crossfeed_lp_l: f32,
    crossfeed_lp_r: f32,
    // Precomputed FFT window, bin mapping & planner
    hann_window: Vec<f32>,
    bin_ranges: [(usize, usize); SPECTRUM_BINS],
    fft_buf: Vec<f32>,
    fft_scratch: Vec<Complex<f32>>,
    fft: Arc<dyn rustfft::Fft<f32>>,
    fft_frame_decimator: u8,
    smoothed_bins: [f32; SPECTRUM_BINS],
}

impl RealtimeDspProcessor {
    fn new(sample_rate: f32) -> Self {
        let config = DspConfig::default();
        let flat_coeffs = Self::make_band_coeffs(sample_rate, 1000.0, 0.0);
        let filters_l = [DirectForm2Transposed::<f32>::new(flat_coeffs); 10];
        let filters_r = [DirectForm2Transposed::<f32>::new(flat_coeffs); 10];

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_WINDOW_SIZE);

        let hann_window: Vec<f32> = (0..FFT_WINDOW_SIZE)
            .map(|i| {
                0.5 * (1.0
                    - (2.0 * std::f32::consts::PI * i as f32 / (FFT_WINDOW_SIZE - 1) as f32)
                        .cos())
            })
            .collect();

        let nyquist_bins = FFT_WINDOW_SIZE / 2;
        let mut bin_ranges = [(1usize, 2usize); SPECTRUM_BINS];
        for (bin_idx, slot) in bin_ranges.iter_mut().enumerate() {
            let t0 = bin_idx as f32 / SPECTRUM_BINS as f32;
            let t1 = (bin_idx + 1) as f32 / SPECTRUM_BINS as f32;
            let start = ((nyquist_bins as f32).powf(t0) as usize).clamp(1, nyquist_bins - 1);
            let end = ((nyquist_bins as f32).powf(t1) as usize).clamp(start + 1, nyquist_bins);
            *slot = (start, end);
        }

        let mut proc = Self {
            sample_rate,
            config: config.clone(),
            preamp_linear: 1.0,
            any_eq_active: false,
            filters_l,
            filters_r,
            delay_l: vec![0.0; 1536],
            delay_r: vec![0.0; 1536],
            delay_idx: 0,
            crossfeed_lp_l: 0.0,
            crossfeed_lp_r: 0.0,
            hann_window,
            bin_ranges,
            fft_buf: Vec::with_capacity(FFT_WINDOW_SIZE),
            fft_scratch: vec![Complex::new(0.0, 0.0); FFT_WINDOW_SIZE],
            fft,
            fft_frame_decimator: 0,
            smoothed_bins: [0.0; SPECTRUM_BINS],
        };
        proc.update_config(config);
        proc
    }

    fn make_band_coeffs(sample_rate: f32, freq: f32, gain_db: f32) -> Coefficients<f32> {
        let clamped_freq = freq.min(sample_rate * 0.46).max(20.0);
        let filter_type = if freq <= 35.0 {
            FilterType::LowShelf(gain_db)
        } else if freq >= 15000.0 {
            FilterType::HighShelf(gain_db)
        } else {
            FilterType::PeakingEQ(gain_db)
        };

        Coefficients::<f32>::from_params(
            filter_type,
            sample_rate.hz(),
            clamped_freq.hz(),
            1.414_f32,
        )
        .unwrap_or_else(|_| {
            Coefficients::<f32>::from_params(
                FilterType::PeakingEQ(0.0),
                48000.0.hz(),
                1000.0.hz(),
                1.0,
            )
            .unwrap()
        })
    }

    fn update_config(&mut self, config: DspConfig) {
        self.preamp_linear = if config.eq_enabled && config.preamp_db.abs() > 0.05 {
            10.0_f32.powf(config.preamp_db / 20.0)
        } else {
            1.0
        };
        self.any_eq_active = config.eq_enabled
            && (config.preamp_db.abs() > 0.05
                || config.bands_db.iter().any(|&b| b.abs() > 0.05));
        self.config = config;
        for (i, &freq) in EQ_BAND_FREQS.iter().enumerate() {
            let gain = if self.config.eq_enabled {
                self.config.bands_db[i]
            } else {
                0.0
            };
            let coeffs = Self::make_band_coeffs(self.sample_rate, freq, gain);
            self.filters_l[i].update_coefficients(coeffs);
            self.filters_r[i].update_coefficients(coeffs);
        }
    }

    fn reset_state(&mut self) {
        for i in 0..10 {
            self.filters_l[i].reset_state();
            self.filters_r[i].reset_state();
        }
        self.delay_l.fill(0.0);
        self.delay_r.fill(0.0);
        self.delay_idx = 0;
        self.crossfeed_lp_l = 0.0;
        self.crossfeed_lp_r = 0.0;
        self.fft_buf.clear();
    }

    #[inline(always)]
    fn fast_soft_clip(x: f32) -> f32 {
        let c = x.clamp(-1.5, 1.5);
        c - (c * c * c) * (1.0 / 6.75)
    }

    #[inline]
    fn process_stereo_frame(&mut self, mut left: f32, mut right: f32, volume: f32) -> (f32, f32) {
        // 1. Pre-amp + 10-band Biquad EQ (skipped completely when Flat)
        if self.any_eq_active {
            left *= self.preamp_linear;
            right *= self.preamp_linear;

            for i in 0..10 {
                if self.config.bands_db[i].abs() > 0.05 {
                    left = self.filters_l[i].run(left);
                    right = self.filters_r[i].run(right);
                }
            }
        }

        // 2. Spatial / Stereo Crossfeed + Room Stage
        if self.config.spatial_enabled {
            let len = self.delay_l.len();
            self.delay_l[self.delay_idx] = left;
            self.delay_r[self.delay_idx] = right;

            // ~300us interaural time delay (~14 samples @ 48kHz) for headphone crossfeed
            let cross_samples = ((self.sample_rate * 0.00032) as usize).clamp(4, len - 1);
            // ~18ms early reflection tap for room width
            let room_samples = ((self.sample_rate * 0.018) as usize).clamp(64, len - 1);

            let cross_idx = (self.delay_idx + len - cross_samples) % len;
            let room_idx = (self.delay_idx + len - room_samples) % len;

            // Low-pass filter contralateral signal (~700Hz cutoff)
            let alpha = 0.085_f32;
            self.crossfeed_lp_l += alpha * (self.delay_l[cross_idx] - self.crossfeed_lp_l);
            self.crossfeed_lp_r += alpha * (self.delay_r[cross_idx] - self.crossfeed_lp_r);

            let cf = self.config.crossfeed_amount * 0.42;
            let rw = self.config.room_width * 0.22;

            let mid = 0.5 * (left + right);
            let side = 0.5 * (left - right) * (1.0 + self.config.room_width * 0.35);

            let wide_l = mid + side;
            let wide_r = mid - side;

            left = wide_l + self.crossfeed_lp_r * cf + self.delay_r[room_idx] * rw;
            right = wide_r + self.crossfeed_lp_l * cf + self.delay_l[room_idx] * rw;

            self.delay_idx = (self.delay_idx + 1) % len;
        }

        // Fast polynomial soft limiter + master volume (zero transcendental math per sample)
        let out_l = Self::fast_soft_clip(left * volume);
        let out_r = Self::fast_soft_clip(right * volume);

        // 3. Push mono sum into FFT tap buffer
        if self.fft_buf.len() < FFT_WINDOW_SIZE {
            self.fft_buf.push(0.5 * (out_l + out_r));
        }

        (out_l, out_r)
    }

    fn compute_fft_if_ready(&mut self, shared: &AudioSharedState) {
        if self.fft_buf.len() < FFT_WINDOW_SIZE {
            return;
        }
        if !shared.spectrum_active.load(Ordering::Relaxed) {
            self.fft_buf.clear();
            return;
        }
        // Decimate FFT execution so we only compute ~12-15 FFTs/sec instead of 47 FFTs/sec
        self.fft_frame_decimator = self.fft_frame_decimator.wrapping_add(1);
        if self.fft_frame_decimator % 3 != 0 && self.smoothed_bins.iter().any(|&v| v > 0.03) {
            self.fft_buf.clear();
            return;
        }

        for (i, &sample) in self.fft_buf.iter().enumerate() {
            self.fft_scratch[i] = Complex::new(sample * self.hann_window[i], 0.0);
        }
        self.fft_buf.clear();

        self.fft.process(&mut self.fft_scratch);

        for (bin_idx, &(start, end)) in self.bin_ranges.iter().enumerate() {
            let mut sum = 0.0_f32;
            for k in start..end {
                let c = self.fft_scratch[k];
                sum += (c.re * c.re + c.im * c.im).sqrt();
            }
            let avg = sum / ((end - start) as f32);
            // Convert magnitude to normalized 0.0..1.0 perceptual scale
            let db = 20.0 * (avg / 12.0 + 1e-5).log10();
            let norm = ((db + 42.0) / 42.0).clamp(0.02, 1.0);

            // Fast attack, smooth decay
            if norm > self.smoothed_bins[bin_idx] {
                self.smoothed_bins[bin_idx] =
                    self.smoothed_bins[bin_idx] * 0.35 + norm * 0.65;
            } else {
                self.smoothed_bins[bin_idx] =
                    (self.smoothed_bins[bin_idx] * 0.82).max(0.02);
            }
        }

        if let Ok(mut guard) = shared.spectrum.try_lock() {
            *guard = self.smoothed_bins;
        }
    }
}

pub struct NativeAudioEngine {
    cmd_tx: flume::Sender<AudioCommand>,
    pub shared: Arc<AudioSharedState>,
}

impl NativeAudioEngine {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = flume::unbounded::<AudioCommand>();
        let shared = AudioSharedState::new();
        let shared_clone = shared.clone();

        std::thread::Builder::new()
            .name("gpui-native-audio-worker".to_string())
            .spawn(move || {
                run_audio_worker(cmd_rx, shared_clone);
            })
            .expect("Failed to spawn native audio worker thread");

        Self { cmd_tx, shared }
    }

    pub fn send(&self, cmd: AudioCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn playback_status(&self) -> PlaybackStatus {
        if self.shared.is_playing.load(Ordering::Relaxed) {
            PlaybackStatus::Playing
        } else if self.shared.position_secs() > 0.01 {
            PlaybackStatus::Paused
        } else {
            PlaybackStatus::Stopped
        }
    }
}

fn run_audio_worker(cmd_rx: flume::Receiver<AudioCommand>, shared: Arc<AudioSharedState>) {
    // 1. Initialize CPAL default output stream + lock-free SPSC ringbuf (~3 seconds stereo PCM)
    let host = cpal::default_host();
    let device = host.default_output_device();

    let (sample_rate, channels) = if let Some(ref dev) = device {
        if let Ok(cfg) = dev.default_output_config() {
            (cfg.sample_rate().0, cfg.channels() as usize)
        } else {
            (48_000, 2)
        }
    } else {
        (48_000, 2)
    };

    shared.sample_rate.store(sample_rate, Ordering::Relaxed);

    // 3 seconds of interleaved stereo f32 samples in lock-free ringbuf
    let ring_capacity = (sample_rate as usize) * 2 * 3;
    let rb = HeapRb::<f32>::new(ring_capacity);
    let (mut ring_prod, mut ring_cons) = rb.split();

    let dsp_shared = Arc::new(Mutex::new(RealtimeDspProcessor::new(sample_rate as f32)));
    let dsp_for_cb = dsp_shared.clone();
    let shared_for_cb = shared.clone();

    let _cpal_stream = device.and_then(|dev| {
        let config = dev.default_output_config().ok()?;
        let stream_config: cpal::StreamConfig = config.into();
        let out_channels = stream_config.channels as usize;

        dev.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Immediately flush any queued PCM samples from the previous track or seek position
                if shared_for_cb.flush_requested.load(Ordering::Acquire) {
                    ring_cons.clear();
                    if let Ok(mut dsp) = dsp_for_cb.try_lock() {
                        dsp.reset_state();
                    }
                    shared_for_cb.flush_requested.store(false, Ordering::Release);
                }

                let is_playing = shared_for_cb.is_playing.load(Ordering::Acquire);
                let vol = shared_for_cb.volume();

                if !is_playing {
                    data.fill(0.0);
                    return;
                }

                let mut peak_l = 0.0_f32;
                let mut peak_r = 0.0_f32;
                let mut frames_played = 0u32;

                if let Ok(mut dsp) = dsp_for_cb.try_lock() {
                    for frame in data.chunks_mut(out_channels.max(1)) {
                        let raw_l = ring_cons.try_pop().unwrap_or(0.0);
                        let raw_r = ring_cons.try_pop().unwrap_or(raw_l);

                        let (proc_l, proc_r) = dsp.process_stereo_frame(raw_l, raw_r, vol);
                        peak_l = peak_l.max(proc_l.abs());
                        peak_r = peak_r.max(proc_r.abs());

                        if out_channels == 1 {
                            frame[0] = 0.5 * (proc_l + proc_r);
                        } else {
                            frame[0] = proc_l;
                            frame[1] = proc_r;
                            for ch in frame.iter_mut().skip(2) {
                                *ch = 0.0;
                            }
                        }
                        frames_played += 1;
                    }
                    dsp.compute_fft_if_ready(&shared_for_cb);
                } else {
                    for frame in data.chunks_mut(out_channels.max(1)) {
                        let l = ring_cons.try_pop().unwrap_or(0.0) * vol;
                        let r = ring_cons.try_pop().unwrap_or(l) * vol;
                        frame[0] = l;
                        if out_channels > 1 {
                            frame[1] = r;
                        }
                        frames_played += 1;
                    }
                }

                if let Ok(mut peaks) = shared_for_cb.peak_levels.try_lock() {
                    peaks.0 = peaks.0 * 0.6 + peak_l * 0.4;
                    peaks.1 = peaks.1 * 0.6 + peak_r * 0.4;
                }

                if frames_played > 0 {
                    let dt = frames_played as f32 / sample_rate as f32;
                    let cur = f32::from_bits(shared_for_cb.position_bits.load(Ordering::Relaxed));
                    let dur = f32::from_bits(shared_for_cb.duration_bits.load(Ordering::Relaxed));
                    let next = if dur > 0.0 && cur + dt >= dur {
                        0.0
                    } else {
                        cur + dt
                    };
                    shared_for_cb
                        .position_bits
                        .store(next.to_bits(), Ordering::Relaxed);
                }
            },
            |err| {
                eprintln!("CPAL output stream error: {err}");
            },
            None,
        )
        .ok()
    });

    if let Some(ref stream) = _cpal_stream {
        let _ = stream.play();
    }

    let _ = channels;

    // 2. Decoder & Synthesizer loop feeding the lock-free ringbuf
    let mut decoded_stereo_pcm: Vec<f32> = Vec::new();
    let mut read_cursor: usize = 0;
    // Keep at most ~180ms of stereo audio buffered in the ringbuf so track changes & seeks are instant
    let max_prebuffer_samples = ((sample_rate as usize) * 2) / 5;

    let flush_consumer_sync = |shared: &AudioSharedState| {
        shared.flush_requested.store(true, Ordering::Release);
        for _ in 0..25 {
            if !shared.flush_requested.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        shared.flush_requested.store(false, Ordering::Release);
    };

    loop {
        // Drain pending commands, coalescing superseded PlaySource commands if multiple arrived at once
        let mut pending_play: Option<(TrackSource, f64)> = None;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCommand::PlaySource {
                    source,
                    duration_hint,
                } => {
                    pending_play = Some((source, duration_hint));
                }
                AudioCommand::Pause => {
                    shared.is_playing.store(false, Ordering::Release);
                    if let Ok(mut sp) = shared.spectrum.lock() {
                        for b in sp.iter_mut() {
                            *b *= 0.25;
                        }
                    }
                }
                AudioCommand::Resume => {
                    if !decoded_stereo_pcm.is_empty() {
                        shared.is_playing.store(true, Ordering::Release);
                    }
                }
                AudioCommand::Stop => {
                    pending_play = None;
                    shared.is_playing.store(false, Ordering::Release);
                    flush_consumer_sync(&shared);
                    read_cursor = 0;
                    shared
                        .position_bits
                        .store(0.0_f32.to_bits(), Ordering::Relaxed);
                }
                AudioCommand::Seek(target_secs) => {
                    flush_consumer_sync(&shared);
                    if !decoded_stereo_pcm.is_empty() {
                        let frame_idx = (target_secs.max(0.0) * sample_rate as f64) as usize;
                        let sample_idx = (frame_idx * 2) % decoded_stereo_pcm.len();
                        read_cursor = sample_idx - (sample_idx % 2);
                    }
                    shared
                        .position_bits
                        .store((target_secs as f32).to_bits(), Ordering::Relaxed);
                }
                AudioCommand::SetVolume(vol) => {
                    shared
                        .volume_bits
                        .store(vol.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
                }
                AudioCommand::UpdateDsp(cfg) => {
                    if let Ok(mut dsp) = dsp_shared.lock() {
                        dsp.update_config(cfg);
                    }
                }
            }
        }

        if let Some((source, duration_hint)) = pending_play {
            // Immediately pause output and flush any old track samples from the ring buffer
            shared.is_playing.store(false, Ordering::Release);
            flush_consumer_sync(&shared);
            shared
                .position_bits
                .store(0.0_f32.to_bits(), Ordering::Relaxed);
            shared
                .duration_bits
                .store((duration_hint as f32).to_bits(), Ordering::Relaxed);

            match decode_or_synthesize_source(&source, sample_rate, duration_hint, &shared) {
                Ok((pcm, actual_duration)) => {
                    decoded_stereo_pcm = pcm;
                    read_cursor = 0;
                    // Ensure ring buffer is completely clean right before pushing the new track
                    flush_consumer_sync(&shared);
                    shared
                        .duration_bits
                        .store((actual_duration as f32).to_bits(), Ordering::Relaxed);
                    shared.is_playing.store(true, Ordering::Release);
                }
                Err(err) => {
                    if let Ok(mut st) = shared.status_text.lock() {
                        *st = format!("Playback error: {err}");
                    }
                }
            }
        }

        // Feed decoded/synthesized PCM frames into the lock-free ring buffer
        if shared.is_playing.load(Ordering::Acquire)
            && !shared.flush_requested.load(Ordering::Acquire)
            && !decoded_stereo_pcm.is_empty()
        {
            let occupied = ring_prod.occupied_len();
            let vacant = ring_prod.vacant_len();
            if occupied < max_prebuffer_samples && vacant >= 512 {
                let remaining = decoded_stereo_pcm.len().saturating_sub(read_cursor);
                if remaining == 0 {
                    // Loop PCM cursor back to start of buffer seamlessly
                    read_cursor = 0;
                } else {
                    let needed = (max_prebuffer_samples - occupied).min(vacant).min(4096);
                    let to_push = needed.min(remaining);
                    let pushed = ring_prod
                        .push_slice(&decoded_stereo_pcm[read_cursor..read_cursor + to_push]);
                    read_cursor += pushed;
                }
            }

            // If running in a headless environment without an active CPAL output callback draining the ringbuf,
            // advance FFT & position so the UI spectrum visualizer and scrubber still animate smoothly!
            if _cpal_stream.is_none() {
                let step = ((sample_rate as usize / 60) * 2).max(2);
                if read_cursor + step < decoded_stereo_pcm.len() {
                    if let Ok(mut dsp) = dsp_shared.lock() {
                        let vol = shared.volume();
                        for pair in
                            decoded_stereo_pcm[read_cursor..read_cursor + step].chunks_exact(2)
                        {
                            let _ = dsp.process_stereo_frame(pair[0], pair[1], vol);
                        }
                        dsp.compute_fft_if_ready(&shared);
                    }
                    read_cursor += step;
                    let pos = (read_cursor / 2) as f32 / sample_rate as f32;
                    shared.position_bits.store(pos.to_bits(), Ordering::Relaxed);
                } else {
                    read_cursor = 0;
                }
            }

            std::thread::sleep(Duration::from_millis(28));
        } else {
            // When paused or stopped, block on command channel instead of busy-polling
            std::thread::sleep(Duration::from_millis(80));
        }
    }
}

/// Decodes AAC / ALAC / fMP4 / MP3 / WAV via `symphonia`, or generates a rich polyphonic
/// stereo synth track for built-in demo tracks.
fn decode_or_synthesize_source(
    source: &TrackSource,
    target_sample_rate: u32,
    duration_hint: f64,
    shared: &AudioSharedState,
) -> Result<(Vec<f32>, f64), String> {
    match source {
        TrackSource::StreamUrl(url) => {
            if let Ok(mut st) = shared.status_text.lock() {
                *st = format!("Streaming & decoding AAC/fMP4 via Symphonia: {url}");
            }
            let resp = reqwest::blocking::get(url)
                .map_err(|e| format!("Failed to fetch stream URL: {e}"))?;
            let bytes = resp
                .bytes()
                .map_err(|e| format!("Failed to read stream bytes: {e}"))?
                .to_vec();
            let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
            let mut hint = Hint::new();
            if url.contains(".m4a") || url.contains(".mp4") || url.contains("aac") {
                hint.with_extension("m4a");
            } else if url.contains(".mp3") {
                hint.with_extension("mp3");
            }
            decode_media_source(mss, hint, target_sample_rate, shared)
        }
        TrackSource::LocalFile(path) => {
            if let Ok(mut st) = shared.status_text.lock() {
                *st = format!("Decoding local file via Symphonia: {path}");
            }
            let file =
                std::fs::File::open(path).map_err(|e| format!("Cannot open {path}: {e}"))?;
            let mss = MediaSourceStream::new(Box::new(file), Default::default());
            let mut hint = Hint::new();
            if let Some(ext) = std::path::Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
            {
                hint.with_extension(ext);
            }
            decode_media_source(mss, hint, target_sample_rate, shared)
        }
        TrackSource::SynthDemo {
            root_midi,
            tempo_bpm,
            style,
        } => {
            if let Ok(mut st) = shared.status_text.lock() {
                *st = format!(
                    "Playing Native Rust Polyphonic Synth ({target_sample_rate}Hz 32-bit float PCM)"
                );
            }
            let secs = duration_hint.clamp(30.0, 180.0);
            let pcm = generate_polyphonic_synth(*root_midi, *tempo_bpm, *style, target_sample_rate, secs);
            Ok((pcm, secs))
        }
    }
}

fn decode_media_source(
    mss: MediaSourceStream,
    hint: Hint,
    target_sample_rate: u32,
    shared: &AudioSharedState,
) -> Result<(Vec<f32>, f64), String> {
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Symphonia probe error: {e}"))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "No supported audio track found".to_string())?;

    let track_id = track.id;
    let src_rate = track.codec_params.sample_rate.unwrap_or(44_100);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Symphonia codec init error: {e}"))?;

    let mut raw_stereo = Vec::<f32>::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        if let Ok(audio_buf) = decoder.decode(&packet) {
            let spec = *audio_buf.spec();
            let capacity = audio_buf.capacity() as u64;
            let buf = sample_buf.get_or_insert_with(|| SampleBuffer::<f32>::new(capacity, spec));
            if buf.capacity() < audio_buf.frames() * spec.channels.count() {
                *buf = SampleBuffer::<f32>::new(capacity, spec);
            }
            buf.copy_interleaved_ref(audio_buf);
            let samples = buf.samples();
            let ch_count = spec.channels.count().max(1);
            if ch_count == 1 {
                for &s in samples {
                    raw_stereo.push(s);
                    raw_stereo.push(s);
                }
            } else {
                for frame in samples.chunks(ch_count) {
                    raw_stereo.push(frame[0]);
                    raw_stereo.push(frame[1]);
                }
            }
        }
    }

    if raw_stereo.is_empty() {
        return Err("Decoded audio stream contained 0 frames".to_string());
    }

    // Resample linearly if source sample rate differs from CPAL output device sample rate
    let final_stereo = if src_rate != target_sample_rate && src_rate > 0 {
        let ratio = src_rate as f64 / target_sample_rate as f64;
        let in_frames = raw_stereo.len() / 2;
        let out_frames = (in_frames as f64 / ratio) as usize;
        let mut resampled = Vec::with_capacity(out_frames * 2);
        for i in 0..out_frames {
            let src_pos = i as f64 * ratio;
            let idx = (src_pos as usize).min(in_frames.saturating_sub(2));
            let frac = (src_pos - idx as f64) as f32;
            let l = raw_stereo[idx * 2] * (1.0 - frac) + raw_stereo[(idx + 1) * 2] * frac;
            let r = raw_stereo[idx * 2 + 1] * (1.0 - frac) + raw_stereo[(idx + 1) * 2 + 1] * frac;
            resampled.push(l);
            resampled.push(r);
        }
        resampled
    } else {
        raw_stereo
    };

    let duration_secs = (final_stereo.len() / 2) as f64 / target_sample_rate as f64;
    if let Ok(mut st) = shared.status_text.lock() {
        *st = format!(
            "Playing Symphonia Decoded Stream ({src_rate}Hz -> {target_sample_rate}Hz, {:.1}s)",
            duration_secs
        );
    }

    Ok((final_stereo, duration_secs))
}

fn midi_to_freq(midi: f32) -> f32 {
    440.0 * 2.0_f32.powf((midi - 69.0) / 12.0)
}

/// Generates a warm polyphonic stereo synthesizer track (sub-bass + lush pads + arpeggio + percussive hi-hats)
/// so the 10-band EQ (32Hz..16kHz), stereo crossfeed, and FFT visualizer have full-spectrum signal immediately.
fn generate_polyphonic_synth(
    root_midi: u8,
    tempo_bpm: u16,
    style: u8,
    sample_rate: u32,
    duration_secs: f64,
) -> Vec<f32> {
    let sr = sample_rate as f32;
    let beat_secs = 60.0 / (tempo_bpm.max(60) as f32);
    let bar_secs = beat_secs * 4.0;
    // Generate exactly 1 seamless 4-bar progression cycle (~8-12s) so synthesis is near-instant (<20ms)
    let cycle_secs = (bar_secs as f64 * 4.0).min(duration_secs);
    let total_frames = (cycle_secs * sample_rate as f64) as usize;
    let mut out = Vec::with_capacity(total_frames * 2);

    // Chord progressions (semitone offsets from root)
    let chords: [[f32; 4]; 4] = match style % 4 {
        0 => [
            [0.0, 3.0, 7.0, 10.0],  // Cm7
            [-4.0, 0.0, 3.0, 7.0],  // Abmaj7
            [-7.0, -3.0, 0.0, 5.0], // Fm7
            [-5.0, -1.0, 2.0, 7.0], // G7
        ],
        1 => [
            [0.0, 4.0, 7.0, 11.0],  // Fmaj7
            [-3.0, 0.0, 4.0, 7.0],  // Dm7
            [-5.0, -2.0, 2.0, 5.0], // Bbmaj7
            [-7.0, 0.0, 4.0, 7.0],  // C6
        ],
        2 => [
            [0.0, 3.0, 7.0, 14.0], // Em9
            [-2.0, 2.0, 5.0, 9.0], // D6
            [-4.0, 0.0, 4.0, 7.0], // Cmaj7
            [-5.0, -2.0, 2.0, 6.0],// Bm7
        ],
        _ => [
            [0.0, 3.0, 7.0, 10.0],
            [-2.0, 1.0, 5.0, 8.0],
            [-5.0, -2.0, 2.0, 5.0],
            [-7.0, -3.0, 0.0, 4.0],
        ],
    };

    let mut noise_seed: u32 = 0x1234_5678;

    for i in 0..total_frames {
        let t = i as f32 / sr;
        let bar_idx = ((t / bar_secs) as usize) % 4;
        let chord = &chords[bar_idx];

        let beat_phase = (t % beat_secs) / beat_secs;
        let sixteenth_phase = (t % (beat_secs * 0.25)) / (beat_secs * 0.25);
        let step_idx = ((t / (beat_secs * 0.25)) as usize) % 4;

        // 1. Deep Sub-Bass + 2nd harmonic (35Hz - 120Hz) for testing 32Hz / 64Hz / 125Hz EQ bands
        let bass_freq = midi_to_freq(root_midi as f32 - 24.0 + chord[0]);
        let bass_env = (1.0 - beat_phase * 0.65).max(0.2);
        let sub = (2.0 * std::f32::consts::PI * bass_freq * t).sin() * 0.34 * bass_env
            + (2.0 * std::f32::consts::PI * bass_freq * 2.0 * t).sin() * 0.14 * bass_env;

        // 2. Warm Stereo Detuned Pad (250Hz - 2kHz)
        let mut pad_l = 0.0_f32;
        let mut pad_r = 0.0_f32;
        for (v, &offset) in chord.iter().enumerate() {
            let f = midi_to_freq(root_midi as f32 + offset);
            let detune = 1.0 + (v as f32 - 1.5) * 0.0025;
            pad_l += (2.0 * std::f32::consts::PI * f * detune * t).sin() * 0.08;
            pad_r += (2.0 * std::f32::consts::PI * f * (2.0 - detune) * t).sin() * 0.08;
        }

        // 3. Pluck Arpeggio (1kHz - 6kHz) panned across stereo field
        let arp_note = root_midi as f32 + 12.0 + chord[step_idx];
        let arp_freq = midi_to_freq(arp_note);
        let arp_env = (-sixteenth_phase * 5.5).exp();
        let arp_sig = ((2.0 * std::f32::consts::PI * arp_freq * t).sin()
            + 0.4 * (2.0 * std::f32::consts::PI * arp_freq * 2.0 * t).sin())
            * 0.16
            * arp_env;
        let pan = if step_idx % 2 == 0 { 0.75 } else { 0.25 };

        // 4. Crisp Air / Shimmer Hi-Hat (8kHz - 16kHz) for testing 8K & 16K EQ bands
        noise_seed = noise_seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let white = ((noise_seed >> 16) as f32 / 32768.0) - 1.0;
        let hat_env = (-sixteenth_phase * 18.0).exp();
        let hat = white * (2.0 * std::f32::consts::PI * 10500.0 * t).sin() * 0.07 * hat_env;

        let left = (sub + pad_l + arp_sig * pan + hat).clamp(-0.95, 0.95);
        let right = (sub + pad_r + arp_sig * (1.0 - pan) - hat * 0.8).clamp(-0.95, 0.95);

        out.push(left);
        out.push(right);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EqPreset;

    #[test]
    fn test_realtime_dsp_biquad_and_fft() {
        let mut cfg = DspConfig::default();
        cfg.apply_preset(EqPreset::Electronic);
        cfg.spatial_enabled = true;
        cfg.crossfeed_amount = 0.45;
        cfg.room_width = 0.35;

        let mut dsp = RealtimeDspProcessor::new(48_000.0);
        dsp.update_config(cfg);
        let shared = AudioSharedState::new();

        // Generate 2048 stereo frames of synth audio and run through 10-band biquad EQ + crossfeed + FFT
        let synth = generate_polyphonic_synth(60, 120, 0, 48_000, 0.25);
        assert!(synth.len() >= 4096);

        for pair in synth.chunks_exact(2).take(2048) {
            let (out_l, out_r) = dsp.process_stereo_frame(pair[0], pair[1], 0.9);
            assert!(out_l.is_finite() && out_l.abs() <= 1.0);
            assert!(out_r.is_finite() && out_r.abs() <= 1.0);
        }

        dsp.compute_fft_if_ready(&shared);
        let bins = shared.spectrum_snapshot();
        assert_eq!(bins.len(), SPECTRUM_BINS);
        assert!(bins.iter().any(|&v| v > 0.05));
    }
}
