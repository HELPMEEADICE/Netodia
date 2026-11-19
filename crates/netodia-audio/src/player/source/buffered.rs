//! Decoding ahead of the real-time audio callback into a bounded PCM queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::thread;
use std::time::Duration;

use crossbeam_queue::ArrayQueue;
use netodia_core::error::AppError;

use super::audio::{AudioSource, ChannelCount, Sample, SampleRate, SeekError, SourceFormat};

const PCM_BLOCK_SAMPLES: usize = 16 * 1024;
const PCM_QUEUE_BLOCKS: usize = 24;
// One block per queued item, plus one each for the callback and decoder. Leave a spare slot
// so returning a block never waits for the decoder to finish releasing a slot it just popped.
const RECYCLE_SLOTS: usize = PCM_QUEUE_BLOCKS + 3;
// Four blocks give even 192 kHz stereo about 170 ms of decoded audio before playback starts.
const LOCAL_PREFILL_BLOCKS: usize = 4;
const SOURCE_POLL_INTERVAL: Duration = Duration::from_millis(2);

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

#[derive(Clone, Copy)]
struct Metadata {
    channels: ChannelCount,
    sample_rate: SampleRate,
    format: SourceFormat,
    total_duration: Option<Duration>,
}

/// A decoded source whose producer runs on a worker thread and whose consumer never waits.
///
/// Open this outside the audio callback: it waits for the decoder and its initial PCM reserve,
/// so the caller can build the output converter with a known shape. Subsequent
/// reads and codec work happen on the detached producer thread. Dropping the source disconnects
/// the bounded channel and asks that thread to stop.
pub struct BufferedAudioSource {
    receiver: Receiver<Vec<Sample>>,
    recycled: Arc<ArrayQueue<Vec<Sample>>>,
    current: Vec<Sample>,
    next_sample: usize,
    cancelled: Arc<AtomicBool>,
    state: BufferState,
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

