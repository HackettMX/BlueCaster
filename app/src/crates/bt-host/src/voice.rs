//! HFP audio (headset microphone, developer mode): the audio connection on
//! the HFP service level connection, and SCO data both ways.
//!
//! [`crate::BtHost::voice_start`] hands a [`VoiceTx`] and a [`VoiceRx`] to
//! the BTstack thread and asks the HFP audio gateway for the audio
//! connection (codec negotiation first). Once it is up, received SCO
//! payloads go to the `VoiceRx`, and on every HCI_EVENT_SCO_CAN_SEND_NOW the
//! `VoiceTx` fills one packet of the length BTstack wants for the
//! connection. The codec is the caller's business: payloads are what goes
//! over HCI (CVSD: 16-bit linear PCM, mSBC / LC3-SWB: H2 framed frames).
//!
//! SCO must be able to flow before BTstack is asked for a packet length: on
//! an H2 transport BTstack asserts (and aborts the process) unless the
//! controller's SCO data packet length holds 3 ms of audio, and the USB
//! transport's isochronous pipes may not run. A link that cannot carry SCO
//! is given up ([`VoiceEvent::Failed`] with [`NO_SCO_PATH`]), and so is one
//! whose sends keep failing.

use std::time::{Duration, Instant};

use btstack_sys::Stack;
use btstack_sys::consts::{
    HCI_EVENT_PACKET, HCI_EVENT_SCO_CAN_SEND_NOW, HCI_SCO_DATA_PACKET, HFP_CODEC_CVSD, HFP_CODEC_LC3_SWB,
    HFP_CODEC_MSBC,
};
use btstack_sys::events::BufferSize;

use crate::events::{event_type, le16, u8_at};

/// Headset mode of a stream (the profile's `headset_mic`, developer mode)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum HeadsetMic {
    #[default]
    Off,
    /// The voice link for as long as the stream runs
    On,
    /// Classic devices: the voice link only while an application records
    /// from the microphone output's capture side, full-quality A2DP
    /// otherwise. LE Audio treats it as [`HeadsetMic::On`]: its
    /// conversational stream keeps the media quality anyway.
    Auto,
}

impl HeadsetMic {
    /// "off" | "on" | "auto"; anything else is off
    pub fn parse(s: &str) -> HeadsetMic {
        match s {
            "on" => HeadsetMic::On,
            "auto" => HeadsetMic::Auto,
            _ => HeadsetMic::Off,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HeadsetMic::Off => "off",
            HeadsetMic::On => "on",
            HeadsetMic::Auto => "auto",
        }
    }

    /// Not off: the stack offers voice, the profile's output is needed
    pub fn is_on(self) -> bool {
        self != HeadsetMic::Off
    }
}

/// The status of [`VoiceEvent::Failed`] and [`HostError::Refused`] when the
/// transport or the controller cannot carry SCO for the link
pub const NO_SCO_PATH: u8 = 0xFF;

/// Failed SCO sends in a row after which the audio connection is given up
const SEND_FAILURES: u32 = 10;
/// How long after the service level connection the audio connection waits,
/// at least: the headphones send their own commands (NREC, CLIP, XAPL, VGS,
/// battery) right after it, and BTstack's AG answers ERROR to a command
/// that arrives while its own +BCS waits for OK (the WH-1000XM4's AT+BCS
/// then got that ERROR and no SCO followed)
const SLC_SETTLE: Duration = Duration::from_millis(1000);
/// ... and since the last command the headphones sent
const HF_QUIET: Duration = Duration::from_millis(500);

/// What happens to the HFP audio connection ([`crate::HostEvent::Voice`])
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    /// The audio connection is up: the negotiated codec (HFP codec id: 1
    /// CVSD, 2 mSBC, 3 LC3-SWB), the SCO handle, and the packet lengths
    /// of the (e)SCO link over the air (0 for SCO)
    Established { codec: u8, sco_handle: u16, rx_len: u16, tx_len: u16 },
    /// The audio connection went down (asked for, or by the headphones)
    Released,
    /// The audio connection could not be opened (HCI / BTstack status), or
    /// SCO cannot flow on it ([`NO_SCO_PATH`], or the status of the sends
    /// that kept failing): closed already when it was asked for, otherwise
    /// the caller closes it
    Failed { status: u8 },
}

