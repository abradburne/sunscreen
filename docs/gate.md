# The hearing-model gate

On by default. `--no-gate` turns it off (`Params.gate = !flags.contains("--no-gate")`). Implemented by `masking` (the model) and the `audible` block inside `denoise` (the weights).

## The problem

Power subtraction cuts every bin that is above the floor by a small margin, whether or not you could hear the haze in that bin. Measured with `--audibility`, 42 to 83 percent of the energy the plain denoiser removed was haze that was already **masked** by louder music in the same time and frequency neighbourhood: inaudible, and exactly where cutting does damage to the music (it takes a little of the music with it). The gate scales the cut by how audible the haze is: nothing where it is masked, the full cut where it is exposed.

## The model: `masking`

Inputs: the channel-averaged `power`, the scaled `noise` floor, and `noise_scale`. Output: a masking threshold per frame and bin, as power per bin.

### Music estimate

For each bin the music estimate is

```
m = max( power - noise / noise_scale, 0 )
```

The unscaled floor `noise / noise_scale` (the plain 15th percentile) is subtracted, so what is left is the part of the bin that is above the haze. Bin 0 is skipped in the sums.

### Bark bands

`bark(hz) = 13 * atan(0.00076 * hz) + 3.5 * atan((hz / 7500)^2)` (Zwicker's critical-band-rate formula). A bin's band is `floor(bark(f))`, so a band is one Bark wide. At 48 kHz that gives 25 bands (0 to 24). `width[b]` is the number of bins in band `b` (at least 1).

Per frame, for each band the code accumulates:
- `e[b]`: sum of `m` (band energy),
- `lg[b]`: sum of `ln(m + 1e-20)`,
- `cnt[b]`: number of bins.

### Tonality from spectral flatness

The spectral flatness measure (SFM) of band `b` is the geometric mean over the arithmetic mean of `m`:

```
sfm_db = 10 * log10( exp(lg/cnt) / (e/cnt) )
alpha  = clamp( sfm_db / -60, 0, 1 )
```

A flat, noise-like band has SFM near 0 dB, so `alpha` is near 0. A peaky, tonal band (a few strong bins, the rest near zero, which the `1e-20` makes very small in the geometric mean) has very negative SFM and `alpha` near 1. `alpha` is the tonality coefficient: 1 means fully tonal.

### Johnston offset

How far below a masker its masking threshold sits depends strongly on whether the masker is tonal or noise-like. Johnston's rule, as used here, is

```
offset_dB = alpha * (14.5 + z) + (1 - alpha) * 5.5
```

with `z` the band index (the Bark number). The masker's effective strength is `e[b] * 10^(-offset/10)`. A tonal masker (alpha = 1) hides things 14.5 dB plus a Bark-dependent amount below it, so it masks little; a noise-like masker (alpha = 0) hides things only 5.5 dB below it, so it masks much more. Practical consequence: a held vocal note masks little, a dense noisy mix masks a lot. If a band has no energy (`cnt == 0` or `e <= 0`) its masker strength is 0.

The earlier idea of a fixed masking offset was dropped: 10 dB gave 43 percent "masked" on Glass Hearts and 25 dB gave 1 percent, so the constant decided the answer. Making it depend on tonality removes that free parameter.

### Schroeder spreading

Each band's masker also masks neighbouring bands. The spreading function for a maskee `dz` Bark above (or below, for negative `dz`) the masker is

```
sf(dz) [dB] = 15.81 + 7.5 * (dz + 0.474) - 17.5 * sqrt(1 + (dz + 0.474)^2)
```

stored as linear power in `sf[masker * nb + maskee]`. The threshold in band `b` is `sum over maskers m of masker[m] * sf[m, b]`, and the per-bin threshold is that sum divided by `width[b]`, i.e. spread evenly over the bins of the band. The same value is assigned to every bin of the band, including bin 0 (which is not summed into any band energy but is assigned the band-0 threshold).

### What is deliberately left out

There is no temporal masking and no threshold in quiet. Both would only add masking, so omitting them makes the gate conservative: it errs toward cutting.

## From threshold to weight

In `denoise`, when `p.gate` is true:

1. **Noise-to-mask ratio** in dB, per frame and bin:
   `nmr = 10 * log10( (noise / noise_scale) / (thr + 1e-30) + 1e-30 )`.
   This compares the unscaled haze estimate to the masking threshold.
2. **Ramp.** `a = clamp( (nmr + GATE_RAMP_DB) / (2 * GATE_RAMP_DB), 0, 1 )` with `GATE_RAMP_DB = 3.0`. So `a = 0` when the haze is at least 3 dB below the mask (masked), `a = 1` when it is at least 3 dB above (exposed), and it rises linearly across that 6 dB window.
3. **Dilate.** `GATE_DILATE = 2`. Each weight is replaced by the maximum over a window of +/-2 frames and +/-2 bins (a 5 by 5 box). Audible regions grow.
4. **Blur** the dilated weights with the same Gaussian as the mask (sigma 2 bins, 1.5 frames).
5. **Apply** after the mask has been blurred: `gdb *= weight` (the attenuation in dB is multiplied by the weight).

## Why the order matters: dilate, blur, then apply after the mask blur

If the weight were applied to the gain *before* the mask blur, the gated-off (zero attenuation) neighbours would be averaged into the blur and dilute the attenuation in bins where the haze is audible. In testing, gating before the blur cost 23 to 36 percent of the audible removal. Applying the weight after the mask blur means audible bins keep their full cut.

The weights are dilated before they are blurred so that the blur's softening falls on the masked side of each boundary: the weight stays near 1 across the audible region and fades into the masked region, rather than fading inside the audible region and reducing its cut.

Result: 99 percent of the removal of audible haze is kept, while wasted removal falls 48 to 80 percent. On Glass Hearts the correlation between the removed signal and the music's envelope goes from 0.549 to 0.419, the overall change from -0.61 dB to -0.46 dB, attacks are unchanged, and the vocal-melody lines mostly vanish from the removed signal's spectrogram. Per-track figures are in [findings.md](findings.md).

## What to listen for

If the haze texture seems to switch on and off as vocal phrases start and stop ("breathing"), compare with `--no-gate`. The model says the haze is masked there; your ears are the final judge.

## Checking it: `--audibility`

`audibility()` runs `denoise` once without and once with the gate and bins the removed energy `power * (1 - g^2)` by `nmr` into three classes: masked (`nmr < -GATE_RAMP_DB`), borderline (within +/-3 dB), audible (`nmr > +GATE_RAMP_DB`). Both rows are shown as a percentage of the **ungated** total, so you can see how much masked removal the gate saved and confirm the audible row hardly moves. See [diagnostics.md](diagnostics.md).

The unit test `gate_spares_masked_haze_keeps_exposed` pins both directions: removal in a masked region must at least halve, removal in an exposed region must stay above 90 percent ([testing.md](testing.md)).
