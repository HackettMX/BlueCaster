//! Stream planning (v1 plan_stream() / choose_streaming_contexts() /
//! codec_config_ltvs() in le_audio_presets.cpp).
//!
//! Per preset in the requested order, the first one every device of the
//! group supports wins: a device with Front Left + Right gets one 2-channel
//! ASE (when its PAC allows 2 channels, the SDU fits and
//! `prefer_two_channel_ase`) or two 1-channel ASEs, else one mono ASE (no
//! Audio_Channel_Allocation: it renders the downmix); a device with one
//! location gets one ASE with it; no location = mono. Left / right audio go
//! to FL / FR only when the group has both, otherwise every channel carries
//! the downmix. One CIG (id 1), CIS ids in plan order, at most [`MAX_CIS`].
//! Every SDU must fit one HCI ISO data packet (SDU + 4 <=
//! ISO_Data_Packet_Length) and every CIS needs an ISO buffer.
//!
//! Conversational configuration ([`StreamRequest::conversational`], the
//! headset microphone): the sink layout stays as above (sink preset for
//! C->P) and every device with a source ASE gets one mono source direction
//! (its first source ASE) on its first sink CIS; a device of the group with
//! source ASEs only gets a CIS of its own that carries P->C only. The source
//! preset is the first of [`StreamRequest::source_preset_order`] (those with
//! the sink preset's frame duration first) that every such device's Source
//! PAC records allow. When none fits, or no device has a source ASE, the
//! plan has no source direction and says why in [`StreamPlan::source_note`]:
//! playback goes on without the microphone.

use crate::defs::{
    CFG_CHANNEL_ALLOCATION, CFG_FRAME_BLOCKS_PER_SDU, CFG_FRAME_DURATION, CFG_OCTETS_PER_FRAME, CFG_SAMPLING_FREQUENCY,
    CONTEXT_CONVERSATIONAL, CONTEXT_MEDIA, CONTEXT_UNSPECIFIED, FD_BIT_7500US, FD_BIT_10000US, LOCATION_FRONT_LEFT,
    LOCATION_FRONT_RIGHT, LOCATION_MONO, PHY_2M, sf_hz_to_cap_bit, sf_hz_to_code,
};
use crate::ltv;
use crate::pacs_ascs::{Ase, PacRecord};
use crate::presets::{DEFAULT_PRESET_ORDER, DEFAULT_SOURCE_PRESET_ORDER, PresetTable, StreamPreset};

/// CIS of one stream at most (BTstack MAX_NR_CIS)
pub const MAX_CIS: usize = 4;

/// `CisPlan::cis_handle` until the CIG is created (HCI_CON_HANDLE_INVALID)
const CIS_HANDLE_INVALID: u16 = 0xFFFF;

/// What the caller wants; modules may change it in
/// `LeGroupHooks::adjust_stream_request` (v1 le_audio::StreamRequest;
/// bt-host's `LeStreamRequest`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRequest {
    /// Preset names to try, best first
    pub preset_order: Vec<String>,
    /// Streaming Audio Contexts in Enable (CONTEXT_*)
    pub contexts: u16,
    /// PHY_*
    pub target_phy: u8,
    /// One ASE carrying both channels (one CIS) when a device can, rather
    /// than one ASE per channel
    pub prefer_two_channel_ase: bool,
    /// Conversational configuration: also a source direction (the headset
    /// microphone) on the devices with source ASEs
    pub conversational: bool,
    /// Presets of the source direction, best first (only with
    /// `conversational`)
    pub source_preset_order: Vec<String>,
    /// Streaming Audio Contexts in the Enable of source ASEs (CONTEXT_*)
    pub source_contexts: u16,
}

impl Default for StreamRequest {
    /// v1's defaults: [`DEFAULT_PRESET_ORDER`], Media, 2M PHY, 2-channel
    /// ASEs; no source direction (when asked for: [`DEFAULT_SOURCE_PRESET_ORDER`],
    /// Conversational)
    fn default() -> Self {
        StreamRequest {
            preset_order: DEFAULT_PRESET_ORDER.iter().map(|s| s.to_string()).collect(),
            contexts: CONTEXT_MEDIA,
            target_phy: PHY_2M,
            prefer_two_channel_ase: true,
            conversational: false,
            source_preset_order: DEFAULT_SOURCE_PRESET_ORDER.iter().map(|s| s.to_string()).collect(),
            source_contexts: CONTEXT_CONVERSATIONAL,
        }
    }
}

/// Where an output channel's audio comes from
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelSource {
    Left,
    Right,
    #[default]
    Mono,
}

/// One LC3 channel of a CIS
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChannelPlan {
    /// Audio location bit, 0 = mono
    pub allocation: u32,
    pub source: ChannelSource,
}

/// The source direction (P->C, the microphone) of a CIS: one source ASE
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceDir {
    pub ase_id: u8,
    /// LC3 channels in the SDU (1: mono)
    pub channels: u8,
    /// Audio location bits of the channels, 0 = mono (no
    /// Audio_Channel_Allocation)
    pub allocation: u32,
    /// Per codec frame and channel (the source preset's)
    pub octets: u16,
}

impl SourceDir {
    /// SDU length: `octets` per channel
    pub fn sdu_len(&self) -> u16 {
        self.octets * u16::from(self.channels)
    }
}

/// One CIS of the plan: a sink ASE (C->P) and / or a source ASE (P->C)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CisPlan {
    /// bt-host's LeDeviceId value
    pub device_id: u32,
    /// The sink ASE; meaningless (0) on a source-only CIS (no `channels`)
    pub ase_id: u8,
    pub cis_id: u8,
    /// ACL handle of the device
    pub acl: u16,
    /// From GAP_SUBEVENT_CIG_CREATED (0xFFFF until then)
    pub cis_handle: u16,
    /// In SDU order (ascending location bit); empty: no sink direction
    pub channels: Vec<ChannelPlan>,
    /// The microphone direction on this CIS
    pub source_ase: Option<SourceDir>,
}

impl CisPlan {
    /// All audio location bits of the channels
    pub fn allocation(&self) -> u32 {
        self.channels.iter().fold(0, |a, c| a | c.allocation)
    }

    /// SDU length: `octets` per channel
    pub fn sdu_len(&self, octets: u16) -> u16 {
        octets * self.channels.len() as u16
    }

    /// Has a sink ASE: the encode thread fills an SDU for it and the sender
    /// sends on it
    pub fn has_sink(&self) -> bool {
        !self.channels.is_empty()
    }
}

/// One planned ASE (sink or source) of a [`StreamPlan`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedAse {
    pub device_id: u32,
    pub ase_id: u8,
    /// Index into [`StreamPlan::cis`]
    pub cis: usize,
    pub sink: bool,
}

