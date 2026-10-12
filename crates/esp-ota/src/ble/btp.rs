//! The BTP (Matter Core spec 4.19) central role as a pure, synchronous state
//! machine: no Bluetooth, no async, time passed in explicitly.
//!
//! The wire format mirrors rs-matter 0.2's peripheral (`btp/session.rs`):
//! header = flags, [management opcode], [ack number], sequence number,
//! [little-endian SDU length on the beginning segment]. The peripheral's
//! handshake response implicitly carries its sequence number 0, which the
//! central acknowledges; the central's first data segment is sequence 0.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Result};

const FLAG_HANDSHAKE: u8 = 0x40;
const FLAG_MANAGEMENT: u8 = 0x20;
const FLAG_ACK: u8 = 0x08;
const FLAG_END: u8 = 0x04;
const FLAG_CONTINUE: u8 = 0x02;
const FLAG_BEGIN: u8 = 0x01;
const HANDSHAKE_OPCODE: u8 = 0x6c;
const HANDSHAKE_FLAGS: u8 = FLAG_HANDSHAKE | FLAG_MANAGEMENT | FLAG_END | FLAG_BEGIN;

/// The only BTP version Matter defines.
pub const BTP_VERSION: u8 = 4;
/// ATT MTU announced in the handshake. btleplug cannot report the negotiated
/// MTU on macOS, and rs-matter's peripheral falls back to the minimum unless
/// the announced and negotiated MTUs agree, so 23 (20-byte segments) is the
/// one value every peer accepts.
pub const REQUESTED_ATT_MTU: u16 = 23;
/// Receive window announced in the handshake (segments in flight per direction).
pub const REQUESTED_WINDOW: u8 = 6;
/// Largest SDU (Matter message) accepted for sending, as in rs-matter.
pub const MAX_SDU: usize = 1280 - 40 - 8;
/// SDUs that may wait in the send queue before `can_queue` reports false.
const MAX_QUEUED_SDUS: usize = 2;
/// Delay before a standalone ACK, giving outgoing data a chance to carry it.
/// The spec allows up to 15 s; acking sooner keeps the peer's window open.
pub const ACK_SEND_DELAY: Duration = Duration::from_millis(250);
/// How long the peer may leave our segments unacknowledged before the link
/// is considered dead. The spec's ACK timeout is 15 s; rs-matter checks it
/// once a second, so allow twice that.
pub const ACK_RECEIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// The 9-byte handshake request written to C1.
pub fn handshake_request() -> [u8; 9] {
    let mtu = REQUESTED_ATT_MTU.to_le_bytes();
    [
        HANDSHAKE_FLAGS,
        HANDSHAKE_OPCODE,
        BTP_VERSION, // supported versions, one per nibble, preferred first
        0,
        0,
        0,
        mtu[0],
        mtu[1],
        REQUESTED_WINDOW,
    ]
}

/// A BTP session in the central (GATT client) role.
#[derive(Debug)]
pub struct BtpCentral {
    segment_size: usize,
    window: u8,

    /// Last sequence number we sent (255 before the first, so the first is 0).
    tx_last_seq: u8,
    /// Last of our sequence numbers the peer acknowledged.
    tx_acked_seq: u8,
    /// When the oldest still-unacknowledged segment was sent (or the last
    /// partial ACK arrived), for the ACK-received timeout.
    tx_unacked_since: Option<Instant>,
    tx_queue: VecDeque<Vec<u8>>,
    tx_offset: usize,

    /// Last sequence number received from the peer.
    rx_last_seq: u8,
    /// Last peer sequence number we acknowledged.
    rx_acked_seq: u8,
    /// When the oldest not-yet-acknowledged peer segment arrived.
    rx_unacked_since: Option<Instant>,
    rx_partial: Option<(usize, Vec<u8>)>,
    rx_ready: VecDeque<Vec<u8>>,
}

impl BtpCentral {
    /// Completes the handshake from the peer's response indication.
    pub fn from_handshake_response(data: &[u8], now: Instant) -> Result<Self> {
        ensure!(
            data.len() >= 6 && data[0] == HANDSHAKE_FLAGS && data[1] == HANDSHAKE_OPCODE,
            "not a BTP handshake response: {data:02x?}"
        );
        let version = data[2];
        let segment_size = u16::from_le_bytes([data[3], data[4]]) as usize;
        let window = data[5];
        ensure!(
            version == BTP_VERSION,
            "peer selected unsupported BTP version {version}"
        );
        // The smallest useful segment holds a 5-byte beginning header plus data.
        ensure!(
            segment_size >= 6,
            "peer selected segment size {segment_size}"
        );
        ensure!(window >= 2, "peer selected window size {window}");
        if segment_size > usize::from(REQUESTED_ATT_MTU) - 3 {
            log::warn!("BTP peer selected {segment_size}-byte segments; more than requested");
        }
        Ok(Self {
            segment_size,
            window,
            tx_last_seq: 255,
            tx_acked_seq: 255,
            tx_unacked_since: None,
            tx_queue: VecDeque::new(),
            tx_offset: 0,
            // The handshake response is the peer's sequence number 0.
            rx_last_seq: 0,
            rx_acked_seq: 255,
            rx_unacked_since: Some(now),
            rx_partial: None,
            rx_ready: VecDeque::new(),
        })
    }

