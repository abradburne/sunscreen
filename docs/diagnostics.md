# Diagnostics

Each diagnostic flag measures something, prints it, and exits without writing audio. They are how every decision in the project was made and how to check any change. Run them as `sunscreen "Song.mp3" /tmp --flag`.

Before a diagnostic runs, `main` prints the `input`, `params` and `floors` lines. Flags are tested in the order below (the first one present wins). All use the default `Params` for the file's sample rate, except where noted. Band labels such as `3500+` mean "from 3500 Hz up to the next band edge"; the `BANDS` edges are 0, 350, 3500, 7000, 12000 Hz and Nyquist.

A recurring principle: a measurement is only trusted once a control (a signal whose answer is known) has been run through it. The controls cited here come from the code (where `--flutter` builds its own) or from the project's measurement notes.

| Flag | Needs stereo | What it measures |
|---|---|---|
| `--flutter` | no | frame-to-frame jitter of the haze level |
| `--audibility` | no | masked / borderline / audible split of removed energy |
| `--attacks` | no | rise time and pre-energy at detected hits |
| `--phase` | no | phase coherence around tonal peaks |
| `--width` | yes (asserts) | L/R coherence and side/mid per band |
| `--repeats` | no | how similar and how phase-aligned repeated sections are |
| `--modspec` | no | modulation spectrum of high-band envelopes, at codec frame rates |
| `--tone=ref.wav` / `--tone=pop` | no | the uncapped `--match` correction |
| `--profile` | no | third-octave long-term spectrum |
| `--floors` | no | noise floor against music level |
| `--holes` | no | rate of MP3 "spectral holes" |

## `--flutter`

`flutter(p, x, sr)`. Question: how much does the haze level jump around?

For every bin `f` (excluding the first and last), and every 1-second block (`bf = round(sr / hop)` frames):

1. Take the bin's power over the block. Skip the block if its **median** exceeds 2.5 times the rolling floor (`noise / noise_scale`) at the block centre, or if that floor is below 1e-12. This keeps only bins that are haze-dominated in that second.
2. Group the block's frames in chunks of 8 (about 85 ms) and convert each chunk's mean power to dB. These are the level readings.
3. Compute the **robust spread** of the readings: `1.4826 * MAD` around the median (median absolute deviation, scaled to equal sigma for Gaussian data), so the odd drum hit in a quiet bin does not count as flutter.
4. Average the spreads over all qualifying (bin, block) pairs per band.

Output: for each band, `lo+ value dB (count)`. Two rows are printed: `track` (your file) and `steady` (the control: uniform noise at 0.1 amplitude, from a fixed LCG with seed 42, given the same length and channel count as your file). Steady noise gives the chance baseline: spread that comes from nothing but estimating a level from 8 frames.

How to read it: a higher number means a more fluttery haze. Absolute values are not comparable across tracks, because rhythmic music also moves the level; compare stages of the same track (original, Clean, Clean+smooth). Glass Hearts, 3.5 to 7 kHz / 7 to 12 kHz: original 5.93 / 7.39, Clean 6.97 / 8.60, smoothed 5.19 / 6.45. The unit test for `--smooth` uses this function ([testing.md](testing.md)).

## `--audibility`

`audibility(p, x, sr)`. Question: how much of what the denoiser removes was inaudible?

Runs `denoise` with the gate off and on, takes per-bin `power` and the final gain `g`, and sums the removed energy `power * (1 - g^2)` into three classes by noise-to-mask ratio `nmr = 10 log10( (noise/noise_scale) / thr )`: masked (`nmr < -3 dB`, `-GATE_RAMP_DB`), within +/-3 dB, audible (`nmr > +3 dB`). The threshold `thr` comes from `masking` ([gate.md](gate.md)). Both rows are printed as a percentage of the **ungated total**.

How to read it: the gate is working if the masked share drops a lot and the audible share barely moves. Glass Hearts: masked 42.4 to 21.9 percent, audible 30.0 to 29.8. Note the classification uses the same model as the gate, so this checks that the gate does what the model says; whether the model matches ears is a listening question.

## `--attacks`

`attacks(p, x, sr)`. Question: do the drums keep their attack?

1. Detect hits with `onsets` (see [denoise.md](denoise.md)).
2. Make a mono signal, remove everything below 2 kHz (STFT with bins below 2 kHz zeroed), and compute a 1 ms RMS envelope (window of +/-0.5 ms) using prefix sums.
3. For each hit, search the frame's span (but not earlier than 30 ms into the file) for the envelope peak `pk`. Skip hits with `pk < 1e-6`.
4. Walk back from the peak, up to 30 ms, to find where the envelope first drops below 90 percent and below 10 percent of the peak. The **rise time** is the time between those two points. Hits where either crossing is not found are skipped.
5. **Pre-energy**: mean power from 25 ms to 5 ms before the peak, relative to the peak power, in dB.

