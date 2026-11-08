"""HFP audio on the virtual link: SCO data between the controllers, and a
hands-free (HF) role for emu_sink.py --hfp.

Bumble's controller drops HCI SCO data ("unknown packet type"), its link has
no SCO path, Read Buffer Size reports no SCO buffers, it has no handler for
the legacy Setup / Accept Synchronous Connection commands (BTstack's HFP uses
Setup), its LMP features have no SCO / eSCO bits, and its Synchronous
Connection Complete carries no link parameters. HfpEmuController adds that:

  - SCO / eSCO LMP features (eSCO EV3-EV5, EDR eSCO, CVSD and transparent
    data), so BTstack offers mSBC and LC3-SWB (they need eSCO);
  - Read Buffer Size: SCO_BUFFERS buffers of SCO_PACKET_LENGTH bytes;
  - Write Synchronous Flow Control Enable: BTstack turns it on, and then
    gets Number Of Completed Packets for its SCO packets (a host that does
    not, Bumble's, sends paced itself; a full queue drops the oldest);
  - legacy Setup / Accept Synchronous Connection (and the enhanced ones of
    Bumble), Synchronous Connection Complete with eSCO parameters as a real
    controller reports them (7.5 ms interval, 60-byte packets, air mode);
  - HCI SCO data carried to the peer controller's host at the air rate: per
    ESCO_INTERVAL as many bytes as the link carries (CVSD with 16-bit
    samples: 120 bytes = 60 air bytes; transparent: 60 bytes), the
    Number Of Completed Packets when each one is out.

HfSide is the HF: Bumble's HfProtocol (the service level connection, codec
negotiation with the codecs of the case), an HF SDP record, accepting the
AG's audio connection, and SCO both ways. What arrives is checked: CVSD, the 1 kHz
tone in the 16-bit PCM (BlueCaster's test tone mixed to mono: 1 kHz and
1.5 kHz, or 1 kHz alone); mSBC and LC3-SWB, which have no decoder
here, the H2 headers and their sequence (BlueCaster's encoder output is
checked in voice-link/tests/emu_frames.rs). What goes back is a 1.5 kHz
tone: CVSD as 16-bit PCM, mSBC / LC3-SWB as frames encoded by BlueCaster's
codecs (voice_frames.py). It prints a summary line every second:

  HFP rx: <n> packets <bytes> bytes codec <name> tone <verdict> | tx: <n> packets
"""

import asyncio
import collections
import logging
import math
import os
import struct
import sys

from bumble import hci, hfp, lmp
from bumble.controller import ScoLink
from bumble.rfcomm import Server

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from emu_sink import EmuController  # noqa: E402
import voice_frames  # noqa: E402

log = logging.getLogger('emu_hfp')

VOICE_LMP_FEATURES = (
    hci.LmpFeatureMask.SCO_LINK
    | hci.LmpFeatureMask.HV2_PACKETS
    | hci.LmpFeatureMask.HV3_PACKETS
    | hci.LmpFeatureMask.CVSD_SYNCHRONOUS_DATA
    | hci.LmpFeatureMask.TRANSPARENT_SYNCHRONOUS_DATA
    | hci.LmpFeatureMask.EXTENDED_SCO_LINK_EV3_PACKETS
    | hci.LmpFeatureMask.EV4_PACKETS
    | hci.LmpFeatureMask.EV5_PACKETS
    | hci.LmpFeatureMask.ENHANCED_DATA_RATE_ESCO_2_MBPS_MODE
    | hci.LmpFeatureMask.ENHANCED_DATA_RATE_ESCO_3_MBPS_MODE
    | hci.LmpFeatureMask.LMP_3_SLOT_ENHANCED_DATA_RATE_ESCO_PACKETS
)
SCO_PACKET_LENGTH = 60
SCO_BUFFERS = 8
ESCO_INTERVAL = 0.0075      # 12 slots (eSCO S4 / T2: 2-EV3)
ESCO_AIR_BYTES = 60         # air bytes per interval and direction

# The voice setting of the link being set up, by the pair of addresses: the
# acceptor's controller reports the same air mode (one link on this bus)
SCO_SETUP = {}


