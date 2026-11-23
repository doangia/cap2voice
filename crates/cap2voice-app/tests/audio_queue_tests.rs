//! Integration tests for `AudioPlayer` non-overlapping FIFO queuing,
//! bounded capacity eviction, real-time volume, `stop_and_clear`, error surfacing,
//! and real-PC speaker playback.

use cap2voice_app::api::TtsEngine;
use cap2voice_app::audio::{
    validate_mp3_bytes, AudioBackend, AudioError, AudioPlayback, AudioPlayer, EnqueueOutcome,
    DEFAULT_MAX_PENDING_CLIPS, DEFAULT_VOLUME,
};
use cap2voice_app::tts::EdgeTtsEngine;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct FakeBackendState {
    active_clip: Option<Vec<u8>>,
    active_count: usize,
    max_concurrent_active: usize,
    played_order: Vec<Vec<u8>>,
    volume_history: Vec<f32>,
    stop_count: usize,
    fail_next_play_with: Option<AudioError>,
}

#[derive(Debug, Clone)]
struct FakeBackendController {
    inner: Arc<(Mutex<FakeBackendState>, Condvar)>,
}

impl FakeBackendController {
    fn new(initial_volume: f32) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(FakeBackendState {
                    active_clip: None,
                    active_count: 0,
                    max_concurrent_active: 0,
                    played_order: Vec::new(),
                    volume_history: vec![initial_volume],
                    stop_count: 0,
                    fail_next_play_with: None,
                }),
                Condvar::new(),
            )),
        }
    }

    fn wait_until_active(&self, expected_clip: &str, timeout: Duration) {
        self.wait_until_active_bytes(expected_clip.as_bytes(), timeout);
    }

    fn wait_until_active_bytes(&self, expected_bytes: &[u8], timeout: Duration) {
        let (lock, cvar) = &*self.inner;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, timed_out) = cvar
            .wait_timeout_while(guard, timeout, |st| {
                st.active_clip.as_deref() != Some(expected_bytes)
            })
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            !timed_out.timed_out(),
            "timed out waiting for active clip (len={}), current_len={:?}",
            expected_bytes.len(),
            guard.active_clip.as_ref().map(Vec::len)
        );
    }

    fn finish_active_clip(&self) {
        let (lock, cvar) = &*self.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.active_clip = None;
        guard.active_count = 0;
        cvar.notify_all();
    }

    fn set_fail_next_play(&self, err: AudioError) {
        let (lock, _) = &*self.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.fail_next_play_with = Some(err);
    }

    fn snapshot(&self) -> (usize, Vec<String>, Vec<f32>, usize) {
        let (lock, _) = &*self.inner;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let played_strings = guard
            .played_order
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        (
            guard.max_concurrent_active,
            played_strings,
            guard.volume_history.clone(),
            guard.stop_count,
        )
    }

    fn played_payloads(&self) -> Vec<Vec<u8>> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .played_order
            .clone()
    }
}

struct FakeAudioBackend {
    controller: FakeBackendController,
}

impl AudioBackend for FakeAudioBackend {
    fn play_mp3(&mut self, mp3_bytes: Vec<u8>) -> Result<(), AudioError> {
        let (lock, cvar) = &*self.controller.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(err) = guard.fail_next_play_with.take() {
            return Err(err);
        }
        guard.active_count += 1;
        if guard.active_count > guard.max_concurrent_active {
            guard.max_concurrent_active = guard.active_count;
        }
        guard.active_clip = Some(mp3_bytes.clone());
        guard.played_order.push(mp3_bytes);
        cvar.notify_all();
        Ok(())
    }

    fn is_playing(&self) -> bool {
        let (lock, _) = &*self.controller.inner;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.active_clip.is_some()
    }

    fn set_volume(&mut self, volume: f32) {
        let (lock, _) = &*self.controller.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.volume_history.push(volume);
    }

    fn stop_immediately(&mut self) {
        let (lock, cvar) = &*self.controller.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.active_clip = None;
        guard.active_count = 0;
        guard.stop_count += 1;
        cvar.notify_all();
    }
}

