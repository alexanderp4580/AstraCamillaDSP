# CamillaDSP night mode — handoff

Sessions: 2026-09-12, continued 2026-09-13. Adding a new `NightMode` processor to
this CamillaDSP clone: a dialogue-preserving dynamics processor for watching films
quietly. The processor is functionally complete. It has been measured extensively and
checked by ear on a phone speaker only, which is not a fair test of a night mode.

**Full design spec: `NIGHT-MODE-PLAN.md`, next to this file** (approved plan —
architecture, rejected alternatives, parameter table, 12-stage build order, full
test plan). Read that first. This file is only session state: what is built,
what is proven, what is open.

Original copy of the plan lives at
`/home/deck/.claude/plans/scrutinize-this-and-come-keen-quilt.md`.

Nothing is committed. All work is in the working tree on `main`.

---

## Goal and signal path

Films are too loud on explosions and too quiet on dialogue at night. Existing
AVR/Dolby night mode compresses uniformly and sounds flat. This processor
reduces loud effects while leaving speech alone.

Playback chain: TV analog aux out → HiFiBerry DAC+ADC Pro → CamillaDSP. The
eventual host is the moOde Pi described in `../MOODE-AUDIO-HANDOFF.md`, where
CamillaDSP already captures the ADC as channels 2,3 of the mixbus.

Three constraints this forces, all load-bearing in the design:

1. **Always 2 channels.** The TV downmixes 5.1 before aux out, so there is never
   a real centre channel — dialogue must be inferred from a phantom centre.
2. **Level is not absolute.** Aux out tracks the TV volume knob, so fixed dBFS
   thresholds are meaningless. The processor self-calibrates against a tracked
   dialogue reference instead.
3. **Latency cannot be compensated.** The picture comes from the TV itself, so
   any added delay is uncorrectable lip-sync error. Hence zero lookahead.

---

## Build environment — read this before running cargo

This Steam Deck had **no Rust toolchain at all**. Installed `rustup` into
`~/.cargo` (userspace, survives SteamOS updates, `rm -rf ~/.cargo ~/.rustup` to
remove). `/` has only ~594 MB free, so nothing may be installed system-wide.

`alsa` and `alsa-sys` are **unconditional** Linux dependencies in `Cargo.toml`,
so `--no-default-features` does not avoid them. SteamOS ships `libasound.so` but
no headers and no `alsa.pc`. Workaround: a pkg-config shim pointing at the
Flatpak Freedesktop SDK headers and the system library.

`/home/deck/.local/share/camilladsp-dev/pkgconfig/alsa.pc` (outside the repo,
deliberately — do not commit it):

```
prefix=/home/deck/.local/share/flatpak/runtime/org.freedesktop.Sdk/x86_64/25.08/43f98359f53fb32d84218462532c8db7e28f1e5246f2c185e8f695908ce98157/files
includedir=${prefix}/include

Name: alsa
Description: ALSA dev shim (SteamOS has libasound but no headers)
Version: 1.2.14
Libs: -L/usr/lib -lasound
Cflags: -I${includedir}
```

Build and test with:

```bash
cd /home/deck/SketchBook/Projects/camilladsp
source ~/.cargo/env
export PKG_CONFIG_PATH=/home/deck/.local/share/camilladsp-dev/pkgconfig
cargo test night_mode
```

`yt-dlp` 2026.8.19 is installed via pipx (for fetching trailer audio as test
material later). `ffmpeg` was already present.

---

## What is built

### Stage 0 — enabling changes

- `src/filters/biquad.rs` — `flush_subnormals` is now `pub` (callers using
  `process_single` must flush themselves; only `process_waveform` does it
  automatically). Added `set_coefficients`, to swap coefficients while keeping
  filter state, so a live config reload does not click.
- `src/processors/compressor.rs`, `src/processors/noisegate.rs` — **fixed a
  latent panic.** `sum_monitor_channels` did
  `scratch.copy_from_slice(&waveforms[ch])`, which panics when a monitored
  channel is an empty vector. Unused capture channels *do* arrive empty —
  `pipeline.rs:119,152` and `mixer.rs:101` all guard for it, these two did not.
  Now skips empty channels and returns false when none carry data, and
  `process_chunk` returns early in that case. **Worth upstreaming separately
  from the night mode feature.**

### Stage 1 — config surface and wiring

- `src/config/mod.rs` — `NightMode` variant on `enum Processor`, plus
  `NightModeParameters` (22 fields, all `Option<T>` with accessor defaults
  except `channels`) following the `CompressorParameters` idiom.
- `src/config/utils.rs` — validation arm calling `validate_night_mode(fs, …)`.
- `src/pipeline.rs` — construction arm.
- `src/processors/mod.rs` — `pub mod night_mode;`
- `src/processors/night_mode.rs` — new, with complete `validate_night_mode`.

