"""The headset microphone of the emulated sinks (--mic): a source ASE.

--mic gives every sink (--mic 1 only the first, --mic 2 only the second,
--mic 1,2 both) what a headset with a microphone has (bap.py builds it from
source_setup() here):
  - a Source PAC record: LC3 16 and 32 kHz (--mic-rates), 10 ms frames, 1
    channel, 40-80 octets (16_2_1 / 32_2_1)
  - Source Audio Locations (the sink's own location, Front Left for a stereo
    headset) and source contexts Conversational, Game and Unspecified
    (supported and available)
  - a source ASE (ASE id after the sink ASEs)

Once BlueCaster (the receiver) wrote Receiver Start Ready and the source ASE
is Streaming, a sender task writes one LC3 SDU per SDU interval on its CIS:
a 1 kHz tone at half scale, frames pre-encoded by liblc3 (mic_frames.py).
The emulated controller paces it as it paces BlueCaster (Number Of Completed
Packets once an SDU went out, 2 queued). It stops when the ASE leaves
Streaming (Disable: Disabling until BlueCaster's Receiver Stop Ready) and
logs "<sink>: microphone: sent N SDUs".
"""

import asyncio
import logging

from bumble.device import IsoPacketStream
from bumble.profiles.ascs import AseStateMachine, AudioRole
from bumble.profiles.bap import (
    AudioLocation,
    CodecSpecificCapabilities,
    ContextType,
    SupportedFrameDuration,
    SupportedSamplingFrequency,
)
from bumble.profiles.pacs import PacRecord
from bumble import hci

from .mic_frames import FRAMES, OCTETS

log = logging.getLogger('emu_le_audio')

SOURCE_CONTEXTS = ContextType.CONVERSATIONAL | ContextType.GAME | ContextType.UNSPECIFIED
QUEUE = 2   # SDUs queued in the controller, as BlueCaster does


def add_arguments(parser):
    parser.add_argument('--mic', nargs='?', const='all', metavar='SINKS',
                        help='the sinks have a microphone (source ASE); SINKS: 1-based numbers, e.g. 1 or 1,2 '
                             '(default: all)')
    parser.add_argument('--mic-rates', default='16000,32000',
                        help='sampling rates of the Source PAC record (16000 and / or 32000; default both)')


def has_mic(sink, args):
    value = getattr(args, 'mic', None)
    if not value:
        return False
    if value == 'all':
        return True
    return str(sink.index + 1) in value.split(',')


def source_setup(sink, args):
    """bap.py's PACS / ASCS additions for a sink with a microphone: (PACS
    keyword arguments, source ASE ids); ({}, []) without one"""
    if not has_mic(sink, args):
        return {}, []
    rates = SupportedSamplingFrequency(0)
    for hz in str(getattr(args, 'mic_rates', '16000,32000')).split(','):
        rates |= {'16000': SupportedSamplingFrequency.FREQ_16000,
                  '32000': SupportedSamplingFrequency.FREQ_32000}[hz.strip()]
    record = PacRecord(
        coding_format=hci.CodingFormat(hci.CodecID.LC3),
        codec_specific_capabilities=CodecSpecificCapabilities(
            supported_sampling_frequencies=rates,
            supported_frame_durations=SupportedFrameDuration.DURATION_10000_US_SUPPORTED,
            supported_audio_channel_count=[1],
            min_octets_per_codec_frame=40,
            max_octets_per_codec_frame=80,
            supported_max_codec_frames_per_sdu=1,
        ),
    )
    single = sink.locations if sink.locations and sink.locations & (sink.locations - 1) == 0 else 0
    location = single or (AudioLocation.FRONT_LEFT if sink.locations else 0)
    sink.state['mic'] = dict(location=location, ase_id=sink.sink_ases + 1)
    pacs = dict(
        supported_source_context=SOURCE_CONTEXTS,
        available_source_context=SOURCE_CONTEXTS,
        source_pac=[record],
        source_audio_locations=AudioLocation(location),
    )
    return pacs, [sink.sink_ases + 1]


def configure_sink(sink, args):
    mic = sink.state.get('mic')
    if mic is None:
        return
    ascs = sink.state['bap']['ascs']
    for ase in ascs.ase_state_machines.values():
        if ase.role != AudioRole.SOURCE:
            continue
        mic['sending'] = None   # SDUs of the running sender

        def on_state(ase=ase):
            if ase.state == AseStateMachine.State.STREAMING and ase.cis_link is not None:
                mic['sending'] = [0]
                asyncio.get_running_loop().create_task(send(sink, ase, ase.cis_link, mic['sending']))
            elif mic['sending'] is not None:
                # Logged right away: the process may be ended soon after
                log.info('%s: microphone: sent %d SDUs (ASE %s)', sink.name, mic['sending'][0], ase.state.name)
                mic['sending'] = None
        ase.on('state_change', on_state)


def frame_rate(ase):
    """(sampling Hz, octets) the source ASE was configured with"""
    cfg = ase.codec_specific_configuration
    hz = getattr(getattr(cfg, 'sampling_frequency', None), 'hz', None)
    return hz, getattr(cfg, 'octets_per_codec_frame', None)


async def send(sink, ase, link, count):
    hz, octets = frame_rate(ase)
    frames = FRAMES.get(hz)
    if frames is None or octets != OCTETS.get(hz):
        log.error('%s: microphone: no frames for %s Hz / %s octets (have %s)', sink.name, hz, octets, OCTETS)
        return
    log.info('%s: microphone: sending a 1 kHz tone on CIS 0x%04X, LC3 %d Hz %d octets', sink.name, link.handle,
             hz, octets)
    stream = IsoPacketStream(link, QUEUE)
    n = 0
    while ase.state == AseStateMachine.State.STREAMING and ase.cis_link is link:
        try:
            await asyncio.wait_for(stream.write(frames[n % len(frames)]), 0.5)
        except asyncio.TimeoutError:
            break
        n += 1
        count[0] += 1
    log.debug('%s: microphone: sender ended after %d SDUs', sink.name, n)