fn accept_non_empty(bytes: &[u8]) -> Result<(), AudioError> {
    if bytes.is_empty() {
        return Err(AudioError::EmptyAudio);
    }
    Ok(())
}

fn wait_until_idle(player: &dyn AudioPlayback, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if player.is_idle() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        player.is_idle(),
        "timed out waiting for AudioPlayer idle (is_playing={}, pending={})",
        player.is_playing(),
        player.pending_len()
    );
}

#[test]
fn audio_player_plays_fifo_sequentially_without_overlap_and_drops_oldest_unplayed_when_full() {
    let controller = FakeBackendController::new(DEFAULT_VOLUME);
    let backend_ctrl = controller.clone();
    let player = match AudioPlayer::with_backend(
        DEFAULT_VOLUME,
        DEFAULT_MAX_PENDING_CLIPS,
        move |_| {
            Ok(FakeAudioBackend {
                controller: backend_ctrl,
            })
        },
        accept_non_empty,
    ) {
        Ok(p) => p,
        Err(err) => panic!("expected fake AudioPlayer init to succeed: {err}"),
    };

    let res0 = player.enqueue_mp3(b"clip-0".to_vec());
    assert_eq!(
        res0,
        Ok(EnqueueOutcome {
            pending_len: 1,
            dropped_oldest: false,
        })
    );
    controller.wait_until_active("clip-0", Duration::from_secs(1));
    assert!(player.is_playing());
    assert!(!player.is_idle());

    // Push 5 more items while clip-0 is actively playing -> cap 3 pending keeps clip-3, clip-4, clip-5.
    assert_eq!(
        player.enqueue_mp3(b"clip-1".to_vec()),
        Ok(EnqueueOutcome {
            pending_len: 1,
            dropped_oldest: false,
        })
    );
    assert_eq!(
        player.enqueue_mp3(b"clip-2".to_vec()),
        Ok(EnqueueOutcome {
            pending_len: 2,
            dropped_oldest: false,
        })
    );
    assert_eq!(
        player.enqueue_mp3(b"clip-3".to_vec()),
        Ok(EnqueueOutcome {
            pending_len: 3,
            dropped_oldest: false,
        })
    );
    assert_eq!(
        player.enqueue_mp3(b"clip-4".to_vec()),
        Ok(EnqueueOutcome {
            pending_len: 3,
            dropped_oldest: true,
        })
    );
    assert_eq!(
        player.enqueue_mp3(b"clip-5".to_vec()),
        Ok(EnqueueOutcome {
            pending_len: 3,
            dropped_oldest: true,
        })
    );
    assert_eq!(player.pending_len(), 3);

    controller.finish_active_clip();
    controller.wait_until_active("clip-3", Duration::from_secs(1));

    controller.finish_active_clip();
    controller.wait_until_active("clip-4", Duration::from_secs(1));

    controller.finish_active_clip();
    controller.wait_until_active("clip-5", Duration::from_secs(1));

    controller.finish_active_clip();
    wait_until_idle(&player, Duration::from_secs(1));

    let (max_concurrent, played_order, _, _) = controller.snapshot();
    assert_eq!(max_concurrent, 1);
    assert_eq!(
        played_order,
        vec![
            "clip-0".to_string(),
            "clip-3".to_string(),
            "clip-4".to_string(),
            "clip-5".to_string(),
        ]
    );
}

