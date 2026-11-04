//! The start-up sequence and frame streaming, exactly as the capture showed them.

use std::thread;
use std::time::{Duration, Instant};

use crate::packet::{
    BRIGHTNESS_ECHO_FRAMES, FrameMarker, STARTUP_0C, STARTUP_1A, STARTUP_1D, STARTUP_18,
    brightness_command, packets,
};
use crate::{
    BootAnimation, Brightness, FeatureQuery, FeatureReply, Frame, HEIGHT, StandbyImage, Transport,
    TransportError, WIDTH,
};

/// How often GET 0x0d is polled during a boot-animation upload, as in the capture.
const BOOT_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// How long one block may take to be confirmed; the capture needed about 0.27 s.
const BOOT_BLOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling pace of a boot-animation upload.
#[derive(Debug, Clone, Copy)]
struct BootTiming {
    poll: Duration,
    block_timeout: Duration,
}

/// What the panel reported during start-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelInfo {
    /// Firmware version string, or `"unknown"`.
    pub firmware: String,
    /// Reported width in pixels.
    pub width: u16,
    /// Reported height in pixels.
    pub height: u16,
}

/// Bytes 1..9 of GET 0x0f; constant `e2 ff ff ff ea ff ff ff` so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusReply(pub [u8; 8]);

/// A started panel session; dropping it closes the device and the panel shows its own image.
#[derive(Debug)]
pub struct Panel<T> {
    transport: T,
    info: PanelInfo,
    marker: FrameMarker,
    /// Brightness to echo in byte 2 and how many frames are left.
    echo: Option<(Brightness, u8)>,
}

impl<T: Transport> Panel<T> {
    /// Runs the start-up sequence exactly as TT RGB Plus 3.0 did.
    ///
    /// Aborts before the first `03 0c` if the panel does not report 480x128.
    ///
    /// # Errors
    /// [`SessionError::UnexpectedGeometry`] for another panel size,
    /// [`SessionError::BadReply`] for a malformed reply, [`SessionError::Transport`]
    /// for a device failure.
    pub fn start(mut transport: T) -> Result<Self, SessionError> {
        let firmware = parse_firmware(&query(&mut transport, FeatureQuery::Firmware)?);
        query(&mut transport, FeatureQuery::Report10)?;
        transport.set_feature(&STARTUP_18)?;
        transport.set_feature(&STARTUP_1A)?;
        let (width, height) = parse_geometry(&query(&mut transport, FeatureQuery::Geometry)?)?;
        if (width, height) != (WIDTH, HEIGHT) {
            return Err(SessionError::UnexpectedGeometry { width, height });
        }
        transport.set_feature(&STARTUP_0C)?;
        query(&mut transport, FeatureQuery::Status)?;
        transport.set_feature(&STARTUP_1D)?;
        Ok(Self {
            transport,
            info: PanelInfo {
                firmware,
                width,
                height,
            },
            marker: FrameMarker::FirstAfterStart,
            echo: None,
        })
    }

    /// Sends `03 0c NN`; the next few frames carry NN in byte 2, as TT RGB Plus does.
    /// 0 turns the panel dark; streaming continues.
    ///
    /// # Errors
    /// [`SessionError::Transport`] if the report cannot be sent.
    pub fn set_brightness(&mut self, brightness: Brightness) -> Result<(), SessionError> {
        self.transport
            .set_feature(&brightness_command(brightness))?;
        self.echo = Some((brightness, BRIGHTNESS_ECHO_FRAMES));
        Ok(())
    }

    /// What the panel reported at start-up.
    pub fn info(&self) -> &PanelInfo {
        &self.info
    }

