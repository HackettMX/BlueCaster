//! Audio COM pieces that the `wasapi` crate does not cover:
//!
//! - [`set_default_endpoint`]: switch the Windows default output device
//!   through the undocumented IPolicyConfig interface (virtual device mode
//!   with "switch the default device automatically", as v1's
//!   audio_device_enum.cpp does).
//! - [`EndpointVolume`]: master volume and mute of a render endpoint
//!   (IAudioEndpointVolume), the primitives of volume sync.
//! - [`session_process_ids`]: the processes that have an audio session, for
//!   the Application capture app picker (IAudioSessionControl2).
//! - [`output_sessions`], [`set_output_sessions`]: the sessions on the default
//!   output device, with their volume and mute (the app volume mixer).
//! - [`capture_sessions_active`]: whether an app records from the capture
//!   side of a render endpoint (headset mode's Auto: a virtual cable's
//!   "CABLE Output" for its "CABLE Input").
//! - [`ComApartment`]: per-thread COM initialisation (MTA).

use core::ffi::c_void;
use core::marker::PhantomData;

use windows::Win32::Devices::FunctionDiscovery::{PKEY_Device_ContainerId, PKEY_DeviceInterface_FriendlyName};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::PKEY_AudioEndpoint_FormFactor;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, ERole, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, WAVEFORMATEX,
    eCapture, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToGUID, PropVariantToStringAlloc, PropVariantToUInt32,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::core::{BOOL, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR};

// ---------------------------------------------------------------------------
// COM apartment
// ---------------------------------------------------------------------------

/// COM initialised on the calling thread for as long as this lives.
///
/// Joins the multithreaded apartment. A thread that already chose a
/// single-threaded apartment (a UI thread) keeps it: COM is usable there
/// too, and changing it is not ours to do, so nothing is undone on drop.
/// `!Send`: CoUninitialize must run on the thread that initialised.
#[derive(Debug)]
pub struct ComApartment {
    uninit: bool,
    _not_send: PhantomData<*const ()>,
}

impl ComApartment {
    pub fn init_mta() -> windows::core::Result<ComApartment> {
        // SAFETY: plain call with no reserved pointer; balanced by
        // CoUninitialize in Drop on the same thread when it succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(ComApartment { uninit: false, _not_send: PhantomData });
        }
        // S_OK and S_FALSE (already initialised) both need a CoUninitialize
        hr.ok()?;
        Ok(ComApartment { uninit: true, _not_send: PhantomData })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: balances the successful CoInitializeEx of init_mta on
            // this same thread (the type is !Send).
            unsafe { CoUninitialize() };
        }
    }
}

// ---------------------------------------------------------------------------
// IPolicyConfig
// ---------------------------------------------------------------------------

/// CPolicyConfigClient (Windows 7 and later)
const CLSID_CPOLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
/// IPolicyConfig. The Vista variant (IPolicyConfigVista) has no
/// ResetDeviceFormat, so its vtable does not match this one; never use it.
const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

/// IPolicyConfig's vtable, in declaration order (v1's MIDL_INTERFACE). Only
/// SetDefaultEndpoint is called, but every slot before it must be in place
/// for its offset to be right. Parameter types follow v1; the opaque ones
/// (DeviceShareMode, PROPERTYKEY references) are pointers either way.
#[repr(C)]
#[allow(non_snake_case, dead_code)]
struct IPolicyConfigVtbl {
    base: IUnknown_Vtbl,
    GetMixFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut *mut WAVEFORMATEX) -> HRESULT,
    GetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut *mut WAVEFORMATEX) -> HRESULT,
    ResetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    SetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut WAVEFORMATEX, *mut WAVEFORMATEX) -> HRESULT,
    GetProcessingPeriod: unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut i64, *mut i64) -> HRESULT,
    SetProcessingPeriod: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut i64) -> HRESULT,
    GetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
    SetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
    GetPropertyValue: unsafe extern "system" fn(
        *mut c_void,
        PCWSTR,
        BOOL,
        *const c_void, // const PROPERTYKEY &
        *mut PROPVARIANT,
    ) -> HRESULT,
    SetPropertyValue: unsafe extern "system" fn(
        *mut c_void,
        PCWSTR,
        BOOL,
        *const c_void, // const PROPERTYKEY &
        *const PROPVARIANT,
    ) -> HRESULT,
    SetDefaultEndpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
    SetEndpointVisibility: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT,
}

