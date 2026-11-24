//! Audio playback queue and `rodio` MP3 player for sequential Vietnamese TTS.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use thiserror::Error;

/// Default maximum number of pending unplayed TTS clips held in the FIFO queue.
pub const DEFAULT_MAX_PENDING_CLIPS: usize = 3;

/// Default audio playback gain (`75%`).
pub const DEFAULT_VOLUME: f32 = 0.75;

const PLAYBACK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Typed errors emitted by queue validation, audio device opening, and MP3 decoding.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AudioError {
    #[error("kích thước hàng đợi âm thanh không hợp lệ ({capacity}; tối thiểu 1)")]
    InvalidCapacity { capacity: usize },

    #[error("âm lượng không hợp lệ: {detail} (phải nằm trong 0.0..=1.0)")]
    InvalidVolume { detail: String },

    #[error("dữ liệu âm thanh TTS rỗng")]
    EmptyAudio,

    #[error("không thể mở thiết bị phát âm thanh: {detail}")]
    DeviceOpenFailed { detail: String },

    #[error("không thể giải mã âm thanh MP3: {detail}")]
    DecodeFailed { detail: String },

    #[error("luồng phát âm thanh đã dừng")]
    WorkerStopped,
}

/// Validate that `volume` is a finite gain within `0.0..=1.0`.
pub fn validate_volume(volume: f32) -> Result<f32, AudioError> {
    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
        return Err(AudioError::InvalidVolume {
            detail: format!("{volume}"),
        });
    }
    Ok(volume)
}

/// Encoding of a TTS audio payload. Edge-TTS returns MP3; Piper returns
/// WAV (PCM16, 22050 Hz mono). The format travels with the bytes
/// (`TtsSynthesis::format`, the TTS cache, the playback queue) so each
/// layer decodes them with the right hint instead of assuming MP3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    Wav,
}

impl AudioFormat {
    /// File extension used for cached audio files (`mp3` / `wav`).
    pub fn extension(self) -> &'static str {
        match self {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav => "wav",
        }
    }

    /// `rodio` decoder hint for this encoding.
    pub fn decoder_hint(self) -> &'static str {
        match self {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav => "wav",
        }
    }
}

/// Decode raw audio bytes in `format` into a `rodio::Decoder`.
pub fn decode_audio_bytes(
    audio_bytes: Vec<u8>,
    format: AudioFormat,
) -> Result<rodio::Decoder<Cursor<Vec<u8>>>, AudioError> {
    if audio_bytes.is_empty() {
        return Err(AudioError::EmptyAudio);
    }
    let byte_len = audio_bytes.len() as u64;
    rodio::Decoder::builder()
        .with_data(Cursor::new(audio_bytes))
        .with_byte_len(byte_len)
        .with_seekable(true)
        .with_hint(format.decoder_hint())
        .build()
        .map_err(|err| AudioError::DecodeFailed {
            detail: err.to_string(),
        })
}

/// Decode raw MP3 bytes into a `rodio::Decoder`.
pub fn decode_mp3_bytes(mp3_bytes: Vec<u8>) -> Result<rodio::Decoder<Cursor<Vec<u8>>>, AudioError> {
    decode_audio_bytes(mp3_bytes, AudioFormat::Mp3)
}

/// Validate that raw bytes represent a non-empty, decodable audio stream
/// in `format`.
pub fn validate_audio_bytes(audio_bytes: &[u8], format: AudioFormat) -> Result<(), AudioError> {
    decode_audio_bytes(audio_bytes.to_vec(), format).map(|_| ())
}

/// Validate that raw bytes represent a non-empty, decodable MP3 stream.
pub fn validate_mp3_bytes(mp3_bytes: &[u8]) -> Result<(), AudioError> {
    validate_audio_bytes(mp3_bytes, AudioFormat::Mp3)
}

/// Pure-logic bounded FIFO queue that retains at most `max_pending` unplayed items,
/// evicting the oldest unplayed item when full.
#[derive(Debug, Clone)]
pub struct BoundedAudioQueue<T> {
    max_pending: usize,
    items: VecDeque<T>,
    dropped_count: u64,
}

