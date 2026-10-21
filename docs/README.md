# sunscreen internals

These pages explain how sunscreen works at the level of the code: the algorithms, the formulas, the constants and the reasons behind them. The user-level overview is in the top-level [README](../README.md); start there if you only want to run the tool.

Everything lives in `src/main.rs`. Names in `code font` (`FLOOR_KNOTS`, `noise_floor`, `match_curve`, ...) are the identifiers in that file, so you can search for them. Every constant quoted here is the value in the code at the time of writing.

## Overview

sunscreen cleans tracks rendered by Suno. It does six separate jobs:

0. **Debuzz**: subtract the faint buzz Suno's decoder leaves on a fixed 240-sample grid (automatic, only when detected).
1. **Denoise** a broadband "haze" that sits under the music and is strongest in the highs. The haze level follows the music, so the noise floor is estimated as a rolling percentile rather than from a fixed profile. A hearing model (the gate) then limits the cut to haze you could actually hear.
2. **Smooth** the haze so it stops fluttering (optional, `--smooth`).
3. **Widen** the stereo image above 10 kHz, where Suno output collapses toward mono (optional, `--widen`).
4. **Rebuild the top end** above the MP3 lowpass (optional, `--excite`), and **nudge the tonal balance** toward a commercial-master target (optional, `--match`).
5. **Master** to a loudness target with a true-peak limiter (optional, `--master`).

All processing is done in the short-time Fourier transform (STFT) domain with the original phase kept. Nothing is normalised or limited except by the opt-in `--master` stage, and nothing is dithered. The output length matches the input sample for sample.

## Signal flow

```
 input.mp3/.wav
      |
      v
 read_audio ......... symphonia decode, gapless, f64 per channel
      |
      v
 debuzz ............. auto (240-sample decoder grid)   debuzz.md
      |
      v
 STFT (Hann, n/hop = 4) ........ stft.md
      |
      v
 denoise ............ always on                        denoise.md
   power_grid (channel-averaged power)
   noise_floor (rolling 15th percentile)
   power-subtraction gain, alpha(f), per-band max cut
   [gate: masking() -> audibility weight]              gate.md
   dB-domain Gaussian blur of the mask
   [onset protection: mask -> unity around hits]
      |                 \
      v                  +--> "Removed Noise.wav" = original - Clean
   "Clean.wav"
      |
      v
 smooth_haze ........ --smooth                         smooth.md
      |
      v
 widen .............. --widen (stereo only)            widen.md
      |
      v
 excite ............. --excite                         excite.md
      |
      v
 match_curve + EQ ... --match                          match.md
      |
      v
 master ............. --master (LUFS + true-peak limit) master.md
      |
      v
 24-bit WAV (write_wav24), one cumulative file per enabled stage
```

The order is fixed: debuzz, denoise, smooth, widen, excite, match, master. Each optional stage takes the previous stage's output (`stage` in `main`), and each writes its own file (`Clean+smooth`, `Clean+smooth+widen`, ...), so you can audition one step at a time. Every written file (Clean and each stage) gets a 5 ms raised-cosine fade at both ends (`edge_fade`), because Suno files often start or stop mid-sound and click; Clean is faded before Removed is taken, so the two still sum to the original exactly.

## Pages

| Page | Contents |
|---|---|
| [stft.md](stft.md) | The transform every stage shares: window, size scaling with sample rate, padding, weighted overlap-add, exact round trip, linked stereo, phase |
| [debuzz.md](debuzz.md) | Suno's decoder grid: how it was found, detection, subtraction, what it costs |
| [denoise.md](denoise.md) | Rolling noise floor, power subtraction, frequency-dependent alpha, per-region maximum cut, mask blur, drum-hit protection, why the floor is rolling |
| [gate.md](gate.md) | The hearing-model gate: Bark bands, spreading, tonality offset, noise-to-mask ratio, dilate-blur-apply ordering |
| [smooth.md](smooth.md) | `--smooth`: pulling haze toward its own moving average |
| [widen.md](widen.md) | `--widen`: restoring high-frequency stereo width without touching the mono sum |
| [excite.md](excite.md) | `--excite`: spectral band replication above the codec cutoff |
| [match.md](match.md) | `--match`: static EQ toward a target long-term spectrum |
| [master.md](master.md) | `--master`: BS.1770 loudness, true peak, the look-ahead limiter |
| [diagnostics.md](diagnostics.md) | Every measurement flag, what it computes, how to read it |
| [findings.md](findings.md) | What was measured, what was built because of it, and what was rejected |
| [testing.md](testing.md) | The test suite and the mutation-check discipline |

## Design rule

Every stage exists because a measurement on real Suno tracks showed the problem. Ideas whose measurement showed no problem (spectral holes, phase smear, soft attacks, repeat averaging) were not built. Every stage has a unit test with a positive control (the effect appears when the feature is on) and a negative control (it does not when the feature is off or when the content should be left alone). See [findings.md](findings.md) and [testing.md](testing.md).

## Running order of `main`

Diagnostic flags (`--flutter`, `--audibility`, `--attacks`, `--phase`, `--width`, `--repeats`, `--modspec`, `--tone=`, `--profile`, `--floors`, `--holes`) are checked in that order right after the parameter lines are printed. The first one present runs, prints, and exits; no audio is written. Otherwise `main` runs the STFT round-trip check, `denoise`, writes `Clean` and `Removed Noise`, prints the report lines (peak, RMS change, per-band attenuation, `env corr`), then runs the optional stages and writes the spectrogram PNG and `Report.txt`.