/// A CPolicyConfigClient object, queried for IPolicyConfig
struct PolicyConfig {
    /// The IPolicyConfig pointer, held (and released) as an IUnknown
    policy: IUnknown,
}

impl PolicyConfig {
    /// COM must be initialised on the calling thread
    fn new() -> windows::core::Result<PolicyConfig> {
        // SAFETY: creating an in-process COM object; COM is initialised (the
        // caller's contract), and the IUnknown is released on drop.
        let unknown: IUnknown = unsafe { CoCreateInstance(&CLSID_CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)? };
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: valid IID and out pointer; on success `raw` holds one
        // reference to an IPolicyConfig, which `policy` takes over.
        unsafe { unknown.query(&IID_IPOLICY_CONFIG, &mut raw) }.ok()?;
        // SAFETY: `raw` is a non-null interface pointer we own (the
        // QueryInterface succeeded); an IPolicyConfig is an IUnknown.
        let policy = unsafe { IUnknown::from_raw(raw) };
        Ok(PolicyConfig { policy })
    }

    fn set_default_endpoint(&self, device_id: &[u16], role: ERole) -> HRESULT {
        debug_assert_eq!(device_id.last(), Some(&0));
        let this = self.policy.as_raw();
        // SAFETY: a COM object's first field is its vtable pointer, and this
        // pointer came from QueryInterface(IID_IPolicyConfig), so the vtable
        // has IPolicyConfigVtbl's layout.
        let vtbl = unsafe { &**(this as *const *const IPolicyConfigVtbl) };
        // SAFETY: `this` is live while `self` is, and `device_id` is a
        // NUL-terminated device ID that outlives the call.
        unsafe { (vtbl.SetDefaultEndpoint)(this, PCWSTR(device_id.as_ptr()), role) }
    }
}

/// Make `device_id` the default output device for the Console and
/// Multimedia roles, as v1 does (Communications is left alone, so calls keep
/// their own device). COM must be initialised on the calling thread.
///
/// This changes a system-wide user setting.
pub fn set_default_endpoint(device_id: &str) -> windows::core::Result<()> {
    if device_id.is_empty() {
        return Err(windows::Win32::Foundation::E_INVALIDARG.into());
    }
    let policy =
        PolicyConfig::new().inspect_err(|e| tracing::debug!("audio_policy: IPolicyConfig unavailable: {e}"))?;
    if tracing::enabled!(tracing::Level::DEBUG) {
        let previous = enumerator()
            .and_then(|en| render_device(&en, None))
            .and_then(|d| endpoint_id(&d))
            .unwrap_or_else(|e| format!("(none: {e})"));
        tracing::debug!("audio_policy: default output {previous} -> {device_id}");
    }
    let wid: Vec<u16> = device_id.encode_utf16().chain(Some(0)).collect();
    let hr_console = policy.set_default_endpoint(&wid, eConsole);
    let hr_multimedia = policy.set_default_endpoint(&wid, eMultimedia);
    tracing::debug!(
        "audio_policy: SetDefaultEndpoint eConsole 0x{:08x}, eMultimedia 0x{:08x}",
        hr_console.0 as u32,
        hr_multimedia.0 as u32
    );
    if hr_console.is_err() || hr_multimedia.is_err() {
        tracing::warn!(
            "SetDefaultEndpoint failed: eConsole=0x{:08x}, eMultimedia=0x{:08x}",
            hr_console.0 as u32,
            hr_multimedia.0 as u32
        );
    }
    hr_console.ok()?;
    hr_multimedia.ok()
}

