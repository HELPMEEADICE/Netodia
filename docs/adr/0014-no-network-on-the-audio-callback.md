# ADR 14: The network never touches the audio callback thread

**Status:** Accepted, 2026-08-20

The mixer pulls samples inside the audio device's callback. Whatever is on the end of that pull runs
on that thread, and a live stream's end is a socket. A blocking read there stalls the entire output
block for as long as the network is wedged, which is not just the station going quiet: it is every
deck, so a local track crossfading out of a station stalls with it, and a wedged connection becomes
an application-wide audio dropout.

Decision: a ring buffer with its own feed thread sits between the network and the decoder. The feed
thread fills it and can block as much as it likes; the source above it pops without blocking and
yields silence when the ring is dry rather than ending. This is a structural constraint on the whole
player, not a property of the radio feature.

Alternatives: reading the socket inside the source and accepting the stall; an async source polled
from the runtime; a larger read buffer to make the stall rare.

Trade: this is the one decision here with no real competition, which is precisely why it is written
down: having no competition is what makes it look like an implementation detail later. A bigger
buffer makes the stall rarer and not shorter, which is the worst of both: it fails less often and
just as badly, and it costs resident memory in a process where memory is a product requirement
([ADR 2](0002-native-rust-desktop-app.md)). An async source does not help either, because the
callback is not a runtime worker and cannot await anything; it would mean blocking on a future,
which is the original stall wearing a different type.

It costs a thread per live stream, a copy through the ring, and the two edge cases that come with a
producer and a consumer at different rates. A dry ring yields `Some(0.0)` rather than `None`,
because ending the source would tear the deck down over a stall the connection is about to recover
from. And a partial frame is never split across the ring boundary and the silence that follows it,
or a stereo frame shears there and that deck's channel parity flips permanently.

Two things fall out of it that look like separate decisions and are not. Because starvation is
already a state the ring knows about, it is published as the buffering indicator the UI needed
anyway rather than being inferred. And reconnect lives in the feed thread rather than in the
playback monitor: that thread already holds the URL, the client and the ring, so it re-opens and
keeps filling the same ring, the source never ends, the deck never blinks, and the state machine
needs no reconnect path at all. Putting it in the monitor would mean handing the engine an HTTP
client and inverting the dependency direction the crate graph exists to hold.

**Amendment, 2026-11-11:** the same boundary now covers local file reads and packet decoding.
Pulling a `FileDecoder` directly from the mixer made disk or codec stalls hold up the device
callback even with real-time thread scheduling enabled. Local play, seek, crossfade and gapless
preload now wrap the opened and positioned decoder in `BufferedAudioSource`. Its worker fills a
bounded PCM queue; the callback only consumes samples already decoded there. Opening, seeking
and waiting for the initial reserve happen outside the deck lock.

Local sources prefill four blocks before mounting, or stop at EOF for short files. The existing
24-block queue remains bounded to about 1.5 MiB of sample data per source. Blocks contain whole
frames, so a temporary empty queue cannot shift channel alignment. This moves blocking work off
the callback, but a producer stall longer than the queued audio can still exhaust that reserve.