/// The source direction of a [`StreamPlan`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePlan {
    pub preset: StreamPreset,
    /// StreamRequest::source_contexts
    pub requested_contexts: u16,
    /// Streaming Audio Contexts in the source ASEs' Enable (0: none
    /// available yet)
    pub contexts: u16,
    /// Finalized after Config Codec (clamped into every source ASE's range)
    pub presentation_delay_us: u32,
}

/// The configuration of a unicast stream (v1 struct StreamPlan)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamPlan {
    pub preset: StreamPreset,
    pub cig_id: u8,
    pub phy: u8,
    /// StreamRequest::contexts
    pub requested_contexts: u16,
    /// Streaming Audio Contexts sent in Enable ([`choose_streaming_contexts`];
    /// 0 = none available yet, the BAP procedure waits for one before Enable)
    pub contexts: u16,
    /// Finalized after Config Codec (clamped into every ASE's range)
    pub presentation_delay_us: u32,
    pub cis: Vec<CisPlan>,
    /// The microphone direction (conversational configuration)
    pub source: Option<SourcePlan>,
    /// Why a requested source direction is missing (empty otherwise)
    pub source_note: String,
}

impl StreamPlan {
    /// LC3 channels of all CIS
    pub fn channel_count(&self) -> usize {
        self.cis.iter().map(|c| c.channels.len()).sum()
    }

    /// The CIS with a sink direction, in plan order: the encode thread's
    /// SDU set has one SDU for each, the ISO sender sends on them
    pub fn sink_cis(&self) -> impl Iterator<Item = &CisPlan> {
        self.cis.iter().filter(|c| c.has_sink())
    }

    /// The CIS with a source direction, in plan order
    pub fn source_cis(&self) -> impl Iterator<Item = &CisPlan> {
        self.cis.iter().filter(|c| c.source_ase.is_some())
    }

    /// Every planned ASE: per CIS its sink ASE, then its source ASE
    pub fn ases(&self) -> Vec<PlannedAse> {
        let mut v = Vec::with_capacity(self.cis.len() * 2);
        for (i, c) in self.cis.iter().enumerate() {
            if c.has_sink() {
                v.push(PlannedAse { device_id: c.device_id, ase_id: c.ase_id, cis: i, sink: true });
            }
            if let Some(s) = c.source_ase {
                v.push(PlannedAse { device_id: c.device_id, ase_id: s.ase_id, cis: i, sink: false });
            }
        }
        v
    }

    /// Is this ASE of this device in the plan?
    pub fn has_ase(&self, device_id: u32, ase_id: u8) -> bool {
        self.ases().iter().any(|a| a.device_id == device_id && a.ase_id == ase_id)
    }

    /// The source direction for the log and the UI, e.g. "microphone: LC3 32
    /// kHz 10 ms 80 octets (64 kbps), 1 ch over 1 CIS, 32_2_1"; None without
    /// one
    pub fn describe_source(&self) -> Option<String> {
        let s = self.source.as_ref()?;
        let p = &s.preset;
        let channels: usize = self.source_cis().map(|c| c.source_ase.map_or(0, |d| usize::from(d.channels))).sum();
        Some(format!(
            "microphone: LC3 {} kHz {} ms {} octets ({} kbps), {} ch over {} CIS, {}",
            p.sampling_hz / 1000,
            if p.frame_us == 7500 { "7.5" } else { "10" },
            p.octets,
            p.kbps_per_channel(),
            channels,
            self.source_cis().count(),
            p.name
        ))
    }

    /// One line for the log and the UI, e.g. "LC3 48 kHz 10 ms 155 octets
    /// (124 kbps/ch), 2 ch over 1 CIS, 48_6_2" (v1 StreamPlan::describe;
    /// the emulator tests read it after "Streaming LE Audio:")
    pub fn describe(&self) -> String {
        let p = &self.preset;
        format!(
            "LC3 {} kHz {} ms {} octets ({} kbps/ch), {} ch over {} CIS, {}",
            p.sampling_hz / 1000,
            if p.frame_us == 7500 { "7.5" } else { "10" },
            p.octets,
            p.kbps_per_channel(),
            self.channel_count(),
            self.cis.len(),
            p.name
        )
    }
}

/// What planning needs of one connected, ready device (a copy of v1
/// LeAudioDevice's PACS / ASCS part)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanDevice {
    /// bt-host's LeDeviceId value
    pub id: u32,
    /// For the error text: the device name, or its address when it has
    /// none (v1 used `name.empty() ? addr.str() : name`)
    pub name: String,
    pub acl: u16,
    pub sink_pacs: Vec<PacRecord>,
    pub sink_locations: u32,
    pub available_sink_contexts: u16,
    pub source_pacs: Vec<PacRecord>,
    pub source_locations: u32,
    pub available_source_contexts: u16,
    /// All ASEs (sink and source; source ASEs only in a conversational plan)
    pub ases: Vec<Ase>,
}

/// The controller's ISO buffers (LE Read Buffer Size v2)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoLimits {
    /// ISO_Data_Packet_Length
    pub packet_length: u16,
    /// Total_Num_ISO_Data_Packets
    pub packet_count: u8,
}

