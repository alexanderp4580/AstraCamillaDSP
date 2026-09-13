# Signal analysis subsystem (spectrum, energy, bpm) for CamillaDSP

## Context

The GUI fork (AstraCamillaGui, a camillaEQ fork) needs a live spectrum display and,
eventually, other real-time signal telemetry — energy/loudness, beat detection. The
first working version faked this by running a **second CamillaDSP instance** whose
"pipeline" is a 2→64 mixer fanned into 64 fixed bandpass filters, polled via the
existing `GetPlaybackSignalPeak` command and read as 64 fake channels. That instance
either generates its own white noise (self-contained but useless) or has to be
pw-link'd to whatever the main instance is doing (works, but doubles the process
count, doubles the ALSA/PipeWire wiring, and needs external plumbing — see
`dsp-dev/README.md` for the full story of getting that working and why the ALSA
loopback path in particular was a dead end on this hardware).

Goal: build a real spectrum analyzer, and a home for future signal-analysis features
(energy, BPM), **into CamillaDSP itself**, exposed over the websocket protocol it
already runs. One process, one port, no external process lifecycle to manage, no
pw-link. This is the plan for that, written before implementation.

## Rejected: extra process, ALSA loopback, WebRTC, raw UDP

**Extra CamillaDSP instance as spectrum analyzer.** What we shipped for the initial
test. Works but is architecturally wrong: doubles the process/config/port surface for
a feature that's really "one more thing to compute from the signal already in the
pipeline." Also structurally can't reflect the *processed* signal correctly by
accident — a shared raw-input tap (the obvious way to link two instances) only sees
pre-EQ audio, so an EQ change never shows up. It has to be pointed explicitly at the
main instance's actual output (its PipeWire sink monitor), a detail that's easy to
get wrong and stays wrong until someone notices the display doesn't move.

**ALSA loopback (`snd-aloop` + `dsnoop`) for capture-sharing.** Tried first, before
the above. Broke repeatedly and non-deterministically: capture substreams got stuck
in a `DRAINING` kernel state after any concurrent open, and `snd-aloop` can't be
reloaded to clear it because PipeWire always holds the card's control device open.
Full postmortem in `dsp-dev/README.md`. Not a CamillaDSP problem — ALSA loopback
fighting a PipeWire-native system — but it's why the eventual working test used
CamillaDSP's native PipeWire backend instead, and it's also the reason "just share a
capture device between two processes" is not the model to build on going forward.

**WebRTC for delivery.** Solves NAT traversal and jitter buffering for raw media
streams between parties that don't share a network. We have one process talking to
one client on a LAN (or Deck-local). No NAT to traverse, no raw media to stream — the
payload is small JSON frames (spectrum bins, a bpm estimate). WebRTC's SDP/ICE
negotiation and SCTP-over-UDP data channels buy nothing here that a websocket doesn't
already do more simply. Would only make sense if the design were "stream raw PCM to
the browser and FFT it client-side" — a different, bigger feature, not this one.

**Raw UDP over the websocket, for latency.** Considered when worried that TCP's
head-of-line blocking under packet loss could stall frame delivery. Two problems:
browsers can't open raw UDP sockets at all (WebRTC's unreliable data channel is the
closest browser-native equivalent, and that's SCTP-over-UDP, not raw UDP), and the
actual failure mode — a queue of stale frames backing up behind one dropped packet —
is better solved by never having a queue in the first place (below) than by changing
transport. On a healthy LAN, TCP HOL blocking on a visualizer feed isn't perceptible
anyway.

## Design

**Transport: same websocket, new message kind.** CamillaDSP's protocol today is
strict request → response (`GetPlaybackSignalPeak` → one JSON reply). Analysis data
is inherently a push stream — the client doesn't know the right poll interval, and
guessing wrong either duplicates frames or misses transients. So: client sends
`Subscribe { topics: [...] }` once, server pushes tagged event messages
(`AnalysisFrame`) on its own cadence, no further request needed. The reply/event
distinction needs a clear envelope tag so it can't be confused with the existing
command/response pairs — cheap insurance even though we're currently the only client.

