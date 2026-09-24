// CamillaDSP - A flexible tool for processing audio
// Copyright (C) 2026 Henrik Enquist
//
// This file is part of CamillaDSP.
//
// CamillaDSP is free software; you can redistribute it and/or modify it
// under the terms of either:
//
// a) the GNU General Public License version 3,
//    or
// b) the Mozilla Public License Version 2.0.
//
// You should have received copies of the GNU General Public License and the
// Mozilla Public License along with this program. If not, see
// <https://www.gnu.org/licenses/> and <https://www.mozilla.org/MPL/2.0/>.

//! Night mode: a dialogue-preserving dynamics processor for watching films quietly.
//!
//! The whole sidechain is feedforward, derived only from the unmodified input samples.
//! The dialogue reference, the confidence estimate and both envelope followers are
//! therefore pure functions of the input, and the output can never influence them.
//! Every time constant here exists to keep gain movement inaudible, not to keep a
//! control loop stable.

use crate::PrcFmt;
use crate::Res;
use crate::audiochunk::AudioChunk;
use crate::config;
use crate::filters::Filter;
use crate::filters::biquad::{Biquad, BiquadCoefficients};
use crate::filters::limiter::Limiter;
use crate::processors::Processor;
use crate::utils::decibels::db_to_linear;

/// Dialogue level assumed before any has been measured, in dBFS.
const INITIAL_REFERENCE_DB: PrcFmt = -27.0;

/// The dialogue reference is only allowed to track inside this window, in dBFS.
/// Levels outside it are not film dialogue and are rejected rather than followed.
const REFERENCE_MIN_DB: PrcFmt = -45.0;
const REFERENCE_MAX_DB: PrcFmt = -12.0;

/// Corner of the sidechain highpass, in Hz. Keeps rumble from dominating the
/// level estimate, since the bass shelf already deals with low frequencies.
const SIDECHAIN_HIGHPASS_HZ: PrcFmt = 60.0;

/// Floor added to every mean square before converting to dB. Squared sample
/// values underflow to subnormals in f32 well before the level stops mattering.
const POWER_FLOOR: PrcFmt = 1e-20;

/// Level reported before anything has been measured, in dB.
const SILENT_LEVEL_DB: PrcFmt = -200.0;

/// Rate at which the slow, statistical part of the sidechain runs, in Hz.
/// Fixed in time rather than in chunks: anything measured per chunk would change
/// behaviour when the user changes chunksize, and a chunk can be far too long to
/// resolve the syllabic rates the dialogue detector depends on.
const HOP_RATE_HZ: usize = 400;

/// Window used to measure level for the slow stage, in seconds. Short on purpose.
///
/// `attack` and `release` smooth the gain, not this. Smoothing the measurement
/// instead makes the reduction lag the signal in both directions: it arrives late on
/// a loud hit, and on the way back down it keeps holding reduction that the current
/// level no longer justifies, which buries the quiet passage after a loud scene.
const LEVEL_WINDOW_S: PrcFmt = 0.03;

/// Gap between wanted and applied reduction, in dB, beyond which the gain is allowed
/// to recover at the faster scene rate. Stops deep reduction from being carried into
/// material that does not deserve it.
const RELEASE_CATCHUP_DB: PrcFmt = 6.0;

/// Gap at which catching up stops, giving the switch hysteresis. Without it the last
/// few dB would crawl back at the slow rate.
const RELEASE_CATCHUP_CLEAR_DB: PrcFmt = 0.5;

/// Fast stage time constants, in seconds. Fixed rather than exposed: they are not
/// tunable by ear without producing artifacts, and the useful range is narrow.
const FAST_ATTACK_S: PrcFmt = 0.002;
const FAST_RELEASE_S: PrcFmt = 0.08;

/// The fast stage only looks at what sits far above dialogue, in dB above the
/// reference, so ordinary loud content is left to the slow stage.
const FAST_HEADROOM_DB: PrcFmt = 18.0;

/// Slope of the fast stage, equivalent to a ratio of 10:1.
const FAST_SLOPE: PrcFmt = 0.9;

/// Band the dialogue detector listens to, in Hz. Not exposed: the useful range is
/// well established and the band is tied to the detector's calibration.
const SPEECH_LOW_HZ: PrcFmt = 300.0;
const SPEECH_HIGH_HZ: PrcFmt = 3500.0;

/// Correlation range mapped onto "centred". Below the first value the content is
/// spread across the stereo image, above the second it is effectively mono.
const RHO_MIN: PrcFmt = 0.3;
const RHO_SPAN: PrcFmt = 0.6;

/// Speech-band share of total power mapped onto "sounds like speech". Dialogue
/// concentrates its energy in the speech band; explosions and score do not.
const BAND_FRACTION_MIN: PrcFmt = 0.2;
const BAND_FRACTION_SPAN: PrcFmt = 0.4;

/// Confidence believes dialogue quickly and stops believing it slowly, so that
/// pauses between sentences do not duck the start of the next word.
const CONFIDENCE_RISE_S: PrcFmt = 0.15;
const CONFIDENCE_FALL_S: PrcFmt = 1.2;

/// How far above the dialogue reference, in dB, content can sit and still be fully
/// protected, and the range over which that protection fades to nothing.
///
/// Dialogue varies, and someone shouting is legitimately well above the average
/// level of the dialogue around it, so protection has to survive that. But nothing
/// 25 dB above the dialogue reference is dialogue, however centred and speech-like it
/// measures. Without this bound a single false positive hands back unlimited
/// reduction, which is what lets a loud centred impact through untouched.
const PROTECTION_FULL_ABOVE_DB: PrcFmt = 15.0;
const PROTECTION_FADE_DB: PrcFmt = 10.0;

/// Release used after a scene change, in seconds. Without it, the ordinary release
/// keeps ducking for several seconds, so the dialogue that follows an explosion is
/// exactly what gets swallowed. Short enough to be out of the way before the next
/// line, long enough not to be heard moving.
const SCENE_RELEASE_S: PrcFmt = 0.25;

/// Gain reduction, in dB, at which the bass shelf reaches its full depth. Below it
/// the shelf scales in proportion, so quiet scenes get no tonal change at all.
const BASS_FULL_AT_DB: PrcFmt = 12.0;

/// Confidence needed to start trusting a level as dialogue, and the lower value it
/// must fall back through before that trust is withdrawn. The gap stops the tracker
/// flickering on and off around a single threshold.
const GATE_OPEN_CONFIDENCE: PrcFmt = 0.7;
const GATE_CLOSE_CONFIDENCE: PrcFmt = 0.5;

/// Once open, the gate stays open at least this long, in seconds.
const GATE_HOLD_S: PrcFmt = 0.3;

/// Centre and width of the syllabic modulation bandpass, in Hz, run at hop rate.
/// Speech envelopes peak around 4 to 8 Hz. Music modulates there too, which is why
/// this is one factor in a product rather than a decision on its own.
const MODULATION_FREQ_HZ: PrcFmt = 5.5;
const MODULATION_Q: PrcFmt = 1.2;

/// Modulation depth, in dB, treated as unmistakably speech-like.
const MODULATION_FULL_DB: PrcFmt = 2.0;

/// Smoothing for the speech-band level and the modulation estimate, in seconds.
const MODULATION_SMOOTH_S: PrcFmt = 0.7;

/// Time constant between confidence and gain, in seconds. Must stay at or above
/// one second. Hearing is most sensitive to gain modulation around 4 Hz, which is
/// also the rate speech modulates at, so a faster path here would turn the
/// detector into an audible tremolo generator. Do not shorten this.
const CONFIDENCE_TO_GAIN_S: PrcFmt = 1.0;

#[derive(Clone, Debug)]
pub enum DetectorMode {
    /// Phantom centre derived from a stereo pair.
    Stereo,
    /// One or more discrete dialogue channels.
    Discrete(Vec<usize>),
}

#[derive(Clone, Debug)]
pub struct NightMode {
    pub name: String,
    pub channels: usize,
    pub samplerate: usize,
    pub monitor_channels: Vec<usize>,
    pub process_channels: Vec<usize>,
    pub detector: DetectorMode,

    pub amount: PrcFmt,
    pub max_attenuation: PrcFmt,
    pub bass_reduction: PrcFmt,
    pub bass_frequency: PrcFmt,
    pub headroom: PrcFmt,
    pub ratio: PrcFmt,
    pub transient_softening: PrcFmt,
    pub dialogue_protection: PrcFmt,
    pub presence_gain: PrcFmt,
    pub presence_frequency: PrcFmt,
    pub presence_q: PrcFmt,
    pub modulation_weight: PrcFmt,
    pub silence_threshold: PrcFmt,
    pub reference_pinned: Option<PrcFmt>,
    pub reference_slew: PrcFmt,
    pub limiter: Limiter,

    slow_attack: PrcFmt,
    slow_release: PrcFmt,
    scene_release: PrcFmt,
    level_smooth: PrcFmt,
    fast_attack: PrcFmt,
    fast_release: PrcFmt,

    // Persistent DSP state. Must survive update_parameters.
    pub reference_db: PrcFmt,
    env_slow_power: PrcFmt,
    env_fast_power: PrcFmt,
    reduction_db: PrcFmt,
    catching_up: bool,
    sidechain_highpass: Vec<Biquad>,
    clipping: bool,

    // Hop state. The accumulator carries across chunk boundaries, so a hop is
    // always the same length in samples no matter how the chunks fall.
    hop_frames: usize,
    hop_count: usize,
    acc_broadband: PrcFmt,
    acc_mid_full: PrcFmt,
    acc_mid_speech: PrcFmt,
    acc_lr: PrcFmt,
    acc_left: PrcFmt,
    acc_right: PrcFmt,
    hop_level_db: PrcFmt,
    hops_measured: usize,

    // Dialogue confidence. conf_hop is updated once per hop, conf_smooth follows it
    // per sample and is the only thing the gain law is allowed to see.
    speech_highpass: Vec<Biquad>,
    speech_lowpass: Vec<Biquad>,
    confidence_rise: PrcFmt,
    confidence_fall: PrcFmt,
    confidence_to_gain: PrcFmt,
    conf_hop: PrcFmt,
    conf_smooth: PrcFmt,
    modulation_bandpass: Biquad,
    modulation_smooth: PrcFmt,
    speech_db_slow: PrcFmt,
    modulation_energy: PrcFmt,

    // Reference tracking. A fixed step per hop rather than an average, so the rate
    // of change has a stated ceiling and one misjudged passage cannot drag it far.
    reference_step: PrcFmt,
    gate_hold_hops: usize,
    gate_open: bool,
    gate_hold: usize,

