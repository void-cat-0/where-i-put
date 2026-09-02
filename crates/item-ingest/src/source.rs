//! Frame sources. `MockSource` keeps the skeleton runnable with no hardware;
//! `NokhwaSource` (feature `camera`) is the USB/local-webcam implementation;
//! `RtspSource` (feature `rtsp`) pulls IP cameras through ffmpeg-next.

use item_core::FrameMeta;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("no frame available")]
    Eof,
    #[error(transparent)]
    Capture(#[from] Box<dyn std::error::Error + Send + Sync>),
}

pub struct Frame {
    pub meta: FrameMeta,
    /// RGB8 pixels, row-major, width*height*3 long.
    pub rgb: Vec<u8>,
}

pub trait FrameSource: Send {
    /// Block until the next frame or return Err(Eof).
    fn next_frame(&mut self) -> Result<Frame, SourceError>;
}

/// Deterministic stub for tests and demos: yields a fixed number of blank
/// frames so the ingest -> store path can be exercised end to end.
pub struct MockSource {
    camera_id: String,
    remaining: u32,
    width: u32,
    height: u32,
}

impl MockSource {
    pub fn new(camera_id: impl Into<String>, frames: u32, size: (u32, u32)) -> Self {
        Self { camera_id: camera_id.into(), remaining: frames, width: size.0, height: size.1 }
    }
}

impl FrameSource for MockSource {
    fn next_frame(&mut self) -> Result<Frame, SourceError> {
        if self.remaining == 0 {
            return Err(SourceError::Eof);
        }
        self.remaining -= 1;
        Ok(Frame {
            meta: FrameMeta {
                camera_id: self.camera_id.clone(),
                captured_at: crate::now(),
                width: self.width,
                height: self.height,
                snapshot_path: None,
            },
            rgb: vec![0u8; (self.width * self.height * 3) as usize],
        })
    }
}

#[cfg(feature = "camera")]
pub mod nokhwa {
    use super::{Frame, FrameSource, SourceError};
    use item_core::FrameMeta;

    pub struct NokhwaSource {
        cam: ::nokhwa::input_api::Camera,
        camera_id: String,
    }

    impl NokhwaSource {
        /// `index` is the OS camera index (0 = built-in webcam on most machines).
        pub fn new(camera_id: impl Into<String>, index: usize) -> Result<Self, SourceError> {
            use ::nokhwa::{parameters::DeviceApi, query_camera};
            let config = query_camera(DeviceApi::Auto, index)
                .map_err(|e| SourceError::Capture(Box::new(e)))?;
            let cam = ::nokhwa::input_api::Camera::new(config)
                .map_err(|e| SourceError::Capture(Box::new(e)))?;
            Ok(Self { cam, camera_id: camera_id.into() })
        }
    }

    impl FrameSource for NokhwaSource {
        fn next_frame(&mut self) -> Result<Frame, SourceError> {
            let buf = self.cam.read().map_err(|e| SourceError::Capture(Box::new(e)))?;
            let img = buf
                .decode()
                .map_err(|e| SourceError::Capture(Box::new(e)))?
                .to_rgb8();
            let (w, h) = img.dimensions();
            Ok(Frame {
                meta: FrameMeta {
                    camera_id: self.camera_id.clone(),
                    captured_at: crate::now(),
                    width: w,
                    height: h,
                    snapshot_path: None,
                },
                rgb: img.into_raw(),
            })
        }
    }
}

#[cfg(feature = "rtsp")]
pub mod rtsp {
    //! RTSP via FFmpeg (libavformat + libswscale). Links native libav*; the
    //! build needs FFMPEG_DIR pointing at an FFmpeg dev distribution
    //! (see README "RTSP backend"). Idiom follows rust-ffmpeg's own
    //! dump-frames example: packets() -> send_packet -> drain receive_frame.
    //!
    //! Design notes:
    //! - `rtsp_transport=tcp`: home Wi-Fi drops UDP RTP constantly; TCP keeps
    //!   frames whole at the cost of a little latency. `timeout` bounds socket
    //!   stalls at 5 s so a dead camera surfaces as an error, not a hang.
    //! - Frames are converted to packed RGB8 via swscale, matching the
    //!   `Frame::rgb` contract, so detectors never touch pixel formats.
    //! - `captured_at` is wall-clock arrival time, not stream PTS: good enough
    //!   for dedup windows; switch to PTS-epoch mapping if we ever need exact
    //!   event timing.
    //! - No auto-reconnect yet: Eof/Err surfaces to the caller, which is the
    //!   hook point for a supervision loop.

    use ffmpeg_next as ffmpeg;

    use super::{Frame, FrameSource, SourceError};
    use item_core::FrameMeta;

    pub struct RtspSource {
        camera_id: String,
        input: ffmpeg::format::context::input::Input,
        video_index: usize,
        decoder: ffmpeg::codec::decoder::Video,
        scaler: ffmpeg::software::scaling::context::Context,
        /// Preallocated RGB24 target for the scaler (decoder-size).
        rgb_frame: ffmpeg::frame::Video,
        /// Decode buffer reused across next_frame calls.
        decoded: ffmpeg::frame::Video,
    }

