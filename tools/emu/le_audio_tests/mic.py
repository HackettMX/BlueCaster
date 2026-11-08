"""Headset microphone cases (LE Audio source ASEs) of run_test.py --le-audio.

The sinks get a microphone with emu_le_audio_sink.py --mic (le_audio/mic.py:
Source PAC 16 / 32 kHz 10 ms, a source ASE, a 1 kHz tone at half scale);
run_test.py runs BlueCaster with --headset-mic --mic-output <out>/mic.wav for
cases tagged 'mic'. The playback direction is checked as in every case
(sent = received, the test tone); the checks here are the microphone's:

  - BlueCaster planned the source direction ("LE Audio: microphone: LC3 ...")
    with the expected preset, wrote Receiver Start Ready and, when stopping,
    Receiver Stop Ready
  - the SDUs the headset sent ("microphone: sent N SDUs" in sink.log) are
    the ones in BlueCaster's capture (bluecaster_decode: "Mic SDUs", no
    problems on either side)
  - mic.wav: mono at the microphone's rate, as many frames as BlueCaster's
    decoder put out ("LE Audio: microphone: N frames out ..."), which is
    what arrived less the jitter buffer's prefill (2) within a small
    tolerance; few frames concealed, none late
  - the 1 kHz tone (amplitude 0.5) in every 100 ms block of mic.wav but a
    short gap at the start and the end

  mic-stereo     stereo headset with a microphone: 48_6_2 to it, 32_2_1 from
                 it, both on the one CIS
  mic-earbuds    two earbuds, only the left one with a microphone: its CIS
                 carries both directions, the right one plays only
  mic-mono-16k   mono headset, 16_2_1 both ways (its microphone takes 16 kHz
                 only)
"""

import math
import os
import re
import struct
import wave

from .case import MONO_TONES, Case

STEREO = r'LC3 48000 Hz 10 ms 155 octets, 2 channel\(s\) over 1 CIS'
EARBUDS = r'LC3 48000 Hz 10 ms 155 octets, 2 channel\(s\) over 2 CIS'

CASES = [
    Case('mic-stereo', 'stereo', STEREO, sink_args=['--mic'], tags=('mic',)),
    Case('mic-earbuds', 'earbuds', EARBUDS, sink_args=['--mic', '1'], tags=('mic',)),
    Case('mic-mono-16k', 'mono', r'LC3 16000 Hz 10 ms 40 octets, 1 channel\(s\) over 1 CIS', tones=MONO_TONES,
         sink_args=['--mic', '--mic-rates', '16000'], bluecaster_args=['--le-preset', '16_2_1'], tags=('mic',)),
]

# What each case's microphone is: (preset, Hz, microphone CIS)
EXPECTED = {
    'mic-stereo': ('32_2_1', 32000, 1),
    'mic-earbuds': ('32_2_1', 32000, 1),
    'mic-mono-16k': ('16_2_1', 16000, 1),
}

PREFILL = 2            # BlueCaster's jitter buffer (pipeline le_rx PREFILL_SDUS)
# SDUs that arrive after BlueCaster's Disable (before the source ASE left
# Streaming) are in its capture but not decoded
SDU_TOLERANCE = 3      # SDUs the headset sent that may not be in BlueCaster's capture (in flight at the end)
MAX_CONCEALED = 2
TONE_HZ, TONE_AMP = 1000.0, 0.5
BLOCK_S = 0.1
MAX_GAP_S = 0.3

MIC_OUT = re.compile(r'LE Audio: microphone: (\d+) frames out \((\d+) decoded, (\d+) concealed\), (\d+) SDUs, '
                     r'(\d+) late, (\d+) resyncs, (\d+) errors')


def bluecaster_args(case, out):
    """run_test.py adds these for a microphone case"""
    if 'mic' not in case.tags:
        return []
    return ['--headset-mic', '--mic-output', os.path.join(out, 'mic.wav')]


def goertzel(xs, rate, freq):
    k = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for x in xs:
        s1, s2 = x + k * s1 - s2, s1
    return 2 * math.sqrt(max(s1 * s1 + s2 * s2 - k * s1 * s2, 0.0)) / len(xs)


