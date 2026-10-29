//! Hear one computer's sound on the other. The owner captures what it plays (system output loopback:
//! WASAPI loopback on Windows, a CoreAudio process tap on macOS 14.6+, both via cpal) and streams raw
//! PCM; the listener plays it on its default output through a small jitter buffer.
//!
//! Wire format on the authenticated stream (owner → listener, little-endian; the listener sends
//! nothing, so any byte or EOF from it means it hung up):
//! ```text
//! header  b"LKSN" | rate u32 (8000..=384000) | channels u8 (1..=8)
//! chunk   frames u16 (1..=MAX_FRAMES) | frames * channels i16, interleaved
//! ```

use crate::network::secure::ServerStream;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, SampleFormat, SizedSample, StreamConfig,
};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
    time::Duration,
};
use tauri::{async_runtime::JoinHandle, AppHandle, Emitter};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
    time::timeout,
};

const MAGIC: &[u8; 4] = b"LKSN";
const MAX_FRAMES: usize = 4096;
/// Captured chunks waiting for the network (~10 ms each); when full, new audio is dropped.
const QUEUE: usize = 8;
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const JITTER_MS: usize = 80;
const MAX_BEHIND_MS: usize = 300;

/// Owner side: stream this computer's sound until the listener hangs up.
pub async fn serve(s: ServerStream) -> io::Result<()> {
    let (tx, mut chunks) = mpsc::channel(QUEUE);
    let (etx, mut errors) = mpsc::unbounded_channel();
    let (stream, rate, channels) = off_runtime(move || capture(tx, etx)).await.map_err(io::Error::other)?;
    let (mut rd, mut wr) = tokio::io::split(s);
    let mut header = MAGIC.to_vec();
    header.extend_from_slice(&rate.to_le_bytes());
    header.push(channels as u8);
    timeout(WRITE_TIMEOUT, wr.write_all(&header)).await??;
    let mut out = Vec::new();
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            chunk = chunks.recv() => {
                let Some(chunk) = chunk else { break };
                out.clear();
                encode(&chunk, channels, &mut out);
                timeout(WRITE_TIMEOUT, wr.write_all(&out)).await??;
            }
            e = errors.recv() => return Err(io::Error::other(e.unwrap_or_default())),
            _ = rd.read(&mut probe) => break,
        }
    }
    drop(stream);
    Ok(())
}

/// Starts loopback capture of the default output device. Returns the stream, its rate and channels.
fn capture(tx: mpsc::Sender<Vec<i16>>, etx: mpsc::UnboundedSender<String>) -> Result<(cpal::Stream, u32, u16), String> {
    let device = cpal::default_host().default_output_device().ok_or("This computer has no sound output")?;
    let config = device.default_output_config().map_err(capture_error)?;
    if !(1..=8).contains(&config.channels()) {
        return Err(format!("Can't share sound with {} channels", config.channels()));
    }
    let c = config.config();
    let stream = match config.sample_format() {
        SampleFormat::F32 => build_capture::<f32>(&device, c, tx, etx),
        SampleFormat::I16 => build_capture::<i16>(&device, c, tx, etx),
        SampleFormat::I32 => build_capture::<i32>(&device, c, tx, etx),
        f => return Err(format!("Can't share sound in {f} format")),
    }
    .map_err(capture_error)?;
    stream.play().map_err(capture_error)?;
    Ok((stream, c.sample_rate, c.channels))
}

fn build_capture<T>(
    device: &cpal::Device,
    config: StreamConfig,
    tx: mpsc::Sender<Vec<i16>>,
    etx: mpsc::UnboundedSender<String>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let frame = config.channels as usize;
    // an output device used as input = loopback (WASAPI loopback / CoreAudio process tap)
    device.build_input_stream(
        config,
        move |data: &[T], _: &_| {
            for part in data.chunks(MAX_FRAMES * frame) {
                let _ = tx.try_send(part.iter().map(|s| s.to_sample::<i16>()).collect());
            }
        },
        move |e| {
            if fatal(&e) {
                let _ = etx.send(e.to_string());
            }
        },
        None,
    )
}