// ---------------------------------------------------------------------------
// Device helpers
// ---------------------------------------------------------------------------

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    // SAFETY: creating an in-process COM object; COM is initialised (the
    // callers' contract).
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// A render endpoint by ID, or the default one (Console role) for None
fn render_device(enumerator: &IMMDeviceEnumerator, device_id: Option<&str>) -> windows::core::Result<IMMDevice> {
    match device_id {
        Some(id) => {
            let wid: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
            // SAFETY: `wid` is NUL-terminated and outlives the call.
            unsafe { enumerator.GetDevice(PCWSTR(wid.as_ptr())) }
        }
        // SAFETY: plain call on a live enumerator.
        None => unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) },
    }
}

fn endpoint_id(device: &IMMDevice) -> windows::core::Result<String> {
    // SAFETY: plain call on a live device; the returned string is ours.
    let pwstr = unsafe { device.GetId()? };
    // SAFETY: GetId returned a valid NUL-terminated string.
    let id = unsafe { pwstr.to_string() };
    // SAFETY: the string was allocated by COM for us and is freed once.
    unsafe { CoTaskMemFree(Some(pwstr.0 as *const c_void)) };
    id.map_err(|_| windows::Win32::Foundation::E_UNEXPECTED.into())
}

// ---------------------------------------------------------------------------
// Endpoint volume
// ---------------------------------------------------------------------------

/// Master volume and mute of one render endpoint (IAudioEndpointVolume).
///
/// Bound to the device it was opened on: with `None`, the device that was
/// the default when it was opened. To follow the default device, compare
/// [`EndpointVolume::device_id`] with the current default and reopen.
/// `!Send`: use it on the thread that opened it, with COM initialised
/// there for its lifetime.
#[derive(Debug)]
pub struct EndpointVolume {
    volume: IAudioEndpointVolume,
    id: String,
}

impl EndpointVolume {
    pub fn open(device_id: Option<&str>) -> windows::core::Result<EndpointVolume> {
        let enumerator = enumerator()?;
        let device = render_device(&enumerator, device_id)?;
        let id = endpoint_id(&device)?;
        // SAFETY: activating a documented endpoint service on a live device.
        let volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None)? };
        tracing::trace!("audio_policy: endpoint volume opened on {id} (asked for {device_id:?})");
        Ok(EndpointVolume { volume, id })
    }

    /// The endpoint ID this volume belongs to
    pub fn device_id(&self) -> &str {
        &self.id
    }

    /// Master volume as the scalar the Windows slider shows, 0.0..=1.0
    pub fn volume(&self) -> windows::core::Result<f32> {
        // SAFETY: plain call on a live interface.
        unsafe { self.volume.GetMasterVolumeLevelScalar() }
    }

    /// Set the master volume scalar (clamped to 0.0..=1.0). No event
    /// context, as in v1: listeners see an ordinary volume change.
    pub fn set_volume(&self, level: f32) -> windows::core::Result<()> {
        let level = if level.is_nan() { 0.0 } else { level.clamp(0.0, 1.0) };
        tracing::trace!("audio_policy: volume of {} set to {level:.3}", self.id);
        // SAFETY: plain call; a null event context GUID is allowed.
        unsafe { self.volume.SetMasterVolumeLevelScalar(level, core::ptr::null()) }
    }

    pub fn muted(&self) -> windows::core::Result<bool> {
        // SAFETY: plain call on a live interface.
        Ok(unsafe { self.volume.GetMute()? }.as_bool())
    }

    pub fn set_muted(&self, muted: bool) -> windows::core::Result<()> {
        tracing::trace!("audio_policy: mute of {} set to {muted}", self.id);
        // SAFETY: plain call; a null event context GUID is allowed.
        unsafe { self.volume.SetMute(muted, core::ptr::null()) }
    }
}

// ---------------------------------------------------------------------------
// Endpoint properties
// ---------------------------------------------------------------------------

/// Endpoint properties that hint at a virtual device (v1's
/// is_virtual_device heuristic reads these after the name patterns)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndpointHints {
    /// PKEY_AudioEndpoint_FormFactor (EndpointFormFactor; 8 = Unknown)
    pub form_factor: Option<u32>,
    /// PKEY_Device_ContainerId as a u128 (GUID::to_u128)
    pub container_id: Option<u128>,
}

