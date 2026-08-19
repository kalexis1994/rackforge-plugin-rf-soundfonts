//! A background reader that keeps sounding voices supplied from disk.
//!
//! The division of labour follows LinuxSampler, which has been doing this on
//! modest hardware for twenty years. Every sample keeps its head in memory, so
//! a note always starts instantly. A voice that outlives that head takes a
//! *stream*: a ring buffer filled by this thread and drained by the audio
//! thread, which never performs I/O and never blocks.
//!
//! Three of its design choices are load-bearing and were not obvious:
//!
//! **More streams than voices.** A key released at the end of a phrase is
//! still sounding through its release tail and still needs its buffer. Sizing
//! the pool to the voice count would starve exactly the notes a player is
//! listening to fade.
//!
//! **A bounded refill per cycle.** Filling one stream to the brim while
//! another runs dry converts a busy passage into a dropout on a single voice.
//! Each pass tops up a few streams by a capped amount, so pressure is shared.
//!
//! **Sleeping when idle.** A reader that spins burns a core the audio thread
//! may want. It wakes on demand and when a stream falls below its refill
//! threshold.
//!
//! Measured margins on the target hardware: p99 random read of 6 ms against a
//! ring holding 0.74 s of audio, and 11 MB/s of demand at 64 voices against
//! 43-60 MB/s available.

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::SoundfontError;
use crate::pcm_cache::{self, CacheHeader};
use crate::sample_store::StreamedSample;
use crate::spsc::SpscRing;

/// Frames each stream buffers ahead. About 0.74 s at 44.1 kHz.
pub const STREAM_RING_FRAMES: usize = 32_768;

/// Widest sample the engine accepts, which sizes every ring.
const MAX_CHANNELS: usize = 2;

/// Streams available at once. Deliberately above any sensible voice count.
pub const MAX_STREAMS: usize = 72;

/// Streams topped up per pass, so no single voice monopolises the reader.
const REFILL_STREAMS_PER_RUN: usize = 8;

/// A stream is refilled once it has fallen this far below full.
const REFILL_THRESHOLD_FRAMES: usize = STREAM_RING_FRAMES / 4;

/// Frames moved into one stream in one pass.
const MAX_REFILL_FRAMES: usize = 8_192;

/// How long the reader sleeps when nothing needs attention.
const IDLE_NAP: Duration = Duration::from_millis(2);

/// Lifecycle of a slot, as seen by both threads.
mod state {
    /// Nobody owns this slot.
    pub const FREE: u8 = 0;
    /// The audio thread claimed it; the reader has not opened it yet.
    pub const CLAIMED: u8 = 1;
    /// The reader is filling it.
    pub const ACTIVE: u8 = 2;
    /// The voice ended; the reader should close it and free the slot.
    pub const RELEASING: u8 = 3;
}

/// One stream's shared state.
#[derive(Debug)]
struct Slot {
    state: AtomicU8,
    ring: SpscRing<f32>,
    /// Set by the audio thread before claiming; read by the reader.
    request: Mutex<Option<Request>>,
    /// True once the reader has delivered the final frame.
    exhausted: AtomicBool,
    /// Counts refills that found the ring already empty.
    starved: AtomicUsize,
}

#[derive(Clone, Debug)]
struct Request {
    cache_path: PathBuf,
    header: CacheHeader,
    first_frame: usize,
}

/// The audio thread's handle to one stream.
///
/// Dropping it returns the slot, so a voice that ends for any reason —
/// including a panic unwinding through the engine — cannot leak a stream.
#[derive(Debug)]
pub struct StreamReader {
    slot: Arc<Slot>,
    shared: Arc<Shared>,
    channels: usize,
}

