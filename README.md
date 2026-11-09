# BlueCaster (formerly A2DP-Windows-Bridge)

Bluetooth audio transmitter for Windows. It turns an ordinary Bluetooth adapter, a cheap USB dongle or on many laptops the built-in one, from the basic codecs Windows gives it into a Hi-Fi transmitter: LDAC, LHDC V5 and aptX Adaptive at up to 96 kHz (LHDC V5 up to 192 kHz), plus AAC, SBC XQ and aptX. Windows itself offers none of the first three. BlueCaster sets the adapter up by itself and gives it back to Windows with one click ([how it works](https://seiyafunaokajp.github.io/BlueCaster/architecture)).

![BlueCaster streaming LDAC 990 kbps / 96 kHz to Sony WH-1000XM4 (Home screen)](docs/images/app_sample.png)

**[Download](https://github.com/SeiyaFunaokaJP/BlueCaster/releases)** (installer `-setup.exe` or `-portable.zip`) | **[Documentation](https://seiyafunaokajp.github.io/BlueCaster/)**

## Codecs

| Codec | Bitrate | Sample rate | Bit depth | Windows built-in | BlueCaster |
|-------|---------|-------------|-----------|:----------------:|:------:|
| LHDC V5 | 400–1000 kbps, ABR 160–400 | 44.1–192 kHz | 16/24 bit | ❌ | ✅ |
| L2HC (Huawei) | 320/480/640/960 kbps, ABR | 48/96 kHz | 16/24 bit | ❌ | 🧪 |
| LDAC | 330/660/990 kbps | 44.1–96 kHz | 16/24/32 bit | ❌ | ✅ |
| aptX Adaptive | 265/427 kbps (96 kHz: 646) | 48/96 kHz | 24 bit | ❌ | ✅ |
| Opus (Android) | 256 kbps | 48 kHz | 16 bit | ❌ | 🧪 |
| aptX HD | 576 kbps | 44.1/48 kHz | 24 bit | ❌ | 🧪 |
| aptX | 352/384 kbps | 44.1/48 kHz | 16 bit | ✅ | ✅ |
| aptX Low Latency | 352 kbps | 44.1/48 kHz | 16 bit | ❌ | 🧪 |
| AAC | up to 256 kbps | 44.1/48 kHz | 16 bit | Windows 11 | ✅ |
| SBC XQ | up to 552 kbps | 44.1/48 kHz | 16 bit | ❌ | ✅ |
| SBC | up to ~345 kbps | 44.1/48 kHz | 16 bit | ✅ | ✅ |
| FastStream | 198/216 kbps | 44.1/48 kHz | 16 bit | ❌ | 🧪 |
| Opus 05 (PipeWire) | 256 kbps | 48 kHz | 16 bit | ❌ | 🧪 🔧 |
| LC3 (LE Audio) | 32–124 kbps per channel | 16–48 kHz | 16 bit | Windows 11 | 🧪 |

✅ supported · 🧪 experimental: works against a virtual Bluetooth sink, not yet confirmed with real headphones ([reports welcome](https://github.com/SeiyaFunaokaJP/BlueCaster/issues/11)) · 🔧 developer mode only · ❌ not supported

L2HC is Huawei's vendor-specific A2DP codec. Its encoder here is a clean-room C99 port of [L2HC-native-linux](https://github.com/darakcheeff/L2HC-native-linux) (MIT), 10 ms frames with MTU fragmentation. Huawei earbuds advertise the L2HC endpoint only after **Smart HD Audio / L2HC** is enabled in the AI Life app; Auto-select never picks L2HC, choose it on its codec card.

## Requirements

- **Windows 10 (22H2) or 11**, x64 or ARM64 (ARM64 and Windows 10 not yet tried on real hardware)
- **A USB Bluetooth adapter for BlueCaster**. A separate dongle is recommended: a built-in adapter works too if it is a USB device (as in many laptops), but Windows then has no Bluetooth of its own while BlueCaster uses it. See [Recommended Adapters](https://seiyafunaokajp.github.io/BlueCaster/setup#adapters) and the [verified hardware](https://seiyafunaokajp.github.io/BlueCaster/hardware).

### Why does BlueCaster take over a whole adapter?

Windows loads only kernel drivers signed through Microsoft, which requires an EV code signing certificate (several hundred US dollars a year, usually only for companies). BlueCaster instead puts the adapter on Microsoft's own signed WinUSB driver and runs the Bluetooth stack in user mode. If you want something simpler, commercial software with a signed driver works with the built-in Bluetooth.

## Setup

1. **Install**: run `BlueCaster-<version>-win64-setup.exe` (`-arm64-setup.exe` on ARM64; no administrator rights needed), or extract the portable zip and run `bluecaster.exe`. Installing over v1 or A2DP Windows Bridge 2.0.x replaces it and keeps its settings, pairings and firmware.
2. **Plug in the adapter.**
3. **Follow the first-run setup**: choose the adapter and press **Set up this adapter**. It assigns WinUSB (Windows asks for administrator permission) and downloads the firmware Realtek adapters need. Then press **Connect** next to your headphones.
4. After that, connect from **Home** or the tray icon.

> **Warning:** While WinUSB is assigned, Windows' own Bluetooth cannot use the adapter. With the built-in adapter, the PC has no Windows Bluetooth at all until you restore the driver. **Adapters > Restore original driver** gives the adapter back to Windows at any time, and the uninstaller offers it too.

See the [Setup guide](https://seiyafunaokajp.github.io/BlueCaster/setup) and the [Usage guide](https://seiyafunaokajp.github.io/BlueCaster/usage). For dropouts or failed connections: [Dropouts and Connection Problems](https://seiyafunaokajp.github.io/BlueCaster/wireless).

## Features

- **Codec choice**: picks the best codec the headphones support, or the one you choose; ABR on LHDC V5, LDAC, L2HC and aptX Adaptive
- **LE Audio (experimental)**: LC3 to LE Audio headphones and earbuds with a Bluetooth 5.2 adapter; tested only against the Bluetooth emulator ([details](https://seiyafunaokajp.github.io/BlueCaster/usage#le-audio))
- **Capture**: all system audio, a virtual audio device (e.g. VB-CABLE), or one application
- **Runs in the tray**: keeps streaming with the window closed, reconnects after a lost link and after sleep
- **Driver setup in the app**: assigns and restores WinUSB without Zadig; headphones paired in Windows through the adapter connect without pairing again
- **Headphone controls**: volume sync and play / pause / next / previous
- **Diagnostics**: link quality, AFH (avoids your Wi-Fi channels), connection flow, log, HCI capture
- **Command line**: `bluecaster-core.exe --cli` ([CLI Mode](https://seiyafunaokajp.github.io/BlueCaster/usage#cli-mode))
- **Updates**: daily check, updates itself
- **8 languages**: English, Japanese, Chinese (Simplified / Traditional), Korean, German, French, Spanish
- **Headset microphone (experimental)**: the headphones' microphone as a Windows microphone through a virtual audio cable such as VB-CABLE; classic devices play at call quality meanwhile, LE Audio keeps its quality ([details](https://seiyafunaokajp.github.io/BlueCaster/usage#codecs))
- **Works with** third-party apps such as earbud control apps: [Works With](https://seiyafunaokajp.github.io/BlueCaster/works-with) (not supported by this project)

## Building

Requirements: **Rust** 1.90+, **Visual Studio 2022+** with the C++ desktop workload, **Node.js 22** with **pnpm** (`corepack enable`), **Git**, **PowerShell 7**.

```powershell
git clone --recursive https://github.com/SeiyaFunaokaJP/BlueCaster.git
cd BlueCaster
pwsh ./build.ps1                      # bluecaster.exe, bluecaster-core.exe, bluecaster-driver.exe in target/release/
pwsh ./build.ps1 -Release -Arch x64   # installer and portable zip (-Arch arm64 for ARM64)
```

Double-clicking `build.bat` runs the first command. The other switches (debug build, tests, emulator test) are listed at the top of [build.ps1](build.ps1). See [Building](https://seiyafunaokajp.github.io/BlueCaster/building) for the steps one by one and [Architecture](https://seiyafunaokajp.github.io/BlueCaster/architecture) for how the programs fit together.

## License

The BlueCaster source code is dual-licensed under the **Apache License 2.0** ([LICENSE-APACHE](LICENSE-APACHE)) or the **MIT License** ([LICENSE-MIT](LICENSE-MIT)), at your option. See [LICENSE](LICENSE).

> **Important:** The distributed programs include [BTstack](https://github.com/bluekitchen/btstack), whose license allows use and redistribution only for **personal, non-commercial purposes** ("solely for personal benefit and not for any commercial purpose or for monetary gain"). Commercial licenses are available from BlueKitchen GmbH.

Third-party libraries (BTstack, libldac, libopenaptx, fdk-aac, the LHDC V5 encoder, the L2HC encoder, libopus, Tauri, SolidJS and others) are used under their own licenses: see [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).