/// Read the [`EndpointHints`] of a render or capture endpoint. COM must be
/// initialised on the calling thread.
pub fn endpoint_hints(device_id: &str) -> windows::core::Result<EndpointHints> {
    let enumerator = enumerator()?;
    let device = render_device(&enumerator, Some(device_id))?;
    // SAFETY: plain call on a live device.
    let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
    let read = |key: &PROPERTYKEY| -> Option<PROPVARIANT> {
        // SAFETY: `key` is a valid PROPERTYKEY; the value is ours to clear.
        unsafe { store.GetValue(key) }.ok()
    };
    let mut hints = EndpointHints::default();
    if let Some(mut v) = read(&PKEY_AudioEndpoint_FormFactor) {
        // SAFETY: `v` is an initialised PROPVARIANT; VT_EMPTY (property not
        // set) makes the conversion fail, which reads as "unknown".
        hints.form_factor = unsafe { PropVariantToUInt32(&v) }.ok();
        // SAFETY: clears the value GetValue gave us, once.
        let _ = unsafe { PropVariantClear(&mut v) };
    }
    if let Some(mut v) = read(&PKEY_Device_ContainerId) {
        // SAFETY: as above, for a VT_CLSID value.
        hints.container_id = unsafe { PropVariantToGUID(&v) }.ok().map(|g| g.to_u128());
        // SAFETY: clears the value GetValue gave us, once.
        let _ = unsafe { PropVariantClear(&mut v) };
    }
    tracing::trace!("audio_policy: hints of {device_id}: {hints:?}");
    Ok(hints)
}

// ---------------------------------------------------------------------------
// Audio sessions
// ---------------------------------------------------------------------------

/// A session on the default output device
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputSession {
    pub process_id: u32,
    pub system_sounds: bool,
    /// 0.0-1.0
    pub volume: f32,
    pub muted: bool,
}

/// The sessions on the default output device, without expired ones. COM must
/// be initialised on the calling thread.
fn default_output_sessions() -> windows::core::Result<Vec<(OutputSession, ISimpleAudioVolume)>> {
    let enumerator = enumerator()?;
    // SAFETY: plain call on a live enumerator.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
    // SAFETY: activating a documented endpoint service on a live device.
    let manager = unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)? };
    // SAFETY: plain call on a live session manager.
    let sessions = unsafe { manager.GetSessionEnumerator()? };
    // SAFETY: plain call on a live session enumerator.
    let n = unsafe { sessions.GetCount()? };
    let mut out = Vec::new();
    for s in 0..n {
        // SAFETY: `s` is within the enumerator's count.
        let Ok(control) = (unsafe { sessions.GetSession(s) }) else {
            continue;
        };
        // SAFETY: plain call on a live session control.
        if unsafe { control.GetState() }.is_ok_and(|state| state == AudioSessionStateExpired) {
            continue;
        }
        let (Ok(control2), Ok(simple)) =
            (control.cast::<IAudioSessionControl2>(), control.cast::<ISimpleAudioVolume>())
        else {
            continue;
        };
        // SAFETY: plain calls on live session interfaces.
        let session = unsafe {
            OutputSession {
                process_id: control2.GetProcessId().unwrap_or(0),
                system_sounds: control2.IsSystemSoundsSession() == S_OK,
                volume: simple.GetMasterVolume().unwrap_or(1.0),
                muted: simple.GetMute().is_ok_and(|m| m.as_bool()),
            }
        };
        out.push((session, simple));
    }
    Ok(out)
}

/// The sessions on the default output device. COM must be initialised on the
/// calling thread.
pub fn output_sessions() -> windows::core::Result<Vec<OutputSession>> {
    Ok(default_output_sessions()?.into_iter().map(|(s, _)| s).collect())
}