/// Picks the preset and the ASE / channel allocation for the devices (v1
/// plan_stream). Err with the reason (for the log / error text) when no
/// common configuration exists.
pub fn plan_stream(
    devices: &[PlanDevice],
    request: &StreamRequest,
    presets: &PresetTable,
    iso: IsoLimits,
) -> Result<StreamPlan, String> {
    if devices.is_empty() {
        return Err("no device".into());
    }

    // The controller's ISO buffers: every SDU must fit one HCI ISO data
    // packet (BTstack would fragment a longer one, and its per-CIS flow
    // control counts SDUs, not packets), and every CIS needs at least one
    let iso_len = iso.packet_length as u32;
    if iso_len == 0 || iso.packet_count == 0 {
        return Err("the Bluetooth adapter reports no ISO data buffers (no LE Audio / CIS support)".into());
    }
    let max_cis = MAX_CIS.min(iso.packet_count as usize);
    tracing::debug!(
        "LE Audio plan: {} device(s), ISO packets {} x {iso_len} bytes (max {max_cis} CIS), preset order {:?}, prefer 2-channel ASE {}",
        devices.len(),
        iso.packet_count,
        request.preset_order,
        request.prefer_two_channel_ase
    );
    let fits = |sdu_len: u32| sdu_len + 4 <= iso_len;

    let mut sink_ases: Vec<Vec<u8>> = Vec::with_capacity(devices.len());
    for d in devices {
        let mut ids: Vec<u8> = d.ases.iter().filter(|a| a.sink).map(|a| a.id).collect();
        ids.sort_unstable();
        // A set member with a microphone only (conversational): a
        // source-only CIS later
        let source_only = request.conversational && d.ases.iter().any(|a| !a.sink);
        if ids.is_empty() && !source_only {
            return Err(format!("{}: no sink ASE (not an LE Audio sink?)", d.name));
        }
        tracing::debug!(
            "LE Audio plan: {} sink ASEs {ids:?}, sink locations 0x{:08X}, available sink contexts 0x{:04X}, {} sink PAC record(s)",
            d.name,
            d.sink_locations,
            d.available_sink_contexts,
            d.sink_pacs.len()
        );
        sink_ases.push(ids);
    }
    if sink_ases.iter().all(|ids| ids.is_empty()) {
        return Err("no device of the group has a sink ASE".into());
    }

    // ASEs and channels of every device for preset p, by audio location;
    // None if a device cannot take p
    let mut too_long = false;
    let mut layout = |p: &StreamPreset| -> Option<Vec<CisPlan>> {
        let mut cis = Vec::new();
        for (d, ids) in devices.iter().zip(&sink_ases) {
            if ids.is_empty() {
                continue;
            }
            let add = |cis: &mut Vec<CisPlan>, ase_id: u8, locs: &[u32]| {
                cis.push(CisPlan {
                    device_id: d.id,
                    ase_id,
                    cis_id: 0,
                    acl: d.acl,
                    cis_handle: CIS_HANDLE_INVALID,
                    channels: locs
                        .iter()
                        .map(|&l| ChannelPlan { allocation: l, source: ChannelSource::Mono })
                        .collect(),
                    source_ase: None,
                });
            };
            let one = device_supports(d, p, 1);
            let one_fits = fits(p.octets as u32);
            too_long |= one && !one_fits;
            let lr = d.sink_locations & (LOCATION_FRONT_LEFT | LOCATION_FRONT_RIGHT);
            if lr.count_ones() == 2 {
                let two_ok = device_supports(d, p, 2);
                let two = two_ok && fits(2 * p.octets as u32);
                let split = one && one_fits && ids.len() >= 2;
                too_long |= two_ok && !two;
                if two && (request.prefer_two_channel_ase || !split) {
                    add(&mut cis, ids[0], &[LOCATION_FRONT_LEFT, LOCATION_FRONT_RIGHT]);
                } else if split {
                    add(&mut cis, ids[0], &[LOCATION_FRONT_LEFT]);
                    add(&mut cis, ids[1], &[LOCATION_FRONT_RIGHT]);
                } else if one && one_fits {
                    // A stereo device that takes one channel only: mono (no
                    // Audio_Channel_Allocation), so that it renders the
                    // downmix rather than playing it on the left only
                    add(&mut cis, ids[0], &[LOCATION_MONO]);
                } else {
                    return None;
                }
            } else if one && one_fits {
                add(&mut cis, ids[0], &[if lr.count_ones() == 1 { lr } else { LOCATION_MONO }]);
            } else {
                return None;
            }
        }
        Some(cis)
    };

    // First preset in the requested order every device supports
    let mut chosen = None;
    let mut too_many = false;
    for name in &request.preset_order {
        let Some(p) = presets.find(name) else {
            tracing::debug!("LE Audio plan: preset {name} unknown, skipped");
            continue;
        };
        let Some(cis) = layout(p) else {
            tracing::debug!("LE Audio plan: preset {name} not supported by every device");
            continue;
        };
        if cis.len() > max_cis {
            tracing::debug!("LE Audio plan: preset {name} needs {} CIS (max {max_cis})", cis.len());
            too_many = true;
            continue;
        }
        chosen = Some((p, cis));
        break;
    }
    let Some((preset, mut cis)) = chosen else {
        return Err(if too_many {
            format!("the group needs more CIS than the adapter can take ({max_cis})")
        } else if too_long {
            format!("no LC3 configuration of the devices fits the adapter's ISO packets ({iso_len} bytes)")
        } else {
            "no LC3 configuration supported by all devices (sink PAC records)".into()
        });
    };

    // Channel sources: left / right only when the group has both
    let has = |loc: u32| cis.iter().any(|c| c.channels.iter().any(|ch| ch.allocation == loc));
    let stereo = has(LOCATION_FRONT_LEFT) && has(LOCATION_FRONT_RIGHT);
    tracing::debug!("LE Audio plan: preset {}, {} CIS, stereo {stereo}", preset.name, cis.len());
    for c in cis.iter_mut() {
        for ch in &mut c.channels {
            ch.source = match ch.allocation {
                LOCATION_FRONT_LEFT if stereo => ChannelSource::Left,
                LOCATION_FRONT_RIGHT if stereo => ChannelSource::Right,
                _ => ChannelSource::Mono,
            };
        }
    }

    // The microphone (conversational configuration)
    let mut source = None;
    let mut source_note = String::new();
    if request.conversational {
        match plan_source(devices, request, presets, preset, iso_len, max_cis, &mut cis) {
            Ok(s) => source = Some(s),
            Err(why) => {
                tracing::warn!("LE Audio plan: no microphone: {why}");
                source_note = why;
            }
        }
    }
    for (i, c) in cis.iter_mut().enumerate() {
        c.cis_id = i as u8;
    }

    // Streaming context: checked again right before Enable (BAP), as the
    // available contexts may change until then. A conversational stream
    // also names Conversational for the sink ASEs (where available).
    let requested = request.contexts | if source.is_some() { CONTEXT_CONVERSATIONAL } else { 0 };
    let available: Vec<u16> = devices
        .iter()
        .zip(&sink_ases)
        .filter(|(_, ids)| !ids.is_empty())
        .map(|(d, _)| d.available_sink_contexts)
        .collect();
    Ok(StreamPlan {
        preset: preset.clone(),
        cig_id: 1,
        phy: request.target_phy,
        requested_contexts: requested,
        contexts: choose_streaming_contexts(&available, requested),
        presentation_delay_us: preset.presentation_delay_us,
        cis,
        source,
        source_note,
    })
}

