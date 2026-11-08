# Headset microphone (developer note)

Status: in development on `feat/headset-mic`. Not user-facing yet: every part
of it is behind developer mode. Verified on the emulator only (see
"Verified" and the hardware checklist at the end).

The feature was written in parallel parts (W1 transport and HFP AG, W2
voice-link, W3 classic session, W4 LE Audio, W5 UI and settings, W6
integration). The first sections are the contract between them, as the API
stands now; "As implemented" collects what each part does beyond it.

## What the feature is

"Headset mode" lets the headphones' microphone reach Windows, and sends the
PC's audio to the headphones over the same voice link:

- **Classic devices (HFP):** the A2DP stream is suspended, an HFP audio
  connection (eSCO/SCO) is opened on the existing service level connection,
  and both directions run over it. The codec is negotiated: LC3-SWB (32 kHz),
  mSBC (16 kHz) or CVSD (8 kHz), in that preference order. Playback is mono
  call quality while headset mode is on; this is inherent to HFP.
- **LE Audio devices:** the unicast stream becomes bidirectional (a
  conversational configuration): the sink ASEs keep playing LC3 at the media
  preset, and a source ASE sends the microphone to the PC on the same CIS
  (or on its own CIS). Playback quality does not change.

The microphone audio is **rendered** to a Windows output device the user picks
(a virtual audio cable such as VB-CABLE's "CABLE Input"); applications then
see the cable's capture side ("CABLE Output") as a microphone. BlueCaster has
no audio driver of its own, so this is the only way without a signed kernel
driver.

## Settings and IPC (crate `ipc`, core `state.rs`, UI)

Per device, in `Profile` (`app/src/crates/ipc/src/protocol.rs`), all with
`#[serde(default)]`:

| Field | Type | Meaning |
|---|---|---|
| `headset_mic` | `String` | Headset mode for this device: `"off"` (default), `"on"`, `"auto"` (see "Auto mode"); anything else reads as off. |
| `mic_device_id` | `String` | Render endpoint id that receives the microphone. Empty = none chosen (headset mode stays off and the UI says so). |
| `mic_device_name` | `String` | Friendly name, for display when the device is missing. |

In `StreamStatus`: `voice: Option<VoiceStatus>` (`#[serde(default)]
#[ts(optional = nullable)]`), where

```rust
pub struct VoiceStatus {
    /// "cvsd" | "msbc" | "lc3_swb" | "lc3"
    pub codec: String,
    /// "hfp" | "le"
    pub link: String,
    /// Sampling rate of the voice link in Hz (8000 / 16000 / 32000 / 48000 ...)
    pub rate: u32,
    /// Microphone output device in use (id), empty for a WAV file
    pub mic_device_id: String,
}
```

`voice` is `Some` only while a voice link carries audio (classic: the HFP
audio connection is up and armed; LE: the microphone path runs). The core
clears it on Idle / Error / Reconnecting like the battery.

The core's `bt_host::HeadsetMic` (re-exported as
`bluecaster_core::HeadsetMic`: `Off`, `On`, `Auto`, `parse(&str)`,
`as_str()`, `is_on()`) carries the mode from there on. The core's stored
profile (`profiles.json`) keeps the string, written only when not off; a
`true` / `false` saved by the first development builds reads as on / off,
and the IPC value is normalized on saving (anything else is stored as
off).

Headset mode is **in force** when developer mode is on
(`Settings::headset_mic_in_force(mode) -> HeadsetMic`, Off outside it), the
profile's mode is not off and `mic_device_id` is not empty. `state.rs`
`start_profile` then sets `StreamRequest::headset_mic` (the mode) /
`mic_output = Some(mic_device_id)` and, for an LE Audio entry, the same in
`LeRequest::headset_mic` / `mic_output` (`set_headset_mic`; never for a
broadcast). LE Audio treats Auto as On: its conversational stream keeps the
media preset, so there is no quality to win back while nobody records. The
stack offers voice (`voice_wanted`) for On and Auto alike.

`profile.save` of the profile in use that changes only `headset_mic` /
`mic_device_*` (`headset_change`):

- `Unused` (outside developer mode, or the output changed while headset
  mode is off): the stream is not touched.
- `InPlace` (a classic stream streaming): `Service::set_headset_mic(mode,
  mic_output) -> bool` (`VoiceChange { mode, mic_output }`); false (no
  classic stream streaming, or a stack started without voice when turning
  on or to auto) restarts the profile as for any other change.
- `Restart` (LE Audio entries, or a stream still connecting): the stream
  starts again.

Feedback loop check (pure, `profiles.rs`): `mic_output_conflicts(source,
mic_id, default_output_id) -> Option<&'static str>` returns
`"error.headset_mic_loop"` when the microphone's render endpoint is the
captured virtual / output device, or the default output while System
Loopback is captured; `Profile::mic_device_conflicts` calls it with the
profile's source. Both sessions call it through `voice::mic_conflict` with
the source they really capture.

CLI (`app/src/core/src/cli.rs`, "Headset microphone (developer mode)"):
`--headset-mic [on|auto]` (headset mode once streaming, alone: on; on an
`--le-audio` run, the conversational configuration), `--mic-output <name|path.wav>` (a render
device by id or part of its friendly name, default the Windows default
output, or a WAV file to write). No flag for the codec: the negotiation
decides. Development / testing: `--headset-mic-toggle <on_s>,<off_s>`
switches headset mode on and off at those seconds of a classic stream
through `Service::set_headset_mic` (`Service::set_voice_offered` makes the
stack offer voice from the start).

