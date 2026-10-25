# Testing

Run with `cargo test --release`. The suite is in the `tests` module at the bottom of `src/lib.rs` and has eighteen tests. All of them generate their own signals from a fixed linear congruential generator (`seed * 6364136223846793005 + 1`, top 53 bits mapped to [-0.5, 0.5)), so they need no audio files and are deterministic.

Common pattern: the test builds a signal where the right answer is known (a pure tone, hiss at a known level, a brick-wall lowpass, a mono-forced band), runs one stage, and asserts both that the stage did its job and that it left the content it should not touch alone.

Note: tests that call `params_for` directly run with the gate **off** (the CLI turns it on); only the gate test sets it explicitly.

| Test | Signal | Asserts |
|---|---|---|
| `stft_round_trip_is_exact` | 10007 samples of pseudo-random data, n = 2048, hop = 512 | `resynth` with a no-op edit returns the input to within 1e-12 on every sample |
| `removes_hiss_keeps_tone` | 48 kHz stereo, 12 s: a 1 kHz sine (amplitude 0.3) on for the first 40% of each second, plus hiss (0.01 amplitude). The note is gated because a steady 12 s sine would itself be the floor, by design | residual against the clean tone falls below 0.8 of the input residual; overall level change stays above -0.5 dB (the tone survives) |
| `onset_protection_keeps_attacks` | decaying 2 kHz hits every 0.5 s (amplitude 0.5, decay constant 400 samples) over hiss (0.02) | removed energy in the first 10 ms of each hit with protection is below half the removed energy without it |
| `excite_fills_above_codec_lowpass_only` | 6 s of noise brick-walled at 16 kHz by zeroing bins | a full-band input and a 20 kHz lowpass return `None`; the cutoff is found within 200 Hz of 16 kHz; the long-term spectrum below 0.9c changes by less than 0.01 dB; 18 kHz rises by more than 40 dB |
| `smooth_haze_flattens_flutter_keeps_hits` | 10 s of hiss with 6 Hz depth-0.5 amplitude flutter plus 1 kHz notes (amplitude 0.3) on 40% of each second | flutter in the 3.5 to 7 kHz and 7 to 12 kHz bands (summed, from `flutter`) drops by more than 1 dB; energy during the notes changes by less than 0.2 dB |
| `widen_restores_high_width_mono_safe` | independent L/R noise whose content above 12 kHz is forced to mono | L+R changes by less than 1e-9; 16 to 20 kHz coherence goes from above 0.99 to below 0.9; side/mid there rises by more than 10 dB; the width below 7 kHz moves by less than 0.01 dB |
| `gate_spares_masked_haze_keeps_exposed` | harmonics of 220 Hz (1 to 12, amplitude 0.08/h) on 40% of each second, over hiss (0.01) | with the gate, removal in 200 Hz to 2.8 kHz while the notes play drops below half of the ungated value (masked); removal in 8 to 16 kHz stays above 90% of it (exposed) |
| `modspec_finds_frame_rate_flutter` | 20 s of white noise, with and without 10% amplitude flutter at 75 Hz | in every band, steady noise reads under 4 dB at 75 Hz and the fluttered noise reads over 10 dB |
| `lufs_reads_reference_sine` | stereo 1 kHz sine, amplitude 0.1, at 48 and 44.1 kHz | reads -20 LUFS within 0.1 |
| `true_peak_sees_between_samples` | fs/4 sine at 45 degrees (every sample 0.707 of the true peak) | sample peak -3.01 dB; true peak within 0.2 dB of 0 |
| `master_hits_loudness_and_ceiling` | quiet stereo noise with a sharp hit every 0.5 s | output within 0.1 LU of -14 LUFS, true peak at or under -1 dBTP, more than 1 dB of limiting |
| `run_with_reports_errors_and_cancels` | a missing file, and a 1 s tone with cancel already set | the missing file comes back as an error naming it; the cancelled run returns `cancelled` before writing any report line |
| `debuzz_does_not_ring_after_on_grid_note` | stereo noise, a 240-sample pattern, and a phase-locked 2200 Hz note held 1 s in every 4 | energy in the seconds after each note no higher than the input's (without the per-bin gate: +17.9 dB) |
| `debuzz_removes_decoder_grid` | at 48 and 44.1 kHz: stereo noise plus a fixed random one-period waveform (240 / 441 samples) 26 dB under it | plain noise not flagged; mixture above 15 dB; output below 3 dB; residual against the clean noise under 6% of it |
| `edge_fade_silences_ends_only` | 1 s of constant 0.5, stereo | first and last samples silent, fade-in monotonic, everything beyond the 5 ms ramps untouched |
| `strength_follows_haze_flutter` | 12 s of steady hiss; the same hiss with its level jumping up to ±20 dB every 100 ms | the steady hiss gets `STRENGTH_MIN`, the jumping hiss full strength; `denoise` with auto strength removes less than half of what full strength removes from the steady hiss |
| `abrupt_end_gets_fade_out` | 12 s of noise cut dead, then 1 s of silence; the same ringing out to -80 dB over its last 2 s | the ring-out isn't flagged; the cut is, at the right sample; the fade-out is longer than 5 ms, silent at the end, and leaves earlier music alone; `with_fades` hands the signal back unfaded |
| `match_follows_reference_tilt_capped` | white noise; a reference is the same noise with a smooth `tanh` tilt of 1.5 dB around 1 kHz | the correction between 250 Hz and 4 kHz matches the tilt to within 0.6 dB; with a 12 dB tilt no value exceeds `MATCH_MAX_DB`; the correction at 23 kHz is zero (fade) |