impl StreamReader {
    /// Takes up to `out.len()` samples, returning how many were available.
    ///
    /// Never blocks and never allocates. A short read means the reader has not
    /// caught up; the caller decides whether to hold the last frame or fade.
    pub fn read(&self, out: &mut [f32]) -> usize {
        let taken = self.slot.ring.pop_slice(out);
        if taken < out.len() && !self.is_exhausted() {
            self.slot.starved.fetch_add(1, Ordering::Relaxed);
            self.shared.wake();
        }
        taken
    }

    /// Whether the reader has delivered the last frame of the sample.
    pub fn is_exhausted(&self) -> bool {
        self.slot.exhausted.load(Ordering::Acquire) && self.slot.ring.is_empty()
    }

    /// Frames buffered ahead, for diagnosis.
    pub fn buffered_frames(&self) -> usize {
        self.slot.ring.len() / self.channels.max(1)
    }

    /// Times this stream was read faster than it could be filled.
    pub fn starved_count(&self) -> usize {
        self.slot.starved.load(Ordering::Relaxed)
    }
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        self.slot.state.store(state::RELEASING, Ordering::Release);
        self.shared.wake();
    }
}

/// State shared between the audio thread and the reader.
#[derive(Debug)]
struct Shared {
    slots: Vec<Arc<Slot>>,
    running: AtomicBool,
    /// Only ever locked briefly by the reader and by `wake`, and never held
    /// across I/O, so the audio thread's `notify_one` cannot be delayed.
    signal: Mutex<bool>,
    condition: Condvar,
}

impl Shared {
    fn wake(&self) {
        if let Ok(mut pending) = self.signal.lock() {
            *pending = true;
        }
        self.condition.notify_one();
    }
}

