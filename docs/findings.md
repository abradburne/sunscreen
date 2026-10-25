# Findings: what was measured, built and rejected

The project rule: **measure first**. A candidate stage is built only if a diagnostic on real Suno tracks shows the problem exists, and each diagnostic is validated against a control (a signal where the answer is known) before its result is trusted. Every built stage then gets a unit test with a positive control (the effect appears) and a negative control (it does not when disabled, or when the content should be left alone); see [testing.md](testing.md).

Test material: four Suno tracks (Glass Hearts, Warm Current, French Exit, What About Me; some as MP3, some as WAV) and human-made, commercially mastered references by Ariana Grande, milet and Hikaru Utada / Utada.

## Built because the measurement showed a problem

| Stage | Measurement | Result |
|---|---|---|
| Rolling-floor denoise | `--floors`: haze follows the music | haze is not a fixed floor, see below |
| Gate | `--audibility`: 42 to 83 percent of ungated removal was masked | wasted removal down 48 to 80 percent, audible removal kept at 99 percent |
| `--smooth` | `--flutter`: denoise increases flutter | brought back below the original |
| `--widen` | `--width`: every track collapses to mono at the top, WAVs included | 16 to 20 kHz side/mid -18.0 to -8.6 dB |
| `--excite` | long-term spectrum: MP3 stops near 18 kHz | 20 to 22 kHz -87 to -16 dB on Warm Current |
| `--match` | `--tone` sweep: corrections disagree in sign across tracks | per-track correction, capped at 3 dB |

### Haze follows the music (`--floors`)

Per 1 s block and band, level `L` (mean power) and floor `F` (15th percentile) are compared, grouped by loudness quintile. Additive constant hiss pins the quiet blocks, so `F/L` rises toward 0 dB at the quiet end. The control (a human-made track with steady hiss added) shows exactly that: quietest-quintile `F/L` at 3.5 k / 7 k / 12 k+ rises from -15.8 / -22.0 / -27.6 dB to -14.7 / -14.3 / -16.7 dB.

`F/L` for the Suno tracks, 3.5 to 7 kHz, quietest to loudest quintile:

| Track | F/L (dB) |
|---|---|
| Glass Hearts | -20.6, -13.5, -11.8, -12.0, -10.9 |
| Warm Current | -34.5, -27.6, -21.6, -16.6, -15.5 |
| French Exit | -30.8 ... -24.5 |
| What About Me | -18.2 ... -10.8 |

The quietest blocks sit further below the floor, not at it: the haze falls with the music; there is no constant floor. That is the reason for the rolling floor rather than a whole-song profile. (The 12 kHz+ band is unreadable here; the codec lowpass leaves empty bins that drag the percentile to -50 to -90 dB.)

### Gate

`--audibility`, share of ungated removed energy (masked, ungated to gated; audible, ungated to gated):

| Track | Masked | Audible |
|---|---|---|
| Glass Hearts | 42.4 to 21.9 | 30.0 to 29.8 |
| Warm Current | 71.5 to 18.9 | 12.2 to 12.0 |
| French Exit | 83.3 to 17.1 | 5.1 to 5.0 |
| What About Me | 43.9 to 22.0 | 29.1 to 28.9 |

On Glass Hearts the removed signal's correlation with the music's envelope goes 0.549 to 0.419, overall change -0.61 to -0.46 dB, attacks unchanged (10.40 ms rise, -15.0 dB pre-energy). Two lessons from building it: gating before the mask blur cost 23 to 36 percent of the audible removal (hence dilate, blur, apply after); and a fixed masking offset is a guess that decides the answer (10 dB gave 43 percent masked, 25 dB gave 1 percent), which is why the tonality-dependent Johnston offset replaced it. See [gate.md](gate.md).

### Reference match

There is no common "Suno tone". Corrections to 2 to 3 kHz against the 12 masters: Glass Hearts -2.1 / -2.4 dB, French Exit +5.5 / +4.9, Warm Current +2.3 / +0.8, What About Me +0.5 / -1.1. The references agree with each other (third-octave spread 1 to 2.7 dB from 125 Hz to 10 kHz), so their mean is a stable target, and the built-in `POP_TARGET` lands within 0.7 dB of the average of the 12 single-reference corrections.

## Measured and rejected

### Spectral hole filling (`--holes`)

