//! Adaptive spectral denoise for Suno renders: rolling-percentile noise floor,
//! linked-stereo power-subtraction mask, original phase kept.
//! Usage: sunscreen <in.mp3|in.wav|in.flac|in.m4a> [out_dir] [title] [--no-gate] [--no-debuzz] [--strength=x] [--smooth[=ms]] [--widen[=x]] [--excite[=dB]] [--match[=ref.wav]] [--master[=LUFS]]

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;
use std::f64::consts::PI;
use std::path::Path;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// Every report line of a run, written to "<title> - Report.txt" at the end. Kept
// per thread so an app can run several songs at once; `run_with` redirects the
// lines and can cancel between them.
thread_local! {
    static REPORT: RefCell<String> = const { RefCell::new(String::new()) };
    static SINK: RefCell<Option<Box<dyn FnMut(&str)>>> = const { RefCell::new(None) };
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Panic payload that unwinds a cancelled run.
struct Cancelled;

fn report_line(s: &str) {
    if CANCEL.with_borrow(|c| c.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))) { std::panic::panic_any(Cancelled); }
    REPORT.with_borrow_mut(|r| { r.push_str(s); r.push('\n'); });
    SINK.with_borrow_mut(|k| match k { Some(f) => f(s), None => println!("{s}") });
}
macro_rules! say { ($($t:tt)*) => { report_line(&format!($($t)*)) } }

struct Params {
    n: usize,
    hop: usize,
    block_s: f64,        // percentile block length
    pctl: f64,           // per-block percentile
    noise_scale: f64,    // err on the gentle side
    alpha_lo: f64,       // below ALPHA_RAMP.0
    alpha_hi: f64,       // above ALPHA_RAMP.1
    sigma_f: f64,        // mask blur, bins
    sigma_t: f64,        // mask blur, frames
    protect_onsets: bool,
    gate: bool,          // hearing-model gating of the attenuation
    strength: Option<f64>, // scales the attenuation in dB; None = from the haze's flutter
}

const ONSET_K: f64 = 2.0; // flux over local median to count as a hit
// exciter: cutoff = where long-term level falls this far below the 8-12 kHz median
const CUTOFF_DROP_DB: f64 = 25.0;
const EXCITE_TOP_HZ: f64 = 22000.0;
const EXCITE_FADE_HZ: f64 = 500.0; // fade-in width above the cutoff
const EXCITE_DEFAULT_DB: f64 = -3.0; // relative to the extrapolated slope
const EXCITE_MIN_CUTOFF_HZ: f64 = 19500.0; // Suno v6 reaches ~20 kHz: nothing to rebuild
const HOLE_DB: f64 = 15.0;
// haze smoothing: full effect at/below the floor, none from SMOOTH_SNR x floor up
const SMOOTH_SNR: f64 = 4.0;
const SMOOTH_LIMIT_DB: f64 = 6.0;
const SMOOTH_DEFAULT_MS: f64 = 85.0;
// cleaning strength: full from this 7-12 kHz haze flutter up, never below the minimum
const STRENGTH_FULL_FLUTTER_DB: f64 = 8.5;
const STRENGTH_MIN: f64 = 0.4;
// widen: only above this, side gain capped (0.5 = side gets at most -6 dB of mid)
const WIDEN_FROM_HZ: f64 = 10000.0;
const WIDEN_MAX_G: f64 = 0.5;
const WIDEN_DELAY_MS: f64 = 5.0;
// hearing model gate ramps over +-GATE_RAMP_DB of haze-to-mask ratio
const GATE_RAMP_DB: f64 = 3.0;
const GATE_DILATE: usize = 2; // bins and frames
const ALPHA_RAMP: (f64, f64) = (3500.0, 7000.0);
// (freq Hz, max attenuation dB) knots; flat between pairs, log-freq raised cosine across gaps
const FLOOR_KNOTS: [(f64, f64); 4] = [(300.0, 1.0), (450.0, 3.5), (3500.0, 3.5), (7000.0, 7.0)];
// --match: largest correction, smoothing span, and where it applies (fades to 0
// half an octave outside, so it never fights the codec lowpass or the exciter)
const MATCH_MAX_DB: f64 = 3.0;
const MATCH_SMOOTH_OCT: f64 = 1.0;
const MATCH_RANGE: (f64, f64) = (40.0, 12000.0); // the 12 references disagree by 3-4.6 dB above 12 kHz
// Mean third-octave long-term spectrum of 12 commercially mastered pop tracks
// (Ariana Grande, milet, Hikaru Utada / Utada; `--profile`), 0 dB = 125 Hz - 8 kHz
// mean. Per-point spread across the 12 is 1-2.7 dB from 125 Hz to 10 kHz.
const POP_TARGET: [(f64, f64); 28] = [(32.0, 16.3), (40.0, 18.1), (50.0, 18.1), (63.0, 18.1), (79.0, 17.0), (100.0, 14.6), (126.0, 12.4), (159.0, 9.6), (200.0, 6.8), (252.0, 5.9), (318.0, 5.2), (400.0, 5.3), (504.0, 4.7), (635.0, 3.0), (800.0, 4.2), (1008.0, 3.3), (1270.0, -0.3), (1600.0, -1.0), (2016.0, -2.8), (2540.0, -5.1), (3200.0, -6.0), (4032.0, -8.1), (5080.0, -10.8), (6400.0, -12.8), (8064.0, -13.6), (10160.0, -15.0), (12801.0, -18.2), (16128.0, -23.5)];

/// POP_TARGET at any frequency: linear in log frequency, held flat past the ends.
fn pop_target(h: f64) -> f64 {
    let t = &POP_TARGET;
    if h <= t[0].0 { return t[0].1; }
    t.windows(2).find(|w| h <= w[1].0).map_or(t[t.len() - 1].1, |w| {
        let a = (h / w[0].0).log2() / (w[1].0 / w[0].0).log2();
        w[0].1 + a * (w[1].1 - w[0].1)
    })
}

/// A reference track's long-term spectrum as a target function of Hz.
fn ltas_target(r: &[Vec<f64>], rsr: u32) -> impl Fn(f64) -> f64 {
    let rp = params_for(rsr);
    let rl = ltas(&Stft::new(rp.n, rp.hop, r[0].len()), r);
    move |h: f64| {
        let k = (h * rp.n as f64 / rsr as f64).min((rl.len() - 1) as f64);
        let i = (k as usize).min(rl.len() - 2);
        rl[i] + (rl[i + 1] - rl[i]) * (k - i as f64)
    }
}

// --master: integrated loudness target (LUFS) and true-peak ceiling (dBTP);
// limiter look-ahead each side of a peak, and release time
const MASTER_DEFAULT_LUFS: f64 = -14.0;
const TRUE_PEAK_DBTP: f64 = -1.0;
const LIMIT_ATTACK_MS: f64 = 2.0;
const LIMIT_RELEASE_MS: f64 = 80.0;
const TP_HALF_TAPS: usize = 6; // 12 taps per phase, as in BS.1770's reference 48-tap 4x filter

// debuzz: Suno's decoder repeats a fixed waveform DEBUZZ_HZ times a second (every
// 240 samples at 48 kHz). Counts as present when its synchronous average stands
// DEBUZZ_MIN_DB above chance (Suno 15-29 dB, human masters ~0); it is estimated
// over DEBUZZ_WINDOW_S windows so its slowly drifting level is tracked.
const DEBUZZ_HZ: u32 = 200;
const DEBUZZ_MIN_DB: f64 = 6.0;
const DEBUZZ_WINDOW_S: f64 = 4.0;
const GRID_BLOCK: usize = 1 << 19; // grid_scan autocorrelation block, ~11 s at 48 kHz

// Suno files often start or stop mid-sound (first sample up to -14 dBFS, last
// 100 ms still at -20 dBFS); human masters start and end on digital silence.
// A short raised-cosine fade at each end removes the click without softening
// an opening downbeat audibly.
const EDGE_FADE_MS: f64 = 5.0;
// Some stop mid-phrase: their last 250 ms before any trailing silence sit at -38 to
// -1 dB re the song (9 of 21 Suno tracks); human masters end at -52 dB or below.
// Those get a fade-out over the last END_FADE_S instead of the 5 ms one.
const END_ABRUPT_DB: f64 = -40.0;
const END_FADE_S: f64 = 3.0;

const TONE_HZ: [f64; 10] = [63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 3000.0, 4000.0, 8000.0, 12000.0];
const BANDS: [(f64, f64); 5] = [(0.0, 350.0), (350.0, 3500.0), (3500.0, 7000.0), (7000.0, 12000.0), (12000.0, f64::MAX)];

fn params_for(sr: u32) -> Params {
    // 2048/512 at 48 kHz, scaled proportionally
    let n = ((2048.0 * sr as f64 / 48000.0 / 4.0).round() as usize * 4).max(256);
    Params { n, hop: n / 4, block_s: 1.0, pctl: 0.15, noise_scale: 0.8, alpha_lo: 1.0, alpha_hi: 2.0, sigma_f: 2.0, sigma_t: 1.5, protect_onsets: true, gate: false, strength: Some(1.0) }
}

/// Raised cosine 0→1 across [a,b] on a log-frequency axis.
fn log_ramp(f: f64, a: f64, b: f64) -> f64 {
    if f <= a { return 0.0; }
    if f >= b { return 1.0; }
    let t = (f.ln() - a.ln()) / (b.ln() - a.ln());
    0.5 - 0.5 * (PI * t).cos()
}

fn floor_db(f: f64) -> f64 {
    let mut db = FLOOR_KNOTS[0].1;
    for w in FLOOR_KNOTS.windows(2) {
        db += (w[1].1 - w[0].1) * log_ramp(f, w[0].0, w[1].0);
    }
    db
}

fn alpha(p: &Params, f: f64) -> f64 {
    p.alpha_lo + (p.alpha_hi - p.alpha_lo) * log_ramp(f, ALPHA_RAMP.0, ALPHA_RAMP.1)
}

fn hann(n: usize) -> Vec<f64> {
    (0..n).map(|k| 0.5 - 0.5 * (2.0 * PI * k as f64 / n as f64).cos()).collect()
}

/// Frame t covers padded samples [t*hop, t*hop+n); padding of n-hop in front so
/// every original sample sees the same number of frames.
struct Stft {
    n: usize,
    hop: usize,
    pad: usize,
    frames: usize,
    win: Vec<f64>,
    fwd: std::sync::Arc<dyn realfft::RealToComplex<f64>>,
    inv: std::sync::Arc<dyn realfft::ComplexToReal<f64>>,
}

impl Stft {
    fn new(n: usize, hop: usize, len: usize) -> Self {
        let mut planner = RealFftPlanner::<f64>::new();
        let pad = n - hop;
        Stft { n, hop, pad, frames: (pad + len - 1) / hop + 1, win: hann(n), fwd: planner.plan_fft_forward(n), inv: planner.plan_fft_inverse(n) }
    }

    fn bins(&self) -> usize { self.n / 2 + 1 }

    fn frame(&self, x: &[f64], t: usize, out: &mut [Complex<f64>]) {
        let mut buf = vec![0.0; self.n];
        let start = (t * self.hop) as isize - self.pad as isize;
        for (k, b) in buf.iter_mut().enumerate() {
            let i = start + k as isize;
            if i >= 0 && (i as usize) < x.len() { *b = x[i as usize] * self.win[k]; }
        }
        self.fwd.process(&mut buf, out).unwrap();
    }

    /// Weighted overlap-add; `spec(t, &mut bins)` fills/edits each frame's spectrum.
    fn resynth(&self, x: &[f64], mut edit: impl FnMut(usize, &mut [Complex<f64>])) -> Vec<f64> {
        let mut acc = vec![0.0; x.len()];
        let mut wsum = vec![0.0; x.len()];
        let mut spec = vec![Complex::default(); self.bins()];
        let mut buf = vec![0.0; self.n];
        for t in 0..self.frames {
            self.frame(x, t, &mut spec);
            edit(t, &mut spec);
            spec[0].im = 0.0;
            spec[self.n / 2].im = 0.0;
            self.inv.process(&mut spec, &mut buf).unwrap();
            let start = (t * self.hop) as isize - self.pad as isize;
            for k in 0..self.n {
                let i = start + k as isize;
                if i >= 0 && (i as usize) < x.len() {
                    acc[i as usize] += buf[k] / self.n as f64 * self.win[k];
                    wsum[i as usize] += self.win[k] * self.win[k];
                }
            }
        }
        acc.iter().zip(&wsum).map(|(a, w)| if *w > 1e-12 { a / w } else { 0.0 }).collect()
    }
}

/// Separable Gaussian blur on a [frames][bins] grid, edge = nearest.
fn blur(g: &mut [f32], frames: usize, bins: usize, sf: f64, st: f64) {
    let kern = |s: f64| -> Vec<f32> {
        let r = (3.0 * s).ceil() as isize;
        let k: Vec<f64> = (-r..=r).map(|i| (-(i * i) as f64 / (2.0 * s * s)).exp()).collect();
        let sum: f64 = k.iter().sum();
        k.iter().map(|v| (v / sum) as f32).collect()
    };
    let (kf, kt) = (kern(sf), kern(st));
    let (rf, rt) = ((kf.len() / 2) as isize, (kt.len() / 2) as isize);
    let src = g.to_vec();
    for t in 0..frames {
        for f in 0..bins {
            g[t * bins + f] = kf.iter().enumerate()
                .map(|(j, w)| w * src[t * bins + (f as isize + j as isize - rf).clamp(0, bins as isize - 1) as usize]).sum();
        }
    }
    let src = g.to_vec();
    for t in 0..frames {
        for f in 0..bins {
            g[t * bins + f] = kt.iter().enumerate()
                .map(|(j, w)| w * src[(t as isize + j as isize - rt).clamp(0, frames as isize - 1) as usize * bins + f]).sum();
        }
    }
}

/// Rolling noise floor: per-block percentile → 4 s log-domain smoothing →
/// interpolate to frames → light freq smoothing → scale. Returns [frames][bins] power.
fn noise_floor(p: &Params, power: &[f32], frames: usize, bins: usize, sr: f64) -> Vec<f32> {
    let bf = ((p.block_s * sr / p.hop as f64).round() as usize).max(1);
    let nblocks = frames.div_ceil(bf);
    let mut blk = vec![0.0f64; nblocks * bins]; // log power
    let mut tmp = Vec::with_capacity(bf);
    for b in 0..nblocks {
        let (t0, t1) = (b * bf, ((b + 1) * bf).min(frames));
        for f in 0..bins {
            tmp.clear();
            tmp.extend((t0..t1).map(|t| power[t * bins + f]));
            let k = ((tmp.len() - 1) as f64 * p.pctl).round() as usize;
            let v = *tmp.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1;
            blk[b * bins + f] = (v as f64 + 1e-20).ln();
        }
    }
    // ~4 s smoothing over 1 s blocks: weights [.5,1,1,1,.5], renormalised at edges
    let w = [0.5, 1.0, 1.0, 1.0, 0.5];
    let mut sm = vec![0.0f64; nblocks * bins];
    for b in 0..nblocks {
        for f in 0..bins {
            let (mut s, mut ws) = (0.0, 0.0);
            for (j, wj) in w.iter().enumerate() {
                let bb = b as isize + j as isize - 2;
                if bb >= 0 && (bb as usize) < nblocks { s += wj * blk[bb as usize * bins + f]; ws += wj; }
            }
            sm[b * bins + f] = s / ws;
        }
    }
    let mut out = vec![0.0f32; frames * bins];
    for t in 0..frames {
        // block b's centre sits at frame b*bf + bf/2
        let pos = ((t as f64 - bf as f64 / 2.0) / bf as f64).clamp(0.0, (nblocks - 1) as f64);
        let (b0, fr) = (pos.floor() as usize, pos.fract());
        let b1 = (b0 + 1).min(nblocks - 1);
        for f in 0..bins {
            let l = sm[b0 * bins + f] * (1.0 - fr) + sm[b1 * bins + f] * fr;
            out[t * bins + f] = l as f32; // stays log until after smoothing
        }
    }
    blur(&mut out, frames, bins, 1.0, 0.0001);
    for v in out.iter_mut() { *v = (v.exp() as f64 * p.noise_scale) as f32; }
    out
}