#[test]
fn audio_player_set_volume_validates_bounds_and_applies_linear_gain_in_real_time() {
    let controller = FakeBackendController::new(DEFAULT_VOLUME);
    let backend_ctrl = controller.clone();
    let player = match AudioPlayer::with_backend(
        DEFAULT_VOLUME,
        DEFAULT_MAX_PENDING_CLIPS,
        move |_| {
            Ok(FakeAudioBackend {
                controller: backend_ctrl,
            })
        },
        accept_non_empty,
    ) {
        Ok(p) => p,
        Err(err) => panic!("expected fake AudioPlayer init to succeed: {err}"),
    };

    assert!((player.volume() - 0.75).abs() < f32::EPSILON);

    assert_eq!(
        player.enqueue_mp3(b"clip-vol".to_vec()),
        Ok(EnqueueOutcome {
            pending_len: 1,
            dropped_oldest: false,
        })
    );
    controller.wait_until_active("clip-vol", Duration::from_secs(1));

    assert_eq!(player.set_volume(0.0), Ok(()));
    assert!((player.volume() - 0.0).abs() < f32::EPSILON);

    assert_eq!(player.set_volume(0.5), Ok(()));
    assert!((player.volume() - 0.5).abs() < f32::EPSILON);

    assert_eq!(player.set_volume(1.0), Ok(()));
    assert!((player.volume() - 1.0).abs() < f32::EPSILON);

    assert!(matches!(
        player.set_volume(-0.01),
        Err(AudioError::InvalidVolume { .. })
    ));
    assert!(matches!(
        player.set_volume(1.01),
        Err(AudioError::InvalidVolume { .. })
    ));
    assert!(matches!(
        player.set_volume(f32::NAN),
        Err(AudioError::InvalidVolume { .. })
    ));
    assert!(matches!(
        player.set_volume(f32::INFINITY),
        Err(AudioError::InvalidVolume { .. })
    ));
    assert!((player.volume() - 1.0).abs() < f32::EPSILON);

    controller.finish_active_clip();
    wait_until_idle(&player, Duration::from_secs(1));

    let (_, _, volume_history, _) = controller.snapshot();
    assert_eq!(volume_history, vec![0.75, 0.0, 0.5, 1.0]);
}

#[test]
fn audio_player_stop_and_clear_halts_active_playback_under_200ms_and_recovers() {
    let controller = FakeBackendController::new(DEFAULT_VOLUME);
    let backend_ctrl = controller.clone();
    let player = match AudioPlayer::with_backend(
        DEFAULT_VOLUME,
        DEFAULT_MAX_PENDING_CLIPS,
        move |_| {
            Ok(FakeAudioBackend {
                controller: backend_ctrl,
            })
        },
        accept_non_empty,
    ) {
        Ok(p) => p,
        Err(err) => panic!("expected fake AudioPlayer init to succeed: {err}"),
    };

    assert!(player.enqueue_mp3(b"clip-0".to_vec()).is_ok());
    controller.wait_until_active("clip-0", Duration::from_secs(1));
    assert!(player.enqueue_mp3(b"clip-1".to_vec()).is_ok());
    assert!(player.enqueue_mp3(b"clip-2".to_vec()).is_ok());
    assert!(player.enqueue_mp3(b"clip-3".to_vec()).is_ok());
    assert_eq!(player.pending_len(), 3);

    let started = Instant::now();
    assert_eq!(player.stop_and_clear(), Ok(()));
    let stop_elapsed = started.elapsed();

    assert!(
        stop_elapsed < Duration::from_millis(200),
        "stop_and_clear took {:?}, expected < 200ms",
        stop_elapsed
    );
    assert!(player.is_idle());
    assert!(!player.is_playing());
    assert_eq!(player.pending_len(), 0);

    let (_, played_before_recovery, _, stop_count) = controller.snapshot();
    assert_eq!(stop_count, 1);
    assert_eq!(played_before_recovery, vec!["clip-0".to_string()]);

    // Confirm player recovers cleanly for subsequent speech after stop_and_clear.
    assert!(player.enqueue_mp3(b"clip-4".to_vec()).is_ok());
    controller.wait_until_active("clip-4", Duration::from_secs(1));
    controller.finish_active_clip();
    wait_until_idle(&player, Duration::from_secs(1));

    let (_, played_after_recovery, _, _) = controller.snapshot();
    assert_eq!(
        played_after_recovery,
        vec!["clip-0".to_string(), "clip-4".to_string()]
    );
}

fn synthetic_silent_mp3_frames(frames: usize) -> Vec<u8> {
    let mut frame = vec![0u8; 417];
    frame[0] = 0xFF;
    frame[1] = 0xFB;
    frame[2] = 0x90;
    frame[3] = 0xC4;

    let mut bytes = Vec::with_capacity(417 * frames);
    for _ in 0..frames {
        bytes.extend_from_slice(&frame);
    }
    bytes
}