impl<T> BoundedAudioQueue<T> {
    /// Construct a bounded FIFO queue with capacity `max_pending >= 1`.
    pub fn new(max_pending: usize) -> Result<Self, AudioError> {
        if max_pending == 0 {
            return Err(AudioError::InvalidCapacity {
                capacity: max_pending,
            });
        }
        Ok(Self {
            max_pending,
            items: VecDeque::with_capacity(max_pending),
            dropped_count: 0,
        })
    }

    pub fn capacity(&self) -> usize {
        self.max_pending
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped_count
    }

    /// Push `item` to the back of the queue, returning `Some(evicted)` if the
    /// oldest unplayed item had to be dropped.
    pub fn push(&mut self, item: T) -> Option<T> {
        let evicted = if self.items.len() >= self.max_pending {
            self.dropped_count = self.dropped_count.saturating_add(1);
            self.items.pop_front()
        } else {
            None
        };
        self.items.push_back(item);
        evicted
    }

    /// Pop the oldest unplayed item from the front of the queue.
    pub fn pop_next(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    /// Remove all pending items and return how many were cleared.
    pub fn clear(&mut self) -> usize {
        let count = self.items.len();
        self.items.clear();
        count
    }
}

impl<T> Default for BoundedAudioQueue<T> {
    fn default() -> Self {
        Self {
            max_pending: DEFAULT_MAX_PENDING_CLIPS,
            items: VecDeque::with_capacity(DEFAULT_MAX_PENDING_CLIPS),
            dropped_count: 0,
        }
    }
}

/// Outcome returned when a TTS MP3 clip is accepted into the bounded FIFO queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnqueueOutcome {
    pub pending_len: usize,
    pub dropped_oldest: bool,
}

/// Low-level audio playback backend used inside `AudioPlayer`'s worker thread.
pub trait AudioBackend {
    fn play_audio(&mut self, audio_bytes: Vec<u8>, format: AudioFormat) -> Result<(), AudioError>;
    fn is_playing(&self) -> bool;
    fn set_volume(&mut self, volume: f32);
    fn stop_immediately(&mut self);
}

/// Thread-safe audio playback boundary for pipeline and UI callers.
pub trait AudioPlayback: Send + Sync {
    fn enqueue_audio(
        &self,
        audio_bytes: Vec<u8>,
        format: AudioFormat,
    ) -> Result<EnqueueOutcome, AudioError>;
    /// MP3 shorthand for `enqueue_audio(bytes, AudioFormat::Mp3)`.
    fn enqueue_mp3(&self, mp3_bytes: Vec<u8>) -> Result<EnqueueOutcome, AudioError> {
        self.enqueue_audio(mp3_bytes, AudioFormat::Mp3)
    }
    fn set_volume(&self, volume: f32) -> Result<(), AudioError>;
    fn volume(&self) -> f32;
    fn stop_and_clear(&self) -> Result<(), AudioError>;
    fn is_playing(&self) -> bool;
    fn pending_len(&self) -> usize;
    fn is_idle(&self) -> bool;
    fn last_error(&self) -> Option<AudioError>;
    fn take_last_error(&self) -> Option<AudioError>;
}

struct RodioTrack {
    player: rodio::Player,
}

impl RodioTrack {
    fn new(player: rodio::Player, initial_volume: f32) -> Self {
        player.set_volume(initial_volume);
        Self { player }
    }

    fn play_source<S>(&mut self, source: S)
    where
        S: rodio::Source + Send + 'static,
    {
        self.player.append(source);
        // Ensure playback is unpaused when starting a clip.
        self.player.play();
    }

    fn play_audio(&mut self, audio_bytes: Vec<u8>, format: AudioFormat) -> Result<(), AudioError> {
        let decoder = decode_audio_bytes(audio_bytes, format)?;
        self.play_source(decoder);
        Ok(())
    }

    fn is_playing(&self) -> bool {
        !self.player.empty()
    }

    fn set_volume(&mut self, volume: f32) {
        self.player.set_volume(volume);
    }

    fn stop_immediately(&mut self) {
        self.player.stop();
    }
}

struct RodioBackend {
    _device_sink: rodio::MixerDeviceSink,
    track: RodioTrack,
}