Language keys (all 8 files): `ui.headset_mic` ("Headset microphone"),
`ui.headset_mic_hint`, `ui.mic_output` ("Microphone output"),
`ui.mic_output_hint` (apps use the cable's recording side),
`ui.headset_mic_quality` (the call-quality warning, shown only for classic
devices), `ui.headset_mic_no_output` (no output device chosen),
`status.voice_codec` ("Voice: %s"), codec names `codec.cvsd`,
`codec.msbc`, `codec.lc3_swb` ("lc3" shows as plain "LC3"),
`error.headset_mic_loop` (the feedback-loop refusal, a status message),
`status.headset_mic_failed`, `status.headset_mic_output_failed`. Hints stay
under 120 characters.

## Auto mode

`"auto"` gives classic devices the voice link only while an application
records the microphone, and full-quality A2DP otherwise.

- **What pairs** (`win_sys::audio_policy::capture_sessions_active(render_id)
  -> windows::core::Result<Option<bool>>`, through
  `audio_capture::mic_capture_active(id) -> Result<Option<bool>,
  CaptureError>`, which initialises COM itself): the active capture
  endpoints in the microphone output's container (`PKEY_Device_ContainerId`,
  e.g. a USB headset's own microphone); when none is, those of the same
  adapter (`PKEY_DeviceInterface_FriendlyName`: "VB-Audio Virtual Cable" for
  both "CABLE Input" and "CABLE Output"). The null container
  `{00000000-0000-0000-ffff-ffffffffffff}` (internal and software devices
  share it) and an all-zero one pair nothing, or every built-in microphone
  would pair with the cable. `Some(true)`: an audio session on one of them
  is `AudioSessionStateActive` (an app has its capture started), this
  process's own left out; `None`: no capture endpoint pairs (logged once as
  a warning, counted as nobody recording); an error (the output is gone)
  counts as nobody too, logged once.
