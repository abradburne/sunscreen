# `--excite[=dB]`: rebuild the top end above the codec cutoff

Implemented by `excite`. The argument is a level offset in dB relative to the extrapolated slope; the default `EXCITE_DEFAULT_DB` is -3.

## The problem

Suno MP3s stop at about 18 kHz (the WAVs at about 20 kHz). The encoder's lowpass leaves an empty region above. Nothing real can be recovered there, but silence on a spectrogram sounds dull, and a plausible continuation of the track's own spectrum sounds more natural.

## The idea: spectral band replication

This is the trick HE-AAC uses: copy part of the existing spectrum up into the empty region, scaled to a believable level. It is done frame by frame, so the new highs move with the music (cymbal hits make the new air flash, quiet passages stay quiet).

## Algorithm

Input is the previous stage's output. Let `hz_per = sr / n`, `bin(hz) = round(hz / hz_per)`, `top = bin(min(EXCITE_TOP_HZ, sr/2))` clipped to the last bin, with `EXCITE_TOP_HZ = 22000`.

1. **Long-term spectrum.** `l = ltas(...)`: the mean power per bin over all frames and channels, in dB.
2. **Reference level.** The median of `l` over 8 to 12 kHz.
3. **Find the cutoff.** Scan down from `top` to `bin(10000)` and take the highest bin whose long-term level is above `reference - CUTOFF_DROP_DB` (`CUTOFF_DROP_DB = 25`); the cutoff `c` is that bin plus 1. If no bin qualifies, the function returns `None` and the stage is skipped (`excite no codec lowpass found, skipped`).
4. **Full-band guard.** If `c >= top - bin(500)` the file is effectively full-band and the stage is also skipped.
5. **Fit the slope.** Least-squares fit of `l` (dB) against `log2(Hz)` over bins `c/2 .. 0.9c`. This gives a slope in dB per octave and a line through the mean of those points.
6. **Fill start.** `start = 0.9c` (integer arithmetic). Starting inside the codec's own roll-off, at 90 percent of the cutoff, avoids a notch at the cutoff.
7. **Shift.** `k = ceil( (top - 0.8c) / (n/hop) ) * (n/hop)` bins. The source for target bin `f` is bin `f - k`. The rounding to a multiple of `n / hop` (4) means the highest source bin `top - k` is at or below `0.8c`, clear of the sparse, "holey" bins near the cutoff. The reported `shift` is `k * hz_per`.
8. **Gain per target bin** for `f = start .. top`:

   ```
   target  = line(log2(f * hz_per)) + amount_db
   missing = max( 10^(target/10) - 10^(l[f]/10), 0 )
   ramp    = 0.5 - 0.5 * cos( pi * min((f - start) / fade, 1) )        fade = bin(EXCITE_FADE_HZ), EXCITE_FADE_HZ = 500
   gain    = min( sqrt( missing / 10^(l[f - k]/10) ), 1 ) * ramp
   ```

   - `target` continues the track's own fitted high-frequency slope, plus the offset.
   - `missing` is only the power the target needs *over what is already there*, so real content (for example the lowpass roll-off between 0.9c and c) is topped up rather than doubled.
   - `gain` converts that missing power into an amplitude gain on the source bin (the source bin's long-term power is `l[f - k]`).
   - The `min(..., 1)` cap means a source bin is never amplified, only attenuated or copied.
   - `ramp` fades the stage in over 500 Hz above `start`.
9. **Apply.** For every channel, in each frame, `s[start + i] += gain[i] * src[start + i - k]`, where `src` is a copy of the frame's original spectrum (so adding does not feed back).

The gains are static (derived from long-term spectra), while the *copied* content is the music's own instantaneous spectrum, so the added highs have the music's time structure.

## Why the shift is a multiple of 4 bins

Moving a frame forward by `hop` samples rotates the phase of bin `f` by `2 pi f hop / n`. A partial copied up by `k` bins advances by `2 pi (f + k) hop / n` between frames. With `n / hop = 4`, a shift `k` that is a multiple of 4 adds `2 pi (k/4)`, a whole number of turns, so the copy advances in phase exactly as it would have as a real partial at the higher frequency. Partials therefore stay coherent from frame to frame and overlap-add reinforces them instead of smearing them. A shift that is not a multiple of 4 would give inconsistent phase advance and a rough, noisy result.

## Level and what it is not

At the default -3 dB the added content sits 3 dB below the extrapolated slope. Measured on Warm Current, 20 to 22 kHz goes from -87 dB to -16 dB, with a smooth roll-off and no notch at the cutoff.

It is plausible air, not the real lost detail. If cymbals turn metallic, use `--excite=-6` or `-9`.

## Constants

| Constant | Value | Role |
|---|---|---|
| `CUTOFF_DROP_DB` | 25 | how far below the 8-12 kHz median counts as "gone" |
| `EXCITE_TOP_HZ` | 22000 | highest frequency filled (also limited to Nyquist) |
| `EXCITE_FADE_HZ` | 500 | fade-in width above `start` |
| `EXCITE_DEFAULT_DB` | -3 | default offset from the fitted slope |

## Output

`main` prints `excite cutoff N Hz | slope dB/oct | shift Hz | level dB` and a table of the long-term level in 2 kHz steps from 12 kHz upward, before and after. The third panel of the spectrogram PNG is the exciter output (so it shows the stages up to and including excite, not match).

## Test

`excite_fills_above_codec_lowpass_only`: a brick-wall at 16 kHz is detected within 200 Hz, a full-band input is skipped, nothing below 0.9c changes by more than 0.01 dB, and 18 kHz gains more than 40 dB. See [testing.md](testing.md).