impl RodioBackend {
    fn open_default(initial_volume: f32) -> Result<Self, AudioError> {
        let mut device_sink = rodio::DeviceSinkBuilder::open_default_sink().map_err(|err| {
            AudioError::DeviceOpenFailed {
                detail: err.to_string(),
            }
        })?;
        device_sink.log_on_drop(false);
        let player = rodio::Player::connect_new(device_sink.mixer());
        Ok(Self {
            _device_sink: device_sink,
            track: RodioTrack::new(player, initial_volume),
        })
    }
}

impl AudioBackend for RodioBackend {
    fn play_audio(&mut self, audio_bytes: Vec<u8>, format: AudioFormat) -> Result<(), AudioError> {
        self.track.play_audio(audio_bytes, format)
    }

    fn is_playing(&self) -> bool {
        self.track.is_playing()
    }

    fn set_volume(&mut self, volume: f32) {
        self.track.set_volume(volume);
    }

    fn stop_immediately(&mut self) {
        self.track.stop_immediately();
    }
}

struct SharedAudioState {
    queue: BoundedAudioQueue<(Vec<u8>, AudioFormat)>,
    volume: f32,
    volume_generation: u64,
    handled_volume_generation: u64,
    stop_generation: u64,
    handled_stop_generation: u64,
    is_playing: bool,
    last_error: Option<AudioError>,
    shutdown: bool,
    worker_exited: bool,
}

impl SharedAudioState {
    fn has_pending_control(&self) -> bool {
        self.shutdown
            || self.handled_stop_generation < self.stop_generation
            || self.handled_volume_generation < self.volume_generation
    }
}

struct WorkerExitGuard {
    shared: Arc<(Mutex<SharedAudioState>, Condvar)>,
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.is_playing = false;
        state.worker_exited = true;
        cvar.notify_all();
    }
}

type ClipValidator = Box<dyn Fn(&[u8], AudioFormat) -> Result<(), AudioError> + Send + Sync>;

/// Non-overlapping bounded FIFO audio player backed by a dedicated worker thread.
pub struct AudioPlayer {
    shared: Arc<(Mutex<SharedAudioState>, Condvar)>,
    validator: ClipValidator,
    worker: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Open the default OS audio output device with `DEFAULT_MAX_PENDING_CLIPS` (3).
    pub fn new(initial_volume: f32) -> Result<Self, AudioError> {
        Self::with_config(initial_volume, DEFAULT_MAX_PENDING_CLIPS)
    }

    /// Open the default OS audio output device with an explicit pending-queue capacity.
    pub fn with_config(initial_volume: f32, max_pending: usize) -> Result<Self, AudioError> {
        Self::with_backend(
            initial_volume,
            max_pending,
            RodioBackend::open_default,
            validate_audio_bytes,
        )
    }