- **The poll** (`voice.rs` `AutoWatch`): on the session tick (200 ms), at
  most once a second (`AUTO_POLL`); the output is resolved once per mode or
  output change, not on every poll. A WAV `mic_output` counts as always
  recorded from (the CLI's test path).
- **Hysteresis**: the first poll that sees a recording turns the voice link
  on; it goes back to A2DP only after the polls have seen nobody for 2 s
  (`AUTO_IDLE`), so an app that opens the device again does not bounce the
  link. The link going down by Auto leaves the plain Streaming status (no
  message); `StreamStatus.voice` is None while Auto is idle.
- `held_off` (after a failure or a refusal) is not cleared by Auto's own on
  and off, only by a mode or output change. The headphones hanging up in
  Auto keep the link closed until nobody has recorded (then the next
  recording opens it again); in On they end headset mode for the request as
  before.
- **The emulator** cannot test the detection (no Windows capture sessions
  there): `run_test.py --voice cvsd-auto` runs `--headset-mic auto` with a
  WAV output and checks Auto's "an application records" log line plus
  cvsd's checks. The pairing rule is unit-tested
  (`capture_endpoints_pair_by_container_then_adapter`), the hysteresis too
  (`auto_turns_on_at_once_and_off_after_two_idle_seconds`); the ignored
  `win-sys` test `print_capture_sessions` prints every output's pairing and
  answer on a real PC (record "CABLE Output" to see `Some(true)`).

## Voice audio plumbing (crate `app/src/crates/voice-link`)

`#![forbid(unsafe_code)]`; deps: `wasapi`, `audio-capture` (for `Format`,
`Sink`, `render_devices`), `rtrb`, `tracing`, `btstack-sys` / `lc3-sys`
(codecs), `win-sys` (COM apartment, MMCSS), `thiserror`. `voice_link` is in
`OWN_TARGETS` in `app/src/core/src/logging.rs`.

```rust
/// PCM going to the Windows render endpoint (the headset microphone).
pub struct Renderer { .. }
impl Renderer {
    /// Opens the render endpoint `device_id` ("" = the default output;
    /// WASAPI shared, event-driven, autoconvert on, MMCSS "Audio" thread
    /// "voice-render"), consuming mono i16 at `rate` Hz. Silence until
    /// `prefill_ms` is queued and on underrun; drift control (below).
    pub fn open(device_id: &str, rate: u32, prefill_ms: u32) -> Result<Renderer, RenderError>;
    /// The producer side: lock-free, callable from the BTstack or decode
    /// thread. The first call hands out the queue's producer; a later one
    /// is detached and only counts its pushes as overflow.
    pub fn producer(&self) -> PcmProducer;
    pub fn stats(&self) -> RenderStats;
    pub fn ended(&self) -> bool;              // device removed or failed
    pub fn end_error(&self) -> Option<&str>;
}
pub struct PcmProducer { .. }
impl PcmProducer { pub fn push(&mut self, mono_i16: &[i16]) -> usize /* frames accepted */; }
/// The same queue without a device (tests, tools)
pub fn pcm_channel(capacity: usize) -> (PcmProducer, PcmConsumer);
/// The microphone into a mono 16-bit WAV file ("voice-wav" writer thread)
pub struct WavSink { .. }
impl WavSink {
    pub fn create(path: &Path, rate: u32) -> io::Result<WavSink>;
    pub fn producer(&mut self) -> Option<PcmProducer>; // once
}

/// Capture -> mono i16 at a fixed rate, for the uplink (audio_capture::Sink;
/// the capture is opened at `preferred_rate = rate`, a windowed-sinc
/// resampler covers a capture that still differs)
pub struct UplinkSink { .. }
impl UplinkSink { pub fn new(rate: u32, capacity_ms: u32) -> (UplinkSink, UplinkConsumer); }
impl UplinkConsumer {
    /// Pops up to `out.len()` mono samples; zeros when short (an underrun).
    /// Never blocks.
    pub fn pop(&mut self, out: &mut [i16]) -> usize;
}

/// HFP voice codecs over SCO payloads
pub enum VoiceCodec { Cvsd, Msbc, Lc3Swb } // rate(), from_hfp_id(), hfp_id(), name()
pub struct VoiceEncoder { .. } // new(codec), fill(&mut UplinkConsumer, &mut [u8]), stats()
pub struct VoiceDecoder { .. } // new(codec), decode(payload, status, &mut PcmProducer), stats()
```

The codec primitives come from `btstack-sys` (`btstack_sys::voice`, C glue in
`csrc/voice_codecs.c`): `MsbcEncoder` / `MsbcDecoder` (the Bluedroid SBC
encoder / decoder through BTstack's instance API, mSBC mode; the decoder does
H2 sync and `btstack_sbc_plc` itself) and `CvsdPlc` (`btstack_cvsd_plc`, in
pieces of at most 60 samples). The Bluedroid encoder is always compiled
(feature `sbc-reference` adds only `csrc/sbc_reference.c`). LC3-SWB uses
`lc3_sys::{Lc3Encoder, Lc3Decoder}` (32 kHz, 7.5 ms, 58 octets) with H2
framing / sync in Rust (`voice_link::h2`). Encoder: CVSD is 16-bit LE PCM;
mSBC is H2 header + 57-byte frame + 1 padding byte per 120 samples;
LC3-SWB H2 header + 58-byte frame per 240 samples. Any packet status flag
other than 0 is concealed.

## Microphone output, shared by both paths (core `voice.rs`)

```rust
pub enum MicTarget { Device { id: String, name: String }, Wav(PathBuf) }
impl MicTarget {
    pub fn device_id(&self) -> &str;     // "" for a file
    pub fn nth(&self, n: u32) -> MicTarget; // x.wav, x-2.wav, ... for later LE streams
}
/// id, friendly-name part, ".wav" path; None / "": the default output.
/// None when there is no output device at all.
pub fn resolve_mic_output(spec: Option<&str>) -> Option<MicTarget>;
/// profiles::mic_output_conflicts against the default output now
pub fn mic_conflict(target: &MicTarget, source: &SourceRequest) -> Option<&'static str>;
pub enum MicOutput { Device(Renderer), Wav(WavSink) }
impl MicOutput {
    /// Renderer at `rate`, prefill 40 ms, or the WavSink
    pub fn open(target: &MicTarget, rate: u32) -> Result<MicOutput, String>;
    pub fn producer(&mut self) -> Option<PcmProducer>;
    pub fn ended(&self) -> Option<String>;
}
```

The classic session opens it at the HFP codec's rate when the audio
connection is up; the LE session at the microphone's LC3 rate when the
stream starts (`le_stream::LeMicPath`, the le-decode thread's `PcmOut` is a
small adapter around the `PcmProducer`).

## SCO transport (crates `hci-transport`, `win-sys`, `btstack-sys`)

`btstack_config.h` has `ENABLE_SCO_OVER_HCI`, `ENABLE_HFP_WIDE_BAND_SPEECH`,
`ENABLE_HFP_SUPER_WIDE_BAND_SPEECH`.

`ENABLE_SCO_OVER_HCI` is not inert: BTstack's HCI init then sends Write
Synchronous Flow Control Enable and Write Default Erroneous Data Reporting to
every controller that lists them (hci.c, the init step
`HCI_INIT_WRITE_SYNCHRONOUS_FLOW_CONTROL_ENABLE` and the one after it), and
BCM Write SCO PCM Int (SCO routed
over HCI) to Broadcom controllers. BTstack has no switch for it at run time.
**SCO hidden** (`btstack-sys/src/transport.rs`, the default of every run):
the transport clears those two supported-commands bits (octet 10 bit 4,
octet 18 bit 3) in the controller's Read Local Supported Commands answer
before BTstack sees it, as LE hidden does for LE, so a stack without voice
initializes as before the feature. `Stack::set_sco_hidden(false)` (bt-host,
before HCI init, when `HostConfig::voice`) shows them. extern/btstack is not
patched. The Broadcom command depends only on the manufacturer and is still
sent (see the hardware checklist).

`HciBackend` (`btstack-sys/src/transport.rs`):

```rust
/// BTstack's set_sco_config: voice setting and number of SCO connections.
/// The USB backend starts/stops the isochronous pipes here.
fn set_sco_config(&mut self, voice_setting: u16, connections: u8) {}
/// An "H2" (USB isochronous) transport: BTstack then sizes SCO payloads to
/// 3 ms of audio and uses the transport's HCI_EVENT_SCO_CAN_SEND_NOW.
fn is_h2(&self) -> bool { false }
/// SCO data can flow now (USB: interface 1 claimed and its pipes run);
/// read when an audio connection comes up, after set_sco_config
fn sco_ready(&self) -> bool { true }
```

USB is an H2 transport **only when the stack offers voice**:
`UsbTransport::with_sco(HostConfig::voice)` (bt-host `backend()`); without
it interface 1 is not claimed and the backend is a plain async one, as
before the feature. `Stack::transport_is_h2()`, `Stack::sco_ready()` expose
both to bt-host.

shim.c keeps `transport_sync` / `transport_async` for non-H2 backends and adds
`transport_h2` (`"H2_BLUECASTER"`), chosen by `is_h2()` at install; it
registers a SCO packet handler before any SCO can-send request, which
forwards to Rust as `SOURCE_SCO`. The TCP backend carries packet type 3 both
ways.

Rust API (`btstack-sys/src/stack.rs`): `hfp_ag_init(voice)` (voice: codec
negotiation, eSCO S4; false: the battery-only AG), `sco_send`,
`sco_request_can_send_now`, `sco_packet_length`, `hfp_ag_audio_connect`,
`hfp_ag_audio_release`. `sco_send` copies into HCI's outgoing buffer; call
it only on `HCI_EVENT_SCO_CAN_SEND_NOW`. The SDP record of the voice AG gets
its wide band speech / LC3-SWB bits although
`hfp_ag_create_sdp_record_with_codecs` tests the HF's codec negotiation bit
(7) instead of the AG's (9): shim.c adds bit 7 to the features it passes to
the record builder only.

## HFP audio (crate `bt-host`)

`HostConfig::voice` (offer codec negotiation, mSBC, LC3-SWB, eSCO S4 in the
HFP AG; off = the battery-only AG) is set by `Service::host_config` when the
starting classic request is in headset mode, after
`set_headset_mic(true, ..)`, or with `set_voice_offered`. `ensure_host`
restarts a running stack without voice when voice is wanted.

```rust
pub enum VoiceEvent {
    Established { codec: u8, sco_handle: u16, rx_len: u16, tx_len: u16 },
    Released,
    Failed { status: u8 },
}
impl BtHost {
    pub fn voice_start(&self, tx: Box<dyn VoiceTx>, rx: Box<dyn VoiceRx>) -> Result<(), HostError>;
    pub fn voice_stop(&self) -> Result<(), HostError>;
    pub fn voice_enabled(&self) -> bool;
    /// What voice_start would answer, opening nothing
    pub fn voice_check(&self) -> Result<(), HostError>;
    /// An audio connection the headphones opened gets tx / rx
    pub fn voice_attach(&self, tx: Box<dyn VoiceTx>, rx: Box<dyn VoiceRx>) -> Result<(), HostError>;
}
pub trait VoiceTx: Send { fn fill(&mut self, out: &mut [u8]); }
pub trait VoiceRx: Send { fn packet(&mut self, payload: &[u8], status: u8); }
pub enum HostError { NotEnabled, NotConnected, Busy, Refused(u8), NoAnswer }
```

`voice_start` fails with `NotConnected` until the service level connection
is up (callers retry), `NotEnabled` without `HostConfig::voice`, `Busy`
while the headphones' own audio connection is open, `Refused(0xFF)`
(`NO_SCO_PATH`) when the controller cannot carry SCO over the transport
(below). `VoiceTx::fill` gets `sco_packet_length - 3` bytes on every SCO
can-send-now (TCP / emulator: 60; USB H2: 48 / 24), `VoiceRx::packet` every
received payload. `Released` comes after `voice_stop`, a release by the
headphones, the SCO link's HCI Disconnection Complete, or the service level
connection going down (once: whichever comes first).

**The SCO path is checked before BTstack sizes a packet.** On an H2
transport `hci_get_sco_packet_length_for_connection` asserts that the
controller's SCO data packet length holds 3 ms of audio (24 samples × 2
bytes for 16-bit CVSD = 48, 24 for transparent mSBC / LC3-SWB), and
`HAVE_ASSERT` aborts the process. bt-host reads the length from the Read
Buffer Size answer (`btstack_sys::events::read_buffer_size`, seen by
`HostApp::handle_hci` during HCI init; the emulator reports 60 × 8) and
`voice::sco_fits(h2, codec, len)` decides:

- `voice_start` / `voice_check`: `Refused(NO_SCO_PATH)` when no codec fits
  (length below 24, 0, or Read Buffer Size not seen) on H2; HeadsetMode
  gives up with `status.headset_mic_failed`.
- `Established` of ours: when the negotiated codec does not fit, or
  `sco_ready()` is false (the pipes did not start), the audio connection is
  released and `Failed { status: NO_SCO_PATH }` sent; `sco_packet_length`
  and `sco_send` are never called for it.
- `Established` of the headphones' own: reported as before but marked not
  usable: nothing is sent, `voice_attach` answers `Refused`.
- Ten failed `sco_send`s in a row (or a packet length of 3 or less):
  `Failed { status }` once and no more can-send-now requests; the caller
  closes it (HeadsetMode: back to A2DP).
- `voice_stop` whose release BTstack refuses (status != 0): no RELEASED
  would follow, so the state is cleared and `Released` sent at once (also
  for the release on the A2DP signaling release).

Not covered: the USB SCO thread stopping later (unplug, I/O errors) stops
the transport's send tokens without a failed send; the link then stays
silent until it is switched off or the adapter fails as a whole.

## LE Audio source ASEs (crates `le-audio`, `bt-host/src/le`, `pipeline`, core `le_stream.rs`)

- `read_device` reads Source PAC, Source Audio Locations and subscribes to
  source ASE notifications (`LeDevice::source_pacs`, `source_locations`).
- `plan::StreamRequest::conversational` adds a source direction to the
  plan: on the first sink CIS of each device (`CisPlan::source_ase:
  Option<SourceDir { ase_id, channels, allocation, octets }>`), or a
  source-only CIS for a set member with source ASEs only;
  `StreamPlan::source: Option<SourcePlan>` and `source_note` (why a
  requested microphone is missing: the stream then plays without it).
- Source presets: `32_2_1, 32_1_1, 24_2_1, 24_1_1, 16_2_1, 16_1_1`, those
  with the sink preset's frame duration first; GMAP (UGTs with the Source
  feature) puts `32_1_gs, 32_2_gs, 48_1_gs, 48_2_gs` first. Contexts:
  Conversational (GMAP: Game + Conversational) on the source ASEs, and next
  to Media on the sink ASEs where available.
- CIG: `max_sdu_p_to_c`, `rtn_p_to_c`, `phy_p_to_c`, the P->C SDU interval.
  Source ASEs go through Config Codec / QoS / Enable; the client writes
  Receiver Start Ready once all CIS are up and Receiver Stop Ready on
  Disable. `IsoSender` gets only CIS with a C->P direction.
- Receive: `LeCore::on_iso` routes HCI ISO data packets to `IsoRx` (a ring
  producer, `le/iso_recv.rs`); `LeStreamStarted::rx: Option<LeRxHandle>`
  holds the consumer, the streams and the LC3 parameters.
  `pipeline::le_rx` ("le-decode" thread): per source CIS a sequence-number
  jitter buffer (prefill two SDU intervals), one LC3 decoder per channel
  with PLC, a mono mix, one frame per call into a `PcmOut`, data driven (no
  clock of its own).
- TMAS claims Call Gateway + UMS (0x0005) and GMAS UGG Features 0x07 always
  (static GATT values).

## As implemented

### USB SCO (W1)

Compile-tested only. `UsbTransport` is an H2 transport and ports BTstack's
`platform/windows/hci_transport_h2_winusb.c`:

- interface 1 is claimed through nusb at open (nusb vendored as 0.2.8 plus
  `Interface::winusb_handle()`, see `app/src/crates/nusb/SOURCE.txt`, wired
  by `[patch.crates-io]`);