**Delivery: single mutable slot per topic, not a queue.** "Only the latest frame
matters, drop anything stale" doesn't need backpressure logic or a coalescing
buffer — it's one `Arc<RwLock<...>>` slot overwritten every update, read whenever the
sender loop gets to it, exactly the same shape as `PlaybackStatus`/`CaptureStatus`
today. This is also what makes TCP head-of-line blocking a non-issue: there's never a
backlog to block on.

**Topics, independent cadence.** Spectrum and energy update every chunk (~20ms).
BPM needs seconds of onset history and updates far less often, and a bare number is
the wrong shape for it anyway — real beat trackers flip between a tempo and its
octave, so the useful output is `{bpm, confidence, phase}` at minimum, and an
onset/pulse event stream is probably a better v1 than a committed BPM estimate (see
Phasing). Each topic gets its own frame type; there's no shared "AnalysisData"
struct trying to hold every field at once.

**`tap: Capture | Playback`, reusing the axis CamillaDSP already has.** The existing
protocol already distinguishes `GetCaptureSignalPeak` from `GetPlaybackSignalPeak`.
New topics take the same parameter instead of inventing a new pre/post-processing
concept — consistent, and it directly encodes the answer to "does this reflect the
EQ or not," which the extra-instance version got wrong by default.

**Subscription is the toggle.** A topic is only computed while at least one
subscriber wants it — checked once per chunk inside its own update function, no
separate enable flag, no config surface. Nobody watching the spectrum view costs
zero FFTs. This generalizes to every future topic for free.

**Analysis parameters are not pipeline config.** Bin count, frequency range, BPM
window length etc. don't touch the signal path, so they don't belong in `SetConfig`
— per the existing finding that structural pipeline changes rebuild the pipeline
while parameter changes don't (`processing.rs`), routing analysis params through
`SetConfig` would risk unnecessary rebuilds for something that isn't audio-path
state at all. They're subscription-time arguments instead, changeable any time with
zero audio impact.

**Frames carry a sequence number / sample position**, not just wall-clock time, so a
future beat-synced consumer can detect drops and line up frames from different
topics against the same audio timeline.

**Single-client simplification, explicit.** We are the only client (the GUI). This
removes real complexity that a multi-client design would need: no negotiating
whose bin count/frequency range wins if two subscribers want different `Spectrum`
parameters, no worrying about the event/response envelope confusing tooling we
don't control. Revisit if that assumption ever changes.

## Integration points (verified against current source)

The shape of this already exists in the codebase three times over — this is a
fourth instance of an established pattern, not a bolt-on:

| Concern | Existing pattern to mirror | Location |
|---|---|---|
| Shared live state | `capture_status` / `playback_status` / `processing_status`, each `Arc<RwLock<...>>` | `src/lib.rs:53-56` |
| Status struct shape | `PlaybackStatus { signal_rms, signal_peak, ... }` | `src/lib.rs:204-210` |
| Per-chunk update, non-blocking | `update_playback_signal_status`, uses `try_write()` + skip-and-log (`xtrace!`) rather than blocking the real-time thread | `src/lib.rs:227` |
| Tap point | `chunk = pipeline.process_chunk(chunk);` — post-pipeline, pre-send-to-playback | `src/processing.rs:119` |
| Raw sample access | `AudioChunk { frames, channels, waveforms: Vec<Vec<PrcFmt>> }` | `src/audiochunk.rs:23-30` |
| Per-chunk stats template | `ChunkStats` (today: rms/peak) | `src/audiochunk.rs` |
| FFT | `realfft` already a dependency, already used the same way (`RealFftPlanner::<PrcFmt>::new()`) | `Cargo.toml`; `src/filters/fftconv.rs:187` |
| Protocol enums | `WsCommand`, `WsReply` | `src/socketserver.rs:88`, `:201` |
| Command dispatch | `match` arms next to `GetPlaybackSignalPeak` | `src/socketserver.rs:778` |