    /// Construct an `AudioPlayer` using a custom `AudioBackend` factory and pre-queue validator.
    pub fn with_backend<B, F, V>(
        initial_volume: f32,
        max_pending: usize,
        backend_factory: F,
        validator: V,
    ) -> Result<Self, AudioError>
    where
        B: AudioBackend + 'static,
        F: FnOnce(f32) -> Result<B, AudioError> + Send + 'static,
        V: Fn(&[u8], AudioFormat) -> Result<(), AudioError> + Send + Sync + 'static,
    {
        let volume = validate_volume(initial_volume)?;
        let queue = BoundedAudioQueue::new(max_pending)?;
        let shared = Arc::new((
            Mutex::new(SharedAudioState {
                queue,
                volume,
                volume_generation: 0,
                handled_volume_generation: 0,
                stop_generation: 0,
                handled_stop_generation: 0,
                is_playing: false,
                last_error: None,
                shutdown: false,
                worker_exited: false,
            }),
            Condvar::new(),
        ));

        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), AudioError>>(1);
        let worker_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("cap2voice-audio".to_string())
            .spawn(move || {
                let _exit_guard = WorkerExitGuard {
                    shared: Arc::clone(&worker_shared),
                };
                match backend_factory(volume) {
                    Ok(mut backend) => {
                        let _ = init_tx.send(Ok(()));
                        run_worker_loop(&worker_shared, &mut backend);
                    }
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                    }
                }
            })
            .map_err(|err| AudioError::DeviceOpenFailed {
                detail: format!("không thể tạo luồng âm thanh: {err}"),
            })?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                validator: Box::new(validator),
                worker: Some(handle),
            }),
            Ok(Err(err)) => {
                let _ = handle.join();
                Err(err)
            }
            Err(_) => {
                let _ = handle.join();
                Err(AudioError::WorkerStopped)
            }
        }
    }

    /// Validate an audio payload in `format` and enqueue it onto the
    /// bounded FIFO queue.
    pub fn enqueue_audio(
        &self,
        audio_bytes: Vec<u8>,
        format: AudioFormat,
    ) -> Result<EnqueueOutcome, AudioError> {
        {
            let (lock, _) = &*self.shared;
            let state = lock.lock().unwrap_or_else(|e| e.into_inner());
            if state.worker_exited || state.shutdown {
                return Err(AudioError::WorkerStopped);
            }
        }

        if let Err(err) = (self.validator)(&audio_bytes, format) {
            let (lock, _) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.last_error = Some(err.clone());
            return Err(err);
        }

        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.worker_exited || state.shutdown {
            return Err(AudioError::WorkerStopped);
        }
        let dropped_oldest = state.queue.push((audio_bytes, format)).is_some();
        let pending_len = state.queue.len();
        cvar.notify_all();
        Ok(EnqueueOutcome {
            pending_len,
            dropped_oldest,
        })
    }

    /// MP3 shorthand for `enqueue_audio(bytes, AudioFormat::Mp3)`.
    pub fn enqueue_mp3(&self, mp3_bytes: Vec<u8>) -> Result<EnqueueOutcome, AudioError> {
        self.enqueue_audio(mp3_bytes, AudioFormat::Mp3)
    }

    /// Update playback gain linearly within `0.0..=1.0` and wait until applied by the worker.
    pub fn set_volume(&self, volume: f32) -> Result<(), AudioError> {
        let validated = validate_volume(volume)?;
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.worker_exited || state.shutdown {
            return Err(AudioError::WorkerStopped);
        }
        state.volume = validated;
        state.volume_generation = state.volume_generation.saturating_add(1);
        let target_gen = state.volume_generation;
        cvar.notify_all();

        let state = cvar
            .wait_while(state, |st| {
                !st.worker_exited && !st.shutdown && st.handled_volume_generation < target_gen
            })
            .unwrap_or_else(|e| e.into_inner());
        if state.handled_volume_generation < target_gen {
            return Err(AudioError::WorkerStopped);
        }
        Ok(())
    }

    /// Current target playback volume (`0.0..=1.0`).
    pub fn volume(&self) -> f32 {
        let (lock, _) = &*self.shared;
        lock.lock().unwrap_or_else(|e| e.into_inner()).volume
    }

    /// Halt active playback immediately and discard all pending queued clips.
    pub fn stop_and_clear(&self) -> Result<(), AudioError> {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.worker_exited || state.shutdown {
            return Err(AudioError::WorkerStopped);
        }
        state.queue.clear();
        state.stop_generation = state.stop_generation.saturating_add(1);
        let target_gen = state.stop_generation;
        cvar.notify_all();

        let state = cvar
            .wait_while(state, |st| {
                !st.worker_exited && !st.shutdown && st.handled_stop_generation < target_gen
            })
            .unwrap_or_else(|e| e.into_inner());
        if state.handled_stop_generation < target_gen {
            return Err(AudioError::WorkerStopped);
        }
        Ok(())
    }

    /// Returns `true` while a clip is actively playing on the audio backend.
    pub fn is_playing(&self) -> bool {
        let (lock, _) = &*self.shared;
        lock.lock().unwrap_or_else(|e| e.into_inner()).is_playing
    }

    /// Number of unplayed clips currently waiting in the pending FIFO queue.
    pub fn pending_len(&self) -> usize {
        let (lock, _) = &*self.shared;
        lock.lock().unwrap_or_else(|e| e.into_inner()).queue.len()
    }

    /// Returns `true` when nothing is playing and no clips are pending.
    pub fn is_idle(&self) -> bool {
        let (lock, _) = &*self.shared;
        let state = lock.lock().unwrap_or_else(|e| e.into_inner());
        !state.is_playing && state.queue.is_empty()
    }

    /// Inspect the most recent non-fatal audio error, if any.
    pub fn last_error(&self) -> Option<AudioError> {
        let (lock, _) = &*self.shared;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error
            .clone()
    }

    /// Take and clear the most recent non-fatal audio error, if any.
    pub fn take_last_error(&self) -> Option<AudioError> {
        let (lock, _) = &*self.shared;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error
            .take()
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        {
            let (lock, cvar) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown = true;
            state.queue.clear();
            cvar.notify_all();
        }
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

fn run_worker_loop<B: AudioBackend>(
    shared: &Arc<(Mutex<SharedAudioState>, Condvar)>,
    backend: &mut B,
) {
    let (lock, cvar) = &**shared;
    loop {
        enum WorkerAction {
            Exit,
            Stop { stop_gen: u64 },
            SetVolume { volume: f32, volume_gen: u64 },
            StartClip(Vec<u8>, AudioFormat),
            PollPlayback,
            WaitIdle,
        }

        let action = {
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            if state.shutdown {
                WorkerAction::Exit
            } else if state.handled_stop_generation < state.stop_generation {
                WorkerAction::Stop {
                    stop_gen: state.stop_generation,
                }
            } else if state.handled_volume_generation < state.volume_generation {
                WorkerAction::SetVolume {
                    volume: state.volume,
                    volume_gen: state.volume_generation,
                }
            } else if state.is_playing {
                WorkerAction::PollPlayback
            } else if let Some((next_clip, next_format)) = state.queue.pop_next() {
                state.is_playing = true;
                cvar.notify_all();
                WorkerAction::StartClip(next_clip, next_format)
            } else {
                WorkerAction::WaitIdle
            }
        };

        match action {
            WorkerAction::Exit => {
                backend.stop_immediately();
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                state.is_playing = false;
                state.queue.clear();
                cvar.notify_all();
                break;
            }
            WorkerAction::Stop { stop_gen } => {
                backend.stop_immediately();
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                state.is_playing = false;
                state.queue.clear();
                if state.handled_stop_generation < stop_gen {
                    state.handled_stop_generation = stop_gen;
                }
                cvar.notify_all();
            }
            WorkerAction::SetVolume { volume, volume_gen } => {
                backend.set_volume(volume);
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                if state.handled_volume_generation < volume_gen {
                    state.handled_volume_generation = volume_gen;
                }
                cvar.notify_all();
            }
            WorkerAction::StartClip(bytes, format) => {
                let res = backend.play_audio(bytes, format);
                let active_now = if res.is_ok() {
                    backend.is_playing()
                } else {
                    false
                };
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                match res {
                    Ok(()) => {
                        state.last_error = None;
                        state.is_playing = active_now;
                    }
                    Err(err) => {
                        state.is_playing = false;
                        state.last_error = Some(err);
                    }
                }
                cvar.notify_all();
            }
            WorkerAction::PollPlayback => {
                if !backend.is_playing() {
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    state.is_playing = false;
                    cvar.notify_all();
                } else {
                    let state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if !state.has_pending_control() && state.is_playing {
                        drop(cvar.wait_timeout(state, PLAYBACK_POLL_INTERVAL));
                    }
                }
            }
            WorkerAction::WaitIdle => {
                let state = lock.lock().unwrap_or_else(|e| e.into_inner());
                if !state.has_pending_control() && state.queue.is_empty() {
                    drop(cvar.wait(state));
                }
            }
        }
    }
}

impl AudioPlayback for AudioPlayer {
    fn enqueue_audio(
        &self,
        audio_bytes: Vec<u8>,
        format: AudioFormat,
    ) -> Result<EnqueueOutcome, AudioError> {
        Self::enqueue_audio(self, audio_bytes, format)
    }

    fn enqueue_mp3(&self, mp3_bytes: Vec<u8>) -> Result<EnqueueOutcome, AudioError> {
        Self::enqueue_mp3(self, mp3_bytes)
    }

    fn set_volume(&self, volume: f32) -> Result<(), AudioError> {
        Self::set_volume(self, volume)
    }

    fn volume(&self) -> f32 {
        Self::volume(self)
    }

    fn stop_and_clear(&self) -> Result<(), AudioError> {
        Self::stop_and_clear(self)
    }

    fn is_playing(&self) -> bool {
        Self::is_playing(self)
    }

    fn pending_len(&self) -> usize {
        Self::pending_len(self)
    }

    fn is_idle(&self) -> bool {
        Self::is_idle(self)
    }

    fn last_error(&self) -> Option<AudioError> {
        Self::last_error(self)
    }

    fn take_last_error(&self) -> Option<AudioError> {
        Self::take_last_error(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_audio_bytes, decode_mp3_bytes, validate_audio_bytes, validate_volume, AudioError,
        AudioFormat, BoundedAudioQueue, RodioTrack, DEFAULT_MAX_PENDING_CLIPS, DEFAULT_VOLUME,
    };
    use rodio::buffer::SamplesBuffer;
    use rodio::nz;

    fn synthetic_silent_mp3_bytes() -> Vec<u8> {
        // MPEG-1 Layer III, 128 kbps, 44100 Hz, mono, no padding -> 417 bytes/frame.
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0xC4;

        let mut bytes = Vec::with_capacity(417 * 4);
        for _ in 0..4 {
            bytes.extend_from_slice(&frame);
        }
        bytes
    }

    #[test]
    fn bounded_queue_rejects_zero_capacity_and_defaults_to_three() {
        assert_eq!(
            BoundedAudioQueue::<u32>::new(0).err(),
            Some(AudioError::InvalidCapacity { capacity: 0 })
        );
        let default_q = BoundedAudioQueue::<u32>::default();
        assert_eq!(default_q.capacity(), DEFAULT_MAX_PENDING_CLIPS);
        assert_eq!(default_q.capacity(), 3);
        assert!(default_q.is_empty());
        assert_eq!(default_q.len(), 0);
        assert_eq!(default_q.dropped_count(), 0);
    }

    #[test]
    fn bounded_queue_preserves_fifo_and_evicts_oldest_unplayed_when_full() {
        let Ok(mut q) = BoundedAudioQueue::new(3) else {
            panic!("expected capacity 3 queue");
        };

        assert_eq!(q.push("clip-1"), None);
        assert_eq!(q.push("clip-2"), None);
        assert_eq!(q.push("clip-3"), None);
        assert_eq!(q.len(), 3);
        assert_eq!(q.dropped_count(), 0);

        assert_eq!(q.push("clip-4"), Some("clip-1"));
        assert_eq!(q.push("clip-5"), Some("clip-2"));
        assert_eq!(q.len(), 3);
        assert_eq!(q.dropped_count(), 2);

        assert_eq!(q.pop_next(), Some("clip-3"));
        assert_eq!(q.pop_next(), Some("clip-4"));
        assert_eq!(q.pop_next(), Some("clip-5"));
        assert_eq!(q.pop_next(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn bounded_queue_clear_empties_pending_and_returns_count() {
        let Ok(mut q) = BoundedAudioQueue::new(3) else {
            panic!("expected capacity 3 queue");
        };

        assert_eq!(q.push(10), None);
        assert_eq!(q.push(20), None);
        assert_eq!(q.clear(), 2);
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert_eq!(q.pop_next(), None);
    }

    #[test]
    fn validate_volume_accepts_unit_interval_and_rejects_out_of_range_or_non_finite() {
        assert_eq!(validate_volume(0.0), Ok(0.0));
        assert_eq!(validate_volume(0.75), Ok(0.75));
        assert_eq!(validate_volume(1.0), Ok(1.0));

        assert!(matches!(
            validate_volume(-0.01),
            Err(AudioError::InvalidVolume { .. })
        ));
        assert!(matches!(
            validate_volume(1.01),
            Err(AudioError::InvalidVolume { .. })
        ));
        assert!(matches!(
            validate_volume(f32::NAN),
            Err(AudioError::InvalidVolume { .. })
        ));
        assert!(matches!(
            validate_volume(f32::INFINITY),
            Err(AudioError::InvalidVolume { .. })
        ));
        assert!(matches!(
            validate_volume(f32::NEG_INFINITY),
            Err(AudioError::InvalidVolume { .. })
        ));
    }

    #[test]
    fn decode_audio_bytes_accepts_wav() {
        // Minimal valid WAV: PCM16 mono 22050 Hz, 4 silent samples.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&22050u32.to_le_bytes());
        wav.extend_from_slice(&44100u32.to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&8u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 8]);
        // Fix the RIFF chunk size (36 + data size).
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());

        assert!(validate_audio_bytes(&wav, AudioFormat::Wav).is_ok());
        let decoder = match decode_audio_bytes(wav.clone(), AudioFormat::Wav) {
            Ok(dec) => dec,
            Err(err) => panic!("expected synthetic WAV to decode, got: {err}"),
        };
        let samples: Vec<f32> = decoder.collect();
        assert_eq!(samples.len(), 4);
        assert!(samples.iter().all(|s| *s == 0.0));

        // NOTE: the format hint is advisory only — symphonia probes the
        // content, so the same bytes decode under either hint. The
        // `AudioFormat` carried with the bytes (cache, queue) is what
        // routes them; it is not enforced here.
        assert!(decode_audio_bytes(wav, AudioFormat::Mp3).is_ok());
        assert!(matches!(
            decode_audio_bytes(Vec::new(), AudioFormat::Wav),
            Err(AudioError::EmptyAudio)
        ));
    }

    #[test]
    fn decode_mp3_bytes_rejects_empty_and_corrupt_data_and_decodes_valid_mpeg_frames() {
        assert!(matches!(
            decode_mp3_bytes(Vec::new()),
            Err(AudioError::EmptyAudio)
        ));
        assert!(matches!(
            decode_mp3_bytes(b"not-mp3".to_vec()),
            Err(AudioError::DecodeFailed { .. })
        ));

        let mp3 = synthetic_silent_mp3_bytes();
        let decoder = match decode_mp3_bytes(mp3) {
            Ok(dec) => dec,
            Err(err) => panic!("expected synthetic MPEG frames to decode, got: {err}"),
        };
        let samples: Vec<f32> = decoder.take(256).collect();
        assert!(!samples.is_empty());
    }

    #[test]
    fn rodio_track_scales_samples_linearly_and_resumes_cleanly_after_stop_immediately() {
        let (player, mut output) = rodio::Player::new();
        let mut track = RodioTrack::new(player, DEFAULT_VOLUME);

        let first = SamplesBuffer::new(nz!(1), nz!(44100), vec![0.8_f32; 8192]);
        track.play_source(first);
        assert!(track.is_playing());

        let sample_default = output.next().unwrap_or(0.0);
        assert!(
            (sample_default - 0.60).abs() < 1e-4,
            "expected 0.8 * 0.75 = 0.60, got {sample_default}"
        );

        track.set_volume(0.5);
        for _ in 0..512 {
            let _ = output.next();
        }
        let sample_half = output.next().unwrap_or(0.0);
        assert!(
            (sample_half - 0.40).abs() < 1e-4,
            "expected 0.8 * 0.50 = 0.40, got {sample_half}"
        );

        track.set_volume(0.0);
        for _ in 0..512 {
            let _ = output.next();
        }
        let sample_muted = output.next().unwrap_or(1.0);
        assert!(
            sample_muted.abs() < 1e-6,
            "expected 0.0 when muted, got {sample_muted}"
        );

        track.set_volume(1.0);
        for _ in 0..512 {
            let _ = output.next();
        }
        let sample_full = output.next().unwrap_or(0.0);
        assert!(
            (sample_full - 0.80).abs() < 1e-4,
            "expected 0.8 * 1.00 = 0.80, got {sample_full}"
        );

        track.stop_immediately();
        for _ in 0..512 {
            let _ = output.next();
        }
        assert!(!track.is_playing());
        let after_stop = output.next().unwrap_or(1.0);
        assert!(
            after_stop.abs() < 1e-6,
            "expected silence after stop_immediately, got {after_stop}"
        );

        let second = SamplesBuffer::new(nz!(1), nz!(44100), vec![0.4_f32; 4096]);
        track.play_source(second);
        assert!(track.is_playing());
        let resumed = output
            .by_ref()
            .take(1024)
            .find(|s| s.abs() > 1e-6)
            .unwrap_or(0.0);
        assert!(
            (resumed - 0.40).abs() < 1e-4,
            "expected resumed playback at 0.40 after stop_immediately, got {resumed}"
        );
    }
}
