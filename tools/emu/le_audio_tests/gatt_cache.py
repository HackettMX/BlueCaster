"""Kept GATT services (le_gatt_cache.txt) cases and checks of run_test.py
--le-audio (standard library only).

The sink (Bumble) has a Database Hash and drops the link once: BlueCaster
discovers on the first connection and keeps the services with the hash; on
the reconnect it reads the hash again and skips discovery, or, when the
sink added a service in between (emu option --gatt-change,
le_audio/gatt_cache.py), discovers again and finds it.
"""

import re

from .case import Case

STEREO = r'LC3 48000 Hz 10 ms 155 octets, 2 channel\(s\) over 1 CIS'

CASES = [
    Case('gatt-cache', 'stereo', STEREO, sink_args=['--drop-link-after', '3'],
         tags=('gatt_cache',), streams=2, lossy=True, duration=14),
    Case('gatt-cache-changed', 'stereo', STEREO, sink_args=['--gatt-change', '--drop-link-after', '3'],
         tags=('gatt_cache',), streams=2, lossy=True, duration=14),
]


def check(case, ctx):
    if 'gatt_cache' not in case.tags:
        return []
    log, sink_log = ctx['bluecaster_log'], ctx['sink_log']
    failures = []
    lists = [int(n) for n in re.findall(r'LE Audio: \S+: (\d+) services, \d+ characteristics', log)]
    skipped = len(re.findall(r'services unchanged \(Database Hash\), discovery skipped', log))
    if not re.search(r'services kept with their Database Hash', log):
        failures.append('the first connection did not keep the services with their hash')
    if len(lists) < 2:
        failures.append(f'{len(lists)} service list(s) in the log, expected one per connection')
    if case.name == 'gatt-cache':
        if skipped != 1:
            failures.append(f'discovery skipped {skipped} time(s), expected once (the reconnect)')
        elif len(lists) >= 2 and lists[1] != lists[0]:
            failures.append(f'the kept list has {lists[1]} services, the discovered one {lists[0]}')
    else:
        if 'database changed' not in sink_log:
            failures.append('the sink did not change its database')
        if skipped:
            failures.append('discovery was skipped although the database changed')
        if not re.search(r'Database Hash changed \(0x00\), discovering the services', log):
            failures.append('the changed Database Hash was not noticed')
        if len(lists) >= 2 and lists[1] != lists[0] + 1:
            failures.append(f'service lists {lists}: the added service was not found')
    return failures