/// Decode MP3, WAV, FLAC or M4A (AAC/ALAC) to per-channel f64. Gapless mode trims
/// MP3 encoder delay/padding. Errors are worded for the person who chose the file.
fn read_audio(path: &Path) -> Result<(Vec<Vec<f64>>, u32), String> {
    use symphonia::core::{audio::SampleBuffer, codecs::{CODEC_TYPE_NULL, DecoderOptions}, errors::Error, formats::FormatOptions, io::MediaSourceStream, probe::Hint};
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into());
    let file = std::fs::File::open(path).map_err(|e| format!("can't open {name}: {e}"))?;
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) { hint.with_extension(ext); }
    let mut fmt = symphonia::default::get_probe()
        .format(&hint, MediaSourceStream::new(Box::new(file), Default::default()), &FormatOptions { enable_gapless: true, ..Default::default() }, &Default::default())
        .map_err(|_| format!("{name} isn't MP3, WAV, FLAC or M4A audio"))?.format;
    let track = fmt.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or_else(|| format!("{name} has no audio sunscreen can read (Apple Music downloads are copy-protected)"))?;
    let id = track.id;
    // copy-protected (FairPlay) M4A has a codec symphonia doesn't know
    let mut dec = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())
        .map_err(|_| format!("can't decode {name} (copy-protected, or an unsupported codec)"))?;
    // AAC in MP4 may not declare its layout up front, so take it from the first decoded packet
    let (mut chans, mut sr) = (Vec::<Vec<f64>>::new(), 0);
    while let Ok(pkt) = fmt.next_packet() {
        if pkt.track_id() != id { continue; }
        let buf = match dec.decode(&pkt) {
            Ok(b) => b,
            Err(Error::DecodeError(_)) => continue, // a corrupt frame: skip it, as players do
            Err(e) => return Err(format!("can't decode {name}: {e}")),
        };
        if chans.is_empty() {
            chans = vec![Vec::new(); buf.spec().channels.count()];
            sr = buf.spec().rate;
        }
        let mut sb = SampleBuffer::<f32>::new(buf.capacity() as u64, *buf.spec());
        sb.copy_interleaved_ref(buf);
        let ch = chans.len();
        for (i, s) in sb.samples().iter().enumerate() { chans[i % ch].push(*s as f64); }
    }
    if chans.is_empty() || chans[0].len() < sr as usize { return Err(format!("{name} is shorter than a second")); }
    Ok((chans, sr))
}

/// 24-bit PCM. Returns number of samples that exceeded full scale (clamped on write).
fn write_wav24(path: &Path, chans: &[Vec<f64>], sr: u32) -> usize {
    let spec = hound::WavSpec { channels: chans.len() as u16, sample_rate: sr, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let mut over = 0;
    for i in 0..chans[0].len() {
        for c in chans {
            let v = c[i] * 8388608.0;
            if v.abs() > 8388607.0 { over += 1; }
            w.write_sample(v.round().clamp(-8388608.0, 8388607.0) as i32).unwrap();
        }
    }
    w.finalize().unwrap();
    over
}

fn peak_db(chans: &[Vec<f64>]) -> f64 {
    20.0 * chans.iter().flatten().fold(0.0f64, |m, v| m.max(v.abs())).log10()
}

fn rms(chans: &[Vec<f64>]) -> f64 {
    let n: usize = chans.iter().map(|c| c.len()).sum();
    (chans.iter().flatten().map(|v| v * v).sum::<f64>() / n as f64).sqrt()
}

/// Grayscale-to-heat spectrogram, linear freq axis, each half normalised to its own max.
fn spectrogram_png(path: &Path, sigs: &[&[f64]], n: usize, hop: usize) {
    let rows = n / 4; // pair-average bins down to n/4 rows
    let mut img: Vec<u8> = Vec::new();
    let mut width = 0;
    for sig in sigs {
        let st = Stft::new(n, hop, sig.len());
        let mut spec = vec![Complex::default(); st.bins()];
        let mut db = vec![0.0f64; st.frames * rows];
        for t in 0..st.frames {
            st.frame(sig, t, &mut spec);
            for r in 0..rows {
                let p = spec[2 * r].norm_sqr() + spec[2 * r + 1].norm_sqr();
                db[t * rows + r] = 10.0 * (p + 1e-20).log10();
            }
        }
        let top = db.iter().cloned().fold(f64::MIN, f64::max);
        width = st.frames;
        for r in (0..rows).rev() {
            for t in 0..st.frames {
                let x = ((db[t * rows + r] - top + 80.0) / 80.0).clamp(0.0, 1.0);
                img.extend([(255.0 * (x * 1.6).min(1.0)) as u8, (255.0 * ((x - 0.35) * 1.6).clamp(0.0, 1.0)) as u8, (255.0 * ((x - 0.7) * 3.3).clamp(0.0, 1.0)) as u8]);
            }
        }
        img.extend(std::iter::repeat_n(255u8, width * 3 * 4)); // separator
    }
    let h = img.len() / (width * 3);
    let mut e = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(path).unwrap()), width as u32, h as u32);
    e.set_color(png::ColorType::Rgb);
    e.write_header().unwrap().write_image_data(&img).unwrap();
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let cov: f64 = a.iter().zip(b).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f64 = a.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = b.iter().map(|y| (y - mb).powi(2)).sum();
    cov / (va * vb).sqrt()
}

/// 1 s RMS envelope of the channel-averaged signal.
fn envelope(chans: &[Vec<f64>], sr: usize) -> Vec<f64> {
    let mono: Vec<f64> = (0..chans[0].len()).map(|i| chans.iter().map(|c| c[i]).sum::<f64>() / chans.len() as f64).collect();
    mono.chunks(sr).map(|c| (c.iter().map(|v| v * v).sum::<f64>() / c.len() as f64).sqrt()).collect()
}

/// Per-frame 0..1 weight pulling the mask to unity around attacks. The time blur
/// otherwise drags pre-hit attenuation onto the attack and dulls drums; the hit
/// itself masks any haze let through for those few frames.
fn onset_weights(power: &[f32], frames: usize, bins: usize, hz_per_bin: f64) -> Vec<f64> {
    let mut w = vec![0.0f64; frames];
    for t in onsets(power, frames, bins, hz_per_bin) {
        // frames t-2..t+1 hold the attack (window is 4 hops long)
        for (d, v) in [(-2isize, 0.5), (-1, 1.0), (0, 1.0), (1, 0.5)] {
            let i = t as isize + d;
            if i >= 0 && (i as usize) < frames { w[i as usize] = w[i as usize].max(v); }
        }
    }
    w
}

/// Onset frames: peaks of positive log spectral flux above ONSET_K x local median.
fn onsets(power: &[f32], frames: usize, bins: usize, hz_per_bin: f64) -> Vec<usize> {
    let lo = (150.0 / hz_per_bin) as usize;
    let lp = |t: usize, f: usize| (power[t * bins + f] as f64 + 1e-10).ln();
    let flux: Vec<f64> = (0..frames).map(|t| if t == 0 { 0.0 } else {
        (lo..bins).map(|f| (lp(t, f) - lp(t - 1, f)).max(0.0)).sum::<f64>() / (bins - lo) as f64
    }).collect();
    let r = 12; // ~128 ms each side at 48k/512
    let mut win = Vec::with_capacity(2 * r + 1);
    (1..frames.saturating_sub(1)).filter(|&t| {
        win.clear();
        win.extend_from_slice(&flux[t.saturating_sub(r)..(t + r + 1).min(frames)]);
        let k = win.len() / 2;
        let med = *win.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1;
        flux[t] > ONSET_K * med && flux[t] > 0.1 && flux[t] >= flux[t - 1] && flux[t] >= flux[t + 1]
    }).collect()
}

/// Attack shape at detected hits, on content above 2 kHz with a 1 ms RMS envelope.
/// Returns (median 10-90% rise ms, median pre-energy dB = mean power 5-25 ms before
/// the peak relative to the peak, hits measured).
fn attacks(p: &Params, x: &[Vec<f64>], sr: u32) -> (f64, f64, usize) {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let (frames, bins) = (st.frames, st.bins());
    let power = power_grid(&st, x);
    let lo = (2000.0 / (sr as f64 / p.n as f64)) as usize;
    let mono: Vec<f64> = (0..x[0].len()).map(|i| x.iter().map(|c| c[i]).sum::<f64>() / x.len() as f64).collect();
    let hp = st.resynth(&mono, |_, s| for v in s[..lo].iter_mut() { *v = Complex::default(); });
    let ms = |m: f64| (m / 1000.0 * sr as f64).round() as usize;
    let half = ms(0.5);
    let mut pre = vec![0.0; hp.len() + 1];
    for (i, v) in hp.iter().enumerate() { pre[i + 1] = pre[i] + v * v; }
    let env = |i: usize| { let (a, b) = (i.saturating_sub(half), (i + half + 1).min(hp.len())); ((pre[b] - pre[a]) / (b - a) as f64).sqrt() };
    let (mut rises, mut pres) = (Vec::new(), Vec::new());
    for t in onsets(&power, frames, bins, sr as f64 / p.n as f64) {
        // frame t's window spans [t*hop - pad, +n); the attack lies inside it
        let c = (t * p.hop + p.n / 2) as isize - st.pad as isize;
        let (a, b) = ((c - (p.n / 2) as isize).max(ms(30.0) as isize) as usize, ((c + (p.n / 2) as isize) as usize).min(hp.len() - 1));
        if a >= b { continue; }
        let ip = (a..b).max_by(|i, j| env(*i).total_cmp(&env(*j))).unwrap();
        let pk = env(ip);
        if pk < 1e-6 { continue; }
        let back = |lvl: f64| (ip.saturating_sub(ms(30.0))..=ip).rev().find(|&i| env(i) < lvl * pk);
        let (Some(i10), Some(i90)) = (back(0.1), back(0.9)) else { continue };
        rises.push((i90 - i10) as f64 * 1000.0 / sr as f64);
        let (s0, s1) = (ip - ms(25.0), ip - ms(5.0));
        let pe = (s0..s1).map(|i| hp[i] * hp[i]).sum::<f64>() / (s1 - s0) as f64;
        pres.push(10.0 * (pe / (pk * pk) + 1e-20).log10());
    }
    let med = |v: &mut Vec<f64>| { if v.is_empty() { return f64::NAN; } v.sort_by(|a, b| a.total_cmp(b)); v[v.len() / 2] };
    let n = rises.len();
    (med(&mut rises), med(&mut pres), n)
}

/// Long-term average power spectrum in dB, all channels.
fn ltas(st: &Stft, x: &[Vec<f64>]) -> Vec<f64> {
    let mut acc = vec![0.0f64; st.bins()];
    let mut spec = vec![Complex::default(); st.bins()];
    for c in x {
        for t in 0..st.frames {
            st.frame(c, t, &mut spec);
            for (a, s) in acc.iter_mut().zip(&spec) { *a += s.norm_sqr(); }
        }
    }
    acc.iter().map(|a| 10.0 * (a / (x.len() * st.frames) as f64 + 1e-30).log10()).collect()
}

struct Excite { cutoff_hz: f64, slope_db_oct: f64, shift_hz: f64 }

/// Spectral band replication above a lossy-codec lowpass: copy the band below the
/// cutoff up by k bins (k multiple of n/hop keeps the shifted partials phase-coherent
/// across frames), scaled per bin so the long-term level follows the slope fitted
/// over [c/2, 0.9c], plus `amount_db`. Never boosts a source bin. None if no cutoff.
fn excite(p: &Params, x: &[Vec<f64>], sr: u32, amount_db: f64) -> Option<(Vec<Vec<f64>>, Excite)> {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let hz_per = sr as f64 / p.n as f64;
    let bin = |hz: f64| (hz / hz_per).round() as usize;
    let top = bin(EXCITE_TOP_HZ.min(sr as f64 / 2.0)).min(st.bins() - 1);
    let l = ltas(&st, x);
    let mut reference: Vec<f64> = l[bin(8000.0)..bin(12000.0)].to_vec();
    reference.sort_by(|a, b| a.total_cmp(b));
    let reference = reference[reference.len() / 2];
    let c = (bin(10000.0)..=top).rev().find(|&f| l[f] > reference - CUTOFF_DROP_DB)? + 1;
    if c >= top.saturating_sub(bin(500.0)) || c as f64 * hz_per >= EXCITE_MIN_CUTOFF_HZ { return None; } // nothing audible missing
    // least-squares dB vs log2(Hz)
    let pts: Vec<(f64, f64)> = (c / 2..c * 9 / 10).map(|f| ((f as f64 * hz_per).log2(), l[f])).collect();
    let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64, pts.iter().map(|p| p.1).sum::<f64>() / pts.len() as f64);
    let slope = pts.iter().map(|(a, b)| (a - mx) * (b - my)).sum::<f64>() / pts.iter().map(|(a, _)| (a - mx).powi(2)).sum::<f64>();
    // fill from 0.9c (inside the codec's rolloff, so no notch) with source ending at
    // 0.8c (clear of the sparse, hole-y bins near the cutoff)
    let start = c * 9 / 10;
    let hop_ratio = p.n / p.hop;
    let k = (top - c * 8 / 10).div_ceil(hop_ratio) * hop_ratio;
    let fade = bin(EXCITE_FADE_HZ).max(1);
    let gains: Vec<f64> = (start..=top).map(|f| {
        let target = my + slope * ((f as f64 * hz_per).log2() - mx) + amount_db;
        // only the power missing versus target, so real content isn't doubled
        let missing = (10f64.powf(target / 10.0) - 10f64.powf(l[f] / 10.0)).max(0.0);
        let ramp = 0.5 - 0.5 * (PI * ((f - start) as f64 / fade as f64).min(1.0)).cos();
        (missing / 10f64.powf(l[f - k] / 10.0)).sqrt().min(1.0) * ramp
    }).collect();
    let out = x.iter().map(|ch| st.resynth(ch, |_, s| {
        let src = s.to_vec();
        for (i, g) in gains.iter().enumerate() { s[start + i] += *g * src[start + i - k]; }
    })).collect();
    Some((out, Excite { cutoff_hz: c as f64 * hz_per, slope_db_oct: slope, shift_hz: k as f64 * hz_per }))
}

/// Fraction of bins per band that dip `HOLE_DB` below the geometric mean of the
/// same bin in the neighbouring frames (MP3 zeroed-coefficient flicker).
fn hole_rates(n: usize, x: &[Vec<f64>], sr: u32) -> Vec<f64> {
    let st = Stft::new(n, n / 4, x[0].len());
    let bins = st.bins();
    let mut spec = vec![Complex::default(); bins];
    let (mut holes, mut total) = (vec![0usize; BANDS.len()], vec![0usize; BANDS.len()]);
    let band_of: Vec<usize> = (0..bins).map(|f| BANDS.iter().position(|(lo, hi)| { let hz = f as f64 * sr as f64 / n as f64; hz >= *lo && hz < *hi }).unwrap()).collect();
    let thresh = 10f64.powf(-HOLE_DB / 10.0);
    for c in x {
        let mut prev2 = vec![0.0; bins];
        let mut prev = vec![0.0; bins];
        for t in 0..st.frames {
            st.frame(c, t, &mut spec);
            let cur: Vec<f64> = spec.iter().map(|s| s.norm_sqr()).collect();
            if t >= 2 {
                for f in 0..bins {
                    let e = (prev2[f] * cur[f]).sqrt();
                    if e < 1e-9 { continue; } // silence
                    total[band_of[f]] += 1;
                    if prev[f] < thresh * e { holes[band_of[f]] += 1; }
                }
            }
            prev2 = std::mem::replace(&mut prev, cur);
        }
    }
    holes.iter().zip(&total).map(|(h, t)| if *t > 0 { *h as f64 / *t as f64 } else { 0.0 }).collect()
}

/// Channel-averaged power, [frames][bins].
fn power_grid(st: &Stft, x: &[Vec<f64>]) -> Vec<f32> {
    let bins = st.bins();
    let mut power = vec![0.0f32; st.frames * bins];
    let mut spec = vec![Complex::default(); bins];
    for c in x {
        for t in 0..st.frames {
            st.frame(c, t, &mut spec);
            for f in 0..bins { power[t * bins + f] += (spec[f].norm_sqr() / x.len() as f64) as f32; }
        }
    }
    power
}

/// Mean robust spread (dB) of ~85 ms level readings within 1 s blocks, per band, over bins whose
/// block median sits within 2.5x the rolling floor (haze-dominated). Steady noise
/// gives the chance baseline; more than that means the haze itself flutters.
fn flutter(p: &Params, x: &[Vec<f64>], sr: u32) -> Vec<(f64, usize)> {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let (frames, bins) = (st.frames, st.bins());
    let power = power_grid(&st, x);
    let noise = noise_floor(p, &power, frames, bins, sr as f64);
    flutter_grid(p, &power, &noise, frames, bins, sr)
}