Idea: restore bins that MP3 zeroed. Measured 7 to 12 kHz hole rate at n = 2048:

| Source | Hole rate |
|---|---|
| French Exit WAV | 1.95% |
| 320k re-encode | 1.95% |
| 128k re-encode | 2.19% |
| 64k re-encode (positive control) | 5.55% |
| Suno MP3s (179 to 197 kbps VBR) | 1.6 to 1.95% |

Above 12 kHz the Suno MP3s read 2.58 percent against 2.04 for the WAV, about a 320k encode. The detector works (the 64k control lights up); at Suno's bit rates there are no holes to fill. Revisit only for files at 96 kbps or below.

### Repetition averaging (`--repeats`)

Idea: Suno songs repeat sections, so average the repeats to cut haze. A median of *levels* across repeats cannot lower the haze power, since every repeat carries the same haze level; it only smooths texture, which `--smooth` already does more cheaply. Real reduction needs waveform averaging, which needs repeats aligned sample for sample.

| Track | Similarity p50 / max | Coherence below 350 / 350 to 3.5 k / 3.5 to 7 k / 7 to 12 k / 12 k+ |
|---|---|---|
| Exact-repeat control | 0.87 / 0.97 | 0.89 / 0.88 / 0.86 / 0.82 / 0.81 |
| Glass Hearts | 0.77 / 0.95 | 0.46 / 0.14 / 0.01 / 0.02 / 0.01 |
| Warm Current | 0.75 / 0.92 | 0.33 / 0.11 / 0.03 / 0.02 / 0.02 |

Suno re-renders every repeat. Above 3.5 kHz repeats are unrelated as waveforms, so waveform averaging would cancel music, not haze.

### Phase locking (`--phase`)

Idea: Suno output might be phasey (loose phase between neighbouring bins of a partial) and benefit from phase locking. Mean deviation from the ideal pi offset at tonal peaks:

| Mean deviation (degrees) | 350 Hz to 3.5 k | 3.5 to 7 k | 7 to 12 k | 12 k+ |
|---|---|---|---|---|
| Steady-tone control | 0.0 | 0.1 | n/a | n/a |
| Utada (2 tracks, human-made, AAC) | 10.8 to 11.5 | 15.3 to 17.3 | 18.0 to 19.4 | 18.8 to 19.8 |
| Suno (four tracks) | 8.2 to 12.7 | 16.5 to 16.9 | 16.4 to 17.1 | 17.0 to 19.2 |

Suno sits right alongside the human-made reference, so there is no phasiness to fix. An earlier version used the rolling noise floor as the peak test and counted a sustained note as "floor"; the steady-tone control caught that.

### Sharper attacks (`--attacks`)

Idea: Suno transients are soft; sharpen them. 10 to 90 percent rise time and pre-peak energy on content above 2 kHz:

| Source | Median rise | Pre-energy |
|---|---|---|
| Instant-attack control | 1.38 ms | -45.4 dB |
| 10 ms ramp control | 9.21 ms | -14.5 dB |
| Utada (2 tracks) | 7.9 to 8.0 ms | -15.5 / -17.0 dB |
| Warm Current, French Exit | 7.2 / 4.7 ms | -16.7 / -20.5 dB |
| Glass Hearts, What About Me | 10.2 / 11.2 ms | -15.0 / -16.9 dB |

Suno is not consistently smeared: two tracks are 2 to 3 ms slower than the human reference, two are as fast or faster, and pre-energy matches. There is no codec damage to repair. A "punch" transient shaper would be a mixing taste choice and is parked. The short-window-switching half of the idea is covered by drum-hit protection. `--attacks` remains the regression check for the chain.

### Harder denoise on the side signal

Idea: haze is concentrated in the side signal. On three tracks, mid and side haze reduction differed by at most about 0.1 dB in every band. It is not, so the mask stays linked across channels.

### Codec frame-rate fingerprint (`--modspec`): first verdict wrong, see [debuzz.md](debuzz.md)

Idea: if Suno decodes from a neural codec at a fixed frame rate (EnCodec 75 Hz, DAC 86 Hz, many at 50 Hz), haze flutter at that rate would show as a sharp peak in the modulation spectrum, and a modulation notch could cut haze with little collateral damage. Control: 10 percent flutter at 75 Hz in noise reads above 10 dB, steady noise below 4 dB (and mutation-checked).

