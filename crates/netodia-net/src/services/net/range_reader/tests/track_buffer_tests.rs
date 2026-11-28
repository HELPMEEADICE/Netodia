use super::{CHUNK_BYTES, RangeReader, listener_url, request, track_buffer};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn accept(listener: &TcpListener) -> io::Result<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                return Ok(stream);
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error),
        }
    }
}

fn span(request: &str) -> io::Result<(usize, usize)> {
    let range = request
        .lines()
        .find_map(|line| line.strip_prefix("range: bytes="))
        .and_then(|value| value.split_once('-'))
        .ok_or_else(|| io::Error::other("range missing"))?;
    Ok((range.0.parse().map_err(io::Error::other)?, range.1.parse().map_err(io::Error::other)?))
}

fn send(stream: &mut TcpStream, body: &[u8], (start, end): (usize, usize)) -> io::Result<()> {
    let bytes = &body[start..=end];
    write!(
        stream,
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nETag: \"song\"\r\nConnection: close\r\n\r\n",
        body.len(),
        bytes.len()
    )?;
    stream.write_all(bytes)
}

fn start_buffering(reader: &mut RangeReader) -> io::Result<tokio::runtime::Runtime> {
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let client = reqwest::Client::builder().no_proxy().build().map_err(io::Error::other)?;
    {
        let _entered = runtime.enter();
        reader.buffer_track(client);
    }
    Ok(runtime)
}

fn wait_for_download(reader: &RangeReader) -> io::Result<()> {
    let task = reader.buffer_task.as_ref().ok_or_else(|| io::Error::other("buffer missing"))?;
    let deadline = Instant::now() + Duration::from_secs(8);
    while task.is_running() {
        if Instant::now() >= deadline {
            return Err(io::Error::other("buffer did not finish"));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

#[test]
fn parallel_segments_buffer_the_entire_track_for_a_reopened_reader() -> TestResult {
    let segment = usize::try_from(track_buffer::segment_end(0) + 1)?;
    let head = usize::try_from(CHUNK_BYTES)?;
    let body: Vec<u8> =
        (0..4 * segment + 17).map(|index| u8::try_from(index % 251).unwrap_or_default()).collect();
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = listener_url(&listener)?;
    listener.set_nonblocking(true)?;

    std::thread::scope(|scope| -> TestResult {
        let server = scope.spawn(|| {
            let mut probe = accept(&listener)?;
            let probe_range = span(&request(&mut probe)?.to_ascii_lowercase())?;
            assert_eq!(probe_range, (0, head - 1));
            send(&mut probe, &body, probe_range)?;
            drop(probe);

            // All three requests must arrive before the server returns any of their bodies.
            let mut first_wave = Vec::new();
            for _ in 0..3 {
                let mut stream = accept(&listener)?;
                let request = request(&mut stream)?.to_ascii_lowercase();
                assert!(request.lines().any(|line| line == "if-range: \"song\""));
                first_wave.push((span(&request)?, stream));
            }
            first_wave.sort_unstable_by_key(|(range, _)| *range);
            for (index, (range, mut stream)) in first_wave.into_iter().enumerate() {
                assert_eq!(range, ((index + 1) * segment, (index + 2) * segment - 1));
                send(&mut stream, &body, range)?;
            }
            for expected in [(4 * segment, body.len() - 1), (head, segment - 1)] {
                let mut stream = accept(&listener)?;
                let range = span(&request(&mut stream)?.to_ascii_lowercase())?;
                assert_eq!(range, expected);
                send(&mut stream, &body, range)?;
            }
            Ok::<_, io::Error>(())
        });
        let mut reader = RangeReader::open(&url)?;

        let _runtime = start_buffering(&mut reader)?;
        server.join().map_err(|_| io::Error::other("range server panicked"))??;
        wait_for_download(&reader)?;

        let mut reopened = reader.reopen();
        let mut received = Vec::new();
        reopened.read_to_end(&mut received)?;
        assert_eq!(received, body);
        Ok(())
    })
}

#[test]
fn a_failed_background_range_leaves_serial_playback_available() -> TestResult {
    let segment = usize::try_from(track_buffer::segment_end(0) + 1)?;
    let head = usize::try_from(CHUNK_BYTES)?;
    let body = vec![42; segment + 9];
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = listener_url(&listener)?;
    listener.set_nonblocking(true)?;

    std::thread::scope(|scope| -> TestResult {
        let server = scope.spawn(|| {
            let mut probe = accept(&listener)?;
            let range = span(&request(&mut probe)?.to_ascii_lowercase())?;
            send(&mut probe, &body, range)?;
            drop(probe);

            let mut background = accept(&listener)?;
            let range = span(&request(&mut background)?.to_ascii_lowercase())?;
            assert_eq!(range, (segment, body.len() - 1));
            background
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            drop(background);

            let mut foreground = accept(&listener)?;
            let range = span(&request(&mut foreground)?.to_ascii_lowercase())?;
            assert_eq!(range, (head, body.len() - 1));
            send(&mut foreground, &body, range)
        });
        let mut reader = RangeReader::open(&url)?;
        let _runtime = start_buffering(&mut reader)?;
        wait_for_download(&reader)?;

        let mut received = Vec::new();
        reader.read_to_end(&mut received)?;

        server.join().map_err(|_| io::Error::other("range server panicked"))??;
        assert_eq!(received, body);
        Ok(())
    })
}
