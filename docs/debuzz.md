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

1. `grid_period`: the fewest whole samples that hold whole cycles of 200 Hz, `sr / gcd(sr, DEBUZZ_HZ)`: 240 at 48 kHz, 441 (two cycles) at 44.1 kHz, so a Suno song resampled to 44.1 kHz is still caught (+18.6 dB on Warm Current, -47.9 dBFS removed against -49.7 at 48 kHz). At odd rates where that would take more than 4 cycles the stage is off.
2. Mix to mono and keep content above 1 kHz (`band_pass`: one full-length FFT with bins zeroed), so bass doesn't drown the measurement.
3. Fold at the period: for each of the 240 phases, the mean and variance over all `k` periods in the track.
4. SNR = energy of the synchronous average divided by `variance / k`, the energy random content would leave in it. Content with no grid reads about 0 dB; the stage acts at `DEBUZZ_MIN_DB` (6 dB).

### Other periods (`--periods`)

`grid_scan` scores every period from `sr/1000` to `sr/10` at once from the autocorrelation (mono, above 1 kHz, averaged over ~11 s blocks): `10 log10(1 + 2 sum_d rho(d * per))`, the fold SNR over a half-block window. `--periods` prints the top eight, then the exact whole-song fold SNR and the repeating part's power for the top four at 1-4x their period. On Suno the top periods are all divisors of 240 (48, 60, 80, 120); most of the buzz energy repeats every 60 samples (800 Hz).

A general any-period debuzz was measured and not built (2026-10-23):
- Human masters also hold exactly periodic content: The Workout (Utada) reads +11.2 dB at 119 samples, Apple And Cinnamon +7.5 dB at 239. An unknown period would need a threshold near 14 dB (Suno: 15.8 to 23.5).
- The period can't be picked by captured energy: on Suno it keeps rising with multiples (240: -26.2 dB, 480: -25.3, 960: -24.4 re the band) as held notes leak in, and the top-scoring period (60) catches only part of the buzz.
Without samples from other generators there is nothing to calibrate against; `--periods` is the tool for when they arrive.

## Removal: `debuzz`

The buzz's shape holds across a song while its level drifts, so per channel the shape is estimated once and only its level is tracked:

1. **Shape:** the synchronous average over the whole channel, one value per phase (240). Over a 3-minute song that averages about 40,000 periods, so music leaks into it about 20 dB less than into a 4 s average.
2. **Level:** Hann windows of `DEBUZZ_WINDOW_S` (4 s, rounded to whole periods) at 50% overlap. In each window, the weighted synchronous average is projected onto the shape: one gain per window, never negative.
3. **Template:** gain x shape, overlap-added with the same weights and normalised. Windows with fewer than 50 periods (the file's edges) are skipped as too noisy to trust.
4. **Per-bin gate:** both the signal and the template go through the denoiser's STFT (2048 / 512 at 48 kHz). In each bin the template is scaled by `min(1, |x|^2 / |t|^2)` before it is subtracted. Real buzz is part of the signal and never outweighs it, so this only bites where the template holds something that isn't there.

It runs before denoise, so the buzz is never mistaken for steady noise floor, and it ends up in `Removed Noise.wav`.

### Why not a fresh average per window (v0.6-v0.7)

The first version averaged each 4 s window on its own: 240 free values per window, estimated from loud music. Two problems came out of a user's salsa track:

- **Ringing on held notes.** Any steady note with a harmonic on a multiple of 200 Hz leaks into the window's average. That means every A at 440 Hz tuning (5 x 440 = 2200, 10 x 440 = 4400). The leaked tone was then subtracted for up to 4 s around the note, including after it had stopped, which put an inverted copy of the harmonic into the silence: a ring that every later stage made louder. On a synthetic bright A5 note (0.7 s on, 1.3 s off), the gaps came out 9 to 12 dB louder than the input, with the note's harmonics +20 to +30 dB. A C6 note, off the grid, was untouched. Now the gaps come out quieter than the input, as they should.
- **Music damage as large as the buzz.** With a known buzz (French Exit's) added to three clean human masters, the old estimate's error against the truth was 0 to +11 dB *above* the buzz itself, because 240 values fitted to 4 s of loud music carry noise about the size of the buzz. Shape plus gain brings it to between -3 and +0.6 dB on the real (drifting) buzz, and -9 to +0.4 dB on a buzz whose level only drifts. Per-band gains (four bands) did worse than one gain, since they add back degrees of freedom.

The remaining cost is a dent of about 4 dB in on-grid harmonics *while* such a note plays, in the worst case of a note phase-locked across the whole song (a synthetic oscillator). Real performances restart their phase with each note, so it averages out of the shape.

## Measured

- **Removed:** -45.9 / -49.7 / -52.3 dBFS (Glass Hearts / Warm Current / French Exit), about 4.5 dB less than v0.7's -41.6 / -44.9 / -47.7. The difference is music the old estimate took along with the buzz.
- **Chain unchanged otherwise:** on Glass Hearts the denoiser's band attenuation is within 0.07 dB, and pre-hit energy is identical (-14.9 dB). It costs under a second on a 5-minute track.
- **The haze is separate:** removing the buzz does not reduce what the denoiser takes out, so the buzz is its own defect.
- **Don't judge it by the repeat left afterwards.** Subtracting a per-window average drives the whole-song fold to zero by construction, even where it adds error. Only a known buzz added to known music measures removal honestly.

## Constants

| Constant | Value | Meaning |
|---|---|---|
| `DEBUZZ_HZ` | 200 | grid rate; period `sr / gcd(sr, 200)` samples (whole cycles) |
| `GRID_BLOCK` | 2^19 | `grid_scan` autocorrelation block (~11 s at 48 kHz) |
| `DEBUZZ_MIN_DB` | 6 | repeat SNR to count as present |
| `DEBUZZ_WINDOW_S` | 4 | level-tracking window |

## Tests

`debuzz_does_not_ring_after_on_grid_note`: stereo noise, a 240-sample pattern, and a 2200 Hz note held 1 s in every 4, phase-locked so it lands in the shape. The energy in the seconds after each note must be no higher than the input's. Without the per-bin gate it reads +17.9 dB.

`debuzz_removes_decoder_grid`: at 48 and 44.1 kHz, stereo noise plus a fixed random one-period waveform (240 / 441 samples) 26 dB under it. Plain noise must read below `DEBUZZ_MIN_DB`, the mixture above 15 dB, and the output below 3 dB; the output minus the clean noise must be under 6% of the noise level (what remains is the noise inside the notches, about 28 dB down). It fails if debuzz is disabled.
