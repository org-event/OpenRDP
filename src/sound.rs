//! The Mac's sound, played on the client (MS-RDPEA, the `rdpsnd` channel).
//!
//! ScreenCaptureKit captures what the Mac plays, without this server's own
//! sound, as 32-bit float samples, one plane per channel. They go out as
//! 16-bit PCM, which every client takes: 48 kHz stereo is 1.5 Mbit/s, little
//! next to the video. The capture runs only while a client has the channel
//! open; the Mac keeps playing through its own speakers too.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use dispatch2::{DispatchQueue, DispatchRetained};
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError};
use ironrdp_server::{
    RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread as _, DefinedClass as _, define_class, msg_send};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGMainDisplayID;
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

const CHANNELS: u16 = 2;
const BITS: u16 = 16;
/// Offered in this order; ScreenCaptureKit captures at the chosen rate.
const RATES: [u32; 2] = [48_000, 44_100];

fn pcm(rate: u32) -> AudioFormat {
    let block_align = CHANNELS * BITS / 8;
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: CHANNELS,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(block_align),
        n_block_align: block_align,
        bits_per_sample: BITS,
        data: None,
    }
}

#[derive(Default)]
pub struct MacSoundFactory {
    events: Option<UnboundedSender<ServerEvent>>,
}

impl ServerEventSender for MacSoundFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.events = Some(sender);
    }
}

impl SoundServerFactory for MacSoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(MacSound {
            formats: RATES.map(pcm).to_vec(),
            events: self.events.clone(),
            capture: None,
        })
    }
}

#[derive(Debug)]
struct MacSound {
    formats: Vec<AudioFormat>,
    events: Option<UnboundedSender<ServerEvent>>,
    capture: Option<AudioCapture>,
}

impl RdpsndServerHandler for MacSound {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        // In our order of preference already.
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let events = self
            .events
            .clone()
            .ok_or_else(|| boxed(anyhow!("no server event sender")))?;
        let rate = format.format().n_samples_per_sec;
        let capture = AudioCapture::start(rate, events).map_err(boxed)?;
        info!(rate, "sound started");
        self.capture = Some(capture);
        Ok(())
    }

    fn stop(&mut self) {
        if self.capture.take().is_some() {
            info!("sound stopped");
        }
    }
}

#[derive(Debug)]
struct SoundError(String);

impl std::fmt::Display for SoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SoundError {}

fn boxed(err: anyhow::Error) -> Box<dyn RdpsndError> {
    Box::new(SoundError(format!("{err:#}")))
}

struct OutputIvars {
    events: UnboundedSender<ServerEvent>,
    started: Instant,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OpenRdpSoundOutput"]
    #[ivars = OutputIvars]
    struct SoundOutput;

    unsafe impl NSObjectProtocol for SoundOutput {}

    unsafe impl SCStreamOutput for SoundOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            // SAFETY: the sample buffer is valid for the duration of the callback.
            match unsafe { pcm16(sample) } {
                Ok(Some(data)) => {
                    let ivars = self.ivars();
                    let timestamp =
                        u32::try_from(ivars.started.elapsed().as_millis()).unwrap_or(u32::MAX);
                    let _ = ivars
                        .events
                        .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                            data, timestamp,
                        )));
                }
                Ok(None) => {}
                Err(err) => debug!(%err, "audio sample skipped"),
            }
        }
    }

    unsafe impl SCStreamDelegate for SoundOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            warn!(error = %error.localizedDescription(), "sound capture stopped");
        }
    }
);