- `set_sco_config` selects alternate setting 1/2/3 (8-bit voice) or 2/4/5
  (16-bit) for one to three links and starts a `usb-sco` thread, which keeps
  8 isochronous reads of 3 ms in flight (`WinUsb_ReadIsochPipeAsap`,
  continuing the stream) and reassembles the IN byte stream into SCO
  packets; each outgoing SCO packet is one `WinUsb_WriteIsochPipeAsap`, up to
  20 queued or in flight (60 ms); `HCI_EVENT_SCO_CAN_SEND_NOW` is handed out
  while there is room; with no link left the thread stops and alternate
  setting 0 is selected;
- the WinUSB calls are in `win-sys/src/usb_isoch.rs`; their OVERLAPPEDs set
  the low bit of `hEvent`, so nusb's completion port never sees them;
- only a stack with voice gets an H2 USB transport (`with_sco`; `is_h2` is
  read at install, before open) and claims interface 1; without a
  claimable interface 1, SCO is refused (`can_send`) and dropped (`send`,
  still reported as sent so BTstack's buffer is freed), and `sco_ready()`
  is false, so bt-host gives the audio connection up at Established
  instead of letting it stall without send tokens;
- high speed adapters: one packet per 2^(bInterval-1) microframes, rounded
  to whole 1 ms frames (bInterval 1: 24 packets per 3 ms read); full speed
  (most adapters): 3 packets per read; unknown speed counts as full speed.

Unit tests: alternate setting choice, packet sizes, SCO reassembly (also
across failed isochronous packets), send token accounting, packet counts
for both speeds.

### voice-link (W2, W3, W6)

- `Renderer` queue: max(4 x prefill, 250 ms). `RenderStats {
  frames_written, silence_frames, underruns, overflow, dropped, duplicated,
  queued }` (frames = mono samples at the voice rate).
- Drift control (`drift.rs`, renderer and uplink alike): the trough of the
  queue level over 200 ms against the prefill, ± the tolerance (renderer:
  one device period; uplink: 10 ms), at most 0.25 % correction per window.
  A trough more than twice the tolerance below primes again once
  (`Adjust::Reprime`). The first window after (re)priming moves the level
  to the prefill plus half the tolerance in one step (at most a tolerance's
  worth): a late input block only lowers the trough, so the level keeps its
  room below and a loaded machine no longer causes small corrective steps
  (on the emulator the CVSD uplink repeated 4 samples every 200 ms after
  the start before the one-step move, audible as clicks).
- `UplinkConsumer` sends zeros until 30 ms are queued (at the start and
  after an underrun). `queued()`, `stats() -> UplinkStats`, `rate()`.
- `voice_link::h2`: `H2Framer`, `H2Sync` (a port of BTstack's
  `hfp_h2_sync`: bad or unsynchronised bytes come out as one `Bad` per 60).
- 10 s debug summaries: "voice link: render ...", "voice link: uplink
  ...", "voice link: downlink ...".

### Headset mode in a classic session (W3; `crates/core/src/voice.rs`, `HeadsetMode`, driven from `stream.rs`)

- Events: the host-events thread queues `HostEvent::Voice` for the session
  (`Inner::take_voice_events`) and wakes its tick; `take_voice_change()` is
  polled like `take_source_change()`. The session emits
  `ServiceEvent::Voice(Option<ipc::VoiceStatus>)`; `on_service_event` sets
  `StreamStatus.voice` from it.
- On (each tick of a link that is fine, until it works or is given up):
  `host.voice_check()` first: NotConnected is retried for up to 20 s (the
  service level connection comes after A2DP), Busy waits for the headphones'
  own Established, anything else (NotEnabled, Refused: no SCO path) gives
  up with `status.headset_mic_failed`; only then resolve the output (a
  device enumeration, not on every tick of the wait) and refuse with
  `error.headset_mic_loop` (a Streaming status message, the stream goes
  on) when `mic_conflict` says so; then stop the capture and the encode
  thread, `host.stop_stream()` (AVDTP Suspend, link kept),
  `host.voice_start(tx, rx)`.
- tx / rx are shells around `Arc<Mutex<Option<..>>>` slots: silence /
  dropped packets until armed. On `Established{codec}` the session thread
  opens the `MicOutput` at the codec rate, a capture of the current source
  at `preferred_rate = codec rate` into an `UplinkSink` (200 ms), and
  installs encoder + uplink and decoder + producer under the lock. The
  BTstack thread only `try_lock`s (contended only while arming / disarming:
  one packet of silence), and the armed path allocates nothing.
- Off, `Released`, `Failed` (before Established: bt-host closed it
  already; after Established, SCO stopped flowing: `voice_stop`), no
  Established within 10 s, the render device ending, an output that does
  not open: `voice_stop` (when asked for),
  disarm, then `host.start_stream()` + a fresh encoder and pipeline + the
  A2DP capture again (`A2dpSide::resume`, the steps a reconnect takes). A
  resume that fails ends the session with its error. Failures give
  `status.headset_mic_failed` / `status.headset_mic_output_failed` and hold
  headset mode off until the next change.
- Released by the headphones while on: back to A2DP and the request is
  cleared (as if switched off), so a headset that hangs up is not reopened
  in a loop; saving the setting again or a new stream opens it again.
- The headphones open the audio connection themselves (a voice assistant
  or call button): with headset mode on (and no conflict) it is taken over
  (`voice_attach`) and armed. Otherwise the decision depends on the stack:
  - started **with voice** (developer headset mode, `host.voice_enabled()`):
    `voice_stop` closes it, as nothing would carry it;
  - started **without voice** (every user outside developer mode, and a
    developer with headset mode off): it is **left alone** and logged once
    per session ("no headset mode, left alone"). Before this branch BTstack
    accepted such links and BlueCaster ignored them; closing them would
    break the headphones' own button for every user. Without voice the
    transport is not H2, so BTstack's SCO length assert cannot fire for it.
- Link loss while on: the voice link is dropped, the A2DP capture started
  again and the normal reconnect builds the pipeline; headset mode stays
  requested, so the reconnected stream opens the voice link again (stale
  voice events are dropped after the reconnect). Session end: `voice_stop`,
  no resume.
- While on, a source change restarts only the uplink capture, a bitpool
  change only updates the encoder config A2DP resumes with, and the A2DP
  capture's reopen / app-attach logic is paused. An Application source
  that was not running when the link came up, or exited since (its process
  gone or the capture ended), is looked for at most once a second
  (`app_recheck_due`) and the uplink captures it again once it runs
  (silence until then; a process whose capture failed is not retried).
- `Service::set_headset_mic` waits for the service's `control` lock (a
  short holder, such as another in-place change, no longer turns the save
  into a profile restart); without a stream streaming it answers false at
  once, so a start or stop under way, which holds `control` until the old
  session ended, is waited for only when it begins between that check and
  the lock (the session thread itself never takes `control`, so there is
  no deadlock with it). Off clears `voice_wanted`: the running
  stack keeps offering voice (`ensure_host` restarts only when voice is
  wanted and missing), a stack started later goes without it.