/// Cleaning strength from the 7-12 kHz haze flutter: Suno v5's haze flutters
/// 9-17 dB and gets the full cut; v6 renders the highs as a steady wash (3.9-4.6 dB,
/// below human masters' 5.5-10) that the floor estimate mistakes for haze, and at
/// full strength it takes 4-5 dB more than on v5 (slightly duller by ear). v6 lands
/// near 0.5, which removes about what a human master loses.
fn haze_strength(flutter_db: f64) -> f64 {
    if flutter_db.is_nan() { 1.0 } else { (flutter_db / STRENGTH_FULL_FLUTTER_DB).clamp(STRENGTH_MIN, 1.0) }
}

fn flutter_grid(p: &Params, power: &[f32], noise: &[f32], frames: usize, bins: usize, sr: u32) -> Vec<(f64, usize)> {
    let bf = (sr as f64 / p.hop as f64).round() as usize;
    let mut acc = vec![(0.0f64, 0usize); BANDS.len()];
    let mut col = Vec::with_capacity(bf);
    for f in 1..bins - 1 {
        let hz = f as f64 * sr as f64 / p.n as f64;
        let b = BANDS.iter().position(|(lo, hi)| hz >= *lo && hz < *hi).unwrap();
        for blk in 0..frames / bf {
            col.clear();
            col.extend((blk * bf..(blk + 1) * bf).map(|t| power[t * bins + f] as f64));
            let mut sorted = col.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let floor = noise[(blk * bf + bf / 2) * bins + f] as f64 / p.noise_scale;
            if sorted[bf / 2] > 2.5 * floor || floor < 1e-12 { continue; }
            let lv: Vec<f64> = col.chunks(8).map(|c| 10.0 * (c.iter().sum::<f64>() / c.len() as f64 + 1e-30).log10()).collect();
            // robust sigma (1.4826 x MAD) so the odd drum hit in a quiet bin doesn't count as flutter
            let med = |v: &mut Vec<f64>| { v.sort_by(|a, b| a.total_cmp(b)); v[v.len() / 2] };
            let m = med(&mut lv.clone());
            acc[b].0 += 1.4826 * med(&mut lv.iter().map(|v| (v - m).abs()).collect());
            acc[b].1 += 1;
        }
    }
    acc.iter().map(|(s, n)| (if *n > 0 { s / *n as f64 } else { f64::NAN }, *n)).collect()
}

/// Noise-substitution cousin of AAC PNS: where a bin is haze-dominated, pull its
/// level toward its own `smooth_ms` moving average, flattening the frame-to-frame
/// flutter of the static. Original phase, one gain for all channels, so the stereo
/// image can't move. Frames well above the floor (music, hits) are left alone.
fn smooth_haze(p: &Params, x: &[Vec<f64>], sr: u32, smooth_ms: f64) -> Vec<Vec<f64>> {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let (frames, bins) = (st.frames, st.bins());
    let power = power_grid(&st, x);
    let noise = noise_floor(p, &power, frames, bins, sr as f64);
    let r = ((smooth_ms / 1000.0 * sr as f64 / p.hop as f64 / 2.0).round() as usize).max(1);
    let lim = 10f64.powf(SMOOTH_LIMIT_DB / 20.0);
    let mut gain = vec![1.0f32; frames * bins];
    for f in 0..bins {
        // running sum for a centred moving average over 2r+1 frames
        let col: Vec<f64> = (0..frames).map(|t| power[t * bins + f] as f64).collect();
        let mut pre = vec![0.0; frames + 1];
        for t in 0..frames { pre[t + 1] = pre[t] + col[t]; }
        for t in 0..frames {
            let (a, b) = (t.saturating_sub(r), (t + r + 1).min(frames));
            let avg = (pre[b] - pre[a]) / (b - a) as f64;
            let floor = noise[t * bins + f] as f64 / p.noise_scale;
            let snr = col[t] / (floor + 1e-30);
            // 1 at/below the floor, 0 from SMOOTH_SNR x floor up
            let w = ((SMOOTH_SNR - snr) / (SMOOTH_SNR - 1.0)).clamp(0.0, 1.0);
            gain[t * bins + f] = (avg / (col[t] + 1e-30)).powf(w / 2.0).clamp(1.0 / lim, lim) as f32;
        }
    }
    x.iter().map(|c| st.resynth(c, |t, s| for f in 0..bins { s[f] *= gain[t * bins + f] as f64; })).collect()
}

/// Diagnostic for repetition averaging. Returns (best-match cosine similarity per
/// 1 s segment, per-band complex coherence of the top pairs after sample alignment).
/// Magnitude similarity says repeats exist; coherence says whether averaging them
/// as waveforms (the only way to cut haze power, not just its texture) could work.
fn repeats(p: &Params, x: &[Vec<f64>], sr: u32) -> (Vec<f64>, Vec<f64>) {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let (frames, bins) = (st.frames, st.bins());
    let power = power_grid(&st, x);
    let hz = |f: usize| f as f64 * sr as f64 / p.n as f64;
    // 40 log-spaced bands, 100 Hz - 16 kHz
    let edges: Vec<usize> = (0..=40).map(|i| ((100.0 * 160f64.powf(i as f64 / 40.0)) / hz(1)).round() as usize).collect();
    let q = (0.25 * sr as f64 / p.hop as f64).round() as usize; // quarter-second cells
    let cells = frames / q;
    let feat: Vec<Vec<f64>> = (0..cells).map(|c| (0..40).map(|b| {
        let s: f64 = (c * q..(c + 1) * q).flat_map(|t| (edges[b]..edges[b + 1].max(edges[b] + 1)).map(move |f| (t, f))).map(|(t, f)| power[t * bins + f] as f64).sum();
        10.0 * (s + 1e-20).log10()
    }).collect()).collect();
    // segment = 4 consecutive cells (1 s), mean-removed, unit norm
    let seg: Vec<Vec<f64>> = (0..cells.saturating_sub(3)).map(|i| {
        let mut v: Vec<f64> = (i..i + 4).flat_map(|c| feat[c].iter().copied()).collect();
        let m = v.iter().sum::<f64>() / v.len() as f64;
        v.iter_mut().for_each(|a| *a -= m);
        let nrm = v.iter().map(|a| a * a).sum::<f64>().sqrt() + 1e-12;
        v.iter_mut().for_each(|a| *a /= nrm);
        v
    }).collect();
    let gap = 16; // >= 4 s apart
    let best: Vec<(f64, usize)> = (0..seg.len()).map(|i| (0..seg.len()).filter(|j| i.abs_diff(*j) >= gap)
        .map(|j| (seg[i].iter().zip(&seg[j]).map(|(a, b)| a * b).sum::<f64>(), j))
        .fold((-1.0, 0), |a, b| if b.0 > a.0 { b } else { a })).collect();
    let mut order: Vec<usize> = (0..best.len()).collect();
    order.sort_by(|a, b| best[*b].0.total_cmp(&best[*a].0));
    // sample-align top pairs by cross-correlation (+-1 hop), then per-band coherence
    let mono: Vec<f64> = (0..x[0].len()).map(|i| x.iter().map(|c| c[i]).sum::<f64>() / x.len() as f64).collect();
    let seg_len = sr as usize;
    let mut acc = vec![(Complex::<f64>::default(), 0.0f64, 0.0f64); BANDS.len()];
    let mut spec_a = vec![Complex::default(); bins];
    let mut spec_b = vec![Complex::default(); bins];
    let fst = Stft::new(p.n, p.hop, mono.len());
    let fft_len = (seg_len + 2 * (q * p.hop / 2 + p.hop)).next_power_of_two() * 2;
    let mut planner = RealFftPlanner::<f64>::new();
    let (xfwd, xinv) = (planner.plan_fft_forward(fft_len), planner.plan_fft_inverse(fft_len));
    for &i in order.iter().take(200) {
        let (a0, b0) = (i * q * p.hop, best[i].1 * q * p.hop);
        if a0 + seg_len + p.hop > mono.len() { continue; }
        // FFT cross-correlation over +-half a cell (matches are only cell-accurate)
        let reach = q * p.hop / 2 + p.hop;
        if b0 < reach || b0 + seg_len + reach > mono.len() { continue; }
        let fft_n = (seg_len + 2 * reach).next_power_of_two() * 2;
        let mut fa = vec![0.0; fft_n];
        let mut fb = vec![0.0; fft_n];
        fa[..seg_len].copy_from_slice(&mono[a0..a0 + seg_len]);
        fb[..seg_len + 2 * reach].copy_from_slice(&mono[b0 - reach..b0 + seg_len + reach]);
        let (mut sa, mut sb) = (xfwd.make_output_vec(), xfwd.make_output_vec());
        xfwd.process(&mut fa, &mut sa).unwrap();
        xfwd.process(&mut fb, &mut sb).unwrap();
        let mut prod: Vec<Complex<f64>> = sa.iter().zip(&sb).map(|(a, b)| a.conj() * b).collect();
        prod[0].im = 0.0;
        let last = prod.len() - 1;
        prod[last].im = 0.0;
        let mut corr = vec![0.0; fft_n];
        xinv.process(&mut prod, &mut corr).unwrap();
        let k = (0..=2 * reach).max_by(|a, b| corr[*a].total_cmp(&corr[*b])).unwrap();
        let lag = k as isize - reach as isize;
        let shift = b0 as isize + lag - a0 as isize;
        let shifted: Vec<f64> = (0..mono.len()).map(|k| { let j = k as isize + shift; if j >= 0 && (j as usize) < mono.len() { mono[j as usize] } else { 0.0 } }).collect();
        let t0 = a0 / p.hop + 3;
        for t in t0..t0 + q * 4 {
            fst.frame(&mono, t, &mut spec_a);
            fst.frame(&shifted, t, &mut spec_b);
            for f in 1..bins {
                let b = BANDS.iter().position(|(lo, hi)| hz(f) >= *lo && hz(f) < *hi).unwrap();
                acc[b].0 += spec_a[f] * spec_b[f].conj();
                acc[b].1 += spec_a[f].norm_sqr();
                acc[b].2 += spec_b[f].norm_sqr();
            }
        }
    }
    let coh = acc.iter().map(|(c, pa, pb)| c.norm() / (pa * pb).sqrt().max(1e-30)).collect();
    (best.iter().map(|b| b.0).collect(), coh)
}

const WIDTH_BANDS: [(f64, f64); 8] = [(0.0, 350.0), (350.0, 1000.0), (1000.0, 3500.0), (3500.0, 7000.0), (7000.0, 10000.0), (10000.0, 13000.0), (13000.0, 16000.0), (16000.0, 20000.0)];

/// Per WIDTH_BANDS: (energy-weighted L/R coherence, side/mid energy in dB).
/// Coherence is taken per frame across ~1/7-octave sub-bands, so a band that was
/// coded as one signal panned L/R (intensity stereo) reads exactly 1.
fn width(p: &Params, x: &[Vec<f64>], sr: u32) -> Vec<(f64, f64)> {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let bins = st.bins();
    let hz = |f: usize| f as f64 * sr as f64 / p.n as f64;
    let edges: Vec<usize> = (0..=60).map(|i| (50.0 * 400f64.powf(i as f64 / 60.0) / hz(1)).round() as usize).collect();
    let (mut sl, mut sr_) = (vec![Complex::default(); bins], vec![Complex::default(); bins]);
    let mut acc = vec![(0.0f64, 0.0f64, 0.0f64, 0.0f64); WIDTH_BANDS.len()]; // coh*w, w, side, mid
    for t in 0..st.frames {
        st.frame(&x[0], t, &mut sl);
        st.frame(&x[1], t, &mut sr_);
        for w in edges.windows(2) {
            let (a, b) = (w[0].max(1), w[1].min(bins).max(w[0] + 1));
            if a >= bins { break; }
            let Some(k) = WIDTH_BANDS.iter().position(|(lo, hi)| hz(a) >= *lo && hz(a) < *hi) else { continue };
            let (mut c, mut pl, mut pr, mut ps, mut pm) = (Complex::<f64>::default(), 0.0, 0.0, 0.0, 0.0);
            for f in a..b.min(bins) {
                c += sl[f] * sr_[f].conj();
                pl += sl[f].norm_sqr();
                pr += sr_[f].norm_sqr();
                ps += ((sl[f] - sr_[f]) * 0.5).norm_sqr();
                pm += ((sl[f] + sr_[f]) * 0.5).norm_sqr();
            }
            if pl * pr < 1e-24 { continue; }
            let e = pl + pr;
            acc[k].0 += e * c.norm() / (pl * pr).sqrt();
            acc[k].1 += e;
            acc[k].2 += ps;
            acc[k].3 += pm;
        }
    }
    acc.iter().map(|(cw, w, s, m)| (cw / w.max(1e-30), 10.0 * (s / m.max(1e-30)).log10())).collect()
}

struct Widen { ref_side_mid_db: f64, top_before_db: f64, top_after_db: f64 }

/// Restores high-band width that collapsed toward mono. Above WIDEN_FROM_HZ, adds
/// g(f) * delayed mid into the side so each bin's long-term side/mid ratio rises to
/// the track's own 3.5-13 kHz ratio (scaled by `amount`). Only side changes, so the
/// mono fold-down L+R is bit-for-bit the same. A delay of a few ms reads as width at
/// these frequencies; its comb spacing (~200 Hz) is far below what the ear resolves up there.
fn widen(p: &Params, x: &[Vec<f64>], sr: u32, amount: f64) -> Option<(Vec<Vec<f64>>, Widen)> {
    if x.len() != 2 { return None; }
    let st = Stft::new(p.n, p.hop, x[0].len());
    let bins = st.bins();
    let hz = |f: usize| f as f64 * sr as f64 / p.n as f64;
    let (mut sl, mut sr_) = (vec![Complex::default(); bins], vec![Complex::default(); bins]);
    let (mut side, mut mid) = (vec![0.0f64; bins], vec![0.0f64; bins]);
    for t in 0..st.frames {
        st.frame(&x[0], t, &mut sl);
        st.frame(&x[1], t, &mut sr_);
        for f in 0..bins {
            side[f] += ((sl[f] - sr_[f]) * 0.5).norm_sqr();
            mid[f] += ((sl[f] + sr_[f]) * 0.5).norm_sqr();
        }
    }
    let ratio = |a: f64, b: f64| { let r = (0..bins).filter(|f| hz(*f) >= a && hz(*f) < b); let (s, m) = r.fold((0.0, 0.0), |(s, m), f| (s + side[f], m + mid[f])); s / m.max(1e-30) };
    let target = ratio(3500.0, 13000.0) * amount;
    let top = sr as f64 / 2.0;
    let before = ratio(16000.0, top);
    let ramp_end = WIDEN_FROM_HZ + 2000.0;
    // side power to add = (target - current) * mid, delivered as g * mid
    let g: Vec<f64> = (0..bins).map(|f| {
        if hz(f) < WIDEN_FROM_HZ || mid[f] < 1e-30 { return 0.0; }
        let ramp = 0.5 - 0.5 * (PI * ((hz(f) - WIDEN_FROM_HZ) / (ramp_end - WIDEN_FROM_HZ)).min(1.0)).cos();
        ((target - side[f] / mid[f]).max(0.0) * ramp).sqrt().min(WIDEN_MAX_G)
    }).collect();
    let d = WIDEN_DELAY_MS / 1000.0 * sr as f64;
    let rot: Vec<Complex<f64>> = (0..bins).map(|f| Complex::from_polar(1.0, -2.0 * PI * f as f64 * d / p.n as f64)).collect();
    let other = |c: usize| &x[1 - c];
    let out: Vec<Vec<f64>> = (0..2).map(|c| {
        let sign = if c == 0 { 1.0 } else { -1.0 };
        let mut o = vec![Complex::default(); bins];
        st.resynth(&x[c], |t, s| {
            st.frame(other(c), t, &mut o);
            for f in 0..bins { if g[f] > 0.0 { s[f] += (s[f] + o[f]) * 0.5 * rot[f] * (sign * g[f]); } }
        })
    }).collect();
    let after = {
        let st2 = Stft::new(p.n, p.hop, out[0].len());
        let (mut s2, mut m2) = (0.0, 0.0);
        for t in 0..st2.frames {
            st2.frame(&out[0], t, &mut sl);
            st2.frame(&out[1], t, &mut sr_);
            for f in (0..bins).filter(|f| hz(*f) >= 16000.0) { s2 += ((sl[f] - sr_[f]) * 0.5).norm_sqr(); m2 += ((sl[f] + sr_[f]) * 0.5).norm_sqr(); }
        }
        s2 / m2.max(1e-30)
    };
    let db = |r: f64| 10.0 * r.max(1e-30).log10();
    Some((out, Widen { ref_side_mid_db: db(target), top_before_db: db(before), top_after_db: db(after) }))
}