/// Owns the reader thread and the pool of streams.
pub struct Streamer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Streamer {
    /// Starts the reader with a preallocated pool.
    ///
    /// Every ring is allocated here so that claiming a stream later, on the
    /// audio thread, touches no allocator.
    pub fn start() -> Self {
        let slots: Vec<Arc<Slot>> = (0..MAX_STREAMS)
            .map(|_| {
                Arc::new(Slot {
                    state: AtomicU8::new(state::FREE),
                    ring: SpscRing::new(STREAM_RING_FRAMES * MAX_CHANNELS),
                    request: Mutex::new(None),
                    exhausted: AtomicBool::new(false),
                    starved: AtomicUsize::new(0),
                })
            })
            .collect();
        let shared = Arc::new(Shared {
            slots,
            running: AtomicBool::new(true),
            signal: Mutex::new(false),
            condition: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("rf-soundfonts-streamer".into())
            .spawn(move || {
                elevate_priority();
                run(worker)
            })
            .ok();
        Self { shared, thread }
    }

    /// Claims a stream for a voice, starting at `first_frame`.
    ///
    /// Returns `None` when every stream is busy. That is a real condition, not
    /// an error: the caller should let the note play from its resident head
    /// and stop when it runs out, which is far better than failing the note.
    pub fn claim(&self, sample: &StreamedSample, first_frame: usize) -> Option<StreamReader> {
        let channels = usize::from(sample.channels).max(1);
        for slot in &self.shared.slots {
            if slot
                .state
                .compare_exchange(
                    state::FREE,
                    state::CLAIMED,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue;
            }
            slot.ring.clear();
            slot.exhausted.store(false, Ordering::Release);
            slot.starved.store(0, Ordering::Relaxed);
            // Only ever contended against the reader picking up a request, and
            // held for a pointer move; never across I/O.
            if let Ok(mut request) = slot.request.lock() {
                *request = Some(Request {
                    cache_path: sample.cache_path.clone(),
                    header: sample.header,
                    first_frame,
                });
            }
            self.shared.wake();
            return Some(StreamReader {
                slot: Arc::clone(slot),
                shared: Arc::clone(&self.shared),
                channels,
            });
        }
        None
    }

    /// Streams currently in use.
    pub fn active_streams(&self) -> usize {
        self.shared
            .slots
            .iter()
            .filter(|slot| slot.state.load(Ordering::Relaxed) != state::FREE)
            .count()
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::Release);
        self.shared.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Real-time priority requested for the reader.
///
/// Below the audio thread, which runs at 75, so a reader that overruns can
/// never delay a callback. Above ordinary work, so the reader cannot be held
/// off by a web request or a controller refresh at the one moment it matters:
/// the instant a note outlives its resident head, three quarters of a second
/// after every key press.
#[cfg(target_os = "linux")]
const READER_PRIORITY: i32 = 40;

/// Asks for real-time scheduling, and carries on without it.
///
/// A reader on the ordinary scheduler still works while the machine is quiet;
/// it only becomes audible under contention. Failing to start over a limit the
/// platform did not grant would trade an occasional artefact for no sound.
#[cfg(target_os = "linux")]
fn elevate_priority() {
    let parameters = libc::sched_param {
        sched_priority: READER_PRIORITY,
    };
    // SAFETY: `parameters` is fully initialised and pid 0 names this thread.
    unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &parameters) };
}

#[cfg(not(target_os = "linux"))]
fn elevate_priority() {}

/// One open cache file per stream in flight.
struct OpenStream {
    file: File,
    header: CacheHeader,
    next_frame: usize,
}

fn run(shared: Arc<Shared>) {
    let mut open: HashMap<usize, OpenStream> = HashMap::new();
    // Sized for the widest sample the engine accepts, so one buffer serves
    // mono and stereo streams alike without reallocating per note.
    let mut block = vec![0.0_f32; MAX_REFILL_FRAMES * MAX_CHANNELS];
    let mut scratch: Vec<u8> = Vec::new();
    // Round-robin cursor, so the same few streams are not always served first.
    let mut cursor = 0_usize;

    while shared.running.load(Ordering::Acquire) {
        let mut worked = false;
        let mut served = 0;

        for step in 0..shared.slots.len() {
            let index = (cursor + step) % shared.slots.len();
            let slot = &shared.slots[index];
            match slot.state.load(Ordering::Acquire) {
                state::CLAIMED => {
                    let request = slot.request.lock().ok().and_then(|mut held| held.take());
                    let Some(request) = request else { continue };
                    match File::open(&request.cache_path) {
                        Ok(file) => {
                            open.insert(
                                index,
                                OpenStream {
                                    file,
                                    header: request.header,
                                    next_frame: request.first_frame,
                                },
                            );
                            slot.state.store(state::ACTIVE, Ordering::Release);
                        }
                        Err(_) => {
                            // Nothing can be delivered; mark it finished so the
                            // voice stops rather than waiting forever.
                            slot.exhausted.store(true, Ordering::Release);
                            slot.state.store(state::ACTIVE, Ordering::Release);
                        }
                    }
                    worked = true;
                }
                state::RELEASING => {
                    open.remove(&index);
                    slot.ring.clear();
                    slot.state.store(state::FREE, Ordering::Release);
                    worked = true;
                }
                state::ACTIVE => {
                    if served >= REFILL_STREAMS_PER_RUN {
                        continue;
                    }
                    let Some(stream) = open.get_mut(&index) else {
                        continue;
                    };
                    // Taken from the sample rather than from the streamer: a
                    // library may mix mono and stereo material, and using one
                    // global width would misread every sample of the other.
                    let channels = usize::from(stream.header.channels).max(1);
                    let vacancy_frames = slot.ring.vacancy() / channels;
                    if vacancy_frames < REFILL_THRESHOLD_FRAMES {
                        continue;
                    }
                    let wanted = vacancy_frames.min(MAX_REFILL_FRAMES);
                    let out = &mut block[..wanted * channels];
                    match pcm_cache::read_frames(
                        &stream.file,
                        &stream.header,
                        stream.next_frame,
                        out,
                        &mut scratch,
                    ) {
                        Ok(0) => {
                            slot.exhausted.store(true, Ordering::Release);
                        }
                        Ok(frames) => {
                            let pushed = slot.ring.push_slice(&out[..frames * channels]);
                            stream.next_frame += pushed / channels;
                            if stream.next_frame >= stream.header.frame_count {
                                slot.exhausted.store(true, Ordering::Release);
                            }
                            served += 1;
                            worked = true;
                        }
                        Err(_) => {
                            // A read that fails mid-note ends the note rather
                            // than repeating whatever the ring last held.
                            slot.exhausted.store(true, Ordering::Release);
                        }
                    }
                }
                _ => {}
            }
        }
        cursor = (cursor + REFILL_STREAMS_PER_RUN) % shared.slots.len();

        if !worked {
            let Ok(guard) = shared.signal.lock() else {
                break;
            };
            let (mut pending, _) = shared
                .condition
                .wait_timeout(guard, IDLE_NAP)
                .unwrap_or_else(|error| error.into_inner());
            *pending = false;
        }
    }
}

/// Frames a voice keeps ahead of its playback cursor.
///
/// Small on purpose. It exists only to give the interpolator two adjacent
/// frames to work with; the real buffering is the stream's ring, which holds
/// three orders of magnitude more.
const WINDOW_FRAMES: usize = 4_096;

/// Frames kept behind the cursor so a repeated index is still readable.
///
/// Two would do for the 44.1-to-48 kHz case; sixteen also covers a note played
/// well below its root, where the cursor crawls and repeats an index often.
const HISTORY_FRAMES: usize = 16;

/// A voice's sliding view of a sample whose tail lives on disk.
///
/// Playback only ever moves forward, which is what makes a window possible at
/// all: frames behind the cursor can be discarded and never asked for again.
/// A looping region would break that assumption by jumping backwards, so such
/// regions are loaded whole rather than streamed.
#[derive(Debug)]
pub struct StreamWindow {
    reader: StreamReader,
    channels: usize,
    /// Interleaved frames, valid in `..filled`.
    window: Box<[f32]>,
    /// Absolute frame index of the first frame in `window`.
    start_frame: usize,
    /// Frames currently valid.
    filled: usize,
    /// Times a frame was wanted before the reader could supply it.
    starved_frames: usize,
}

impl StreamWindow {
    /// Wraps a reader that resumes at `first_streamed_frame`.
    pub fn new(reader: StreamReader, channels: usize, first_streamed_frame: usize) -> Self {
        Self {
            reader,
            channels,
            window: vec![0.0; WINDOW_FRAMES * channels].into_boxed_slice(),
            start_frame: first_streamed_frame,
            filled: 0,
            starved_frames: 0,
        }
    }

