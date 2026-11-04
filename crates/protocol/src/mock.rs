//! A scripted transport for tests; never touches a device.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::{FeatureCommand, FeatureQuery, FeatureReply, OutputPacket, Transport, TransportError};

/// Replies captured from the panel.
pub mod fixtures {
    /// GET 0x0a: firmware string "21.0.5.4".
    pub const FIRMWARE: [u8; 13] = [
        0x0a, 0x0c, 0x8c, 0x17, 0x9d, b'2', b'1', b'.', b'0', b'.', b'5', b'.', b'4',
    ];
    /// GET 0x07: 480x128.
    pub const GEOMETRY: [u8; 13] = [0x07, 0, 0, 0, 0, 0, 0, 0xe0, 0x01, 0x80, 0x00, 0x04, 0x05];
    /// GET 0x0f: constant status.
    pub const STATUS: [u8; 9] = [0x0f, 0xe2, 0xff, 0xff, 0xff, 0xea, 0xff, 0xff, 0xff];
    /// GET 0x10.
    pub const REPORT10: [u8; 2] = [0x10, 0x05];
}

/// One item sent to the mock device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// A SET_FEATURE report (full 64 bytes).
    SetFeature(Vec<u8>),
    /// A GET_FEATURE of this report ID.
    GetFeature(u8),
    /// An output report (full 1024 bytes).
    Output(Vec<u8>),
}

/// Shared record of everything a [`MockTransport`] was asked to do.
#[derive(Debug, Clone, Default)]
pub struct MockLog(Arc<Mutex<Vec<Sent>>>);

impl MockLog {
    /// A copy of the log so far.
    pub fn snapshot(&self) -> Vec<Sent> {
        self.0.lock().expect("lock poisoned").clone()
    }

    fn push(&self, sent: Sent) {
        self.0.lock().expect("lock poisoned").push(sent);
    }
}

/// A [`Transport`] that logs everything and answers GETs from a table.
#[derive(Debug)]
pub struct MockTransport {
    log: MockLog,
    replies: HashMap<u8, Vec<u8>>,
    sequences: HashMap<u8, VecDeque<Vec<u8>>>,
    calls: usize,
    fail_from: Option<usize>,
}

impl MockTransport {
    /// A transport answering with the captured [`fixtures`], and its log.
    pub fn new() -> (Self, MockLog) {
        let log = MockLog::default();
        let replies = HashMap::from([
            (0x0a, fixtures::FIRMWARE.to_vec()),
            (0x07, fixtures::GEOMETRY.to_vec()),
            (0x0f, fixtures::STATUS.to_vec()),
            (0x10, fixtures::REPORT10.to_vec()),
        ]);
        let transport = Self {
            log: log.clone(),
            replies,
            sequences: HashMap::new(),
            calls: 0,
            fail_from: None,
        };
        (transport, log)
    }

    /// Overrides the reply to GET `id`.
    #[must_use]
    pub fn with_reply(mut self, id: u8, bytes: &[u8]) -> Self {
        self.replies.insert(id, bytes.to_vec());
        self
    }

    /// Answers successive GETs of `id` with `replies` in order; the last one repeats.
    #[must_use]
    pub fn with_reply_sequence(mut self, id: u8, replies: &[&[u8]]) -> Self {
        self.sequences
            .insert(id, replies.iter().map(|r| r.to_vec()).collect());
        self
    }

    /// Makes every call with index `call` or later fail (and log nothing).
    #[must_use]
    pub fn failing_from(mut self, call: usize) -> Self {
        self.fail_from = Some(call);
        self
    }

    fn begin(&mut self) -> Result<(), TransportError> {
        let index = self.calls;
        self.calls += 1;
        match self.fail_from {
            Some(n) if index >= n => Err(TransportError::Io {
                op: "mock",
                message: "injected failure".to_owned(),
            }),
            _ => Ok(()),
        }
    }
}

impl Transport for MockTransport {
    fn set_feature(&mut self, command: &FeatureCommand) -> Result<(), TransportError> {
        self.begin()?;
        self.log.push(Sent::SetFeature(command.as_bytes().to_vec()));
        Ok(())
    }

    fn get_feature(&mut self, query: FeatureQuery) -> Result<FeatureReply, TransportError> {
        self.begin()?;
        let id = query.report_id();
        self.log.push(Sent::GetFeature(id));
        if let Some(queue) = self.sequences.get_mut(&id) {
            let next = if queue.len() > 1 {
                queue.pop_front()
            } else {
                queue.front().cloned()
            };
            if let Some(bytes) = next {
                return Ok(FeatureReply::new(&bytes));
            }
        }
        self.replies
            .get(&id)
            .map(|bytes| FeatureReply::new(bytes))
            .ok_or(TransportError::Io {
                op: "mock",
                message: format!("no reply scripted for 0x{id:02x}"),
            })
    }

    fn write_output(&mut self, packet: &OutputPacket) -> Result<(), TransportError> {
        self.begin()?;
        self.log.push(Sent::Output(packet.as_bytes().to_vec()));
        Ok(())
    }
}
