//! Development helper for headset mode's Auto: records (into nothing) from a
//! capture endpoint for a while, so that an audio session is active on it,
//! as an application using the microphone would.
//!
//!     cargo run -p win-sys --example hold_capture -- "CABLE Output" 12
//!
//! The first argument is part of the endpoint's friendly name, the second
//! the seconds to hold (default 10).

use std::time::{Duration, Instant};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, eCapture,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ};

fn main() -> windows::core::Result<()> {
    let mut args = std::env::args().skip(1);
    let wanted = args.next().unwrap_or_else(|| "CABLE Output".into()).to_lowercase();
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    // SAFETY: plain COM initialisation on this thread.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    // SAFETY: documented COM calls on live objects.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let mut found = None;
        for i in 0..devices.GetCount()? {
            let device = devices.Item(i)?;
            let store = device.OpenPropertyStore(STGM_READ)?;
            let value = store.GetValue(&PKEY_Device_FriendlyName)?;
            let name = PropVariantToStringAlloc(&value).map(|p| p.to_string().unwrap_or_default()).unwrap_or_default();
            if name.to_lowercase().contains(&wanted) {
                found = Some((device, name));
                break;
            }
        }
        let Some((device, name)) = found else {
            eprintln!("no active capture endpoint whose name contains {wanted:?}");
            std::process::exit(1);
        };
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 10_000_000, 0, format, None)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        println!("recording from {name} for {secs} s");
        let until = Instant::now() + Duration::from_secs(secs);
        let mut frames = 0u64;
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
            // Drain what arrived so the stream keeps flowing
            loop {
                let mut data = std::ptr::null_mut();
                let mut n = 0u32;
                let mut flags = 0u32;
                if capture.GetBuffer(&mut data, &mut n, &mut flags, None, None).is_err() || n == 0 {
                    break;
                }
                frames += n as u64;
                let _ = capture.ReleaseBuffer(n);
            }
        }
        client.Stop()?;
        println!("done: {frames} frames");
    }
    Ok(())
}