/// Payload bytes to send, on the BTstack thread
pub trait VoiceTx: Send {
    /// Fills `out` completely (the SCO payload of one packet)
    fn fill(&mut self, out: &mut [u8]);
}

/// Received payloads, on the BTstack thread
pub trait VoiceRx: Send {
    /// One SCO packet's payload; `status` is the HCI packet status flag (0
    /// correctly received, 1 possibly invalid, 2 no data, 3 partially lost)
    fn packet(&mut self, payload: &[u8], status: u8);
}

/// Why a voice request failed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    /// The stack was set up without voice ([`crate::HostConfig::voice`])
    NotEnabled,
    /// No HFP service level connection (yet)
    NotConnected,
    /// An audio connection is already open or being opened
    Busy,
    /// The service level connection is too fresh, or the headphones are
    /// still sending commands: ask again shortly
    Settling,
    /// BTstack refused the request (its status)
    Refused(u8),
    /// The BTstack thread did not answer
    NoAnswer,
}

/// "CVSD", "mSBC", "LC3-SWB"
pub fn codec_name(codec: u8) -> &'static str {
    match codec {
        HFP_CODEC_CVSD => "CVSD",
        HFP_CODEC_MSBC => "mSBC",
        HFP_CODEC_LC3_SWB => "LC3-SWB",
        _ => "unknown",
    }
}

/// The SCO payload of 3 ms BTstack's H2 path sizes packets to: 24 samples
/// of 16-bit CVSD (48 bytes), 24 bytes of transparent data (mSBC,
/// LC3-SWB); `None` (the codec not negotiated yet): the smallest
pub fn h2_sco_payload(codec: Option<u8>) -> u16 {
    match codec {
        Some(HFP_CODEC_CVSD) => 48,
        _ => 24,
    }
}

/// Whether BTstack can size SCO packets for `codec`: on an H2 transport it
/// asserts that the controller's SCO data packet length (Read Buffer Size;
/// None: not seen) holds [`h2_sco_payload`]. Other transports split the
/// payload to the controller's length.
pub fn sco_fits(h2: bool, codec: Option<u8>, controller_sco_len: Option<u8>) -> bool {
    !h2 || controller_sco_len.is_some_and(|len| u16::from(len) >= h2_sco_payload(codec))
}

/// A SCO data packet: (handle, packet status flag, payload)
pub fn parse_sco(packet: &[u8]) -> Option<(u16, u8, &[u8])> {
    let head = le16(packet, 0)?;
    let len = u8_at(packet, 2)? as usize;
    let payload = packet.get(3..3 + len)?;
    Some((head & 0x0FFF, ((head >> 12) & 0x03) as u8, payload))
}

/// The volume (AVRCP absolute volume, 0..127) as an HFP speaker gain
/// (0..15): headphones play the voice link at their HFP gain, not at the
/// A2DP volume
pub fn speaker_gain(volume: u8) -> u8 {
    ((u32::from(volume.min(127)) * 15 + 63) / 127) as u8
}

/// An HFP speaker gain (0..15) as a volume (0..127)
pub fn gain_volume(gain: u8) -> u8 {
    ((u32::from(gain.min(15)) * 127 + 7) / 15) as u8
}

/// The voice state on the BTstack thread
#[derive(Default)]
pub(crate) struct Voice {
    tx: Option<Box<dyn VoiceTx>>,
    rx: Option<Box<dyn VoiceRx>>,
    /// The ACL of the audio connection asked for (between voice_start and
    /// Established / Failed)
    opening: Option<u16>,
    /// The open audio connection: SCO handle and its ACL
    sco: Option<(u16, u16)>,
    /// SCO can flow on the open audio connection (and has not given up)
    usable: bool,
    /// The controller's SCO data packet length (Read Buffer Size)
    controller_sco_len: Option<u8>,
    buf: Vec<u8>,
    /// When the service level connection came up
    slc_since: Option<Instant>,
    /// The last HFP event from the headphones (a command of theirs)
    hf_activity: Option<Instant>,
    /// Packets sent / failed sends, for the log at the end
    sent: u64,
    send_errors: u64,
    /// Failed sends since the last one that went
    failures_in_row: u32,
    received: u64,
}