impl SoundOutput {
    fn new(events: UnboundedSender<ServerEvent>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            events,
            started: Instant::now(),
        });
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Converts one ScreenCaptureKit audio buffer, float planes, to interleaved
/// 16-bit little-endian PCM.
unsafe fn pcm16(sample: &CMSampleBuffer) -> anyhow::Result<Option<Vec<u8>>> {
    // SAFETY: reading a valid sample buffer.
    let (frames, block) = unsafe { (sample.num_samples(), sample.data_buffer()) };
    let Some(block) = block else { return Ok(None) };
    let frames = usize::try_from(frames).context("sample count")?;
    if frames == 0 {
        return Ok(None);
    }
    let channels = usize::from(CHANNELS);
    // SAFETY: reading a valid block buffer.
    let length = unsafe { block.data_length() };
    anyhow::ensure!(
        length == frames * channels * 4,
        "{length} bytes for {frames} frames of {channels} float channels"
    );
    let mut floats = vec![0f32; frames * channels];
    // SAFETY: `floats` holds exactly `length` bytes.
    let status = unsafe {
        block.copy_data_bytes(
            0,
            length,
            std::ptr::NonNull::new_unchecked(floats.as_mut_ptr().cast()),
        )
    };
    anyhow::ensure!(status == 0, "CMBlockBufferCopyDataBytes: {status}");
    Ok(Some(interleave(&floats, frames, channels)))
}

/// `planes` holds `channels` runs of `frames` samples each.
fn interleave(planes: &[f32], frames: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames * channels * 2);
    for frame in 0..frames {
        for channel in 0..channels {
            let value = planes[channel * frames + frame].clamp(-1.0, 1.0);
            let sample = (value * f32::from(i16::MAX)).round() as i16;
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
    out
}

/// ScreenCaptureKit stream that delivers only the Mac's sound.
struct AudioCapture {
    stream: Retained<SCStream>,
    _output: Retained<SoundOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

impl std::fmt::Debug for AudioCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioCapture")
    }
}

// SAFETY: SCStream may be stopped from any thread; the other fields are only kept alive.
unsafe impl Send for AudioCapture {}

impl AudioCapture {
    fn start(rate: u32, events: UnboundedSender<ServerEvent>) -> anyhow::Result<Self> {
        // The stream needs a display; it captures the whole system's sound
        // whichever display that is.
        let display = crate::capture::find_display(CGMainDisplayID())?;
        // SAFETY: plain Objective-C object construction and property setters.
        unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(rate.try_into().unwrap_or(48_000));
            config.setChannelCount(CHANNELS.try_into().unwrap_or(2));
            // The picture is not used: keep it tiny and rare.
            config.setWidth(2);
            config.setHeight(2);
            config.setDestinationRect(CGRect::new(CGPoint::ZERO, CGSize::new(2.0, 2.0)));
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: 1,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });

            let output = SoundOutput::new(events);
            let queue = DispatchQueue::new("openrdp.sound", None);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            // A stream without a screen output logs every frame it drops.
            for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*output),
                        kind,
                        Some(&queue),
                    )
                    .map_err(|e| anyhow!("addStreamOutput: {}", e.localizedDescription()))?;
            }

            let (tx, rx) = mpsc::channel();
            let done = block2::RcBlock::new(move |err: *mut NSError| {
                let result = match err.as_ref() {
                    Some(err) => Err(err.localizedDescription().to_string()),
                    None => Ok(()),
                };
                let _ = tx.send(result);
            });
            stream.startCaptureWithCompletionHandler(Some(&done));
            rx.recv_timeout(Duration::from_secs(2))
                .context("sound capture did not start")?
                .map_err(|e| anyhow!("startCapture: {e}"))?;

            Ok(Self {
                stream,
                _output: output,
                _queue: queue,
            })
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel();
        let done = block2::RcBlock::new(move |_err: *mut NSError| {
            let _ = tx.send(());
        });
        // SAFETY: stopping a started stream.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) };
        if rx.recv_timeout(Duration::from_secs(2)).is_err() {
            warn!("sound capture did not stop in time");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaves_planes_into_pcm16() {
        // Two frames: left 1.0, -1.0; right 0.5, 2.0 (clipped).
        let planes = [1.0, -1.0, 0.5, 2.0];
        let out = interleave(&planes, 2, 2);
        let samples: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(samples, [32767, 16384, -32767, 32767]);
    }

    #[test]
    fn formats_are_consistent_pcm() {
        for rate in RATES {
            let format = pcm(rate);
            assert_eq!(format.n_block_align, 4);
            assert_eq!(format.n_avg_bytes_per_sec, rate * 4);
        }
    }
}