fn capture_error(e: cpal::Error) -> String {
    #[cfg(target_os = "macos")]
    return format!("Sharing sound needs macOS 14.6 or later and the System Audio Recording permission ({e})");
    #[cfg(not(target_os = "macos"))]
    format!("Can't capture sound: {e}")
}

fn fatal(e: &cpal::Error) -> bool {
    !matches!(e.kind(), cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied)
}

/// cpal joins COM (single-threaded apartment) on the thread that builds a stream on Windows; keep that
/// off the async runtime's threads. Also turns a setup panic into an error.
async fn off_runtime<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.await.unwrap_or_else(|_| Err("Sound setup failed".into()))
}

fn encode(samples: &[i16], channels: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&((samples.len() / channels as usize) as u16).to_le_bytes());
    out.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

async fn read_header<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<(u32, u16)> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).await?;
    let rate = r.read_u32_le().await?;
    let channels = r.read_u8().await? as u16;
    if &magic != MAGIC || !(8_000..=384_000).contains(&rate) || !(1..=8).contains(&channels) {
        return Err(invalid("Not a sound stream"));
    }
    Ok((rate, channels))
}

/// Reads one chunk into `samples`; `bytes` is a reused scratch buffer.
async fn read_chunk<R: AsyncRead + Unpin>(r: &mut R, channels: u16, bytes: &mut Vec<u8>, samples: &mut Vec<i16>) -> io::Result<()> {
    let frames = r.read_u16_le().await? as usize;
    if frames == 0 || frames > MAX_FRAMES {
        return Err(invalid("Bad sound chunk"));
    }
    bytes.resize(frames * channels as usize * 2, 0);
    r.read_exact(bytes).await?;
    samples.clear();
    samples.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
    Ok(())
}

/// Sender frames → the output device's channels and rate, as f32.
struct Convert {
    in_ch: usize,
    out_ch: usize,
    /// input frames per output frame
    step: f64,
    /// position of the next output frame in the current input chunk; -1 = `prev`
    pos: f64,
    prev: Vec<f32>,
    mapped: Vec<f32>,
    out: Vec<f32>,
}

impl Convert {
    fn new(in_ch: u16, from: u32, out_ch: u16, to: u32) -> Self {
        Self {
            in_ch: in_ch as usize,
            out_ch: out_ch as usize,
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: vec![0.0; out_ch as usize],
            mapped: Vec::new(),
            out: Vec::new(),
        }
    }

    fn run(&mut self, input: &[i16]) -> &[f32] {
        let (ic, oc) = (self.in_ch, self.out_ch);
        self.mapped.clear();
        for f in input.chunks_exact(ic) {
            for c in 0..oc {
                let v = if oc == 1 {
                    f.iter().map(|&s| s as f32).sum::<f32>() / ic as f32
                } else if ic == 1 {
                    f[0] as f32
                } else if c < ic {
                    f[c] as f32
                } else {
                    0.0 // stereo into surround: front left/right only
                };
                self.mapped.push(v / 32768.0);
            }
        }
        if self.step == 1.0 {
            return &self.mapped;
        }
        // linear interpolation, carrying the last frame and the fractional position across chunks
        let n = self.mapped.len() / oc;
        self.out.clear();
        if n == 0 {
            return &self.out;
        }
        while self.pos < (n - 1) as f64 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let i = i as isize;
            for c in 0..oc {
                let a = if i < 0 { self.prev[c] } else { self.mapped[i as usize * oc + c] };
                let b = self.mapped[(i + 1) as usize * oc + c];
                self.out.push(a + (b - a) * t);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev.copy_from_slice(&self.mapped[(n - 1) * oc..]);
        &self.out
    }
}

/// Played samples, interleaved at the output's channels. Waits until `target` is buffered before
/// playing (again after running dry), and skips ahead to `target` once more than `max` is queued.
struct Jitter {
    buf: VecDeque<f32>,
    primed: bool,
    target: usize,
    max: usize,
}

impl Jitter {
    fn new(rate: u32, channels: u16) -> Self {
        let per_ms = rate as usize * channels as usize / 1000;
        let frame = channels as usize;
        Self {
            buf: VecDeque::new(),
            primed: false,
            target: per_ms * JITTER_MS / frame * frame,
            max: per_ms * MAX_BEHIND_MS / frame * frame,
        }
    }