#[test]
fn audio_player_surfaces_device_open_and_decode_errors_without_panicking() {
    let err_init = AudioPlayer::with_backend(
        DEFAULT_VOLUME,
        DEFAULT_MAX_PENDING_CLIPS,
        |_| -> Result<FakeAudioBackend, AudioError> {
            Err(AudioError::DeviceOpenFailed {
                detail: "no output device".to_string(),
            })
        },
        accept_non_empty,
    );
    assert_eq!(
        err_init.err(),
        Some(AudioError::DeviceOpenFailed {
            detail: "no output device".to_string(),
        })
    );

    // Pre-queue validation rejects corrupt/empty MP3 without evicting valid pending clips.
    let controller = FakeBackendController::new(DEFAULT_VOLUME);
    let backend_ctrl = controller.clone();
    let player_real_validator = match AudioPlayer::with_backend(
        DEFAULT_VOLUME,
        DEFAULT_MAX_PENDING_CLIPS,
        move |_| {
            Ok(FakeAudioBackend {
                controller: backend_ctrl,
            })
        },
        validate_mp3_bytes,
    ) {
        Ok(p) => p,
        Err(err) => panic!("expected fake AudioPlayer init to succeed: {err}"),
    };

    let m0 = synthetic_silent_mp3_frames(4);
    let m1 = synthetic_silent_mp3_frames(5);
    let m2 = synthetic_silent_mp3_frames(6);
    let m3 = synthetic_silent_mp3_frames(7);

    assert!(player_real_validator.enqueue_mp3(m0.clone()).is_ok());
    controller.wait_until_active_bytes(&m0, Duration::from_secs(1));
    assert_eq!(
        player_real_validator.enqueue_mp3(m1.clone()),
        Ok(EnqueueOutcome {
            pending_len: 1,
            dropped_oldest: false,
        })
    );
    assert_eq!(
        player_real_validator.enqueue_mp3(m2.clone()),
        Ok(EnqueueOutcome {
            pending_len: 2,
            dropped_oldest: false,
        })
    );
    assert_eq!(
        player_real_validator.enqueue_mp3(m3.clone()),
        Ok(EnqueueOutcome {
            pending_len: 3,
            dropped_oldest: false,
        })
    );

    assert_eq!(
        player_real_validator.enqueue_mp3(Vec::new()),
        Err(AudioError::EmptyAudio)
    );
    assert!(matches!(
        player_real_validator.enqueue_mp3(b"corrupt-mp3-data".to_vec()),
        Err(AudioError::DecodeFailed { .. })
    ));
    assert!(matches!(
        player_real_validator.last_error(),
        Some(AudioError::DecodeFailed { .. })
    ));
    assert_eq!(player_real_validator.pending_len(), 3);

    controller.finish_active_clip();
    controller.wait_until_active_bytes(&m1, Duration::from_secs(1));
    controller.finish_active_clip();
    controller.wait_until_active_bytes(&m2, Duration::from_secs(1));
    controller.finish_active_clip();
    controller.wait_until_active_bytes(&m3, Duration::from_secs(1));
    controller.finish_active_clip();
    wait_until_idle(&player_real_validator, Duration::from_secs(1));
    assert_eq!(controller.played_payloads(), vec![m0, m1, m2, m3]);

    // Worker-side play_mp3 decode failure records last_error without killing the worker,
    // and clears last_error once the next valid clip plays.
    let controller2 = FakeBackendController::new(DEFAULT_VOLUME);
    let backend_ctrl2 = controller2.clone();
    let player = match AudioPlayer::with_backend(
        DEFAULT_VOLUME,
        DEFAULT_MAX_PENDING_CLIPS,
        move |_| {
            Ok(FakeAudioBackend {
                controller: backend_ctrl2,
            })
        },
        accept_non_empty,
    ) {
        Ok(p) => p,
        Err(err) => panic!("expected fake AudioPlayer init to succeed: {err}"),
    };

    controller2.set_fail_next_play(AudioError::DecodeFailed {
        detail: "corrupt frame".to_string(),
    });
    assert!(player.enqueue_mp3(b"bad-clip".to_vec()).is_ok());

    let err_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < err_deadline && player.last_error().is_none() {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        player.last_error(),
        Some(AudioError::DecodeFailed {
            detail: "corrupt frame".to_string(),
        })
    );
    assert_eq!(
        player.take_last_error(),
        Some(AudioError::DecodeFailed {
            detail: "corrupt frame".to_string(),
        })
    );
    assert_eq!(player.last_error(), None);

    assert!(player.enqueue_mp3(b"good-clip".to_vec()).is_ok());
    controller2.wait_until_active("good-clip", Duration::from_secs(1));
    controller2.finish_active_clip();
    wait_until_idle(&player, Duration::from_secs(1));
    assert_eq!(player.last_error(), None);
}

