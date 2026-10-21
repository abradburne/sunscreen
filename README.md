# sunscreen ☀️

*Protection from Suno haze.*

Cleans up tracks downloaded from Suno. It reduces the broadband "haze" (static that sounds like an off-station radio), restores width in the high frequencies, and rebuilds the top end that the MP3 cut off. It's a single Rust binary that reads MP3 or WAV and writes 24-bit WAV.

Every stage is there because a measurement on real Suno tracks showed the problem exists. Ideas that measured as non-problems (spectral holes, phase smear, soft attacks, repeat averaging) were dropped.

## Quick start

**Prebuilt binaries:** grab the archive for your platform from the GitHub Releases page (macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64), unpack it, and put `sunscreen` on your PATH. On macOS, clear the download quarantine once: `xattr -d com.apple.quarantine sunscreen`.

**From source:**

```sh
# install Rust once (skip if `cargo --version` works)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# build and put `sunscreen` on your PATH (~/.cargo/bin)
cargo install --path .

# recommended: everything on
sunscreen "My Song.mp3" out --gate --smooth --widen --excite
```

A 5-minute track takes about 10 seconds on a laptop.

Do a whole folder:

```sh
for f in *.mp3; do sunscreen "$f" cleaned "${f%.mp3}" --gate --smooth --widen --excite; done
```

## Usage

```
sunscreen <in.wav|in.mp3> [out_dir] [title] [--gate] [--smooth[=ms]] [--widen[=x]] [--excite[=dB]]
```

| Argument | Default | Meaning |
|---|---|---|
| `in` | — | MP3 or WAV (16/24/32-bit int or float), any sample rate, mono or stereo. For FLAC/M4A, convert first: `ffmpeg -i song.m4a song.wav` |
| `out_dir` | the input's folder | Where outputs go (created if missing) |
| `title` | the input's file name | Prefix for output file names |

The input length and sample alignment are kept exactly. Nothing is normalised, limited or EQ'd; if a stage pushes a sample past full scale, the run prints `CLIPPING` instead of turning the track down.

## Output files

Each run always writes:

| File | What it is |
|---|---|
| `<title> - Clean.wav` | The denoised track (with `--gate`, the gated denoise) |
| `<title> - Removed Noise.wav` | Original minus Clean, sample for sample. Listen to it to hear what was taken. If you hear drums or vocals in it, the denoise is taking music. |
| `<title> - Spectrogram.png` | The densest 10 s: original, removed, and (with `--excite`) the final result, each panel normalised separately |

Each optional stage then writes a **cumulative** file, so you can A/B one step at a time:

```
Clean → Clean+smooth → Clean+smooth+widen → Clean+smooth+widen+excite
```

The stages always run in that order. A file name lists only the stages you turned on, so `--widen --excite` gives `Clean+widen` and `Clean+widen+excite`.

The run also prints a report: the parameters used, the STFT round-trip error (should be about 1e-16), peak levels, RMS change, attenuation per band, and how much the removed signal follows the music (`env corr`).

## What each stage does, and why

### Denoise (always on)

**Problem:** Suno renders sit on a broadband haze that's strongest in the highs.

**How:**
- **Noise floor:** a *rolling* estimate. Per 1-second block, take the 15th-percentile power in each frequency bin, smooth that across about 4 s, and scale it by 0.8 to stay gentle. A single global profile from the "quietest" frames doesn't work, because a full mix has no noise-only moments, so that profile ends up being quiet music.
- **Gain:** power subtraction, `g = sqrt(max(1 − α·noise/power, 0))`. α is 1 below 3.5 kHz and ramps to 2 by 7 kHz.
- **Maximum cut per band:** 1 dB below 300 Hz, 3.5 dB from 450 Hz to 3.5 kHz, 7 dB above 7 kHz. The steps between are smooth crossfades.
- **Smoothing:** the gain mask is blurred across time and frequency to prevent the "musical noise" warble.
- **Stereo:** one mask for both channels, so the stereo image can't shift. The original phase is kept.
- **Drum-hit protection:** detected hits (about 3–5 per second) get no cut for about 40 ms. Without this, the blur pulls attenuation from the quiet frames before a hit onto the attack and dulls it. The hit itself covers any haze let through.

**Expect:** about −0.2 to −0.7 dB overall, more in the highs. It's subtle by design.

### `--gate` — only cut haze you can hear

**Problem:** 42–83% of what the denoiser removed (by energy) was haze already *masked* by louder music: inaudible, and exactly where cutting damages the music.

**How:** a hearing model (Johnston-style) estimates, in each frame and critical band, the level below which the music hides other sound. It sets that level from whether the music is tonal (a held vocal note hides little) or noise-like (hides more). The cut is then scaled by how audible the haze is: none where it's masked, full where it's exposed.

**Measured:** wasted removal drops 48–80%, while 99% of the removal of *audible* haze is kept. On Glass Hearts, how closely the removed signal follows the music falls from 0.549 to 0.419 (less music taken), and the vocal-melody lines mostly vanish from the removed signal.

**Status:** opt-in until confirmed by ear. Listen for the haze texture switching on and off as vocal phrases start and stop.

### `--smooth[=ms]` — steady out the static (default 85 ms)

**Problem:** the haze *flutters*, which is the "off-station radio" quality. Plain denoising makes it worse: power subtraction cuts the quiet dips harder than the peaks, which made the flutter 1–1.2 dB more pronounced on Glass Hearts.

**How:** this is the same idea as AAC's noise substitution (in noise-like bands the ear hears level, not waveform). Wherever haze dominates a bin, its level is pulled toward its own short moving average. It keeps the original phase, uses one gain for both channels, caps changes at ±6 dB, and leaves frames ≥4× above the floor (music, hits) alone.