- Logs: "voice: ..." (switching, codec, "A2DP suspended / resumed", a 10 s
  summary, "voice: <codec> link down after N s: ..."), plus voice-link's
  own summaries.

### LE Audio microphone (W4, W6; `crates/core/src/le_stream.rs`)

- Before the stream is set up, `le_mic_target(headset_mic, mic_output,
  source)` resolves the output and runs the feedback-loop check: no output
  device (`status.headset_mic_output_failed`) or a conflict
  (`error.headset_mic_loop`) streams without the microphone (no
  conversational configuration asked for) and shows that message as the
  Streaming status.
- After each (re)start, `start_mic` opens the output at the microphone's
  rate (`MicTarget::nth`: a stream set up again writes `x-2.wav`, ...) and
  starts the le-decode thread into it; the session reports
  `VoiceStatus { codec: "lc3", link: "le", rate, mic_device_id }` and None
  when the microphone stops (link loss, session end). An output that does
  not open, or a render device that goes away later, stops only the
  microphone (`status.headset_mic_output_failed`); the stream goes on.
- Stopping: the decode thread first (what arrived is decoded), then the
  output (a WAV file is finished).
- The CLI's LE flow (`cli.rs` `le_run`) uses the same `le_mic_target` /
  `start_mic` and prints "Microphone: LC3 at N Hz -> <target>".

## Emulator (tools/emu)

HFP: `emu_sink.py --hfp [cvsd|msbc|lc3swb]` adds an HF role (`hfp_audio.py`:
`HfpEmuController`, `HfSide`; Bumble `HfProtocol`, HF SDP record, codec
list), accepts the AG's audio connection, sinks SCO and sends a 1.5 kHz
tone back (mSBC / LC3-SWB: four frames of it from BlueCaster's own encoders,
`tools/emu/voice_frames.py`, generated by `cargo run -p voice-link --example
emu_voice_frames`, checked by `voice-link/tests/emu_frames.rs`).

- BTstack's non-H2 SCO path on TCP: the emulated controller lists Write
  Synchronous Flow Control Enable (`hfp_audio.py` would turn it on and send
  Number Of Completed Packets per SCO packet), but its features say "no
  BR/EDR" (octet 4 bit 5, Bumble's default), so `hci_classic_supported()`
  is false during HCI init and BTstack skips its classic init steps, the
  SCO commands included (the HCI captures have no 0x0C2F / 0x0C5B with or
  without voice). BTstack then paces SCO by the packets it receives
  (`sco_tx_ready`), which the controller sends in its 7.5 ms interval.
  Packets per second each way: 267 (CVSD), 133 (mSBC, LC3-SWB). The
  emulator therefore cannot show the SCO init commands; its captures do
  show SCO hidden at work (octet 10 of Read Local Supported Commands as
  BTstack saw it: 0xF4 with voice, 0xE4 without).
- eSCO only on the bus; Synchronous Connection Complete reports interval
  12 slots, window 2, 60-byte packets. BTstack picks S4 (CVSD) and T2
  (mSBC, LC3-SWB).
- The `cvsd` HF has no codec negotiation, `msbc` offers CVSD + mSBC,
  `lc3swb` CVSD + mSBC + LC3-SWB.

`run_test.py --voice [cvsd|msbc|lc3swb|cvsd-roundtrip|cvsd-auto]` (base port + 400)
runs `--headset-mic --mic-output <out>/mic.wav` and checks the negotiated
codec in both logs, SCO both ways in both captures, "Voice: <codec> at
<rate> Hz" in BlueCaster's output, the sink's verdict on what BlueCaster
sent (cvsd: the test tone mixed to mono in 90 % of the 20 ms blocks of the
last 3 s; msbc / lc3swb: H2 frames in sequence), and that mic.wav holds the
HF's 1.5 kHz tone. `cvsd-roundtrip` runs `--headset-mic-toggle 3,6` and
also checks "voice: A2DP resumed", A2DP media for at least 2 s after the
last SCO packet and the test tone in the last second the sink decoded.

LE: `emu_le_audio_sink.py --mic [SINKS]` (1-based sink numbers, default
all) and `--mic-rates` add a Source PAC, source contexts and a source ASE
that sends a 1 kHz tone (frames from the ignored test `mic_frames` in
`app/src/crates/codec/src/lc3.rs`). `run_test.py --le-audio` cases
`mic-stereo`, `mic-earbuds`, `mic-mono-16k` run `--headset-mic --mic-output
<out>/mic.wav` and check the planned microphone preset, Receiver Start /
Stop Ready, the SDU counts at every stage, mic.wav against BlueCaster's
"LE Audio: microphone: N frames out" and the tone in it. `bluecaster_decode`
reports a microphone as "=== Microphone N" ("Mic ..." labels, a mono WAV
per CIS).

## Verified

On the emulator only, 2026-11-08 (W6, after the integration): see the
summary lines below; `cargo test --workspace` covers the output resolution,
the LE microphone through `WavSink`, the profile's LE request and the drift
control (a late capture block every 250 ms needs no step after priming).

