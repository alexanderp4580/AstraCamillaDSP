# AstraCamillaDsp

A fork of [CamillaDSP](https://github.com/HEnquist/camilladsp) by Henrik Enquist,
based on [v4.1.3](https://github.com/HEnquist/camilladsp/tree/v4.1.3). It is built
to run on a Raspberry Pi under [moOde audio](https://moodeaudio.org/), together with
[AstraCamillaGui](https://github.com/alexanderp4580/AstraCamillaGui).

Configuration files are the same as upstream v4.1.3. Everything not listed below
works exactly as in the upstream
[documentation](https://github.com/HEnquist/camilladsp/blob/v4.1.3/README.md).

## Changes from upstream

- **NightMode processor.** Makes films watchable at low volume by reducing loud
  effects and music while leaving dialogue alone. It adds no latency.
  Reference: [nightmode.md](nightmode.md).
- **Live analysis over the websocket.** Clients subscribe to spectrum and level
  data and the server pushes frames, so there is no polling and no second
  CamillaDSP instance. See [Analysis API](#analysis-api) below.
- **Bugfix.** The compressor and noise gate no longer panic when a monitored
  channel is unused and arrives empty.
- **CI** builds and tests Linux targets only.

## Analysis API

These commands go over the normal control websocket.

Subscribe:
```json
{"Subscribe": [{"Spectrum": {"num_bins": 64, "fmin": 20, "fmax": 20000}}, "Energy"]}
```
All `Spectrum` fields are optional. The values shown are the defaults.

The server then pushes the following messages. Unlike command replies, they have
no `result` field:
```json
{"SpectrumFrame": {"seq": 812, "bins_db": [-62.1, -58.4, ...]}}
{"EnergyFrame": {"seq": 813, "rms_db": [-21.3, -22.0], "peak_db": [-9.8, -10.4]}}
```

- `SpectrumFrame`: log-spaced bins of the processed (playback-side) signal. At most
  one frame is sent per audio chunk.
- `EnergyFrame`: RMS and peak level per playback channel, sent at up to 60 fps.

Stop:
```json
{"Unsubscribe": [{"Spectrum": {}}, "Energy"]}
```

The FFT only runs while spectrum is subscribed. It assumes one analysis client:
when any client unsubscribes from `Spectrum`, the FFT stops for everyone.

## Building

```bash
cargo build --release
```
Build options and backends are the same as upstream. For a Pi, use the
`camilladsp-linux-aarch64` artifact from the
[Publish](https://github.com/alexanderp4580/AstraCamillaDSP/actions/workflows/publish.yml)
workflow.

## License

Same as upstream: GPL-3.0 or MPL-2.0, at your option. See
[LICENSE_GLPv3.txt](LICENSE_GLPv3.txt) and [LICENSE_MPL2.0.txt](LICENSE_MPL2.0.txt).
