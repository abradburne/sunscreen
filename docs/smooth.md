# `--smooth[=ms]`: steady out the static

Implemented by `smooth_haze`. Default window `SMOOTH_DEFAULT_MS = 85`.

## The problem

The haze does not just sit there, it flutters: its level in a given frequency bin jumps around from frame to frame. That is the "off-station radio" quality. Plain denoising makes it worse. Power subtraction cuts the quiet dips harder than the peaks (a dip is closer to the floor, so more of its power is subtracted), which deepens the level variation. This is the classic "musical noise" effect. On Glass Hearts the denoiser raised the flutter measure by 1 to 1.2 dB.

## The idea

AAC's perceptual noise substitution rests on the fact that in noise-like bands the ear hears the *level* of the noise, not its exact waveform. So wherever a bin is dominated by haze, sunscreen does not need to preserve its exact frame-to-frame level; it can pull each bin's level toward that bin's own short moving average. This flattens the flutter while keeping the original phase (no random-phase replacement), and one gain is used for all channels so the stereo image cannot move.

## Algorithm

Input is the previous stage's output (`stage`, which is `Clean` unless nothing else ran first). The function recomputes everything from that signal: `power_grid`, then `noise_floor` on it. So the floor used here is the floor of the already-denoised audio, not of the original.

1. **Half-width of the averaging window:** `r = max( round(smooth_ms / 1000 * sr / hop / 2), 1 )` frames. At 85 ms and 48 kHz, `r = 4`, a window of `2r + 1 = 9` frames (about 96 ms of hop positions; the frames overlap 75 percent, so neighbouring readings are correlated).
2. **Moving average per bin:** for each bin `f`, the centred average of the power over frames `t-r .. t+r` (truncated at the file edges, divided by the number actually present), computed with a running (prefix) sum.
3. **Haze weight:** with `floor = noise / noise_scale` (the unscaled floor) and `snr = power / (floor + 1e-30)`:

   ```
   w = clamp( (SMOOTH_SNR - snr) / (SMOOTH_SNR - 1), 0, 1 )      SMOOTH_SNR = 4
   ```

   `w = 1` when the bin is at or below the floor, falling linearly to `w = 0` at 4 times the floor and above. Frames of music and drum hits (more than 4x the floor) are left alone; the transition between is partial.
4. **Gain:** `gain = (avg / power)^(w / 2)`, then clamped to `[1/lim, lim]` with `lim = 10^(SMOOTH_LIMIT_DB / 20)` and `SMOOTH_LIMIT_DB = 6`. The exponent `w/2` converts a power ratio to an amplitude gain scaled by the haze weight. Dips (power below the average) get a gain above 1, peaks a gain below 1, and neither can move by more than 6 dB.
5. **Apply:** the gain grid is multiplied onto every channel's spectrum in `resynth`. Original phase is kept.

## Constants

| Constant | Value | Effect |
|---|---|---|
| `SMOOTH_DEFAULT_MS` | 85 | default averaging window |
| `SMOOTH_SNR` | 4.0 | multiple of the floor above which a bin is left alone |
| `SMOOTH_LIMIT_DB` | 6.0 | largest boost or cut applied to a bin |

## Tuning

- Haze sounds watery or underwater: shorten the window, e.g. `--smooth=40`.
- Grit remains: lengthen it, e.g. `--smooth=150`.
- Other direction: raise `SMOOTH_SNR` to let smoothing act on louder bins; lower `SMOOTH_LIMIT_DB` to make it milder.

## Measured effect

Using `--flutter` (see [diagnostics.md](diagnostics.md)): on Glass Hearts the flutter of the haze in 3.5 to 7 kHz and 7 to 12 kHz goes from 5.93 / 7.39 dB (original) to 6.97 / 8.60 dB (Clean) to 5.19 / 6.45 dB (smoothed), i.e. below the original. Overall level changes by about +0.1 dB. Absolute flutter values are not comparable between tracks, because rhythmic music is also level variation; only stage-to-stage comparisons on the same track are fair.

## Test

`smooth_haze_flattens_flutter_keeps_hits`: hiss modulated at 6 Hz plus loud 1 kHz notes. Flutter in the two upper-high bands must fall by more than 1 dB (summed), and the notes must change by less than 0.2 dB. See [testing.md](testing.md).