impl Voice {
    /// HFP_SUBEVENT_SERVICE_LEVEL_CONNECTION_ESTABLISHED
    pub fn on_slc_established(&mut self) {
        let now = Instant::now();
        self.slc_since = Some(now);
        self.hf_activity = Some(now);
    }

    /// The ACL of the open audio connection (ours or the headphones')
    pub fn audio_acl(&self) -> Option<u16> {
        self.sco.map(|(_, acl)| acl)
    }

    /// Any HFP subevent: the headphones are talking
    pub fn on_hf_activity(&mut self) {
        self.hf_activity = Some(Instant::now());
    }

    /// The audio connection may be asked for only once the service level
    /// connection is [`SLC_SETTLE`] old and the headphones quiet for
    /// [`HF_QUIET`]
    fn settling(&self) -> bool {
        let now = Instant::now();
        self.slc_since.is_some_and(|t| now < t + SLC_SETTLE) || self.hf_activity.is_some_and(|t| now < t + HF_QUIET)
    }

    /// The answer to Read Buffer Size (HCI init)
    pub fn on_buffer_size(&mut self, b: &BufferSize) {
        if b.status != 0 {
            tracing::info!("HCI Read Buffer Size failed (0x{:02x}): no SCO length known", b.status);
            return;
        }
        tracing::debug!(
            "controller buffers: ACL {} x {} bytes, SCO {} x {} bytes",
            b.acl_packets,
            b.acl_packet_length,
            b.sco_packets,
            b.sco_packet_length
        );
        self.controller_sco_len = Some(b.sco_packet_length);
    }

    /// [`crate::BtHost::voice_start`] on the BTstack thread
    pub fn start(
        &mut self,
        stack: &Stack,
        enabled: bool,
        acl: Option<u16>,
        tx: Box<dyn VoiceTx>,
        rx: Box<dyn VoiceRx>,
    ) -> Result<(), HostError> {
        self.check(enabled, acl, stack.transport_is_h2())?;
        let Some(acl) = acl else { return Err(HostError::NotConnected) };
        let status = stack.hfp_ag_audio_connect(acl);
        if status != 0 {
            tracing::info!("HFP: audio connection refused by BTstack (0x{status:02x})");
            return Err(HostError::Refused(status));
        }
        tracing::info!("HFP: opening the audio connection on ACL 0x{acl:04x}");
        self.tx = Some(tx);
        self.rx = Some(rx);
        self.opening = Some(acl);
        self.sent = 0;
        self.send_errors = 0;
        self.failures_in_row = 0;
        self.received = 0;
        Ok(())
    }

    /// What [`Voice::start`] would answer, opening nothing (`h2`: the
    /// transport is an H2 one). Refused when the controller's SCO buffers
    /// cannot carry any codec over it.
    pub fn check(&self, enabled: bool, acl: Option<u16>, h2: bool) -> Result<(), HostError> {
        if !enabled {
            return Err(HostError::NotEnabled);
        }
        if !sco_fits(h2, None, self.controller_sco_len) {
            tracing::info!(
                "HFP: the controller's SCO packet length ({:?}) is too short for SCO over USB",
                self.controller_sco_len
            );
            return Err(HostError::Refused(NO_SCO_PATH));
        }
        if acl.is_none() {
            return Err(HostError::NotConnected);
        }
        if self.opening.is_some() || self.sco.is_some() {
            return Err(HostError::Busy);
        }
        if self.settling() {
            return Err(HostError::Settling);
        }
        Ok(())
    }

    /// [`crate::BtHost::voice_attach`] on the BTstack thread: an audio
    /// connection the headphones opened gets its `tx` / `rx`
    pub fn attach(&mut self, stack: &Stack, tx: Box<dyn VoiceTx>, rx: Box<dyn VoiceRx>) -> Result<(), HostError> {
        let Some((sco_handle, _)) = self.sco else { return Err(HostError::NotConnected) };
        if self.tx.is_some() {
            return Err(HostError::Busy);
        }
        if !self.usable {
            tracing::info!("HFP: the audio connection the headphones opened cannot carry SCO here");
            return Err(HostError::Refused(NO_SCO_PATH));
        }
        tracing::info!("HFP: taking over the audio connection the headphones opened");
        self.failures_in_row = 0;
        self.tx = Some(tx);
        self.rx = Some(rx);
        stack.sco_request_can_send_now(sco_handle);
        Ok(())
    }