Validation rejects: attack ≤ 0, release < attack, ratio < 1, amount outside
0–100, negative max attenuation, bass reduction > 10 dB, presence gain outside
0–6 dB, presence Q outside 0.3–3.0, presence/bass frequency ≥ fs/2, ceiling
outside −20…0 dBFS, modulation weight outside 0–1, reference slew ≤ 0, pinned
reference outside the −45…−12 dBFS window, out-of-range channel indices, and
**a non-stereo channel count with `dialogue_channels` unset** — it refuses to
guess which channel is the centre rather than silently mangling a mix.

### Stages 2 to 10 — the processor itself

All functional stages are built. `src/processors/night_mode.rs`, plus the README
section, `exampleconfigs/nightmode.yml` and a CHANGELOG entry.

Signal flow per chunk: measure monitored power (60 Hz highpassed, power summed) →
prepare detector signals → accumulate into fixed hops → per-sample gain law →
apply, with bass shelf, presence lift and ceiling.

- **Two dynamics stages in series.** Slow (0.15 s / 1.5 s, threshold reference +
  `headroom`) does most of the work; fast (2 ms / 80 ms, threshold reference +
  18 dB, depth from `transient_softening`) catches leading edges.
- **Gain-domain smoothing.** Level is measured over a fixed 30 ms window; `attack`
  and `release` smooth the resulting reduction in dB, not the measurement. A hysteretic
  catch-up releases at 0.25 s instead of 1.5 s whenever the applied reduction is more
  than 6 dB deeper than the signal warrants. See the findings below; this was not
  optional. Only the slow stage is smoothed, never the total.
- **Dialogue detector.** Speech-band correlation between channels, speech-band
  share of total power, and syllabic modulation via a 5.5 Hz bandpass run at hop
  rate. Combined as a product, smoothed fast-up/slow-down, then held at τ ≥ 1 s
  before it is allowed to touch gain.
- **Adaptive reference.** Sign-step tracker bounded by `reference_slew`, Schmitt
  gate at 0.7/0.5 with a 300 ms hold, clamped to −45…−12 dBFS, frozen in silence
  and when pinned via `reference_level`.
- **Bass shelf and presence lift.** Both as fixed-coefficient band extraction with
  a per-sample depth (`x − k·LP(x)` and `x + k·BP(x)`), so no coefficient is ever
  recomputed and there is no zipper. Shelf depth follows current reduction;
  presence depth follows confidence, and is off by default.
- **Live reconfiguration** preserves every follower, the reference, confidence and
  all filter state; coefficients are swapped in place via `set_coefficients`.

Two details in there that are easy to undo by accident:

- The monitored channels are **power summed, not amplitude summed** (the existing
  compressor does the latter), so the level does not depend on whether the channels
  happen to be correlated.
- `amount` scales the reduction **in dB**, not as a dry/wet signal blend. A blend
  would be a linear-domain interpolation: 50% of a −20 dB reduction gives −6 dB, not
  −10. There is a test that fails if anyone changes this.

---

## Proven

149 tests pass, in **both** f64 and `--features 32bit` (f32). `cargo fmt` clean,
and `cargo clippy --all-targets` reports zero warnings for the new file. 40 of the
tests are night mode's own.

### Verified on real audio

Rendered a synthetic 13 s scene (quiet syllabic dialogue → silence → loud two-tone
burst → dialogue) through the built binary via the file backend:

```
t   input    reduction
0-3  -34.9    0.00 dB   dialogue, untouched
4    silent   0.00 dB
5-8   -8.0  -14.7 dB    loud section
9    -34.9   -3.7 dB    dialogue, recovering
10-12 -34.9   0.00 dB   dialogue, fully released
```

`amount` scaling confirmed end to end: −15.86 dB at 100 against −12.09 dB at 75,
a ratio of 0.762, which is the dB-domain behaviour the README promises.

Reproduce with:
```bash
sed -e 's|"input.wav"|"/tmp/input.wav"|' -e 's|"output.wav"|"/tmp/output.wav"|' \
    exampleconfigs/nightmode.yml > /tmp/nm.yml
./target/debug/camilladsp /tmp/nm.yml
```

### Verified on a real trailer

`yt-dlp` fetched the *Dune: Part Two* official trailer (144 s), converted to 48 kHz
stereo, rendered at defaults (`amount: 75`):

- `amount: 0` is bit-exact: max sample difference 0.0.
- Loudest second reduced 4.4 dB; every quiet second touched by exactly 0.00 dB.
- Loud-to-quiet spread over one-second windows: 41.2 dB in, 37.3 dB out.
- Median reduction across the loud half: 2.3 dB. The detector gives back about
  1.5 dB overall against `dialogue_protection: 0`.