    /// Negotiated payload size of one BTP segment, header included.
    pub fn segment_size(&self) -> usize {
        self.segment_size
    }

    /// Negotiated window size.
    pub fn window(&self) -> u8 {
        self.window
    }

    /// Whether `queue_sdu` will accept another message now.
    pub fn can_queue(&self) -> bool {
        self.tx_queue.len() < MAX_QUEUED_SDUS
    }

    /// Whether every queued message has been fully segmented and sent.
    pub fn tx_idle(&self) -> bool {
        self.tx_queue.is_empty()
    }

    /// Queues a Matter message for segmentation.
    pub fn queue_sdu(&mut self, sdu: &[u8]) -> Result<()> {
        ensure!(
            !sdu.is_empty() && sdu.len() <= MAX_SDU,
            "BTP SDU of {} bytes",
            sdu.len()
        );
        ensure!(self.can_queue(), "BTP send queue full");
        self.tx_queue.push_back(sdu.to_vec());
        Ok(())
    }

    /// Next reassembled message from the peer.
    pub fn poll_sdu(&mut self) -> Option<Vec<u8>> {
        self.rx_ready.pop_front()
    }

    /// Whether a reassembled message is waiting.
    pub fn sdu_available(&self) -> bool {
        !self.rx_ready.is_empty()
    }

    fn tx_in_flight(&self) -> u8 {
        self.tx_last_seq.wrapping_sub(self.tx_acked_seq)
    }

    fn rx_unacked(&self) -> u8 {
        self.rx_last_seq.wrapping_sub(self.rx_acked_seq)
    }

    fn pending_ack(&self) -> Option<u8> {
        (self.rx_unacked() > 0).then_some(self.rx_last_seq)
    }

    /// The peer's receive window has room. As in the spec (and rs-matter), the
    /// last free slot is reserved for a segment that carries an ACK, so both
    /// sides can never stall with full windows.
    fn can_send(&self) -> bool {
        let free = self.window.saturating_sub(self.tx_in_flight());
        free >= 2 || (free == 1 && self.pending_ack().is_some())
    }

    fn ack_due(&self, now: Instant) -> bool {
        self.pending_ack().is_some()
            && (self.rx_unacked() >= (self.window / 2).max(1)
                || self
                    .rx_unacked_since
                    .is_some_and(|since| now >= since + ACK_SEND_DELAY))
    }