    // Dynamic bass shelf. The band is extracted with fixed coefficients and scaled
    // by a per-sample factor, so shelf depth can move without ever recomputing a
    // coefficient, which is what would otherwise cause zipper noise.
    bass_lowpass: Vec<Biquad>,
    bass_depth_max: PrcFmt,

    // Presence lift, built the same way as the bass shelf: fixed coefficients, and a
    // per-sample depth taken from the confidence smoother. Because that smoother is
    // held at a second or more, the boost cannot move at syllabic rate, which is what
    // would make a gated EQ audible as spectral pumping.
    presence_bandpass: Vec<Biquad>,
    presence_depth_max: PrcFmt,

    // Scratch, allocated once.
    sc_gain: Vec<PrcFmt>,
    sc_tmp: Vec<PrcFmt>,
    sc_left: Vec<PrcFmt>,
    sc_right: Vec<PrcFmt>,
    sc_mid: Vec<PrcFmt>,
    sc_bass: Vec<PrcFmt>,
    sc_presence: Vec<PrcFmt>,
}

/// One-pole smoothing coefficient for a time constant in seconds.
fn smoothing_coeff(time_constant: PrcFmt, rate: PrcFmt) -> PrcFmt {
    (-1.0 / rate / time_constant).exp()
}

impl NightMode {
    /// Creates a NightMode processor from a config struct
    pub fn from_config(
        name: &str,
        config: config::NightModeParameters,
        samplerate: usize,
        chunksize: usize,
    ) -> Self {
        let channels = config.channels;
        let mut monitor_channels = config.monitor_channels();
        if monitor_channels.is_empty() {
            monitor_channels.extend(0..channels);
        }
        let mut process_channels = config.process_channels();
        if process_channels.is_empty() {
            process_channels.extend(0..channels);
        }
        let dialogue_channels = config.dialogue_channels();
        let detector = if dialogue_channels.is_empty() {
            DetectorMode::Stereo
        } else {
            DetectorMode::Discrete(dialogue_channels)
        };

        let limiter = Limiter::from_config(
            "Ceiling",
            config::LimiterParameters {
                clip_limit: config.ceiling(),
                soft_clip: Some(true),
            },
        );

        let reference_db = config.reference_level.unwrap_or(INITIAL_REFERENCE_DB);
        let srate = samplerate as PrcFmt;
        let hop_rate = (samplerate / HOP_RATE_HZ).max(1);
        let hop_rate = srate / hop_rate as PrcFmt;
        let sidechain_highpass = make_sidechain_highpass(samplerate, monitor_channels.len());
        let process_channel_count = process_channels.len();

        debug!(
            "Creating night mode '{}', channels: {}, monitor_channels: {:?}, process_channels: {:?}, detector: {:?}, amount: {}, max_attenuation: {}, headroom: {}, ratio: {}",
            name,
            channels,
            monitor_channels,
            process_channels,
            detector,
            config.amount(),
            config.max_attenuation(),
            config.headroom(),
            config.ratio()
        );

        NightMode {
            name: name.to_string(),
            channels,
            samplerate,
            monitor_channels,
            process_channels,
            detector,
            amount: config.amount() / 100.0,
            max_attenuation: config.max_attenuation(),
            bass_reduction: config.bass_reduction(),
            bass_frequency: config.bass_frequency(),
            headroom: config.headroom(),
            ratio: config.ratio(),
            transient_softening: config.transient_softening() / 100.0,
            dialogue_protection: config.dialogue_protection() / 100.0,
            presence_gain: config.presence_gain(),
            presence_frequency: config.presence_frequency(),
            presence_q: config.presence_q(),
            modulation_weight: config.modulation_weight(),
            silence_threshold: config.silence_threshold(),
            reference_pinned: config.reference_level,
            reference_slew: config.reference_slew(),
            limiter,
            slow_attack: smoothing_coeff(config.attack(), srate),
            slow_release: smoothing_coeff(config.release(), srate),
            scene_release: smoothing_coeff(SCENE_RELEASE_S, srate),
            level_smooth: smoothing_coeff(LEVEL_WINDOW_S, srate),
            fast_attack: smoothing_coeff(FAST_ATTACK_S, srate),
            fast_release: smoothing_coeff(FAST_RELEASE_S, srate),
            reference_db,
            env_slow_power: 0.0,
            env_fast_power: 0.0,
            reduction_db: 0.0,
            catching_up: false,
            sidechain_highpass,
            clipping: false,
            hop_frames: (samplerate / HOP_RATE_HZ).max(1),
            hop_count: 0,
            acc_broadband: 0.0,
            acc_mid_full: 0.0,
            acc_mid_speech: 0.0,
            acc_lr: 0.0,
            acc_left: 0.0,
            acc_right: 0.0,
            hop_level_db: SILENT_LEVEL_DB,
            hops_measured: 0,
            speech_highpass: make_speech_filters(samplerate, true),
            speech_lowpass: make_speech_filters(samplerate, false),
            confidence_rise: smoothing_coeff(CONFIDENCE_RISE_S, hop_rate),
            confidence_fall: smoothing_coeff(CONFIDENCE_FALL_S, hop_rate),
            confidence_to_gain: smoothing_coeff(CONFIDENCE_TO_GAIN_S, srate),
            conf_hop: 0.0,
            conf_smooth: 0.0,
            modulation_bandpass: make_modulation_bandpass(hop_rate),
            modulation_smooth: smoothing_coeff(MODULATION_SMOOTH_S, hop_rate),
            speech_db_slow: SILENT_LEVEL_DB,
            modulation_energy: 0.0,
            reference_step: config.reference_slew() / hop_rate,
            gate_hold_hops: (GATE_HOLD_S * hop_rate).round() as usize,
            gate_open: false,
            gate_hold: 0,
            bass_lowpass: make_bass_lowpass(
                samplerate,
                config.bass_frequency(),
                process_channel_count,
            ),
            bass_depth_max: bass_depth(config.bass_reduction()),
            presence_bandpass: make_presence_bandpass(
                samplerate,
                config.presence_frequency(),
                config.presence_q(),
                process_channel_count,
            ),
            presence_depth_max: presence_depth(config.presence_gain()),
            sc_gain: vec![0.0; chunksize],
            sc_tmp: vec![0.0; chunksize],
            sc_left: vec![0.0; chunksize],
            sc_right: vec![0.0; chunksize],
            sc_mid: vec![0.0; chunksize],
            sc_bass: vec![0.0; chunksize],
            sc_presence: vec![0.0; chunksize],
        }
    }

    /// Number of frames carried by this chunk, or None when no monitored
    /// channel holds any data. Unused capture channels arrive as empty vectors.
    fn active_frames(&self, input: &AudioChunk) -> Option<usize> {
        self.monitor_channels
            .iter()
            .map(|ch| input.waveforms[*ch].len())
            .find(|len| *len > 0)
            .map(|len| len.min(self.sc_gain.len()))
    }

    /// Mean power of the monitored channels, highpassed, written to self.sc_gain.
    /// Power summed rather than amplitude summed, so the result does not depend on
    /// whether the monitored channels happen to be correlated.
    fn measure_power(&mut self, input: &AudioChunk, frames: usize) {
        self.sc_gain[..frames].fill(0.0);
        let mut counted = 0;
        for (ch, highpass) in self
            .monitor_channels
            .iter()
            .zip(self.sidechain_highpass.iter_mut())
        {
            let waveform = &input.waveforms[*ch];
            if waveform.is_empty() {
                continue;
            }
            let len = frames.min(waveform.len());
            self.sc_tmp[..len].copy_from_slice(&waveform[..len]);
            highpass.process_waveform(&mut self.sc_tmp[..len]).unwrap();
            for (acc, val) in self.sc_gain[..len]
                .iter_mut()
                .zip(self.sc_tmp[..len].iter())
            {
                *acc += *val * *val;
            }
            counted += 1;
        }
        if counted > 1 {
            let scale = 1.0 / counted as PrcFmt;
            for val in self.sc_gain[..frames].iter_mut() {
                *val *= scale;
            }
        }
    }

    /// Prepare the detector signals: speech-band left and right in self.sc_left and
    /// self.sc_right, and the full-band centre power in self.sc_mid.
    ///
    /// Correlation is measured inside the speech band so that mono bass, which is
    /// mono in almost every mix, cannot pass as dialogue.
    fn measure_detector(&mut self, input: &AudioChunk, frames: usize) {
        match &self.detector {
            DetectorMode::Stereo => {
                let left = &input.waveforms[0];
                let right = &input.waveforms[1];
                if left.is_empty() || right.is_empty() {
                    self.sc_mid[..frames].fill(0.0);
                    self.sc_left[..frames].fill(0.0);
                    self.sc_right[..frames].fill(0.0);
                    return;
                }
                for j in 0..frames {
                    let mid = 0.5 * (left[j] + right[j]);
                    self.sc_mid[j] = mid * mid;
                }
                self.sc_left[..frames].copy_from_slice(&left[..frames]);
                self.sc_right[..frames].copy_from_slice(&right[..frames]);
            }
            DetectorMode::Discrete(channels) => {
                self.sc_left[..frames].fill(0.0);
                let mut counted = 0;
                for ch in channels.iter() {
                    let waveform = &input.waveforms[*ch];
                    if waveform.is_empty() {
                        continue;
                    }
                    for (acc, val) in self.sc_left[..frames].iter_mut().zip(waveform.iter()) {
                        *acc += *val;
                    }
                    counted += 1;
                }
                if counted > 1 {
                    let scale = 1.0 / counted as PrcFmt;
                    for val in self.sc_left[..frames].iter_mut() {
                        *val *= scale;
                    }
                }
                for j in 0..frames {
                    self.sc_mid[j] = self.sc_left[j] * self.sc_left[j];
                }
                // A discrete dialogue channel is centred by definition, so the
                // correlation term is fixed at 1 by comparing the signal with itself.
                self.sc_right[..frames].copy_from_slice(&self.sc_left[..frames]);
            }
        }
        self.speech_highpass[0]
            .process_waveform(&mut self.sc_left[..frames])
            .unwrap();
        self.speech_lowpass[0]
            .process_waveform(&mut self.sc_left[..frames])
            .unwrap();
        self.speech_highpass[1]
            .process_waveform(&mut self.sc_right[..frames])
            .unwrap();
        self.speech_lowpass[1]
            .process_waveform(&mut self.sc_right[..frames])
            .unwrap();
    }