Fetch it again with:
```bash
yt-dlp --no-playlist -f bestaudio -x --audio-format wav \
  -o /tmp/trailer.%\(ext\)s "ytsearch1:Dune Part Two official trailer"
ffmpeg -y -i /tmp/trailer.wav -ar 48000 -ac 2 -c:a pcm_s16le /tmp/tr48.wav
```

**Read those numbers with the caveat in mind:** a trailer is already heavily
compressed and limited for streaming, so its loud-to-quiet span is far narrower than
a film's, and the dialogue sits high in the mix. That pushes the tracked reference up,
the threshold with it, and leaves less above the threshold to work on. Four dB on the
loudest second is a plausible result for this material, not evidence about how the
processor behaves on an actual film. A real film, or at minimum a scene rip, is needed
before tuning any constant.

- `amount = 0` leaves samples untouched
- content below threshold is untouched (within 0.3 dB)
- loud content is clearly but boundedly reduced
- `max_attenuation` cap holds under a full-scale signal
- reduction scales in the dB domain — 50% gives half the dB of 100%
- digital silence stays exactly zero, everything finite
- ceiling is enforced
- empty channel waveforms do not panic; all-empty monitors is a clean no-op
- the whole validation rejection table

### Findings worth keeping

**The soft clipper is not a safety net.** `Limiter::apply_clip` with `soft_clip`
**shapes every sample it sees**, not just ones over the limit — the cubic
waveshaper is unconditional. A −3 dBFS sine was measurably altered by a ceiling at
−1 dBFS. The `amount = 0` transparency test caught it. Fixed by checking the peak
first and only invoking the limiter when something actually exceeds the ceiling,
which also makes the common case cheaper. The plan had assumed this was "a net that
is essentially never hit"; it was not.

**A dB-domain envelope follower cannot have a fast attack.** The repo's compressor
idiom smooths level in dB, starting from a −200 dB floor. A one-pole then has to
traverse ~200 dB before it responds, so the nominally 2 ms fast stage did almost
nothing for the first 10 ms after a silent passage — precisely the scene-change case
it exists for. Both followers now smooth **linear power** and convert to dB
afterwards. The existing compressor and noise gate still have this flaw.

**The release tail swallows dialogue after an explosion.** Found only by rendering
real audio; every unit test passed because they measured steady states. With a 1.5 s
release, speech following a loud scene was ducked by 11, 9, 7 and 5 dB over four
seconds. Fixed with program-dependent release: a drop to 6% of the envelope
(about 12 dB) is a scene change and releases in 0.25 s, while smaller fluctuations
keep the slow release that prevents pumping. Dialogue now recovers inside a second
and the loud section is essentially unaffected (−15.9 → −14.7 dB). There is a test
for it now, but note the general lesson: steady-state tests cannot see this class of
problem.

**`amount: 0` was not transparent on real material.** The unit test used a −3 dBFS
sine, which never reaches the ceiling, so it passed. A real trailer is mastered hot
and peaks above the default −1 dBFS ceiling, so the always-on soft clipper engaged
even with the dynamics fully disabled — max sample difference 0.138. That makes
`amount: 0` useless as an A/B reference, which is exactly what it is for. Now, when
`amount` is 0 and `presence_gain` is off, `apply` returns early and touches nothing:
the processor cannot push anything over the ceiling if it is not doing anything. Re-measured
on the trailer as exactly 0.0 difference. The test now uses a full-scale signal.

**Protection was unbounded, so loud content could dodge the processor.** Found by
listening: a loud hit at 1:06 of the trailer got only 7.6 dB while its neighbours got
14 to 20. The detector had judged it centred and speech-like — plausibly a shouted
line over a centred score — and protection then handed back reduction without limit.
Confidence now only decides *whether* to protect; how much it may give back is capped
by how plausible the level is as dialogue, fading out above reference + 15 dB and
reaching zero by + 25 dB. Someone shouting is still protected; a 22 dB impact is not.
That one spot went from −7.6 dB to −27.9 dB, loudest-10 s average from −23.3 to
−31.5 dB, and the quietest seconds barely moved (−6.20 to −6.45).

**Smoothing belonged on the gain, not on the level.** Also found by listening: at 1:41
a quiet passage following a loud one was buried, and the next hit seemed to arrive
before the processor reacted. Measuring in 50 ms windows showed the cause — as the
input decayed from −16 to −46 dB the processor was still holding 32 down to 8 dB of
reduction, so the output sat near −54 dB. Because `attack` and `release` smoothed the
*level measurement*, the reduction target itself lagged in both directions. Now level
is measured over a fixed 30 ms window and the smoothing is applied to the reduction in
dB, with a hysteretic catch-up that releases at the scene rate whenever the applied
reduction is more than 6 dB deeper than the signal warrants. The quiet passage came up
by 4 to 10 dB, onset timing unchanged.