Concretely: add `spectrum_status: Arc<RwLock<SpectrumStatus>>` alongside the other
three status fields; define `SpectrumStatus { bins: Vec<f32>, active: bool, ... }`;
add `update_spectrum_status(...)` as a copy of `update_playback_signal_status`'s
locking discipline; call it from `processing.rs:119` right where the existing status
update already happens; add `Subscribe`/`Unsubscribe`/`AnalysisFrame` to the
`WsCommand`/`WsReply` enums and their dispatch match. No new crate for spectrum.

## Open item: pipeline rebuilds

The analysis tap sits right after `pipeline.process_chunk(chunk)`. Structural config
changes rebuild the pipeline (`processing.rs`) — the tap needs to keep working
correctly across that rebuild the same way `NightMode`'s own `set_coefficients`
live-reload path had to be made rebuild-safe. Not yet designed in detail; look at
how the existing `playback_status` update survives a rebuild today (it must, since
peak/rms metering doesn't glitch on config reload) and copy that.

## Phasing

1. **Spectrum + Energy, Playback tap only. Built and verified.** `Subscribe`/
   `Unsubscribe` on `WsCommand`, pushed `SpectrumFrame`/`EnergyFrame` on `WsReply`,
   `SpectrumStatus` in `lib.rs` gating a `realfft`-based FFT tapped at
   `processing.rs`'s post-pipeline point, log-binned into `num_bins` bins. Read
   timeout on the connection is only armed while a subscription is active, so an
   unsubscribed client sees zero behavior change from before this feature existed.
   149 tests still pass in both f64 and `--features 32bit`, clippy clean on the new
   code. Verified end to end over the real websocket protocol: a +12dB `SetConfig`
   change on a 1kHz filter showed up as +10.4dB on the corresponding spectrum bin
   within ~1.5s of pushed frames, with no second process and no PipeWire
   output-tapping — the thing the earlier two-instance version could only get right
   by accident of which node happened to be linked to which.
2. **Capture tap.** Same topics, `tap: Capture` — mechanical once (1) exists, since
   it's the same axis the protocol already has for peak/rms. Not yet built; `tap` is
   deliberately not a parameter on `AnalysisTopic::Spectrum` yet so there's no
   half-supported option sitting in the API before this lands.
3. **BPM — separate research spike, not bundled with (1).** Real tempo tracking
   (onset detection + autocorrelation or comb-filtering over several seconds,
   octave-error handling) is a harder DSP problem than spectrum/energy and shouldn't
   block shipping those. Likely starts as an onset/pulse event stream (useful for
   visuals on its own) before attempting a stable `{bpm, confidence, phase}` figure.

### What (1) actually touched

- `src/lib.rs` — `SpectrumStatus`, `update_spectrum_status` (mirrors
  `update_playback_signal_status`'s `try_write`-and-skip discipline), `StatusStructs`
  gained a `spectrum` field.
- `src/processing.rs` — `run_processing` takes the new `Arc<RwLock<SpectrumStatus>>`,
  plans a `realfft` FFT once per thread lifetime (not per chunk), calls
  `update_spectrum_status` right after `pipeline.process_chunk(chunk)`. Survives
  pipeline rebuilds for free — the tap doesn't hold a reference to `pipeline` itself.
- `src/bin.rs` — creates the `spectrum_status` Arc alongside `capture_status`/
  `playback_status`, threads it into both `run_processing` and `SharedData`.
- `src/socketserver.rs` — `AnalysisTopic` enum, `Subscribe`/`Unsubscribe` on
  `WsCommand`, `SpectrumFrame`/`EnergyFrame`/`Subscribe`/`Unsubscribe` on `WsReply`,
  `push_analysis_frames`, and a per-connection read-timeout toggle
  (`SetSocketReadTimeout`) so the push loop only costs anything on connections that
  actually use it.
- No new Cargo dependencies — `realfft`/`num-complex` were already there for
  `filters/fftconv.rs`.

## Known cost

This is fork-maintenance surface: real DSP code diverging further from upstream
HEnquist/camilladsp, on top of NightMode and the empty-channel panic fix already
carried. Worth it for the elimination of the second-instance/pw-link setup, but
worth naming plainly rather than treating as free.
