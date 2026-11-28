use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};

use futures_util::{StreamExt, stream};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_RANGE, ETAG, HeaderValue, IF_RANGE, RANGE};
use tokio::task::JoinHandle;

use super::{
    CHUNK_BYTES, CHUNK_SIZE, CachedMedia, MediaError, RECONNECT_DELAY, REQUEST_TIMEOUT, RangeChunk,
    RangeReader, StatusCode, Storage, content_range, request_error,
};

// The foreground streams the opening segment while three requests fill later segments.
// A fixed segment size limits excess traffic if an urgent seek overtakes the download.
const SEGMENT_BYTES: u64 = 512 * 1024;
const PARALLEL_REQUESTS: usize = 3;
const MAX_TRACK_BYTES: u64 = 128 * 1024 * 1024;

pub(super) fn segment_end(start: u64) -> u64 {
    start | (SEGMENT_BYTES - 1)
}

pub(super) struct BufferTask(JoinHandle<()>);

impl BufferTask {
    pub(super) fn is_running(&self) -> bool {
        !self.0.is_finished()
    }

    pub(super) fn abort(&self) {
        self.0.abort();
    }
}

impl Drop for BufferTask {
    fn drop(&mut self) {
        self.abort();
    }
}

pub(super) struct TrackBuffer {
    // The original probe owns slot zero. Missing slots are still readable via the foreground.
    chunks: Vec<Option<RangeChunk>>,
    byte_len: u64,
    task: Weak<BufferTask>,
}

impl TrackBuffer {
    pub(super) fn chunk(&self, position: u64) -> Option<&RangeChunk> {
        self.chunks.get(chunk_index(position)?)?.as_ref()
    }

    pub(super) fn cache(&mut self, chunk: &RangeChunk) {
        if chunk.start >= CHUNK_BYTES
            && chunk.start < self.byte_len
            && chunk.start.is_multiple_of(CHUNK_BYTES)
            && chunk.bytes.len() as u64 == (self.byte_len - chunk.start).min(CHUNK_BYTES)
            && let Some(slot) =
                chunk_index(chunk.start).and_then(|index| self.chunks.get_mut(index))
            && slot.is_none()
        {
            *slot = Some(chunk.clone());
        }
    }
}

fn chunk_index(start: u64) -> Option<usize> {
    usize::try_from(start / CHUNK_BYTES).ok()
}

pub(super) fn start(reader: &RangeReader, client: reqwest::Client) -> Option<Arc<BufferTask>> {
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let mut media = reader.lock_media();
    let byte_len = media.byte_len?;
    let Storage::Range { head, .. } = &media.storage else { return None };
    if !(SEGMENT_BYTES + 1..=MAX_TRACK_BYTES).contains(&byte_len)
        || head.len() as u64 != CHUNK_BYTES
    {
        return None;
    }
    let chunk_count = chunk_index(byte_len - 1)? + 1;
    let buffer = media.track_buffer.get_or_insert_with(|| TrackBuffer {
        chunks: vec![None; chunk_count],
        byte_len,
        task: Weak::new(),
    });
    if let Some(task) = buffer.task.upgrade().filter(|task| task.is_running()) {
        return Some(task);
    }
    if buffer.chunks[1..].iter().all(Option::is_some) {
        return None;
    }

    let download = Download {
        client,
        url: reader.url.clone(),
        validator: reader.validator.clone(),
        byte_len,
        media: Arc::downgrade(&reader.media),
    };
    let task = Arc::new(BufferTask(runtime.spawn(download.run())));
    buffer.task = Arc::downgrade(&task);
    Some(task)
}

struct Download {
    client: reqwest::Client,
    url: reqwest::Url,
    validator: Option<HeaderValue>,
    byte_len: u64,
    media: Weak<Mutex<CachedMedia>>,
}

impl Download {
    async fn run(self) {
        let segment_starts = (1..self.byte_len.div_ceil(SEGMENT_BYTES)).map(|index| {
            let start = index * SEGMENT_BYTES;
            start..(start + SEGMENT_BYTES).min(self.byte_len)
        });
        let mut requests = stream::iter(segment_starts)
            .map(|segment| self.fetch_and_cache(segment))
            .buffer_unordered(PARALLEL_REQUESTS);
        while let Some(result) = requests.next().await {
            if result.is_err() {
                return;
            }
        }
        // A seek or pause can leave gaps in the opening segment. Fill them after the other work
        // so the foreground gets the first chance to read it on the way to audible playback.
        let _ = self.fetch_and_cache(CHUNK_BYTES..SEGMENT_BYTES).await;
    }

    async fn fetch_and_cache(&self, segment: Range<u64>) -> Result<(), MediaError> {
        let Some(start) = self.first_missing(&segment) else { return Ok(()) };
        let end = segment.end - 1;
        let bytes = match self.fetch(start, end).await {
            Err(
                MediaError::Transport
                | MediaError::Timeout
                | MediaError::HttpStatus(408 | 500..=599),
            ) => {
                tokio::time::sleep(RECONNECT_DELAY).await;
                let Some(start) = self.first_missing(&segment) else { return Ok(()) };
                return self.fetch(start, end).await.map(|bytes| self.cache(start, &bytes));
            }
            result => result?,
        };
        self.cache(start, &bytes);
        Ok(())
    }

    fn first_missing(&self, segment: &Range<u64>) -> Option<u64> {
        let media = self.media.upgrade()?;
        let media = media.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let buffer = media.track_buffer.as_ref()?;
        (chunk_index(segment.start)?..=chunk_index(segment.end - 1)?)
            .find(|&index| buffer.chunks[index].is_none())
            .and_then(|index| u64::try_from(index).ok())
            .map(|index| index * CHUNK_BYTES)
    }

    async fn fetch(&self, start: u64, end: u64) -> Result<Vec<u8>, MediaError> {
        let mut request = self
            .client
            .get(self.url.clone())
            .timeout(REQUEST_TIMEOUT)
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, format!("bytes={start}-{end}"));
        if let Some(validator) = &self.validator {
            request = request.header(IF_RANGE, validator);
        }
        let response = request.send().await.map_err(|error| request_error(&error))?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(MediaError::HttpStatus(response.status().as_u16()));
        }
        let expected_len = end - start + 1;
        if content_range(response.headers().get(CONTENT_RANGE))?
            != (start, end, Some(self.byte_len))
            || response.content_length().is_some_and(|len| len != expected_len)
            || self.validator.as_ref().is_some_and(|expected| {
                response.headers().get(ETAG).is_some_and(|actual| actual != expected)
            })
        {
            return Err(MediaError::InvalidRange);
        }
        let bytes = super::super::read_capped(response, "Media segment", expected_len)
            .await
            .map_err(|_| MediaError::Transport)?;
        if bytes.len() as u64 != expected_len {
            return Err(MediaError::Transport);
        }
        Ok(bytes)
    }

    fn cache(&self, start: u64, bytes: &[u8]) {
        let Some(media) = self.media.upgrade() else { return };
        let mut media = media.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(buffer) = &mut media.track_buffer else { return };
        let mut start = start;
        for bytes in bytes.chunks(CHUNK_SIZE) {
            if buffer.chunk(start).is_none() {
                buffer.cache(&RangeChunk { start, bytes: Arc::from(bytes) });
            }
            start += CHUNK_BYTES;
        }
    }
}