    /// Streams one frame. `BB`: 0x00 on the session's first frame (wins over an echo, which
    /// then starts on the next frame), NN during a brightness echo, else 0xff.
    ///
    /// # Errors
    /// [`SessionError::Transport`] if a packet cannot be written.
    pub fn send_frame(&mut self, frame: &Frame) -> Result<(), SessionError> {
        let marker = match (self.marker, self.echo) {
            (FrameMarker::FirstAfterStart, _) => FrameMarker::FirstAfterStart,
            (_, Some((brightness, _))) => FrameMarker::Brightness(brightness),
            _ => FrameMarker::Continuing,
        };
        for packet in packets(frame.as_bytes(), marker) {
            self.transport.write_output(&packet)?;
        }
        if marker != FrameMarker::FirstAfterStart {
            self.echo = match self.echo {
                Some((brightness, left)) if left > 1 => Some((brightness, left - 1)),
                _ => None,
            };
        }
        self.marker = FrameMarker::Continuing;
        Ok(())
    }

    /// Uploads `image` as the panel's standby image: **writes to the panel's flash**.
    ///
    /// Sends the packets back to back (slot 1, then slot 0), as the capture showed, with no
    /// other traffic in between. The next frame then carries the first-frame marker (0x00).
    /// On an error the upload may be partial: drop the session.
    ///
    /// # Errors
    /// [`SessionError::Transport`] if a packet cannot be written.
    pub fn upload_standby(&mut self, image: &StandbyImage) -> Result<(), SessionError> {
        for packet in image.packets() {
            self.transport.write_output(&packet)?;
        }
        self.marker = FrameMarker::FirstAfterStart;
        Ok(())
    }

    /// Uploads `animation` as the panel's boot animation: **writes to the panel's flash**.
    ///
    /// Exactly the captured flow: block 0 (the table) and block 1 are sent back to back, then
    /// GET 0x0d is polled about every 5 ms until it reads `0d NN` with NN the block just
    /// sent, then the next block goes out; after the last block it polls until NN equals the
    /// block count. `progress` gets the percentage (10, 20, ... 100 for ten blocks) each time
    /// a block is confirmed. Finally GET 0x0f runs and the next frame carries the first-frame
    /// marker (0x00). Callers must not stream frames or send keep-alives meanwhile.
    ///
    /// On an error the upload may be partial: drop the session.
    ///
    /// # Errors
    /// [`SessionError::BootUploadTimeout`] if a block is not confirmed within 5 s,
    /// [`SessionError::BadReply`] for a malformed reply, [`SessionError::Transport`] if a
    /// packet cannot be written or a report not read.
    pub fn upload_boot_animation(
        &mut self,
        animation: &BootAnimation,
        progress: impl FnMut(u8),
    ) -> Result<(), SessionError> {
        let timing = BootTiming {
            poll: BOOT_POLL_INTERVAL,
            block_timeout: BOOT_BLOCK_TIMEOUT,
        };
        self.upload_boot_animation_timed(animation, progress, timing)
    }

    fn upload_boot_animation_timed(
        &mut self,
        animation: &BootAnimation,
        mut progress: impl FnMut(u8),
        timing: BootTiming,
    ) -> Result<(), SessionError> {
        let blocks = animation.blocks();
        let total = blocks.len() - 1;
        // The marker changes as soon as the panel could have seen a packet.
        self.marker = FrameMarker::FirstAfterStart;
        for packet in blocks[0].iter().chain(&blocks[1]) {
            self.transport.write_output(packet)?;
        }
        let mut sent = 1;
        loop {
            self.wait_for_block(sent, timing)?;
            let percent = u8::try_from(sent * 100 / total).expect("at most 100 percent");
            progress(percent);
            if sent == total {
                break;
            }
            sent += 1;
            for packet in &blocks[sent] {
                self.transport.write_output(packet)?;
            }
        }
        self.keep_alive()?;
        Ok(())
    }

    /// Polls GET 0x0d until it reads `0d block`.
    fn wait_for_block(&mut self, block: usize, timing: BootTiming) -> Result<(), SessionError> {
        let started = Instant::now();
        loop {
            let reply = query(&mut self.transport, FeatureQuery::UploadProgress)?;
            let done = reply.as_bytes().get(1).ok_or(SessionError::BadReply {
                report_id: FeatureQuery::UploadProgress.report_id(),
            })?;
            if usize::from(*done) == block {
                return Ok(());
            }
            if started.elapsed() >= timing.block_timeout {
                return Err(SessionError::BootUploadTimeout { block });
            }
            thread::sleep(timing.poll);
        }
    }