    /// Accumulate the measured power into fixed-length hops, completing as many as
    /// the chunk contains and carrying the remainder over to the next chunk.
    /// Must run before compute_gain, which overwrites the power with the gain.
    fn accumulate_hops(&mut self, frames: usize) {
        let mut index = 0;
        while index < frames {
            let take = (self.hop_frames - self.hop_count).min(frames - index);
            for j in index..index + take {
                self.acc_broadband += self.sc_gain[j];
                self.acc_mid_full += self.sc_mid[j];
                let left = self.sc_left[j];
                let right = self.sc_right[j];
                let mid = 0.5 * (left + right);
                self.acc_mid_speech += mid * mid;
                self.acc_lr += left * right;
                self.acc_left += left * left;
                self.acc_right += right * right;
            }
            self.hop_count += take;
            index += take;
            if self.hop_count == self.hop_frames {
                self.finish_hop();
            }
        }
    }

    fn finish_hop(&mut self) {
        let frames = self.hop_frames as PrcFmt;
        self.hop_level_db = 10.0 * (self.acc_broadband / frames + POWER_FLOOR).log10();

        let rho = self.acc_lr / (self.acc_left * self.acc_right + POWER_FLOOR).sqrt();
        let centred = ((rho - RHO_MIN) / RHO_SPAN).clamp(0.0, 1.0);
        let fraction = self.acc_mid_speech / (self.acc_mid_full + POWER_FLOOR);
        let speechlike = ((fraction - BAND_FRACTION_MIN) / BAND_FRACTION_SPAN).clamp(0.0, 1.0);

        // Syllabic modulation: how much the speech-band level swings at speech rates.
        // Measured as a deviation from its own slow mean, so absolute level drops out.
        let speech_db = 10.0 * (self.acc_mid_speech / frames + POWER_FLOOR).log10();
        if self.speech_db_slow <= SILENT_LEVEL_DB {
            self.speech_db_slow = speech_db;
        }
        self.speech_db_slow = self.modulation_smooth * self.speech_db_slow
            + (1.0 - self.modulation_smooth) * speech_db;
        let swing = self
            .modulation_bandpass
            .process_single(speech_db - self.speech_db_slow);
        self.modulation_energy = self.modulation_smooth * self.modulation_energy
            + (1.0 - self.modulation_smooth) * swing.abs();
        self.modulation_bandpass.flush_subnormals();
        let modulation = (self.modulation_energy / MODULATION_FULL_DB).clamp(0.0, 1.0);

        let weight = self.modulation_weight;
        let raw = centred * speechlike * (1.0 - weight + weight * modulation);

        // Silence carries no usable correlation or spectrum, so hold the previous
        // estimate rather than letting the noise floor move it.
        if self.hop_level_db > self.silence_threshold {
            let coeff = if raw > self.conf_hop {
                self.confidence_rise
            } else {
                self.confidence_fall
            };
            self.conf_hop = coeff * self.conf_hop + (1.0 - coeff) * raw;
            self.track_reference();
        }

        self.acc_broadband = 0.0;
        self.acc_mid_full = 0.0;
        self.acc_mid_speech = 0.0;
        self.acc_lr = 0.0;
        self.acc_left = 0.0;
        self.acc_right = 0.0;
        self.hop_count = 0;
        self.hops_measured += 1;

        // Once a second, so the reference and the detector can be followed on real
        // material. Tuning either by ear without seeing these is guesswork.
        if self.hops_measured.is_multiple_of(HOP_RATE_HZ) {
            debug!(
                "Night mode '{}': level {:.1} dB, reference {:.1} dB, confidence {:.2}, gate {}",
                self.name, self.hop_level_db, self.reference_db, self.conf_hop, self.gate_open
            );
        }
    }

    /// Move the dialogue reference one bounded step toward the current level, if the
    /// current level is credible as dialogue.
    ///
    /// A fixed step toward the observation rather than a weighted average: the rate
    /// of change then has a hard ceiling that can be stated, and a stretch of
    /// misclassified music moves the reference by a bounded amount instead of
    /// proportionally to how wrong it was.
    fn track_reference(&mut self) {
        if self.reference_pinned.is_some() {
            return;
        }

        if self.gate_hold > 0 {
            self.gate_hold -= 1;
        }
        let threshold = if self.gate_open {
            GATE_CLOSE_CONFIDENCE
        } else {
            GATE_OPEN_CONFIDENCE
        };
        if self.conf_hop > threshold {
            if !self.gate_open {
                self.gate_open = true;
                self.gate_hold = self.gate_hold_hops;
            }
        } else if self.gate_hold == 0 {
            self.gate_open = false;
        }

        if !self.gate_open {
            return;
        }
        // Levels outside the window are not film dialogue. Reject the observation
        // rather than following it.
        if !(REFERENCE_MIN_DB..=REFERENCE_MAX_DB).contains(&self.hop_level_db) {
            return;
        }
        let difference = self.hop_level_db - self.reference_db;
        if difference.abs() < self.reference_step {
            self.reference_db = self.hop_level_db;
        } else {
            self.reference_db += self.reference_step * difference.signum();
        }
        self.reference_db = self.reference_db.clamp(REFERENCE_MIN_DB, REFERENCE_MAX_DB);
    }

    /// Turn the power in self.sc_gain into a linear gain, in place.
    ///
    /// Two stages in series with deliberately different speeds. The slow one does
    /// most of the work and is too slow to be heard moving. The fast one only
    /// catches the leading edge the slow one misses, and stays shallow so it does
    /// not flatten the attack that makes an impact sound like an impact.
    fn compute_gain(&mut self, frames: usize) {
        let threshold = self.reference_db + self.headroom;
        let slope = (self.ratio - 1.0) / self.ratio;
        let fast_threshold = self.reference_db + FAST_HEADROOM_DB;
        for (index, val) in self.sc_gain[..frames].iter_mut().enumerate() {
            let power = *val;

            // Level is measured over a short fixed window, in the power domain. A
            // one-pole in the dB domain would have to traverse the whole way up from
            // the silence floor, making a nominally fast response arbitrarily slow
            // after a quiet passage.
            self.env_slow_power =
                self.level_smooth * self.env_slow_power + (1.0 - self.level_smooth) * power;

            let fast_coeff = if power >= self.env_fast_power {
                self.fast_attack
            } else {
                self.fast_release
            };
            self.env_fast_power = fast_coeff * self.env_fast_power + (1.0 - fast_coeff) * power;

            // Keep the decay tails out of subnormal range.
            if self.env_slow_power < POWER_FLOOR {
                self.env_slow_power = 0.0;
            }
            if self.env_fast_power < POWER_FLOOR {
                self.env_fast_power = 0.0;
            }

            let slow_db = 10.0 * (self.env_slow_power + POWER_FLOOR).log10();
            let fast_db = 10.0 * (self.env_fast_power + POWER_FLOOR).log10();

            let slow_reduction = if slow_db > threshold {
                -(slow_db - threshold) * slope
            } else {
                0.0
            };
            let fast_reduction = if fast_db > fast_threshold {
                -(fast_db - fast_threshold) * FAST_SLOPE * self.transient_softening
            } else {
                0.0
            };

            self.conf_smooth = self.confidence_to_gain * self.conf_smooth
                + (1.0 - self.confidence_to_gain) * self.conf_hop;

            // Confidence can only ever take reduction away, never add it. A false
            // positive therefore means night mode does less, while the failure that
            // would actually hurt, ducking speech, is not reachable from here.
            //
            // How much it can take away is bounded by how plausible the level is as
            // dialogue, so a loud impact that happens to measure as centred and
            // speech-like still gets dealt with.
            let above = slow_db - (self.reference_db + PROTECTION_FULL_ABOVE_DB);
            let plausible = 1.0 - (above / PROTECTION_FADE_DB).clamp(0.0, 1.0);
            let protection = 1.0 - self.dialogue_protection * self.conf_smooth * plausible;
            // Smooth the slow stage's reduction in dB, rather than smoothing the
            // measurement. `attack` sets how fast it deepens, `release` how fast it
            // lets go. Once the applied reduction is far deeper than the signal
            // warrants, recover at the scene rate until the gap is closed, so a quiet
            // passage after something loud is not left buried.
            if slow_reduction - self.reduction_db > RELEASE_CATCHUP_DB {
                self.catching_up = true;
            } else if slow_reduction - self.reduction_db < RELEASE_CATCHUP_CLEAR_DB {
                self.catching_up = false;
            }
            let coeff = if slow_reduction < self.reduction_db {
                self.slow_attack
            } else if self.catching_up {
                self.scene_release
            } else {
                self.slow_release
            };
            self.reduction_db = coeff * self.reduction_db + (1.0 - coeff) * slow_reduction;

            // The fast stage is deliberately not smoothed here: its own follower is
            // already fast, and putting it behind the slow stage's attack would defeat
            // the reason it exists.
            let mut reduction = (self.reduction_db + fast_reduction) * protection;
            reduction = (reduction * self.amount).max(-self.max_attenuation);

            // The shelf follows how hard the processor is working, so a quiet scene
            // gets no tonal change whatsoever.
            self.sc_bass[index] =
                self.bass_depth_max * (-reduction / BASS_FULL_AT_DB).clamp(0.0, 1.0);
            // Exactly zero whenever confidence is zero, so the processor is
            // spectrally inert on everything that is not dialogue.
            self.sc_presence[index] = self.presence_depth_max * self.conf_smooth;
            *val = db_to_linear(reduction);
        }
    }

    fn apply(&mut self, input: &mut AudioChunk, frames: usize) {
        // With no amount and no presence lift there is no gain to apply and nothing
        // this processor could have pushed over the ceiling, so leave the audio
        // completely alone. That keeps `amount: 0` an honest A/B reference even for
        // material already mastered above the ceiling.
        if self.amount == 0.0 && self.presence_depth_max == 0.0 {
            return;
        }
        let mut clipped = false;
        let shelving = self.bass_depth_max > 0.0;
        let lifting = self.presence_depth_max > 0.0;
        for (index, ch) in self.process_channels.iter().enumerate() {
            let waveform = &mut input.waveforms[*ch];
            if waveform.is_empty() {
                continue;
            }
            let len = frames.min(waveform.len());
            let mut peak: PrcFmt = 0.0;
            let lowpass = &mut self.bass_lowpass[index];
            let bandpass = &mut self.presence_bandpass[index];
            for (((sample, gain), bass), presence) in waveform[..len]
                .iter_mut()
                .zip(self.sc_gain[..len].iter())
                .zip(self.sc_bass[..len].iter())
                .zip(self.sc_presence[..len].iter())
            {
                let mut value = *sample;
                if shelving {
                    value -= *bass * lowpass.process_single(value);
                }
                if lifting {
                    value += *presence * bandpass.process_single(value);
                }
                value *= *gain;
                *sample = value;
                peak = peak.max(value.abs());
            }
            // process_single does not flush on its own.
            if shelving {
                lowpass.flush_subnormals();
            }
            if lifting {
                bandpass.flush_subnormals();
            }
            // Soft clipping shapes every sample it sees, so only reach for it when
            // something actually exceeds the ceiling. Otherwise it would add a little
            // distortion to quiet material that never needed limiting.
            if peak > self.limiter.clip_limit {
                self.limiter.apply_clip(&mut waveform[..len]);
                clipped = true;
            }
        }
        if clipped != self.clipping {
            self.clipping = clipped;
            if clipped {
                debug!("Night mode '{}' is hitting the ceiling", self.name);
            }
        }
    }
}

