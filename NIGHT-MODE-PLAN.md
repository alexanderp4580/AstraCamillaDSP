# NightMode processor for CamillaDSP

## Context

Watching films late at night through TV analog aux out → HiFiBerry DAC+ ADC Pro → CamillaDSP. Explosions and score are too loud, dialogue too quiet. Existing AVR/Dolby "night mode" compresses uniformly against a fixed reference, which is why it sounds flat. Goal: a new `NightMode` processor that reduces loud effects and music while keeping speech clear, with enough control to tune by ear and hard rails so no setting can ruin the audio.

Three facts from the hardware path constrain the design:

1. **Always 2-channel.** The TV downmixes any 5.1 before aux out, so a real center channel never exists — dialogue must be inferred from a phantom center.
2. **Level is not absolute.** Aux-out level tracks the TV's volume knob, so fixed dBFS thresholds are meaningless; the processor must self-calibrate.
3. **Latency cannot be compensated.** The picture comes from the TV itself, so added delay is lip-sync error with no offset available.

## Core design

**Two serial dynamics stages, not one.** A single broadband stage faces an unresolvable dilemma: slow enough to be inaudible (attack ≥150 ms) lets the first 150 ms of an explosion through at full level; fast enough to catch it (attack ≤5 ms) modulates gain in the 50–300 ms range, which is exactly the "breathing" that makes AVR night mode sound lifeless. Night mode must destroy *macro*dynamics (scene-to-scene contrast) while preserving *micro*dynamics (transient attack shape).

| Stage | Job | Attack | Release | Threshold | Depth |
|---|---|---|---|---|---|
| A — scene leveler | Get a blockbuster down to watchable | 150 ms | 1.5 s | ref + `headroom` | most of the reduction |
| B — transient tamer | Catch impulsive peaks A misses | 2 ms | 80 ms | ref + 18 dB | shallow, bounded |

**Rejected: multiband.** Not primarily for timbre reasons. An explosion is broadband, so four detectors all fire together and multiband ≈ broadband — no benefit on the target signal. Dialogue is band-limited, so a mid-band detector fires *only* on dialogue and selectively ducks the band we promised to protect. Multiband inverts the goal.

**Rejected: BS.1770/LUFS leveler.** K-weighting is only two biquads, so cost isn't the issue — *absolute* LUFS targeting needs an absolute target, which reintroduces the drift problem fact #2 creates. Keep one cheap idea from it: high-pass the broadband sidechain at 60 Hz so LF rumble doesn't dominate the gain calculation and get counted twice against the bass shelf.

**Rejected: lookahead, including my chunk-internal forward scan.** Available lookahead would vary from `chunksize` samples (transient at chunk start) to zero (at chunk end), so identical audio offset by one sample yields different output and golden tests become chunk-alignment dependent. And it isn't needed: stage A is slow by design, stage B is shallow, and residual pop-through is arguably desirable — it preserves impact while the sustained boom still gets reduced. Ship v1 documented as **zero added latency, no A/V sync impact**.