/// The source direction of a conversational plan: preset, and one source
/// ASE per device with one on its first sink CIS (or a source-only CIS).
/// Err(why) when there is none.
fn plan_source(
    devices: &[PlanDevice],
    request: &StreamRequest,
    presets: &PresetTable,
    sink: &StreamPreset,
    iso_len: u32,
    max_cis: usize,
    cis: &mut Vec<CisPlan>,
) -> Result<SourcePlan, String> {
    // The first source ASE of every device that has one
    let mics: Vec<(&PlanDevice, u8)> = devices
        .iter()
        .filter_map(|d| d.ases.iter().filter(|a| !a.sink).map(|a| a.id).min().map(|id| (d, id)))
        .collect();
    if mics.is_empty() {
        return Err("no device of the group has a source ASE".into());
    }
    // The order asked for, those with the sink's frame duration first (one
    // ISO interval for both directions)
    let mut candidates: Vec<&StreamPreset> =
        request.source_preset_order.iter().filter_map(|n| presets.find(n)).collect();
    candidates.sort_by_key(|p| p.frame_us != sink.frame_us);
    let preset = candidates
        .into_iter()
        .find(|p| u32::from(p.octets) + 4 <= iso_len && mics.iter().all(|(d, _)| pacs_support(&d.source_pacs, p, 1)))
        .ok_or_else(|| "no microphone configuration supported by every device (source PAC records)".to_string())?;

    let mut extra = 0;
    for (d, ase_id) in &mics {
        let allocation = if d.source_locations.count_ones() == 1 { d.source_locations } else { LOCATION_MONO };
        let dir = SourceDir { ase_id: *ase_id, channels: 1, allocation, octets: preset.octets };
        match cis.iter_mut().find(|c| c.device_id == d.id) {
            Some(c) => c.source_ase = Some(dir),
            None => {
                extra += 1;
                cis.push(CisPlan {
                    device_id: d.id,
                    ase_id: 0,
                    cis_id: 0,
                    acl: d.acl,
                    cis_handle: CIS_HANDLE_INVALID,
                    channels: Vec::new(),
                    source_ase: Some(dir),
                })
            }
        }
    }
    if cis.len() > max_cis {
        cis.retain(|c| c.has_sink());
        for c in cis.iter_mut() {
            c.source_ase = None;
        }
        return Err(format!("{extra} more CIS for the microphone than the adapter can take ({max_cis})"));
    }
    let available: Vec<u16> = mics.iter().map(|(d, _)| d.available_source_contexts).collect();
    tracing::debug!(
        "LE Audio plan: microphone preset {}, source ASEs {:?}",
        preset.name,
        mics.iter().map(|(d, a)| (d.id, *a)).collect::<Vec<_>>()
    );
    Ok(SourcePlan {
        preset: preset.clone(),
        requested_contexts: request.source_contexts,
        contexts: choose_streaming_contexts(&available, request.source_contexts),
        presentation_delay_us: preset.presentation_delay_us,
    })
}

/// Does one of the device's LC3 sink PAC records allow this preset with the
/// given number of channels per ASE? Audio_Channel_Counts says how many
/// channels an ASE may carry. Supported_Max_Codec_Frames_Per_SDU (default 1
/// when absent) counts codec frames per channel in one SDU, i.e. it limits
/// Codec_Frame_Blocks_Per_SDU (always 1 here), not the channel count.
fn device_supports(d: &PlanDevice, p: &StreamPreset, channels_per_ase: u32) -> bool {
    pacs_support(&d.sink_pacs, p, channels_per_ase)
}

/// As [`device_supports`] for any PAC records (sink or source)
fn pacs_support(pacs: &[PacRecord], p: &StreamPreset, channels_per_ase: u32) -> bool {
    let sf_bit = sf_hz_to_cap_bit(p.sampling_hz);
    let fd_bit = if p.frame_us == 7500 { FD_BIT_7500US } else { FD_BIT_10000US };
    pacs.iter().any(|r| {
        r.lc3
            && r.caps.sampling_frequencies & sf_bit != 0
            && r.caps.frame_durations & fd_bit != 0
            && (r.caps.max_octets == 0 || (r.caps.min_octets..=r.caps.max_octets).contains(&p.octets))
            && (r.caps.channel_counts as u32) & (1 << (channels_per_ase - 1)) != 0
    })
}

/// Streaming Audio Contexts for Enable from each device's Available Sink
/// Contexts: the requested contexts available on every device, else
/// Unspecified if that is available on every device, else 0 (none: Enable
/// must wait)
pub fn choose_streaming_contexts(available: &[u16], requested: u16) -> u16 {
    // BAP 1.0.1 5.6.3 / PACS: Enable may only name contexts that are
    // available on the server; Unspecified when the requested ones are not
    // (if Unspecified is available itself)
    let mut common = requested;
    let mut unspecified = CONTEXT_UNSPECIFIED;
    for &a in available {
        common &= a;
        unspecified &= a;
    }
    let chosen = if common != 0 { common } else { unspecified };
    tracing::debug!("LE Audio contexts: requested 0x{requested:04X}, available {available:04X?} -> 0x{chosen:04X}");
    chosen
}

/// Codec Specific Configuration LTVs of an ASE for the plan (sampling
/// frequency, frame duration, channel allocation unless mono, octets per
/// frame, one frame block per SDU)
pub fn codec_config_ltvs(plan: &StreamPlan, cis: &CisPlan) -> Vec<u8> {
    let p = &plan.preset;
    let mut out = Vec::new();
    ltv::put(&mut out, CFG_SAMPLING_FREQUENCY, &[sf_hz_to_code(p.sampling_hz)]);
    ltv::put(&mut out, CFG_FRAME_DURATION, &[if p.frame_us == 7500 { 0x00 } else { 0x01 }]);
    let alloc = cis.allocation();
    if alloc != 0 {
        ltv::put(&mut out, CFG_CHANNEL_ALLOCATION, &alloc.to_le_bytes());
    }
    ltv::put(&mut out, CFG_OCTETS_PER_FRAME, &p.octets.to_le_bytes());
    ltv::put(&mut out, CFG_FRAME_BLOCKS_PER_SDU, &[1]);
    out
}

