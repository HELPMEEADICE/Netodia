//! The primitives every outbound fetch in the tree shares.
//!
//! Two rules live here and both are violable from any file, so the corpus pins walk the
//! tree for them rather than listing call sites: a URL arriving from outside the app is
//! **parsed** rather than prefix-tested, and a body is **streamed under a cap** rather than
//! collected and measured afterwards.
//!
//! The lyrics directory client and request pacer sit here beside the shared HTTP primitives.

pub mod lyrics_directory;
pub mod pacer;
pub mod range_reader;

use std::time::Duration;

use netodia_core::error::{AppError, describe};

// Reqwest's TCP connector races the other address family after 300 ms when DNS returns both.
// Give that Happy Eyeballs attempt time to connect before the overall connection deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the process-wide shared `reqwest::Client`. Kept out of any constructor so the rustls
/// stack and connection pool load only on the first real request; both `OnceLock` holders init
/// through this, so the app reuses one pool.
///
/// The deadline is **per read, not whole-body**: a legitimately slow download may take minutes,
/// but no single read should sit silent that long. The build is documented infallible for these
/// options; the fallback is logged paranoia.
pub fn build_http_client() -> reqwest::Client {
    http_client_builder().build().unwrap_or_else(|e| {
        log::warn!(
            "reqwest::Client::builder().build() failed unexpectedly ({}); falling back to \
             default client without timeouts — downloads may hang on a wedged socket",
            describe(&e)
        );
        reqwest::Client::new()
    })
}

fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(Duration::from_mins(1))
        .pool_max_idle_per_host(4)
        .user_agent(concat!("Netodia/", env!("CARGO_PKG_VERSION")))
}

/// `candidate` as an absolute `http`/`https` URL that names a host, or `None`.
///
/// **The parse is the check, and that is the whole point.** A `starts_with("http://")` test admits
/// the bare scheme, which names nothing and is not a fetch anything can make — two of the four
/// spellings this replaced did exactly that, so a line reading `http://` became a bad URL. It also
/// gets case for free, `Url` lowercasing the scheme
/// where a prefix test has to remember to.
///
/// Every URL taken from outside the app goes through here, including artwork URLs and playlist
/// entries.
pub fn http_url(candidate: &str) -> Option<reqwest::Url> {
    let parsed = reqwest::Url::parse(candidate.trim()).ok()?;
    is_http(&parsed).then_some(parsed)
}

/// [`http_url`] where only the verdict is wanted. Two callers wrote this line out for themselves.
pub fn is_http_url(candidate: &str) -> bool {
    http_url(candidate).is_some()
}

/// The rule itself, asked of a URL already parsed. [`http_url`] is this plus the parse.
///
/// `Url::join` returns an absolute URI unchanged, so a playlist line reading `file:///etc/passwd`
/// or `data:…` comes back out of it as a `Url` like any other. Nothing downstream re-asks, and the
/// text form is gone by then, so the check has to be reachable on the parsed value too.
pub fn is_http(url: &reqwest::Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.has_host()
}

/// GET `url` and read at most `max_bytes` of what comes back.
///
/// [`read_capped`] is the half that holds; this is the request around it, plus the two cheap
/// refusals that come before a byte is read — a non-success status, and a `Content-Length` already
/// over the cap. The header check is a courtesy a host can omit or lie about, which is why it sits
/// here rather than instead of the streamed bound.
///
/// `what` is a noun phrase, capitalized as every [`read_capped`] caller passes one: it is the
/// subject of all four messages the two halves can raise, so a refusal points at the right half of
/// a two-request fetch. Timeout and cap are the caller's, which can differ widely by request.
pub async fn get_capped(
    client: &reqwest::Client,
    url: &reqwest::Url,
    what: &str,
    timeout: std::time::Duration,
    max_bytes: u64,
) -> Result<Vec<u8>, AppError> {
    let response = client
        .get(url.clone())
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| AppError::network(format!("{what} could not be fetched"), e))?;
    if !response.status().is_success() {
        return Err(AppError::network_msg(format!(
            "{what} request returned HTTP {}",
            response.status().as_u16()
        )));
    }
    if response.content_length().is_some_and(|len| len > max_bytes) {
        // Worded as `read_capped` words it, the two refusing the same thing from either side of
        // the download.
        return Err(AppError::network_msg(format!("{what} is larger than {max_bytes} bytes")));
    }
    read_capped(response, what, max_bytes).await
}

/// [`get_capped`] for a body that is text.
///
/// `from_utf8` *moves*, where `from_utf8_lossy` on an owned `Vec` copies the lot — once per
/// playlist per reload. Lossy stays on the error arm rather than being dropped for the cheaper
/// spelling: one Latin-1 byte in a track title should cost a replacement character, not the playlist.
pub async fn get_capped_text(
    client: &reqwest::Client,
    url: &reqwest::Url,
    what: &str,
    timeout: std::time::Duration,
    max_bytes: u64,
) -> Result<String, AppError> {
    let body = get_capped(client, url, what, timeout, max_bytes).await?;
    Ok(String::from_utf8(body)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// Ceiling on the capacity a `Content-Length` may claim before a byte has arrived. High enough to
/// skip the cheap end of the growth chain on every body here, low enough that a host overstating
/// its length buys one hint rather than the caller's whole cap, which for the largest of them is
/// two orders of magnitude more.
const READ_HINT_MAX_BYTES: u64 = 64 * 1024;

/// Read at most `max_bytes` of `response`, refusing as soon as the body crosses the cap.
///
/// **Streamed rather than `bytes()`-ed**, and that is the whole point: a `Content-Length` check
/// ahead of the call is a courtesy a host can omit or lie about, so a cap enforced only after
/// `bytes()` has returned has already allocated whatever was sent. **Every response body in the
/// tree is read here**, the updater's streamed-to-disk download aside, and a `.json::<T>()` is not
/// the exemption it looks like: it allocates the whole body before serde sees a byte, so a typed
/// decode bounds the *shape* and nothing about the size. Each caller brings its own `max_bytes` and
/// its own `what`, which names the thing in the error so a refusal points at the right half of a
/// two-request fetch.
pub async fn read_capped(
    response: reqwest::Response,
    what: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, AppError> {
    use futures_util::StreamExt;

    // The same header the cap deliberately doesn't trust is still a fine allocation hint, clamped
    // by [`READ_HINT_MAX_BYTES`] because it is a claim. It buys the reallocations up to the clamp,
    // not the ones past it: a body larger than the hint still grows the rest of the way, which for
    // an HLS segment arriving every few seconds is the point worth being honest about.
    let hint = response.content_length().unwrap_or(0).min(max_bytes).min(READ_HINT_MAX_BYTES);
    let mut body = Vec::with_capacity(usize::try_from(hint).unwrap_or(0));
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|e| AppError::network(format!("{what} could not be read"), e))?;
        if body.len().saturating_add(chunk.len()) as u64 > max_bytes {
            return Err(AppError::network_msg(format!("{what} is larger than {max_bytes} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
#[path = "tests/mod_tests.rs"]
mod tests;