/// Vertical phase coherence at tonal peaks: for a steady partial under our
/// (frame-start-aligned) Hann window, bins k-1 and k+1 sit exactly pi from bin k
/// whatever the fractional frequency. Returns per BANDS: (power-weighted mean
/// deviation in degrees, fraction of peaks deviating > 30 degrees, peak count).
fn phase_dev(p: &Params, x: &[Vec<f64>], sr: u32) -> Vec<(f64, f64, usize)> {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let (frames, bins) = (st.frames, st.bins());
    let hz = |f: usize| f as f64 * sr as f64 / p.n as f64;
    let wrap = |a: f64| (a + PI).rem_euclid(2.0 * PI) - PI;
    let mut acc = vec![(0.0f64, 0.0f64, 0usize, 0usize); BANDS.len()]; // dev*w, w, n>30, n
    let mut spec = vec![Complex::default(); bins];
    for c in x {
        for t in 0..frames {
            st.frame(c, t, &mut spec);
            let pw: Vec<f64> = spec.iter().map(|s| s.norm_sqr()).collect();
            for k in 3..bins - 3 {
                if !(pw[k] > pw[k - 1] && pw[k] > pw[k + 1] && pw[k] > pw[k - 2] && pw[k] > pw[k + 2]) { continue; }
                // tonal = stands 10 dB over its spectral neighbourhood (a rolling time
                // floor would call a sustained note "noise")
                let mut hood: Vec<f64> = pw[k.saturating_sub(8)..(k + 9).min(bins)].to_vec();
                let mid = hood.len() / 2;
                if pw[k] < 10.0 * *hood.select_nth_unstable_by(mid, |a, b| a.total_cmp(b)).1 { continue; }
                let ph = spec[k].arg();
                let dev = (wrap(spec[k - 1].arg() - ph - PI).abs() + wrap(spec[k + 1].arg() - ph - PI).abs()) / 2.0;
                let b = BANDS.iter().position(|(lo, hi)| hz(k) >= *lo && hz(k) < *hi).unwrap();
                acc[b].0 += dev.to_degrees() * pw[k];
                acc[b].1 += pw[k];
                acc[b].2 += (dev.to_degrees() > 30.0) as usize;
                acc[b].3 += 1;
            }
        }
    }
    acc.iter().map(|(d, w, big, n)| (d / w.max(1e-30), *big as f64 / (*n).max(1) as f64, *n)).collect()
}

fn bark(hz: f64) -> f64 { 13.0 * (0.00076 * hz).atan() + 3.5 * (hz / 7500.0).powi(2).atan() }

/// Per-bin masking threshold [frames][bins] from the music estimate max(P - N, 0):
/// 1-Bark band energies, Schroeder spreading, and Johnston's tonality-dependent
/// offset per masker band: alpha*(14.5 + z) + (1 - alpha)*5.5 dB, alpha from the
/// band's spectral flatness (tonal maskers hide far less). No temporal masking or
/// threshold in quiet, both of which would only add masking.
fn masking(power: &[f32], noise: &[f32], frames: usize, bins: usize, hz_per_bin: f64, noise_scale: f64) -> Vec<f32> {
    let band: Vec<usize> = (0..bins).map(|f| bark(f as f64 * hz_per_bin).floor() as usize).collect();
    let nb = band[bins - 1] + 1;
    let width: Vec<f64> = (0..nb).map(|b| band.iter().filter(|x| **x == b).count().max(1) as f64).collect();
    let sf: Vec<f64> = (0..nb * nb).map(|i| {
        let dz = (i % nb) as f64 - (i / nb) as f64; // maskee - masker
        10f64.powf((15.81 + 7.5 * (dz + 0.474) - 17.5 * (1.0 + (dz + 0.474).powi(2)).sqrt()) / 10.0)
    }).collect();
    let mut out = vec![0.0f32; frames * bins];
    let (mut e, mut lg, mut cnt) = (vec![0.0f64; nb], vec![0.0f64; nb], vec![0usize; nb]);
    for t in 0..frames {
        e.iter_mut().for_each(|v| *v = 0.0);
        lg.iter_mut().for_each(|v| *v = 0.0);
        cnt.iter_mut().for_each(|v| *v = 0);
        for f in 1..bins {
            let i = t * bins + f;
            let m = (power[i] as f64 - noise[i] as f64 / noise_scale).max(0.0);
            e[band[f]] += m;
            lg[band[f]] += (m + 1e-20).ln();
            cnt[band[f]] += 1;
        }
        // masker strength after its own tonality offset
        let masker: Vec<f64> = (0..nb).map(|m| {
            if cnt[m] == 0 || e[m] <= 0.0 { return 0.0; }
            let sfm_db = 10.0 * ((lg[m] / cnt[m] as f64).exp() / (e[m] / cnt[m] as f64)).log10();
            let alpha = (sfm_db / -60.0).clamp(0.0, 1.0);
            let off = alpha * (14.5 + m as f64) + (1.0 - alpha) * 5.5;
            e[m] * 10f64.powf(-off / 10.0)
        }).collect();
        for f in 0..bins {
            let b = band[f];
            let thr: f64 = (0..nb).map(|m| masker[m] * sf[m * nb + b]).sum();
            out[t * bins + f] = (thr / width[b]) as f32;
        }
    }
    out
}

/// Diagnostic: share of the energy the denoiser removes where the haze sits
/// below / around / above the masking threshold.
fn audibility(p: &Params, x: &[Vec<f64>], sr: u32) -> [f64; 3] {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let (frames, bins) = (st.frames, st.bins());
    let power = power_grid(&st, x);
    let noise = noise_floor(p, &power, frames, bins, sr as f64);
    let thr = masking(&power, &noise, frames, bins, sr as f64 / p.n as f64, p.noise_scale);
    let (_, gain, _) = denoise(p, x, sr);
    let mut acc = [0.0f64; 3];
    for i in 0..frames * bins {
        let removed = power[i] as f64 * (1.0 - (gain[i] as f64).powi(2));
        let nmr = 10.0 * ((noise[i] as f64 / p.noise_scale) / (thr[i] as f64 + 1e-30) + 1e-30).log10();
        acc[if nmr < -GATE_RAMP_DB { 0 } else if nmr > GATE_RAMP_DB { 2 } else { 1 }] += removed;
    }
    acc
}

/// Modulation spectrum of each high band's log-power envelope (~1 kHz envelope
/// rate, 4 s Welch segments, 0.25 Hz resolution). A codec running at a fixed
/// frame rate leaves a sharp peak at that rate; music modulates at beat and
/// syllable rates or in broad humps. Returns (envelope rate, per-band spectra).
fn modspec(x: &[Vec<f64>], sr: u32) -> (f64, Vec<Vec<f64>>) {
    let len = x[0].len();
    let n = len.next_power_of_two();
    let mut buf: Vec<f64> = (0..n).map(|i| if i < len { x.iter().map(|c| c[i]).sum::<f64>() / x.len() as f64 } else { 0.0 }).collect();
    let mut planner = RealFftPlanner::<f64>::new();
    let (fwd, inv) = (planner.plan_fft_forward(n), planner.plan_fft_inverse(n));
    let mut spec = fwd.make_output_vec();
    fwd.process(&mut buf, &mut spec).unwrap();
    let hz = sr as f64 / n as f64;
    let dec = sr as usize / 1000;
    let (seg, rate) = (4000, sr as f64 / dec as f64);
    let win = hann(seg);
    let seg_fft = planner.plan_fft_forward(seg);
    let bands = BANDS[2..].iter().map(|&(lo, hi)| {
        let mut s: Vec<Complex<f64>> = spec.iter().enumerate()
            .map(|(k, &c)| if (lo..hi).contains(&(k as f64 * hz)) { c } else { Complex::new(0.0, 0.0) }).collect();
        if let Some(last) = s.last_mut() { last.im = 0.0; }
        let mut band = inv.make_output_vec();
        inv.process(&mut s, &mut band).unwrap();
        // log so loud sections don't dominate the average
        let env: Vec<f64> = band[..len].chunks_exact(dec).map(|c| (c.iter().map(|v| v * v).sum::<f64>() + 1e-30).ln()).collect();
        let mut acc = vec![0.0; seg / 2 + 1];
        let mut out = seg_fft.make_output_vec();
        for start in (0..env.len().saturating_sub(seg)).step_by(seg / 2) {
            let w = &env[start..start + seg];
            let mean = w.iter().sum::<f64>() / seg as f64;
            let mut frame: Vec<f64> = w.iter().zip(&win).map(|(v, h)| (v - mean) * h).collect();
            seg_fft.process(&mut frame, &mut out).unwrap();
            for (a, c) in acc.iter_mut().zip(&out) { *a += c.norm_sqr(); }
        }
        acc
    }).collect();
    (rate, bands)
}

/// Peak prominence in dB at bin k: power over the median of +-5 Hz around it
/// (excluding +-1 Hz), taking the best bin within +-1 of k.
fn prominence(spec: &[f64], k: usize, bins_per_hz: f64) -> f64 {
    let (near, far) = (bins_per_hz.round() as usize, (5.0 * bins_per_hz).round() as usize);
    if k < far || k + far >= spec.len() { return 0.0; }
    let mut ring: Vec<f64> = (k - far..=k + far).filter(|&j| j.abs_diff(k) > near).map(|j| spec[j]).collect();
    ring.sort_by(f64::total_cmp);
    let peak = (k - 1..=k + 1).map(|j| spec[j]).fold(0.0, f64::max);
    10.0 * (peak / ring[ring.len() / 2]).log10()
}

/// Static EQ curve (dB per bin) toward a target long-term spectrum (dB at Hz):
/// smoothed over MATCH_SMOOTH_OCT octaves, level-neutral (the track-energy-
/// weighted mean correction is removed, so RMS stays put), capped at
/// +-MATCH_MAX_DB and faded out beyond MATCH_RANGE.
fn match_curve(p: &Params, x: &[Vec<f64>], sr: u32, target: impl Fn(f64) -> f64, cap: f64) -> Vec<f64> {
    let lt = ltas(&Stft::new(p.n, p.hop, x[0].len()), x);
    let hz = |f: usize| f as f64 * sr as f64 / p.n as f64;
    let raw: Vec<f64> = (0..lt.len()).map(|f| target(hz(f)) - lt[f]).collect();
    let half = 2f64.powf(MATCH_SMOOTH_OCT / 2.0);
    let mut c: Vec<f64> = (0..lt.len()).map(|f| {
        let (a, b) = (((f as f64 / half).floor() as usize).max(1), ((f as f64 * half).ceil() as usize).clamp(1, lt.len() - 1));
        raw[a..=b].iter().sum::<f64>() / (b - a + 1) as f64
    }).collect();
    let fade = |h: f64| (1.0 - (2.0 * (MATCH_RANGE.0 / h.max(1.0)).log2()).max(0.0) - (2.0 * (h / MATCH_RANGE.1).log2()).max(0.0)).clamp(0.0, 1.0);
    let (num, den) = (0..c.len()).filter(|&f| fade(hz(f)) == 1.0).fold((0.0, 0.0), |(n, d), f| { let w = 10f64.powf(lt[f] / 10.0); (n + w * c[f], d + w) });
    for (f, v) in c.iter_mut().enumerate() { *v = (*v - num / den).clamp(-cap, cap) * fade(hz(f)); }
    c
}

/// Third-octave band levels (dB, mean power per bin) from 31.5 Hz to 16 kHz,
/// shifted so the 125 Hz - 8 kHz points average 0 dB.
fn third_octaves(lt: &[f64], sr: u32, n: usize) -> Vec<(f64, f64)> {
    let mut v: Vec<(f64, f64)> = (0..30).map(|i| 31.5 * 2f64.powf(i as f64 / 3.0)).filter(|h| *h <= 16500.0).map(|h| {
        let bin = |f: f64| ((f * n as f64 / sr as f64).round() as usize).clamp(1, lt.len() - 1);
        let (a, b) = (bin(h / 2f64.powf(1.0 / 6.0)), bin(h * 2f64.powf(1.0 / 6.0)).max(bin(h / 2f64.powf(1.0 / 6.0))));
        (h, 10.0 * (lt[a..=b].iter().map(|d| 10f64.powf(d / 10.0)).sum::<f64>() / (b - a + 1) as f64).log10())
    }).collect();
    let mid: Vec<f64> = v.iter().filter(|(h, _)| (120.0..8500.0).contains(h)).map(|(_, d)| *d).collect();
    let off = mid.iter().sum::<f64>() / mid.len() as f64;
    v.iter_mut().for_each(|(_, d)| *d -= off);
    v
}

/// Core DSP. Returns (clean, gain mask [frames][bins], stft geometry).
/// Direct-form-I biquad.
fn biquad(x: &[f64], b: [f64; 3], a: [f64; 2]) -> Vec<f64> {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.iter().map(|&v| {
        let y = b[0] * v + b[1] * x1 + b[2] * x2 - a[0] * y1 - a[1] * y2;
        (x2, x1, y2, y1) = (x1, v, y1, y);
        y
    }).collect()
}

/// BS.1770-4 K-weighting (high shelf + high pass) at any sample rate: the
/// analog prototype through the bilinear transform, as libebur128 does; at
/// 48 kHz it reproduces the standard's coefficient table.
fn k_weight(x: &[f64], sr: u32) -> Vec<f64> {
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (PI * f0 / sr as f64).tan();
    let (vh, a0) = (10f64.powf(g / 20.0), 1.0 + k / q + k * k);
    let vb = vh.powf(0.4996667741545416);
    let shelf = biquad(x, [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0]);
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (PI * f0 / sr as f64).tan();
    let a0 = 1.0 + k / q + k * k;
    biquad(&shelf, [1.0, -2.0, 1.0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0])
}

/// Integrated loudness, BS.1770-4: 400 ms blocks every 100 ms, absolute gate
/// at -70 LUFS, relative gate 10 LU below the absolute-gated mean.
fn lufs(x: &[Vec<f64>], sr: u32) -> f64 {
    let kw: Vec<Vec<f64>> = x.iter().map(|c| k_weight(c, sr)).collect();
    let (blk, hop) = ((0.4 * sr as f64) as usize, (0.1 * sr as f64) as usize);
    let z: Vec<f64> = (0..kw[0].len().saturating_sub(blk) / hop + 1).filter(|j| j * hop + blk <= kw[0].len())
        .map(|j| kw.iter().map(|c| c[j * hop..j * hop + blk].iter().map(|v| v * v).sum::<f64>() / blk as f64).sum()).collect();
    let loud = |m: f64| -0.691 + 10.0 * m.log10();
    let gated_mean = |thr: f64| { let v: Vec<f64> = z.iter().copied().filter(|&m| loud(m) > thr).collect(); v.iter().sum::<f64>() / v.len() as f64 };
    loud(gated_mean(loud(gated_mean(-70.0)) - 10.0).max(1e-30))
}

/// Linked true-peak envelope: per sample, the largest |value| at the sample and
/// at the three 4x-oversampled points after it (windowed-sinc interpolation).
fn true_peak_env(x: &[Vec<f64>]) -> Vec<f64> {
    let h = TP_HALF_TAPS as isize;
    let taps: Vec<Vec<f64>> = (1..4).map(|p| (-h + 1..=h).map(|k| {
        let t = k as f64 - p as f64 / 4.0;
        let w = 0.5 + 0.5 * (PI * t / (h as f64 + 0.5)).cos();
        if t == 0.0 { 1.0 } else { (PI * t).sin() / (PI * t) * w }
    }).collect()).collect();
    let n = x[0].len();
    let mut env = vec![0.0f64; n];
    for c in x {
        for i in 0..n {
            let mut m = c[i].abs();
            for tp in &taps {
                let mut acc = 0.0;
                for (j, w) in tp.iter().enumerate() {
                    let idx = i as isize - h + 1 + j as isize;
                    if idx >= 0 && (idx as usize) < n { acc += c[idx as usize] * w; }
                }
                m = m.max(acc.abs());
            }
            env[i] = env[i].max(m);
        }
    }
    env
}