Output: the number of hits measured, the median rise time in ms, and the median pre-energy in dB. Lower rise time and lower pre-energy mean a sharper attack.

Controls: an instant-attack signal reads 1.38 ms and -45.4 dB; a 10 ms ramp reads 9.21 ms and -14.5 dB.

How to use it: this is the **regression check** for the whole chain. Run it on the original and on each cumulative output; if rise time grows or pre-energy rises, a stage is dulling drums. On Glass Hearts pre-energy goes -15.0 to -15.2 dB across Clean, +smooth, +widen, +excite, i.e. no added smearing. The hit count can change between runs (228 to 191 after smoothing), which makes the rise time a hint, not a verdict.

## `--phase`

`phase_dev(p, x, sr)`. Question: are tonal peaks phase-coherent (is the sound "phasey")?

For a steady partial under the frame-start-aligned Hann window, the two neighbours of the peak bin `k` sit exactly pi away from it in phase, whatever the partial's exact frequency. The diagnostic finds tonal peaks and measures how far the neighbours drift from that.

- A bin `k` (3 to `bins-4`) is a peak if its power exceeds that of bins `k-2 .. k+2`.
- It must also stand at least 10 dB over its neighbourhood: power at least 10 times the median of the 17 bins `k-8 .. k+8`. (A rolling time floor was tried first and wrongly counted sustained notes as noise.)
- `dev = ( |wrap(arg(k-1) - arg(k) - pi)| + |wrap(arg(k+1) - arg(k) - pi)| ) / 2`.

Output per band: the power-weighted mean deviation in degrees, the percentage of peaks with `dev > 30` degrees, and the peak count.

Controls: a steady tone reads 0.0 to 0.1 degrees. A human-made AAC reference (Utada) reads 10.8 to 11.5 degrees at 350 Hz to 3.5 kHz up to 18.8 to 19.8 above 12 kHz; Suno (Glass Hearts, Warm Current, French Exit, What About Me) reads 8.2 to 12.7 up to 17 to 19.2. Suno sits alongside the reference, so there is no extra phase smear to repair.

## `--width`

`width(p, x, sr)`; asserts two channels. Question: how wide is the stereo image, per band?

Per frame, the spectrum is divided into 60 log-spaced sub-bands from 50 Hz up (`50 * 400^(i/60)`, about a seventh of an octave each; the code comment says one sixth). In each sub-band: coherence `|sum L R*| / sqrt( sum|L|^2 * sum|R|^2 )`, and the side and mid power. Sub-bands are grouped into `WIDTH_BANDS` (0 to 350, 350 to 1000, 1000 to 3500, 3500 to 7000, 7000 to 10000, 10000 to 13000, 13000 to 16000, 16000 to 20000 Hz). Coherence is averaged weighted by `|L|^2 + |R|^2`; side/mid is `10 log10( sum side / sum mid )` over the whole band.

Measuring coherence per narrow sub-band and per frame means a band coded as one signal panned left or right (intensity stereo) reads exactly 1, not a spuriously lower value.

Output: per band `coherence` and `side/mid` in dB. Coherence near 1 and side/mid far below the mids is the collapse that `--widen` repairs. Warm Current, 7 to 13 kHz / 16 to 20 kHz: coherence 0.53 / 0.967, side/mid -4.4 / -16.2 dB. Control: the `widen` unit test builds a signal whose content above 12 kHz is forced to mono, and `width` must read it as coherence above 0.99.

## `--repeats`

`repeats(p, x, sr)`. Question: do repeated sections repeat as *waveforms*? Only if they did could averaging repeats lower the haze power.

1. Divide the track into quarter-second cells. For each cell, 40 log-spaced band energies in dB (100 Hz to 16 kHz). A segment is 4 consecutive cells (1 s), mean-removed and unit-normalised.
2. For each segment, find the best-matching segment at least 16 cells (4 s) away, by cosine similarity. Print the p10, p50, p90 and max similarity.
3. Take the 200 best pairs. Align each pair at sample level by FFT cross-correlation over +/- half a cell, then compute per-`BANDS` complex coherence `|sum A B*| / sqrt( sum|A|^2 sum|B|^2 )` over the segment.

