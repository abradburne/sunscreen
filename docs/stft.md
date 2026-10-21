# The STFT framework

Every stage works on the same short-time Fourier transform. It is implemented once, in the `Stft` struct, and reused by the denoiser, the gate, smoothing, widening, the exciter, the EQ and most diagnostics.

## Frame size scales with sample rate

`params_for(sr)` picks the frame length `n`:

```
n   = max( round(2048 * sr / 48000 / 4) * 4, 256 )
hop = n / 4
```

So the design point is 2048 / 512 at 48 kHz, and other rates are scaled proportionally so a frame always spans about 42.7 ms and the hop about 10.7 ms. Rounding to a multiple of 4 keeps `hop` an integer and `n / hop` exactly 4.

| Sample rate | n | hop | bin spacing (sr / n) |
|---|---|---|---|
| 48000 | 2048 | 512 | 23.44 Hz |
| 44100 | 1880 | 470 | 23.46 Hz |
| 96000 | 4096 | 1024 | 23.44 Hz |
| 32000 | 1364 | 341 | 23.46 Hz |
| 22050 | 940 | 235 | 23.45 Hz |

The 256 floor only matters for very low sample rates. Because the frame length is not a power of two at some rates, `realfft` is planned for arbitrary `n`.

The same `Params` struct carries the other denoiser settings; see [denoise.md](denoise.md).

## Window

`hann(n)` returns the periodic Hann window `0.5 - 0.5 * cos(2 pi k / n)` for `k = 0..n-1` (the denominator is `n`, not `n - 1`). With a hop of `n / 4` the squared window sums to a constant (1.5) in the interior.

The window is applied twice, once on analysis (`Stft::frame`) and once on synthesis (`Stft::resynth`). That is weighted overlap-add (WOLA). Applying it on synthesis too means any modification of a frame's spectrum is tapered at the frame edges, which hides frame-to-frame discontinuities in the processed signal.

## Padding and frame placement

`Stft::new(n, hop, len)` sets:

```
pad    = n - hop
frames = (pad + len - 1) / hop + 1          (integer division)
```

Frame `t` covers padded samples `[t*hop, t*hop + n)`, which are original samples `[t*hop - pad, t*hop - pad + n)`. Samples outside the signal read as zero. The `n - hop` samples of padding in front mean the first original sample is already covered by `n / hop` = 4 frames, the same as every interior sample. The frame count is enough for the last frames to cover the end the same way.

The consequence: no sample is treated differently because it is near the start or end of the file. There is no edge special case anywhere downstream.

## Analysis

`Stft::frame(x, t, out)` fills a zeroed buffer with `x[i] * win[k]` for the in-range samples and runs the real forward FFT, giving `n/2 + 1` complex bins (`bins()`).

Several stages then need power. `power_grid(st, x)` computes `|X|^2` per frame and bin, **averaged over channels**, stored as a flat `[frames][bins]` `f32` array. This single channel-averaged power grid is what the noise floor, the gate, the onset detector and the smoother all read.

## Synthesis: weighted overlap-add

`Stft::resynth(x, edit)` does, for every frame `t`:

1. analyse frame `t` of `x`;
2. call `edit(t, &mut spectrum)` so the caller can change the bins (apply a gain, add a copy, do nothing);
3. force the imaginary part of the DC bin and the Nyquist bin to zero (a real signal needs this and the inverse FFT requires it);
4. inverse FFT, divide by `n` (realfft is unnormalised), multiply by the window, and add into an accumulator;
5. add `win^2` into a second accumulator, `wsum`.

At the end each sample is `acc / wsum` (or 0 where `wsum` is below 1e-12). Dividing by the accumulated squared window makes the reconstruction exact for an unmodified spectrum, whatever the window or hop, so correctness does not depend on the sum being exactly the constant 1.5.

## Why the round trip is exact

With `edit` a no-op, each frame returns exactly the windowed input; multiplying by the window again and dividing by the sum of squared windows gives back the input up to floating-point rounding. All of this is `f64`, so the error is around 1e-16. `main` measures it on channel 0 every run and prints it:

```
stft rt    max |err| = ...e-16
```

and the unit test `stft_round_trip_is_exact` asserts it is below 1e-12 on 10007 samples of pseudo-random data. If this line ever shows more than ~1e-12, the framework is broken and every downstream number is suspect.

## Linked stereo

The denoiser computes **one** gain mask from the channel-averaged power and multiplies it onto **every** channel's spectrum. Smoothing and the exciter likewise use one gain per bin for all channels; the EQ in `--match` uses one curve for all channels. Because the same real, positive gain scales left and right identically in every bin and frame, interchannel level differences and interchannel phase differences are preserved exactly. The stereo image cannot shift as a side effect of cleaning. (`--widen` is the one stage that deliberately changes the image, and it does so by editing only the side signal; see [widen.md](widen.md).)

Mono input follows the same code path with one channel.

## Phase is preserved

Gains are real and non-negative, applied by `spectrum[f] *= gain`. A real positive scale factor changes magnitude and leaves the argument (phase) of each bin untouched. sunscreen never estimates or replaces phase; the original phase of the music, and of the haze that remains, is kept. The only stages that move spectral content between bins are `excite` (copies complex values up by a multiple of 4 bins so the copied partials stay phase-coherent, see [excite.md](excite.md)) and `widen` (adds a phase-rotated copy of mid into side, see [widen.md](widen.md)).

## Cost

Everything is computed on whole-file grids (`frames x bins` of `f32` for power, noise, gain and gate weight), not streamed. That is why a 5-minute track needs only seconds, and also why memory grows linearly with track length.
