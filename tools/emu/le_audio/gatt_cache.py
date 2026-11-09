"""A database that changes between links, for BlueCaster's kept services.

Bumble's Generic Attribute service has a Database Hash (Core 5.1 GATT
Caching) on every sink already. With --gatt-change the first sink adds a
service when its first link goes down, which changes the hash: on the
reconnect BlueCaster must discover again and see the new service. It logs

  GATT <sink>: database changed (service 0xFFF0 added)
"""

import logging

from bumble import gatt

log = logging.getLogger('emu_le_audio')

ADDED = gatt.UUID.from_16_bits(0xFFF0, 'BlueCaster test service')


def add_arguments(parser):
    parser.add_argument('--gatt-change', action='store_true',
                        help='the first sink adds a service when its first link goes down')


def configure_sink(sink, args):
    if not args.gatt_change or sink.index != 0:
        return
    state = {'changed': False}

    def on_disconnection(_reason):
        if state['changed']:
            return
        state['changed'] = True
        char = gatt.Characteristic(gatt.UUID.from_16_bits(0xFFF1), gatt.Characteristic.Properties.READ,
                                   gatt.Characteristic.READABLE, b'\x01')
        sink.device.add_service(gatt.Service(ADDED, [char]))
        log.info('GATT %s: database changed (service 0xFFF0 added)', sink.name)

    @sink.device.on('connection')
    def on_connection(connection):
        connection.on('disconnection', on_disconnection)