    /// GET 0x0f; the panel expects it every [`KEEP_ALIVE_INTERVAL`](crate::KEEP_ALIVE_INTERVAL).
    ///
    /// # Errors
    /// [`SessionError::BadReply`] for a short reply, [`SessionError::Transport`] otherwise.
    pub fn keep_alive(&mut self) -> Result<StatusReply, SessionError> {
        let reply = query(&mut self.transport, FeatureQuery::Status)?;
        let status: [u8; 8] = reply
            .as_bytes()
            .get(1..9)
            .and_then(|b| b.try_into().ok())
            .ok_or(SessionError::BadReply {
                report_id: FeatureQuery::Status.report_id(),
            })?;
        Ok(StatusReply(status))
    }

    /// Ends the session and returns the transport (the panel stays in its current state).
    pub fn into_transport(self) -> T {
        self.transport
    }
}

fn query<T: Transport>(
    transport: &mut T,
    query: FeatureQuery,
) -> Result<FeatureReply, SessionError> {
    let reply = transport.get_feature(query)?;
    if reply.as_bytes().first() == Some(&query.report_id()) {
        Ok(reply)
    } else {
        Err(SessionError::BadReply {
            report_id: query.report_id(),
        })
    }
}

fn parse_firmware(reply: &FeatureReply) -> String {
    let text = reply.as_bytes().get(5..).unwrap_or_default();
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    let text = &text[..end];
    if !text.is_empty() && text.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        String::from_utf8_lossy(text).into_owned()
    } else {
        "unknown".to_owned()
    }
}

fn parse_geometry(reply: &FeatureReply) -> Result<(u16, u16), SessionError> {
    let bytes = reply.as_bytes();
    if bytes.len() < 11 {
        return Err(SessionError::BadReply {
            report_id: FeatureQuery::Geometry.report_id(),
        });
    }
    Ok((
        u16::from_le_bytes([bytes[7], bytes[8]]),
        u16::from_le_bytes([bytes[9], bytes[10]]),
    ))
}