def _pair(a, b):
    return frozenset((str(a), str(b)))


def air_mode(voice_setting):
    """HCI air mode: 0x02 CVSD, 0x03 transparent"""
    return 0x03 if (voice_setting & 0x03) == 0x03 else 0x02


def hci_bytes_per_interval(voice_setting):
    """HCI payload bytes per ESCO_INTERVAL: CVSD with 16-bit samples carries
    two HCI bytes per air byte"""
    if air_mode(voice_setting) == 0x02 and voice_setting & 0x0020:
        return 2 * ESCO_AIR_BYTES
    return ESCO_AIR_BYTES


class HfpEmuController(EmuController):
    """EmuController plus SCO / eSCO (see module doc)."""

    supported_commands = EmuController.supported_commands | {
        hci.HCI_SETUP_SYNCHRONOUS_CONNECTION_COMMAND,
        hci.HCI_ACCEPT_SYNCHRONOUS_CONNECTION_REQUEST_COMMAND,
        hci.HCI_ENHANCED_SETUP_SYNCHRONOUS_CONNECTION_COMMAND,
        hci.HCI_ENHANCED_ACCEPT_SYNCHRONOUS_CONNECTION_REQUEST_COMMAND,
        hci.HCI_WRITE_SYNCHRONOUS_FLOW_CONTROL_ENABLE_COMMAND,
    }

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.lmp_features |= VOICE_LMP_FEATURES
        self.sco_flow_control = False
        self.sco_queues = {}       # SCO handle -> deque of payloads from the host
        self.sco_timers = {}       # SCO handle -> TimerHandle of the next interval
        self.sco_counts = {}       # SCO handle -> [from host, carried, dropped]

    # ---- configuration ----

    def on_hci_read_buffer_size_command(self, command):
        result = super().on_hci_read_buffer_size_command(command)
        result.hc_synchronous_data_packet_length = SCO_PACKET_LENGTH
        result.hc_total_num_synchronous_data_packets = SCO_BUFFERS
        return result

    def on_hci_write_synchronous_flow_control_enable_command(self, command):
        self.sco_flow_control = bool(command.synchronous_flow_control_enable)
        log.info('[%s] synchronous flow control %s', self.name, 'on' if self.sco_flow_control else 'off')
        return hci.HCI_StatusReturnParameters(status=hci.HCI_ErrorCode.SUCCESS)

    # ---- setup ----

    def _setup(self, peer_address, voice_setting, esco):
        SCO_SETUP[_pair(self.public_address, peer_address)] = voice_setting
        future = self.send_lmp_packet(
            peer_address,
            lmp.LmpEscoLinkReq(
                esco_handle=0, esco_lt_addr=0, timing_control_flags=0, d_esco=0, t_esco=12, w_esco=2,
                esco_packet_type_c_to_p=0, esco_packet_type_p_to_c=0,
                packet_length_c_to_p=ESCO_AIR_BYTES, packet_length_p_to_c=ESCO_AIR_BYTES,
                air_mode=air_mode(voice_setting), negotiation_state=0))
        link_type = (hci.HCI_Connection_Complete_Event.LinkType.ESCO if esco
                     else hci.HCI_Connection_Complete_Event.LinkType.SCO)
        future.add_done_callback(
            lambda f: self.on_classic_sco_connection_complete(peer_address, f.result(), link_type))

    def on_hci_setup_synchronous_connection_command(self, command):
        # Bumble has no class for it: handle (2), transmit / receive
        # bandwidth (4 + 4), max latency (2), voice setting (2),
        # retransmission effort (1), packet type (2)
        handle, _tx, _rx, _latency, voice_setting, _effort, packet_type = struct.unpack_from(
            '<HIIHHBH', command.parameters)
        connection = self.find_connection_by_handle(handle)
        if connection is None:
            self._send_hci_command_status(hci.HCI_ErrorCode.UNKNOWN_CONNECTION_IDENTIFIER_ERROR, command.op_code)
            return None
        self._send_hci_command_status(hci.HCI_ErrorCode.SUCCESS, command.op_code)
        # EV3-EV5 allowed, or one of the EDR types (bits 6-9 are "shall not
        # be used") not excluded
        esco = bool(packet_type & 0x0038) or (packet_type & 0x03C0) != 0x03C0
        log.info('[%s] Setup Synchronous Connection: voice setting 0x%04X, packet types 0x%04X (%s)',
                 self.name, voice_setting, packet_type, 'eSCO' if esco else 'SCO')
        self._setup(connection.peer_address, voice_setting, esco)
        return None

    def on_hci_enhanced_setup_synchronous_connection_command(self, command):
        connection = self.find_connection_by_handle(command.connection_handle)
        if connection is None:
            self._send_hci_command_status(hci.HCI_ErrorCode.UNKNOWN_CONNECTION_IDENTIFIER_ERROR, command.op_code)
            return None
        self._send_hci_command_status(hci.HCI_ErrorCode.SUCCESS, command.op_code)
        coding = command.transmit_coding_format
        transparent = getattr(coding, 'codec_id', None) == hci.CodecID.TRANSPARENT
        self._setup(connection.peer_address, 0x0003 if transparent else 0x0060, True)
        return None

    def on_hci_accept_synchronous_connection_request_command(self, command):
        connection = self.classic_connections.get(command.bd_addr)
        if connection is None:
            self._send_hci_command_status(hci.HCI_ErrorCode.UNKNOWN_CONNECTION_IDENTIFIER_ERROR, command.op_code)
            return None
        self._send_hci_command_status(hci.HCI_ErrorCode.SUCCESS, command.op_code)
        self._accept(connection.peer_address)
        return None

    def on_hci_enhanced_accept_synchronous_connection_request_command(self, command):
        connection = self.classic_connections.get(command.bd_addr)
        if connection is None:
            self._send_hci_command_status(hci.HCI_ErrorCode.UNKNOWN_CONNECTION_IDENTIFIER_ERROR, command.op_code)
            return None
        self._send_hci_command_status(hci.HCI_ErrorCode.SUCCESS, command.op_code)
        self._accept(connection.peer_address)
        return None

    def _accept(self, peer_address):
        pending = self.sco_links.get(peer_address)
        link_type = pending.link_type if pending else hci.HCI_Connection_Complete_Event.LinkType.ESCO
        self.send_lmp_packet(peer_address, lmp.LmpAcceptedExt(lmp.Opcode.LMP_ESCO_LINK_REQ))
        self.on_classic_sco_connection_complete(peer_address, hci.HCI_ErrorCode.SUCCESS, link_type)

    def on_classic_sco_connection_complete(self, peer_address, status, link_type):
        voice_setting = SCO_SETUP.get(_pair(self.public_address, peer_address), 0x0060)
        esco = link_type == hci.HCI_Connection_Complete_Event.LinkType.ESCO
        handle = 0
        if status == hci.HCI_ErrorCode.SUCCESS:
            handle = self.allocate_connection_handle()
            self.sco_links[peer_address] = ScoLink(handle=handle, link_type=link_type, peer_address=peer_address)
            self.sco_queues[handle] = collections.deque()
            self.sco_counts[handle] = [0, 0, 0]
            self._schedule(handle, voice_setting, asyncio.get_running_loop().time() + ESCO_INTERVAL)
            log.info('[%s] SCO link 0x%04X to %s up (%s, voice setting 0x%04X)', self.name, handle, peer_address,
                     'eSCO' if esco else 'SCO', voice_setting)
        else:
            self.sco_links.pop(peer_address, None)
        self.send_hci_packet(
            hci.HCI_Synchronous_Connection_Complete_Event(
                status=status,
                connection_handle=handle,
                bd_addr=peer_address,
                link_type=link_type,
                transmission_interval=12 if esco else 6,
                retransmission_window=2 if esco else 0,
                rx_packet_length=ESCO_AIR_BYTES if esco else 0,
                tx_packet_length=ESCO_AIR_BYTES if esco else 0,
                air_mode=air_mode(voice_setting),
            )
        )

    def on_classic_sco_disconnected(self, peer_address, reason):
        link = self.sco_links.get(peer_address)
        if link is not None:
            self._stop(link.handle)
        super().on_classic_sco_disconnected(peer_address, reason)

    def on_hci_disconnect_command(self, command):
        link = self.find_classic_sco_link_by_handle(command.connection_handle)
        if link is not None:
            self._stop(link.handle)
        return super().on_hci_disconnect_command(command)

    def _stop(self, handle):
        timer = self.sco_timers.pop(handle, None)
        if timer is not None:
            timer.cancel()
        counts = self.sco_counts.pop(handle, None)
        self.sco_queues.pop(handle, None)
        if counts:
            log.info('[%s] SCO link 0x%04X down: %d packets from the host, %d carried, %d dropped',
                     self.name, handle, *counts)
            print(f'SCO [{self.name}] 0x{handle:04X}: {counts[0]} from host, {counts[1]} carried, '
                  f'{counts[2]} dropped', flush=True)

    # ---- data ----

    def on_hci_packet(self, packet):
        if isinstance(packet, hci.HCI_SynchronousDataPacket):
            self.on_hci_sco_data_packet(packet)
            return
        super().on_hci_packet(packet)

    def on_hci_sco_data_packet(self, packet):
        handle = packet.connection_handle
        q = self.sco_queues.get(handle)
        if q is None:
            log.warning('[%s] SCO data for unknown handle 0x%04X', self.name, handle)
            return
        self.sco_counts[handle][0] += 1
        q.append(bytes(packet.data))
        if not self.sco_flow_control and len(q) > SCO_BUFFERS:
            q.popleft()
            self.sco_counts[handle][2] += 1

    def _schedule(self, handle, voice_setting, due, credit=0):
        loop = asyncio.get_running_loop()
        self.sco_timers[handle] = loop.call_at(due, self._interval, handle, voice_setting, due, credit)

    def _interval(self, handle, voice_setting, due, credit):
        """One eSCO interval: the bytes the air carries in it go to the peer"""
        q = self.sco_queues.get(handle)
        if q is None:
            return
        link = self.find_classic_sco_link_by_handle(handle)
        peer = self.link.find_classic_controller(link.peer_address) if link else None
        peer_link = peer.sco_links.get(self.public_address) if peer is not None else None
        budget = hci_bytes_per_interval(voice_setting)
        credit = min(credit + budget, 2 * budget)
        done = 0
        while q and len(q[0]) <= credit:
            data = q.popleft()
            credit -= len(data)
            done += 1
            self.sco_counts[handle][1] += 1
            if peer_link is not None and peer_link.handle:
                peer.send_hci_packet(hci.HCI_SynchronousDataPacket(
                    connection_handle=peer_link.handle,
                    packet_status=hci.HCI_SynchronousDataPacket.Status.CORRECTLY_RECEIVED_DATA,
                    data_total_length=len(data), data=data))
        if not q:
            credit = 0      # nothing waits: the air time is gone
        if done and self.sco_flow_control:
            self.send_hci_packet(hci.HCI_Number_Of_Completed_Packets_Event(
                connection_handles=[handle], num_completed_packets=[done]))
        now = asyncio.get_running_loop().time()
        due += ESCO_INTERVAL
        if due < now - 0.1:   # the event loop stalled: do not catch up in a burst
            due = now
        self._schedule(handle, voice_setting, due, credit)


