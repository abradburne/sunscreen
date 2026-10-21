# Debuzz: Suno's decoder grid

The first stage, on by default and automatic: it only acts when it finds the grid, and `--no-debuzz` turns it off.

## The problem

Suno's decoder works on a fixed grid of 240 samples at 48 kHz, i.e. 200 frames a second. It leaves a waveform that **repeats exactly every 240 samples**, locked to the start of the file, for the whole song: a faint buzz made of 200 Hz and its harmonics, strongest between about 1.4 and 11 kHz.

| Track | Repeat above chance | Buzz level | Track level |
|---|---|---|---|
| Glass Hearts | +23.0 dB | -41.6 dBFS | -15.8 dBFS |
| Warm Current | +20.7 dB | -44.9 dBFS | -15.3 dBFS |
| French Exit | +22.0 dB | -47.7 dBFS | -17.6 dBFS |
| What About Me | -1.4 dB | none | |
| Human masters (2) | -1.0, -2.4 dB | none | |

That is roughly 20 dB below the high-frequency content of the music: inaudible under a loud chorus, but present as a faint buzzy static in quiet passages and tails. Its shape is stable across a song (each tenth of the song correlates 0.71-0.94 with the whole-song average), while its level drifts by a few dB with the music. What About Me shows no grid, perhaps an older Suno model.

### How it was found

The search for a codec frame-rate fingerprint first came up empty (see [findings.md](findings.md)) for three reasons: 200 Hz was not among the candidate rates, the search stopped at 400 Hz, and the 1 kHz envelope used by `--modspec` folded the grid's harmonics onto one alias. Running clean human masters through EnCodec showed that codec's own grid (40 samples), which prompted a wider search:

- **Comb test:** at a 4 kHz envelope, every 200 Hz harmonic from 200 to 1800 Hz stands above off-grid controls halfway between them: mean +2.2 to +2.7 dB on-grid against +0.1 to +0.5 dB off-grid on three Suno tracks. On human tracks, on-grid equals off-grid.
- **Period scan:** folding the 7-12 kHz band power at candidate periods peaks sharply at exactly 240 samples (239 and 241 read at control level; the multiples 480 and 960 also read high).
- **Waveform fold:** the synchronous average of the signal itself at 240 samples carries 15-29 dB more energy than random content would leave, in every band from 1 kHz up. That makes it a deterministic, subtractable waveform, not just a statistical texture.

The same grid also modulates the *power* of the high band by 3-3.7 dB peak to peak over each 240-sample cycle, with a phase that varies with frequency. That second artifact is not removed by debuzz (nor by per-band periodic gain; likely spectral imaging from the decoder's upsampler) and remains an open problem.

Is it a watermark? Probably not. A fixed waveform repeating at the decoder's own step size is the classic side effect of upsampling layers, and watermarks are normally keyed, spread out and built to survive editing, whereas this one subtracts cleanly. It does, however, reliably identify audio from this Suno decoder.

## Detection: `grid_snr`

1. `grid_period`: the grid period is `sr / DEBUZZ_HZ` (240 at 48 kHz). If 200 Hz doesn't divide the sample rate evenly (44.1 kHz gives 220.5), there can be no sample-locked grid, and the stage is off.
2. Mix to mono and keep content above 1 kHz (`band_pass`: one full-length FFT with bins zeroed), so bass doesn't drown the measurement.
3. Fold at the period: for each of the 240 phases, the mean and variance over all `k` periods in the track.
4. SNR = energy of the synchronous average divided by `variance / k`, the energy random content would leave in it. Content with no grid reads about 0 dB; the stage acts at `DEBUZZ_MIN_DB` (6 dB).

## Removal: `debuzz`

Per channel, the repeating waveform is estimated locally and subtracted:

1. Hann windows of `DEBUZZ_WINDOW_S` (4 s, rounded to whole periods) at 50% overlap.
2. In each window, the weighted synchronous average at each of the 240 phases is the local template.
3. The windows' templates are overlap-added with the same weights and normalised, so the template follows the buzz's slowly drifting level.
4. Subtract it. Windows with fewer than 50 periods (the file's edges) are skipped as too noisy to trust.

In frequency terms this is a comb of notches about 0.25 Hz wide (1 / 4 s) at every multiple of 200 Hz. Music almost never sits within a quarter hertz of those exact frequencies, so the only cost is the sliver of music inside the notches: on a human track with no grid, the same operation removes content 33 dB below the track.

It runs before denoise, so the buzz is never mistaken for steady noise floor, and it ends up in `Removed Noise.wav`.

## Measured

- **Removed:** -41.6 / -44.9 / -47.7 dBFS (Glass Hearts / Warm Current / French Exit). Afterwards the 240-sample repeat drops from +20...+28 dB to below chance in every band.
- **Chain unchanged otherwise:** on Glass Hearts the denoiser's band attenuation is within 0.07 dB, and pre-hit energy is identical (-14.9 dB). It costs about 0.8 s on a 5-minute track.
- **The haze is separate:** removing the buzz does not reduce what the denoiser takes out, so the buzz is its own defect.

## Constants

| Constant | Value | Meaning |
|---|---|---|
| `DEBUZZ_HZ` | 200 | grid rate; period `sr / 200` samples |
| `DEBUZZ_MIN_DB` | 6 | repeat SNR to count as present |
| `DEBUZZ_WINDOW_S` | 4 | estimation window; sets notch width (~1/4 Hz) and how fast the level is tracked |

## Test

`debuzz_removes_decoder_grid`: stereo noise plus a fixed random 240-sample waveform 26 dB under it. Plain noise must read below `DEBUZZ_MIN_DB`, the mixture above 15 dB, and the output below 3 dB; the output minus the clean noise must be under 6% of the noise level (what remains is the noise inside the notches, about 28 dB down). It fails if debuzz is disabled.