/// Codec Specific Configuration LTVs of a source ASE (as
/// [`codec_config_ltvs`], with the source preset); empty without a source
/// plan
pub fn source_codec_config_ltvs(plan: &StreamPlan, dir: &SourceDir) -> Vec<u8> {
    let Some(s) = &plan.source else { return Vec::new() };
    let p = &s.preset;
    let mut out = Vec::new();
    ltv::put(&mut out, CFG_SAMPLING_FREQUENCY, &[sf_hz_to_code(p.sampling_hz)]);
    ltv::put(&mut out, CFG_FRAME_DURATION, &[if p.frame_us == 7500 { 0x00 } else { 0x01 }]);
    if dir.allocation != 0 {
        ltv::put(&mut out, CFG_CHANNEL_ALLOCATION, &dir.allocation.to_le_bytes());
    }
    ltv::put(&mut out, CFG_OCTETS_PER_FRAME, &dir.octets.to_le_bytes());
    ltv::put(&mut out, CFG_FRAME_BLOCKS_PER_SDU, &[1]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defs::*;
    use crate::pacs_ascs::{ConfigQos, cp_config_qos, parse_pac_records};

    /// The emulator's sink PAC record (tools/emu/le_audio/bap.py): LC3
    /// 16/24/32/48 kHz, 7.5 + 10 ms, 26-155 octets, `channels` (bit n =
    /// n+1 channels)
    fn emu_pacs(channels: u8) -> Vec<PacRecord> {
        let mut caps = Vec::new();
        ltv::put(&mut caps, CAP_SAMPLING_FREQUENCIES, &0x00B4u16.to_le_bytes());
        ltv::put(&mut caps, CAP_FRAME_DURATIONS, &[0x03]);
        ltv::put(&mut caps, CAP_CHANNEL_COUNTS, &[channels]);
        ltv::put(&mut caps, CAP_OCTETS_PER_FRAME, &[26, 0, 155, 0]);
        ltv::put(&mut caps, CAP_MAX_FRAMES_PER_SDU, &[1]);
        let mut v = vec![1, CODING_FORMAT_LC3, 0, 0, 0, 0, caps.len() as u8];
        v.extend_from_slice(&caps);
        v.push(0);
        parse_pac_records(&v).unwrap()
    }

    const CONTEXTS: u16 = CONTEXT_MEDIA | CONTEXT_UNSPECIFIED | CONTEXT_CONVERSATIONAL | CONTEXT_GAME;

    /// A sink of emu_le_audio_sink.py's LAYOUTS: (locations, sink ASEs,
    /// channel counts)
    fn sink(id: u32, locations: u32, ases: u8, channels: u8) -> PlanDevice {
        PlanDevice {
            id,
            name: format!("sink {id}"),
            acl: 0x40 + id as u16,
            sink_pacs: emu_pacs(channels),
            sink_locations: locations,
            available_sink_contexts: CONTEXTS,
            ases: (1..=ases).map(|i| Ase { id: i, ..Ase::default() }).collect(),
            ..PlanDevice::default()
        }
    }

    /// The emulator's microphone (emu_le_audio_sink.py --mic): Source PAC
    /// LC3 16 + 32 kHz, 10 ms, 1 channel, 40-80 octets; source ASE 3;
    /// Conversational available
    fn with_mic(mut d: PlanDevice, location: u32) -> PlanDevice {
        let mut pacs = emu_pacs(0x01);
        pacs[0].caps.sampling_frequencies = SF_BIT_16000 | SF_BIT_32000;
        pacs[0].caps.frame_durations = FD_BIT_10000US;
        pacs[0].caps.min_octets = 40;
        pacs[0].caps.max_octets = 80;
        d.source_pacs = pacs;
        d.source_locations = location;
        d.available_source_contexts = CONTEXT_CONVERSATIONAL | CONTEXT_UNSPECIFIED;
        d.ases.push(Ase { id: 3, sink: false, ..Ase::default() });
        d
    }

    fn conversational() -> StreamRequest {
        StreamRequest { conversational: true, ..StreamRequest::default() }
    }

    const FL: u32 = LOCATION_FRONT_LEFT;
    const FR: u32 = LOCATION_FRONT_RIGHT;
    /// Bumble's virtual controller (LE Read Buffer Size v2)
    const EMU_ISO: IsoLimits = IsoLimits { packet_length: 960, packet_count: 64 };

    fn plan(devices: &[PlanDevice], request: &StreamRequest, iso: IsoLimits) -> Result<StreamPlan, String> {
        plan_stream(devices, request, &PresetTable::new(), iso)
    }

    /// (device, ASE, CIS id, [(allocation, source)]) of every CIS
    type Layout = Vec<(u32, u8, u8, Vec<(u32, ChannelSource)>)>;

    fn layout(p: &StreamPlan) -> Layout {
        p.cis
            .iter()
            .map(|c| {
                (c.device_id, c.ase_id, c.cis_id, c.channels.iter().map(|ch| (ch.allocation, ch.source)).collect())
            })
            .collect()
    }

    use ChannelSource::{Left, Mono, Right};

    #[test]
    fn stereo_one_two_channel_ase() {
        // 'stereo': FL + FR, 2 sink ASEs, 1 or 2 channels per ASE
        let p = plan(&[sink(1, FL | FR, 2, 0x03)], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(p.preset.name, "48_6_2");
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(FL, Left), (FR, Right)])]);
        assert_eq!((p.cig_id, p.phy, p.presentation_delay_us), (1, PHY_2M, 40000));
        assert_eq!((p.requested_contexts, p.contexts), (CONTEXT_MEDIA, CONTEXT_MEDIA));
        assert_eq!(p.cis[0].acl, 0x41);
        assert_eq!(p.cis[0].cis_handle, 0xFFFF);
        assert_eq!(p.cis[0].allocation(), 0x03);
        assert_eq!(p.cis[0].sdu_len(p.preset.octets), 310);
        assert_eq!(p.channel_count(), 2);
        assert_eq!(p.describe(), "LC3 48 kHz 10 ms 155 octets (124 kbps/ch), 2 ch over 1 CIS, 48_6_2");

        // Not preferred: one ASE per channel
        let request = StreamRequest { prefer_two_channel_ase: false, ..StreamRequest::default() };
        let p = plan(&[sink(1, FL | FR, 2, 0x03)], &request, EMU_ISO).unwrap();
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(FL, Left)]), (1, 2, 1, vec![(FR, Right)])]);
    }

    #[test]
    fn two_ase() {
        // 'two-ase': FL + FR, 2 sink ASEs, 1 channel per ASE
        let p = plan(&[sink(1, FL | FR, 2, 0x01)], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(p.preset.name, "48_6_2");
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(FL, Left)]), (1, 2, 1, vec![(FR, Right)])]);
        assert_eq!(p.describe(), "LC3 48 kHz 10 ms 155 octets (124 kbps/ch), 2 ch over 2 CIS, 48_6_2");
    }

    #[test]
    fn earbuds() {
        // 'earbuds': two devices, FL and FR, one ASE each
        let devices = [sink(1, FL, 1, 0x01), sink(2, FR, 1, 0x01)];
        let p = plan(&devices, &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(p.preset.name, "48_6_2");
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(FL, Left)]), (2, 1, 1, vec![(FR, Right)])]);
        assert_eq!((p.cis[0].acl, p.cis[1].acl), (0x41, 0x42));
    }

    #[test]
    fn missing_earbud_gets_the_downmix() {
        // Only the left earbud: its channel carries the downmix
        let p = plan(&[sink(1, FL, 1, 0x01)], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(FL, Mono)])]);
        assert_eq!(codec_config_ltvs(&p, &p.cis[0]), [2, 1, 8, 2, 2, 1, 5, 3, 1, 0, 0, 0, 3, 4, 155, 0, 2, 5, 1]);
    }

    #[test]
    fn mono_and_no_location() {
        // 'mono' (no location) with --le-preset 16_2_1
        let request = StreamRequest { preset_order: vec!["16_2_1".into()], ..StreamRequest::default() };
        let p = plan(&[sink(1, 0, 1, 0x01)], &request, EMU_ISO).unwrap();
        assert_eq!(p.preset.name, "16_2_1");
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(LOCATION_MONO, Mono)])]);
        assert_eq!(p.describe(), "LC3 16 kHz 10 ms 40 octets (32 kbps/ch), 1 ch over 1 CIS, 16_2_1");
        // No Audio_Channel_Allocation for a mono ASE
        assert_eq!(codec_config_ltvs(&p, &p.cis[0]), [2, 1, 3, 2, 2, 1, 3, 4, 40, 0, 2, 5, 1]);

        // No location, but 2 channels per ASE: still one mono channel
        let p = plan(&[sink(1, 0, 2, 0x03)], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(LOCATION_MONO, Mono)])]);
        // Front Left + Right, but one 1-channel ASE: mono (renders the downmix)
        let p = plan(&[sink(1, FL | FR, 1, 0x01)], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(LOCATION_MONO, Mono)])]);
        // A location other than FL / FR counts as none
        let p = plan(&[sink(1, 0x0000_0004, 1, 0x01)], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(LOCATION_MONO, Mono)])]);
    }

    #[test]
    fn sdu_too_large_for_the_iso_packet() {
        // 2 x 155 + 4 > 251: two 1-channel ASEs instead
        let iso = IsoLimits { packet_length: 251, packet_count: 8 };
        let p = plan(&[sink(1, FL | FR, 2, 0x03)], &StreamRequest::default(), iso).unwrap();
        assert_eq!(p.preset.name, "48_6_2");
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(FL, Left)]), (1, 2, 1, vec![(FR, Right)])]);
        // Only one ASE: one mono channel
        let p = plan(&[sink(1, FL | FR, 1, 0x03)], &StreamRequest::default(), iso).unwrap();
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(LOCATION_MONO, Mono)])]);
        // 155 + 4 > 158: the next preset (48_4_2, 120 octets) fits
        let iso = IsoLimits { packet_length: 158, packet_count: 8 };
        let p = plan(&[sink(1, FL, 1, 0x01)], &StreamRequest::default(), iso).unwrap();
        assert_eq!(p.preset.name, "48_4_2");
        // Nothing fits
        let iso = IsoLimits { packet_length: 40, packet_count: 8 };
        assert_eq!(
            plan(&[sink(1, FL | FR, 2, 0x03)], &StreamRequest::default(), iso),
            Err("no LC3 configuration of the devices fits the adapter's ISO packets (40 bytes)".into())
        );
    }

    #[test]
    fn more_cis_than_possible() {
        // Three stereo devices with one ASE per channel: 6 CIS > 4
        let request = StreamRequest { prefer_two_channel_ase: false, ..StreamRequest::default() };
        let devices = [sink(1, FL | FR, 2, 0x01), sink(2, FL | FR, 2, 0x01), sink(3, FL | FR, 2, 0x01)];
        assert_eq!(
            plan(&devices, &request, EMU_ISO),
            Err("the group needs more CIS than the adapter can take (4)".into())
        );
        // Two of them: 4 CIS, the most there may be
        let p = plan(&devices[..2], &request, EMU_ISO).unwrap();
        assert_eq!(p.cis.len(), 4);
        assert_eq!(p.cis.iter().map(|c| c.cis_id).collect::<Vec<_>>(), [0, 1, 2, 3]);
        // Fewer ISO buffers than CIS
        let iso = IsoLimits { packet_length: 960, packet_count: 1 };
        assert_eq!(
            plan(&[sink(1, FL | FR, 2, 0x01)], &StreamRequest::default(), iso),
            Err("the group needs more CIS than the adapter can take (1)".into())
        );
    }

    #[test]
    fn refusals() {
        let request = StreamRequest::default();
        assert_eq!(plan(&[], &request, EMU_ISO), Err("no device".into()));
        assert_eq!(
            plan(&[sink(1, FL | FR, 2, 0x03)], &request, IsoLimits { packet_length: 0, packet_count: 0 }),
            Err("the Bluetooth adapter reports no ISO data buffers (no LE Audio / CIS support)".into())
        );
        let mut no_ase = sink(2, FL, 1, 0x01);
        no_ase.ases[0].sink = false;
        no_ase.name = "AA:BB:CC:DD:EE:FF".into();
        assert_eq!(
            plan(&[sink(1, FL, 1, 0x01), no_ase], &request, EMU_ISO),
            Err("AA:BB:CC:DD:EE:FF: no sink ASE (not an LE Audio sink?)".into())
        );
        // 44.1 kHz only
        let mut odd = sink(1, FL, 1, 0x01);
        odd.sink_pacs[0].caps.sampling_frequencies = SF_BIT_44100;
        assert_eq!(
            plan(&[odd], &request, EMU_ISO),
            Err("no LC3 configuration supported by all devices (sink PAC records)".into())
        );
        // Unknown preset names are skipped
        let request = StreamRequest { preset_order: vec!["nope".into(), "24_2_2".into()], ..request };
        assert_eq!(plan(&[sink(1, FL, 1, 0x01)], &request, EMU_ISO).unwrap().preset.name, "24_2_2");
    }

    #[test]
    fn octet_range_and_frame_duration() {
        // 7.5 ms not supported: the 10 ms 48_2_gr after 48_3_gr
        let mut d = sink(1, FL | FR, 1, 0x03);
        d.sink_pacs[0].caps.frame_durations = FD_BIT_10000US;
        let request = StreamRequest {
            preset_order: vec!["48_3_gr".into(), "48_2_gr".into()],
            contexts: CONTEXT_GAME,
            target_phy: PHY_2M,
            prefer_two_channel_ase: true,
            ..StreamRequest::default()
        };
        let p = plan(&[d.clone()], &request, EMU_ISO).unwrap();
        assert_eq!(p.preset.name, "48_2_gr");
        assert_eq!(p.presentation_delay_us, 10000);
        // Octets above the PAC's maximum
        d.sink_pacs[0].caps.max_octets = 120;
        let p = plan(&[d.clone()], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(p.preset.name, "48_4_2");
        // max_octets 0: no octet limit
        d.sink_pacs[0].caps.max_octets = 0;
        d.sink_pacs[0].caps.min_octets = 200;
        assert_eq!(plan(&[d], &StreamRequest::default(), EMU_ISO).unwrap().preset.name, "48_6_2");
    }

    #[test]
    fn streaming_contexts() {
        let m = CONTEXT_MEDIA;
        let u = CONTEXT_UNSPECIFIED;
        let c = CONTEXT_CONVERSATIONAL;
        let g = CONTEXT_GAME;
        assert_eq!(choose_streaming_contexts(&[m | u, m], m), m);
        assert_eq!(choose_streaming_contexts(&[m | u | g], g | m), g | m);
        assert_eq!(choose_streaming_contexts(&[m | u, m | g], g | m), m);
        // Game not available: Unspecified if every device has it
        assert_eq!(choose_streaming_contexts(&[m | u, u], g), u);
        assert_eq!(choose_streaming_contexts(&[m | u, m], g), 0);
        assert_eq!(choose_streaming_contexts(&[m, c], m), 0);
        assert_eq!(choose_streaming_contexts(&[0], m), 0);
        assert_eq!(choose_streaming_contexts(&[], m), m);

        // In the plan
        let mut d = sink(1, FL, 1, 0x01);
        d.available_sink_contexts = u | c;
        let p = plan(&[d.clone()], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!((p.requested_contexts, p.contexts), (m, u));
        d.available_sink_contexts = 0;
        let p = plan(&[d], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(p.contexts, 0);
    }

    /// Config QoS as bt-host's BAP procedure builds it from the plan (v1
    /// Unicast::set_phase ConfigQos, QoS within the server's preferences)
    fn qos(p: &StreamPlan, c: &CisPlan, framing: u8, latency_ms: u16) -> ConfigQos {
        ConfigQos {
            ase_id: c.ase_id,
            cig_id: p.cig_id,
            cis_id: c.cis_id,
            sdu_interval_us: p.preset.sdu_interval_us(),
            framing,
            phy: p.phy,
            max_sdu: c.sdu_len(p.preset.octets),
            rtn: p.preset.rtn,
            max_transport_latency_ms: latency_ms,
            presentation_delay_us: p.presentation_delay_us,
        }
    }

    /// Golden bytes, derived by hand from v1's codec_config_ltvs() and
    /// Unicast's Config Codec / Config QoS PDUs (le_audio_bap.cpp)
    #[test]
    fn golden_codec_config_and_qos() {
        // stereo: 48_6_2, one 2-channel ASE
        let p = plan(&[sink(1, FL | FR, 2, 0x03)], &StreamRequest::default(), EMU_ISO).unwrap();
        let cfg = codec_config_ltvs(&p, &p.cis[0]);
        assert_eq!(
            cfg,
            [
                0x02, 0x01, 0x08, // Sampling_Frequency 48 kHz
                0x02, 0x02, 0x01, // Frame_Duration 10 ms
                0x05, 0x03, 0x03, 0x00, 0x00, 0x00, // Audio_Channel_Allocation FL | FR
                0x03, 0x04, 0x9B, 0x00, // Octets_Per_Codec_Frame 155
                0x02, 0x05, 0x01, // Codec_Frame_Blocks_Per_SDU 1
            ]
        );
        let pdu = crate::pacs_ascs::cp_config_codec(&[crate::pacs_ascs::ConfigCodec {
            ase_id: p.cis[0].ase_id,
            target_latency: p.preset.target_latency,
            target_phy: p.phy,
            codec_id: [CODING_FORMAT_LC3, 0, 0, 0, 0],
            config: cfg,
        }]);
        assert_eq!(&pdu[..11], [0x01, 0x01, 0x01, 0x03, 0x02, 0x06, 0, 0, 0, 0, 19]);
        assert_eq!(
            cp_config_qos(&[qos(&p, &p.cis[0], 0, p.preset.max_transport_latency_ms)]),
            [
                0x02, 0x01, // Config QoS, 1 ASE
                0x01, 0x01, 0x00, // ASE 1, CIG 1, CIS 0
                0x10, 0x27, 0x00, // SDU interval 10000 us
                0x00, 0x02, // unframed, 2M PHY
                0x36, 0x01, // max SDU 310
                0x0D, // RTN 13
                0x64, 0x00, // max transport latency 100 ms
                0x40, 0x9C, 0x00, // presentation delay 40000 us
            ]
        );

        // earbuds with GMAP 48_3_gr (tools/emu/le_audio_tests/tmap_gmap.py:
        // "SDU interval 7500 us, max SDU 90, RTN 1, latency 15 ms,
        // presentation delay 10000 us")
        let request =
            StreamRequest { preset_order: vec!["48_3_gr".into()], contexts: CONTEXT_GAME, ..StreamRequest::default() };
        let p = plan(&[sink(1, FL, 1, 0x01), sink(2, FR, 1, 0x01)], &request, EMU_ISO).unwrap();
        assert_eq!(p.describe(), "LC3 48 kHz 7.5 ms 90 octets (96 kbps/ch), 2 ch over 2 CIS, 48_3_gr");
        assert_eq!(p.preset.target_latency, TARGET_LATENCY_LOW);
        assert_eq!(codec_config_ltvs(&p, &p.cis[0]), [2, 1, 8, 2, 2, 0, 5, 3, 1, 0, 0, 0, 3, 4, 90, 0, 2, 5, 1]);
        assert_eq!(codec_config_ltvs(&p, &p.cis[1]), [2, 1, 8, 2, 2, 0, 5, 3, 2, 0, 0, 0, 3, 4, 90, 0, 2, 5, 1]);
        assert_eq!(
            cp_config_qos(&[qos(&p, &p.cis[1], 0, p.preset.max_transport_latency_ms)]),
            [0x02, 0x01, 0x01, 0x01, 0x01, 0x4C, 0x1D, 0x00, 0x00, 0x02, 90, 0x00, 0x01, 15, 0x00, 0x10, 0x27, 0x00]
        );
    }

    #[test]
    fn no_source_direction_unless_asked() {
        let p = plan(&[with_mic(sink(1, FL | FR, 2, 0x03), FL)], &StreamRequest::default(), EMU_ISO).unwrap();
        assert_eq!(p.source, None);
        assert_eq!(p.source_note, "");
        assert!(p.cis.iter().all(|c| c.source_ase.is_none()));
        assert_eq!(p.ases().len(), 1);
        assert_eq!(p.describe_source(), None);
    }

    #[test]
    fn conversational_headset() {
        // 'mic-stereo': one 2-channel sink ASE, the mic on the same CIS
        let p = plan(&[with_mic(sink(1, FL | FR, 2, 0x03), FL)], &conversational(), EMU_ISO).unwrap();
        assert_eq!(p.preset.name, "48_6_2");
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(FL, Left), (FR, Right)])]);
        let s = p.source.as_ref().unwrap();
        assert_eq!(s.preset.name, "32_2_1");
        assert_eq!((s.requested_contexts, s.contexts), (CONTEXT_CONVERSATIONAL, CONTEXT_CONVERSATIONAL));
        assert_eq!(s.presentation_delay_us, 40000);
        assert_eq!(p.cis[0].source_ase, Some(SourceDir { ase_id: 3, channels: 1, allocation: FL, octets: 80 }));
        assert_eq!(p.cis[0].source_ase.unwrap().sdu_len(), 80);
        // Conversational added to the sink contexts (the device has it)
        assert_eq!((p.requested_contexts, p.contexts), (CONTEXT_MEDIA | CONTEXT_CONVERSATIONAL, 0x0006));
        assert_eq!(
            p.ases(),
            [
                PlannedAse { device_id: 1, ase_id: 1, cis: 0, sink: true },
                PlannedAse { device_id: 1, ase_id: 3, cis: 0, sink: false }
            ]
        );
        assert!(p.has_ase(1, 3) && !p.has_ase(1, 2) && !p.has_ase(2, 3));
        assert_eq!(p.sink_cis().count(), 1);
        assert_eq!(p.source_cis().count(), 1);
        assert_eq!(p.describe(), "LC3 48 kHz 10 ms 155 octets (124 kbps/ch), 2 ch over 1 CIS, 48_6_2");
        assert_eq!(
            p.describe_source().unwrap(),
            "microphone: LC3 32 kHz 10 ms 80 octets (64 kbps), 1 ch over 1 CIS, 32_2_1"
        );
        assert_eq!(
            source_codec_config_ltvs(&p, &p.cis[0].source_ase.unwrap()),
            [2, 1, 6, 2, 2, 1, 5, 3, 1, 0, 0, 0, 3, 4, 80, 0, 2, 5, 1]
        );
        // A microphone without a single location: mono
        let p = plan(&[with_mic(sink(1, FL | FR, 2, 0x03), FL | FR)], &conversational(), EMU_ISO).unwrap();
        let dir = p.cis[0].source_ase.unwrap();
        assert_eq!(dir.allocation, LOCATION_MONO);
        assert_eq!(source_codec_config_ltvs(&p, &dir), [2, 1, 6, 2, 2, 1, 3, 4, 80, 0, 2, 5, 1]);
    }

    #[test]
    fn conversational_source_presets() {
        // 16 kHz only: 16_2_1 ('mic-mono-16k')
        let mut d = with_mic(sink(1, 0, 1, 0x01), 0);
        d.source_pacs[0].caps.sampling_frequencies = SF_BIT_16000;
        let request = StreamRequest { preset_order: vec!["16_2_1".into()], ..conversational() };
        let p = plan(&[d.clone()], &request, EMU_ISO).unwrap();
        assert_eq!(p.source.as_ref().unwrap().preset.name, "16_2_1");
        assert_eq!(p.cis[0].source_ase.unwrap().octets, 40);
        // A 7.5 ms sink preset prefers a 7.5 ms microphone preset when the
        // device has one
        let mut d2 = with_mic(sink(1, FL | FR, 2, 0x03), FL);
        d2.source_pacs[0].caps.frame_durations = FD_BIT_7500US | FD_BIT_10000US;
        d2.source_pacs[0].caps.min_octets = 30;
        let request = StreamRequest { preset_order: vec!["48_3_gr".into()], ..conversational() };
        let p = plan(&[d2.clone()], &request, EMU_ISO).unwrap();
        assert_eq!(p.source.as_ref().unwrap().preset.name, "32_1_1");
        // The GMAP order
        let request = StreamRequest {
            preset_order: vec!["48_3_gr".into()],
            source_preset_order: vec!["32_1_gs".into(), "32_2_gs".into()],
            ..conversational()
        };
        let s = plan(&[d2], &request, EMU_ISO).unwrap().source.unwrap();
        assert_eq!((s.preset.name.as_str(), s.presentation_delay_us, s.preset.rtn), ("32_1_gs", 60000, 1));
        // Nothing the Source PAC allows: no microphone, playback stays
        d.source_pacs[0].caps.sampling_frequencies = SF_BIT_48000;
        let p = plan(&[d.clone()], &conversational(), EMU_ISO).unwrap();
        assert_eq!(p.source, None);
        assert_eq!(p.source_note, "no microphone configuration supported by every device (source PAC records)");
        assert!(p.cis.iter().all(|c| c.source_ase.is_none()));
        assert_eq!(p.requested_contexts, CONTEXT_MEDIA);
        // No source ASE at all
        let p = plan(&[sink(1, FL | FR, 2, 0x03)], &conversational(), EMU_ISO).unwrap();
        assert_eq!(p.source_note, "no device of the group has a source ASE");
    }

    #[test]
    fn conversational_earbuds() {
        // 'mic-earbuds': the left earbud has the microphone
        let devices = [with_mic(sink(1, FL, 1, 0x01), FL), sink(2, FR, 1, 0x01)];
        let p = plan(&devices, &conversational(), EMU_ISO).unwrap();
        assert_eq!(layout(&p), vec![(1, 1, 0, vec![(FL, Left)]), (2, 1, 1, vec![(FR, Right)])]);
        assert_eq!(p.cis[0].source_ase.map(|s| s.ase_id), Some(3));
        assert_eq!(p.cis[1].source_ase, None);
        // Sink contexts: Conversational only where every sink has it
        let mut right = sink(2, FR, 1, 0x01);
        right.available_sink_contexts = CONTEXT_MEDIA;
        let p = plan(&[devices[0].clone(), right], &conversational(), EMU_ISO).unwrap();
        assert_eq!(p.contexts, CONTEXT_MEDIA);
        // Both with a microphone: one source per earbud
        let devices = [with_mic(sink(1, FL, 1, 0x01), FL), with_mic(sink(2, FR, 1, 0x01), FR)];
        let p = plan(&devices, &conversational(), EMU_ISO).unwrap();
        assert_eq!(p.source_cis().count(), 2);
        assert_eq!(p.cis[1].source_ase.unwrap().allocation, FR);
        assert_eq!(
            p.describe_source().unwrap(),
            "microphone: LC3 32 kHz 10 ms 80 octets (64 kbps), 2 ch over 2 CIS, 32_2_1"
        );
    }

    #[test]
    fn source_only_member() {
        // A set member with a microphone and no sink ASE: its own CIS, P->C
        // only
        let mut mic = with_mic(sink(2, 0, 0, 0x01), FL);
        mic.ases.retain(|a| !a.sink);
        let devices = [sink(1, FL | FR, 2, 0x03), mic.clone()];
        let p = plan(&devices, &conversational(), EMU_ISO).unwrap();
        assert_eq!(p.cis.len(), 2);
        assert!(p.cis[0].has_sink() && p.cis[0].source_ase.is_none());
        assert!(!p.cis[1].has_sink());
        assert_eq!((p.cis[1].device_id, p.cis[1].cis_id, p.cis[1].acl), (2, 1, 0x42));
        assert_eq!(p.cis[1].source_ase.unwrap().ase_id, 3);
        assert_eq!(p.sink_cis().count(), 1);
        assert_eq!(p.ases().len(), 2);
        assert_eq!(p.channel_count(), 2);
        // Not conversational: such a device is refused as before
        assert_eq!(
            plan(&devices, &StreamRequest::default(), EMU_ISO),
            Err("sink 2: no sink ASE (not an LE Audio sink?)".into())
        );
        // Microphones only
        assert_eq!(plan(&[mic], &conversational(), EMU_ISO), Err("no device of the group has a sink ASE".into()));
        // No CIS left for it: no microphone
        let request = StreamRequest { prefer_two_channel_ase: false, ..conversational() };
        let devices = [sink(1, FL | FR, 2, 0x01), sink(3, FL | FR, 2, 0x01), devices[1].clone()];
        let p = plan(&devices, &request, EMU_ISO).unwrap();
        assert_eq!(p.cis.len(), 4);
        assert_eq!(p.source, None);
        assert_eq!(p.source_note, "1 more CIS for the microphone than the adapter can take (4)");
    }
}