    /// Frames wanted before the reader could supply them.
    pub fn starved_frames(&self) -> usize {
        self.starved_frames
    }

    /// Whether the sample has ended and the window is drained.
    pub fn is_finished(&self, frame: usize) -> bool {
        self.reader.is_exhausted() && frame >= self.start_frame + self.filled
    }

    /// Returns one frame's samples, fetching more from the reader if needed.
    ///
    /// Returns `None` when the reader has not caught up. The caller should
    /// emit silence for that frame rather than repeat the last one, which
    /// would leave a DC step behind when the stream resumes.
    pub fn frame(&mut self, frame: usize) -> Option<&[f32]> {
        if frame < self.start_frame {
            // Behind the window. With the history margin this should not
            // happen, but it is counted rather than passed over silently: an
            // unexplained gap in a note is precisely what took a measurement
            // to find last time.
            self.starved_frames += 1;
            return None;
        }
        if frame >= self.start_frame + self.filled {
            self.advance_to(frame);
        }
        let offset = frame.checked_sub(self.start_frame)?;
        if offset >= self.filled {
            self.starved_frames += 1;
            return None;
        }
        let base = offset * self.channels;
        Some(&self.window[base..base + self.channels])
    }

    /// Discards consumed frames and pulls more from the reader.
    ///
    /// Never allocates: the window is a fixed buffer and compaction is a move
    /// within it. Compaction happens roughly once per window of output, which
    /// at 48 kHz is a few dozen times a second per voice.
    ///
    /// A few frames behind the cursor are always kept. The interpolator asks
    /// for frame *n* and then frame *n+1*, and a 44.1 kHz sample played at
    /// 48 kHz advances by 0.919 frames per output frame — so `floor()` repeats
    /// an index roughly once every twelve frames. Discarding everything below
    /// the highest frame requested would leave that repeat behind the window,
    /// and a frame the window cannot supply becomes silence: two clicks, one
    /// entering the gap and one leaving it, on whichever notes happen to align
    /// that way.
    fn advance_to(&mut self, frame: usize) {
        let target = frame.saturating_sub(HISTORY_FRAMES);
        let consumed = target.saturating_sub(self.start_frame).min(self.filled);
        if consumed > 0 {
            self.window
                .copy_within(consumed * self.channels..self.filled * self.channels, 0);
            self.filled -= consumed;
            self.start_frame += consumed;
        }
        if frame.saturating_sub(HISTORY_FRAMES) > self.start_frame {
            // The cursor jumped past everything buffered; drop the gap so the
            // window realigns rather than serving stale audio.
            self.filled = 0;
            self.start_frame = frame;
        }
        let space = (self.window.len() / self.channels) - self.filled;
        if space == 0 {
            return;
        }
        let taken = self
            .reader
            .read(&mut self.window[self.filled * self.channels..]);
        self.filled += taken / self.channels;
    }
}

/// Reads a stream to its end, for tests and offline rendering.
pub fn drain(
    reader: &StreamReader,
    out: &mut Vec<f32>,
    limit: usize,
) -> Result<(), SoundfontError> {
    let mut block = [0.0_f32; 1024];
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while out.len() < limit {
        let taken = reader.read(&mut block);
        if taken == 0 {
            if reader.is_exhausted() {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(SoundfontError::Invalid("stream stalled".into()));
            }
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        out.extend_from_slice(&block[..taken]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Wave;
    use crate::pcm_cache::CacheFormat;
    use std::fs;
    use std::path::Path;

    fn temp_root() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "rf-soundfonts-streamer-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    /// A cached ramp whose value at frame *n* is `n`, so any misalignment in
    /// the pipeline shows up as a wrong number rather than as plausible audio.
    fn ramp(root: &Path, name: &str, frames: usize, channels: u8) -> StreamedSample {
        let samples: Vec<f32> = (0..frames)
            .flat_map(|frame| (0..channels).map(move |_| frame as f32))
            .collect();
        let wave = Wave {
            name: name.into(),
            sample_rate: 44_100,
            channels,
            source_bits: 32,
            samples: Arc::from(samples),
            sample_params: None,
        };
        let cache_path = root.join(format!("{name}.pcm"));
        let header = pcm_cache::write(&cache_path, &wave, CacheFormat::Float32).unwrap();
        StreamedSample {
            name: name.into(),
            sample_rate: 44_100,
            channels,
            frame_count: frames,
            preload: Arc::from(vec![0.0_f32; 0]),
            preload_frames: 0,
            cache_path,
            header,
        }
    }

    #[test]
    fn a_stream_delivers_the_sample_in_order() {
        let root = temp_root();
        let sample = ramp(&root, "ramp", 5_000, 1);
        let streamer = Streamer::start();
        let reader = streamer.claim(&sample, 0).unwrap();
        let mut out = Vec::new();
        drain(&reader, &mut out, 5_000).unwrap();
        assert_eq!(out.len(), 5_000);
        for (frame, value) in out.iter().enumerate() {
            assert_eq!(*value, frame as f32, "frame {frame} arrived wrong");
        }
    }

    #[test]
    fn a_stream_can_start_anywhere_in_the_sample() {
        // The property preloading depends on: the reader picks up exactly
        // where the resident head stopped.
        let root = temp_root();
        let sample = ramp(&root, "offset", 4_000, 1);
        let streamer = Streamer::start();
        let reader = streamer.claim(&sample, 1_000).unwrap();
        let mut out = Vec::new();
        drain(&reader, &mut out, 100).unwrap();
        assert_eq!(out[0], 1_000.0, "the tail did not resume at the head's end");
        assert_eq!(out[99], 1_099.0);
    }

    #[test]
    fn a_stereo_stream_keeps_its_channels_interleaved() {
        let root = temp_root();
        let sample = ramp(&root, "stereo", 2_000, 2);
        let streamer = Streamer::start();
        let reader = streamer.claim(&sample, 0).unwrap();
        let mut out = Vec::new();
        drain(&reader, &mut out, 400).unwrap();
        // Both channels of frame n hold n.
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
        assert_eq!(out[2], 1.0);
        assert_eq!(out[3], 1.0);
    }

    #[test]
    fn a_stream_reports_the_end_of_its_sample() {
        let root = temp_root();
        let sample = ramp(&root, "short", 200, 1);
        let streamer = Streamer::start();
        let reader = streamer.claim(&sample, 0).unwrap();
        let mut out = Vec::new();
        drain(&reader, &mut out, 10_000).unwrap();
        assert_eq!(out.len(), 200);
        assert!(reader.is_exhausted());
    }

    #[test]
    fn dropping_a_reader_returns_its_stream() {
        let root = temp_root();
        let sample = ramp(&root, "recycle", 1_000, 1);
        let streamer = Streamer::start();
        {
            let _reader = streamer.claim(&sample, 0).unwrap();
            assert_eq!(streamer.active_streams(), 1);
        }
        // The reader thread reclaims asynchronously.
        for _ in 0..200 {
            if streamer.active_streams() == 0 {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("a finished voice leaked its stream");
    }

    #[test]
    fn the_pool_refuses_rather_than_growing_without_bound() {
        let root = temp_root();
        let sample = ramp(&root, "pool", 100_000, 1);
        let streamer = Streamer::start();
        let readers: Vec<StreamReader> = (0..MAX_STREAMS)
            .filter_map(|_| streamer.claim(&sample, 0))
            .collect();
        assert_eq!(readers.len(), MAX_STREAMS);
        assert!(
            streamer.claim(&sample, 0).is_none(),
            "the pool handed out a stream it does not have"
        );
    }

    #[test]
    fn a_missing_cache_ends_the_note_instead_of_hanging_it() {
        let root = temp_root();
        let mut sample = ramp(&root, "gone", 1_000, 1);
        fs::remove_file(&sample.cache_path).unwrap();
        sample.frame_count = 1_000;
        let streamer = Streamer::start();
        let reader = streamer.claim(&sample, 0).unwrap();
        for _ in 0..500 {
            if reader.is_exhausted() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("a voice waited forever on a cache that will never arrive");
    }

    #[test]
    fn many_streams_run_together_without_crossing_over() {
        // Each stream reads a different sample; a mix-up would show as one
        // stream returning another's values.
        let root = temp_root();
        let streamer = Streamer::start();
        let samples: Vec<StreamedSample> = (0..16)
            .map(|index| ramp(&root, &format!("voice{index}"), 3_000, 1))
            .collect();
        let readers: Vec<StreamReader> = samples
            .iter()
            .enumerate()
            .map(|(index, sample)| streamer.claim(sample, index * 10).unwrap())
            .collect();
        for (index, reader) in readers.iter().enumerate() {
            let mut out = Vec::new();
            drain(reader, &mut out, 50).unwrap();
            assert_eq!(
                out[0],
                (index * 10) as f32,
                "stream {index} returned another stream's audio"
            );
        }
    }

    /// Builds a sample whose head is resident and whose tail is cached, with
    /// the value of frame *n* equal to *n* throughout both halves.
    fn split_ramp(root: &Path, name: &str, frames: usize, preload: usize) -> StreamedSample {
        let sample = ramp(root, name, frames, 1);
        let head: Vec<f32> = (0..preload).map(|frame| frame as f32).collect();
        StreamedSample {
            preload: Arc::from(head),
            preload_frames: preload,
            ..sample
        }
    }

    #[test]
    fn a_voice_crosses_from_its_head_into_the_stream_without_a_seam() {
        // The property the whole subsystem exists to provide: the listener
        // cannot tell where memory ended and the disk began.
        let root = temp_root();
        let sample = split_ramp(&root, "seam", 20_000, 1_000);
        let streamer = Streamer::start();
        let reader = streamer.claim(&sample, sample.preload_frames).unwrap();
        let params = crate::SampleParams {
            unity_note: 60,
            fine_tune: 0,
            attenuation_db: 0.0,
            sample_loop: None,
        };
        let mut config = crate::VoiceConfig::inherit(&crate::Instrument {
            name: "seam".into(),
            bank: 0,
            program: 0,
            regions: Vec::new(),
            envelope: crate::EnvelopeSpec::default(),
            pitch_envelope: crate::PitchEnvelopeSpec::default(),
            lfo: crate::LfoSpec::default(),
        });
        // Remove every gain shaping so the output is the raw ramp.
        config.velocity_tracking = 0.0;
        let mut voice =
            crate::Voice::from_streamed(&sample, Some(reader), params, 60, 127, 44_100, config)
                .unwrap();

        // Let the reader fill before playing, as a real note would while its
        // head plays out.
        thread::sleep(Duration::from_millis(50));

        let mut values = Vec::new();
        for _ in 0..3_000 {
            values.push(voice.next_frame()[0]);
        }
        // Frames on both sides of the boundary must continue the ramp.
        let before = values[990];
        let after = values[1_010];
        assert!(before > 980.0 && before < 1_000.0, "head value {before}");
        assert!(
            after > 1_000.0 && after < 1_020.0,
            "the tail did not continue the head: {after}"
        );
        // No step larger than one frame anywhere across the seam.
        for window in values[900..1_100].windows(2) {
            let step = (window[1] - window[0]).abs();
            assert!(step < 2.0, "a discontinuity of {step} at the seam");
        }
        assert_eq!(voice.starved_frames(), 0, "the reader could not keep up");
    }

    #[test]
    fn a_voice_without_a_stream_plays_its_head_and_stops() {
        // What happens when the pool is exhausted: a shorter note, not a
        // failed one.
        let root = temp_root();
        let sample = split_ramp(&root, "headonly", 20_000, 500);
        let params = crate::SampleParams {
            unity_note: 60,
            fine_tune: 0,
            attenuation_db: 0.0,
            sample_loop: None,
        };
        let config = crate::VoiceConfig {
            velocity_tracking: 0.0,
            ..crate::VoiceConfig::inherit(&crate::Instrument {
                name: "headonly".into(),
                bank: 0,
                program: 0,
                regions: Vec::new(),
                envelope: crate::EnvelopeSpec::default(),
                pitch_envelope: crate::PitchEnvelopeSpec::default(),
                lfo: crate::LfoSpec::default(),
            })
        };
        let mut voice =
            crate::Voice::from_streamed(&sample, None, params, 60, 127, 44_100, config).unwrap();
        let mut sounded = 0;
        for _ in 0..2_000 {
            if voice.next_frame()[0].abs() > 1e-6 {
                sounded += 1;
            }
        }
        assert!(sounded > 400, "the head did not play at all");
        assert!(sounded < 600, "audio appeared past the resident head");
    }

    #[test]
    fn a_streamer_shuts_its_thread_down_when_dropped() {
        let root = temp_root();
        let sample = ramp(&root, "shutdown", 1_000, 1);
        let streamer = Streamer::start();
        let reader = streamer.claim(&sample, 0).unwrap();
        drop(reader);
        drop(streamer);
    }
}
