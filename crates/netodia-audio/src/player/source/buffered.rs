//! Decoding ahead of the real-time audio callback into a bounded PCM queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::thread;
use std::time::Duration;

use netodia_core::error::AppError;

use super::audio::{AudioSource, ChannelCount, Sample, SampleRate, SeekError, SourceFormat};

const PCM_BLOCK_SAMPLES: usize = 16 * 1024;
const PCM_QUEUE_BLOCKS: usize = 24;
const SOURCE_POLL_INTERVAL: Duration = Duration::from_millis(2);

const BUFFERING: u8 = 0;
const PLAYING: u8 = 1;
const ENDED: u8 = 2;

/// State reported by the buffered decoder source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferState {
    /// The worker has not supplied another PCM block yet.
    Buffering,
    /// PCM is available to the audio callback.
    Playing,
    /// The decoder reached the end of its source.
    Ended,
}

struct Shared {
    cancelled: Arc<AtomicBool>,
    state: AtomicU8,
}

#[derive(Clone, Copy)]
struct Metadata {
    channels: ChannelCount,
    sample_rate: SampleRate,
    format: SourceFormat,
    total_duration: Option<Duration>,
}

/// A decoded source whose producer runs on a worker thread and whose consumer never waits.
///
/// Open this on a blocking worker: it waits for the decoder to open and produce its first bounded
/// PCM block, so the caller can build the output converter with a known shape. Subsequent network
/// reads and codec work happen on the detached producer thread. Dropping the source disconnects
/// the bounded channel and asks that thread to stop.
pub struct BufferedAudioSource {
    receiver: Receiver<Vec<Sample>>,
    current: std::vec::IntoIter<Sample>,
    shared: Arc<Shared>,
    metadata: Metadata,
}

impl BufferedAudioSource {
    /// Opens a blocking decoder on its own thread and returns after its first PCM block is ready.
    pub fn open<F, S>(factory: F) -> Result<Self, AppError>
    where
        F: FnOnce() -> Result<S, AppError> + Send + 'static,
        S: AudioSource + 'static,
    {
        Self::open_cancellable(move |_| factory())
    }