/// Minimum over [i - r, i + r] for every i (monotonic deque, O(n)).
fn sliding_min(v: &[f64], r: usize) -> Vec<f64> {
    let mut dq = std::collections::VecDeque::new();
    let mut next = 0;
    (0..v.len()).map(|i| {
        while next < v.len() && next <= i + r {
            while dq.back().is_some_and(|&j: &usize| v[j] >= v[next]) { dq.pop_back(); }
            dq.push_back(next);
            next += 1;
        }
        while dq.front().is_some_and(|&j| j + r < i) { dq.pop_front(); }
        v[*dq.front().unwrap()]
    }).collect()
}

/// True-peak limiter, offline and linked across channels. The gain each sample
/// needs is min-filtered over +-attack, then box-averaged over +-attack/2: every
/// sample in that box saw this sample in its min window, so the smoothed gain
/// never exceeds what this sample needs, and it ramps in before the peak.
/// Release is a one-pole rise back toward that ceiling. Returns (out, max GR dB).
fn limit(x: &[Vec<f64>], sr: u32, ceiling: f64) -> (Vec<Vec<f64>>, f64) {
    let want: Vec<f64> = true_peak_env(x).iter().map(|p| (ceiling / p.max(1e-12)).min(1.0)).collect();
    let l = ((LIMIT_ATTACK_MS * 1e-3 * sr as f64) as usize).max(2);
    let h = sliding_min(&want, l);
    let mut pre = vec![0.0; h.len() + 1];
    for (i, v) in h.iter().enumerate() { pre[i + 1] = pre[i] + v; }
    let half = l / 2;
    let r = (-1.0 / (LIMIT_RELEASE_MS * 1e-3 * sr as f64)).exp();
    let mut g = 1.0f64;
    let gain: Vec<f64> = (0..h.len()).map(|i| {
        let (a, b) = (i.saturating_sub(half), (i + half + 1).min(h.len()));
        let s = (pre[b] - pre[a]) / (b - a) as f64;
        g = if s < g { s } else { s + (g - s) * r };
        g
    }).collect();
    let max_gr = -20.0 * gain.iter().copied().fold(1.0, f64::min).log10();
    (x.iter().map(|c| c.iter().zip(&gain).map(|(v, g)| v * g).collect()).collect(), max_gr)
}

struct Master { in_lufs: f64, gain_db: f64, max_gr_db: f64, out_lufs: f64, out_tp_db: f64 }

/// Gain to the loudness target, then true-peak limit. Limiting lowers loudness
/// and interpolation can overshoot slightly, so iterate: re-aim the gain at the
/// measured shortfall and tighten the ceiling by any measured overshoot.
fn master(x: &[Vec<f64>], sr: u32, target: f64) -> (Vec<Vec<f64>>, Master) {
    let in_lufs = lufs(x, sr);
    let ceiling = 10f64.powf(TRUE_PEAK_DBTP / 20.0);
    let (mut gain_db, mut ceil) = (target - in_lufs, ceiling);
    let mut last = None;
    for _ in 0..6 {
        let g = 10f64.powf(gain_db / 20.0);
        let (out, max_gr_db) = limit(&x.iter().map(|c| c.iter().map(|v| v * g).collect()).collect::<Vec<Vec<f64>>>(), sr, ceil);
        let out_lufs = lufs(&out, sr);
        let tp = true_peak_env(&out).into_iter().fold(0.0, f64::max);
        let done = tp <= ceiling * 1.0001 && (out_lufs - target).abs() < 0.05;
        if tp > ceiling { ceil *= ceiling / tp; }
        gain_db += target - out_lufs;
        last = Some((out, Master { in_lufs, gain_db: gain_db - (target - out_lufs), max_gr_db, out_lufs, out_tp_db: 20.0 * tp.log10() }));
        if done { break; }
    }
    last.unwrap()
}

/// `sig` band-limited to [lo, hi) Hz by zeroing bins of one full-length FFT.
fn band_pass(sig: &[f64], sr: u32, lo: f64, hi: f64) -> Vec<f64> {
    let n = sig.len().next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let (fwd, inv) = (planner.plan_fft_forward(n), planner.plan_fft_inverse(n));
    let mut buf: Vec<f64> = sig.iter().copied().chain(std::iter::repeat(0.0)).take(n).collect();
    let mut spec = fwd.make_output_vec();
    fwd.process(&mut buf, &mut spec).unwrap();
    for (k, c) in spec.iter_mut().enumerate() {
        if !(lo..hi).contains(&(k as f64 * sr as f64 / n as f64)) { *c = Complex::new(0.0, 0.0); }
    }
    if let Some(last) = spec.last_mut() { last.im = 0.0; }
    let mut out = inv.make_output_vec();
    inv.process(&mut spec, &mut out).unwrap();
    out.truncate(sig.len());
    out.iter_mut().for_each(|v| *v /= n as f64);
    out
}

/// Sample period of the decoder grid: the fewest whole samples holding whole cycles
/// of DEBUZZ_HZ (240 at 48 kHz; 441 = two cycles at 44.1 kHz, e.g. a resampled export).
/// None at odd rates where that takes more than 4 cycles.
fn grid_period(sr: u32) -> Option<usize> {
    let gcd = |mut a: u32, mut b: u32| { while b != 0 { (a, b) = (b, a % b); } a };
    let per = sr / gcd(sr, DEBUZZ_HZ);
    (per * DEBUZZ_HZ <= 4 * sr).then_some(per as usize)
}

/// Mono, above 1 kHz so bass doesn't drown a decoder grid: what the grid tests fold.
fn hp_mono(x: &[Vec<f64>], sr: u32) -> Vec<f64> {
    let mono: Vec<f64> = (0..x[0].len()).map(|i| x.iter().map(|c| c[i]).sum::<f64>() / x.len() as f64).collect();
    band_pass(&mono, sr, 1000.0, f64::MAX)
}

/// Fold `hp` every `per` samples over the whole track. Returns how far the synchronous
/// average stands above chance in dB (its energy vs the variance/k random content
/// would leave in it), and the repeating part's power per sample (energy - chance).
fn fold(hp: &[f64], per: usize) -> (f64, f64) {
    let k = hp.len() / per;
    let (mut sum, mut sq) = (vec![0.0; per], vec![0.0; per]);
    for (i, v) in hp[..k * per].iter().enumerate() { sum[i % per] += v; sq[i % per] += v * v; }
    let mean_sq = sum.iter().map(|s| (s / k as f64).powi(2)).sum::<f64>() / per as f64;
    let var = sum.iter().zip(&sq).map(|(s, q)| q / k as f64 - (s / k as f64).powi(2)).sum::<f64>() / per as f64;
    (10.0 * (mean_sq / (var / k as f64 + 1e-30)).log10(), (mean_sq - var / k as f64).max(0.0))
}

fn grid_snr(x: &[Vec<f64>], sr: u32, per: usize) -> f64 { fold(&hp_mono(x, sr), per).0 }

/// Fold score for every period in `lo..=hi` at once: 10log10(1 + 2 sum_d rho(d*per)),
/// rho the autocorrelation (mono, above 1 kHz) averaged over GRID_BLOCK blocks. A
/// waveform repeating every `per` samples adds to every term, so this is the fold
/// SNR over a GRID_BLOCK/2 window; chance sits near 0 dB.
fn grid_scan(hp: &[f64], lo: usize, hi: usize) -> Vec<(usize, f64)> {
    let (b, lmax) = (GRID_BLOCK.min(hp.len()), GRID_BLOCK / 2);
    let mut planner = RealFftPlanner::<f64>::new();
    let (fwd, inv) = (planner.plan_fft_forward(2 * b), planner.plan_fft_inverse(2 * b));
    let (mut r, mut pairs) = (vec![0.0; lmax], vec![0.0; lmax]);
    for blk in hp.chunks(b).filter(|c| c.len() * 2 >= b) {
        let mut buf: Vec<f64> = blk.iter().copied().chain(std::iter::repeat(0.0)).take(2 * b).collect();
        let mut spec = fwd.make_output_vec();
        fwd.process(&mut buf, &mut spec).unwrap();
        spec.iter_mut().for_each(|c| *c = Complex::new(c.norm_sqr(), 0.0));
        inv.process(&mut spec, &mut buf).unwrap();
        for l in 0..lmax.min(blk.len()) { r[l] += buf[l]; pairs[l] += (blk.len() - l) as f64; }
    }
    let rho = |l: usize| r[l] / pairs[l] / (r[0] / pairs[0] + 1e-30);
    (lo..=hi.min(lmax - 1)).map(|per| {
        let s: f64 = (1..).map(|d| d * per).take_while(|&l| l < lmax).map(rho).sum();
        (per, 10.0 * (1.0 + 2.0 * s).max(1e-3).log10())
    }).collect()
}

/// Subtract a waveform that repeats every `per` samples. Its shape holds across a
/// song while its level drifts, so per channel: one whole-song synchronous average
/// for the shape, then one gain per Hann window of DEBUZZ_WINDOW_S (50% overlap),
/// overlap-added. Fitting one number per window instead of `per` keeps the music's
/// own leakage into the estimate ~20 dB lower than a per-window average.
/// Windows holding fewer than 50 periods (file edges) are skipped as too noisy.
fn debuzz(x: &[Vec<f64>], sr: u32, per: usize) -> Vec<Vec<f64>> {
    let len = x[0].len();
    let w_len = ((DEBUZZ_WINDOW_S * sr as f64) as usize / per * per).max(per);
    let hop = w_len / 2;
    let win = hann(w_len);
    let p = params_for(sr);
    let st = Stft::new(p.n, p.hop, len);
    x.iter().map(|c| {
        let k = (len / per).max(1);
        let mut shape = vec![0.0; per];
        for (i, v) in c[..k * per].iter().enumerate() { shape[i % per] += v / k as f64; }
        let energy = shape.iter().map(|v| v * v).sum::<f64>() + 1e-30;
        let (mut tmpl, mut norm) = (vec![0.0; len], vec![0.0; len]);
        let mut s = -(hop as isize);
        while s < len as isize {
            let (a, b) = (s.max(0) as usize, ((s + w_len as isize) as usize).min(len));
            if b - a >= 50 * per {
                let (mut num, mut den) = (vec![0.0; per], vec![0.0; per]);
                for i in a..b { let w = win[(i as isize - s) as usize]; num[i % per] += c[i] * w; den[i % per] += w; }
                let g = ((0..per).map(|j| num[j] / den[j] * shape[j]).sum::<f64>() / energy).max(0.0);
                for i in a..b {
                    let w = win[(i as isize - s) as usize];
                    tmpl[i] += g * shape[i % per] * w;
                    norm[i] += w;
                }
            }
            s += hop as isize;
        }
        let t: Vec<f64> = tmpl.iter().zip(&norm).map(|(t, n)| if *n > 1e-9 { t / n } else { 0.0 }).collect();
        // A steady note on a multiple of 200 Hz (any A at 440 Hz tuning: 5 x 440 = 2200)
        // can leak into the shape, and subtracting it where the note has stopped rings
        // for seconds (a salsa trumpet did). Real buzz never outweighs the signal it sits in, so per
        // STFT bin the template is scaled by min(1, |x|^2 / |t|^2).
        let mut ts = vec![Complex::default(); st.bins()];
        st.resynth(c, |f, s| {
            st.frame(&t, f, &mut ts);
            for (a, b) in s.iter_mut().zip(&ts) { *a -= b * (a.norm_sqr() / (b.norm_sqr() + 1e-30)).min(1.0); }
        })
    }).collect()
}

/// Where a song that stops mid-sound ends (the sample after its last non-silent
/// one), and its last 250 ms level in dB re the song (mean RMS of its louder half
/// of 1 s blocks). None when it ends quietly, as a mastered song does.
fn abrupt_end(x: &[Vec<f64>], sr: u32) -> Option<(usize, f64)> {
    let pw: Vec<f64> = (0..x[0].len()).map(|i| x.iter().map(|c| c[i] * c[i]).sum::<f64>() / x.len() as f64).collect();
    let end = pw.iter().rposition(|p| *p > 1e-8)? + 1;
    let rms = |s: &[f64]| (s.iter().sum::<f64>() / s.len() as f64).sqrt();
    let mut blocks: Vec<f64> = pw[..end].chunks_exact(sr as usize).map(rms).collect();
    if blocks.is_empty() { return None; }
    blocks.sort_by(|a, b| b.total_cmp(a));
    let half = blocks.len().div_ceil(2);
    let level = blocks[..half].iter().sum::<f64>() / half as f64;
    let db = 20.0 * (rms(&pw[end.saturating_sub(sr as usize / 4)..end]) / (level + 1e-30) + 1e-30).log10();
    (db > END_ABRUPT_DB).then_some((end, db))
}

/// Raised-cosine fade-in over EDGE_FADE_MS, and fade-out over the same, or over
/// END_FADE_S ending at `end` (silence after) for a song that stops mid-sound. In place.
fn edge_fade(x: &mut [Vec<f64>], sr: u32, end: Option<usize>) {
    let len = x[0].len();
    let n = ((EDGE_FADE_MS * 1e-3 * sr as f64) as usize).min(len / 2);
    let (stop, m) = end.map_or((len, n), |e| (e, ((END_FADE_S * sr as f64) as usize).min(e / 4).max(n)));
    let g = |i: usize, n: usize| 0.5 - 0.5 * (PI * i as f64 / n as f64).cos();
    for c in x.iter_mut() {
        for i in 0..n { c[i] *= g(i, n); }
        for i in 0..m { c[stop - 1 - i] *= g(i, m); }
        c[stop..].iter_mut().for_each(|v| *v = 0.0);
    }
}

/// `f` on `x` with edge_fade applied, `x` given back unfaded afterwards: the stage
/// chain stays unfaded, so the fades don't compound from stage to stage.
fn with_fades<R>(x: &mut [Vec<f64>], sr: u32, end: Option<usize>, f: impl FnOnce(&[Vec<f64>]) -> R) -> R {
    let len = x[0].len();
    let head = ((EDGE_FADE_MS * 1e-3 * sr as f64) as usize).min(len / 2);
    let tail = end.map_or(head, |e| len - e + ((END_FADE_S * sr as f64) as usize).min(e / 4).max(head));
    let saved: Vec<(Vec<f64>, Vec<f64>)> = x.iter().map(|c| (c[..head].to_vec(), c[len - tail..].to_vec())).collect();
    edge_fade(x, sr, end);
    let r = f(x);
    for (c, (h, t)) in x.iter_mut().zip(saved) {
        c[..head].copy_from_slice(&h);
        c[len - tail..].copy_from_slice(&t);
    }
    r
}

fn denoise(p: &Params, x: &[Vec<f64>], sr: u32) -> (Vec<Vec<f64>>, Vec<f32>, Stft) {
    let st = Stft::new(p.n, p.hop, x[0].len());
    let (frames, bins) = (st.frames, st.bins());
    let power = power_grid(&st, x);
    let noise = noise_floor(p, &power, frames, bins, sr as f64);
    let strength = p.strength.unwrap_or_else(|| {
        let fl = flutter_grid(p, &power, &noise, frames, bins, sr)[3].0;
        let s = haze_strength(fl);
        say!("strength   {s:.2} (haze flutter 7-12 kHz {fl:.1} dB; full strength from {STRENGTH_FULL_FLUTTER_DB} dB)");
        s
    });
    let p = &Params { strength: Some(strength), ..*p };
    let hz = |f: usize| f as f64 * sr as f64 / p.n as f64;
    let floor_g: Vec<f64> = (0..bins).map(|f| 10f64.powf(-floor_db(hz(f)) / 20.0)).collect();
    let alphas: Vec<f64> = (0..bins).map(|f| alpha(p, hz(f))).collect();
    // gain in dB, floored, so the blur never sees -inf
    let mut gdb = vec![0.0f32; frames * bins];
    for i in 0..frames * bins {
        let f = i % bins;
        let g = (1.0 - alphas[f] * noise[i] as f64 / (power[i] as f64 + 1e-20)).max(0.0).sqrt().max(floor_g[f]);
        gdb[i] = (20.0 * g.log10()) as f32;
    }
    // audibility weight: 0 where the haze is masked by the music, 1 where exposed.
    // Blurred on its own and applied after the mask blur, so gated-off neighbours
    // don't dilute the attenuation where haze is audible.
    let audible = p.gate.then(|| {
        let thr = masking(&power, &noise, frames, bins, sr as f64 / p.n as f64, p.noise_scale);
        let mut a: Vec<f32> = (0..frames * bins).map(|i| {
            let nmr = 10.0 * ((noise[i] as f64 / p.noise_scale) / (thr[i] as f64 + 1e-30) + 1e-30).log10();
            ((nmr + GATE_RAMP_DB) / (2.0 * GATE_RAMP_DB)).clamp(0.0, 1.0) as f32
        }).collect();
        // grow audible regions first so the blur's softening lands on the masked side
        let src = a.clone();
        for t in 0..frames {
            for f in 0..bins {
                let mut m = 0.0f32;
                for tt in t.saturating_sub(GATE_DILATE)..(t + GATE_DILATE + 1).min(frames) {
                    for ff in f.saturating_sub(GATE_DILATE)..(f + GATE_DILATE + 1).min(bins) { m = m.max(src[tt * bins + ff]); }
                }
                a[t * bins + f] = m;
            }
        }
        blur(&mut a, frames, bins, p.sigma_f, p.sigma_t);
        a
    });
    drop(noise);
    blur(&mut gdb, frames, bins, p.sigma_f, p.sigma_t);
    if let Some(a) = &audible { gdb.iter_mut().zip(a).for_each(|(g, w)| *g *= w); }
    if let Some(st) = p.strength.filter(|s| *s != 1.0) { gdb.iter_mut().for_each(|g| *g *= st as f32); }
    let protect = if p.protect_onsets { onset_weights(&power, frames, bins, sr as f64 / p.n as f64) } else { vec![0.0; frames] };
    let hits = protect.iter().filter(|w| **w == 1.0).count() / 2; // two full-weight frames per hit
    if p.protect_onsets { say!("onsets     {hits} protected ({:.1}/s)", hits as f64 / (frames * p.hop) as f64 * sr as f64); }
    let gain: Vec<f32> = gdb.iter().enumerate().map(|(i, d)| {
        let g = (10f64.powf(*d as f64 / 20.0)).clamp(floor_g[i % bins], 1.0);
        (g + protect[i / bins] * (1.0 - g)) as f32
    }).collect();
    let clean = x.iter().map(|c| st.resynth(c, |t, s| for f in 0..bins { s[f] *= gain[t * bins + f] as f64; })).collect();
    (clean, gain, st)
}