## Positive and negative controls

Each stage test contains both kinds of control in the same test:

- **Positive:** the effect appears where it should (hiss falls, the exciter adds 40 dB, the gate cuts masked removal in half, the tilt is followed).
- **Negative:** content that must not change does not (the tone, the notes, everything below 0.9c, the width below 7 kHz, the mono sum, exposed removal).

The onset and gate tests are built as A/B comparisons: the same signal is run with the feature on and off and the assertion is on the ratio, so the test cannot pass if the feature does nothing.

## Mutation-check discipline

A test that has never failed has not been shown to test anything. The discipline is:

1. After writing a test, break the feature on purpose (a mutation) and confirm the test goes red. Examples of mutations for each stage: set `protect_onsets` to false inside `denoise` (the onset test must fail); make the gate weight constant 1 (the gate test must fail); make the exciter gain 0 (the excite test must fail); change the sign on one channel in `widen` (the mono-safety assertion must fail); remove the `.clamp(-cap, cap)` in `match_curve` (the cap assertion must fail); set the fade in `match_curve` to 1 (the 23 kHz assertion must fail).
2. Restore the feature and confirm green.

The project notes record this explicitly for the `modspec` and `match` tests; the same discipline applies to the others, and it is the way to approach any new test. It applies to diagnostics as well: a measurement is trusted only after a control signal with a known answer has passed through it (the 75 Hz flutter control for `--modspec`, the steady-tone control for `--phase`, the 64k re-encode for `--holes`, the exact-repeat file for `--repeats`, the steady-noise row of `--flutter`).

## Round-trip check at run time

Apart from the unit tests, every full run measures the STFT round-trip error on channel 0 and prints it (`stft rt    max |err|`), so a broken transform shows up in the report of any real file ([stft.md](stft.md)).

## Regression checklist when changing the chain

1. `cargo test --release` is green.
2. `--attacks` on the original and on each cumulative output: rise time and pre-energy do not worsen.
3. `--audibility` if the denoiser, floor or gate changed.
4. `--flutter` if the denoiser or smoothing changed.
5. `--width` after anything touching stereo.
6. Listen to `Removed Noise`: hiss is expected; drums or vocals mean music is being taken.