Result: every peak in each Suno track is a multiple of that track's own tempo grid (a sixteenth-note grid: 6.6, 3.5, 4.25 and 4.17 Hz on four tracks), none is shared across tracks, and the candidate codec rates sit at the same prominence as a human-made AAC reference (at or below about 5 dB; one track reads 5.8 to 6.6 dB at 50 Hz, which is the twelfth multiple of its own 4.17 Hz grid). This first verdict was wrong. The candidate list lacked 200 Hz, the search stopped at 400 Hz, and the 1 kHz envelope folded the grid's harmonics together. A wider search after the neural-codec check found Suno's decoder grid: a waveform repeating every 240 samples (200 Hz) in three of four tracks, now removed by the debuzz stage. See [debuzz.md](debuzz.md).

### Residual-only denoise

Idea: split the spectrogram by median filtering into harmonic, percussive and residual parts (H = median over +/-8 frames, P = over +/-8 bins) and cut only the residual, on the theory that haze lives there. Glass Hearts, with the gate on:

| Variant | Audible removal (% of ungated total) | Masked removal | env corr |
|---|---|---|---|
| Gate (current default) | 29.8 | 21.9 | 0.419 |
| + residual, 6 dB | 10.8 | 5.7 | 0.396 |
| + residual, 12 dB | 16.2 | 9.7 | 0.417 |
| + residual, 20 dB | 20.3 | 13.3 | 0.424 |

A third to two thirds of the audible-haze removal is lost for almost nothing gained on music damage. Audible haze sits in bins the split calls harmonic or percussive too. The hearing-model gate already does this job better.

### Off-the-shelf restoration (Apollo)

Apollo (a model trained to restore MP3-compressed music), official checkpoint, run on a 30 s Glass Hearts excerpt at 44.1 kHz. Change versus original (dB):

| | below 350 | 350 to 3.5 k | 3.5 to 7 k | 7 to 12 k | 12 to 16 k | 16 to 18 k | 18 to 22 k |
|---|---|---|---|---|---|---|---|
| Apollo | +0.8 | +0.9 | +0.4 | +0.3 | -0.5 | -4.0 | +9.7 |
| sunscreen Clean | -0.2 | -0.5 | -0.9 | -0.8 | -0.7 | -0.6 | -0.3 |
| sunscreen full chain | -0.1 | -0.4 | -0.8 | -0.8 | -0.6 | -0.5 | +12.2 |

Haze flutter (3.5 to 7 k / 7 to 12 k) was 3.91 / 5.97 dB original and 4.07 / 5.97 dB after Apollo: untouched. Apollo leaves the haze, lifts overall level about 0.8 dB, dents real content at 16 to 18 kHz and adds less air than `--excite`. It was trained on low-bit-rate MP3 damage, which Suno's roughly 190 kbps files do not have; Suno's haze is a different defect. For any learned approach, the training pairs must reproduce Suno's own degradation.

### Re-rendering the noise part (parked)

