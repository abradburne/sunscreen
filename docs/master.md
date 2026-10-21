# `--master[=LUFS]`: loudness and true-peak limiting

The last stage, and the only one that changes overall level. Every earlier stage keeps the level put and reports clipping instead of preventing it; `--master` is the opt-in exception that sets a loudness target and guarantees a peak ceiling.

## Problem

Suno tracks arrive at whatever loudness the model happened to produce. Four tracks measured -11.6, -13.5, -13.7 and -15.6 LUFS, a 4 dB spread, so an album of them jumps in level from song to song. Streaming services normalise to about -14 LUFS, and their encoders need headroom: a file whose *true* peak (the peak between samples, after reconstruction) touches 0 dBFS can clip once it is re-encoded.

## Loudness measurement: `lufs`

Integrated loudness per ITU-R BS.1770-4, the standard behind every LUFS meter:

1. **K-weighting** (`k_weight`): two biquads per channel.
   - A high shelf (+4 dB above about 1.7 kHz) modelling the head's acoustic effect: `f0 = 1681.97 Hz`, `G = 4.0 dB`, `Q = 0.7072`.
   - A high pass at `f0 = 38.1 Hz`, `Q = 0.5003`, which removes rumble the ear barely hears.
   - The standard publishes coefficients only for 48 kHz. These come from the analog prototype through the bilinear transform (`K = tan(pi f0 / fs)`), as libebur128 does, so 44.1 kHz and other rates are exact too. At 48 kHz they reproduce the standard's table.
2. **Blocks**: mean square of each K-weighted channel over 400 ms blocks every 100 ms (75% overlap). The channel powers are summed (weight 1 for left and right) into `z`.
3. **Loudness of a block**: `-0.691 + 10 log10(z)`.
4. **Gating**: drop blocks below -70 LUFS (absolute gate), then drop blocks more than 10 LU below the mean of what remains (relative gate). The integrated loudness is `-0.691 + 10 log10(mean z of surviving blocks)`.

Validation: a stereo 1 kHz sine of amplitude `A` must read `20 log10(A)` LUFS (K-weighting is +0.69 dB at 1 kHz, cancelling the -0.691 offset). On the real tracks, readings match ffmpeg's independent `ebur128` filter to the displayed 0.1 dB.

## True peak: `true_peak_env`

Samples can all sit below a level while the reconstructed waveform peaks above it between them; a sine at a quarter of the sample rate with a 45 degree phase has every sample at 0.707 of its true peak (-3 dB). BS.1770 measures true peak on a 4x oversampled signal.

`true_peak_env` returns, per sample, the largest absolute value at the sample itself and at the three points a quarter, half and three quarters of the way to the next sample, across all channels (linked). Each in-between point is a windowed-sinc interpolation: `sum x[n+k] * sinc(k - p/4) * w`, 12 taps per phase (`TP_HALF_TAPS = 6` each side, as in the standard's 48-tap reference filter), raised-cosine window.

## The limiter: `limit`

Offline (the whole file is in memory), linked across channels, so it never shifts the stereo image.

1. **Required gain** per sample: `want[n] = min(1, ceiling / tp[n])`.
2. **Look-ahead**: `h = sliding_min(want, l)`, the minimum over `[n - l, n + l]` with `l = LIMIT_ATTACK_MS` (2 ms). A monotonic deque makes it O(n).
3. **Attack smoothing**: box average of `h` over `[n - l/2, n + l/2]`. Every sample in that box saw sample `n` inside its own min window, so each is at most `want[n]`, and therefore so is the average. The gain never exceeds what a sample needs, yet it ramps down smoothly *before* the peak instead of snapping at it.
4. **Release**: a one-pole rise back toward that smoothed ceiling, time constant `LIMIT_RELEASE_MS` (80 ms). It drops immediately when the ceiling drops, so it never overshoots.

The function returns the output and the maximum gain reduction in dB.

## Putting it together: `master`

1. Measure the input loudness.
2. Apply the gain `target - input` and limit to the ceiling `TRUE_PEAK_DBTP` (-1 dBTP).
3. Limiting removes a little loudness, and gain changing between samples can make the interpolated peak overshoot slightly. So measure again and iterate, up to 6 times:
   - re-aim the gain by the measured loudness shortfall;
   - tighten the limiter ceiling by any measured true-peak overshoot.
4. Stop when the output is within 0.05 LU of the target and at or under the ceiling.

The run prints `master  -13.7 LUFS in | gain +5.3 dB | limiter max 4.4 dB | out -9.0 LUFS, true peak -1.0 dBTP`. Above 6 dB of limiting it suggests a quieter target: a track squeezed that hard is losing punch.

## Measured

| Track | In | Out at -14 | Limiting |
|---|---|---|---|
| Glass Hearts (full chain) | -13.7 LUFS | -14.0 LUFS, -2.2 dBTP | none |
| Warm Current | -13.5 | -14.0, -2.4 dBTP | none |
| French Exit | -15.6 | -14.0, -2.6 dBTP | none |
| What About Me | -11.6 | -14.0, -2.0 dBTP | none |

At -14 LUFS the limiter rarely has to act on Suno material; the stage is mainly a level match. Pushed to `--master=-9`, Glass Hearts needed +5.3 dB of gain and 4.4 dB of peak limiting, and ffmpeg confirmed -9.0 LUFS and -1.0 dBTP. `--attacks` showed the pre-hit energy unchanged (-15.5 to -15.8 dB), so the limiter does not dull the drums at that setting.

## Constants

| Constant | Value | Meaning |
|---|---|---|
| `MASTER_DEFAULT_LUFS` | -14 | target for bare `--master` (the streaming norm) |
| `TRUE_PEAK_DBTP` | -1 | true-peak ceiling, leaves headroom for lossy re-encoding |
| `LIMIT_ATTACK_MS` | 2 | look-ahead each side of a peak |
| `LIMIT_RELEASE_MS` | 80 | release time constant |
| `TP_HALF_TAPS` | 6 | interpolation taps each side, per oversampling phase |

## Tests

- `lufs_reads_reference_sine`: a stereo 1 kHz sine at amplitude 0.1 reads -20 LUFS within 0.1 at 48 and 44.1 kHz. Fails without K-weighting.
- `true_peak_sees_between_samples`: the fs/4, 45 degree sine has a sample peak of -3.01 dB and a measured true peak within 0.2 dB of 0. Fails without interpolation.
- `master_hits_loudness_and_ceiling`: quiet noise with sharp hits every 0.5 s ends within 0.1 LU of -14 LUFS and at or under -1 dBTP, with more than 1 dB of limiting. Fails with the limiter disabled, and fails without the loudness re-aim.