/// Sets the volume (0.0-1.0) and / or the mute of the sessions `which` picks;
/// returns how many. COM must be initialised on the calling thread.
pub fn set_output_sessions(
    which: impl Fn(&OutputSession) -> bool,
    volume: Option<f32>,
    muted: Option<bool>,
) -> windows::core::Result<usize> {
    let mut count = 0;
    for (session, simple) in default_output_sessions()? {
        if !which(&session) {
            continue;
        }
        if let Some(v) = volume {
            // SAFETY: plain call on a live interface; no event context.
            unsafe { simple.SetMasterVolume(v, core::ptr::null())? };
        }
        if let Some(m) = muted {
            // SAFETY: plain call on a live interface; no event context.
            unsafe { simple.SetMute(m, core::ptr::null())? };
        }
        count += 1;
    }
    Ok(count)
}

/// Process IDs of the app audio sessions on all active output devices, in
/// enumeration order, with duplicates (one per session). Leaves out the
/// system sounds session. COM must be initialised on the calling thread.
pub fn session_process_ids() -> windows::core::Result<Vec<u32>> {
    let enumerator = enumerator()?;
    // SAFETY: plain call on a live enumerator.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
    // SAFETY: plain call on a live collection.
    let count = unsafe { devices.GetCount()? };
    let mut pids = Vec::new();
    for i in 0..count {
        // SAFETY: `i` is within the collection's count.
        let Ok(device) = (unsafe { devices.Item(i) }) else {
            continue;
        };
        // SAFETY: activating a documented endpoint service on a live device.
        let Ok(manager) = (unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) }) else {
            continue;
        };
        // SAFETY: plain call on a live session manager.
        let Ok(sessions) = (unsafe { manager.GetSessionEnumerator() }) else {
            continue;
        };
        // SAFETY: plain call on a live session enumerator.
        let n = unsafe { sessions.GetCount() }.unwrap_or(0);
        for s in 0..n {
            // SAFETY: `s` is within the enumerator's count.
            let Ok(control) = (unsafe { sessions.GetSession(s) }) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            // S_OK = the system sounds session, not an app
            // SAFETY: plain call on a live session control.
            if unsafe { control2.IsSystemSoundsSession() } == S_OK {
                continue;
            }
            // A session shared by several processes returns
            // AUDCLNT_S_NO_SINGLE_PROCESS, a success code, with one of them
            // SAFETY: plain call on a live session control.
            if let Ok(pid) = unsafe { control2.GetProcessId() }
                && pid != 0
            {
                pids.push(pid);
            }
        }
    }
    tracing::trace!("audio_policy: {count} output device(s), app sessions of {pids:?}");
    Ok(pids)
}

// ---------------------------------------------------------------------------
// Recording from a render endpoint's capture side
// ---------------------------------------------------------------------------

/// What pairs a render endpoint with capture endpoints
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Pairing {
    /// PKEY_Device_ContainerId; None for the null / all-zero container
    /// (internal and software devices share it, so it pairs nothing)
    container: Option<u128>,
    /// PKEY_DeviceInterface_FriendlyName: the adapter's name ("VB-Audio
    /// Virtual Cable" for both "CABLE Input" and "CABLE Output")
    adapter: Option<String>,
}

/// The null container {00000000-0000-0000-ffff-ffffffffffff}
const NULL_CONTAINER: u128 = 0x0000_0000_0000_0000_ffff_ffff_ffff_ffff;

/// The capture endpoints (indices into `captures`) paired with `render`:
/// those in the render endpoint's container; when none is, those of the
/// same adapter
fn paired(render: &Pairing, captures: &[Pairing]) -> Vec<usize> {
    let which = |f: &dyn Fn(&Pairing) -> bool| -> Vec<usize> {
        captures.iter().enumerate().filter(|(_, c)| f(c)).map(|(i, _)| i).collect()
    };
    let same_container = match render.container {
        Some(id) => which(&|c| c.container == Some(id)),
        None => Vec::new(),
    };
    if !same_container.is_empty() {
        return same_container;
    }
    match render.adapter.as_deref() {
        Some(name) => which(&|c| c.adapter.as_deref() == Some(name)),
        None => Vec::new(),
    }
}