    /// [`crate::BtHost::voice_stop`] on the BTstack thread. When BTstack
    /// refuses the release no RELEASED follows: the audio connection is
    /// taken as closed now and the [`VoiceEvent::Released`] returned, for
    /// the caller to send.
    pub fn stop(&mut self, stack: &Stack) -> Option<crate::HostEvent> {
        self.tx = None;
        self.rx = None;
        let acl = self.sco.map(|(_, acl)| acl).or(self.opening)?;
        let status = stack.hfp_ag_audio_release(acl);
        tracing::info!("HFP: closing the audio connection (0x{status:02x})");
        if status != 0 {
            tracing::info!("HFP: BTstack refused the release, the audio connection is taken as closed");
            return self.on_released();
        }
        None
    }

    /// HFP_SUBEVENT_AUDIO_CONNECTION_ESTABLISHED (the whole HFP meta event)
    pub fn on_established(&mut self, stack: &Stack, p: &[u8]) -> Option<crate::HostEvent> {
        let status = u8_at(p, 5)?;
        self.opening = None;
        if status != 0 {
            tracing::info!("HFP: audio connection failed (0x{status:02x})");
            self.tx = None;
            self.rx = None;
            return Some(crate::HostEvent::Voice(VoiceEvent::Failed { status }));
        }
        let (acl, sco_handle) = (le16(p, 3)?, le16(p, 6)?);
        let codec = u8_at(p, 14)?;
        let (packet_types, rx_len, tx_len) = (le16(p, 15)?, le16(p, 17)?, le16(p, 19)?);
        let ours = self.tx.is_some();
        let h2 = stack.transport_is_h2();
        // Before BTstack sizes a packet: its H2 path asserts on a short
        // controller buffer
        let fits = sco_fits(h2, Some(codec), self.controller_sco_len);
        let ready = fits && stack.sco_ready();
        if !ready {
            tracing::warn!(
                "HFP: audio connection up (codec {}, SCO handle 0x{sco_handle:04x}), but SCO cannot flow: {}",
                codec_name(codec),
                if fits {
                    "the adapter's SCO interface does not run".to_string()
                } else {
                    format!(
                        "the controller's SCO packet length {:?} is below the {} bytes of SCO over USB",
                        self.controller_sco_len,
                        h2_sco_payload(Some(codec))
                    )
                }
            );
            if ours {
                self.tx = None;
                self.rx = None;
                let status = stack.hfp_ag_audio_release(acl);
                tracing::info!("HFP: closing the audio connection (0x{status:02x})");
                return Some(crate::HostEvent::Voice(VoiceEvent::Failed { status: NO_SCO_PATH }));
            }
            // The headphones': left to the caller, nothing is sent on it
            self.sco = Some((sco_handle, acl));
            self.usable = false;
            return Some(crate::HostEvent::Voice(VoiceEvent::Established { codec, sco_handle, rx_len, tx_len }));
        }
        let packet_length = stack.sco_packet_length(sco_handle);
        tracing::info!(
            "HFP: audio connection established, codec {} ({codec}), SCO handle 0x{sco_handle:04x}, packet types 0x{packet_types:04x}, air rx {rx_len} / tx {tx_len} bytes, HCI packet {packet_length} bytes",
            codec_name(codec)
        );
        self.sco = Some((sco_handle, acl));
        self.usable = true;
        self.failures_in_row = 0;
        if ours {
            stack.sco_request_can_send_now(sco_handle);
        } else {
            tracing::info!("HFP: audio connection opened by the headphones, nothing to send");
        }
        Some(crate::HostEvent::Voice(VoiceEvent::Established { codec, sco_handle, rx_len, tx_len }))
    }

    /// HCI Disconnection Complete: the SCO link of the open audio
    /// connection gone counts as its release (an event once)
    pub fn on_disconnected(&mut self, handle: u16) -> Option<crate::HostEvent> {
        if self.sco.is_none_or(|(sco_handle, _)| sco_handle != handle) {
            return None;
        }
        tracing::info!("HFP: the SCO link 0x{handle:04x} is gone");
        self.on_released()
    }