    /// Opens a decoder with a cancellation flag set when its buffered source is retired.
    /// Blocking readers can use it to stop reconnecting after a stop or track change.
    pub fn open_cancellable<F, S>(factory: F) -> Result<Self, AppError>
    where
        F: FnOnce(Arc<AtomicBool>) -> Result<S, AppError> + Send + 'static,
        S: AudioSource + 'static,
    {
        let (sender, receiver) = sync_channel(PCM_QUEUE_BLOCKS);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let shared = Arc::new(Shared {
            cancelled: Arc::new(AtomicBool::new(false)),
            state: AtomicU8::new(BUFFERING),
        });
        let worker_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("netodia-decoder".to_owned())
            .spawn(move || {
                let mut source = match factory(Arc::clone(&worker_shared.cancelled)) {
                    Ok(source) => source,
                    Err(error) => {
                        worker_shared.state.store(ENDED, Ordering::Release);
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                let metadata = Metadata {
                    channels: source.channels(),
                    sample_rate: source.sample_rate(),
                    format: source.format(),
                    total_duration: source.total_duration(),
                };
                let first = read_block(&mut source, &worker_shared.cancelled);
                if first.is_empty() {
                    let _ = ready_sender.send(Err(AppError::Player(
                        "Decoder produced no audio samples".to_owned(),
                    )));
                    return;
                }
                if sender.send(first).is_err() {
                    return;
                }
                if ready_sender.send(Ok(metadata)).is_err() {
                    return;
                }

                while !worker_shared.cancelled.load(Ordering::Acquire) {
                    let block = read_block(&mut source, &worker_shared.cancelled);
                    if block.is_empty() || sender.send(block).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| AppError::io("Unable to start the audio decoder worker", error))?;

        let metadata = ready_receiver.recv().map_err(|_| {
            AppError::Player("Audio decoder worker stopped before opening the source".to_owned())
        })??;
        Ok(Self { receiver, current: Vec::new().into_iter(), shared, metadata })
    }

    /// Current buffering state, suitable for transport status and UI publication.
    #[must_use]
    pub fn state(&self) -> BufferState {
        match self.shared.state.load(Ordering::Acquire) {
            PLAYING => BufferState::Playing,
            ENDED => BufferState::Ended,
            _ => BufferState::Buffering,
        }
    }
}

impl Iterator for BufferedAudioSource {
    type Item = Sample;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(sample) = self.current.next() {
            self.shared.state.store(PLAYING, Ordering::Release);
            return Some(sample);
        }

        match self.receiver.try_recv() {
            Ok(block) => {
                self.current = block.into_iter();
                self.shared.state.store(PLAYING, Ordering::Release);
                self.current.next()
            }
            Err(TryRecvError::Empty) => {
                self.shared.state.store(BUFFERING, Ordering::Release);
                None
            }
            // Disconnection is observed only after every queued block is drained. A separate
            // finished flag can race the producer's last send and discard that final block.
            Err(TryRecvError::Disconnected) => {
                self.shared.state.store(ENDED, Ordering::Release);
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl AudioSource for BufferedAudioSource {
    fn channels(&self) -> ChannelCount {
        self.metadata.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.metadata.sample_rate
    }

    fn format(&self) -> SourceFormat {
        self.metadata.format
    }

    fn dsp_engaged(&self) -> bool {
        false
    }

    fn total_duration(&self) -> Option<Duration> {
        self.metadata.total_duration
    }

    fn is_buffering(&self) -> bool {
        self.state() == BufferState::Buffering
    }

    fn release_claims(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
    }

    fn try_seek(&mut self, _pos: Duration) -> Result<(), SeekError> {
        Err(SeekError::NotSupported { underlying_source: "buffered decoder" })
    }
}

impl Drop for BufferedAudioSource {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
    }
}

fn read_block<S: AudioSource>(source: &mut S, cancelled: &AtomicBool) -> Vec<Sample> {
    let mut block = Vec::with_capacity(PCM_BLOCK_SAMPLES);
    while block.len() < PCM_BLOCK_SAMPLES && !cancelled.load(Ordering::Acquire) {
        if let Some(sample) = source.next() {
            block.push(sample);
        } else if source.is_buffering() {
            thread::sleep(SOURCE_POLL_INTERVAL);
        } else {
            break;
        }
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZero;
    use std::sync::{Condvar, Mutex};

    struct GatedSource {
        position: usize,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Iterator for GatedSource {
        type Item = Sample;

        fn next(&mut self) -> Option<Self::Item> {
            if self.position == PCM_BLOCK_SAMPLES {
                let (released, wake) = &*self.release;
                let mut released = released.lock().ok()?;
                while !*released {
                    released = wake.wait(released).ok()?;
                }
            }
            if self.position > PCM_BLOCK_SAMPLES {
                return None;
            }
            let sample = if self.position == PCM_BLOCK_SAMPLES { 0.75 } else { 0.25 };
            self.position += 1;
            Some(sample)
        }
    }

    impl AudioSource for GatedSource {
        fn channels(&self) -> ChannelCount {
            NonZero::<u16>::MIN
        }

        fn sample_rate(&self) -> SampleRate {
            NonZero::new(44_100).unwrap_or(NonZero::<u32>::MIN)
        }

        fn format(&self) -> SourceFormat {
            SourceFormat::F32
        }

        fn dsp_engaged(&self) -> bool {
            false
        }

        fn total_duration(&self) -> Option<Duration> {
            Some(Duration::from_secs(1))
        }

        fn try_seek(&mut self, _pos: Duration) -> Result<(), SeekError> {
            Err(SeekError::NotSupported { underlying_source: "test source" })
        }
    }

    #[test]
    fn retiring_a_buffered_source_cancels_its_blocking_reader() -> Result<(), AppError> {
        let (sender, receiver) = sync_channel(1);
        let mut source = BufferedAudioSource::open_cancellable(move |cancelled| {
            let _ = sender.send(cancelled);
            Ok(GatedSource { position: 0, release: Arc::new((Mutex::new(true), Condvar::new())) })
        })?;
        let cancelled = receiver
            .recv()
            .map_err(|_| AppError::Player("decoder cancellation flag missing".to_owned()))?;
        assert!(!cancelled.load(Ordering::Acquire));

        source.release_claims();

        assert!(cancelled.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn an_empty_pull_during_buffering_is_not_end_of_stream() -> Result<(), AppError> {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let source_release = Arc::clone(&release);
        let mut source = BufferedAudioSource::open(move || {
            Ok(GatedSource { position: 0, release: source_release })
        })?;

        for _ in 0..PCM_BLOCK_SAMPLES {
            assert_eq!(source.next(), Some(0.25));
        }
        assert_eq!(source.next(), None);
        assert_eq!(source.state(), BufferState::Buffering);
        assert!(source.is_buffering());

        let (released, wake) = &*release;
        *released.lock().map_err(|_| AppError::Player("test gate poisoned".to_owned()))? = true;
        wake.notify_one();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let recovered = loop {
            if let Some(sample) = source.next() {
                break Some(sample);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            thread::sleep(SOURCE_POLL_INTERVAL);
        };
        assert_eq!(recovered, Some(0.75));
        assert_eq!(source.state(), BufferState::Playing);
        Ok(())
    }
}
