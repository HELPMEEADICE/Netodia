//! The two primitives every outbound fetch in the tree shares, and what each of them refuses.
//!
//! `crates/netodia/tests/net_primitives.rs` walks the corpus for who calls them, which is a
//! different question and the only one anything was asking: nothing here had ever run.

use netodia_core::error::AppError;
use netodia_testkit::http::{TestResponse, TestServer};

use super::{
    build_http_client, get_capped, get_capped_text, http_client_builder, http_url, is_http,
    is_http_url, read_capped,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The cap every case below is measured against. Small enough that crossing it costs a handful of
/// bytes rather than a transfer.
const CAP: u64 = 32;

/// Well clear of anything a loopback server can take, so a failure here is never the deadline.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `len` bytes of filler. Every size here is a few dozen bytes, so the narrowing cannot fail.
fn filler(len: u64) -> Vec<u8> {
    vec![b'x'; usize::try_from(len).unwrap_or(0)]
}

/// One GET against `server`, as the response [`read_capped`] takes.
async fn response_from(server: &TestServer) -> Result<reqwest::Response, reqwest::Error> {
    reqwest::Client::new().get(server.base_url()).send().await
}

/// [`get_capped`] against `server`'s root, with the shared cap and timeout.
async fn get_from(server: &TestServer) -> Result<Vec<u8>, AppError> {
    let url = http_url(&server.base_url()).ok_or_else(|| AppError::network_msg("bad base url"))?;
    get_capped(&reqwest::Client::new(), &url, "Test body", TIMEOUT, CAP).await
}

// ---- what may be fetched ----

/// The parse is the check. A prefix test admits the bare scheme, which names no host and is not a
/// fetch anything can make; accepting only a URL prefix would still admit a hostless URL.
#[test]
fn a_url_from_outside_the_app_is_parsed_rather_than_pattern_matched() {
    let cases = [
        ("http://example.test/logo.png", true),
        ("https://example.test", true),
        // A pasted address arrives with whatever whitespace came with it.
        ("  https://example.test/  ", true),
        // `Url` lowercases the scheme; a prefix test has to remember to.
        ("HTTPS://Example.test/", true),
        ("http://", false),
        ("https://", false),
        ("file:///etc/passwd", false),
        ("data:image/png;base64,iVBORw0KGgo=", false),
        ("ftp://example.test/logo.png", false),
        ("example.test/logo.png", false),
        ("", false),
    ];
    for (candidate, admitted) in cases {
        assert_eq!(http_url(candidate).is_some(), admitted, "for {candidate:?}");
    }
}

/// `Url::join` hands back an absolute URI unchanged, so a playlist line reading `file:///etc/passwd`
/// comes out of it as a `Url` like any other. The text form is gone by then and nothing downstream
/// re-asks, which is why the rule has to be reachable on the parsed value too.
#[test]
fn the_rule_survives_a_join_that_answers_with_someone_elses_scheme() -> TestResult {
    let playlist = reqwest::Url::parse("https://example.test/live/")?;

    assert!(is_http(&playlist.join("stream.mp3")?), "a relative line resolves against the base");
    assert!(!is_http(&playlist.join("file:///etc/passwd")?), "an absolute one replaces it whole");
    Ok(())
}

/// Two callers spelled the `is_some()` out for themselves, which is the drift a delegating helper
/// exists to stop.
#[test]
fn the_verdict_helper_answers_exactly_what_the_parse_does() {
    for candidate in ["https://example.test/", "http://", "file:///etc/passwd", ""] {
        assert_eq!(is_http_url(candidate), http_url(candidate).is_some(), "for {candidate:?}");
    }
}

// ---- what may be read, over a socket ----

/// The refusal fires on the chunk that *would* cross the cap, before it is appended, so a body
/// sitting exactly on the bound arrives whole rather than one byte too many.
#[tokio::test]
async fn a_body_exactly_at_the_cap_arrives_whole() -> TestResult {
    let server = TestServer::start(|_| TestResponse::ok(filler(CAP)))?;

    let body = read_capped(response_from(&server).await?, "Test body", CAP).await?;

    assert_eq!(body, filler(CAP));
    Ok(())
}

/// Streamed rather than collected and measured afterwards, which is the whole point: a cap
/// enforced after `bytes()` has already allocated whatever the host sent.
#[tokio::test]
async fn a_body_one_byte_over_the_cap_is_refused() -> TestResult {
    let server = TestServer::start(|_| TestResponse::ok(filler(CAP + 1)))?;

    let refused = read_capped(response_from(&server).await?, "Test body", CAP).await;

    assert!(
        matches!(&refused, Err(AppError::Network { msg, .. }) if msg.contains("Test body")),
        "the refusal names what was being read, so a two-request fetch says which half of it \
         refused: {refused:?}"
    );
    Ok(())
}

/// The header check is a courtesy a host can lie about, which is why it sits *ahead* of the
/// streamed bound rather than instead of it. A body that would comfortably have fit is what says
/// the claim is what refused.
#[tokio::test]
async fn a_content_length_over_the_cap_is_refused_before_the_body() -> TestResult {
    let server = TestServer::start(|_| TestResponse::ok(filler(4)).claiming_length(CAP + 1))?;

    let refused = get_from(&server).await;

    assert!(
        matches!(&refused, Err(AppError::Network { msg, .. }) if msg.contains("larger than")),
        "a claim over the cap is refused whatever the body turns out to be: {refused:?}"
    );
    Ok(())
}

/// The other half of the same sentence: the field is optional, so a host that sends none must not
/// buy itself an unbounded read.
#[tokio::test]
async fn a_host_that_declares_no_length_is_still_held_to_the_cap() -> TestResult {
    let server =
        TestServer::start(|_| TestResponse::ok(filler(CAP + 1)).without_declared_length())?;

    let refused = get_from(&server).await;

    assert!(
        matches!(&refused, Err(AppError::Network { .. })),
        "with no header to check, the streamed bound is the only thing left: {refused:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_non_success_status_is_refused_and_says_which_one() -> TestResult {
    let server = TestServer::start(|_| TestResponse::status(503))?;

    let refused = get_from(&server).await;

    assert!(
        matches!(&refused, Err(AppError::Network { msg, .. }) if msg.contains("503")),
        "a status in the message is what separates a host that is down from one that refused \
         us: {refused:?}"
    );
    Ok(())
}

/// One Latin-1 byte in a text response should cost a replacement character, not an error.
#[tokio::test]
async fn a_text_body_that_is_not_utf8_comes_back_lossy_rather_than_failing() -> TestResult {
    let server = TestServer::start(|_| TestResponse::ok(vec![b'A', 0xFF, b'B']))?;
    let url = reqwest::Url::parse(&server.base_url())?;

    let text = get_capped_text(&reqwest::Client::new(), &url, "Test body", TIMEOUT, CAP).await?;

    assert_eq!(text, "A\u{fffd}B");
    Ok(())
}

/// The only thing a third-party host learns about who is asking. A default agent is what gets a
/// fetcher blocked, and the version is what makes one install's traffic tellable from another's.
#[tokio::test]
async fn the_shared_client_names_netodia_and_its_version() -> TestResult {
    let server = TestServer::start(|_| TestResponse::ok(""))?;
    let url = reqwest::Url::parse(&server.base_url())?;

    get_capped(&build_http_client(), &url, "Test body", TIMEOUT, CAP).await?;

    let sent = server.requests();
    let agent = sent.first().and_then(|request| request.header("user-agent"));
    assert_eq!(agent, Some(concat!("Netodia/", env!("CARGO_PKG_VERSION"))));
    Ok(())
}

#[tokio::test]
async fn the_shared_client_can_fall_back_to_ipv4() -> TestResult {
    use std::net::{Ipv6Addr, SocketAddr};

    let server = TestServer::start(|_| TestResponse::ok("IPv4"))?;
    let ip_url = reqwest::Url::parse(&server.base_url())?;
    let ipv4 = *ip_url.socket_addrs(|| None)?.first().ok_or("missing IPv4 address")?;
    let ipv6 = SocketAddr::from((Ipv6Addr::LOCALHOST, ipv4.port()));
    let response = http_client_builder()
        .no_proxy()
        .resolve_to_addrs("dual-stack.test", &[ipv6, ipv4])
        .build()?
        .get(format!("http://dual-stack.test:{}/", ipv4.port()))
        .send()
        .await?;

    assert_eq!(response.remote_addr(), Some(ipv4));
    Ok(())
}