Magnitude similarity says repeats exist; coherence says whether they match as waveforms. Control: a file made of exact repeats gives coherence 0.89 / 0.88 / 0.86 / 0.82 / 0.81 across the five bands. Glass Hearts reads 0.46 / 0.14 / 0.01 / 0.02 / 0.01 and Warm Current 0.33 / 0.11 / 0.03 / 0.02 / 0.02, while the similarity scores look high (p50 0.77 and 0.75). Suno re-renders every repeat: they look alike but are unrelated as waveforms above 3.5 kHz.

## `--modspec`

`modspec`, `prominence`. Question: does the haze carry a codec frame-rate fingerprint?

1. Mix to mono, zero-pad to a power of two, FFT. For each of the three upper bands (`BANDS[2..]`: 3.5 to 7 kHz, 7 to 12 kHz, 12 kHz up) keep only that band's bins and inverse-FFT.
2. Envelope: `ln( sum of squares over sr/1000 samples + 1e-30 )`, about a 1 kHz envelope rate. The log keeps loud sections from dominating.
3. Welch averaging: Hann-windowed 4000-sample (4 s) segments with 50 percent overlap, mean removed, power spectrum averaged. Resolution 0.25 Hz.
4. `prominence(k)`: the peak (best of bins `k-1 .. k+1`) over the median of the bins within +/-5 Hz of `k`, excluding +/-1 Hz, in dB.

Output per band: the six most prominent peaks between 5 and 400 Hz (prominence above 3 dB, local maxima), then the prominence at candidate rates: 50 Hz, EnCodec 75 Hz, DAC 86.13 Hz, `sr/512`, MP3 `sr/1152` (frame), MP3 `sr/576` (granule), AAC `sr/1024`.

How to read it: a codec with a fixed frame rate leaves a sharp peak at that rate. Music modulates at beat and syllable rates or in broad humps. Control (`modspec_finds_frame_rate_flutter`): noise with 10 percent amplitude flutter at 75 Hz reads above 10 dB there; steady noise reads under 4 dB. Result on Suno: all peaks are multiples of each track's own tempo grid, none shared between tracks, and the candidate rates sit at the level of a human-made reference. No fingerprint ([findings.md](findings.md)).

## `--tone=ref.wav`, `--tone=pop`

Prints the uncapped `match_curve` correction at 63, 125, 250, 500, 1000, 2000, 3000, 4000, 8000 and 12000 Hz. `pop` uses the built-in target. See [match.md](match.md).

## `--profile`

Prints the third-octave long-term spectrum as `centre_Hz:dB` pairs from 31.5 Hz up to 16 kHz, in dB relative to the mean of the 120 Hz to 8.5 kHz centres (`third_octaves`: each band spans +/- one sixth of an octave, power averaged in the band, then in dB). The built-in `POP_TARGET` is the mean of 12 of these. Use it on tracks you like to make your own target.

## `--floors`

Question: is the haze a fixed floor, or does it follow the music?

For each of the three upper bands and each 1-second block: level `L` = mean power over the block's frames and bins; floor `F` = the 15th percentile of those values. Blocks are sorted by `L` and split into five quintiles from quietest to loudest. For each quintile print the mean `L` in dB and the mean `10 log10(F / L)`.

How to read it: additive constant haze pins quiet blocks at a fixed floor, so `F/L` rises toward 0 dB in the quietest quintile. Haze that scales with the music keeps `F/L` roughly constant or lets it fall. Control: a human-made track with steady hiss added reads quietest-quintile `F/L` of -14.7 / -14.3 / -16.7 dB in the 3.5 k / 7 k / 12 k+ bands, up from -15.8 / -22.0 / -27.6 without the hiss. On Suno the quietest blocks sit further below the floor, not at it: the haze follows the music. The 12 kHz+ band is unreliable here because the codec lowpass leaves empty bins that drag the percentile to -50 to -90 dB.

## `--holes`

`hole_rates`. Question: are there MP3 "spectral holes" (coefficients the encoder zeroed, flicking a bin down for one frame)?

Runs at `n = 2048 * sr/48000` and at `n = 1152 * sr/48000`, with hop `n/4`. For frames `t >= 2`, the geometric mean `e = sqrt( P[t-2] * P[t] )` of a bin's power in the two frames either side of the previous one; the bin in frame `t-1` is a hole if `P[t-1] < 10^(-15/10) * e` (`HOLE_DB = 15`). Bins with `e < 1e-9` (silence) are skipped. Output: the percentage of holes per `BANDS` band.

Controls: the same song through re-encodes. For 7 to 12 kHz at `n = 2048`: French Exit WAV 1.95 percent, 320k 1.95, 128k 2.19, 64k 5.55 (the positive control: the detector sees holes when there really are some). Suno MP3s (179 to 197 kbps VBR) read 1.6 to 1.95 percent, i.e. the baseline. Only bit rates of 96 kbps or below would give something to fill.