fn pairing(device: &IMMDevice) -> Pairing {
    // SAFETY: plain call on a live device.
    let Ok(store) = (unsafe { device.OpenPropertyStore(STGM_READ) }) else {
        return Pairing::default();
    };
    let read = |key: &PROPERTYKEY| -> Option<PROPVARIANT> {
        // SAFETY: `key` is a valid PROPERTYKEY; the value is ours to clear.
        unsafe { store.GetValue(key) }.ok()
    };
    let mut out = Pairing::default();
    if let Some(mut v) = read(&PKEY_Device_ContainerId) {
        // SAFETY: an initialised PROPVARIANT (VT_CLSID; VT_EMPTY fails).
        out.container =
            unsafe { PropVariantToGUID(&v) }.ok().map(|g| g.to_u128()).filter(|&id| id != 0 && id != NULL_CONTAINER);
        // SAFETY: clears the value GetValue gave us, once.
        let _ = unsafe { PropVariantClear(&mut v) };
    }
    if let Some(mut v) = read(&PKEY_DeviceInterface_FriendlyName) {
        // SAFETY: an initialised PROPVARIANT (VT_LPWSTR; VT_EMPTY fails).
        if let Ok(pwstr) = unsafe { PropVariantToStringAlloc(&v) } {
            // SAFETY: a valid NUL-terminated string from COM.
            out.adapter = unsafe { pwstr.to_string() }.ok().filter(|s| !s.trim().is_empty());
            // SAFETY: allocated by COM for us, freed once.
            unsafe { CoTaskMemFree(Some(pwstr.0 as *const c_void)) };
        }
        // SAFETY: clears the value GetValue gave us, once.
        let _ = unsafe { PropVariantClear(&mut v) };
    }
    out
}

