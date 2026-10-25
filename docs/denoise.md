# Denoise

`denoise(p, x, sr)` is the always-on core. It returns the clean signal, the final gain mask (`[frames][bins]`, linear) and the STFT geometry. The "Removed Noise" file is simply `original - clean`, sample for sample.

## The problem

Suno renders sit on a broadband haze: static that is strongest in the highs and that, unlike tape hiss, is not constant. It is also **not** a fixed floor: it rises and falls with the music (see [Why a rolling floor](#why-a-rolling-floor) and [findings.md](findings.md)). The denoiser therefore has to estimate a floor that moves with the track, and has to be gentle enough not to eat the music that sits on top of it.

## Pipeline

Parameters come from `params_for` (see [stft.md](stft.md)):

| Field | Value | Meaning |
|---|---|---|
| `block_s` | 1.0 | percentile block length, seconds |
| `pctl` | 0.15 | per-block percentile |
| `noise_scale` | 0.8 | scale on the floor ("err on the gentle side") |
| `alpha_lo`, `alpha_hi` | 1.0, 2.0 | oversubtraction factor below / above `ALPHA_RAMP` |
| `sigma_f`, `sigma_t` | 2.0 bins, 1.5 frames | mask blur widths |
| `protect_onsets` | true | drum-hit protection |
| `gate` | false in `params_for`, set to `!--no-gate` in `main` | hearing-model gate |

Note that `params_for` itself has `gate: false`; the CLI turns it on unless `--no-gate` is given. The unit tests call `params_for` directly, so they run ungated unless they say otherwise.

Steps, in the order `denoise` runs them:

1. `power_grid`: channel-averaged power per frame and bin.
2. `noise_floor`: the rolling floor, scaled by `noise_scale`.
3. Per-bin gain in dB from power subtraction, with a per-bin minimum (the maximum cut).
4. If gating: compute the audibility weight (see [gate.md](gate.md)).
5. Gaussian blur of the dB gain mask.
6. If gating: multiply the blurred mask by the audibility weight.
7. Onset protection: pull the mask toward unity around detected hits.
8. Apply the final gain to every channel via `resynth`.

## 1. The rolling noise floor: `noise_floor`

Returns an estimate of the haze power in every bin and frame, already multiplied by `noise_scale`.

**Blocks.** `bf = round(block_s * sr / hop)` frames per block (94 frames, about 1 s, at 48 kHz). `nblocks = ceil(frames / bf)`; the last block may be shorter.

**Percentile per block and bin.** For each block and each frequency bin, take that bin's power over the block's frames and pick the element at index `round((len - 1) * pctl)` of the sorted values (via `select_nth_unstable_by`), i.e. the 15th percentile. Store `ln(value + 1e-20)`. The percentile rather than the minimum or the mean keeps it from locking onto a single quiet frame or being pulled up by the music.

**Time smoothing in the log domain.** The block values are smoothed with the weights `[0.5, 1, 1, 1, 0.5]` over blocks `b-2 .. b+2`, renormalised by the weights actually present at the file edges. That is a roughly 4-second-wide smoothing of 1 s blocks. Averaging log power (a geometric mean) keeps one loud block from dominating.

**Interpolation to frames.** Block `b`'s centre is placed at frame `b*bf + bf/2`. For frame `t` the position is `(t - bf/2) / bf`, clamped to `[0, nblocks-1]`, and the floor is linearly interpolated (in the log domain) between the two neighbouring smoothed blocks. The floor therefore varies smoothly frame by frame and has no staircase at block borders.

**Frequency blur.** Still in the log domain, `blur(out, frames, bins, 1.0, 0.0001)` smooths across frequency with a Gaussian of sigma 1 bin. The time sigma of 0.0001 gives a kernel whose off-centre weights underflow to zero, so it is effectively a frequency-only blur.

**Scale.** Finally `exp(...) * noise_scale`. The 0.8 factor deliberately underestimates the floor so the cut stays gentle. Other stages that need the unscaled estimate (the gate, smoothing, flutter) divide by `noise_scale` again.

## 2. Power-subtraction gain

For every frame and bin:

```
g = sqrt( max( 1 - alpha(f) * noise / (power + 1e-20), 0 ) )
g = max( g, floor_g(f) )
gdb = 20 * log10(g)
```

This is classic power (spectral) subtraction expressed as a gain: take `alpha * noise` away from the bin's power and keep the square root of what remains as the magnitude gain. Where the bin is at the floor, `g` goes toward 0 and is held at the per-region limit `floor_g`. Where the bin is well above the floor, `g` is near 1.

The gain is stored in dB (floored, so the later blur never sees minus infinity).

### Frequency-dependent alpha: `alpha`, `ALPHA_RAMP`

`alpha(f) = alpha_lo + (alpha_hi - alpha_lo) * log_ramp(f, 3500, 7000)`. That is 1 below 3.5 kHz, 2 above 7 kHz, and a raised-cosine crossfade on a log-frequency axis between. `ALPHA_RAMP = (3500.0, 7000.0)`. Above 7 kHz the algorithm subtracts twice the estimated haze power; in the mids and below it subtracts exactly the estimate. The haze is strongest in the highs, so the highs get the more aggressive setting.

`log_ramp(f, a, b)` is the helper used throughout: 0 below `a`, 1 above `b`, and `0.5 - 0.5 cos(pi t)` with `t = (ln f - ln a) / (ln b - ln a)` in between. A raised cosine has zero slope at both ends so segments join without a visible corner.

### Maximum cut per region: `floor_db`, `FLOOR_KNOTS`

```
FLOOR_KNOTS = [(300, 1.0), (450, 3.5), (3500, 3.5), (7000, 7.0)]   (Hz, dB)
```

`floor_db(f)` starts at the first knot's dB (1.0) and, for each consecutive pair of knots, adds `(db1 - db0) * log_ramp(f, f0, f1)`. The result:

| Frequency | Maximum attenuation |
|---|---|
| below 300 Hz | 1 dB |
| 300 to 450 Hz | crossfade 1 to 3.5 dB |
| 450 Hz to 3.5 kHz | 3.5 dB |
| 3.5 to 7 kHz | crossfade 3.5 to 7 dB |
| above 7 kHz | 7 dB |

`floor_g(f) = 10^(-floor_db(f)/20)` is the smallest gain allowed in a bin. It is applied twice: when the gain is first computed and again as a clamp after the blur. This makes the denoiser a bounded, region-aware "reduce, don't remove": it protects the low end (where the floor is easily confused with bass and the haze is weak) and lets the highs be cut hardest. Overall level change on real tracks is about -0.2 to -0.7 dB.

## 3. Mask blur

`blur(gdb, frames, bins, sigma_f = 2.0, sigma_t = 1.5)` is a separable Gaussian on the dB gain grid: a frequency pass, then a time pass. The kernel radius is `ceil(3 sigma)`, weights `exp(-i^2 / (2 sigma^2))` normalised to sum to 1, and the edges clamp to the nearest bin or frame.

Power subtraction on its own makes a gain that is jagged in time and frequency; isolated bins that happen to dip pass through while neighbours are cut, which is heard as a warbling "musical noise". Averaging the gain in dB over about 2 bins and 1.5 frames removes the isolated spikes. Working in dB (not linear gain) means the blur averages attenuation in the perceptual domain.

If the gate is on, the blurred dB mask is multiplied by the (separately blurred) audibility weight here, after the blur, not before; see [gate.md](gate.md) for why that order matters.

The final linear gain is `clamp(10^(gdb/20), floor_g(f), 1)`.

## 4. Drum-hit protection: `onsets`, `onset_weights`

Time blurring drags attenuation from the quiet frames before a hit onto the hit's attack and dulls it. Protection avoids that.

### Detecting hits: `onsets`

1. Work in log power of the channel-averaged grid: `lp = ln(power + 1e-10)`.
2. Spectral flux per frame `t`: the mean over bins from 150 Hz up of `max(lp[t] - lp[t-1], 0)`. (Positive log changes only: energy that appeared.) Frame 0 has flux 0.
3. Local median of the flux over a window of 12 frames each side (25 frames).
4. A frame `t` is an onset if all of these hold:
   - `flux[t] > ONSET_K * median` with `ONSET_K = 2.0`;
   - `flux[t] > 0.1` (an absolute floor so silence-to-noise jitter does not count);
   - `flux[t] >= flux[t-1]` and `flux[t] >= flux[t+1]` (a peak).

   The loop runs over `t = 1 .. frames-2`.

Raising `ONSET_K` means fewer hits are detected and protected. The 12-frame radius is in frames, not seconds, so at 48 kHz it is about 128 ms each side; at other rates it scales with the hop. On real tracks this finds about 3 to 5 hits per second.

### Weights: `onset_weights`

For each onset frame `t` the mask is pulled toward unity with weights on frames `t-2, t-1, t, t+1` of `0.5, 1, 1, 0.5` (the maximum is kept where hits overlap). A frame with weight `w` gets

```
gain = g + w * (1 - g)
```

so `w = 1` forces unity gain (no cut) and `w = 0.5` halves the cut in linear gain. The four-frame span matches the four-hop length of the analysis window: it covers every frame whose window contains the attack. That is about 40 ms at 48 kHz. The hit itself masks any haze let through for those few frames.

The count reported in the run (`onsets N protected (x/s)`) is `(number of frames with weight exactly 1) / 2`, since each isolated hit has two full-weight frames.

## Why a rolling floor

The tempting approach is a single noise profile for the whole song, measured from the "quietest" frames, as is done in tape-hiss tools. It fails on Suno for two reasons.

1. **A full mix has no noise-only moments.** There is always music. The quietest frames of the track are quiet music, not silence, so the profile ends up being a spectrum of quiet music and cuts music out of loud sections.
2. **The haze is not constant.** The `--floors` measurement groups 1-second blocks by loudness and compares the floor (15th percentile) to the block level. For additive, constant hiss the quietest blocks pin at the same absolute floor and the floor-to-level ratio rises toward 0 dB. On Suno tracks the quietest blocks sit further below the floor, not at it, meaning the haze falls with the music. A fixed profile is therefore wrong in both directions, too loud for quiet passages and too quiet for loud ones. Details and numbers are in [findings.md](findings.md).

A rolling percentile tracks both effects: each second of music gets its own floor, estimated from the quietest 15 percent of that second's frames in that bin. Smoothing over about 4 seconds stops one unusual block from distorting the cut.

A faster floor was tried: block lengths of 1.0, 0.5 and 0.25 s on Glass Hearts gave RMS changes of -0.46, -0.56 and -0.65 dB and `env corr` of 0.419, 0.435 and 0.452. Neither metric can referee the choice (a floor that follows the music faster will naturally correlate more with it), so it stayed at 1.0 s.

## Strength: `haze_strength`

Before the gain is computed, `flutter_grid` measures the haze's flutter on the same power grid and floor (see `--flutter` in [diagnostics.md](diagnostics.md)), and the 7-12 kHz value sets the strength: `clamp(flutter / STRENGTH_FULL_FLUTTER_DB, STRENGTH_MIN, 1)` with 8.5 dB and 0.4. The blurred, gated gain in dB is multiplied by it. No haze bins at all (NaN) means full strength. `--strength=x` overrides it; tests that use `params_for` run at 1.

Why: Suno v5's haze flutters 9 to 17 dB in that band. Suno v6 (2026-10) renders the highs as a steady wash: 3.9 to 4.6 dB, below human masters (5.5 to 10). The floor estimate reads that wash as haze, and at full strength the removed signal sat 13 to 15 dB under Clean in the highs, against 19 to 25 dB on v5 and 17 to 20 dB on a human master; by ear Clean was slightly duller. At v6's auto strength (0.46 to 0.54) the 7-12 kHz cut drops from about -1.0 to -0.3 to -0.55 dB, about what a human master loses (-0.59).

## What `Report.txt` tells you

- `strength`: the cleaning strength and the 7-12 kHz haze flutter it came from.
- `onsets`: number of protected hits and rate.
- `rms change`: overall level change of Clean versus original.
- `band` lines: energy-weighted attenuation per `BANDS` region, `10 log10( sum(g^2 P) / sum(P) )` using the actual applied gain and the original per-channel power.
- `env corr`: Pearson correlation of the 1-second RMS envelopes of the original and of the removed signal. A high value means the removed signal moves with the music, which means music is being removed along with haze. The gate lowers it (0.549 to 0.419 on Glass Hearts).
