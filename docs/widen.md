# `--widen[=x]`: restore stereo width in the highs

Implemented by `widen`. `x` is the amount (default 1.0; `--widen=0.5` is half).

## The problem

Every Suno track tested collapses toward mono at the top of the spectrum. Measured with `--width`, above 16 kHz the left and right channels are 0.97 to 0.999 coherent (essentially the same signal), while the mids are clearly wider. WAV downloads do it too, so it comes from Suno's generator rather than the MP3 export. See [findings.md](findings.md) for the table.

## Mid and side

For each bin, `mid = (L + R) / 2` and `side = (L - R) / 2`. The mono fold-down `L + R` equals `2 * mid`, and the width of the image is how large `side` is relative to `mid`. Widening means adding side energy.

## Algorithm

**Needs exactly two channels.** If the input is not stereo `widen` returns `None` and `main` prints `widen needs stereo, skipped`.

1. **Long-term side and mid power per bin.** Over all frames, accumulate `|(L-R)/2|^2` and `|(L+R)/2|^2` into `side[f]` and `mid[f]`.
2. **The track's own reference width.** `ratio(a, b)` is the total side power over total mid power for bins in `[a, b)`. The target is

   ```
   target = ratio(3500, 13000) * amount
   ```

   So the highs are widened to the width the *same track* already has in 3.5 to 13 kHz, times `amount`. No external width target is imposed.
3. **A ramp.** Widening is not applied below `WIDEN_FROM_HZ = 10000`. The ramp rises as a raised cosine from 10 kHz to `ramp_end = WIDEN_FROM_HZ + 2000 = 12 kHz` and stays at 1 above. The 7 to 10 kHz region is left narrow on purpose, to keep sibilance ("s" sounds) centred.
4. **Per-bin gain on the delayed mid.** For bins at or above 10 kHz with non-zero mid:

   ```
   g(f) = min( sqrt( max(target - side[f]/mid[f], 0) * ramp ), WIDEN_MAX_G )
   ```

   The needed side power is `(target - current ratio) * mid`; delivering it as `g * mid` in amplitude gives `g^2 = (target - ratio)`. The cap `WIDEN_MAX_G = 0.5` means the added side is at most -6 dB relative to the mid. Bins already wider than the target get `g = 0`.
5. **The delay.** The added signal is mid delayed by `WIDEN_DELAY_MS = 5` ms. `d = 0.005 * sr` samples and the delay is applied in the frequency domain as the phase factor `rot[f] = exp(-j 2 pi f d / n)` per frame. (This is a circular shift of the windowed frame; the window tapers the frame ends, so it behaves as a delay.) A 5 ms delay makes the added side decorrelated from the mid, which the ear reads as width at these frequencies. Its comb spacing (about 200 Hz) is far finer than the ear resolves above 10 kHz.
6. **Apply with opposite signs.** For channel `c`: `s[f] += (s[f] + o[f]) / 2 * rot[f] * (sign * g[f])`, where `o` is the other channel's spectrum in the same frame and `sign = +1` for left, `-1` for right. Equivalently:

   ```
   L' = L + g * delay(mid)
   R' = R - g * delay(mid)
   ```

7. **Report.** `after` is re-measured on the output; `main` prints the target side/mid in dB and the 16 kHz and above value before and after.

## L+R is unchanged

`L' + R' = L + R + g*delay(mid) - g*delay(mid) = L + R`. Only side changes, so the mono fold-down is mathematically identical. Because the two additions are computed independently in floating point, the identity holds to rounding error rather than bit for bit; the unit test checks it to better than 1e-9 on a signal of amplitude 0.1. In practice this means the stage cannot change how the track sounds when summed to mono, apart from floating-point noise.

## Constants

| Constant | Value | Role |
|---|---|---|
| `WIDEN_FROM_HZ` | 10000 | start of widening; ramp reaches full effect 2 kHz higher |
| `WIDEN_MAX_G` | 0.5 | cap on the amplitude gain of delayed mid into side |
| `WIDEN_DELAY_MS` | 5 | decorrelation delay |

## Measured effect

Glass Hearts, above 16 kHz: side relative to mid goes from -18.0 dB to -8.6 dB (the target was -7.9 dB); coherence from 0.97 to 0.77. Nothing below 10 kHz changes.

## Tuning

- Sounds phasey on headphones: `--widen=0.5`.
- A gap between the narrow 7 to 10 kHz and the widened region: lower `WIDEN_FROM_HZ`.
- Delayed-mid widening can sound phasey on sustained material; the cap and the delay are the two knobs.

## Test

`widen_restores_high_width_mono_safe`. See [testing.md](testing.md).