def tone_blocks(samples, rate):
    """(good, bad) block indexes: the tone at its amplitude (within 0.05)
    with > 95% of the energy"""
    n = int(rate * BLOCK_S)
    good, bad = [], []
    for b in range(len(samples) // n):
        xs = samples[b * n:(b + 1) * n]
        mean_sq = sum(x * x for x in xs) / n
        amp = goertzel(xs, rate, TONE_HZ)
        share = (amp * amp / 2) / mean_sq if mean_sq else 0.0
        (good if abs(amp - TONE_AMP) < 0.05 and share > 0.95 else bad).append((b, amp, share))
    return good, bad


def check_wav(path, rate, frames_out, frame_samples):
    failures = []
    try:
        with wave.open(path) as w:
            ch, width, wrate, n = w.getnchannels(), w.getsampwidth(), w.getframerate(), w.getnframes()
            raw = w.readframes(n)
    except (OSError, wave.Error, EOFError) as e:
        return [f'mic.wav: {e}'], ''
    if (ch, width, wrate) != (1, 2, rate):
        return [f'mic.wav: {ch} ch, {8 * width} bit, {wrate} Hz (expected mono 16-bit {rate} Hz)'], ''
    if frames_out is not None and n != frames_out * frame_samples:
        failures.append(f'mic.wav has {n} samples, BlueCaster put out {frames_out} frames of {frame_samples}')
    samples = [v / 32768.0 for v in struct.unpack(f'<{n}h', raw)]
    good, bad = tone_blocks(samples, rate)
    blocks = n // int(rate * BLOCK_S)
    if not good:
        return failures + [f'mic.wav: no {BLOCK_S * 1000:.0f} ms block with the 1 kHz tone in {n / rate:.2f} s'], ''
    first, last = good[0][0], good[-1][0]
    start_gap, end_gap = first * BLOCK_S, (blocks - 1 - last) * BLOCK_S
    inner = [b for b in bad if first < b[0] < last]
    mid = good[len(good) // 2]
    desc = (f'mic {n / rate:.2f} s, 1 kHz amp {mid[1]:.3f} ({100 * mid[2]:.1f}%), gaps {start_gap:.1f} s / '
            f'{end_gap:.1f} s')
    if start_gap > MAX_GAP_S + 1e-9:
        failures.append(f'mic.wav: tone only from {start_gap:.1f} s')
    if end_gap > MAX_GAP_S + 1e-9:
        failures.append(f'mic.wav: no tone in the last {end_gap:.1f} s')
    if inner:
        b, amp, share = inner[0]
        failures.append(f'mic.wav: {len(inner)} block(s) without the tone, first at {b * BLOCK_S:.1f} s '
                        f'(amp {amp:.3f}, {100 * share:.1f}%)')
    return failures, desc


def report_mics(report):
    """(SDUs, result ok) per microphone of a bluecaster_decode report"""
    text = (report or {}).get('text', '')
    sdus = [int(x) for x in re.findall(r'^  Mic SDUs      : (\d+)', text, re.M)]
    ok = re.findall(r'^  Mic result    : (OK|\d+ kind)', text, re.M)
    return sdus, ok


def check(case, ctx):
    if 'mic' not in case.tags:
        return []
    failures = []
    log = ctx['bluecaster_log']
    sink_log = ctx['sink_log']
    preset, rate, mic_cis = EXPECTED[case.name]
    m = re.search(r'LE Audio: microphone: LC3 (\d+) kHz 10 ms (\d+) octets \(\d+ kbps\), (\d+) ch over (\d+) CIS, '
                  r'(\S+)', log)
    if not m:
        failures.append('BlueCaster did not plan the microphone ("LE Audio: microphone: LC3 ...")')
    elif (int(m.group(1)) * 1000, m.group(5), int(m.group(4))) != (rate, preset, mic_cis):
        failures.append(f'microphone planned as {m.group(0)} (expected {preset}, {rate} Hz, {mic_cis} CIS)')
    for step in ('Receiver Start Ready ASE', 'Receiver Stop Ready ASE'):
        if step not in log:
            failures.append(f'BlueCaster did not write {step.split(" ASE")[0]}')

    sent = sum(int(x) for x in re.findall(r'microphone: sent (\d+) SDUs', sink_log))
    if not sent:
        failures.append('the headset sent no microphone SDUs (sink.log)')
    at_bluecaster, ok_bc = report_mics(ctx.get('sent'))
    at_sink, ok_sink = report_mics(ctx.get('received'))
    if len(at_bluecaster) != mic_cis or len(at_sink) != mic_cis:
        failures.append(f'microphone streams: {len(at_bluecaster)} in BlueCaster\'s capture, {len(at_sink)} in the '
                        f'sink\'s (expected {mic_cis})')
    if any(x != 'OK' for x in ok_bc + ok_sink):
        failures.append('bluecaster_decode found problems in a microphone stream (see the reports)')
    received = sum(at_bluecaster)
    # (one more may reach BlueCaster: the SDU written as the ASE left Streaming)
    if sent and not -1 <= sent - received <= SDU_TOLERANCE:
        failures.append(f'the headset sent {sent} microphone SDUs, {received} reached BlueCaster')
    if not 0 <= sum(at_sink) - sent <= 1:   # one SDU may go out while the ASE leaves Streaming
        failures.append(f'sink capture has {sum(at_sink)} microphone SDUs, the sink logged {sent}')

    out = MIC_OUT.search(log)
    frames_out = None
    if not out:
        failures.append('no "LE Audio: microphone: N frames out" statistics in bluecaster.log')
    else:
        frames_out, decoded, concealed, sdus, late, resyncs, errors = (int(x) for x in out.groups())
        # The capture also holds SDUs still in flight when the stream stopped, and the jitter buffer keeps
        # PREFILL of those the decode thread got: compare each stage with the one before it.
        if not 0 <= received - sdus <= SDU_TOLERANCE + PREFILL:
            failures.append(f'the decode thread got {sdus} microphone SDUs of {received} in the capture')
        if not 0 <= sdus - frames_out <= PREFILL:
            failures.append(f'BlueCaster decoded {frames_out} microphone frames of {sdus} SDUs it received')
        if concealed > MAX_CONCEALED or late or resyncs or errors:
            failures.append(f'microphone: {concealed} concealed, {late} late, {resyncs} resyncs, {errors} errors')
    wav_failures, desc = check_wav(os.path.join(ctx['out'], 'mic.wav'), rate, frames_out, rate // 100)
    failures += wav_failures
    if desc:
        ctx['mic_detail'] = f'{desc}, {received} SDUs from the headset'   # run_test.py adds it to its line
    return failures