/// The whole command line, as `sunscreen` runs it: `args[0]` is the program name.
pub fn run(args: Vec<String>) -> Result<(), String> {
    REPORT.with_borrow_mut(String::clear);
    let (flags, args): (Vec<String>, Vec<String>) = args.into_iter().partition(|a| a.starts_with("--"));
    if args.len() < 2 { return Err("usage: sunscreen <in.mp3|in.wav|in.flac|in.m4a> [out_dir] [title] [--no-gate] [--no-debuzz] [--strength=x] [--smooth[=ms]] [--widen[=x]] [--excite[=dB]] [--match[=ref.wav]] [--master[=LUFS]]".into()); }
    // --excite alone = default level; --excite=-6 = 6 dB under the extrapolated slope
    // --smooth alone = default window; --smooth=150 = 150 ms
    let smooth_ms = flags.iter().find_map(|f| f.strip_prefix("--smooth"))
        .map(|v| v.strip_prefix('=').map_or(SMOOTH_DEFAULT_MS, |n| n.parse().expect("--smooth=<ms>")));
    // --widen alone = match the track's own 3.5-13 kHz width; --widen=0.5 = half of that
    let widen_amt = flags.iter().find_map(|f| f.strip_prefix("--widen"))
        .map(|v| v.strip_prefix('=').map_or(1.0, |n| n.parse().expect("--widen=<amount>")));
    let excite_db = flags.iter().find_map(|f| f.strip_prefix("--excite"))
        .map(|v| v.strip_prefix('=').map_or(EXCITE_DEFAULT_DB, |n| n.parse().expect("--excite=<dB>")));
    // --master alone = -14 LUFS (streaming); --master=-9 for louder
    let master_lufs = flags.iter().find_map(|f| f.strip_prefix("--master"))
        .map(|v| v.strip_prefix('=').map_or(MASTER_DEFAULT_LUFS, |n| n.parse().expect("--master=<LUFS>")));
    // --match alone = built-in pop target; --match=ref.wav = that track
    let match_ref = match flags.iter().find_map(|f| f.strip_prefix("--match")) {
        Some(v) => Some(v.strip_prefix('=').map(|r| read_audio(Path::new(r)).map_err(|e| format!("reference: {e}"))).transpose()?),
        None => None,
    };
    let input = Path::new(&args[1]);
    let out_dir = args.get(2).map(Path::new).unwrap_or(input.parent().unwrap());
    let title = args.get(3).cloned().unwrap_or_else(|| input.file_stem().unwrap().to_string_lossy().into());
    std::fs::create_dir_all(out_dir).unwrap();

    let (x, sr) = read_audio(input)?;
    let len = x[0].len();
    // --strength=0.5 halves the cleaning; by default it follows the haze's flutter
    let strength = flags.iter().find_map(|f| f.strip_prefix("--strength=")).map(|v| v.parse().expect("--strength=<x>"));
    let p = Params { gate: !flags.iter().any(|f| f == "--no-gate"), strength, ..params_for(sr) };
    say!("input      {} | {} Hz, {} ch, {} samples ({:.1} s)", input.display(), sr, x.len(), len, len as f64 / sr as f64);
    say!("params     n_fft={} hop={} hann | block={}s p{} x{} | alpha {}->{} over {}-{} Hz | blur {}bin x {}fr",
        p.n, p.hop, p.block_s, (p.pctl * 100.0) as u32, p.noise_scale, p.alpha_lo, p.alpha_hi, ALPHA_RAMP.0, ALPHA_RAMP.1, p.sigma_f, p.sigma_t);
    say!("floors     {:?} (Hz, dB max atten)", FLOOR_KNOTS);

    if flags.iter().any(|f| f == "--flutter") {
        let show = |name: &str, r: Vec<(f64, usize)>| say!("flutter {name:<6} {}", r.iter().zip(BANDS).map(|((v, n), (lo, _))| format!("{:>5.0}+ {v:4.2} dB ({n})", lo)).collect::<Vec<_>>().join(" | "));
        show("track", flutter(&p, &x, sr));
        let mut seed = 42u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let white: Vec<Vec<f64>> = x.iter().map(|c| c.iter().map(|_| 0.1 * rnd()).collect()).collect();
        show("steady", flutter(&p, &white, sr));
        return Ok(());
    }
    if flags.iter().any(|f| f == "--audibility") {
        let base = audibility(&Params { gate: false, ..params_for(sr) }, &x, sr);
        let gated = audibility(&Params { gate: true, ..params_for(sr) }, &x, sr);
        let tot: f64 = base.iter().sum();
        for (name, v) in [("ungated", base), ("gated", gated)] {
            say!("audibility {name:<8} removed energy (% of ungated total): masked {:5.1} | within +-{GATE_RAMP_DB} dB {:5.1} | audible {:5.1}", v[0] / tot * 100.0, v[1] / tot * 100.0, v[2] / tot * 100.0);
        }
        return Ok(());
    }
    if flags.iter().any(|f| f == "--attacks") {
        let (rise, pre, n) = attacks(&p, &x, sr);
        say!("attacks    {n} hits | median rise 10-90% {rise:.2} ms | median pre-energy (5-25 ms before peak) {pre:+.1} dB");
        return Ok(());
    }
    if flags.iter().any(|f| f == "--phase") {
        say!("phase      {}", phase_dev(&p, &x, sr).iter().zip(BANDS).map(|((d, big, n), (lo, _))| format!("{lo:>5.0}+ {d:5.1}deg {:4.1}%>30 ({n})", big * 100.0)).collect::<Vec<_>>().join(" | "));
        return Ok(());
    }
    if flags.iter().any(|f| f == "--width") {
        assert!(x.len() == 2, "--width needs stereo");
        for ((lo, hi), (c, sm)) in WIDTH_BANDS.iter().zip(width(&p, &x, sr)) {
            say!("width      {lo:>5.0}-{hi:<5.0} Hz  coherence {c:.3}  side/mid {sm:+6.1} dB");
        }
        return Ok(());
    }
    if flags.iter().any(|f| f == "--repeats") {
        let (mut sims, coh) = repeats(&p, &x, sr);
        sims.sort_by(|a, b| a.total_cmp(b));
        let pct = |q: f64| sims[((sims.len() - 1) as f64 * q) as usize];
        say!("repeats    best-match similarity over {} segments: p10 {:.3} | p50 {:.3} | p90 {:.3} | max {:.3}", sims.len(), pct(0.1), pct(0.5), pct(0.9), pct(1.0));
        say!("coherence  top-200 aligned pairs: {}", coh.iter().zip(BANDS).map(|(c, (lo, _))| format!("{lo:>5.0}+ {c:.3}")).collect::<Vec<_>>().join(" | "));
        return Ok(());
    }
    if flags.iter().any(|f| f == "--modspec") {
        let (rate, bands) = modspec(&x, sr);
        let bph = 4000.0 / rate;
        // candidate frame rates: neural codecs, then MP3 frame/granule and AAC frame at this sr
        let s = sr as f64;
        let cands = [("50", 50.0), ("EnCodec 75", 75.0), ("DAC 86.1", 86.13), ("sr/512", s / 512.0), ("MP3 sr/1152", s / 1152.0), ("MP3 sr/576", s / 576.0), ("AAC sr/1024", s / 1024.0)];
        for (spec, (lo, _)) in bands.iter().zip(&BANDS[2..]) {
            let prom: Vec<f64> = (0..spec.len()).map(|k| prominence(spec, k, bph)).collect();
            let mut peaks: Vec<usize> = ((5.0 * bph) as usize..(400.0 * bph) as usize)
                .filter(|&k| prom[k] > 3.0 && spec[k] >= spec[k - 1] && spec[k] >= spec[k + 1]).collect();
            peaks.sort_by(|&a, &b| prom[b].total_cmp(&prom[a]));
            say!("modspec {lo:>5.0}+ top: {}", peaks.iter().take(6).map(|&k| format!("{:.2} Hz {:+.1}", k as f64 / bph, prom[k])).collect::<Vec<_>>().join(" | "));
            say!("        {lo:>5.0}+ at: {}", cands.iter().map(|(name, f)| format!("{name} {:+.1}", prominence(spec, (f * bph).round() as usize, bph))).collect::<Vec<_>>().join(" | "));
        }
        return Ok(());
    }
    if let Some(r) = flags.iter().find_map(|f| f.strip_prefix("--tone=")) {
        // uncapped --match curve against a reference, per third-octave-ish point
        let c = if r == "pop" { match_curve(&p, &x, sr, pop_target, f64::INFINITY) } else { let (r, rsr) = read_audio(Path::new(r))?; match_curve(&p, &x, sr, ltas_target(&r, rsr), f64::INFINITY) };
        let at = |h: f64| c[((h * p.n as f64 / sr as f64).round() as usize).min(c.len() - 1)];
        say!("tone       {}", TONE_HZ.iter().map(|&h| format!("{:+.1}", at(h))).collect::<Vec<_>>().join(" "));
        return Ok(());
    }
    if flags.iter().any(|f| f == "--profile") {
        // third-octave long-term spectrum, 0 dB = mean of the 125 Hz - 8 kHz points
        let lt = ltas(&Stft::new(p.n, p.hop, len), &x);
        let v = third_octaves(&lt, sr, p.n);
        say!("profile    {}", v.iter().map(|(h, d)| format!("{h:.0}:{d:.2}")).collect::<Vec<_>>().join(" "));
        return Ok(());
    }
    if flags.iter().any(|f| f == "--floors") {
        // Per 1 s block and band: level L (mean power) and floor F (15th percentile
        // over the block's frames x bins). Additive haze pins quiet blocks at F ~ L
        // with L bottoming out; haze that scales with the music keeps F/L constant.
        let st = Stft::new(p.n, p.hop, len);
        let (frames, bins) = (st.frames, st.bins());
        let power = power_grid(&st, &x);
        let per = (sr as usize / p.hop).max(1);
        let bin_of = |f: f64| ((f * p.n as f64 / sr as f64) as usize).min(bins);
        for &(lo, hi) in &BANDS[2..] {
            let (a, b) = (bin_of(lo), bin_of(hi.min(sr as f64 / 2.0)));
            let mut blocks: Vec<(f64, f64)> = (0..frames / per).map(|k| {
                let mut v: Vec<f32> = (k * per..(k + 1) * per).flat_map(|t| power[t * bins + a..t * bins + b].iter().copied()).collect();
                let l = v.iter().map(|&q| q as f64).sum::<f64>() / v.len() as f64;
                let i = (v.len() as f64 * 0.15) as usize;
                let f = *v.select_nth_unstable_by(i, f32::total_cmp).1 as f64;
                (10.0 * (l + 1e-30).log10(), 10.0 * ((f + 1e-30) / (l + 1e-30)).log10())
            }).filter(|(l, _)| *l > -200.0).collect();
            blocks.sort_by(|a, b| a.0.total_cmp(&b.0));
            let q = |i: usize| { let c = &blocks[i * blocks.len() / 5..(i + 1) * blocks.len() / 5]; (c.iter().map(|b| b.0).sum::<f64>() / c.len() as f64, c.iter().map(|b| b.1).sum::<f64>() / c.len() as f64) };
            say!("floors {lo:>5.0}+ quintile level / floor-to-level: {}", (0..5).map(|i| { let (l, r) = q(i); format!("{l:6.1} dB {r:+5.1}") }).collect::<Vec<_>>().join(" | "));
        }
        return Ok(());
    }
    if flags.iter().any(|f| f == "--periods") {
        // decoder grids from 10 Hz to 1 kHz frame rates: top fold scores, then the
        // exact whole-song fold SNR at each
        let hp = hp_mono(&x, sr);
        let mut s = grid_scan(&hp, sr as usize / 1000, sr as usize / 10);
        s.sort_by(|a, b| b.1.total_cmp(&a.1));
        say!("periods    top: {}", s.iter().take(8).map(|(p, v)| format!("{p} {v:+.1}")).collect::<Vec<_>>().join(" | "));
        let rms = (hp.iter().map(|v| v * v).sum::<f64>() / hp.len() as f64).sqrt();
        for (p, _) in s.iter().take(4) {
            say!("periods    {p}: {}", (1..=4).map(|m| { let (snr, e) = fold(&hp, p * m); format!("x{m} {:+.1} dB, {:.1} dB re hp", snr, 10.0 * (e / rms / rms + 1e-30).log10()) }).collect::<Vec<_>>().join(" | "));
        }
        return Ok(());
    }
    if flags.iter().any(|f| f == "--holes") {
        for n in [2048, 1152] {
            let r = hole_rates(n * sr as usize / 48000, &x, sr);
            say!("holes n={n:<5} {}", r.iter().zip(BANDS).map(|(v, (lo, _))| format!("{:>5.0}+ {:5.2}%", lo, v * 100.0)).collect::<Vec<_>>().join(" | "));
        }
        return Ok(());
    }
    let st = Stft::new(p.n, p.hop, len);
    let rt = st.resynth(&x[0], |_, _| {});
    let rt_err = rt.iter().zip(&x[0]).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    say!("stft rt    max |err| = {rt_err:.3e}");

    // remove the decoder buzz first, so it lands in Removed Noise and never
    // reaches the floor estimate as a set of steady "noise" lines
    let xin = match grid_period(sr).filter(|_| !flags.iter().any(|f| f == "--no-debuzz")) {
        None => { say!("debuzz     off ({})", if grid_period(sr).is_none() { format!("no whole-sample {DEBUZZ_HZ} Hz grid at {sr} Hz") } else { "--no-debuzz".into() }); None }
        Some(per) => {
            let snr = grid_snr(&x, sr, per);
            if snr < DEBUZZ_MIN_DB {
                say!("debuzz     no decoder grid ({snr:+.1} dB vs chance at {per} samples)");
                None
            } else {
                let y = debuzz(&x, sr, per);
                let buzz: Vec<Vec<f64>> = x.iter().zip(&y).map(|(a, b)| a.iter().zip(b).map(|(u, v)| u - v).collect()).collect();
                say!("debuzz     Suno decoder grid: {per}-sample repeat {snr:+.1} dB above chance | removed {:.1} dBFS rms", 20.0 * rms(&buzz).log10());
                Some(y)
            }
        }
    };
    let (mut clean, gain, st) = denoise(&p, xin.as_ref().unwrap_or(&x), sr);
    let edge_db = |i: usize| 20.0 * x.iter().map(|c| c[i].abs()).fold(0.0, f64::max).max(1e-12).log10();
    let (first_db, last_db) = (edge_db(0), edge_db(len - 1));
    let end = abrupt_end(&x, sr);
    say!("edges      first sample {first_db:.1} dBFS, last {last_db:.1} dBFS{}{}", if first_db.max(last_db) > -60.0 { format!(" -> {EDGE_FADE_MS} ms fades remove the click") } else { String::new() },
        end.map_or(String::new(), |(_, db)| format!(" | stops mid-sound ({db:+.1} dB re song in its last 250 ms) -> {END_FADE_S} s fade-out")));
    let end = end.map(|(e, _)| e);
    // Removed gets the same fades as Clean, so it holds only what the cleaning took
    // and Clean + Removed is exactly the faded original
    let mut removed: Vec<Vec<f64>> = x.iter().zip(&clean).map(|(o, c)| o.iter().zip(c).map(|(a, b)| a - b).collect()).collect();
    edge_fade(&mut removed, sr, end);
    // the stage chain starts from the unfaded Clean
    let mut stage = clean.clone();
    edge_fade(&mut clean, sr, end);

    let clean_path = out_dir.join(format!("{title} - Clean.wav"));
    let noise_path = out_dir.join(format!("{title} - Removed Noise.wav"));
    let oc = write_wav24(&clean_path, &clean, sr);
    let on = write_wav24(&noise_path, &removed, sr);
    say!("wrote      {}\n           {}", clean_path.display(), noise_path.display());
    say!("peak       clean {:.2} dBFS | removed {:.2} dBFS | original {:.2} dBFS", peak_db(&clean), peak_db(&removed), peak_db(&x));
    if oc + on > 0 { say!("CLIPPING   {oc} clean / {on} removed samples exceeded full scale (clamped, not normalised)"); }
    say!("rms change {:+.2} dB", 20.0 * (rms(&clean) / rms(&x)).log10());

    // energy-weighted attenuation per band: 10log10(sum g²P / sum P)
    let bins = st.bins();
    let mut spec = vec![Complex::default(); bins];
    let mut acc = [(0.0f64, 0.0f64); BANDS.len()];
    for c in &x {
        for t in 0..st.frames {
            st.frame(c, t, &mut spec);
            for f in 0..bins {
                let hz = f as f64 * sr as f64 / p.n as f64;
                let b = BANDS.iter().position(|(lo, hi)| hz >= *lo && hz < *hi).unwrap();
                let pw = spec[f].norm_sqr();
                acc[b].0 += pw * (gain[t * bins + f] as f64).powi(2);
                acc[b].1 += pw;
            }
        }
    }
    for ((lo, hi), (num, den)) in BANDS.iter().zip(acc) {
        say!("band       {:>5.0}-{:<6} Hz  {:+.2} dB", lo, if *hi == f64::MAX { "nyq".into() } else { format!("{hi:.0}") }, 10.0 * (num / den).log10());
    }

    let (eo, er) = (envelope(&x, sr as usize), envelope(&removed, sr as usize));
    say!("env corr   {:.3} (1 s RMS, original vs removed)", pearson(&eo, &er));

    // densest 10 s by 1 s RMS
    let w = 10.min(eo.len());
    let start = (0..=eo.len() - w).max_by(|a, b| eo[*a..*a + w].iter().sum::<f64>().total_cmp(&eo[*b..*b + w].iter().sum())).unwrap();
    let (s0, s1) = (start * sr as usize, ((start + w) * sr as usize).min(len));
    let mono = |ch: &[Vec<f64>]| -> Vec<f64> { (s0..s1).map(|i| ch.iter().map(|c| c[i]).sum::<f64>() / ch.len() as f64).collect() };
    let png_path = out_dir.join(format!("{title} - Spectrogram.png"));
    // optional stages chain, each writing a cumulative file for A/B: Clean+smooth+widen+excite
    drop(clean);
    let mut name = String::from("Clean");
    let mut emit = |tag: &str, mut sig: Vec<Vec<f64>>, stage: &mut Vec<Vec<f64>>| {
        name.push('+');
        name.push_str(tag);
        let path = out_dir.join(format!("{title} - {name}.wav"));
        let (over, peak) = with_fades(&mut sig, sr, end, |s| (write_wav24(&path, s, sr), peak_db(s)));
        say!("wrote      {} | peak {peak:.2} dBFS{}", path.display(), if over > 0 { format!(" | CLIPPING {over}") } else { String::new() });
        *stage = sig;
    };
    if let Some(ms) = smooth_ms {
        let sm = smooth_haze(&p, &stage, sr, ms);
        say!("smooth     {ms} ms window | rms change {:+.2} dB", 20.0 * (rms(&sm) / rms(&stage)).log10());
        emit("smooth", sm, &mut stage);
    }
    if let Some(amount) = widen_amt {
        match widen(&p, &stage, sr, amount) {
            None => say!("widen      needs stereo, skipped"),
            Some((wd, info)) => {
                say!("widen      target side/mid {:+.1} dB (3.5-13 kHz x{amount}) | 16 kHz+: {:+.1} -> {:+.1} dB", info.ref_side_mid_db, info.top_before_db, info.top_after_db);
                emit("widen", wd, &mut stage);
            }
        }
    }
    let excited = excite_db.and_then(|db| match excite(&p, &stage, sr, db) {
        None => { say!("excite     no codec lowpass below {EXCITE_MIN_CUTOFF_HZ} Hz, skipped"); None }
        Some((ex, info)) => {
            say!("excite     cutoff {:.0} Hz | slope {:+.1} dB/oct | shift {:.0} Hz | level {db:+.1} dB", info.cutoff_hz, info.slope_db_oct, info.shift_hz);
            let st = Stft::new(p.n, p.hop, len);
            let (lc, le) = (ltas(&st, &stage), ltas(&st, &ex));
            let band = |l: &[f64], a: f64, b: f64| { let r = (a / sr as f64 * p.n as f64) as usize..(b / sr as f64 * p.n as f64) as usize; 10.0 * (r.clone().map(|f| 10f64.powf(l[f] / 10.0)).sum::<f64>() / r.len() as f64).log10() };
            for a in (12000..22000).step_by(2000).filter(|a| (*a as f64) < sr as f64 / 2.0) {
                say!("ltas       {:>5}-{:<5} Hz  before {:6.1} dB -> excited {:6.1} dB", a, a + 2000, band(&lc, a as f64, (a + 2000) as f64), band(&le, a as f64, (a + 2000) as f64));
            }
            emit("excite", ex.clone(), &mut stage);
            Some(ex)
        }
    });
    if let Some(m) = &match_ref {
        let curve = match m {
            Some((r, rsr)) => match_curve(&p, &stage, sr, ltas_target(r, *rsr), MATCH_MAX_DB),
            None => match_curve(&p, &stage, sr, pop_target, MATCH_MAX_DB),
        };
        let hz_bin = |h: f64| ((h * p.n as f64 / sr as f64).round() as usize).min(curve.len() - 1);
        let target = flags.iter().find_map(|f| f.strip_prefix("--match=")).map_or("pop target".into(), |r| Path::new(r).file_name().map_or(r.into(), |n| n.to_string_lossy().into_owned()));
        say!("match      toward {target} | {}", [63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0].iter().map(|&h| format!("{:.0} Hz {:+.1}", h, curve[hz_bin(h)])).collect::<Vec<_>>().join(" | "));
        let st = Stft::new(p.n, p.hop, len);
        let g: Vec<f64> = curve.iter().map(|d| 10f64.powf(d / 20.0)).collect();
        let m = stage.iter().map(|c| st.resynth(c, |_, s| s.iter_mut().zip(&g).for_each(|(v, g)| *v *= g))).collect();
        emit("match", m, &mut stage);
    }
    if let Some(target) = master_lufs {
        let (m, info) = master(&stage, sr, target);
        say!("master     {:.1} LUFS in | gain {:+.1} dB | limiter max {:.1} dB | out {:.1} LUFS, true peak {:.1} dBTP", info.in_lufs, info.gain_db, info.max_gr_db.abs(), info.out_lufs, info.out_tp_db);
        if info.max_gr_db > 6.0 { say!("master     heavy limiting: try a quieter target, e.g. --master={:.0}", target - (info.max_gr_db - 6.0).ceil()); }
        emit("master", m, &mut stage);
    }
    let mut panels = vec![mono(&x), mono(&removed)];
    if let Some(ex) = &excited { panels.push(mono(ex)); }
    spectrogram_png(&png_path, &panels.iter().map(|v| v.as_slice()).collect::<Vec<_>>(), p.n, p.hop);
    say!("spectro    {} ({}-{} s; original / removed{}, each normalised)", png_path.display(), start, start + w, if excited.is_some() { " / excited" } else { "" });
    let report_path = out_dir.join(format!("{title} - Report.txt"));
    std::fs::write(&report_path, REPORT.with_borrow(String::clone)).unwrap();
    say!("report     {}", report_path.display());
    Ok(())
}

