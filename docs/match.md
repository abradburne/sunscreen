# `--match[=ref.wav]`: tonal balance like a commercial master

Implemented by `match_curve` (the correction), `pop_target` / `POP_TARGET` (the built-in target) and `ltas_target` (a reference file as target). The EQ is applied in `main` as the last stage.

## The problem

Every Suno track has its own tonal tilt, and the tilts do not share a direction. Against 12 commercially mastered pop tracks, Glass Hearts wants 2 to 3 kHz pulled down about 2 dB (-2.1 / -2.4 dB, mean of 12), while French Exit wants that region lifted about 5 dB (+5.5 / +4.9). Warm Current wants +2.3 / +0.8 and What About Me +0.5 / -1.1. Because the corrections disagree in sign, a fixed "de-Suno EQ" would fix one track and wreck another. The correction has to be computed per track.

## Targets

**Built-in: `POP_TARGET`.** A table of 28 `(Hz, dB)` points at third-octave centres from 32 Hz to 16128 Hz: the mean third-octave long-term spectrum of 12 commercially mastered pop tracks (Ariana Grande, milet, Hikaru Utada / Utada), made with `--profile`. 0 dB is the mean of the 125 Hz to 8 kHz points. The 12 agree within 1 to 2.7 dB per point from 125 Hz to 10 kHz, and 3 to 4.6 dB above 12 kHz. `pop_target(h)` interpolates linearly in log2 frequency and holds the end values flat outside the table.

The 12 references also agree *within* a track: their individual corrections for a given Suno track mostly share a sign (to about +/-2 dB). The built-in target's correction lands within 0.7 dB of the average of the 12 single-reference corrections.

**A reference file: `--match=ref.wav`.** `ltas_target` computes that file's long-term spectrum with the STFT parameters for *its* sample rate (so it works if the reference has a different rate), and returns a function of Hz that interpolates linearly between bins and clamps to the last bin. Use this to aim at a genre or a song you like. The reference can be MP3, WAV, FLAC or M4A; AAC and lossless copies of the same master give curves within 0.1 dB of each other. The report's `match` line names the target (`toward <file>` or `toward pop target`).

## Algorithm: `match_curve(p, x, sr, target, cap)`

Returns a correction in dB per STFT bin.

1. **Track spectrum.** `lt = ltas(...)` of the current stage: mean power per bin over all frames and channels, in dB.
2. **Raw difference.** `raw[f] = target(hz(f)) - lt[f]`.
3. **One-octave smoothing.** With `half = 2^(MATCH_SMOOTH_OCT / 2)` (`MATCH_SMOOTH_OCT = 1`, so `half` is the square root of 2), each bin's value is the plain mean of `raw` over bins `floor(f / half) .. ceil(f * half)` (lower bound at least 1, upper at most the last bin). That is a window half an octave either side, a constant-Q smoothing on a linear-frequency grid. The effect is that the curve shapes tone and cannot notch individual notes.
4. **Range fade.** `fade(h)` is 1 inside `MATCH_RANGE = (40 Hz, 12000 Hz)` and falls linearly in log-frequency to 0 half an octave outside (at about 28.3 Hz and 16.97 kHz): `fade = clamp( 1 - max(2*log2(40/h), 0) - max(2*log2(h/12000), 0), 0, 1 )`.
5. **Level neutrality.** Over the bins where `fade == 1`, compute the **energy-weighted** mean correction, with weights `10^(lt/10)` (the track's own power in each bin). Subtract that mean from every bin. Weighting by the track's power means loud bins dominate, so the correction's net effect on the signal's overall energy is near zero: it rebalances the spectrum rather than turning the whole track up or down.
6. **Cap and fade.** Each bin becomes `clamp( c - mean, -cap, +cap ) * fade(hz)`. In normal use `cap = MATCH_MAX_DB = 3`; `--tone` passes infinity.

Because the clamp comes after the mean is removed, the result is level-neutral only to the extent the cap is not active. With strong corrections the cap breaks exact neutrality slightly.

## Applying the curve

In `main`: `g = 10^(curve/20)` per bin, then `resynth` multiplies every channel's spectrum in every frame by the same `g`. It is a static EQ: one fixed gain per bin, the same in every frame. The run prints the applied correction at 63, 125, 250, 500, 1000, 2000, 4000, 8000 and 16000 Hz.

## Why these limits

- **+/-3 dB cap** (`MATCH_MAX_DB`): this is mastering-style nudging, not rescue EQ. It keeps a mis-matched reference from doing damage.
- **One octave of smoothing**: you are matching tone, not individual frequencies.
- **40 Hz to 12 kHz**: above 12 kHz the 12 references disagree too much to serve as a target, and the exciter owns that region anyway. The half-octave fades mean `--match` also never fights the codec lowpass or the excited band; at 23 kHz the correction is exactly zero.
- **Level neutral**: tonal balance should change without a loudness change you would mistake for improvement.

## `--tone`

`--tone=ref.wav` (or `--tone=pop`) prints the same curve **uncapped** (`cap = infinity`) at 63, 125, 250, 500, 1000, 2000, 3000, 4000, 8000 and 12000 Hz (`TONE_HZ`), without rendering anything. The smoothing, fade and level neutrality still apply. Use it to see how far a track is from a reference and whether the 3 dB cap will bind. `--profile` prints the track's own third-octave spectrum (0 dB = mean of the 125 Hz to 8 kHz points), which is how you build a custom `POP_TARGET`.

## Test

`match_follows_reference_tilt_capped`: a 1.5 dB `tanh` tilt around 1 kHz is recovered to within 0.6 dB between 250 Hz and 4 kHz, a 12 dB tilt never exceeds the 3 dB cap, and the correction is zero at 23 kHz. See [testing.md](testing.md).