**Dialogue reference, self-calibrating.** Track the broadband level while dialogue confidence is high; that becomes the anchor (conceptually Dolby's dialnorm) and the threshold sits `headroom` above it. Track on the **broadband** detector signal — the same one the gain law compares against — not the speech band, or `headroom` means different things on different films. Speech band is for *detection only*.

Use a **bounded-slew sign-step tracker**, not an exponential mean: `R += slew_per_hop * (observed - R).signum()`. Robust to outliers by construction, f32-safe, and it supports a stateable guarantee: *the reference can never move faster than 0.25 dB/s* — the strongest available answer to "slow AGC ruined my film". Clamp `R` to `[-45, -12]` dBFS and reject observations outside it.

**No feedback loop exists.** The entire sidechain is feedforward off unmodified input samples, following the existing idiom (`compressor.rs` measures `monitor_channels` then applies gain to `process_channels`, never re-reading its output). `reference_db`, confidence and both envelopes are pure functions of the input, so the loop is structurally absent rather than merely damped. All smoothing below is about *audibility*, not stability. Worth a module doc comment.

**Detector: normalized cross-correlation, not mid/side ratio.** `rho = Σ(l·r) / sqrt(Σl²·Σr² + eps)`. A hard-panned source produces large mid *and* large side, so a mid/side ratio reads "somewhat centered" — wrong; ρ→0 correctly. Same cost, and immune to the channel gain imbalance an analog path introduces. Confidence is a product of three terms:
- **centeredness** from ρ
- **speech-band energy fraction** (speech-band power / broadband power) — cheap, and directly kills the "loud centered LF-heavy content" false positive
- **syllabic modulation** (4–8 Hz envelope) — weighted by `modulation_weight`, and deliberately only a term in a product, since rhythmic music also modulates at those rates

**Failure is asymmetric by construction.** Gain law is `GR = GR_base * (1 - protection * C)`, so confidence can only ever *reduce* reduction. A false positive (music judged dialogue) means night mode does less — soft failure. A false negative would duck dialogue — hard failure. The structure makes only the soft direction reachable. Two thresholds on the same C: strict (Schmitt 0.7/0.5) to gate the reference update, continuous and ungated for depth. Asymmetric smoothing — rise τ 0.15 s, fall τ 1.2 s — so gaps between sentences don't duck the next word.

**Where the detector actually earns its keep:** with `headroom = 6 dB`, normal dialogue is already below threshold and untouched. The detector matters for *loud* dialogue — shouting, argument over score, a line during an action beat — which would otherwise take ~4.5 dB of reduction. That is precisely the intelligibility failure of AVR night mode.

**Fixed internal hop, not chunk rate.** At chunksize 4096 / 48 kHz the chunk rate is 11.7 Hz — an 8 Hz bandpass there is above Nyquist. Worse, any statistic computed on chunk boundaries makes the sound change with `chunksize` and makes goldens irreproducible. All slow/statistical state runs on `hop = samplerate/400` (~2.5 ms), with a persistent partial accumulator across chunk boundaries since `chunksize` is not a multiple of the hop.

**Confidence → gain must be low-passed with τ ≥ 1 s.** Ear modulation sensitivity peaks near 4 Hz, so a confidence signal derived from a 4–8 Hz feature driving gain directly would modulate gain at the syllabic rate — the worst possible artifact generator. Hardcode the constant with a comment explaining why, so nobody "optimizes" it.

## Two f32 bugs to avoid (CI tests `--features 32bit`)

1. **The repo's per-sample one-pole dies at long time constants.** `a = (-1/fs/tau).exp()` (`compressor.rs:67`): in f32 `a` rounds to exactly `1.0` once `1-a < eps/2`, so **any follower with τ > ~35 s at 48 kHz freezes completely**, and τ ≈ 10 s already carries ~3% error. Running all slow state at hop rate gives `1-a = 1.25e-4` for a 20 s constant — six hundred times above eps. The sign-step tracker has no coefficient near 1 at all.
2. **Squared accumulators denormalize:** `(1e-20)² = 1e-40`, below f32 min normal `1.2e-38`. Epsilon-floor (`1e-20`) every mean-square before `log10`. Note `Biquad::flush_subnormals` (`biquad.rs:449`) is private and only called from the `Filter::process_waveform` impl, **not** `process_single` — so run detector biquads via `process_waveform` on scratch (free flushing, vectorizable), and for the output LF biquads that need `process_single`, make `flush_subnormals` `pub` and call it per chunk.

## Files to change

| File | Change |
|---|---|
| `src/processors/night_mode.rs` | **New.** `NightMode`, `from_config`, `impl Processor`, `validate_night_mode`, `#[cfg(test)] mod tests` |
| `src/processors/mod.rs:23` | `pub mod night_mode;` |
| `src/config/mod.rs:1490` | `NightMode` variant in `enum Processor` |
| `src/config/mod.rs:~1593` | `NightModeParameters` + accessor `impl`, following `CompressorParameters` (`:1508-1544`) |
| `src/config/utils.rs:716-775` | Validation arm. `fs` is in scope at `:633`, so use `validate_night_mode(fs, params)` to check `bass_frequency < fs/2` |
| `src/pipeline.rs:238-263` | Construction arm returning `Box<dyn Processor>` |
| `src/filters/biquad.rs:449` | `flush_subnormals` → `pub`; add `pub fn set_coefficients` |
| `src/processors/compressor.rs:110`, `noisegate.rs` | **Latent panic fix:** `sum_monitor_channels` does `copy_from_slice(&waveforms[ch])` with no `is_empty()` guard, but capture backends leave unused channels as empty `Vec`s (guarded at `pipeline.rs:119,152`, `mixer.rs:101`). Separate commit so it can be cherry-picked |
| `README.md:~2477` | `### Night Mode` section after Noise Gate, `(*)`-marks-optional convention |
| `exampleconfigs/nightmode.yml`, `testing.md`, `CHANGELOG.md` | Example config, listening-test notes, changelog |

## Reuse

- `BiquadCoefficients::from_config(fs, config::BiquadParameters)` (`biquad.rs:83`), `Biquad::new` `:424`, `process_single` `:435`, `Filter::process_waveform` `:460` — detector bandpass, 60 Hz sidechain HP, LF extraction.
- `Limiter::from_config` / `apply_clip` (`filters/limiter.rs:34,67`) — wired as in `compressor.rs:85-93`. **Note it is a clipper, not a limiter** (`:56-71` is `clamp` or a cubic waveshaper), so it is only acceptable as a net that is essentially never hit: ceiling -1 dB, **soft clip hardcoded `true` and not exposed** (`LimiterParameters::soft_clip` defaults to `false` → hard clip, indefensible here). Log a `debug!` counter when it engages.
- Envelope-follower idiom `compressor.rs:124-148`; `db_to_linear`/`linear_to_db` from `utils/decibels`.
- Preallocated scratch in `from_config` (`compressor.rs:69`); `process_chunk` stays allocation-free. Waveforms are always exactly `chunksize` long at the pipeline (`utils/resampling.rs:232,259`; `file_backend/device.rs:698` zero-pads and sets `valid_frames`), so `chunksize` scratch is correct.
- **Power-sum the detector, don't amplitude-sum.** `compressor.rs:108-116` sums amplitudes unnormalized, so two correlated channels read +6 dB and anti-correlated ones partially cancel. Since our reference window and `silence_threshold` are absolute dB, use `sqrt(Σx_ch²/n)`. Document the deviation.

## Parameters

`channels` required; everything else `Option<T>` with a default.

**Primary** — `amount` (%, default `75`) scales all reduction **in the dB domain**, 0 = bit-transparent; `max_attenuation` (dB, `18`) hard cap; `bass_reduction` (dB, `4`, capped at 10) extra LF reduction proportional to current reduction, 0 disables; `dialogue_channels` (`None` → stereo correlation path).

**Gain law** — `headroom` (dB, `6`), `ratio` (`4.0`), `transient_softening` (%, `50`) depth of stage B, `dialogue_protection` (%, `100`) — **`0` disables the detector entirely**, leaving a plain leveler: the A/B reference and the escape hatch.

**Dialogue presence lift** — `presence_gain` (dB, default `0.0` = off, validator caps at `6.0`), `presence_frequency` (Hz, `2500`), `presence_q` (`0.7`). A boost applied *only* in proportion to dialogue confidence. See "Presence lift" below for why this is safe to build here but would not be safe done naively.

**Advanced** — `attack` (s, `0.15`), `release` (s, `1.5`), `reference_level` (dBFS, `None` = adaptive; pinning it disables adaptation and is **required for deterministic tests**), `reference_slew` (dB/s, `0.25`), `modulation_weight` (`0.5`), `bass_frequency` (Hz, `120`), `silence_threshold` (dB, `-70`, freezes the detector — freeze, not bypass, since bypass would create a discontinuity and the gain law already yields 0 dB there), `ceiling` (dBFS, `-1.0`, cannot be disabled), `monitor_channels` (set to exclude LFE), `process_channels`.

**Cut deliberately:** `peak_*` (four fields folded into `transient_softening`; hardcode 2 ms/80 ms/ref+18 dB/ratio 10 — not tunable by ear without producing artifacts; addable later as `Option` fields, source-compatible); speech band edges (consts 300/3500 Hz, coupled to the modulation calibration); `soft_clip`; any lookahead parameter.

`validate_night_mode` rejects: `release < attack`, `attack ≤ 0`, `ratio < 1`, `amount` outside 0–100, negative `max_attenuation`, `bass_reduction > 10`, `presence_gain` outside `[0, 6]`, `presence_q` outside `[0.3, 3.0]`, `presence_frequency ≥ fs/2`, `ceiling` outside `[-20, 0]`, `bass_frequency ≥ fs/2`, out-of-range channel indices, and **`channels != 2` with `dialogue_channels` unset** — fail with a message rather than guessing which channel is the center, since many 5.1 mixes put score and effects in C.

## Signal flow in `process_chunk`

0. **Guards** — skip empty waveforms; `return Ok(())` if all monitor channels are empty. Slice scratch to the actual waveform length rather than `copy_from_slice` on full length.
1. **Broadband sidechain** — per monitor channel: 60 Hz `HighpassFO` via `process_waveform` on scratch, accumulate squares; `power = Σ/n_monitor + 1e-20`.
2. **Detector sidechain** — L/R (or mean of `dialogue_channels`) through 300 Hz HP + 3500 Hz LP via `process_waveform`.
3. **Hop accumulation** — accumulate broadband power, speech power, `Σlr`, `Σl²`, `Σr²`, `hop_count`; fire stage 4 when the hop fills. Accumulators persist across chunks — this is what makes behaviour chunksize-independent.
4. **Per hop (~375 Hz)** — `bb_db`, `sp_db`, ρ; centeredness and band-ratio terms; hop-rate 5.5 Hz bandpass on `sp_db - sp_db_slow` → modulation term; `conf_raw` = product; asymmetric smoother; then Schmitt gate + min-hold, and if gated and `bb_db ∈ [-45,-12]`, one sign-step of `reference_db` (skipped entirely when `reference_level` is pinned).
5. **Gain curve, per sample** — one `log10` per sample as in the compressor; slow and fast envelope followers; per-sample `conf_smooth` (τ ≥ 1 s); `gr_slow` above `ref+headroom`, `gr_fast` above `ref+18`; `gr = (gr_slow+gr_fast) * (1 - protection*conf_smooth)`; then `gr = (gr * amount).max(-max_att)` — **scale then clamp**; derive the per-sample LF shelf depth from `gr`.
6. **Apply** — `out = (x - k_lf[j]*LP(x) + k_pres[j]*BP(x)) * gain[j]` per processed channel, then `flush_subnormals` on both per-channel biquads, then `apply_clip`. `k_pres[j]` is derived per sample from `conf_smooth` and `presence_gain`.

**Presence lift.** Built as requested, with two structural safeguards that make it the *least* artifact-prone way to do it. First, depth is driven by the **same `conf_smooth` signal the gain law uses** (τ ≥ 1 s), never a faster path — so the boost cannot move at syllabic rate, which is the spectral-pumping failure mode. Second, it uses fixed-coefficient band extraction (`x + k·BP(x)` with a constant `Bandpass` biquad) and a per-sample scalar depth, exactly like the bass shelf, so no coefficient is ever recomputed and there is no zipper. `presence_gain` defaults to `0.0`, and its 6 dB validator cap bounds the worst case. `k_pres = (db_to_linear(presence_gain) - 1) * conf_smooth`, so it is exactly 0 whenever confidence is 0 and the processor is spectrally inert on non-dialogue.

**Two further deliberate choices.** `amount` scales reduction in dB because a signal-domain blend `(1-m)x + m·g·x` is a *linear* interpolation — at `m=0.5` with `g=-20 dB` you get -6 dB, not -10 — besides needing a dry buffer and combing against the shelf, which sits in the wet path only. And the LF shelf is `x - k·LP(x)` with a **fixed** first-order `LowpassFO`, not a variable-gain shelf biquad, because a variable shelf needs `sin`/`cos`/`powf` recomputed whenever `k` moves, limiting you to per-chunk coefficient steps with state discontinuities; band-extract-and-subtract makes depth a per-sample multiply. First order avoids the ~+0.5 dB overshoot a second-order LP would cause near the corner.

**Persistent state** (must survive `update_parameters`): both envelope levels, `reference_db` (init `-27.0`, typical film dialogue RMS), confidence smoothers, gate state and hold counter, hop accumulators and `hop_count`, all biquad histories, `mod_energy`. Rebuild `lf_lp` coefficients only when `bass_frequency` actually changed, so a live tweak doesn't click.

## Artifact risks

| Risk | Mitigation |
|---|---|
| Breathing at 4 Hz (worst case — ear sensitivity peak) | `conf_smooth` τ ≥ 1 s, hardcoded with a comment |
| Gate chatter | Schmitt 0.7/0.5 + ~300 ms min-hold; the *gain* path is ungated and continuous, so it cannot chatter |
| Pumping | `release ≥ attack` enforced, default 1.5 s; stage B capped by `transient_softening` and its +18 dB threshold |
| Reference drift on musicals/concert films | Sign-step slew guarantee + absolute window + strict gate + `reference_level` pin |
| Cold start | Init `-27.0`; worst case a few dB wrong, converging at ≤0.25 dB/s |
| Zipper on LF shelf / presence lift | Fixed coefficients + per-sample depth — structurally impossible |
| Spectral pumping from the presence lift | Depth driven only by `conf_smooth` (τ ≥ 1 s); capped at 6 dB; exactly 0 at zero confidence |
| Click on config reload | Preserve state; conditional coefficient rebuild |
| Denormals / f32 follower freeze | Epsilon floors, explicit flush, hop-rate slow state |
| Clipper distortion | Soft clip hardcoded, ceiling -1 dB, stage B shallow; `debug!` counter when hit |

## Staged implementation

Each stage compiles, passes CI, and is independently listenable; roughly one commit each.

0. **Enabling changes** — `flush_subnormals` pub + `set_coefficients`; `is_empty()` guards in compressor/noisegate (separate commit, own changelog line).
1. **Skeleton + wiring** — config variant, full parameter struct, complete `validate_night_mode`, both match arms, no-op `process_chunk`. *Verify:* validation table passes; `amount=0` transparent trivially; a YAML config loads.
2. **Slow leveler, pinned reference** — flow steps 0/1/5/6. **First listenable milestone**; establishes the tonal baseline nothing later may spoil. *Verify:* burst reduced, cap respected, `amount` monotone, quiet dialogue untouched, ceiling enforced, no NaN on silence, empty waveform no panic.
3. **Fast transient stage.** *Verify:* first 10 ms of a 1 ms-rise burst attenuated >2 dB more than stage 2 while steady-state is unchanged within 0.5 dB.
4. **Hop infrastructure, wired to nothing** — accumulator with cross-chunk carry. **Do this before the detector**; retrofitting it is expensive. *Verify:* `chunksize_independence`, plus hop count == `floor(N*chunksize/hop_frames)` for chunksize ∈ {256, 1000, 1024, 4096} (non-power-of-two exercises partial hops).
5. **Detector: centeredness + band ratio** (`modulation_weight` forced 0). *Verify:* loud centered speech protected; decorrelated music not; hard-panned source not. Listen for breathing — if present, `conf_smooth` τ is too short.
6. **Modulation term.** Calibrate the reference constant against real speech before freezing it.
7. **Adaptive reference** — sign-step, Schmitt, hold, window, slew, pin, silence freeze. *Verify:* slew bounded, window respected, silence freezes; then `debug!` the reference once a second across films with different dialnorm.
8. **Dynamic low shelf.** *Verify:* LF-vs-1 kHz reduction difference 2–5 dB at `bass_reduction: 4`, <0.3 dB at 0. Listen for tone change in quiet scenes — there must be none, since `k → 0` as reduction → 0.
9. **Dialogue presence lift** — fixed `Bandpass` extraction per processed channel, `k_pres` from `conf_smooth`. *Verify:* inert at `presence_gain: 0`; at 3 dB with high confidence, measured gain at `presence_frequency` is within 0.5 dB of requested; no boost on decorrelated music; `k_pres` rate of change bounded by the τ ≥ 1 s smoother. Listen specifically for the boost "riding" speech — if audible, `presence_gain` is the knob, and 0 restores stage 8 behaviour.
10. **Live reconfiguration** — state-preserving `update_parameters`, rebuilding LF and presence coefficients only when their frequencies/Q changed. *Verify:* state unchanged, no click; then the Python config-reload suite with a night-mode config added.
11. **Golden reference** — generate the table only once the design is frozen.
12. **Docs** — README section stating zero added latency, dB-domain `amount`, the slew guarantee, `dialogue_channels` required for non-stereo, "combine with the `Loudness` filter for low-level tonal compensation", and honestly that the detector is heuristic with `dialogue_protection: 0` as the escape hatch.

## Verification

**Unit tests** colocated `#[cfg(test)] mod tests`, reusing the `is_close`/`compare_waveforms` idiom from `biquad.rs:591+` (defined locally per module) and building chunks as in `benches/pipeline.rs:make_chunk`. All tests pin `reference_level` unless the test is about the reference. Note `SmallRng::from_os_rng()` (`generatordevice.rs:97`) is unseeded, so tests must synthesize their own signals.

Key cases: `amount=0` bit-transparent; quiet dialogue untouched; loud burst reduced 8–11 dB; reduction never exceeds cap; **reduction monotone in `amount` with GR(50) ≈ 0.5·GR(100)** (fails if anyone reimplements it as a signal blend); loud centered syllabic speech protected while `dialogue_protection: 0` is not; decorrelated music not protected; **hard-panned source not protected** (the case a mid/side detector fails and ρ passes); unmodulated centered tone not protected at `modulation_weight: 1`; LF extra reduction present and disableable; presence lift inert at 0 dB, correct magnitude at high confidence, absent on decorrelated music; **`chunksize_independence` across 256/1024/4096 within 0.5 dB — the most valuable test in the suite**; reference slew bounded and window respected; silence freezes detector; no NaN on digital silence; empty waveform no panic; ceiling enforced; `update_parameters` preserves state without a click; validation rejection table.

**Golden reference in Rust, not a WAV blob** — build a `config::Configuration` and pump chunks as `benches/pipeline.rs:227-238` does; synthesize a deterministic 20 s scene script (dialogue → silence → burst with a 40 Hz component → decorrelated music → loud dialogue → mixed); reduce to one RMS-dB value per 100 ms; compare against a `const GOLDEN: [f32; 200]` with 0.5 dB tolerance. No binary fixtures, passes under f64 and f32 with one tolerance, and failures read as "the burst section is now 3 dB louder".

**Offline listening script** (not CI) — `testscripts/nightmode_render.py` following existing `testscripts/` conventions, rendering a real clip at `amount` 0 and 75 via `capture: WavFile` / `playback: File` with `wav_header: true`, emitting per-100 ms RMS for both plus the difference curve as text.

**Real test material via `yt-dlp`** (the maintained fork; `youtube-dl` is abandoned). Fetch a film trailer's audio, convert to WAV at 48 kHz stereo, and use it as the input for the render script above. Caveats to keep in mind when reading results: trailers are mastered loud and already compressed, so they understate the 20+ dB dialogue-to-explosion span of a real film — good for the transient and dialogue-over-score cases, weak as a proxy for scene leveling. The upside is that YouTube audio is a stereo downmix, matching the TV aux-out path exactly, so the phantom-center correlation detector gets exercised on representative material. Useful measurements from a trailer: the reference tracker's convergence trajectory (`debug!` once a second), reduction applied during explosions vs dialogue, and whether confidence correctly separates dialogue-over-score from score alone. Clips stay local, outside the repo — consistent with keeping binary fixtures out of git.

**CI parity** — `cargo fmt --all -- --check`, `cargo clippy -- -D warnings`, `cargo check --no-default-features`, `cargo test` including `--features 32bit`. Optionally a `bench_nightmode` guard, since per-sample `log10` plus ~8 biquads per channel makes this the heaviest processor in the repo.

**Real listening** — A/B at `amount` 0/50/75 on actual film content through the real chain, loudness-matched so judgment is about texture not level. Confirm the TV's own auto-volume/night mode is off first, or two compressors fight.

## Noted risk

The dialogue-gated presence lift is included by request, built with the safeguards described above (confidence-smoothed depth, fixed coefficients, 6 dB cap, `0.0` default). The residual risk remains real and cannot be designed away entirely: a gated EQ is the element most likely to read as "processed", and it acts on the one signal we otherwise promise not to touch. It is off by default and stage 9 is the last functional stage, so if listening says it colors voices, setting `presence_gain: 0.0` returns exactly to stage 8 behaviour with no config changes elsewhere. For comparison during tuning, level-dependent tonal compensation also exists in the repo as `filters/loudness.rs` (ISO-226), and a static `Peaking` biquad in the pipeline gives the ungated version for A/B.