/// `run` for an app: report lines go to `on_line` instead of stdout, a set
/// `cancel` stops the run at its next report line, and failures (bad input,
/// panics) come back as `Err(message)`. Runs on the calling thread; separate
/// threads can run songs side by side.
pub fn run_with(args: Vec<String>, on_line: impl FnMut(&str) + 'static, cancel: Arc<AtomicBool>) -> Result<(), String> {
    SINK.set(Some(Box::new(on_line)));
    CANCEL.set(Some(cancel));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(args)));
    SINK.set(None);
    CANCEL.set(None);
    match result {
        Ok(r) => r,
        Err(e) if e.is::<Cancelled>() => Err("cancelled".into()),
        Err(e) => Err(e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "sunscreen stopped unexpectedly".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stft_round_trip_is_exact() {
        let x: Vec<f64> = (0..10007).map(|i| ((i * 7919) % 1000) as f64 / 1000.0 - 0.5).collect();
        let st = Stft::new(2048, 512, x.len());
        let y = st.resynth(&x, |_, _| {});
        assert!(x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-12));
    }

    #[test]
    fn removes_hiss_keeps_tone() {
        let sr = 48000;
        let n = sr * 12;
        let mut seed = 1u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        // gated note (on 40% of each second): a steady 12 s sine *is* the floor by design
        let tone: Vec<f64> = (0..n).map(|i| if i % sr < sr * 2 / 5 { 0.3 * (2.0 * PI * 1000.0 * i as f64 / sr as f64).sin() } else { 0.0 }).collect();
        let x: Vec<Vec<f64>> = (0..2).map(|_| tone.iter().map(|t| t + 0.01 * rnd()).collect()).collect();
        let p = params_for(sr as u32);
        let (clean, _, _) = denoise(&p, &x, sr as u32);
        let resid = |c: &[f64]| (c.iter().zip(&tone).map(|(a, b)| (a - b).powi(2)).sum::<f64>() / n as f64).sqrt();
        // hiss residual drops; tone survives within 0.5 dB
        assert!(resid(&clean[0]) < 0.8 * resid(&x[0]), "{} vs {}", resid(&clean[0]), resid(&x[0]));
        let g = (rms(&clean) / rms(&x)).log10() * 20.0;
        assert!(g > -0.5, "tone lost {g} dB");
    }

    #[test]
    fn onset_protection_keeps_attacks() {
        // decaying 2 kHz hits every 0.5 s over hiss
        let sr = 48000;
        let n = sr * 10;
        let mut seed = 7u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let hit = |i: usize| { let k = i % (sr / 2); 0.5 * (-(k as f64) / 400.0).exp() * (2.0 * PI * 2000.0 * k as f64 / sr as f64).sin() };
        let x: Vec<Vec<f64>> = (0..2).map(|_| (0..n).map(|i| hit(i) + 0.02 * rnd()).collect()).collect();
        // removed energy in the first 10 ms of each hit
        let attack_loss = |protect: bool| {
            let p = Params { protect_onsets: protect, ..params_for(sr as u32) };
            let (clean, _, _) = denoise(&p, &x, sr as u32);
            (0..n).filter(|i| i % (sr / 2) < 480).map(|i| (x[0][i] - clean[0][i]).powi(2)).sum::<f64>()
        };
        let (with, without) = (attack_loss(true), attack_loss(false));
        assert!(with < 0.5 * without, "protected {with} vs unprotected {without}");
    }

    #[test]
    fn excite_fills_above_codec_lowpass_only() {
        let sr = 48000u32;
        let n = sr as usize * 6;
        let mut seed = 3u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let noise: Vec<f64> = (0..n).map(|_| 0.1 * rnd()).collect();
        let p = params_for(sr);
        let st = Stft::new(p.n, p.hop, n);
        let cut = (16000.0 / sr as f64 * p.n as f64) as usize;
        // brick-wall at 16 kHz like an MP3 encoder
        let x = vec![st.resynth(&noise, |_, s| for v in s[cut..].iter_mut() { *v = Complex::default(); })];
        assert!(excite(&p, &[noise.clone()], sr, -3.0).is_none(), "full-band input must be skipped");
        // a 20 kHz lowpass (Suno v6) leaves nothing audible to rebuild
        let cut20 = (20000.0 / sr as f64 * p.n as f64) as usize;
        let x20 = vec![st.resynth(&noise, |_, s| for v in s[cut20..].iter_mut() { *v = Complex::default(); })];
        assert!(excite(&p, &x20, sr, -3.0).is_none(), "20 kHz lowpass must be skipped");
        let (ex, info) = excite(&p, &x, sr, -3.0).expect("cutoff not found");
        assert!((info.cutoff_hz - 16000.0).abs() < 200.0, "cutoff {}", info.cutoff_hz);
        let (lx, le) = (ltas(&st, &x), ltas(&st, &ex[..1]));
        let below = (0..cut * 9 / 10).map(|f| (lx[f] - le[f]).abs()).fold(0.0, f64::max);
        assert!(below < 0.01, "touched content below 0.9c by {below} dB");
        let hi = (18000.0 / sr as f64 * p.n as f64) as usize;
        assert!(le[hi] > lx[hi] + 40.0, "nothing added at 18 kHz: {} -> {}", lx[hi], le[hi]);
    }

    #[test]
    fn smooth_haze_flattens_flutter_keeps_hits() {
        // 6 Hz fluttering hiss plus loud 1 kHz notes (on 40% of each second)
        let sr = 48000u32;
        let n = sr as usize * 10;
        let mut seed = 11u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let on = |i: usize| i % (sr as usize) < sr as usize * 2 / 5;
        let x: Vec<Vec<f64>> = (0..2).map(|_| (0..n).map(|i| {
            let tt = i as f64 / sr as f64;
            let hiss = 0.02 * (1.0 + 0.5 * (2.0 * PI * 6.0 * tt).sin()) * rnd();
            hiss + if on(i) { 0.3 * (2.0 * PI * 1000.0 * tt).sin() } else { 0.0 }
        }).collect()).collect();
        let p = params_for(sr);
        let y = smooth_haze(&p, &x, sr, SMOOTH_DEFAULT_MS);
        let band = |v: &[(f64, usize)]| v[2].0 + v[3].0; // 3.5-12 kHz, hiss only
        let (before, after) = (band(&flutter(&p, &x, sr)), band(&flutter(&p, &y, sr)));
        assert!(after < before - 1.0, "flutter {before} -> {after}");
        let note = |c: &[f64]| (0..n).filter(|i| on(*i)).map(|i| c[i] * c[i]).sum::<f64>();
        let d = 10.0 * (note(&y[0]) / note(&x[0])).log10();
        assert!(d.abs() < 0.2, "notes changed by {d} dB");
    }

    #[test]
    fn widen_restores_high_width_mono_safe() {
        // independent L/R noise (wide) whose content above 12 kHz is forced to mono
        let sr = 48000u32;
        let n = sr as usize * 6;
        let mut seed = 5u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let raw: Vec<Vec<f64>> = (0..2).map(|_| (0..n).map(|_| 0.1 * rnd()).collect()).collect();
        let p = params_for(sr);
        let st = Stft::new(p.n, p.hop, n);
        let cut = (12000.0 / sr as f64 * p.n as f64) as usize;
        let mut o = vec![Complex::default(); st.bins()];
        let x: Vec<Vec<f64>> = (0..2).map(|c| st.resynth(&raw[c], |t, s| {
            st.frame(&raw[1 - c], t, &mut o);
            for f in cut..s.len() { s[f] = (s[f] + o[f]) * 0.5; }
        })).collect();
        let before = width(&p, &x, sr);
        let (y, _) = widen(&p, &x, sr, 1.0).unwrap();
        let after = width(&p, &y, sr);
        let mono_err = (0..n).map(|i| ((y[0][i] + y[1][i]) - (x[0][i] + x[1][i])).abs()).fold(0.0, f64::max);
        assert!(mono_err < 1e-9, "mono fold-down changed by {mono_err}");
        assert!(before[7].0 > 0.99 && after[7].0 < 0.9, "16-20 kHz coherence {} -> {}", before[7].0, after[7].0);
        assert!(after[7].1 > before[7].1 + 10.0, "16-20 kHz side/mid {} -> {}", before[7].1, after[7].1);
        let low_err = (0..4).map(|b| (after[b].1 - before[b].1).abs()).fold(0.0, f64::max);
        assert!(low_err < 0.01, "width below 7 kHz moved {low_err} dB");
    }

    #[test]
    fn gate_spares_masked_haze_keeps_exposed() {
        // loud harmonic notes (220 Hz x 1..12, on 40% of each second) over steady hiss:
        // under the notes the hiss near the harmonics is masked, above 8 kHz it's exposed
        let sr = 48000u32;
        let n = sr as usize * 10;
        let mut seed = 9u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let on = |i: usize| i % (sr as usize) < sr as usize * 2 / 5;
        let x: Vec<Vec<f64>> = (0..2).map(|_| (0..n).map(|i| {
            let tt = i as f64 / sr as f64;
            let notes: f64 = if on(i) { (1..=12).map(|h| 0.08 / h as f64 * (2.0 * PI * 220.0 * h as f64 * tt).sin()).sum() } else { 0.0 };
            notes + 0.01 * rnd()
        }).collect()).collect();
        let p = params_for(sr);
        let removed = |gate: bool, lo: f64, hi: f64| {
            let (_, g, st) = denoise(&Params { gate, ..params_for(sr) }, &x, sr);
            let pw = power_grid(&st, &x);
            let bins = st.bins();
            let hz = |f: usize| f as f64 * sr as f64 / p.n as f64;
            (0..st.frames).filter(|t| on((t * p.hop).saturating_sub(st.pad).min(n - 1))).flat_map(|t| (0..bins).map(move |f| (t, f)))
                .filter(|(_, f)| hz(*f) >= lo && hz(*f) < hi).map(|(t, f)| pw[t * bins + f] as f64 * (1.0 - (g[t * bins + f] as f64).powi(2))).sum::<f64>()
        };
        let (masked_off, masked_on) = (removed(false, 200.0, 2800.0), removed(true, 200.0, 2800.0));
        let (exposed_off, exposed_on) = (removed(false, 8000.0, 16000.0), removed(true, 8000.0, 16000.0));
        assert!(masked_on < 0.5 * masked_off, "masked removal {masked_off} -> {masked_on}");
        assert!(exposed_on > 0.9 * exposed_off, "exposed removal {exposed_off} -> {exposed_on}");
    }

    #[test]
    fn modspec_finds_frame_rate_flutter() {
        let sr = 48000u32;
        let n = 20 * sr as usize;
        let mut seed = 5u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let noise: Vec<f64> = (0..n).map(|_| rnd()).collect();
        // 10% amplitude flutter at 75 Hz, like a codec frame grid would leave
        let am: Vec<f64> = noise.iter().enumerate().map(|(i, v)| v * (1.0 + 0.1 * (2.0 * PI * 75.0 * i as f64 / sr as f64).sin())).collect();
        let at75 = |x: &[f64]| { let (rate, b) = modspec(&[x.to_vec()], sr); b.iter().map(|s| prominence(s, (75.0 * 4000.0 / rate).round() as usize, 4000.0 / rate)).collect::<Vec<_>>() };
        for (flat, mod_) in at75(&noise).into_iter().zip(at75(&am)) {
            assert!(flat < 4.0, "steady noise shows {flat} dB at 75 Hz");
            assert!(mod_ > 10.0, "75 Hz flutter only {mod_} dB");
        }
    }

    #[test]
    fn match_follows_reference_tilt_capped() {
        let sr = 48000u32;
        let n = 10 * sr as usize;
        let mut seed = 9u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let x: Vec<Vec<f64>> = vec![(0..n).map(|_| 0.1 * rnd()).collect()];
        let p = params_for(sr);
        let st = Stft::new(p.n, p.hop, n);
        let hz = |f: usize| f as f64 * sr as f64 / p.n as f64;
        // reference = same noise with a smooth tilt of `db` around 1 kHz
        let tilted = |db: f64| vec![st.resynth(&x[0], |_, s| for (f, v) in s.iter_mut().enumerate() { *v *= 10f64.powf(db * (hz(f).max(1.0) / 1000.0).log2().tanh() / 20.0); })];
        let at = |c: &[f64], h: f64| c[(h * p.n as f64 / sr as f64).round() as usize];
        let c = match_curve(&p, &x, sr, ltas_target(&tilted(1.5), sr), MATCH_MAX_DB);
        let want = 1.5 * (2f64.tanh() - (-2f64).tanh());
        let got = at(&c, 4000.0) - at(&c, 250.0);
        assert!((got - want).abs() < 0.6, "tilt 250 Hz -> 4 kHz: got {got:.2}, want {want:.2}");
        let c = match_curve(&p, &x, sr, ltas_target(&tilted(12.0), sr), MATCH_MAX_DB);
        assert!(c.iter().all(|v| v.abs() <= MATCH_MAX_DB + 1e-9), "cap exceeded: {:?}", c.iter().enumerate().filter(|(_, v)| !(v.abs() <= MATCH_MAX_DB + 1e-9)).take(5).collect::<Vec<_>>());
        assert!(at(&c, 23000.0).abs() < 1e-9, "should fade out half an octave above MATCH_RANGE");
    }

    #[test]
    fn lufs_reads_reference_sine() {
        // stereo 1 kHz sine of amplitude A reads 20*log10(A) LUFS (K-weighting is +0.69 dB at 1 kHz, offset -0.691)
        for sr in [48000u32, 44100] {
            let x: Vec<Vec<f64>> = (0..2).map(|_| (0..10 * sr as usize).map(|i| 0.1 * (2.0 * PI * 1000.0 * i as f64 / sr as f64).sin()).collect()).collect();
            let l = lufs(&x, sr);
            assert!((l + 20.0).abs() < 0.1, "{sr} Hz: {l} LUFS");
        }
    }

    #[test]
    fn true_peak_sees_between_samples() {
        // fs/4 sine at 45 degrees: every sample is 0.707 of the true peak
        let x = vec![(0..4800).map(|i| (PI / 2.0 * i as f64 + PI / 4.0).sin()).collect::<Vec<f64>>()];
        let sample_peak = x[0].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let tp = true_peak_env(&x)[100..4700].iter().copied().fold(0.0, f64::max);
        assert!((20.0 * sample_peak.log10() + 3.01).abs() < 0.05);
        assert!((20.0 * tp.log10()).abs() < 0.2, "true peak {} dB", 20.0 * tp.log10());
    }

    #[test]
    fn master_hits_loudness_and_ceiling() {
        let sr = 48000u32;
        let mut seed = 11u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        // quiet noise with a sharp hit every 0.5 s: needs gain, and the hits then need limiting
        let x: Vec<Vec<f64>> = (0..2).map(|_| (0..10 * sr as usize).map(|i| 0.02 * rnd() + if i % (sr as usize / 2) < 48 { 0.3 } else { 0.0 }).collect()).collect();
        let (out, info) = master(&x, sr, -14.0);
        let tp = 20.0 * true_peak_env(&out).into_iter().fold(0.0, f64::max).log10();
        assert!((lufs(&out, sr) + 14.0).abs() < 0.1, "loudness {}", info.out_lufs);
        assert!(tp <= TRUE_PEAK_DBTP + 0.01, "true peak {tp} dBTP");
        assert!(info.max_gr_db > 1.0, "the hits should have needed limiting");
    }

    #[test]
    fn debuzz_removes_decoder_grid() {
        assert_eq!((grid_period(48000), grid_period(44100), grid_period(44099)), (Some(240), Some(441), None));
        for sr in [48000u32, 44100] { debuzz_removes_grid_at(sr); }
    }

    fn debuzz_removes_grid_at(sr: u32) {
        let per = grid_period(sr).unwrap();
        let n = 20 * sr as usize;
        let mut seed = 13u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let noise: Vec<Vec<f64>> = (0..2).map(|_| (0..n).map(|_| 0.2 * rnd()).collect()).collect();
        // a fixed one-period waveform ~26 dB under the noise, as Suno's decoder leaves
        let pattern: Vec<f64> = (0..per).map(|_| 0.02 * rnd()).collect();
        let x: Vec<Vec<f64>> = noise.iter().map(|c| c.iter().enumerate().map(|(i, v)| v + pattern[i % per]).collect()).collect();
        assert!(grid_snr(&noise, sr, per) < DEBUZZ_MIN_DB, "plain noise flagged");
        assert!(grid_snr(&x, sr, per) > 15.0, "grid missed: {}", grid_snr(&x, sr, per));
        let y = debuzz(&x, sr, per);
        assert!(grid_snr(&y, sr, per) < 3.0, "grid left: {}", grid_snr(&y, sr, per));
        // what came out is the pattern, not the noise: the residual against the clean
        // noise is only what falls in the notches, ~28 dB under it (the pattern is -26 dB)
        let err: Vec<Vec<f64>> = y.iter().zip(&noise).map(|(a, b)| a.iter().zip(b).map(|(u, v)| u - v).collect()).collect();
        assert!(rms(&err) < 0.06 * rms(&noise), "residual {} vs noise {}", rms(&err), rms(&noise));
    }

    #[test]
    fn debuzz_does_not_ring_after_on_grid_note() {
        // a held 2200 Hz note (11 x 200 Hz, e.g. the 5th harmonic of A 440) for 1 s in
        // every 4, phase-locked across the song so it lands in the grid's shape
        let sr = 48000u32;
        let per = grid_period(sr).unwrap();
        let n = 20 * sr as usize;
        let mut seed = 7u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let pattern: Vec<f64> = (0..per).map(|_| 0.01 * rnd()).collect();
        let x: Vec<Vec<f64>> = (0..2).map(|_| (0..n).map(|i| {
            let on = (i / sr as usize) % 4 == 0;
            0.02 * rnd() + pattern[i % per] + if on { 0.5 * (2.0 * PI * 2200.0 * i as f64 / sr as f64).sin() } else { 0.0 }
        }).collect()).collect();
        let y = debuzz(&x, sr, per);
        // the seconds after each note: subtraction may only take energy away
        let gaps = |c: &[f64]| -> f64 { (0..5).flat_map(|k| (k * 4 + 1) * sr as usize + sr as usize / 4..(k * 4 + 3) * sr as usize).map(|i| c[i] * c[i]).sum() };
        for (a, b) in x.iter().zip(&y) {
            assert!(gaps(b) <= gaps(a), "rings after the note: gap energy {:.1} dB vs input", 10.0 * (gaps(b) / gaps(a)).log10());
        }
    }

    #[test]
    fn run_with_reports_errors_and_cancels() {
        let dir = std::env::temp_dir().join(format!("sunscreen-run-with-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let x: Vec<Vec<f64>> = vec![(0..48000).map(|i| 0.1 * (i as f64 * 0.05).sin()).collect(); 2];
        write_wav24(&wav, &x, 48000);
        let args = |p: &Path| vec!["sunscreen".to_string(), p.display().to_string(), dir.display().to_string()];
        let lines = std::rc::Rc::new(RefCell::new(Vec::<String>::new()));
        let seen = lines.clone();
        let missing = run_with(args(&dir.join("nope.wav")), move |l| seen.borrow_mut().push(l.into()), Arc::new(AtomicBool::new(false)));
        assert!(missing.as_ref().is_err_and(|e| e.contains("nope.wav")), "{missing:?}");
        assert_eq!(run_with(args(&wav), |_| {}, Arc::new(AtomicBool::new(true))), Err("cancelled".into()));
        // a bad --match reference fails the run up front, worded as the reference's fault
        let junk = dir.join("junk.m4a");
        std::fs::write(&junk, vec![7u8; 4096]).unwrap();
        let bad_ref = run_with([args(&wav), vec![format!("--match={}", junk.display())]].concat(), |_| {}, Arc::new(AtomicBool::new(false)));
        assert_eq!(bad_ref, Err("reference: junk.m4a isn't MP3, WAV, FLAC or M4A audio".into()));
        let short = dir.join("short.wav");
        write_wav24(&short, &[vec![0.1; 1000]], 48000);
        assert_eq!(run_with(args(&short), |_| {}, Arc::new(AtomicBool::new(false))), Err("short.wav is shorter than a second".into()));
        assert!(lines.borrow().is_empty(), "lines before the failure: {:?}", lines.borrow());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edge_fade_silences_ends_only() {
        let sr = 48000u32;
        let x0: Vec<Vec<f64>> = vec![vec![0.5; sr as usize]; 2];
        let mut x = x0.clone();
        edge_fade(&mut x, sr, None);
        let n = (EDGE_FADE_MS * 1e-3 * sr as f64) as usize;
        for c in &x {
            assert_eq!(c[0], 0.0, "first sample not silent");
            assert!(c[c.len() - 1].abs() < 1e-3, "last sample {}", c[c.len() - 1]);
            assert!(c[n..c.len() - n].iter().all(|v| *v == 0.5), "middle touched");
            assert!(c[1..n].windows(2).all(|w| w[1] >= w[0]), "fade-in not monotonic");
        }
    }

    #[test]
    fn abrupt_end_gets_fade_out() {
        let sr = 48000u32;
        let mut seed = 5u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let s = sr as usize;
        // 12 s at full level, cut dead, then 1 s of digital silence
        let cut: Vec<Vec<f64>> = (0..2).map(|_| (0..13 * s).map(|i| if i < 12 * s { 0.3 * rnd() } else { 0.0 }).collect()).collect();
        // the same, but ringing out over its last 2 s to -80 dB, as a mastered song ends
        let rings: Vec<Vec<f64>> = cut.iter().map(|c| c.iter().enumerate().map(|(i, v)| if i < 10 * s { *v } else { v * 10f64.powf(-4.0 * (i - 10 * s) as f64 / (2 * s) as f64) }).collect()).collect();
        assert_eq!(abrupt_end(&rings, sr), None, "a song that rings out was flagged");
        let (end, db) = abrupt_end(&cut, sr).expect("a dead stop was missed");
        assert_eq!(end, 12 * s, "end found at {end}");
        assert!(db > -3.0, "tail level {db}");
        let mut x = cut.clone();
        let m = (END_FADE_S * sr as f64) as usize;
        let seen = with_fades(&mut x, sr, Some(end), |f| f.to_vec());
        assert_eq!(x, cut, "with_fades must hand the signal back unfaded");
        for (c, o) in seen.iter().zip(&cut) {
            assert!(c[end - 1].abs() < 1e-4 && c[end..].iter().all(|v| *v == 0.0), "not silent at the end");
            assert!(c[end - m / 2] .abs() < o[end - m / 2].abs() * 0.51 + 1e-12, "fade-out too short");
            assert_eq!(&c[s..end - m], &o[s..end - m], "music before the fade touched");
        }
    }

    #[test]
    fn strength_follows_haze_flutter() {
        // steady hiss (v6-like wash) vs hiss whose level jumps every 100 ms (v5-like haze;
        // the flutter reading averages ~85 ms, so faster jumps would cancel out)
        let sr = 48000u32;
        let n = 12 * sr as usize;
        let mut seed = 21u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5 };
        let steady: Vec<Vec<f64>> = (0..2).map(|_| (0..n).map(|_| 0.05 * rnd()).collect()).collect();
        let jumps: Vec<f64> = (0..n / 4800 + 1).map(|_| 10f64.powf(rnd() * 40.0 / 20.0)).collect();
        let flutters: Vec<Vec<f64>> = steady.iter().map(|c| c.iter().enumerate().map(|(i, v)| v * jumps[i / 4800]).collect()).collect();
        let p = params_for(sr);
        let (fs, ff) = (flutter(&p, &steady, sr)[3].0, flutter(&p, &flutters, sr)[3].0);
        assert_eq!(haze_strength(fs), STRENGTH_MIN, "steady wash flutter {fs}");
        assert_eq!(haze_strength(ff), 1.0, "fluttering haze flutter {ff}");
        // and denoise applies it: auto strength removes less from the steady wash than full
        let removed = |st: Option<f64>| { let (c, _, _) = denoise(&Params { strength: st, ..params_for(sr) }, &steady, sr); 20.0 * (rms(&c) / rms(&steady)).log10() };
        let (auto, full) = (removed(None), removed(Some(1.0)));
        assert!(auto > full + 0.5 * full.abs(), "auto {auto:.2} dB vs full {full:.2} dB");
    }
}