/// Why a panel session failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The device failed; drop the session and rescan.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The panel is not 480x128; nothing was sent that could change it.
    #[error("panel reports {width}x{height}, expected 480x128")]
    UnexpectedGeometry {
        /// Reported width.
        width: u16,
        /// Reported height.
        height: u16,
    },
    /// The panel did not confirm a boot-animation block in time.
    #[error(
        "the panel did not confirm boot animation block {block} within 5 s; \
         the panel may hold a partial boot animation"
    )]
    BootUploadTimeout {
        /// The block that was never confirmed.
        block: usize,
    },
    /// A reply had the wrong report ID or was too short.
    #[error("unexpected reply to GET 0x{report_id:02x}")]
    BadReply {
        /// The report that was queried.
        report_id: u8,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{build_jpeg, sample_jpeg};
    use crate::mock::{MockLog, MockTransport, Sent};

    fn boot_animation() -> BootAnimation {
        // 5 x 16000 bytes: data blocks of 65536 and 14464 bytes.
        let base = build_jpeg(0xc0, 480, 128, 0x22, 0).len() + 4;
        let jpeg = build_jpeg(0xc0, 480, 128, 0x22, 16000 - base);
        BootAnimation::from_jpegs(&vec![jpeg; 5], 100).unwrap()
    }

    const PROGRESS_0: &[u8] = &[0x0d, 0x00];
    const PROGRESS_1: &[u8] = &[0x0d, 0x01];
    const PROGRESS_2: &[u8] = &[0x0d, 0x02];

    fn padded(prefix: &[u8]) -> Vec<u8> {
        let mut out = prefix.to_vec();
        out.resize(64, 0);
        out
    }

    fn started() -> (Panel<MockTransport>, MockLog) {
        let (transport, log) = MockTransport::new();
        (Panel::start(transport).unwrap(), log)
    }

    fn frame() -> Frame {
        Frame::from_jpeg(sample_jpeg(0)).unwrap()
    }

    /// Byte 2 of every output packet logged so far.
    fn markers(log: &MockLog) -> Vec<u8> {
        log.snapshot()
            .into_iter()
            .filter_map(|s| match s {
                Sent::Output(bytes) => Some(bytes[2]),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn startup_order_and_bytes() {
        let (panel, log) = started();
        let expected = vec![
            Sent::GetFeature(0x0a),
            Sent::GetFeature(0x10),
            Sent::SetFeature(padded(&[0x03, 0x18])),
            Sent::SetFeature(padded(&[0x03, 0x1a])),
            Sent::GetFeature(0x07),
            Sent::SetFeature(padded(&[
                0x03, 0x0c, 0x64, 0x00, 0x00, 0x00, 0x00, 0xe0, 0x01, 0x80, 0x00, 0x04, 0x05,
            ])),
            Sent::GetFeature(0x0f),
            Sent::SetFeature(padded(&[
                0x03, 0x1d, 0x00, 0xff, 0xff, 0xea, 0xff, 0xff, 0xff,
            ])),
        ];
        assert_eq!(log.snapshot(), expected);
        assert_eq!(
            panel.info(),
            &PanelInfo {
                firmware: "21.0.5.4".to_owned(),
                width: 480,
                height: 128
            }
        );
    }

    #[test]
    fn wrong_geometry_aborts_before_set_0c() {
        let geometry = [0x07, 0, 0, 0, 0, 0, 0, 0x20, 0x03, 0xe0, 0x01, 0x04, 0x05];
        let (transport, log) = MockTransport::new();
        let transport = transport.with_reply(0x07, &geometry);
        let err = Panel::start(transport).unwrap_err();
        assert_eq!(
            err,
            SessionError::UnexpectedGeometry {
                width: 800,
                height: 480
            }
        );
        let sent = log.snapshot();
        assert_eq!(sent.last(), Some(&Sent::GetFeature(0x07)));
        assert!(!sent.iter().any(|s| matches!(
            s,
            Sent::SetFeature(b) if b[1] == 0x0c || b[1] == 0x1d
        )));
    }

    #[test]
    fn first_frame_marker_is_zero_then_ff() {
        let (mut panel, log) = started();
        panel.send_frame(&frame()).unwrap();
        panel.send_frame(&frame()).unwrap();
        assert_eq!(markers(&log), [0x00, 0xff]);
    }

    #[test]
    fn only_allowlisted_bytes_reach_the_transport() {
        let startup: Vec<Vec<u8>> = vec![
            padded(&[0x03, 0x18]),
            padded(&[0x03, 0x1a]),
            padded(&[
                0x03, 0x0c, 0x64, 0x00, 0x00, 0x00, 0x00, 0xe0, 0x01, 0x80, 0x00, 0x04, 0x05,
            ]),
            padded(&[0x03, 0x1d, 0x00, 0xff, 0xff, 0xea, 0xff, 0xff, 0xff]),
        ];
        let (transport, log) = MockTransport::new();
        let transport =
            transport.with_reply_sequence(0x0d, &[PROGRESS_0, PROGRESS_1, PROGRESS_1, PROGRESS_2]);
        let mut panel = Panel::start(transport).unwrap();
        panel
            .set_brightness(Brightness::try_from(40).unwrap())
            .unwrap();
        panel
            .set_brightness(Brightness::try_from(0).unwrap())
            .unwrap();
        let small = frame();
        let big = Frame::from_jpeg(sample_jpeg(2500)).unwrap();
        for i in 0..12 {
            panel
                .send_frame(if i == 5 { &big } else { &small })
                .unwrap();
        }
        panel.keep_alive().unwrap();
        panel.keep_alive().unwrap();
        let standby = StandbyImage::from_jpeg(build_jpeg(0xc0, 480, 128, 0x22, 2500)).unwrap();
        panel.upload_standby(&standby).unwrap();

        let boot = boot_animation();
        let boot_packets: usize = boot.blocks().iter().map(Vec::len).sum();
        panel.upload_boot_animation(&boot, |_| {}).unwrap();

        let sent = log.snapshot();
        // Boot: all packets, 4 polls of GET 0x0d, then GET 0x0f.
        assert_eq!(sent.len(), 8 + 2 + (11 + 3) + 2 + 6 + boot_packets + 4 + 1);
        for item in sent {
            let ok = match &item {
                Sent::SetFeature(b) => {
                    startup.contains(b)
                        || (b.len() == 64
                            && b[0] == 0x03
                            && b[1] == 0x0c
                            && b[2] <= 0x64
                            && b[3..].iter().all(|&x| x == 0))
                }
                Sent::GetFeature(id) => matches!(id, 0x07 | 0x0a | 0x0d | 0x0f | 0x10),
                Sent::Output(b) => {
                    b.len() == 1024
                        && b[0] == 0x02
                        && ((b[1] == 0x09
                            && (b[2] <= 0x64 || b[2] == 0xff)
                            && (b[3] == 0 || b[3] == 1))
                            || (b[1] == 0x0d
                                && (b[2] == 0 || b[2] == 1)
                                && b[3] == 0
                                && (b[4] == 0 || b[4] == 1)
                                && b[9..16].iter().all(|&x| x == 0))
                            || (b[1] == 0x0c
                                && b[2] <= 10
                                && b[3..6] == [0, 0, 0]
                                && b[6] <= 1
                                && b[11] <= 1
                                && b[12..16] == [0x28, 0x00, 0xa2, 0x8a]))
                }
            };
            assert!(ok, "unexpected item sent to the device: {item:?}");
        }
    }

    #[test]
    fn standby_upload_sends_both_slots_and_resets_marker() {
        let (mut panel, log) = started();
        panel.send_frame(&frame()).unwrap();
        let standby = StandbyImage::from_jpeg(build_jpeg(0xc0, 480, 128, 0x22, 0)).unwrap();
        panel.upload_standby(&standby).unwrap();
        panel.send_frame(&frame()).unwrap();
        panel.send_frame(&frame()).unwrap();
        // live 0x00, standby slot 1, slot 0, then 0x00 again, then 0xff.
        assert_eq!(markers(&log), [0x00, 0x01, 0x00, 0x00, 0xff]);
        let sent = log.snapshot();
        let standby_cmds: Vec<u8> = sent
            .iter()
            .filter_map(|s| match s {
                Sent::Output(b) if b[1] == 0x0d => Some(b[2]),
                _ => None,
            })
            .collect();
        assert_eq!(standby_cmds, [1, 0]);
    }

    #[test]
    fn boot_upload_waits_for_each_block_before_sending_the_next() {
        let (transport, log) = MockTransport::new();
        let transport = transport.with_reply_sequence(
            0x0d,
            &[PROGRESS_0, PROGRESS_0, PROGRESS_1, PROGRESS_1, PROGRESS_2],
        );
        let mut panel = Panel::start(transport).unwrap();
        panel.send_frame(&frame()).unwrap();
        let mark = log.snapshot().len();
        let mut percents = Vec::new();
        panel
            .upload_boot_animation(&boot_animation(), |p| percents.push(p))
            .unwrap();
        assert_eq!(percents, [50, 100]);

        // Per item after the mark: ("out", block) or ("get", id).
        let items: Vec<(&str, u8)> = log.snapshot()[mark..]
            .iter()
            .map(|s| match s {
                Sent::Output(b) => ("out", b[2]),
                Sent::GetFeature(id) => ("get", *id),
                Sent::SetFeature(_) => ("set", 0),
            })
            .collect();
        let mut runs: Vec<(&str, u8, usize)> = Vec::new();
        for (kind, n) in items {
            match runs.last_mut() {
                Some(run) if run.0 == kind && run.1 == n => run.2 += 1,
                _ => runs.push((kind, n, 1)),
            }
        }
        // Table, block 1, three polls (0, 0, 1), block 2, two polls (1, 2), keep-alive.
        assert_eq!(
            runs,
            [
                ("out", 0, 1),
                ("out", 1, 66),
                ("get", 0x0d, 3),
                ("out", 2, 15),
                ("get", 0x0d, 2),
                ("get", 0x0f, 1),
            ]
        );
        // The next live frame restarts with the first-frame marker.
        panel.send_frame(&frame()).unwrap();
        assert_eq!(*markers(&log).last().unwrap(), 0x00);
    }

    #[test]
    fn boot_upload_times_out_without_sending_the_next_block() {
        let (transport, log) = MockTransport::new();
        let transport = transport.with_reply_sequence(0x0d, &[PROGRESS_0]);
        let mut panel = Panel::start(transport).unwrap();
        let mark = log.snapshot().len();
        let timing = BootTiming {
            poll: Duration::from_millis(1),
            block_timeout: Duration::from_millis(30),
        };
        let err = panel
            .upload_boot_animation_timed(&boot_animation(), |_| {}, timing)
            .unwrap_err();
        assert_eq!(err, SessionError::BootUploadTimeout { block: 1 });
        assert!(err.to_string().contains("partial boot animation"));
        let sent = &log.snapshot()[mark..];
        assert!(
            !sent
                .iter()
                .any(|s| matches!(s, Sent::Output(b) if b[2] == 2))
        );
        assert!(!sent.contains(&Sent::GetFeature(0x0f)));
    }

    #[test]
    fn boot_upload_rejects_a_short_progress_reply() {
        let (transport, _log) = MockTransport::new();
        let transport = transport.with_reply(0x0d, &[0x0d]);
        let mut panel = Panel::start(transport).unwrap();
        assert_eq!(
            panel
                .upload_boot_animation(&boot_animation(), |_| {})
                .unwrap_err(),
            SessionError::BadReply { report_id: 0x0d }
        );
    }

    #[test]
    fn transport_failure_mid_frame_is_reported() {
        let (transport, _log) = MockTransport::new();
        // 8 start-up calls, one packet of the frame, then fail on the second.
        let transport = transport.failing_from(9);
        let mut panel = Panel::start(transport).unwrap();
        let big = Frame::from_jpeg(sample_jpeg(2500)).unwrap();
        assert!(matches!(
            panel.send_frame(&big),
            Err(SessionError::Transport(TransportError::Io {
                op: "mock",
                ..
            }))
        ));
    }

    #[test]
    fn brightness_echo_lasts_eight_frames() {
        let (mut panel, log) = started();
        panel
            .set_brightness(Brightness::try_from(40).unwrap())
            .unwrap();
        for _ in 0..10 {
            panel.send_frame(&frame()).unwrap();
        }
        let mut want = vec![0x00];
        want.extend([0x28; 8]);
        want.push(0xff);
        assert_eq!(markers(&log), want);

        for _ in 0..3 {
            panel.send_frame(&frame()).unwrap();
        }
        panel
            .set_brightness(Brightness::try_from(70).unwrap())
            .unwrap();
        for _ in 0..9 {
            panel.send_frame(&frame()).unwrap();
        }
        let all = markers(&log);
        let tail = &all[all.len() - 9..];
        assert_eq!(tail, [0x46, 0x46, 0x46, 0x46, 0x46, 0x46, 0x46, 0x46, 0xff]);
    }

    #[test]
    fn keep_alive_returns_status_bytes() {
        let (mut panel, _log) = started();
        assert_eq!(
            panel.keep_alive().unwrap(),
            StatusReply([0xe2, 0xff, 0xff, 0xff, 0xea, 0xff, 0xff, 0xff])
        );
    }

    #[test]
    fn bad_reply_id_is_rejected() {
        let (transport, _log) = MockTransport::new();
        let transport = transport.with_reply(0x0a, &[0x0b, 0, 0, 0, 0]);
        assert_eq!(
            Panel::start(transport).unwrap_err(),
            SessionError::BadReply { report_id: 0x0a }
        );
    }
}