/// Shelf depth as a subtraction factor. Subtracting `depth` times a lowpassed copy
/// leaves a gain of `1 - depth` at DC, so this inverts that for a wanted dB figure.
fn bass_depth(reduction_db: PrcFmt) -> PrcFmt {
    1.0 - db_to_linear(-reduction_db)
}

/// Lift depth as an addition factor. Adding `depth` times a unity-gain bandpassed
/// copy gives a gain of `1 + depth` at the centre frequency.
fn presence_depth(gain_db: PrcFmt) -> PrcFmt {
    db_to_linear(gain_db) - 1.0
}

fn make_presence_bandpass(samplerate: usize, freq: PrcFmt, q: PrcFmt, count: usize) -> Vec<Biquad> {
    let coeffs = BiquadCoefficients::from_config(
        samplerate,
        config::BiquadParameters::Bandpass(config::NotchWidth::Q { freq, q }),
    );
    (0..count)
        .map(|n| Biquad::new(&format!("Presence extract {n}"), samplerate, coeffs))
        .collect()
}

fn make_bass_lowpass(samplerate: usize, freq: PrcFmt, count: usize) -> Vec<Biquad> {
    // First order on purpose. A second order lowpass is 90 degrees out of phase at
    // its corner, and subtracting it would put a small bump just above the corner.
    let coeffs =
        BiquadCoefficients::from_config(samplerate, config::BiquadParameters::LowpassFO { freq });
    (0..count)
        .map(|n| Biquad::new(&format!("Bass extract {n}"), samplerate, coeffs))
        .collect()
}

/// Bandpass for the syllabic modulation estimate. Runs at hop rate, not at sample
/// rate, which is the only way a few-Hz filter is affordable.
fn make_modulation_bandpass(hop_rate: PrcFmt) -> Biquad {
    let rate = hop_rate.round() as usize;
    let coeffs = BiquadCoefficients::from_config(
        rate,
        config::BiquadParameters::Bandpass(config::NotchWidth::Q {
            freq: MODULATION_FREQ_HZ,
            q: MODULATION_Q,
        }),
    );
    Biquad::new("Modulation bandpass", rate, coeffs)
}

fn make_speech_filters(samplerate: usize, highpass: bool) -> Vec<Biquad> {
    let parameters = if highpass {
        config::BiquadParameters::Highpass {
            freq: SPEECH_LOW_HZ,
            q: 0.707,
        }
    } else {
        config::BiquadParameters::Lowpass {
            freq: SPEECH_HIGH_HZ,
            q: 0.707,
        }
    };
    let coeffs = BiquadCoefficients::from_config(samplerate, parameters);
    let label = if highpass { "highpass" } else { "lowpass" };
    (0..2)
        .map(|n| Biquad::new(&format!("Speech {label} {n}"), samplerate, coeffs))
        .collect()
}

fn make_sidechain_highpass(samplerate: usize, count: usize) -> Vec<Biquad> {
    let coeffs = BiquadCoefficients::from_config(
        samplerate,
        config::BiquadParameters::HighpassFO {
            freq: SIDECHAIN_HIGHPASS_HZ,
        },
    );
    (0..count)
        .map(|n| Biquad::new(&format!("Sidechain highpass {n}"), samplerate, coeffs))
        .collect()
}

impl Processor for NightMode {
    fn name(&self) -> &str {
        &self.name
    }

    fn process_chunk(&mut self, input: &mut AudioChunk) -> Res<()> {
        let Some(frames) = self.active_frames(input) else {
            return Ok(());
        };
        self.measure_power(input, frames);
        self.measure_detector(input, frames);
        self.accumulate_hops(frames);
        self.compute_gain(frames);
        self.apply(input, frames);
        Ok(())
    }

    fn update_parameters(&mut self, config: config::Processor) {
        if let config::Processor::NightMode {
            parameters: config, ..
        } = config
        {
            let channels = config.channels;
            let mut monitor_channels = config.monitor_channels();
            if monitor_channels.is_empty() {
                monitor_channels.extend(0..channels);
            }
            let mut process_channels = config.process_channels();
            if process_channels.is_empty() {
                process_channels.extend(0..channels);
            }
            let dialogue_channels = config.dialogue_channels();
            self.detector = if dialogue_channels.is_empty() {
                DetectorMode::Stereo
            } else {
                DetectorMode::Discrete(dialogue_channels)
            };

            self.monitor_channels = monitor_channels;
            self.process_channels = process_channels;
            self.amount = config.amount() / 100.0;
            self.max_attenuation = config.max_attenuation();
            self.bass_reduction = config.bass_reduction();
            self.bass_depth_max = bass_depth(config.bass_reduction());
            if self.bass_lowpass.len() != self.process_channels.len() {
                self.bass_lowpass = make_bass_lowpass(
                    self.samplerate,
                    config.bass_frequency(),
                    self.process_channels.len(),
                );
            } else if self.bass_frequency != config.bass_frequency() {
                // Swap coefficients and keep the filter state, so retuning the corner
                // while playing does not click.
                let coeffs = BiquadCoefficients::from_config(
                    self.samplerate,
                    config::BiquadParameters::LowpassFO {
                        freq: config.bass_frequency(),
                    },
                );
                for filter in self.bass_lowpass.iter_mut() {
                    filter.set_coefficients(coeffs);
                }
            }
            self.bass_frequency = config.bass_frequency();
            self.headroom = config.headroom();
            self.ratio = config.ratio();
            self.transient_softening = config.transient_softening() / 100.0;
            self.dialogue_protection = config.dialogue_protection() / 100.0;
            self.presence_gain = config.presence_gain();
            self.presence_depth_max = presence_depth(config.presence_gain());
            if self.presence_bandpass.len() != self.process_channels.len() {
                self.presence_bandpass = make_presence_bandpass(
                    self.samplerate,
                    config.presence_frequency(),
                    config.presence_q(),
                    self.process_channels.len(),
                );
            } else if self.presence_frequency != config.presence_frequency()
                || self.presence_q != config.presence_q()
            {
                let coeffs = BiquadCoefficients::from_config(
                    self.samplerate,
                    config::BiquadParameters::Bandpass(config::NotchWidth::Q {
                        freq: config.presence_frequency(),
                        q: config.presence_q(),
                    }),
                );
                for filter in self.presence_bandpass.iter_mut() {
                    filter.set_coefficients(coeffs);
                }
            }
            self.presence_frequency = config.presence_frequency();
            self.presence_q = config.presence_q();
            self.modulation_weight = config.modulation_weight();
            self.silence_threshold = config.silence_threshold();
            self.reference_slew = config.reference_slew();
            self.reference_step =
                config.reference_slew() / (self.samplerate as PrcFmt / self.hop_frames as PrcFmt);
            self.reference_pinned = config.reference_level;
            if let Some(level) = config.reference_level {
                self.reference_db = level;
            }
            let srate = self.samplerate as PrcFmt;
            self.slow_attack = smoothing_coeff(config.attack(), srate);
            self.slow_release = smoothing_coeff(config.release(), srate);
            self.scene_release = smoothing_coeff(SCENE_RELEASE_S, srate);
            self.level_smooth = smoothing_coeff(LEVEL_WINDOW_S, srate);
            if self.sidechain_highpass.len() != self.monitor_channels.len() {
                self.sidechain_highpass =
                    make_sidechain_highpass(self.samplerate, self.monitor_channels.len());
            }
            self.limiter = Limiter::from_config(
                "Ceiling",
                config::LimiterParameters {
                    clip_limit: config.ceiling(),
                    soft_clip: Some(true),
                },
            );

            debug!(
                "Updated night mode '{}', monitor_channels: {:?}, process_channels: {:?}, amount: {}, max_attenuation: {}, headroom: {}, ratio: {}",
                self.name,
                self.monitor_channels,
                self.process_channels,
                config.amount(),
                config.max_attenuation(),
                config.headroom(),
                config.ratio()
            );
        } else {
            // This should never happen unless there is a bug somewhere else
            panic!("Invalid config change!");
        }
    }
}

