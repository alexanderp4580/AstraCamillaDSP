# Night Mode processor

Part of the [AstraCamillaDsp](README.md) fork. Configured like any other processor in the `processors` section; see the upstream [processor documentation](https://github.com/HEnquist/camilladsp/blob/v4.1.3/README.md#processors) for how processors fit into a pipeline.

The "NightMode" processor makes films watchable at low volume.
It reduces loud effects and music while leaving dialogue alone,
which is what separates it from a plain compressor:
a compressor applied to a film soundtrack flattens the dialogue along with everything else.

It adds no delay, so it does not affect lip sync.

Two dynamics stages run in series, at deliberately different speeds.
The slow stage does most of the work, and is too slow to be heard moving.
The fast stage only catches the leading edge of impacts that the slow one misses,
and stays shallow so that the attack of an impact survives.

Thresholds are not absolute. The processor tracks the level of the dialogue itself and
places its threshold `headroom` dB above that, so the same settings work for a quiet
drama and a loud blockbuster, and nothing needs recalibrating when the source level
changes. That tracked reference can never move faster than `reference_slew` dB per
second, which is what keeps it from wandering the way an auto-volume control does.

Dialogue is identified from three cues measured together: how centred the sound is
(by correlation between the channels, so a hard-panned source is not mistaken for a
centre-panned one), how much of the energy sits in the speech band, and whether the
level moves at syllabic rates. Confidence in that estimate can only ever *reduce* the
gain reduction, never increase it, so a mistake means night mode does less rather than
ducking someone's voice. The detector is a heuristic: if it ever misjudges your
material, `dialogue_protection: 0` switches it off and leaves a plain leveler.

Example:
```
processors:
  movienight:
    type: NightMode
    parameters:
      channels: 2
      amount: 100 (*)
      max_attenuation: 28 (*)
      bass_reduction: 10 (*)
      headroom: 0 (*)
      ratio: 12.0 (*)
      transient_softening: 100 (*)
      dialogue_protection: 60 (*)
      presence_gain: 0.0 (*)

pipeline:
  - type: Processor
    name: movienight
```

  Parameters:
  * `channels`: number of channels, must match the number of channels of the pipeline where the processor is inserted.
  * `amount`: how much of the effect to apply, 0 to 100 percent. Scales every gain reduction, in dB, so 50 gives half as many dB as 100. At 0, and with `presence_gain` left off, the processor passes audio through completely untouched, including the ceiling, which makes it a usable A/B reference. Optional, defaults to 100.
  * `max_attenuation`: hard limit in dB on the total reduction, whatever the content. This is what stops the result sounding flat. Optional, defaults to 28.
  * `bass_reduction`: extra reduction in dB below `bass_frequency`, applied in proportion to how hard the processor is currently working, so quiet scenes get no tonal change. Low frequencies are what carry through a house at night. Set to 0 to disable. Optional, defaults to 10, which is the maximum.
  * `headroom`: how far above the tracked dialogue level, in dB, reduction begins. Larger values let more dynamics through. Optional, defaults to 0, meaning reduction begins at the dialogue level itself. Raise it if the processor feels too eager.
  * `ratio`: compression ratio of the slow stage. Optional, defaults to 12.0.
  * `transient_softening`: depth of the fast stage, 0 to 100 percent. At 0 impacts pass at full level. Optional, defaults to 100.
  * `dialogue_protection`: how strongly dialogue is spared, 0 to 100 percent. At 0 the detector is switched off entirely. Optional, defaults to 60. Not 100, because holding back all reduction on anything the detector likes leaves loud centred music largely untouched; 75 trades a little dialogue protection for night mode actually doing its job.
  * `presence_gain`: a boost in dB around `presence_frequency`, applied only in proportion to how confident the processor is that it is hearing dialogue. Helps intelligibility at very low volume, at the cost of some audible coloration on voices. Optional, defaults to 0, meaning off. Maximum 6.
  * `presence_frequency`: centre frequency in Hz of the presence boost. Optional, defaults to 2500.
  * `presence_q`: Q of the presence boost. Optional, defaults to 0.7.
  * `attack`: time constant in seconds of the slow stage. Optional, defaults to 0.15.
  * `release`: time constant in seconds of the slow stage. Must not be shorter than `attack`. Optional, defaults to 1.5.
  * `reference_level`: pins the dialogue reference to a fixed level in dBFS and stops it adapting. Useful when reproducible behaviour matters more than self-calibration. Optional, adaptive by default.
  * `reference_slew`: the fastest the dialogue reference may move, in dB per second. Optional, defaults to 0.25.
  * `modulation_weight`: how much the syllabic-modulation cue counts, 0 to 1. At 0 dialogue is identified on being centred and in-band alone. Optional, defaults to 0.5.
  * `bass_frequency`: corner frequency in Hz of the bass reduction. Optional, defaults to 120.
  * `silence_threshold`: level in dB below which the detector stops updating, so a silent passage cannot move the reference. Optional, defaults to -70.
  * `ceiling`: output ceiling in dBFS. Always active. Optional, defaults to -1.0.
  * `dialogue_channels`: a list of channels carrying dialogue. Required unless the pipeline is stereo, where the centre is derived from the channel pair instead. There is no default, because guessing which channel holds dialogue would silently mangle a mix.
  * `monitor_channels`: a list of channels used when estimating the level. Optional, defaults to all channels. Worth setting to exclude an LFE channel.
  * `process_channels`: a list of channels to process. Optional, defaults to all channels.

The defaults are deliberately strong, on the basis that someone enabling night mode
wants to hear it working. They are also the first thing to back off if it is doing too
much: raise `headroom`, or lower `amount`, which scales everything at once.

For tonal compensation at low listening levels, combine this with the `Loudness` filter
rather than reaching for `presence_gain`.