One trap inside that change: smoothing the *total* reduction also throttles the fast
stage to the slow attack and destroys the reason it exists. Three tests caught it. Only
the slow stage's contribution is smoothed; the fast stage's is added afterwards.

**The detector can be fooled by centred midrange.** The synthetic "explosion" used
for the render was two steady centred tones, one at 1500 Hz, which lands in the
speech band. Confidence reached ~0.375 and protection gave back most of the
reduction. Real explosions are broadband with most energy below 300 Hz, so this is
partly an artifact of the test signal, but it does show the failure mode: loud,
centred, midrange-heavy content reads as dialogue. Verify against a real trailer
before drawing conclusions, and remember `dialogue_protection: 0` is the escape
hatch.

---

## What is open

Stages 0 to 10 and 12 are done. What remains:

- **Stage 11, golden reference.** Not written. An in-code `const GOLDEN` table of
  per-100 ms RMS over a fixed scene script, tolerance 0.5 dB, no WAV fixtures.
  Worth doing now that behaviour has stopped changing.
- **Listening.** Still nothing heard, only measured. The renders exist at
  `/tmp/tr2_0_100.wav` and `/tmp/tr2_75_100.wav` (regenerate as above; /tmp does not
  survive a reboot). `amount: 0` is bit-exact, so it is a valid A/B reference.
- **A real film, not a trailer.** The trailer's dynamics are pre-squashed, so it
  cannot show whether the leveler does the right thing across a 20 dB
  dialogue-to-explosion span. This is the main gap before tuning anything.
- **Deploying to the moOde Pi.** This has only run on the Steam Deck. The Pi is
  aarch64, so it needs its own build, and the ALSA shim described above is a Steam
  Deck workaround that should not be copied there.
- **Upstreaming.** The two `is_empty()` guard fixes are independent of the feature
  and could be submitted separately. Whether Henrik would want the fast-stage
  parameters exposed rather than hardcoded is an open question; they are `Option`
  fields' worth of work to add later without breaking configs.

### Deviations from the plan worth knowing

- The plan's stage 3 check ("steady-state reduction unchanged within 0.5 dB")
  is wrong as written: a sustained signal above reference + 18 dB legitimately
  engages the fast stage forever, so steady state does change. The test measures the
  onset window instead.
- Program-dependent release was listed under auto-adjust but had no stage of its
  own. It turned out to be essential rather than a refinement.
- The plan assumed one `log10` per sample. There are two, one per follower, since
  smoothing moved into the linear domain.

### Defaults were retuned twice, from measurement

Original defaults did almost nothing on the trailer: 2.7 dB on the loudest sections.
Current defaults are deliberately aggressive, on the basis that someone switching night
mode on wants to hear it: `amount` 100, `headroom` 0, `ratio` 12, `max_attenuation` 28,
`bass_reduction` 10, `transient_softening` 100, `dialogue_protection` 60. On the trailer
that gives 11.1 dB on the loudest ten seconds, 1.4 dB on the quietest ten, deepest
13.9 dB, spread 41.2 -> 32.0 dB.

These are provisional, chosen against a trailer on a phone speaker, and are the first
thing to revisit after a real listening test. To back off: raise `headroom`, or lower
`amount`, which scales everything at once.

Three things learned while choosing them:

- **The reference tracker is not the limiting factor.** Per-second logging (now in the
  code, `debug!` with level, reference, confidence and gate) shows the reference sitting
  stably at -27 to -28.8 dB across the whole trailer. The earlier guess that it was
  riding up with the loud mix was wrong.
- **`dialogue_protection` was the real lever, not `ratio` or `headroom`.** Pushing ratio
  to 8 and headroom to 0 still only reached 6.2 dB, because confidence runs 0.4 to 0.8 on
  trailer content, where loud orchestral score is centred and midrange-rich, and
  protection handed back most of the reduction.
- **There is a hard ceiling around 12 dB on this material.** Ratio 16 with protection 35
  bought only 1.6 dB more than ratio 12 with protection 60. The binding constraint is
  that the trailer's loud content sits only about 15 dB above the dialogue reference, so
  going deeper requires a threshold below dialogue level, which means compressing speech.
  A real film, with a much wider gap, has far more room before hitting this.

### Unverified / worth watching

- The detector's calibration constants (the correlation and band-fraction windows,
  and the modulation reference) were chosen by reasoning, not fitted to real speech.
  They are the first thing to revisit after listening.
- The fast stage keeps acting on sustained content above reference + 18 dB, not only
  on transients. Defensible, and capped by `max_attenuation`, but it means very loud
  sustained passages get both stages.
- Nothing has been heard. Every result so far is a measurement.