    /// HFP_SUBEVENT_AUDIO_CONNECTION_RELEASED, or the service level
    /// connection went down: an event if an audio connection was open or
    /// being opened
    pub fn on_released(&mut self) -> Option<crate::HostEvent> {
        let was = self.sco.take().is_some() || self.opening.take().is_some();
        self.slc_since = None;
        self.usable = false;
        self.tx = None;
        self.rx = None;
        if !was {
            return None;
        }
        tracing::info!(
            "HFP: audio connection released ({} SCO packets sent, {} send errors, {} received)",
            self.sent,
            self.send_errors,
            self.received
        );
        Some(crate::HostEvent::Voice(VoiceEvent::Released))
    }

    /// A packet of [`btstack_sys::Source::Sco`]; an event when the sends
    /// keep failing
    pub fn on_sco(&mut self, stack: &Stack, packet_type: u8, p: &[u8]) -> Option<crate::HostEvent> {
        let (sco_handle, _) = self.sco?;
        match packet_type {
            HCI_SCO_DATA_PACKET => {
                let (handle, status, payload) = parse_sco(p)?;
                if handle != sco_handle {
                    return None;
                }
                self.received += 1;
                if let Some(rx) = self.rx.as_mut() {
                    rx.packet(payload, status);
                }
                None
            }
            HCI_EVENT_PACKET if event_type(p) == Some(HCI_EVENT_SCO_CAN_SEND_NOW) => self.send(stack, sco_handle),
            _ => None,
        }
    }