    impl RtspSource {
        /// Open `url`, e.g.
        /// `rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101`.
        pub fn new(camera_id: impl Into<String>, url: &str) -> Result<Self, SourceError> {
            let err = |e: ffmpeg::Error| SourceError::Capture(Box::new(e));
            ffmpeg::init().map_err(err)?;

            let mut dic = ffmpeg::Dictionary::new();
            dic.set("rtsp_transport", "tcp");
            dic.set("timeout", "5000000"); // microseconds; avoids "timed out" on read
            // Live-low-latency tuning: ffmpeg's defaults buffer ~0.5s of the
            // stream during input probing and keep it queued as "playback
            // backlog" forever for a live source. Keep reads minimal so
            // packets flow out as they arrive. `timeout` (socket I/O) is
            // distinct from `rw_timeout`; for RTSP, use the `timeout`
            // above and these two for backlog.
            dic.set("probesize", "32"); // bytes needed to guess streams; tiny
            dic.set("analyzeduration", "100000"); // 0.1s of analysis at most
            dic.set("fflags", "nobuffer+flush_packets"); // minimize read queueing
            dic.set("max_delay", "100000"); // microseconds of muxer jitter buffer

            let input = ffmpeg::format::input_with_dictionary(url, dic).map_err(err)?;
            let (video_index, decoder, scaler, rgb_frame) = {
                let stream =
                    input.streams().best(ffmpeg::media::Type::Video).ok_or_else(|| {
                        SourceError::Capture("rtsp stream has no video".into())
                    })?;
                let video_index = stream.index();
                let mut context =
                    ffmpeg::codec::context::Context::from_parameters(stream.parameters())
                        .map_err(err)?;
                // HEVC software decode is frame-heavy; rust-ffmpeg defaults to a
                // single decoder thread (ffmpeg CLI picks "auto"). 4 threads
                // brought 720p25 preview from ~13 fps to full rate on an
                // 8-core machine. Cost: frame threading reorders ~4 frames
                // in flight (~160ms latency tax; slice threading would avoid
                // it but HEVC slice parallelism is rare in camera streams).
                context.set_threading(ffmpeg::codec::threading::Config {
                    kind: ffmpeg::codec::threading::Type::Frame,
                    count: 4,
                    ..Default::default()
                });
                // No B-frames in this stream (verified: has_b_frames=0), so
                // telling the decoder to emit each frame immediately is safe
                // and shaves one frame of internal reordering.
                context.set_flags(ffmpeg::codec::Flags::LOW_DELAY);
                let decoder = context.decoder().video().map_err(err)?;
                let scaler = ffmpeg::software::scaling::context::Context::get(
                    decoder.format(),
                    decoder.width(),
                    decoder.height(),
                    ffmpeg::format::Pixel::RGB24,
                    decoder.width(),
                    decoder.height(),
                    ffmpeg::software::scaling::flag::Flags::BILINEAR,
                )
                .map_err(err)?;
                let rgb_frame = ffmpeg::frame::Video::new(
                    ffmpeg::format::Pixel::RGB24,
                    decoder.width(),
                    decoder.height(),
                );
                (video_index, decoder, scaler, rgb_frame)
            };

            Ok(Self {
                camera_id: camera_id.into(),
                input,
                video_index,
                decoder,
                scaler,
                rgb_frame,
                decoded: ffmpeg::frame::Video::empty(),
            })
        }
    }

    impl FrameSource for RtspSource {
        fn next_frame(&mut self) -> Result<Frame, SourceError> {
            let err = |e: ffmpeg::Error| SourceError::Capture(Box::new(e));
            loop {
                match self.decoder.receive_frame(&mut self.decoded) {
                    Ok(()) => break,
                    Err(ffmpeg::Error::Eof) => return Err(SourceError::Eof),
                    // EAGAIN arrives as Error::Other{errno}, so any non-Eof
                    // error means "needs more packets": feed the next video
                    // packet (non-video ones are skipped).
                    Err(_) => {
                        let mut submitted = false;
                        for (s, p) in self.input.packets() {
                            if s.index() != self.video_index {
                                continue;
                            }
                            // EAGAIN on send means drain the decoder first.
                            self.decoder.send_packet(&p).ok();
                            submitted = true;
                            break;
                        }
                        if !submitted {
                            return Err(SourceError::Eof);
                        }
                    }
                }
            }

            self.scaler.run(&self.decoded, &mut self.rgb_frame).map_err(err)?;
            let (w, h) = (self.rgb_frame.width(), self.rgb_frame.height());
            let stride = self.rgb_frame.stride(0) as usize;
            let row = (w as usize) * 3;
            let mut rgb = vec![0u8; row * h as usize];
            let data = self.rgb_frame.data(0);
            for y in 0..h as usize {
                rgb[y * row..(y + 1) * row].copy_from_slice(&data[y * stride..y * stride + row]);
            }
            Ok(Frame {
                meta: FrameMeta {
                    camera_id: self.camera_id.clone(),
                    captured_at: crate::now(),
                    width: w,
                    height: h,
                    snapshot_path: None,
                },
                rgb,
            })
        }
    }

    // SAFETY: the only non-Send member is swscale's raw `*mut SwsContext`,
    // which has no thread affinity; it is only touched via `&mut self`. The
    // intended usage is one dedicated (blocking) camera thread per source,
    // but the type may legitimately be moved between threads between frames.
    unsafe impl Send for RtspSource {}
}