Idea: keep sinusoids, regenerate the stochastic part with clean noise of the same envelope. No measurement separates it from `--smooth` (both flatten the haze's level readings), and `--smooth` already passed listening. Revisit only if smoothing sounds watery at useful settings.

### Neural codec round trips (EnCodec, DAC)

Clean human masters were round-tripped through EnCodec 48 kHz (6, 12, 24 kbps; joint stereo) and DAC 44 kHz (per channel):

| | 16 to 20 kHz coherence / side vs mid | Denoise removal 7 to 12 k | Periodic grid |
|---|---|---|---|
| Human original | 0.84 / -7.6 dB | -0.66 dB | none |
| After EnCodec | 0.997 / -14.5 to -15.1 dB | -0.68 to -0.73 dB | 40 samples (1200 Hz), +12 to +15 dB |
| After DAC | 0.32 / -2.2 dB (wider) | -0.81 to -0.85 dB | 512 samples, weak |
| Suno | 0.97 to 0.999 / -16 to -30 dB | | 240 samples (200 Hz) |

- **High-frequency width collapse is a joint-stereo codec trait.** EnCodec reproduces Suno's collapse exactly; per-channel DAC does the opposite. That supports `--widen`.
- **The haze is not codec quantization noise**: neither codec adds denoiser-removable haze at any tested bitrate. Training pairs made with these codecs would teach width collapse and the wrong grid, not haze.
- **Suno's codec is neither**: its grid is 240 samples at 48 kHz, a 200 Hz latent frame rate.

### Blind sweep (12 Suno tracks vs 14 human masters)

After the buzz turned up in a place the earlier searches never looked, a sweep compared generic statistics instead of testing guesses: 12 Suno tracks (6 MP3, 6 WAV, three albums plus loose tracks) against 14 commercial masters.

| Check | Suno | Human masters | Verdict |
|---|---|---|---|
| Waveform repeat at any period 2-2400 samples (plus common frame sizes to 9600) | 11 of 12: every top period divides 240 (5, 6, 30, 60, 120, 240...), +13 to +23 dB | +6 to +12 dB, all musical notes | Only the 200 Hz decoder grid; debuzz covers it |
| Steady spectral lines | Musical notes, plus lines at 200 Hz multiples (the buzz) | Musical notes; two have 50 Hz mains hum | Nothing new |
| Cyclic (rhythmic) components to 5 kHz in the >2 kHz band | Musical only | Musical only | Nothing |
| **Start and end** | First sample up to -13.7 dBFS; last 100 ms up to -20 dBFS (stops mid-sound) | Digital silence at both ends, 50-760 ms lead-in | **Clicks**: fixed with 5 ms edge fades |
| Bass width, 20-250 Hz | correlation 0.87-0.998 below 60 Hz | 0.34-1.0 | Same range |
| Crest factor | 12-15 dB | 10-11.6 dB | Suno is less compressed; `--master` covers loudness |
| DC, sub-20 Hz, L/R timing, polarity | normal | normal | Nothing |

### Endings

Last 250 ms before any trailing silence, in dB re the song (mean RMS of its louder half of 1 s blocks): human masters (16 Utada tracks) -52 to -82 dB, then 1.4 to 2.5 s of digital silence. Suno: 9 of 21 between -38 and -1 dB, so they stop mid-sound. Built: those get a 3 s fade-out ending at the last sound (threshold -40 dB). It flags exactly those 9 and no human master.

### Suno v6 (2026-10, 3 WAVs)

Same 240-sample decoder buzz (+21 to +22 dB; debuzz works). Highs reach about 20 kHz (v5: 17.8 to 18.1). Width is healthy from 1 to 13 kHz but the top octave still collapses (16-20 kHz side/mid -20 to -25 dB), so `--widen` still helps. Loudness -16.7 to -17.0 LUFS. 2 of 3 stop mid-sound. The fluttering haze is mostly gone: 7-12 kHz flutter 3.9 to 4.6 dB (v5 up to 17, human masters 5.5 to 10), and the floor sits within 10 to 14 dB of the level as on human masters. The denoiser, built for v5, read the steady wash as haze and cut 1 to 1.4 dB of the highs, slightly duller by ear; its strength now follows the flutter ([denoise.md](denoise.md)). `--excite` is skipped above a 19.5 kHz cutoff.

### Album mode: measured, not built

Does a batch need one shared tone target? Per-band spread of the third-octave profile across an album's songs, 63 Hz to 10 kHz: Warm Current (10 songs) 1.31 dB originally, 0.91 after the full chain; Suddenly Summer (7) 1.01 to 0.89. Every song already goes to the same pop target and the same loudness, and that spread is already tighter than the 12 commercial masters behind the pop target (1 to 2.7 dB). A simulated album-mean target instead of the pop target gains only 0.05 to 0.2 dB.

## Open ideas

- A small neural restoration model. The codec check above shows EnCodec and DAC don't reproduce Suno's haze, so training pairs need a different simulator, or self-supervision on Suno files themselves.
- The second grid artifact: the high band's power ripples 3 to 3.7 dB over each 240-sample cycle, with a frequency-dependent phase (likely spectral imaging from the decoder's upsampler). Debuzz doesn't touch it; removing it needs cyclic (frequency-shift) filtering.
- Stems mode: each stem has real noise-only gaps, so the floor could be measured instead of estimated. The first step would be comparing a stem-gap floor with the rolling floor.

## Floor timing (open question)

A faster floor was tried on Glass Hearts: block 1.0 / 0.5 / 0.25 s gave RMS change -0.46 / -0.56 / -0.65 dB and `env corr` 0.419 / 0.435 / 0.452. Neither metric can referee: `env corr` is expected to rise if the haze truly follows the music, and the audibility classes move with the floor estimate. It needs ears; the default stayed at 1.0 s.