# ---- the HF role ----

CODEC_CASES = {
    'cvsd': [hfp.AudioCodec.CVSD],
    'msbc': [hfp.AudioCodec.CVSD, hfp.AudioCodec.MSBC],
    'lc3swb': [hfp.AudioCodec.CVSD, hfp.AudioCodec.MSBC, hfp.AudioCodec.LC3_SWB],
}
CODEC_NAMES = {hfp.AudioCodec.CVSD: 'cvsd', hfp.AudioCodec.MSBC: 'msbc', hfp.AudioCodec.LC3_SWB: 'lc3swb'}
RX_TONE_HZ = 1000
H2_SEQUENCE = (0x08, 0x38, 0xC8, 0xF8)
TX_TONE_HZ = 1500
CVSD_RATE = 8000


def goertzel_ratio(samples, rate, freq):
    """Share of the signal's energy at `freq` (1.0: a pure tone)"""
    n = len(samples)
    if n == 0:
        return 0.0
    k = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for x in samples:
        s1, s2 = x + k * s1 - s2, s1
    power = s1 * s1 + s2 * s2 - k * s1 * s2
    total = sum(x * x for x in samples)
    return (2 * power / n) / total if total else 0.0


class HfSide:
    """The hands-free unit of emu_sink.py --hfp (see module doc)."""

    def __init__(self, device, case):
        self.device = device
        self.case = case
        self.codecs = CODEC_CASES[case]
        features = [hfp.HfFeature.HF_INDICATORS, hfp.HfFeature.REMOTE_VOLUME_CONTROL]
        if len(self.codecs) > 1:
            # cvsd: an HF without codec negotiation (CVSD without +BCS)
            features += [hfp.HfFeature.CODEC_NEGOTIATION, hfp.HfFeature.ESCO_S4_SETTINGS_SUPPORTED]
        else:
            features += [hfp.HfFeature.ESCO_S4_SETTINGS_SUPPORTED]
        self.configuration = hfp.HfConfiguration(
            supported_hf_features=features,
            supported_hf_indicators=[hfp.HfIndicator.BATTERY_LEVEL],
            supported_audio_codecs=self.codecs,
        )
        channel = Server(device).listen(self.on_dlc, 0)
        device.sdp_service_records[0x00010004] = hfp.make_hf_sdp_records(0x00010004, channel, self.configuration)
        device.on('sco_request', self.on_sco_request)
        device.on('sco_connection', self.on_sco_connection)
        self.protocol = None
        self.codec = hfp.AudioCodec.CVSD
        self.rx_packets = self.rx_bytes = self.tx_packets = 0
        self.rx_status = collections.Counter()
        self.pcm = []           # CVSD: received 16-bit samples
        self.h2_good = self.h2_bad = 0   # mSBC / LC3-SWB: packets with / without the next H2 header
        self.h2_seq = None
        self.sco = None
        self.sender = None
        self.reporter = None

    def on_dlc(self, dlc):
        log.info('HFP: RFCOMM channel open')
        self.protocol = hfp.HfProtocol(dlc, self.configuration)
        self.protocol.on('codec_negotiation', self.on_codec)
        asyncio.ensure_future(self.protocol.run())
        print('HFP: service level connection starting', flush=True)

    def on_codec(self, codec):
        self.codec = codec
        print(f'HFP: codec negotiated {CODEC_NAMES.get(codec, codec)}', flush=True)

    def on_sco_request(self, connection, link_type):
        print(f'HFP: audio connection request (link type {link_type})', flush=True)
        params = hfp.ESCO_PARAMETERS[
            hfp.DefaultCodecParameters.ESCO_CVSD_S4 if self.codec == hfp.AudioCodec.CVSD
            else hfp.DefaultCodecParameters.ESCO_MSBC_T2]
        asyncio.ensure_future(self.device.send_command(
            hci.HCI_Enhanced_Accept_Synchronous_Connection_Request_Command(
                bd_addr=connection.peer_address, **params.asdict())))

    def on_sco_connection(self, sco_link):
        self.sco = sco_link
        print(f'HFP: audio connection up, SCO 0x{sco_link.handle:04X}, codec {CODEC_NAMES.get(self.codec)}, '
              f'air mode {int(sco_link.air_mode)}', flush=True)
        sco_link.sink = self.on_sco_packet
        sco_link.on('disconnection', lambda *_: self.on_sco_down())
        self.sender = asyncio.ensure_future(self.send_loop(sco_link.handle))
        if self.reporter is None:
            self.reporter = asyncio.ensure_future(self.report_loop())

    def on_sco_down(self):
        print('HFP: audio connection down', flush=True)
        if self.sender:
            self.sender.cancel()
        self.report()

    def on_sco_packet(self, packet):
        self.rx_packets += 1
        self.rx_bytes += len(packet.data)
        self.rx_status[int(packet.packet_status)] += 1
        data = bytes(packet.data)
        if self.codec == hfp.AudioCodec.CVSD:
            if len(self.pcm) < 10 * CVSD_RATE:
                self.pcm.extend(struct.unpack(f'<{len(data) // 2}h', data[:len(data) // 2 * 2]))
            return
        # 60-byte packets, each one H2 frame: header 0x01 + sequence
        seq = H2_SEQUENCE.index(data[1]) if len(data) >= 2 and data[0] == 0x01 and data[1] in H2_SEQUENCE else None
        if seq is not None and (self.h2_seq is None or seq == (self.h2_seq + 1) % 4):
            self.h2_good += 1
        elif self.h2_good:
            self.h2_bad += 1    # silence before the encoder runs is not counted
        self.h2_seq = seq if seq is not None else self.h2_seq

    async def send_loop(self, handle):
        """60-byte packets at the link's HCI rate: CVSD 16-bit 16 kB/s (one
        every 3.75 ms), transparent 8 kB/s (one every 7.5 ms)"""
        loop = asyncio.get_running_loop()
        cvsd = self.codec == hfp.AudioCodec.CVSD
        period = ESCO_INTERVAL / 2 if cvsd else ESCO_INTERVAL
        phase = 0
        due = loop.time()
        # mSBC / LC3-SWB: H2 header + a frame of the 1.5 kHz tone (and
        # mSBC's padding byte)
        frames = voice_frames.MSBC_FRAMES if self.codec == hfp.AudioCodec.MSBC else voice_frames.LC3_SWB_FRAMES
        seq = 0
        while True:
            if cvsd:
                samples = [int(8000 * math.sin(2 * math.pi * TX_TONE_HZ * (phase + i) / CVSD_RATE))
                           for i in range(SCO_PACKET_LENGTH // 2)]
                phase += len(samples)
                data = struct.pack(f'<{len(samples)}h', *samples)
            else:
                data = (bytes([0x01, H2_SEQUENCE[seq % 4]]) + frames[seq % 4]).ljust(SCO_PACKET_LENGTH, b'\x00')
                seq += 1
            self.device.host.send_sco_sdu(handle, data)
            self.tx_packets += 1
            due += period
            await asyncio.sleep(max(0.0, due - loop.time()))

    def verdict(self):
        if self.codec != hfp.AudioCodec.CVSD:
            ok = self.h2_good >= 100 and self.h2_bad <= self.h2_good // 50
            return f'{"ok" if ok else "FAIL"} h2 {self.h2_good} in sequence, {self.h2_bad} not'
        samples = self.pcm[CVSD_RATE // 5:]     # skip the first 200 ms
        if len(samples) < CVSD_RATE // 2:
            return f'pending ({len(samples)} samples)'
        # In blocks of 20 ms (whole periods of both tones) over the last 3 s:
        # the 1 kHz tone, alone or with the test tone's 1.5 kHz, and nothing
        # else. The sender's drift control repeats a few samples now and
        # then while its queue settles after the start, which breaks the
        # phase of a block, not the tone: 90 % of the blocks.
        block = CVSD_RATE // 50
        samples = samples[-3 * CVSD_RATE:]
        blocks = [samples[i:i + block] for i in range(0, len(samples) - block + 1, block)]
        ratios = [(goertzel_ratio(b, CVSD_RATE, RX_TONE_HZ), goertzel_ratio(b, CVSD_RATE, TX_TONE_HZ)) for b in blocks]
        good = sum(1 for a, b in ratios if a > 0.4 and a + b > 0.9)
        ratio = sum(a for a, _ in ratios) / len(ratios)
        other = sum(b for _, b in ratios) / len(ratios)
        ok = good >= 0.9 * len(ratios)
        return (f'{"ok" if ok else "FAIL"} {RX_TONE_HZ} Hz ratio {ratio:.3f} (+{TX_TONE_HZ} Hz {other:.3f}) '
                f'in {good}/{len(ratios)} blocks')

    def report(self):
        status = ' '.join(f'{k}:{v}' for k, v in sorted(self.rx_status.items()))
        print(f'HFP rx: {self.rx_packets} packets {self.rx_bytes} bytes codec {CODEC_NAMES.get(self.codec)} '
              f'status {status or "-"} tone {self.verdict()} | tx: {self.tx_packets} packets', flush=True)

    async def report_loop(self):
        while True:
            await asyncio.sleep(1.0)
            self.report()