/// Whether a session on `device` records (AudioSessionStateActive), this
/// process's own left out
fn recording(device: &IMMDevice) -> windows::core::Result<bool> {
    let own = std::process::id();
    // SAFETY: activating a documented endpoint service on a live device.
    let manager = unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)? };
    // SAFETY: plain call on a live session manager.
    let sessions = unsafe { manager.GetSessionEnumerator()? };
    // SAFETY: plain call on a live session enumerator.
    let n = unsafe { sessions.GetCount()? };
    for s in 0..n {
        // SAFETY: `s` is within the enumerator's count.
        let Ok(control) = (unsafe { sessions.GetSession(s) }) else {
            continue;
        };
        // SAFETY: plain call on a live session control.
        if !unsafe { control.GetState() }.is_ok_and(|state| state == AudioSessionStateActive) {
            continue;
        }
        // A session of several processes answers with one of them
        let pid = control
            .cast::<IAudioSessionControl2>()
            .ok()
            // SAFETY: plain call on a live session control.
            .and_then(|c| unsafe { c.GetProcessId() }.ok());
        if pid != Some(own) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether an application records from the capture endpoints paired with
/// the render endpoint `render_device_id` (a virtual cable's "CABLE Output"
/// for its "CABLE Input"): an audio session on one of them in the Active
/// state, this process's own left out.
///
/// Paired: the active capture endpoints in the render endpoint's container
/// (PKEY_Device_ContainerId; the null container of internal and software
/// devices pairs nothing), else those of the same adapter
/// (PKEY_DeviceInterface_FriendlyName). `Ok(None)`: no capture endpoint is
/// paired with it. An error: the render endpoint or the capture endpoints
/// cannot be read. COM must be initialised on the calling thread.
pub fn capture_sessions_active(render_device_id: &str) -> windows::core::Result<Option<bool>> {
    let enumerator = enumerator()?;
    let render = pairing(&render_device(&enumerator, Some(render_device_id))?);
    // SAFETY: plain call on a live enumerator.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)? };
    // SAFETY: plain call on a live collection.
    let count = unsafe { devices.GetCount()? };
    let mut captures = Vec::new();
    for i in 0..count {
        // SAFETY: `i` is within the collection's count.
        if let Ok(device) = unsafe { devices.Item(i) } {
            captures.push((pairing(&device), device));
        }
    }
    let keys: Vec<Pairing> = captures.iter().map(|(p, _)| p.clone()).collect();
    let paired = paired(&render, &keys);
    if paired.is_empty() {
        tracing::trace!("audio_policy: no capture endpoint paired with {render_device_id} ({render:?})");
        return Ok(None);
    }
    for i in paired {
        // A capture endpoint whose sessions cannot be read counts as idle
        match recording(&captures[i].1) {
            Ok(true) => return Ok(Some(true)),
            Ok(false) => {}
            Err(e) => tracing::trace!("audio_policy: sessions of capture endpoint {i}: {e}"),
        }
    }
    Ok(Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn com_apartment_nests() {
        let a = ComApartment::init_mta().unwrap();
        let b = ComApartment::init_mta().unwrap();
        drop(b);
        drop(a);
    }

    #[test]
    fn read_default_endpoint_volume() {
        let _com = ComApartment::init_mta().unwrap();
        // A PC without any output device has no default endpoint
        let Ok(v) = EndpointVolume::open(None) else {
            return;
        };
        assert!(!v.device_id().is_empty());
        let level = v.volume().unwrap();
        assert!((0.0..=1.0).contains(&level));
        let _ = v.muted().unwrap();
        // Same endpoint by ID
        let again = EndpointVolume::open(Some(v.device_id())).unwrap();
        assert_eq!(again.device_id(), v.device_id());
        // Every endpoint has a container ID (real or the null one)
        let hints = endpoint_hints(v.device_id()).unwrap();
        assert!(hints.container_id.is_some(), "{hints:?}");
    }

    #[test]
    fn capture_endpoints_pair_by_container_then_adapter() {
        let ep =
            |container: Option<u128>, adapter: Option<&str>| Pairing { container, adapter: adapter.map(Into::into) };
        let headset = ep(Some(7), Some("USB Audio"));
        let caps = [ep(None, Some("Realtek(R) Audio")), headset.clone(), ep(None, Some("VB-Audio Virtual Cable"))];
        // The same container: the headset's microphone
        assert_eq!(paired(&headset, &caps), vec![1]);
        // No container of its own: the cable's capture side by its adapter
        assert_eq!(paired(&ep(None, Some("VB-Audio Virtual Cable")), &caps), vec![2]);
        // A container no capture endpoint has: the adapter decides
        assert_eq!(paired(&ep(Some(9), Some("Realtek(R) Audio")), &caps), vec![0]);
        // Nothing in common
        assert!(paired(&ep(Some(9), Some("HDMI")), &caps).is_empty());
        assert!(paired(&ep(None, None), &caps).is_empty());
    }

    /// Prints, for every output device, what pairs it and whether an app
    /// records from its capture side (VB-CABLE: record "CABLE Output" to
    /// see Some(true) for "CABLE Input")
    #[test]
    #[ignore]
    fn print_capture_sessions() {
        let _com = ComApartment::init_mta().unwrap();
        let enumerator = enumerator().unwrap();
        // SAFETY: plain calls on a live enumerator and collection.
        let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).unwrap() };
        // SAFETY: as above.
        for i in 0..unsafe { devices.GetCount().unwrap() } {
            // SAFETY: `i` is within the collection's count.
            let device = unsafe { devices.Item(i).unwrap() };
            let id = endpoint_id(&device).unwrap();
            println!("{id} {:?}: {:?}", pairing(&device), capture_sessions_active(&id));
        }
        // SAFETY: as above.
        let captures = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).unwrap() };
        // SAFETY: as above.
        for i in 0..unsafe { captures.GetCount().unwrap() } {
            // SAFETY: `i` is within the collection's count.
            let device = unsafe { captures.Item(i).unwrap() };
            println!("capture {} {:?}: {:?}", endpoint_id(&device).unwrap(), pairing(&device), recording(&device));
        }
    }

    #[test]
    fn sessions_enumerate() {
        let _com = ComApartment::init_mta().unwrap();
        let pids = session_process_ids().unwrap();
        assert!(!pids.contains(&0));
    }

    #[test]
    fn policy_config_is_available() {
        // Creating the object and querying IPolicyConfig changes nothing;
        // SetDefaultEndpoint itself is never called by tests.
        let _com = ComApartment::init_mta().unwrap();
        PolicyConfig::new().expect("CPolicyConfigClient / IPolicyConfig");
        assert!(set_default_endpoint("").is_err());
    }
}