    /// Move an already opened and positioned file decoder onto a worker. Wait for an initial
    /// reserve (or EOF on a short file) before handing it to the output callback.
    pub fn from_positioned<S: AudioSource + 'static>(source: S) -> Result<Self, AppError> {
        Self::open_with_prefill(move |_| Ok(source), LOCAL_PREFILL_BLOCKS)
    }

    /// Opens a decoder with a cancellation flag set when its buffered source is retired.
    /// Blocking readers can use it to stop reconnecting after a stop or track change.
    pub fn open_cancellable<F, S>(factory: F) -> Result<Self, AppError>
    where
        F: FnOnce(Arc<AtomicBool>) -> Result<S, AppError> + Send + 'static,
        S: AudioSource + 'static,
    {
        Self::open_with_prefill(factory, 1)
    }

    fn open_with_prefill<F, S>(factory: F, prefill_blocks: usize) -> Result<Self, AppError>
    where
        F: FnOnce(Arc<AtomicBool>) -> Result<S, AppError> + Send + 'static,
        S: AudioSource + 'static,
    {
        let (sender, receiver) = sync_channel(PCM_QUEUE_BLOCKS);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let recycled = Arc::new(ArrayQueue::new(RECYCLE_SLOTS));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_recycled = Arc::clone(&recycled);
        thread::Builder::new()
            .name("netodia-decoder".to_owned())
            .spawn(move || {
                let mut source = match factory(Arc::clone(&worker_cancelled)) {
                    Ok(source) => source,
                    Err(error) => {
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
                for index in 0..prefill_blocks {
                    let block = read_block(&mut source, &worker_cancelled, Vec::new());
                    if block.is_empty() {
                        // Publish EOF before letting the consumer pull a short file.
                        drop(sender);
                        let result = if index == 0 {
                            Err(AppError::Player("Decoder produced no audio samples".to_owned()))
                        } else {
                            Ok(metadata)
                        };
                        let _ = ready_sender.send(result);
                        return;
                    }
                    if sender.send(block).is_err() {
                        return;
                    }
                }
                if ready_sender.send(Ok(metadata)).is_err() {
                    return;
                }

                while !worker_cancelled.load(Ordering::Acquire) {
                    let buffer = worker_recycled.pop().unwrap_or_default();
                    let block = read_block(&mut source, &worker_cancelled, buffer);
                    if block.is_empty() || sender.send(block).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| AppError::io("Unable to start the audio decoder worker", error))?;

        let metadata = ready_receiver.recv().map_err(|_| {
            AppError::Player("Audio decoder worker stopped before opening the source".to_owned())
        })??;
        Ok(Self {
            receiver,
            recycled,
            current: Vec::new(),
            next_sample: 0,
            cancelled,
            state: BufferState::Buffering,
            metadata,
        })
    }

    /// Current buffering state, suitable for transport status and UI publication.
    #[must_use]
    pub fn state(&self) -> BufferState {
        self.state
    }
}

impl Iterator for BufferedAudioSource {
    type Item = Sample;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(&sample) = self.current.get(self.next_sample) {
            self.next_sample += 1;
            return Some(sample);
        }

        if !self.current.is_empty() {
            self.current.clear();
            // Return the allocation before receiving again and waking a blocked producer.
            let _ = self.recycled.push(std::mem::take(&mut self.current));
        }

        match self.receiver.try_recv() {
            Ok(block) => {
                self.current = block;
                self.next_sample = 1;
                self.state = BufferState::Playing;
                self.current.first().copied()
            }
            Err(TryRecvError::Empty) => {
                self.state = BufferState::Buffering;
                None
            }
            // Disconnection is observed only after every queued block is drained. A separate
            // finished flag can race the producer's last send and discard that final block.
            Err(TryRecvError::Disconnected) => {
                self.state = BufferState::Ended;
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
        self.cancelled.store(true, Ordering::Release);
    }

    fn try_seek(&mut self, _pos: Duration) -> Result<(), SeekError> {
        Err(SeekError::NotSupported { underlying_source: "buffered decoder" })
    }
}

impl Drop for BufferedAudioSource {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

fn read_block<S: AudioSource>(
    source: &mut S,
    cancelled: &AtomicBool,
    mut block: Vec<Sample>,
) -> Vec<Sample> {
    // A dry queue can interrupt playback at a block boundary. Keep that boundary on a whole
    // frame so an underrun never loses a partial frame and shifts the remaining channels.
    let channels = usize::from(source.channels().get());
    let block_samples = (PCM_BLOCK_SAMPLES / channels).max(1) * channels;
    block.reserve_exact(block_samples);
    while block.len() < block_samples && !cancelled.load(Ordering::Acquire) {
        if let Some(sample) = source.next() {
            block.push(sample);
        } else if source.is_buffering() {
            thread::sleep(SOURCE_POLL_INTERVAL);
        } else {
            break;
        }
    }
    block.truncate(block.len() / channels * channels);
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZero;
    use std::sync::{Condvar, Mutex};

    struct GatedSource {
        position: usize,
        channels: ChannelCount,
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
            self.channels
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
            Ok(GatedSource {
                position: 0,
                channels: NonZero::<u16>::MIN,
                release: Arc::new((Mutex::new(true), Condvar::new())),
            })
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
            Ok(GatedSource { position: 0, channels: NonZero::<u16>::MIN, release: source_release })
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

    #[test]
    fn a_positioned_short_source_drains_to_eof_without_buffering() -> Result<(), AppError> {
        let source = GatedSource {
            position: 0,
            channels: NonZero::<u16>::MIN,
            release: Arc::new((Mutex::new(true), Condvar::new())),
        };
        let mut buffered = BufferedAudioSource::from_positioned(source)?;
        let mut expected = vec![0.25; PCM_BLOCK_SAMPLES];
        expected.push(0.75);

        let samples: Vec<_> = buffered.by_ref().collect();

        assert_eq!(samples, expected);
        assert_eq!(buffered.state(), BufferState::Ended);
        Ok(())
    }

    #[test]
    fn a_three_channel_block_does_not_split_a_frame() {
        let mut source = GatedSource {
            position: 0,
            channels: NonZero::new(3).unwrap_or(NonZero::<u16>::MIN),
            release: Arc::new((Mutex::new(true), Condvar::new())),
        };
        let cancelled = AtomicBool::new(false);
        let mut block = read_block(&mut source, &cancelled, Vec::new());
        assert_eq!(block.len(), 16_383);
        let allocation = block.as_ptr();
        block.clear();
        // The two samples left before EOF do not make a playable frame.
        let tail = read_block(&mut source, &cancelled, block);
        assert_eq!(tail, Vec::<Sample>::new());
        assert_eq!(tail.as_ptr(), allocation);
    }
}