/// Validate the night mode config, to give a helpful message instead of a panic.
pub fn validate_night_mode(samplerate: usize, config: &config::NightModeParameters) -> Res<()> {
    let channels = config.channels;
    let maxfreq = samplerate as PrcFmt / 2.0;

    if config.attack() <= 0.0 {
        let msg = "Attack value must be larger than zero.";
        return Err(config::ConfigError::new(msg).into());
    }
    if config.release() < config.attack() {
        let msg = "Release value must not be shorter than the attack value.";
        return Err(config::ConfigError::new(msg).into());
    }
    if config.ratio() < 1.0 {
        let msg = "Ratio must be 1.0 or larger.";
        return Err(config::ConfigError::new(msg).into());
    }
    if !(0.0..=100.0).contains(&config.amount()) {
        let msg = "Amount must be between 0 and 100 percent.";
        return Err(config::ConfigError::new(msg).into());
    }
    if config.max_attenuation() < 0.0 {
        let msg = "Max attenuation must not be negative.";
        return Err(config::ConfigError::new(msg).into());
    }
    if !(0.0..=10.0).contains(&config.bass_reduction()) {
        let msg = "Bass reduction must be between 0 and 10 dB.";
        return Err(config::ConfigError::new(msg).into());
    }
    if !(0.0..=100.0).contains(&config.transient_softening()) {
        let msg = "Transient softening must be between 0 and 100 percent.";
        return Err(config::ConfigError::new(msg).into());
    }
    if !(0.0..=100.0).contains(&config.dialogue_protection()) {
        let msg = "Dialogue protection must be between 0 and 100 percent.";
        return Err(config::ConfigError::new(msg).into());
    }
    if !(0.0..=6.0).contains(&config.presence_gain()) {
        let msg = "Presence gain must be between 0 and 6 dB.";
        return Err(config::ConfigError::new(msg).into());
    }
    if !(0.3..=3.0).contains(&config.presence_q()) {
        let msg = "Presence Q must be between 0.3 and 3.0.";
        return Err(config::ConfigError::new(msg).into());
    }
    if config.presence_frequency() >= maxfreq {
        let msg =
            format!("Presence frequency must be lower than samplerate/2, which is {maxfreq}.");
        return Err(config::ConfigError::new(&msg).into());
    }
    if config.bass_frequency() >= maxfreq {
        let msg = format!("Bass frequency must be lower than samplerate/2, which is {maxfreq}.");
        return Err(config::ConfigError::new(&msg).into());
    }
    if !(0.0..=1.0).contains(&config.modulation_weight()) {
        let msg = "Modulation weight must be between 0 and 1.";
        return Err(config::ConfigError::new(msg).into());
    }
    if config.reference_slew() <= 0.0 {
        let msg = "Reference slew must be larger than zero.";
        return Err(config::ConfigError::new(msg).into());
    }
    if !(-20.0..=0.0).contains(&config.ceiling()) {
        let msg = "Ceiling must be between -20 and 0 dBFS.";
        return Err(config::ConfigError::new(msg).into());
    }
    if let Some(level) = config.reference_level
        && !(REFERENCE_MIN_DB..=REFERENCE_MAX_DB).contains(&level)
    {
        let msg = format!(
            "Reference level must be between {REFERENCE_MIN_DB} and {REFERENCE_MAX_DB} dBFS."
        );
        return Err(config::ConfigError::new(&msg).into());
    }

    let dialogue_channels = config.dialogue_channels();
    if dialogue_channels.is_empty() && channels != 2 {
        let msg = format!(
            "A {channels} channel night mode needs 'dialogue_channels' to be set. It can only derive a phantom centre from a stereo pair."
        );
        return Err(config::ConfigError::new(&msg).into());
    }
    for ch in dialogue_channels.iter() {
        if *ch >= channels {
            let msg = format!(
                "Invalid dialogue channel: {}, max is: {}.",
                *ch,
                channels - 1
            );
            return Err(config::ConfigError::new(&msg).into());
        }
    }
    for ch in config.monitor_channels().iter() {
        if *ch >= channels {
            let msg = format!(
                "Invalid monitor channel: {}, max is: {}.",
                *ch,
                channels - 1
            );
            return Err(config::ConfigError::new(&msg).into());
        }
    }
    for ch in config.process_channels().iter() {
        if *ch >= channels {
            let msg = format!(
                "Invalid channel to process: {}, max is: {}.",
                *ch,
                channels - 1
            );
            return Err(config::ConfigError::new(&msg).into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_params() -> config::NightModeParameters {
        config::NightModeParameters {
            channels: 2,
            amount: None,
            max_attenuation: None,
            bass_reduction: None,
            dialogue_channels: None,
            headroom: None,
            ratio: None,
            transient_softening: None,
            dialogue_protection: None,
            presence_gain: None,
            presence_frequency: None,
            presence_q: None,
            attack: None,
            release: None,
            reference_level: None,
            reference_slew: None,
            modulation_weight: None,
            bass_frequency: None,
            silence_threshold: None,
            ceiling: None,
            monitor_channels: None,
            process_channels: None,
        }
    }

    #[test]
    fn defaults_are_valid() {
        assert!(validate_night_mode(48000, &default_params()).is_ok());
    }

    #[test]
    fn validate_rejects_bad_configs() {
        type Mutation = Box<dyn Fn(&mut config::NightModeParameters)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("attack zero", Box::new(|p| p.attack = Some(0.0))),
            (
                "release shorter than attack",
                Box::new(|p| {
                    p.attack = Some(0.5);
                    p.release = Some(0.1)
                }),
            ),
            ("ratio below one", Box::new(|p| p.ratio = Some(0.5))),
            ("amount above 100", Box::new(|p| p.amount = Some(150.0))),
            (
                "negative max attenuation",
                Box::new(|p| p.max_attenuation = Some(-3.0)),
            ),
            (
                "bass reduction too large",
                Box::new(|p| p.bass_reduction = Some(20.0)),
            ),
            (
                "presence gain too large",
                Box::new(|p| p.presence_gain = Some(12.0)),
            ),
            ("presence q too low", Box::new(|p| p.presence_q = Some(0.1))),
            (
                "presence frequency above nyquist",
                Box::new(|p| p.presence_frequency = Some(30000.0)),
            ),
            (
                "bass frequency above nyquist",
                Box::new(|p| p.bass_frequency = Some(30000.0)),
            ),
            ("ceiling above zero", Box::new(|p| p.ceiling = Some(3.0))),
            (
                "modulation weight above one",
                Box::new(|p| p.modulation_weight = Some(2.0)),
            ),
            (
                "reference level outside window",
                Box::new(|p| p.reference_level = Some(-5.0)),
            ),
            (
                "dialogue channel out of range",
                Box::new(|p| p.dialogue_channels = Some(vec![9])),
            ),
            (
                "monitor channel out of range",
                Box::new(|p| p.monitor_channels = Some(vec![7])),
            ),
            (
                "multichannel without dialogue channels",
                Box::new(|p| p.channels = 6),
            ),
        ];
        for (name, mutate) in cases {
            let mut params = default_params();
            mutate(&mut params);
            assert!(
                validate_night_mode(48000, &params).is_err(),
                "config should have been rejected: {name}"
            );
        }
    }

    #[test]
    fn multichannel_is_valid_with_dialogue_channels() {
        let mut params = default_params();
        params.channels = 6;
        params.dialogue_channels = Some(vec![2]);
        assert!(validate_night_mode(48000, &params).is_ok());
    }

    const FS: usize = 48000;
    const CHUNK: usize = 1024;

    fn pinned_params(level: PrcFmt) -> config::NightModeParameters {
        let mut params = default_params();
        params.reference_level = Some(level);
        params
    }

    fn processor(params: config::NightModeParameters) -> NightMode {
        NightMode::from_config("test", params, FS, CHUNK)
    }

    /// Sine at a given peak amplitude, as an identical pair of channels.
    fn stereo_sine(freq: PrcFmt, amplitude: PrcFmt, frames: usize) -> Vec<Vec<PrcFmt>> {
        let step = 2.0 * std::f64::consts::PI as PrcFmt * freq / FS as PrcFmt;
        let wave: Vec<PrcFmt> = (0..frames)
            .map(|n| amplitude * (step * n as PrcFmt).sin())
            .collect();
        vec![wave.clone(), wave]
    }

    fn chunk_from(waveforms: Vec<Vec<PrcFmt>>) -> AudioChunk {
        let frames = waveforms[0].len();
        AudioChunk::new(waveforms, 0.0, 0.0, frames, frames)
    }

    fn rms_db(waveform: &[PrcFmt]) -> PrcFmt {
        let sum: PrcFmt = waveform.iter().map(|v| v * v).sum();
        10.0 * (sum / waveform.len() as PrcFmt + POWER_FLOOR).log10()
    }

    /// Push the same signal through repeatedly and return the gain, in dB, that
    /// the processor settled on by the last chunk.
    fn settled_gain_db(
        processor: &mut NightMode,
        waveforms: Vec<Vec<PrcFmt>>,
        chunks: usize,
    ) -> PrcFmt {
        let input_db = rms_db(&waveforms[0]);
        let mut output_db = input_db;
        for _ in 0..chunks {
            let mut chunk = chunk_from(waveforms.clone());
            processor.process_chunk(&mut chunk).unwrap();
            output_db = rms_db(&chunk.waveforms[0]);
        }
        output_db - input_db
    }

    #[test]
    fn amount_zero_is_transparent_even_for_hot_input() {
        // Full scale, so the signal sits above the default -1 dBFS ceiling. Material
        // mastered this hot is normal, and inserting a disabled processor must not
        // soft clip it, or `amount: 0` is useless as an A/B reference.
        let mut params = pinned_params(-27.0);
        params.amount = Some(0.0);
        let mut processor = processor(params);
        let waveforms = stereo_sine(1000.0, 1.0, CHUNK);
        let mut chunk = chunk_from(waveforms.clone());
        processor.process_chunk(&mut chunk).unwrap();
        for (processed, original) in chunk.waveforms[0].iter().zip(waveforms[0].iter()) {
            assert!(
                (processed - original).abs() < 1e-12,
                "expected untouched samples, got {processed} instead of {original}"
            );
        }
    }

    #[test]
    fn amount_zero_is_transparent() {
        let mut params = pinned_params(-27.0);
        params.amount = Some(0.0);
        let mut processor = processor(params);
        let waveforms = stereo_sine(1000.0, 0.7, CHUNK);
        let mut chunk = chunk_from(waveforms.clone());
        processor.process_chunk(&mut chunk).unwrap();
        for (processed, original) in chunk.waveforms[0].iter().zip(waveforms[0].iter()) {
            assert!(
                (processed - original).abs() < 1e-9,
                "expected untouched samples, got {processed} instead of {original}"
            );
        }
    }

    #[test]
    fn quiet_dialogue_is_untouched() {
        // -30 dBFS sits below the -27 reference plus 6 dB headroom.
        let mut processor = processor(pinned_params(-27.0));
        let waveforms = stereo_sine(
            1000.0,
            db_to_linear(-30.0) * 2.0_f64.sqrt() as PrcFmt,
            CHUNK,
        );
        let gain = settled_gain_db(&mut processor, waveforms, 100);
        assert!(gain.abs() < 0.3, "expected no reduction, got {gain} dB");
    }

    #[test]
    fn loud_content_is_reduced() {
        // Protection off, so this measures the leveler rather than the detector.
        // A centred tone in the speech band is, correctly, treated as dialogue.
        let mut params = pinned_params(-27.0);
        params.dialogue_protection = Some(0.0);
        let mut processor = processor(params);
        let waveforms = stereo_sine(1000.0, db_to_linear(-6.0), CHUNK);
        let gain = settled_gain_db(&mut processor, waveforms, 300);
        // Clearly reduced, but not pinned against max_attenuation.
        assert!(
            gain < -5.0 && gain > -25.0,
            "expected clear but bounded reduction, got {gain} dB"
        );
    }

    #[test]
    fn attenuation_never_exceeds_cap() {
        let mut params = pinned_params(-27.0);
        params.max_attenuation = Some(6.0);
        let mut processor = processor(params);
        let waveforms = stereo_sine(1000.0, 1.0, CHUNK);
        let gain = settled_gain_db(&mut processor, waveforms, 300);
        assert!(gain > -6.5, "cap of 6 dB was exceeded: {gain} dB");
    }

    #[test]
    fn reduction_scales_with_amount() {
        let mut gains = Vec::new();
        for amount in [25.0, 50.0, 100.0] {
            let mut params = pinned_params(-27.0);
            params.amount = Some(amount);
            // Keep the cap and the detector out of the way so the scaling itself
            // is what gets measured.
            params.max_attenuation = Some(40.0);
            params.dialogue_protection = Some(0.0);
            let mut processor = processor(params);
            let waveforms = stereo_sine(1000.0, db_to_linear(-6.0), CHUNK);
            gains.push(settled_gain_db(&mut processor, waveforms, 300));
        }
        assert!(
            gains[0] > gains[1] && gains[1] > gains[2],
            "reduction should grow with amount, got {gains:?}"
        );
        // Scaling happens in the dB domain, so half the amount is half the dB.
        let half = gains[2] * 0.5;
        assert!(
            (gains[1] - half).abs() < 0.5,
            "50% should be half as many dB as 100%: {} vs {}",
            gains[1],
            half
        );
    }

    /// Reduction, in dB, over the first `window` samples of a burst that starts
    /// from silence. The processor starts cold, as it would at a scene change.
    fn onset_gain_db(softening: PrcFmt, window: usize) -> PrcFmt {
        // Reference low enough that the burst sits well above the fast stage's
        // threshold of reference + 18 dB, which is what it is there to catch.
        let mut params = pinned_params(-40.0);
        params.transient_softening = Some(softening);
        params.max_attenuation = Some(40.0);
        let mut processor = processor(params);

        // A chunk of silence first, so the followers sit at the noise floor.
        let mut quiet = chunk_from(vec![vec![0.0; CHUNK], vec![0.0; CHUNK]]);
        processor.process_chunk(&mut quiet).unwrap();

        let waveforms = stereo_sine(1000.0, db_to_linear(-3.0), CHUNK);
        let input_db = rms_db(&waveforms[0][..window]);
        let mut chunk = chunk_from(waveforms);
        processor.process_chunk(&mut chunk).unwrap();
        rms_db(&chunk.waveforms[0][..window]) - input_db
    }

    #[test]
    fn fast_stage_catches_the_leading_edge() {
        // 10 ms at 48 kHz. The slow stage has a 150 ms attack, so on its own it
        // has barely started moving this early into the burst.
        let window = 480;
        let without = onset_gain_db(0.0, window);
        let with = onset_gain_db(100.0, window);
        assert!(
            without - with > 2.0,
            "fast stage should take at least 2 dB more off the onset, got {without} dB without vs {with} dB with"
        );
    }

    #[test]
    fn transient_softening_is_monotone() {
        let window = 480;
        let gains: Vec<PrcFmt> = [0.0, 50.0, 100.0]
            .iter()
            .map(|s| onset_gain_db(*s, window))
            .collect();
        assert!(
            gains[0] > gains[1] && gains[1] > gains[2],
            "more softening should mean more reduction, got {gains:?}"
        );
    }

    /// Render a signal through a processor built for a given chunksize, returning
    /// the processed samples. The signal is split into chunks of exactly that size.
    fn render(
        params: config::NightModeParameters,
        chunksize: usize,
        signal: &[Vec<PrcFmt>],
    ) -> Vec<PrcFmt> {
        let mut processor = NightMode::from_config("test", params, FS, chunksize);
        let mut out = Vec::with_capacity(signal[0].len());
        let mut start = 0;
        while start + chunksize <= signal[0].len() {
            let waveforms: Vec<Vec<PrcFmt>> = signal
                .iter()
                .map(|ch| ch[start..start + chunksize].to_vec())
                .collect();
            let mut chunk = chunk_from(waveforms);
            processor.process_chunk(&mut chunk).unwrap();
            out.extend_from_slice(&chunk.waveforms[0]);
            start += chunksize;
        }
        out
    }

    /// A burst that starts quiet and jumps loud, long enough to exercise both stages.
    fn scene(frames: usize) -> Vec<Vec<PrcFmt>> {
        let step = 2.0 * std::f64::consts::PI as PrcFmt * 1000.0 / FS as PrcFmt;
        let wave: Vec<PrcFmt> = (0..frames)
            .map(|n| {
                let amplitude = if n < frames / 2 {
                    db_to_linear(-30.0)
                } else {
                    db_to_linear(-3.0)
                };
                amplitude * (step * n as PrcFmt).sin()
            })
            .collect();
        vec![wave.clone(), wave]
    }

    #[test]
    fn hops_are_counted_independently_of_chunksize() {
        let hop = FS / HOP_RATE_HZ;
        for chunksize in [256, 1000, 1024, 4096] {
            let chunks = 20;
            let frames = chunksize * chunks;
            let signal = scene(frames);
            let mut processor = NightMode::from_config("test", pinned_params(-27.0), FS, chunksize);
            let mut start = 0;
            while start + chunksize <= frames {
                let waveforms: Vec<Vec<PrcFmt>> = signal
                    .iter()
                    .map(|ch| ch[start..start + chunksize].to_vec())
                    .collect();
                let mut chunk = chunk_from(waveforms);
                processor.process_chunk(&mut chunk).unwrap();
                start += chunksize;
            }
            assert_eq!(
                processor.hops_measured,
                frames / hop,
                "wrong hop count at chunksize {chunksize}"
            );
        }
    }

    #[test]
    fn behaviour_is_independent_of_chunksize() {
        // Two seconds, a length divisible by every chunksize under test.
        let frames = 4096 * 24;
        let signal = scene(frames);
        let reference = render(pinned_params(-27.0), 1024, &signal);
        for chunksize in [256, 4096] {
            let other = render(pinned_params(-27.0), chunksize, &signal);
            let common = reference.len().min(other.len());
            // Compare per 100 ms window, which is how the difference would be heard.
            let window = FS / 10;
            let mut windows = 0;
            for start in (0..common - window).step_by(window) {
                let a = rms_db(&reference[start..start + window]);
                let b = rms_db(&other[start..start + window]);
                assert!(
                    (a - b).abs() < 0.5,
                    "chunksize {chunksize} diverged by {} dB at sample {start}",
                    (a - b).abs()
                );
                windows += 1;
            }
            assert!(windows > 10, "test did not actually compare anything");
        }
    }

    /// Settled reduction for a given stereo signal, with the detector on and off.
    /// Returns (protected, unprotected) in dB. Runs long enough for the one second
    /// confidence-to-gain smoother to converge.
    fn protection_pair(waveforms: Vec<Vec<PrcFmt>>) -> (PrcFmt, PrcFmt) {
        let chunks = 400;
        let mut protected_params = pinned_params(-27.0);
        protected_params.max_attenuation = Some(40.0);
        // These signals are steady, so the syllabic cue can never fire. Weight it to
        // zero to test the centredness and band cues on their own; the modulation cue
        // has its own tests, which use a real timeline.
        protected_params.modulation_weight = Some(0.0);
        let mut unprotected_params = protected_params.clone();
        unprotected_params.dialogue_protection = Some(0.0);

        let protected =
            settled_gain_db(&mut processor(protected_params), waveforms.clone(), chunks);
        let unprotected = settled_gain_db(&mut processor(unprotected_params), waveforms, chunks);
        (protected, unprotected)
    }

    #[test]
    fn loud_centred_speech_band_content_is_protected() {
        // Centred, inside the speech band, and loud enough to be reduced otherwise.
        let waveforms = stereo_sine(1000.0, db_to_linear(-12.0), CHUNK);
        let (protected, unprotected) = protection_pair(waveforms);
        assert!(
            unprotected < -2.0,
            "without protection this should be reduced, got {unprotected} dB"
        );
        assert!(
            protected > unprotected + 2.0,
            "protection should give back at least 2 dB, got {protected} dB vs {unprotected} dB"
        );
    }

    #[test]
    fn implausibly_loud_content_is_not_protected() {
        // Centred and inside the speech band, so the detector is as confident as it
        // ever gets, but 30 dB above the dialogue reference. Nothing that loud is
        // dialogue, and it must be reduced regardless of confidence.
        let waveforms = stereo_sine(1000.0, db_to_linear(-6.0), CHUNK);
        let mut protected_params = pinned_params(-36.0);
        protected_params.max_attenuation = Some(40.0);
        let mut unprotected_params = protected_params.clone();
        unprotected_params.dialogue_protection = Some(0.0);

        let protected = settled_gain_db(&mut processor(protected_params), waveforms.clone(), 400);
        let unprotected = settled_gain_db(&mut processor(unprotected_params), waveforms, 400);
        assert!(
            (protected - unprotected).abs() < 1.0,
            "content far above the dialogue reference should not be protected, got {protected} dB vs {unprotected} dB"
        );
    }

    #[test]
    fn decorrelated_content_is_not_protected() {
        // Two unrelated tones, so correlation is near zero.
        let left = stereo_sine(997.0, db_to_linear(-12.0), CHUNK).remove(0);
        let right: Vec<PrcFmt> = {
            let step = 2.0 * std::f64::consts::PI as PrcFmt * 1493.0 / FS as PrcFmt;
            (0..CHUNK)
                .map(|n| db_to_linear(-12.0) * (step * n as PrcFmt).sin())
                .collect()
        };
        let (protected, unprotected) = protection_pair(vec![left, right]);
        assert!(
            (protected - unprotected).abs() < 1.0,
            "decorrelated content should not be protected, got {protected} dB vs {unprotected} dB"
        );
    }

    #[test]
    fn hard_panned_content_is_not_protected() {
        // The case a mid/side energy ratio gets wrong: a hard panned source produces
        // a large mid and a large side, so the ratio reads it as partly centred.
        // Correlation reads it correctly as not centred at all.
        let left = stereo_sine(1000.0, db_to_linear(-12.0), CHUNK).remove(0);
        let (protected, unprotected) = protection_pair(vec![left, vec![0.0; CHUNK]]);
        assert!(
            (protected - unprotected).abs() < 1.0,
            "hard panned content should not be protected, got {protected} dB vs {unprotected} dB"
        );
    }

    #[test]
    fn out_of_band_content_is_not_protected() {
        // Centred but well below the speech band, as an explosion's rumble would be.
        let waveforms = stereo_sine(60.0, db_to_linear(-12.0), CHUNK);
        let (protected, unprotected) = protection_pair(waveforms);
        assert!(
            (protected - unprotected).abs() < 1.0,
            "low frequency content should not be protected, got {protected} dB vs {unprotected} dB"
        );
    }

    /// Centred tone with an optional syllabic envelope. mod_hz of zero is steady.
    fn centred_tone(mod_hz: PrcFmt, amplitude: PrcFmt, frames: usize) -> Vec<Vec<PrcFmt>> {
        let step = 2.0 * std::f64::consts::PI as PrcFmt * 1000.0 / FS as PrcFmt;
        let mod_step = 2.0 * std::f64::consts::PI as PrcFmt * mod_hz / FS as PrcFmt;
        let wave: Vec<PrcFmt> = (0..frames)
            .map(|n| {
                let envelope = if mod_hz > 0.0 {
                    0.4 + 0.6 * (mod_step * n as PrcFmt).sin().abs()
                } else {
                    1.0
                };
                amplitude * envelope * (step * n as PrcFmt).sin()
            })
            .collect();
        vec![wave.clone(), wave]
    }

    /// Reduction over the final second of a continuously rendered signal, with the
    /// detector on and off. Needs a real timeline, since anything measured from one
    /// repeated chunk cannot carry a syllabic envelope.
    fn protection_pair_rendered(
        signal: Vec<Vec<PrcFmt>>,
        modulation_weight: PrcFmt,
    ) -> (PrcFmt, PrcFmt) {
        let mut protected_params = pinned_params(-27.0);
        protected_params.max_attenuation = Some(40.0);
        protected_params.modulation_weight = Some(modulation_weight);
        let mut unprotected_params = protected_params.clone();
        unprotected_params.dialogue_protection = Some(0.0);

        let protected = render(protected_params, CHUNK, &signal);
        let unprotected = render(unprotected_params, CHUNK, &signal);
        let tail = FS;
        let from = protected.len() - tail;
        let input_db = rms_db(&signal[0][from..from + tail]);
        (
            rms_db(&protected[from..from + tail]) - input_db,
            rms_db(&unprotected[from..from + tail]) - input_db,
        )
    }

    #[test]
    fn syllabic_content_passes_the_modulation_test() {
        // Eight seconds, enough for the modulation estimate and the one second
        // confidence smoother to settle.
        let signal = centred_tone(4.0, db_to_linear(-12.0), FS * 8);
        let (protected, unprotected) = protection_pair_rendered(signal, 1.0);
        assert!(
            unprotected < -2.0,
            "signal should be reduced without protection, got {unprotected} dB"
        );
        assert!(
            protected > unprotected + 1.5,
            "syllabic content should be protected even when modulation is the only cue, got {protected} dB vs {unprotected} dB"
        );
    }

    #[test]
    fn steady_tone_fails_the_modulation_test() {
        // Centred and in band, but with no syllabic movement at all. With the
        // modulation term weighted fully, this must not be mistaken for speech.
        let signal = centred_tone(0.0, db_to_linear(-12.0), FS * 8);
        let (protected, unprotected) = protection_pair_rendered(signal, 1.0);
        assert!(
            (protected - unprotected).abs() < 1.0,
            "a steady tone should not be protected at full modulation weight, got {protected} dB vs {unprotected} dB"
        );
    }

    #[test]
    fn modulation_weight_zero_ignores_modulation() {
        // The same steady tone, with the modulation term switched off, is protected
        // again purely on being centred and in band.
        let signal = centred_tone(0.0, db_to_linear(-12.0), FS * 4);
        let (protected, unprotected) = protection_pair_rendered(signal, 0.0);
        assert!(
            protected > unprotected + 1.5,
            "with modulation weight zero the steady tone should be protected, got {protected} dB vs {unprotected} dB"
        );
    }

    /// Render a signal through an adaptive processor and return it, so the reference
    /// can be inspected afterwards.
    fn render_adaptive(
        signal: &[Vec<PrcFmt>],
        mutate: impl Fn(&mut config::NightModeParameters),
    ) -> NightMode {
        let mut params = default_params();
        mutate(&mut params);
        let mut processor = processor(params);
        let mut start = 0;
        while start + CHUNK <= signal[0].len() {
            let waveforms: Vec<Vec<PrcFmt>> = signal
                .iter()
                .map(|ch| ch[start..start + CHUNK].to_vec())
                .collect();
            let mut chunk = chunk_from(waveforms);
            processor.process_chunk(&mut chunk).unwrap();
            start += CHUNK;
        }
        processor
    }

    #[test]
    fn reference_tracks_dialogue_within_the_slew_limit() {
        let seconds = 8;
        // Dialogue-like, and quieter than the -27 dB the reference starts at, so the
        // tracker has somewhere to move.
        let signal = centred_tone(4.0, db_to_linear(-36.0), FS * seconds);
        let processor = render_adaptive(&signal, |_| {});
        let moved = processor.reference_db - INITIAL_REFERENCE_DB;
        assert!(
            moved < -0.5,
            "reference should have moved down toward the dialogue level, moved {moved} dB"
        );
        let allowed = 0.25 * seconds as PrcFmt + 0.1;
        assert!(
            moved.abs() <= allowed,
            "reference moved {} dB, more than the {allowed} dB the slew limit allows",
            moved.abs()
        );
    }

    #[test]
    fn reference_rejects_levels_outside_the_window() {
        // Centred and in band so confidence is high, with the modulation term
        // switched off so a steady tone still passes, and far louder than any real
        // dialogue. Levels are judged per hop, so the signal is kept steady on
        // purpose: a modulated one would dip into the window during its troughs.
        let signal = centred_tone(0.0, db_to_linear(-5.0), FS * 4);
        let processor = render_adaptive(&signal, |p| p.modulation_weight = Some(0.0));
        assert!(
            (processor.reference_db - INITIAL_REFERENCE_DB).abs() < 1e-9,
            "reference should not have moved, sits at {}",
            processor.reference_db
        );
    }

    #[test]
    fn pinned_reference_never_moves() {
        let signal = centred_tone(4.0, db_to_linear(-36.0), FS * 4);
        let processor = render_adaptive(&signal, |p| p.reference_level = Some(-30.0));
        assert!((processor.reference_db - (-30.0)).abs() < 1e-9);
    }

    #[test]
    fn silence_freezes_the_detector() {
        let dialogue = centred_tone(4.0, db_to_linear(-36.0), FS * 4);
        let mut processor = render_adaptive(&dialogue, |_| {});

        // One chunk of silence first, to flush the hop that straddles the boundary
        // and still legitimately contains dialogue samples.
        let mut silence = chunk_from(vec![vec![0.0; CHUNK], vec![0.0; CHUNK]]);
        processor.process_chunk(&mut silence).unwrap();
        let reference_before = processor.reference_db;
        let confidence_before = processor.conf_hop;

        for _ in 0..200 {
            processor.process_chunk(&mut silence).unwrap();
        }
        assert!(
            (processor.reference_db - reference_before).abs() < 1e-9,
            "silence moved the reference from {reference_before} to {}",
            processor.reference_db
        );
        assert!(
            (processor.conf_hop - confidence_before).abs() < 1e-9,
            "silence moved confidence from {confidence_before} to {}",
            processor.conf_hop
        );
    }

    #[test]
    fn reference_ignores_content_that_is_not_dialogue() {
        // Decorrelated and out of band, so confidence never reaches the gate.
        let step = 2.0 * std::f64::consts::PI as PrcFmt * 60.0 / FS as PrcFmt;
        let left: Vec<PrcFmt> = (0..FS * 4)
            .map(|n| db_to_linear(-36.0) * (step * n as PrcFmt).sin())
            .collect();
        let right: Vec<PrcFmt> = left.iter().map(|v| -v).collect();
        let processor = render_adaptive(&[left, right], |_| {});
        assert!(
            (processor.reference_db - INITIAL_REFERENCE_DB).abs() < 1e-9,
            "reference should have stayed put, sits at {}",
            processor.reference_db
        );
    }

    /// Magnitude of one frequency component, by correlating against it.
    fn tone_magnitude(signal: &[PrcFmt], freq: PrcFmt) -> PrcFmt {
        let step = 2.0 * std::f64::consts::PI as PrcFmt * freq / FS as PrcFmt;
        let mut real = 0.0;
        let mut imag = 0.0;
        for (n, val) in signal.iter().enumerate() {
            let phase = step * n as PrcFmt;
            real += *val * phase.cos();
            imag += *val * phase.sin();
        }
        let scale = 2.0 / signal.len() as PrcFmt;
        ((real * real + imag * imag).sqrt()) * scale
    }

    /// Loud two-tone signal: a midrange component to drive the leveler, plus bass for
    /// the shelf to act on. Bass alone would not trigger anything, because the
    /// sidechain is highpassed at 60 Hz on purpose.
    fn bass_and_midrange(frames: usize) -> Vec<Vec<PrcFmt>> {
        let low = 2.0 * std::f64::consts::PI as PrcFmt * 50.0 / FS as PrcFmt;
        let high = 2.0 * std::f64::consts::PI as PrcFmt * 1000.0 / FS as PrcFmt;
        let amplitude = db_to_linear(-9.0);
        let wave: Vec<PrcFmt> = (0..frames)
            .map(|n| amplitude * (low * n as PrcFmt).sin() + amplitude * (high * n as PrcFmt).sin())
            .collect();
        vec![wave.clone(), wave]
    }

    #[test]
    fn bass_is_reduced_more_than_midrange() {
        let signal = bass_and_midrange(FS * 4);
        let mut ratios = Vec::new();
        for reduction in [0.0, 8.0] {
            let mut params = pinned_params(-27.0);
            params.bass_reduction = Some(reduction);
            // Protection off: the tone pair is centred, so the detector would
            // otherwise treat it as dialogue and back the leveler off.
            params.dialogue_protection = Some(0.0);
            let out = render(params, CHUNK, &signal);
            // Measure over the settled tail only.
            let tail = &out[out.len() - FS..];
            let bass = tone_magnitude(tail, 50.0);
            let mid = tone_magnitude(tail, 1000.0);
            ratios.push(20.0 * (bass / mid).log10());
        }
        let difference = ratios[0] - ratios[1];
        assert!(
            (2.0..=9.0).contains(&difference),
            "8 dB of bass reduction should tilt bass against midrange by a few dB, got {difference} dB (ratios {ratios:?})"
        );
    }

    #[test]
    fn bass_shelf_is_inert_when_disabled() {
        let signal = bass_and_midrange(FS * 2);
        let mut params = pinned_params(-27.0);
        params.bass_reduction = Some(0.0);
        params.dialogue_protection = Some(0.0);
        let out = render(params.clone(), CHUNK, &signal);
        let tail = &out[out.len() - FS..];
        let bass_in = tone_magnitude(&signal[0][signal[0].len() - FS..], 50.0);
        let mid_in = tone_magnitude(&signal[0][signal[0].len() - FS..], 1000.0);
        let bass_out = tone_magnitude(tail, 50.0);
        let mid_out = tone_magnitude(tail, 1000.0);
        // Both tones must be scaled by the same gain, leaving their ratio untouched.
        let tilt = 20.0 * ((bass_out / mid_out) / (bass_in / mid_in)).log10();
        assert!(
            tilt.abs() < 0.3,
            "with bass reduction off the spectrum should be untouched, tilted {tilt} dB"
        );
    }

    #[test]
    fn quiet_scenes_get_no_bass_shelf() {
        // Below threshold there is no gain reduction, so the shelf depth must be zero
        // and the tonal balance must be identical to the input.
        let signal = {
            let low = 2.0 * std::f64::consts::PI as PrcFmt * 50.0 / FS as PrcFmt;
            let high = 2.0 * std::f64::consts::PI as PrcFmt * 1000.0 / FS as PrcFmt;
            let amplitude = db_to_linear(-40.0);
            let wave: Vec<PrcFmt> = (0..FS * 2)
                .map(|n| {
                    amplitude * (low * n as PrcFmt).sin() + amplitude * (high * n as PrcFmt).sin()
                })
                .collect();
            vec![wave.clone(), wave]
        };
        let mut params = pinned_params(-27.0);
        params.bass_reduction = Some(8.0);
        let out = render(params, CHUNK, &signal);
        let tail = &out[out.len() - FS..];
        let reference = &signal[0][signal[0].len() - FS..];
        let tilt = 20.0
            * ((tone_magnitude(tail, 50.0) / tone_magnitude(tail, 1000.0))
                / (tone_magnitude(reference, 50.0) / tone_magnitude(reference, 1000.0)))
            .log10();
        assert!(
            tilt.abs() < 0.1,
            "a quiet scene should get no tonal change, tilted {tilt} dB"
        );
    }

    /// Tilt at the presence frequency against a reference tone well above it, in dB,
    /// relative to the same measurement on the input.
    fn presence_tilt(gain: PrcFmt, waveforms: Vec<Vec<PrcFmt>>) -> PrcFmt {
        let mut params = pinned_params(-27.0);
        params.presence_gain = Some(gain);
        // Steady tones, so the modulation term would otherwise veto the detector.
        params.modulation_weight = Some(0.0);
        let out = render(params, CHUNK, &waveforms);
        let tail = &out[out.len() - FS..];
        let reference = &waveforms[0][waveforms[0].len() - FS..];
        20.0 * ((tone_magnitude(tail, 2500.0) / tone_magnitude(tail, 9000.0))
            / (tone_magnitude(reference, 2500.0) / tone_magnitude(reference, 9000.0)))
        .log10()
    }

    /// A quiet pair of tones, one at the presence frequency and one above it. Quiet so
    /// that no gain reduction is involved and the tone shaping is what gets measured.
    fn presence_probe(correlated: bool) -> Vec<Vec<PrcFmt>> {
        let centre = 2.0 * std::f64::consts::PI as PrcFmt * 2500.0 / FS as PrcFmt;
        let above = 2.0 * std::f64::consts::PI as PrcFmt * 9000.0 / FS as PrcFmt;
        let amplitude = db_to_linear(-40.0);
        let wave: Vec<PrcFmt> = (0..FS * 4)
            .map(|n| {
                amplitude * (centre * n as PrcFmt).sin() + amplitude * (above * n as PrcFmt).sin()
            })
            .collect();
        if correlated {
            vec![wave.clone(), wave]
        } else {
            let inverted: Vec<PrcFmt> = wave.iter().map(|v| -v).collect();
            vec![wave, inverted]
        }
    }

    #[test]
    fn presence_lift_is_inert_when_zero() {
        let tilt = presence_tilt(0.0, presence_probe(true));
        assert!(
            tilt.abs() < 0.1,
            "with no presence gain the spectrum should be untouched, tilted {tilt} dB"
        );
    }

    #[test]
    fn presence_lift_raises_the_speech_band_on_dialogue() {
        let tilt = presence_tilt(6.0, presence_probe(true));
        assert!(
            tilt > 1.0,
            "presence lift should raise the centre frequency on centred content, tilted {tilt} dB"
        );
    }

    #[test]
    fn presence_lift_does_not_touch_non_dialogue() {
        // Out of phase, so correlation is negative and confidence stays at zero.
        let tilt = presence_tilt(6.0, presence_probe(false));
        assert!(
            tilt.abs() < 0.1,
            "presence lift should stay out of the way on non-dialogue, tilted {tilt} dB"
        );
    }

    fn as_processor_config(params: config::NightModeParameters) -> config::Processor {
        config::Processor::NightMode {
            description: None,
            parameters: Box::new(params),
        }
    }

    #[test]
    fn update_parameters_preserves_state() {
        let signal = centred_tone(4.0, db_to_linear(-36.0), FS * 4);
        let mut processor = render_adaptive(&signal, |_| {});

        let reference = processor.reference_db;
        let envelope = processor.env_slow_power;
        let confidence = processor.conf_smooth;
        let hops = processor.hops_measured;

        let mut params = default_params();
        params.amount = Some(50.0);
        processor.update_parameters(as_processor_config(params));

        assert!((processor.reference_db - reference).abs() < 1e-12);
        assert!((processor.env_slow_power - envelope).abs() < 1e-12);
        assert!((processor.conf_smooth - confidence).abs() < 1e-12);
        assert_eq!(processor.hops_measured, hops);
        assert!(
            (processor.amount - 0.5).abs() < 1e-12,
            "the change itself should apply"
        );
    }

    #[test]
    fn a_no_op_update_does_not_disturb_the_audio() {
        // Two processors with identical history. One is reconfigured with the values
        // it already had, which must be inaudible: if any filter state or follower
        // were rebuilt, the next chunk would differ.
        let signal = centred_tone(4.0, db_to_linear(-12.0), FS * 2);
        let mut untouched = render_adaptive(&signal, |_| {});
        let mut updated = render_adaptive(&signal, |_| {});
        updated.update_parameters(as_processor_config(default_params()));

        let next = centred_tone(4.0, db_to_linear(-12.0), CHUNK);
        let mut chunk_a = chunk_from(next.clone());
        let mut chunk_b = chunk_from(next);
        untouched.process_chunk(&mut chunk_a).unwrap();
        updated.process_chunk(&mut chunk_b).unwrap();

        for (a, b) in chunk_a.waveforms[0].iter().zip(chunk_b.waveforms[0].iter()) {
            assert!(
                (a - b).abs() < 1e-9,
                "a no-op reconfiguration changed the output: {a} against {b}"
            );
        }
    }

    #[test]
    fn retuning_the_bass_corner_does_not_rebuild_the_filters() {
        // Changing only the corner frequency swaps coefficients in place, so the
        // number of filters stays the same and their history is not discarded.
        let signal = bass_and_midrange(FS);
        let mut processor = render_adaptive(&signal, |p| p.bass_reduction = Some(6.0));
        let count = processor.bass_lowpass.len();

        let mut params = default_params();
        params.bass_reduction = Some(6.0);
        params.bass_frequency = Some(90.0);
        processor.update_parameters(as_processor_config(params));

        assert_eq!(processor.bass_lowpass.len(), count);
        assert!((processor.bass_frequency - 90.0).abs() < 1e-12);
        // And the audio stays finite and continuous through the change.
        let mut chunk = chunk_from(bass_and_midrange(CHUNK));
        processor.process_chunk(&mut chunk).unwrap();
        assert!(chunk.waveforms[0].iter().all(|v| v.is_finite()));
    }

    #[test]
    fn dialogue_recovers_quickly_after_a_loud_scene() {
        // Two seconds of loud content, then quiet dialogue. The ordinary release is
        // 1.5 s, which on its own leaves speech ducked for several seconds after an
        // explosion. A drop this large is a scene change, and must release faster.
        let loud = 2 * FS;
        let quiet = 3 * FS;
        let step = 2.0 * std::f64::consts::PI as PrcFmt * 1000.0 / FS as PrcFmt;
        let wave: Vec<PrcFmt> = (0..loud + quiet)
            .map(|n| {
                let amplitude = if n < loud {
                    db_to_linear(-6.0)
                } else {
                    db_to_linear(-36.0)
                };
                amplitude * (step * n as PrcFmt).sin()
            })
            .collect();
        let signal = vec![wave.clone(), wave];

        let mut params = pinned_params(-27.0);
        params.dialogue_protection = Some(0.0);
        let out = render(params, CHUNK, &signal);

        // One second after the change, the processor must be out of the way.
        let from = loud + FS;
        let input_db = rms_db(&signal[0][from..from + FS]);
        let output_db = rms_db(&out[from..from + FS]);
        assert!(
            (output_db - input_db).abs() < 0.5,
            "dialogue should be released within a second of the loud scene ending, still reduced by {} dB",
            input_db - output_db
        );

        // And the loud section itself must still be reduced.
        let loud_db = rms_db(&signal[0][FS..loud]) - rms_db(&out[FS..loud]);
        assert!(
            loud_db > 5.0,
            "the loud section should still be levelled, only {loud_db} dB"
        );
    }

    #[test]
    fn silence_stays_silent_and_finite() {
        let mut processor = processor(pinned_params(-27.0));
        let mut chunk = chunk_from(vec![vec![0.0; CHUNK], vec![0.0; CHUNK]]);
        for _ in 0..100 {
            processor.process_chunk(&mut chunk).unwrap();
        }
        assert!(chunk.waveforms[0].iter().all(|v| *v == 0.0));
        assert!(chunk.waveforms.iter().flatten().all(|v| v.is_finite()));
    }

    #[test]
    fn ceiling_is_enforced() {
        // Active, but with the cap at zero so no reduction is applied. That isolates
        // the ceiling: it must still catch a signal louder than it.
        let mut params = pinned_params(-27.0);
        params.max_attenuation = Some(0.0);
        params.ceiling = Some(-6.0);
        let mut processor = processor(params);
        let waveforms = stereo_sine(1000.0, 1.0, CHUNK);
        let mut chunk = chunk_from(waveforms);
        processor.process_chunk(&mut chunk).unwrap();
        let limit = db_to_linear(-6.0);
        assert!(
            chunk.waveforms[0].iter().all(|v| v.abs() <= limit + 1e-9),
            "samples exceeded the ceiling"
        );
    }

    #[test]
    fn empty_waveforms_do_not_panic() {
        let mut params = default_params();
        params.channels = 4;
        params.dialogue_channels = Some(vec![0, 1]);
        params.monitor_channels = Some(vec![0, 1]);
        params.reference_level = Some(-27.0);
        let mut processor = processor(params);
        let waveforms = vec![vec![0.5; CHUNK], vec![0.5; CHUNK], Vec::new(), Vec::new()];
        let mut chunk = chunk_from(waveforms);
        processor.process_chunk(&mut chunk).unwrap();
        assert!(chunk.waveforms[2].is_empty());
        assert!(chunk.waveforms[3].is_empty());
    }

    #[test]
    fn all_empty_monitor_channels_is_a_no_op() {
        let mut processor = processor(pinned_params(-27.0));
        let mut chunk = AudioChunk::new(vec![Vec::new(), Vec::new()], 0.0, 0.0, 0, 0);
        assert!(processor.process_chunk(&mut chunk).is_ok());
    }
}