    /// Next segment to write to C1, if any: a data segment when a message is
    /// queued and the window allows, else a standalone ACK when one is due.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Vec<u8>> {
        if !self.can_send() {
            return None;
        }
        let ack = self.pending_ack();
        let mut out;
        if let Some(sdu) = self.tx_queue.front() {
            let begin = self.tx_offset == 0;
            let mut flags = if begin { FLAG_BEGIN } else { FLAG_CONTINUE };
            if ack.is_some() {
                flags |= FLAG_ACK;
            }
            let header_len = 2 + usize::from(ack.is_some()) + if begin { 2 } else { 0 };
            let remaining = &sdu[self.tx_offset..];
            let chunk = remaining.len().min(self.segment_size - header_len);
            if chunk == remaining.len() {
                flags |= FLAG_END;
            }
            out = Vec::with_capacity(header_len + chunk);
            out.push(flags);
            out.extend(ack);
            out.push(self.tx_last_seq.wrapping_add(1));
            if begin {
                out.extend_from_slice(&(sdu.len() as u16).to_le_bytes());
            }
            out.extend_from_slice(&remaining[..chunk]);
            self.tx_offset += chunk;
            if self.tx_offset == sdu.len() {
                self.tx_queue.pop_front();
                self.tx_offset = 0;
            }
        } else if self.ack_due(now) {
            out = vec![FLAG_ACK, self.rx_last_seq, self.tx_last_seq.wrapping_add(1)];
        } else {
            return None;
        }
        self.tx_last_seq = self.tx_last_seq.wrapping_add(1);
        if self.tx_unacked_since.is_none() {
            self.tx_unacked_since = Some(now);
        }
        if ack.is_some() {
            self.rx_acked_seq = self.rx_last_seq;
            self.rx_unacked_since = None;
        }
        Some(out)
    }

    /// Processes one C2 indication.
    pub fn on_indication(&mut self, data: &[u8], now: Instant) -> Result<()> {
        let mut bytes = data.iter().copied();
        let mut next = || bytes.next();
        let Some(flags) = next() else {
            bail!("empty BTP segment");
        };
        ensure!(
            flags & (FLAG_HANDSHAKE | FLAG_MANAGEMENT) == 0,
            "unexpected BTP control segment {data:02x?}"
        );
        let ack = if flags & FLAG_ACK != 0 {
            Some(next().ok_or_else(|| anyhow::anyhow!("truncated BTP header"))?)
        } else {
            None
        };
        let seq = next().ok_or_else(|| anyhow::anyhow!("truncated BTP header"))?;
        let msg_len = if flags & FLAG_BEGIN != 0 {
            let lo = next().ok_or_else(|| anyhow::anyhow!("truncated BTP header"))?;
            let hi = next().ok_or_else(|| anyhow::anyhow!("truncated BTP header"))?;
            Some(usize::from(u16::from_le_bytes([lo, hi])))
        } else {
            None
        };
        let header_len = 2 + usize::from(ack.is_some()) + if msg_len.is_some() { 2 } else { 0 };
        let payload = &data[header_len..];

        let expected = self.rx_last_seq.wrapping_add(1);
        ensure!(seq == expected, "BTP sequence {seq}, expected {expected}");
        ensure!(
            self.rx_unacked() < self.window,
            "BTP peer overran our receive window"
        );

        if let Some(ack) = ack {
            let newly_acked = ack.wrapping_sub(self.tx_acked_seq);
            ensure!(
                newly_acked <= self.tx_in_flight(),
                "BTP ACK {ack} for a segment never sent (last sent {})",
                self.tx_last_seq
            );
            self.tx_acked_seq = ack;
            self.tx_unacked_since = (self.tx_in_flight() > 0).then_some(now);
        }

        self.rx_last_seq = seq;
        if self.rx_unacked_since.is_none() {
            self.rx_unacked_since = Some(now);
        }

        let segment_flags = flags & (FLAG_BEGIN | FLAG_CONTINUE | FLAG_END);
        if segment_flags == 0 {
            ensure!(
                ack.is_some() && payload.is_empty(),
                "malformed BTP segment {data:02x?}"
            );
            return Ok(());
        }
        ensure!(
            flags & FLAG_END != 0 || payload.len() + header_len == self.segment_size,
            "non-final BTP segment shorter than the segment size"
        );
        match msg_len {
            Some(len) => {
                ensure!(
                    flags & FLAG_CONTINUE == 0,
                    "BTP segment both begins and continues"
                );
                ensure!(
                    self.rx_partial.is_none(),
                    "BTP message began inside another"
                );
                ensure!(len <= 2 * 1583, "BTP message of {len} bytes");
                self.rx_partial = Some((len, Vec::with_capacity(len)));
            }
            None => ensure!(
                self.rx_partial.is_some(),
                "BTP continuation without a beginning"
            ),
        }
        let Some((len, buf)) = self.rx_partial.as_mut() else {
            unreachable!()
        };
        ensure!(
            buf.len() + payload.len() <= *len,
            "BTP segment exceeds message length"
        );
        buf.extend_from_slice(payload);
        if flags & FLAG_END != 0 {
            ensure!(buf.len() == *len, "BTP message ended early");
            let (_, sdu) = self.rx_partial.take().unwrap_or_default();
            if !sdu.is_empty() {
                self.rx_ready.push_back(sdu);
            }
        }
        Ok(())
    }

    /// Fails when the peer has not acknowledged our segments in time.
    pub fn check_timeouts(&self, now: Instant) -> Result<()> {
        if let Some(since) = self.tx_unacked_since {
            ensure!(
                now < since + ACK_RECEIVE_TIMEOUT,
                "BTP peer stopped acknowledging ({}s)",
                ACK_RECEIVE_TIMEOUT.as_secs()
            );
        }
        Ok(())
    }

    /// When `poll_transmit` or `check_timeouts` next needs calling absent
    /// new input. `None` means only new input (an indication or a queued
    /// message) can change anything.
    pub fn next_deadline(&self) -> Option<Instant> {
        let ack = if self.can_send() && self.pending_ack().is_some() {
            self.rx_unacked_since.map(|since| since + ACK_SEND_DELAY)
        } else {
            None
        };
        let timeout = self
            .tx_unacked_since
            .map(|since| since + ACK_RECEIVE_TIMEOUT);
        match (ack, timeout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}