    /// One packet from the VoiceTx, then the next can-send-now request.
    /// [`SEND_FAILURES`] failed sends in a row give the link up (Failed, no
    /// more requests: the caller closes it).
    fn send(&mut self, stack: &Stack, sco_handle: u16) -> Option<crate::HostEvent> {
        if !self.usable {
            return None;
        }
        let tx = self.tx.as_mut()?;
        let length = stack.sco_packet_length(sco_handle) as usize;
        let status = if length <= 3 {
            NO_SCO_PATH
        } else {
            self.buf.resize(length - 3, 0);
            tx.fill(&mut self.buf);
            stack.sco_send(sco_handle, &self.buf)
        };
        if status == 0 {
            self.sent += 1;
            self.failures_in_row = 0;
        } else {
            self.send_errors += 1;
            self.failures_in_row += 1;
            if self.send_errors <= 3 {
                tracing::debug!("SCO send (packet length {length}): 0x{status:02x}");
            }
            if self.failures_in_row >= SEND_FAILURES {
                tracing::warn!(
                    "HFP: {} SCO sends failed in a row (0x{status:02x}), giving the audio connection up",
                    self.failures_in_row
                );
                self.usable = false;
                return Some(crate::HostEvent::Voice(VoiceEvent::Failed { status }));
            }
        }
        stack.sco_request_can_send_now(sco_handle);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sco_packet_fields() {
        // handle 0x0123, status 2 (no data), 3 payload bytes
        let p = [0x23, 0x21, 3, 9, 8, 7, 0xEE];
        assert_eq!(parse_sco(&p), Some((0x0123, 2, &[9u8, 8, 7][..])));
        assert_eq!(parse_sco(&[0x23, 0x01, 4, 1, 2]), None);
        assert_eq!(parse_sco(&[0x23]), None);
    }

    #[test]
    fn volume_and_speaker_gain_map_both_ways() {
        assert_eq!(speaker_gain(0), 0);
        assert_eq!(speaker_gain(127), 15);
        assert_eq!(speaker_gain(64), 8);
        assert_eq!(speaker_gain(200), 15);
        assert_eq!(gain_volume(0), 0);
        assert_eq!(gain_volume(15), 127);
        // A gain the headphones report reads back as the same gain, so
        // their change is not sent back to them altered
        for g in 0..=15 {
            assert_eq!(speaker_gain(gain_volume(g)), g);
        }
    }

    #[test]
    fn sco_over_usb_needs_three_ms_in_the_controller_buffer() {
        // H2: 48 bytes for 16-bit CVSD, 24 transparent; before the codec,
        // what any codec needs
        assert_eq!(h2_sco_payload(Some(HFP_CODEC_CVSD)), 48);
        assert_eq!(h2_sco_payload(Some(HFP_CODEC_MSBC)), 24);
        assert_eq!(h2_sco_payload(Some(HFP_CODEC_LC3_SWB)), 24);
        assert_eq!(h2_sco_payload(None), 24);
        assert!(sco_fits(true, Some(HFP_CODEC_CVSD), Some(64)));
        assert!(sco_fits(true, Some(HFP_CODEC_CVSD), Some(48)));
        assert!(!sco_fits(true, Some(HFP_CODEC_CVSD), Some(47)));
        assert!(sco_fits(true, Some(HFP_CODEC_MSBC), Some(30)));
        assert!(!sco_fits(true, Some(HFP_CODEC_MSBC), Some(0)));
        // Read Buffer Size not seen: BTstack's length stays 0
        assert!(!sco_fits(true, None, None));
        // TCP (the emulator: 60 bytes) and other non-H2 transports split
        assert!(sco_fits(false, Some(HFP_CODEC_CVSD), Some(60)));
        assert!(sco_fits(false, Some(HFP_CODEC_CVSD), Some(0)));
        assert!(sco_fits(false, None, None));
    }

    fn buffer_size(sco: u8) -> BufferSize {
        BufferSize { status: 0, acl_packet_length: 1021, sco_packet_length: sco, acl_packets: 8, sco_packets: 8 }
    }

    #[test]
    fn a_short_sco_buffer_refuses_voice_over_usb_only() {
        let mut v = Voice::default();
        // Not seen yet: refused over USB, fine over TCP
        assert_eq!(v.check(true, Some(0x40), true), Err(HostError::Refused(NO_SCO_PATH)));
        assert_eq!(v.check(true, Some(0x40), false), Ok(()));
        v.on_buffer_size(&buffer_size(0));
        assert_eq!(v.check(true, Some(0x40), true), Err(HostError::Refused(NO_SCO_PATH)));
        v.on_buffer_size(&buffer_size(60));
        assert_eq!(v.check(true, Some(0x40), true), Ok(()));
        // A failed answer keeps what was known
        v.on_buffer_size(&BufferSize { status: 1, ..buffer_size(0) });
        assert_eq!(v.check(true, Some(0x40), true), Ok(()));
        // Not enabled and no service level connection come first / after
        assert_eq!(v.check(false, Some(0x40), true), Err(HostError::NotEnabled));
        assert_eq!(v.check(true, None, true), Err(HostError::NotConnected));
    }

    #[test]
    fn audio_connection_waits_for_the_headphones_to_settle() {
        let mut v = Voice::default();
        v.on_buffer_size(&buffer_size(60));
        v.on_slc_established();
        assert_eq!(v.check(true, Some(0x40), true), Err(HostError::Settling));
        v.slc_since = Some(Instant::now() - SLC_SETTLE);
        assert_eq!(v.check(true, Some(0x40), true), Err(HostError::Settling));
        v.hf_activity = Some(Instant::now() - HF_QUIET);
        assert_eq!(v.check(true, Some(0x40), true), Ok(()));
        v.on_hf_activity();
        assert_eq!(v.check(true, Some(0x40), true), Err(HostError::Settling));
        assert!(v.on_released().is_none());
        assert_eq!(v.slc_since, None);
    }

    #[test]
    fn the_sco_link_going_down_releases_once() {
        let mut v = Voice { sco: Some((0x0101, 0x0040)), usable: true, ..Voice::default() };
        assert_eq!(v.check(true, Some(0x40), false), Err(HostError::Busy));
        // Another link (the ACL, another SCO handle): nothing
        assert_eq!(v.on_disconnected(0x0040), None);
        assert_eq!(v.on_disconnected(0x0102), None);
        assert_eq!(v.on_disconnected(0x0101), Some(crate::HostEvent::Voice(VoiceEvent::Released)));
        assert!(!v.usable);
        assert_eq!(v.check(true, Some(0x40), false), Ok(()));
        // HFP's own RELEASED after it: no second event
        assert_eq!(v.on_released(), None);
        assert_eq!(v.on_disconnected(0x0101), None);
    }

    #[test]
    fn codec_names() {
        assert_eq!(codec_name(1), "CVSD");
        assert_eq!(codec_name(2), "mSBC");
        assert_eq!(codec_name(3), "LC3-SWB");
    }
}