**Measured:** flutter in the 3.5–12 kHz haze goes from 6.97/8.60 dB (Clean) to 5.19/6.45 dB, below the original. Overall level changes by +0.1 dB.

**Tuning:** sounds watery → `--smooth=40`; grit still there → `--smooth=150`.

### `--widen[=x]` — restore stereo width in the highs (default x = 1)

**Problem:** every Suno track tested collapses toward mono at the top. Above 16 kHz, left and right are 0.97–0.999 identical, while the mids are clearly wider. The WAV downloads do it too, so it comes from Suno's generator, not the MP3 export.

**How:** above 10 kHz (ramped in by 12 kHz), a 5 ms-delayed copy of the centre (mid) signal is added to the side signal. That's just enough to bring each band up to the track's *own* width in 3.5–13 kHz, times `x`. Only the side changes, so **L+R (the mono fold-down) is bit-for-bit unchanged**.

**Measured (Glass Hearts):** above 16 kHz, side relative to mid goes from −18.0 to −8.6 dB (target −7.9). Nothing below 10 kHz changes.

**Tuning:** sounds phasey on headphones → `--widen=0.5`. 7–10 kHz is left narrow on purpose, to keep "s" sounds centred.

### `--excite[=dB]` — rebuild the top end above the MP3 cutoff (default −3 dB)

**Problem:** Suno MP3s stop at about 18 kHz (the WAVs at about 20 kHz).

**How:** spectral band replication, the trick HE-AAC uses:
- **Find the cutoff:** where the long-term spectrum falls 25 dB below the 8–12 kHz level.
- **Copy:** the band at about 9–14 kHz is copied up into the empty region, frame by frame, so the new highs follow the music's hits.
- **Level:** the added content continues the track's own fitted high-frequency slope, plus the `dB` offset. Only the power that's *missing* is added, so real content isn't doubled.
- **Phase:** the shift is a multiple of 4 bins, which keeps the copied partials phase-coherent from frame to frame.

If no cutoff is found, the stage is skipped.

**Measured (Warm Current):** 20–22 kHz goes from −87 to −16 dB, with a smooth roll-off and no notch at the cutoff.

**Limit:** it adds plausible "air", not the real lost detail. If cymbals turn metallic, try `--excite=-6` or `-9`.

## A/B workflow

1. Load the original, `Clean`, and each `Clean+…` file into your DAW, level-matched.
2. Compare one step at a time (Clean → +smooth → +widen → +excite), on headphones and speakers.
3. Solo `Removed Noise`: it should sound like hiss. Drums or vocals in it mean music is being taken.
4. Wet/dry trick: put `Removed Noise` under the original with its polarity flipped. Its fader then works as a dry/wet control between Original and Clean.

## Diagnostics

Each one prints a measurement and exits without writing audio. They're how every decision in this README was made; rerun them after changing anything.

| Flag | Measures | Use it to |
|---|---|---|
| `--audibility` | Share of removed energy where the haze is masked / borderline / audible, gated vs ungated | Check that `--gate` saves wasted removal |
| `--flutter` | How much the haze level jumps around (~85 ms readings), against a steady-noise baseline | Check `--smooth`. Only stage-to-stage comparisons are fair. |
| `--width` | L/R coherence and side/mid ratio per band | Check `--widen`; spot collapsed highs |
| `--attacks` | Median 10–90% attack rise time and energy before the peak at detected hits | **Regression check**: make sure the chain doesn't dull drums |
| `--holes` | Rate of MP3 "spectral holes" per band | Only matters for files at 96 kbps or below |
| `--phase` | Phase coherence around tonal peaks | Spot "phasey" sustained notes |
| `--repeats` | How similar repeated sections are, and whether their waveforms line up | Explains why repeat-averaging can't work on Suno |

```sh
sunscreen "My Song.mp3" /tmp --width
```

## Tuning constants

Beyond the flags, the knobs are constants near the top of `src/main.rs` (rebuild after editing):

| Constant | Default | Effect |
|---|---|---|
| `FLOOR_KNOTS` | 1 / 3.5 / 7 dB | Maximum denoise cut per frequency region |
| `ONSET_K` | 2.0 | Hit-detection sensitivity (higher = fewer hits protected) |
| `GATE_RAMP_DB` | 3.0 | Width of the masked→audible crossfade |
| `SMOOTH_SNR`, `SMOOTH_LIMIT_DB` | 4×, ±6 dB | Which bins count as haze, and how far smoothing may move them |
| `WIDEN_FROM_HZ`, `WIDEN_MAX_G`, `WIDEN_DELAY_MS` | 10 kHz, 0.5, 5 ms | Where widening starts, its cap, and the decorrelation delay |
| `CUTOFF_DROP_DB`, `EXCITE_TOP_HZ` | 25 dB, 22 kHz | Cutoff detection, and how high the exciter fills |

## What it can't do

- **Saturation and distortion** are nonlinear. Spectral processing can't undo them.
- **MP3 losses** are gone for good. `--excite` makes up plausible highs; it doesn't recover the originals. Download WAV from Suno if your plan allows it.
- **Mix problems** (muddy balance, harsh vocals) need stems. Running sunscreen on each stem separately and remixing is the next step up.
- The bigger idea is a small neural model trained to undo Suno's codec fingerprint.

## Development

```sh
cargo test --release    # 7 tests: STFT round trip, each stage's intended effect, and that it fails when disabled
cargo build --release   # binary at target/release/sunscreen
```

Everything is in `src/main.rs`. Dependencies are `hound` (WAV writing), `symphonia` (MP3/WAV decoding), `realfft` and `png`.