- `--voice`: 4/4 (cvsd: ~1565 SCO packets each way in 6 s, the test tone
  ratio 0.500; msbc / lc3swb: ~782 H2 frames in sequence; mic.wav 1500 Hz
  at amplitude 0.244 (cvsd, msbc) / 0.260 (lc3swb); cvsd-roundtrip: A2DP
  resumed, both test tones in the last second).
- `--le-audio`: 44/44 (41 in the full run, `mcp-control`,
  `mcp-unsupported`, `gmap-media-context` failed with late SDUs only on
  the playback path under load and passed on their retry). The `mic-*`
  cases write through voice-link's `WavSink` now: 515 SDUs from the
  headset, mic.wav 5.11 s with the 1 kHz tone, no gaps.
- default codec set: 90/90 (88 in the full run; `oppo-hi-res-on` and
  `oppo-drop` showed RTP timestamp steps once and passed on their retry);
  `--scan` 2/2; `--link` 8/8.

After the review fixes (H2 only with voice, SCO length check, SCO hidden,
release / failure paths, the headphones' own link left alone without
voice), 2026-11-08: `--voice` 4/4 (cvsd ~1568 SCO packets each way, 0 send
errors; msbc / lc3swb 789 H2 frames in sequence; cvsd-roundtrip A2DP
resumed, both tones in the last second; "controller buffers: ACL 8 x 1021
bytes, SCO 8 x 60 bytes" parsed in every run), `--le-audio mic-stereo
stereo` 2/2, `--codecs sbc` 1/1, `--link` 8/8. `cargo test --workspace`
covers the Read Buffer Size parser, `sco_fits` / `check` decisions, the SCO
link's disconnection, SCO hidden's bits, the uplink's application look-up
timing and `set_headset_mic` waiting for `control`. None of the new failure
paths (short SCO buffer, pipes not running, failed sends, refused release)
can be reached on the emulator.

### On hardware (2026-11-08): TP-Link UB500 (RTL8761BU, WinUSB) + WH-1000XM4

The first real run, with `bluecaster-core --cli -u 2357:0604 -d <XM4> -c sbc
--test-tone --headset-mic --mic-output ...` (logs and `.pklg` captures in
`C:\BC-mic-target\hw\run5..run10`):

- HCI init with voice: interface 1 claimed, "controller buffers: ACL 6 x
  1021 bytes, SCO 12 x 255 bytes"; without voice the same adapter and
  headphones stream as with beta.3 (`run2-novoice.log`).
- HFP: the WH-1000XM4 offers CVSD + mSBC (`AT+BAC=1,2`), takes `+BCS:2`,
  and the eSCO comes up in ~80 ms as **mSBC** (voice setting 0x0043,
  packet type 0x0040 = EV3, 12 slots, 60-byte air packets, 27-byte HCI
  packets), with no call state and no call UI on the headphones.
- USB SCO: alt 1 (9-byte isochronous packets), ~333 packets/s each way,
  0 USB errors, 0 failed isochronous packets, 0 send errors over 30 s
  (9872 sent / 9854 received). The eSCO link marks ~4 % of the received
  packets as possibly invalid / lost (`status != 0`, "N bad"); the mSBC
  PLC concealed 13-46 frames per run.
- Microphone: a 16 kHz mono WAV with room noise (`mic1.wav`, 29.4 s) and
  rendering to "CABLE Input (VB-Audio Virtual Cable)" at 16 kHz, 0
  underruns / overflows, no drift steps in 12 s.
- Round trip (`--headset-mic-toggle 4,16`): A2DP suspended, mSBC for 12 s,
  A2DP resumed at SBC 357 kbps.

- Auto (`--headset-mic auto --mic-output "CABLE Input"`, with the
  `hold_capture` example recording from "CABLE Output" for 12 s, a 3 s
  pause, then 5 s; `run11.log`): the link came up within a second of the
  recording starting (mSBC, open in 89 / 71 ms), went back to A2DP 2.3 s
  after it ended, twice in a row; A2DP resumed at SBC 357 kbps each time.

Two fixes came out of it:

1. **The codec negotiation raced the headphones' own commands.** Right
   after the service level connection the XM4 sends `AT+NREC=0`,
   `AT+CLIP=1`, `AT+XAPL=...`, `AT+VGS=6`, `AT+IPHONEACCEV=...` within
   ~250 ms. BTstack's AG enforces half duplex: a command that arrives while
   its own OK is pending gets `ERROR` (hfp_ag.c "Received a command while
   ok pending"). When `+BCS:2` went out in that burst, the XM4's `AT+BCS=2`
   got the `ERROR`, no SCO followed and headset mode gave up after
   `OPEN_TIMEOUT` (`run5.log`). `Voice::check` now answers
   `HostError::Settling` until the service level connection is `SLC_SETTLE`
   (1 s) old **and** no HFP event came for `HF_QUIET` (500 ms); the core
   waits for the next tick. The `AT+IPHONEACCEV` after `+XAPL` still gets
   an `ERROR` (as in beta.3: BTstack's answer to XAPL keeps OK pending);
   the XM4 repeats its battery level later.
2. **SetCurrentAlternateSetting answered ERROR_BUSY (170) once** when the
   SCO came up 4 s into A2DP streaming (`run8.log`; an HCI command, a
   control transfer on the same device, was probably in flight).
   `select_alt` tries up to 10 times 10 ms apart (`SCO_ALT_TRIES`,
   `SCO_ALT_RETRY`); the next runs did not hit it.

## Not verified / hardware checklist

Checked on the UB500 + WH-1000XM4 (mSBC, full speed, one link) so far: 0
(Realtek with and without voice), 1, 2, 3, 4, 8, 11 (mSBC only), 12
(accepted, no call UI), 13 (one on/off cycle, and Auto's two record / stop cycles), 14 (12 s, not an hour).
Still open, in this order:

HCI init (every user, `ENABLE_SCO_OVER_HCI` is compiled in):

0. The user's adapters still initialize and stream A2DP, each **without**
   developer headset mode (SCO hidden: the init must be the one of
   3.0.0-beta.3, no Write Synchronous Flow Control Enable / Write Default
   Erroneous Data Reporting in the HCI capture) and **with** it (both
   commands sent if listed; a Command Complete with an error is fine, a
   controller that does not answer stalls init): RTL8761BU, Intel,
   MediaTek. A Broadcom adapter gets BCM Write SCO PCM Int (0xFC1C) in
   both cases: check one still initializes. Headphones that open an
   audio connection themselves (a voice assistant button) with headset mode
   off: BlueCaster does not close it (logs "left alone" once), the button
   works as with 3.0.0-beta.3.

USB SCO (`hci-transport/src/usb.rs`):

1. Interface 1 can be claimed (debug line "interface 1 claimed for SCO");
   adapters whose WinUSB sits on a usbccgp child are unknown.
2. SetCurrentAlternateSetting(1/2) and RegisterIsochBuffer after it succeed
   ("USB: SCO on interface 1 alt N" with the pipe sizes); WinUSB's
   MaximumBytesPerInterval is 9 / 17.
3. IN: about 333 SCO packets/s per link (the 10 s "USB: SCO last ..." debug
   line) with the right handle and length (24 bytes transparent, 48 CVSD
   16-bit), the microphone decodes cleanly; failed isochronous packets and
   gaps (stream continuation falling back to ASAP).
4. OUT: a 3+24 / 3+48 byte write takes 3 frames ("USB: SCO out:"), writes
   complete without ERROR_INVALID_PARAMETER, the headset hears the PC without
   underruns or overruns, tokens come about every 3 ms.
5. Link up / down several times, two links (alt 2 to 4), close during SCO:
   no "did not return" errors, alt 0 selected, the next open works (no
   error 5).
6. High speed adapters: the read packet counts and especially writes
   (WinUSB may reject writes that are not whole frames).
7. Unplug during SCO ends the thread cleanly.
8. The controller's SCO buffer length ("controller buffers: ... SCO N x L
   bytes" debug line): L >= 48 for CVSD, >= 24 for mSBC / LC3-SWB, or
   headset mode is refused / the link given up (`NO_SCO_PATH`) instead of
   BTstack's H2 assert aborting the process.
9. Latency of the 20-slot OUT ring (60 ms); lower `SCO_OUT_SLOTS` if too
   high.
10. Whether controllers align SCO IN to 3-packet boundaries (how fast the
    reassembly recovers after a lost packet).

Classic headphones (HFP):

11. Codec negotiation with real headphones: LC3-SWB / mSBC / CVSD chosen as
    expected, eSCO S4 / T2 accepted, the fallback to CVSD
    (`sco_for_msbc_failed`).
12. Opening the audio connection without a call: accepted, refused, or a
    call UI on the headphones; headphones that close it right away end
    headset mode for that request.
13. Switching on and off while streaming (A2DP suspend / resume) several
    times; link loss while on (the reconnect opens the voice link again).

Render and capture (both paths):

14. The microphone in a real virtual cable (VB-CABLE "CABLE Input"):
    audible in an app recording "CABLE Output", no clicks; over an hour the
    drift control's dropped / repeated counts stay near the clock
    difference ("voice link: render last 10 s" debug lines).
15. The feedback-loop refusal with the cable captured (Virtual Device
    source) and with System Loopback while the cable is the default output.
16. The render device removed while in use (the classic session goes back
    to A2DP, the LE stream plays on without the microphone, both with
    `status.headset_mic_output_failed`).
17. The uplink capture of every source kind at 8 / 16 / 32 kHz (Windows'
    conversion).

LE Audio headsets:

18. A real headset or earbuds with a source ASE: the conversational plan,
    Receiver Start / Stop Ready, the microphone rendered at 16 / 24 / 32 kHz
    (and 48 kHz with GMAP `_gs` presets), playback quality unchanged.
19. Two earbuds with a microphone each (the mono mix aligned by arrival).

UI:

20. The Codecs screen's switch and output picker with real devices; the
    "Voice: ..." line on Home, sidebar, flyout and tray for both kinds.

## Open questions / risks

- WinUSB isochronous is verified on one full speed adapter (RTL8761BU, alt
  1, one mSBC link); high speed adapters, CVSD (alt 2 / 48-byte packets),
  two links and the Intel / MediaTek loaders with voice are not.
- The AG opens HFP audio with `hfp_ag_establish_audio_connection` and no
  call state; the WH-1000XM4 accepts it silently, other headphones may
  refuse or show a call UI, and would need a call state from the AG.
- The LE conversational stream does not follow CCP's call state.
- An LE headset's clock is absorbed only by the renderer's drift control
  (the le-decode thread has no clock of its own); a WAV file has no drift
  control at all (it records what arrives).

## UI: Off / Auto / On and the chip

- Codecs (`screens/Codecs.tsx`, `HeadsetMic`): a `Segmented` Off / Auto /
  On saves `headset_mic`; the output picker and the warnings show for Auto
  and On, Auto adds `ui.headset_mic_auto_hint`.
- Home's device card and the flyout's state line (`HeadsetChip` in
  `screens/Home.tsx`): a microphone chip with the mode, only in developer
  mode for a profile with `mic_device_id`; a click saves the next mode
  (Off -> Auto -> On -> Off, `nextHeadsetMode` in `lib/profile.ts`).
  Accent outline for Auto / On, dashed while Auto has no voice link
  (`waiting`), `linked` while `StreamStatus.voice` is up for that profile;
  the voice codec itself stays in `VoiceCodec`.
- All three save through `saveHeadsetMic` (`screens/Codecs.tsx`), i.e.
  `profile.save` of the device's profile. The mock opens no voice link for
  Auto (no app records from the cable there).
- Keys: `ui.headset_mic_off` / `_auto` / `_on`, `ui.headset_mic_auto_hint`,
  `ui.headset_mic_cycle` (the chip's name: mode and the next one).