    fn push(&mut self, s: &[f32]) {
        self.buf.extend(s);
        if self.buf.len() > self.max {
            self.buf.drain(..self.buf.len() - self.target);
        }
    }

    fn fill<T: FromSample<f32>>(&mut self, out: &mut [T]) {
        if self.buf.len() >= self.target {
            self.primed = true;
        }
        for s in out {
            let v = if self.primed { self.buf.pop_front() } else { None };
            self.primed &= v.is_some();
            *s = T::from_sample_(v.unwrap_or(0.0));
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Playback {
    stream: cpal::Stream,
    jitter: Arc<Mutex<Jitter>>,
    convert: Convert,
    errors: mpsc::UnboundedReceiver<String>,
}

/// Opens the default output at the sender's format if it takes it, else at its own default format.
fn play(rate: u32, channels: u16) -> Result<Playback, String> {
    let device = cpal::default_host().default_output_device().ok_or("This computer has no sound output")?;
    let usable = |f: SampleFormat| matches!(f, SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32);
    let config = match device.supported_output_configs().ok().and_then(|mut all| {
        all.find(|r| r.channels() == channels && r.contains_rate(rate) && usable(r.sample_format()))
            .map(|r| r.with_sample_rate(rate))
    }) {
        Some(c) => c,
        None => device.default_output_config().map_err(|e| e.to_string())?,
    };
    let c = config.config();
    let jitter = Arc::new(Mutex::new(Jitter::new(c.sample_rate, c.channels)));
    let (etx, errors) = mpsc::unbounded_channel();
    let stream = match config.sample_format() {
        SampleFormat::F32 => build_playback::<f32>(&device, c, jitter.clone(), etx),
        SampleFormat::I16 => build_playback::<i16>(&device, c, jitter.clone(), etx),
        SampleFormat::I32 => build_playback::<i32>(&device, c, jitter.clone(), etx),
        f => return Err(format!("Can't play sound in {f} format")),
    }
    .map_err(|e| format!("Can't play sound: {e}"))?;
    stream.play().map_err(|e| format!("Can't play sound: {e}"))?;
    Ok(Playback { stream, jitter, convert: Convert::new(channels, rate, c.channels, c.sample_rate), errors })
}

fn build_playback<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: StreamConfig,
    jitter: Arc<Mutex<Jitter>>,
    etx: mpsc::UnboundedSender<String>,
) -> Result<cpal::Stream, cpal::Error> {
    device.build_output_stream(
        config,
        move |out: &mut [T], _: &_| lock(&jitter).fill(out),
        move |e| {
            if fatal(&e) {
                let _ = etx.send(e.to_string());
            }
        },
        None,
    )
}

/// device fingerprint → (session id, task)
static LISTENING: Mutex<Option<HashMap<String, (u64, JoinHandle<()>)>>> = Mutex::new(None);
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Plays paired device `fingerprint`'s sound here (`on`) or stops. Emits `sound`
/// `{ device, on: false, error }` when it stops on its own.
#[tauri::command]
pub async fn set_listening(app: AppHandle, fingerprint: String, on: bool) -> Result<(), String> {
    if !on {
        if let Some((_, task)) = lock(&LISTENING).get_or_insert_default().remove(&fingerprint) {
            task.abort();
        }
        return Ok(());
    }
    if lock(&LISTENING).get_or_insert_default().contains_key(&fingerprint) {
        return Ok(());
    }
    let mut s = crate::open_service(&app, &fingerprint, crate::kvm::Service::Sound).await?;
    let (rate, channels) = timeout(Duration::from_secs(5), read_header(&mut s))
        .await
        .map_err(|_| "That computer didn't start sharing its sound".to_string())?
        .map_err(|e| e.to_string())?;
    let mut p = off_runtime(move || play(rate, channels)).await?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let device = fingerprint.clone();
    // held while spawning so a session that ends at once still finds itself registered
    let mut listening = lock(&LISTENING);
    let task = tauri::async_runtime::spawn(async move {
        let (mut bytes, mut samples) = (Vec::new(), Vec::new());
        let error = loop {
            tokio::select! {
                r = read_chunk(&mut s, channels, &mut bytes, &mut samples) => if let Err(e) = r {
                    break if e.kind() == io::ErrorKind::UnexpectedEof { "The other computer stopped sharing its sound".into() } else { e.to_string() };
                },
                e = p.errors.recv() => break e.unwrap_or_default(),
            }
            lock(&p.jitter).push(p.convert.run(&samples));
        };
        drop(p.stream);
        let mut listening = lock(&LISTENING);
        let map = listening.get_or_insert_default();
        if map.get(&device).is_some_and(|(i, _)| *i == id) {
            map.remove(&device);
            drop(listening);
            let _ = app.emit("sound", json!({ "device": device, "on": false, "error": error }));
        }
    });
    if let Some((_, old)) = listening.get_or_insert_default().insert(fingerprint, (id, task)) {
        old.abort();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wire_round_trip_and_garbage() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&48_000u32.to_le_bytes());
        out.push(2);
        encode(&[1, -2, 3, i16::MIN], 2, &mut out);
        encode(&[i16::MAX, 0], 2, &mut out);
        a.write_all(&out).await.unwrap();
        assert_eq!(read_header(&mut b).await.unwrap(), (48_000, 2));
        let (mut bytes, mut samples) = (Vec::new(), Vec::new());
        read_chunk(&mut b, 2, &mut bytes, &mut samples).await.unwrap();
        assert_eq!(samples, [1, -2, 3, i16::MIN]);
        read_chunk(&mut b, 2, &mut bytes, &mut samples).await.unwrap();
        assert_eq!(samples, [i16::MAX, 0]);

        for bad in [&b"LKSX\x80\xbb\x00\x00\x02"[..], b"LKSN\x80\xbb\x00\x00\x00", b"LKSN\x10\x00\x00\x00\x02"] {
            assert!(read_header(&mut &bad[..]).await.is_err());
        }
        for frames in [0u16, MAX_FRAMES as u16 + 1] {
            let bad = frames.to_le_bytes();
            assert!(read_chunk(&mut &bad[..], 2, &mut bytes, &mut samples).await.is_err());
        }
        assert!(read_chunk(&mut &[2u8, 0, 1, 0][..], 2, &mut bytes, &mut samples).await.is_err(), "truncated");
    }

    #[test]
    fn channel_mapping() {
        assert_eq!(Convert::new(2, 48_000, 1, 48_000).run(&[16384, 0]), [0.25]);
        assert_eq!(Convert::new(1, 48_000, 2, 48_000).run(&[16384]), [0.5, 0.5]);
        assert_eq!(Convert::new(2, 48_000, 4, 48_000).run(&[16384, -16384]), [0.5, -0.5, 0.0, 0.0]);
    }

    #[test]
    fn resample_is_continuous_across_chunks() {
        let mut c = Convert::new(1, 24_000, 1, 48_000);
        let mut got = c.run(&[0, 1000, 2000, 3000]).to_vec();
        got.extend_from_slice(c.run(&[4000, 5000, 6000, 7000]));
        let want: Vec<f32> = (0..14).map(|i| i as f32 * 500.0 / 32768.0).collect();
        assert_eq!(got.len(), want.len());
        assert!(got.iter().zip(&want).all(|(g, w)| (g - w).abs() < 1e-6), "{got:?}");

        let mut down = Convert::new(2, 96_000, 2, 48_000);
        assert_eq!(down.run(&[0, 0, 100, 100, 200, 200, 300, 300]).len(), 4, "2 frames from 4");
    }

    #[test]
    fn jitter_primes_underruns_and_skips_ahead() {
        let mut j = Jitter::new(1000, 1); // 80 samples target, 300 max
        let mut out = [1.0f32; 4];
        j.push(&[0.5; 40]);
        j.fill(&mut out);
        assert_eq!(out, [0.0; 4], "silence until primed");
        j.push(&[0.5; 40]);
        j.fill(&mut out);
        assert_eq!(out, [0.5; 4]);
        j.push(&[0.25; 400]);
        assert_eq!(j.buf.len(), 80, "too far behind: skip to target");
        assert!(j.buf.iter().all(|&v| v == 0.25));
        let mut big = [1.0f32; 100];
        j.fill(&mut big);
        assert!(big[..80].iter().all(|&v| v == 0.25) && big[80..].iter().all(|&v| v == 0.0));
        assert!(!j.primed);
    }
}