#[tokio::test]
#[ignore = "plays real Vietnamese MP3 clips through Windows PC audio output"]
async fn real_pc_plays_two_vietnamese_mp3_clips_sequentially_with_volume_and_stop_and_clear() {
    // Native Edge-TTS client (D1+A5): no Python server.
    let tts = EdgeTtsEngine::new();

    let clip1 = match tts
        .synthesize(
            "Đêm đã khuya, mưa vẫn còn rơi.",
            "vi-VN-NamMinhNeural",
            "+0%",
        )
        .await
    {
        Ok(synthesis) => synthesis.audio,
        Err(err) => panic!("Edge-TTS clip1 synthesis failed: {err}"),
    };
    let clip2 = match tts
        .synthesize(
            "Ngày mai chúng ta sẽ bàn tiếp.",
            "vi-VN-NamMinhNeural",
            "+0%",
        )
        .await
    {
        Ok(synthesis) => synthesis.audio,
        Err(err) => panic!("Edge-TTS clip2 synthesis failed: {err}"),
    };
    let clip3 = match tts
        .synthesize(
            "Đây là câu thử nghiệm dài hơn một chút để kiểm tra tính năng dừng phát âm thanh ngay lập tức trong hai trăm mili giây.",
            "vi-VN-NamMinhNeural",
            "+0%",
        )
        .await
    {
        Ok(synthesis) => synthesis.audio,
        Err(err) => panic!("Edge-TTS clip3 synthesis failed: {err}"),
    };

    let player = match AudioPlayer::new(DEFAULT_VOLUME) {
        Ok(p) => p,
        Err(err) => panic!("AudioPlayer::new failed on real PC audio device: {err}"),
    };

    let seq_started = Instant::now();
    assert_eq!(
        player.enqueue_mp3(clip1.clone()),
        Ok(EnqueueOutcome {
            pending_len: 1,
            dropped_oldest: false,
        })
    );
    assert!(player.enqueue_mp3(clip2.clone()).is_ok());

    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(player.set_volume(0.40), Ok(()));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(player.set_volume(0.85), Ok(()));

    wait_until_idle(&player, Duration::from_secs(20));
    let sequential_ms = seq_started.elapsed().as_millis();
    assert_eq!(player.last_error(), None);
    assert!(
        sequential_ms >= 1000,
        "expected sequential real speech playback >= 1000 ms, got {sequential_ms} ms"
    );

    assert!(player.enqueue_mp3(clip3.clone()).is_ok());
    assert!(player.enqueue_mp3(clip1.clone()).is_ok());

    let playing_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < playing_deadline && !player.is_playing() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        player.is_playing(),
        "expected AudioPlayer to be actively playing clip3 before stop_and_clear"
    );
    std::thread::sleep(Duration::from_millis(300));

    let stop_started = Instant::now();
    assert_eq!(player.stop_and_clear(), Ok(()));
    let stop_elapsed = stop_started.elapsed();
    let stop_and_clear_ms = stop_elapsed.as_millis();
    assert!(
        stop_elapsed < Duration::from_millis(200),
        "real-PC stop_and_clear took {stop_elapsed:?}, expected < 200 ms"
    );
    assert!(player.is_idle());
    assert!(!player.is_playing());
    assert_eq!(player.pending_len(), 0);
    assert_eq!(player.last_error(), None);

    println!(
        "real-PC audio check OK: clip1_bytes={}, clip2_bytes={}, clip3_bytes={}, sequential_ms={sequential_ms}, stop_and_clear_ms={stop_and_clear_ms}",
        clip1.len(),
        clip2.len(),
        clip3.len(),
    );

    drop(player);
}
