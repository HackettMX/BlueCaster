"""The microphone frames of the emulator's hands-free unit (hfp_audio.py):
a 1.5 kHz tone (amplitude 8000 of 32767) encoded by BlueCaster's codecs, four
frames that repeat seamlessly (45 periods). Without the H2 header, which the
sender adds with its sequence number; mSBC also without the padding byte.

Generated, do not edit:
cargo run -p voice-link --example emu_voice_frames > tools/emu/voice_frames.py
(app/src/crates/voice-link/examples/emu_voice_frames.rs; the test
voice-link/tests/emu_frames.rs checks that they decode to the tone)
"""

TONE_HZ = 1500

# mSBC: 57-byte frames of 120 samples at 16 kHz
MSBC_FRAMES = [
    bytes.fromhex('ad00006d2c200000a7fa770d26174570578b299e4aa7fa770d26174570578b299e4aa7fa770d26174570578b299e4aa7fa770d261745705788'),
    bytes.fromhex('ad00006d2c200000ca6792a9fe9dc34985d15c15e2ca6792a9fe9dc34985d15c15e2ca6792a9fe9dc34985d15c15e2ca6792a9fe9dc34985d0'),
    bytes.fromhex('ad00006d2c200000570578b299e4aa7fa770d26174570578b299e4aa7fa770d26174570578b299e4aa7fa770d26174570578b299e4aa7fa770'),
    bytes.fromhex('ad00006d2c20000034985d15c15e2ca6792a9fe9dc34985d15c15e2ca6792a9fe9dc34985d15c15e2ca6792a9fe9dc34985d15c15e2ca67928'),
]

# LC3-SWB: 58-byte frames of 240 samples (7.5 ms) at 32 kHz
LC3_SWB_FRAMES = [
    bytes.fromhex('c495fa61d205173fb4778235151268e712b405bf0ebea3bfcee83f34a5f4cba22124f08b1ceb57e97541d527ce9602684cc099182f4a669e6c6f'),
    bytes.fromhex('0088444241d6188d01fccd85680027068c792c3d874c143eeae52036f94df6059e1f91f8930317e4ff72aed28d02807e1ea098864f7b669261d3'),
    bytes.fromhex('c495fa61d205173fb4778235151268e712b405bf0ebea3bfcee83f34da0b3762e128f04b10eb67e97941e5270e568409d3c099182f4a669e6c6f'),
    bytes.fromhex('0088444241d6188d01fccd85680027068c792c3d874c143eeae52036f94c09fa61d35d0b93c31be4f372a2d24d32a01f2da098864f7b669261d3'),
]
