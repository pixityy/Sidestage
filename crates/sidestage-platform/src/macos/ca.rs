//! Checked wrappers over the Core Audio HAL: properties, process taps, aggregate devices,
//! IOProcs and property listeners. Every object that Core Audio hands out is owned by a Rust
//! value whose `Drop` releases it, so teardown also happens on early returns and panics.

use std::ffi::c_void;
use std::fmt;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

use anyhow::{Context, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectAddPropertyListenerBlock, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListenerBlock,
    AudioObjectSetPropertyData, CATapDescription, CATapMuteBehavior, kAudioDevicePropertyIOProcStreamUsage,
    kAudioHardwarePropertyRunLoop, kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, kAudioObjectUnknown, kAudioTapPropertyDescription,
    kAudioTapPropertyFormat,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudioFormatFlagIsBigEndian,
    kAudioFormatFlagIsFloat, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSInteger, NSNumber, NSString};

use super::plan::{Plist, StreamUsage};
use super::sys::fourcc;

pub type OSStatus = i32;

/// The HAL's system object (`kAudioObjectSystemObject` is declared as a C `int`).
pub const SYSTEM_OBJECT: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

/// A failed Core Audio call.
#[derive(Debug)]
pub struct CaError {
    pub call: &'static str,
    pub status: OSStatus,
}

impl fmt::Display for CaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed with {}", self.call, fourcc(self.status))
    }
}

impl std::error::Error for CaError {}

pub fn check(status: OSStatus, call: &'static str) -> Result<(), CaError> {
    if status == 0 { Ok(()) } else { Err(CaError { call, status }) }
}

pub const fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain }
}

/// Four-character name of a property selector for error messages.
fn selector_name(selector: u32) -> String {
    selector.to_be_bytes().iter().map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '?' }).collect()
}

/// Reads a fixed-size property, optionally with qualifier data.
///
/// # Safety
/// `T` must be plain data for which all-zero bytes and whatever the HAL writes for this
/// property are valid values (integers, floats, raw pointers, `AudioStreamBasicDescription`).
pub unsafe fn get_qualified<T: Copy, Q>(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
    qualifier: Option<&Q>,
) -> anyhow::Result<T> {
    let addr = address(selector, scope);
    let mut value = MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    let (qsize, qptr) = match qualifier {
        Some(q) => (size_of::<Q>() as u32, ptr::from_ref(q).cast::<c_void>()),
        None => (0, ptr::null()),
    };
    // SAFETY: every pointer refers to a live local of the advertised size.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            qsize,
            qptr,
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    check(status, "AudioObjectGetPropertyData")
        .with_context(|| format!("reading '{}' of audio object {object}", selector_name(selector)))?;
    if size as usize != size_of::<T>() {
        bail!(
            "audio object {object} returned {size} bytes for '{}', expected {}",
            selector_name(selector),
            size_of::<T>()
        );
    }
    // SAFETY: zero-initialized and fully written by the HAL; `T` accepts both (caller contract).
    Ok(unsafe { value.assume_init() })
}

/// Reads a fixed-size property. Same contract as [`get_qualified`].
pub unsafe fn get<T: Copy>(object: AudioObjectID, selector: u32, scope: u32) -> anyhow::Result<T> {
    // SAFETY: forwarded caller contract.
    unsafe { get_qualified::<T, ()>(object, selector, scope, None) }
}

pub fn get_u32(object: AudioObjectID, selector: u32, scope: u32) -> anyhow::Result<u32> {
    // SAFETY: any bit pattern is a valid u32.
    unsafe { get(object, selector, scope) }
}

pub fn get_f64(object: AudioObjectID, selector: u32, scope: u32) -> anyhow::Result<f64> {
    // SAFETY: any bit pattern is a valid f64.
    unsafe { get(object, selector, scope) }
}

pub fn get_asbd(object: AudioObjectID, selector: u32, scope: u32) -> anyhow::Result<AudioStreamBasicDescription> {
    // SAFETY: AudioStreamBasicDescription is plain integers and a float.
    unsafe { get(object, selector, scope) }
}

/// Reads a variable-length array of `u32` (object ID lists).
pub fn get_ids(object: AudioObjectID, selector: u32, scope: u32) -> anyhow::Result<Vec<u32>> {
    let addr = address(selector, scope);
    let mut size = 0u32;
    // SAFETY: valid address and out pointer.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(object, NonNull::from(&addr), 0, ptr::null(), NonNull::from(&mut size))
    };
    check(status, "AudioObjectGetPropertyDataSize")
        .with_context(|| format!("sizing '{}' of audio object {object}", selector_name(selector)))?;
    let mut ids = vec![0u32; size as usize / size_of::<u32>()];
    if ids.is_empty() {
        return Ok(ids);
    }
    let mut size = (ids.len() * size_of::<u32>()) as u32;
    // SAFETY: `ids` has room for `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut ids[0]).cast(),
        )
    };
    check(status, "AudioObjectGetPropertyData")
        .with_context(|| format!("reading '{}' of audio object {object}", selector_name(selector)))?;
    ids.truncate(size as usize / size_of::<u32>());
    Ok(ids)
}

/// Reads a CFString property. The HAL returns these retained (the Create rule), so the string
/// is adopted with `from_raw`, never retained again (which would leak it).
pub fn get_string(object: AudioObjectID, selector: u32, scope: u32) -> anyhow::Result<Option<String>> {
    // SAFETY: a raw pointer may hold any value, including the zeroed null.
    let raw: *const CFString = unsafe { get(object, selector, scope)? };
    // SAFETY: non-null CFString handed to us with +1 retain count.
    Ok(NonNull::new(raw.cast_mut()).map(|p| unsafe { CFRetained::from_raw(p) }.to_string()))
}

/// Writes a fixed-size property.
///
/// # Safety
/// `T` must be the type the HAL expects for this property.
pub unsafe fn set<T>(object: AudioObjectID, selector: u32, scope: u32, value: &T) -> anyhow::Result<()> {
    let addr = address(selector, scope);
    // SAFETY: `value` is live and `size_of::<T>()` bytes long; the HAL only reads it.
    let status = unsafe {
        AudioObjectSetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            ptr::null(),
            size_of::<T>() as u32,
            NonNull::from(value).cast(),
        )
    };
    check(status, "AudioObjectSetPropertyData")
        .with_context(|| format!("writing '{}' of audio object {object}", selector_name(selector)))?;
    Ok(())
}

/// Channel count of every stream in `kAudioDevicePropertyStreamConfiguration` (a
/// variable-length `AudioBufferList`, one buffer per stream).
pub fn stream_channels(object: AudioObjectID, selector: u32, scope: u32) -> anyhow::Result<Vec<usize>> {
    let addr = address(selector, scope);
    let mut size = 0u32;
    // SAFETY: valid address and out pointer.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(object, NonNull::from(&addr), 0, ptr::null(), NonNull::from(&mut size))
    };
    check(status, "AudioObjectGetPropertyDataSize").context("sizing the stream configuration")?;
    if (size as usize) < size_of::<u32>() {
        return Ok(Vec::new());
    }
    // u64 storage keeps the list's pointer-aligned layout.
    let mut storage = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: `storage` has room for `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut storage[0]).cast(),
        )
    };
    check(status, "AudioObjectGetPropertyData").context("reading the stream configuration")?;
    let bytes: Vec<u8> = storage.iter().flat_map(|w| w.to_ne_bytes()).take(size as usize).collect();
    let read_u32 = |at: usize| -> Option<u32> {
        let mut b = [0u8; 4];
        b.copy_from_slice(bytes.get(at..at + 4)?);
        Some(u32::from_ne_bytes(b))
    };
    let count = read_u32(0).unwrap_or(0) as usize;
    let first = std::mem::offset_of!(AudioBufferList, mBuffers);
    let channels_at = std::mem::offset_of!(AudioBuffer, mNumberChannels);
    (0..count)
        .map(|i| {
            read_u32(first + i * size_of::<AudioBuffer>() + channels_at)
                .map(|c| c as usize)
                .context("truncated stream configuration")
        })
        .collect()
}

/// Interleaved native-endian 32-bit float PCM, the only sample format the IOProcs handle.
pub fn is_float32(f: &AudioStreamBasicDescription) -> bool {
    f.mFormatID == kAudioFormatLinearPCM
        && f.mFormatFlags & kAudioFormatFlagIsFloat != 0
        && f.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
        && f.mBitsPerChannel == 32
}

/// Which processes a tap covers, and how.
pub enum TapKind<'a> {
    /// The listed processes' audio for one output device stream, in that stream's format.
    DeviceStream { device_uid: &'a str, stream: usize },
    /// The listed processes mixed to stereo, whatever device they play to.
    StereoMixdown,
    /// Every process except the listed ones, mixed to stereo.
    GlobalExcluding,
}

fn number_array(objects: &[AudioObjectID]) -> Retained<NSArray<NSNumber>> {
    let numbers: Vec<Retained<NSNumber>> = objects.iter().map(|&o| NSNumber::new_u32(o)).collect();
    NSArray::from_retained_slice(&numbers)
}

/// A private `CATapDescription`. Call inside an autorelease pool.
pub fn tap_description(
    kind: &TapKind<'_>,
    objects: &[AudioObjectID],
    mute: CATapMuteBehavior,
    name: &str,
) -> Retained<CATapDescription> {
    let processes = number_array(objects);
    // SAFETY: plain initializers on a fresh allocation, with valid arguments.
    let desc = unsafe {
        match kind {
            TapKind::DeviceStream { device_uid, stream } => {
                CATapDescription::initWithProcesses_andDeviceUID_withStream(
                    CATapDescription::alloc(),
                    &processes,
                    &NSString::from_str(device_uid),
                    NSInteger::try_from(*stream).unwrap_or(0),
                )
            }
            TapKind::StereoMixdown => {
                CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &processes)
            }
            TapKind::GlobalExcluding => {
                CATapDescription::initStereoGlobalTapButExcludeProcesses(CATapDescription::alloc(), &processes)
            }
        }
    };
    // SAFETY: setters on a valid description.
    unsafe {
        desc.setName(&NSString::from_str(name));
        // Invisible to other apps' tap lists.
        desc.setPrivate(true);
        desc.setMuteBehavior(mute);
    }
    desc
}

/// A process tap; destroyed on drop, which also ends any muting it caused.
pub struct Tap {
    id: AudioObjectID,
    uid: String,
    desc: Retained<CATapDescription>,
}

impl Tap {
    pub fn create(desc: Retained<CATapDescription>) -> anyhow::Result<Self> {
        let mut id = kAudioObjectUnknown;
        // SAFETY: valid description and out pointer.
        let status = unsafe { AudioHardwareCreateProcessTap(Some(&*desc), &mut id) };
        check(status, "AudioHardwareCreateProcessTap")?;
        if id == kAudioObjectUnknown {
            bail!("AudioHardwareCreateProcessTap returned no tap (the output device may be gone)");
        }
        // SAFETY: plain getters.
        let uid = unsafe { desc.UUID().UUIDString() }.to_string();
        Ok(Self { id, uid, desc })
    }

    pub fn uid(&self) -> &str {
        &self.uid
    }

    pub fn format(&self) -> anyhow::Result<AudioStreamBasicDescription> {
        get_asbd(self.id, kAudioTapPropertyFormat, kAudioObjectPropertyScopeGlobal).context("reading the tap format")
    }

    /// Replaces the tap's process list (the excluded processes, for a global tap) without
    /// rebuilding, as Chromium does when an app gains or loses helper processes.
    pub fn set_processes(&self, objects: &[AudioObjectID]) -> anyhow::Result<()> {
        // SAFETY: setter on a valid description.
        unsafe { self.desc.setProcesses(&number_array(objects)) };
        self.write_description(&self.desc).context("updating the tap's process list")
    }

    fn write_description(&self, desc: &CATapDescription) -> anyhow::Result<()> {
        let ptr: *const CATapDescription = desc;
        // SAFETY: the property is a `CATapDescription *`; the HAL does not take ownership.
        unsafe { set(self.id, kAudioTapPropertyDescription, kAudioObjectPropertyScopeGlobal, &ptr) }
    }

    /// Chromium's capture-permission probe: reading the tap's description and writing it back
    /// fails when the user has not allowed audio capture (while the tap itself just delivers
    /// silence and still mutes the app).
    pub fn probe_permission(&self) -> anyhow::Result<()> {
        // SAFETY: a raw pointer may hold any value.
        let raw: *mut CATapDescription =
            unsafe { get(self.id, kAudioTapPropertyDescription, kAudioObjectPropertyScopeGlobal)? };
        // SAFETY: the HAL returns the description retained (+1); adopt it.
        let desc = unsafe { Retained::from_raw(raw) }.context("the tap returned no description")?;
        self.write_description(&desc)
    }

    pub fn close(&mut self) -> anyhow::Result<()> {
        let id = std::mem::replace(&mut self.id, kAudioObjectUnknown);
        if id == kAudioObjectUnknown {
            return Ok(());
        }
        // SAFETY: a tap we created and have not destroyed.
        let status = unsafe { AudioHardwareDestroyProcessTap(id) };
        check(status, "AudioHardwareDestroyProcessTap")?;
        Ok(())
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            log::warn!("destroying process tap: {e:#}");
        }
    }
}

/// Core Foundation objects built from a [`Plist`], kept alive while referenced.
enum Cf {
    Str(CFRetained<CFString>),
    Bool(&'static CFBoolean),
    Array(CFRetained<CFArray<CFType>>),
    Dict(CFRetained<CFDictionary<CFString, CFType>>),
}

impl Cf {
    fn build(value: &Plist) -> Self {
        match value {
            Plist::Str(s) => Cf::Str(CFString::from_str(s)),
            Plist::Bool(b) => Cf::Bool(CFBoolean::new(*b)),
            Plist::Array(items) => {
                let items: Vec<Cf> = items.iter().map(Cf::build).collect();
                let refs: Vec<&CFType> = items.iter().map(Cf::as_type).collect();
                Cf::Array(CFArray::from_objects(&refs))
            }
            Plist::Dict(entries) => {
                let keys: Vec<CFRetained<CFString>> = entries.iter().map(|(k, _)| CFString::from_str(k)).collect();
                let values: Vec<Cf> = entries.iter().map(|(_, v)| Cf::build(v)).collect();
                let key_refs: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
                let value_refs: Vec<&CFType> = values.iter().map(Cf::as_type).collect();
                Cf::Dict(CFDictionary::from_slices(&key_refs, &value_refs))
            }
        }
    }

    fn as_type(&self) -> &CFType {
        match self {
            Cf::Str(s) => s,
            Cf::Bool(b) => b,
            Cf::Array(a) => a,
            Cf::Dict(d) => d,
        }
    }
}

/// A private aggregate device; destroyed on drop.
pub struct Aggregate {
    id: AudioObjectID,
}

impl Aggregate {
    pub fn create(description: &Plist) -> anyhow::Result<Self> {
        let Cf::Dict(dict) = Cf::build(description) else {
            bail!("an aggregate device description must be a dictionary");
        };
        let mut id = kAudioObjectUnknown;
        // SAFETY: a CFDictionary of CFString keys to property-list values, and an out pointer.
        let status = unsafe { AudioHardwareCreateAggregateDevice(dict.as_opaque(), NonNull::from(&mut id)) };
        check(status, "AudioHardwareCreateAggregateDevice")?;
        if id == kAudioObjectUnknown {
            bail!("AudioHardwareCreateAggregateDevice returned no device");
        }
        Ok(Self { id })
    }

    pub fn id(&self) -> AudioObjectID {
        self.id
    }

    pub fn close(&mut self) -> anyhow::Result<()> {
        let id = std::mem::replace(&mut self.id, kAudioObjectUnknown);
        if id == kAudioObjectUnknown {
            return Ok(());
        }
        // SAFETY: an aggregate we created and have not destroyed.
        let status = unsafe { AudioHardwareDestroyAggregateDevice(id) };
        check(status, "AudioHardwareDestroyAggregateDevice")?;
        Ok(())
    }
}

impl Drop for Aggregate {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            log::warn!("destroying aggregate device: {e:#}");
        }
    }
}

/// IOProc signature with raw pointers. The bindings declare the buffer arguments `NonNull`,
/// but nothing in the HAL documentation promises that (a device without output streams may
/// pass NULL output data), and a null `NonNull` would be undefined behaviour on entry.
pub type RawIoProc = unsafe extern "C-unwind" fn(
    AudioObjectID,
    *const AudioTimeStamp,
    *const AudioBufferList,
    *const AudioTimeStamp,
    *mut AudioBufferList,
    *const AudioTimeStamp,
    *mut c_void,
) -> OSStatus;

type HalIoProc = unsafe extern "C-unwind" fn(
    AudioObjectID,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    *mut c_void,
) -> OSStatus;

/// An IOProc registered on a device, owning the context it is called with. Dropping it stops
/// and unregisters the IOProc, then frees the context; if Core Audio reports that either step
/// failed, the IO thread might still use the context, so it is leaked instead.
pub struct IoProc {
    device: AudioObjectID,
    id: AudioDeviceIOProcID,
    context: *mut c_void,
    free: unsafe fn(*mut c_void),
    started: bool,
    open: bool,
}

unsafe fn free_box<C>(context: *mut c_void) {
    // SAFETY: `context` came from `Box::<C>::into_raw` and is freed exactly once.
    drop(unsafe { Box::from_raw(context.cast::<C>()) });
}

impl IoProc {
    /// Registers `proc_fn`, called with `context` on the device's IO thread.
    ///
    /// The first registration on an aggregate that contains a tap can block until the user
    /// answers macOS's audio-capture prompt (up to about a minute), so never call this on the
    /// UI thread.
    pub fn create<C: Send + 'static>(
        device: AudioObjectID,
        proc_fn: RawIoProc,
        context: Box<C>,
    ) -> anyhow::Result<Self> {
        let context = Box::into_raw(context).cast::<c_void>();
        // SAFETY: `NonNull<T>` and raw pointers are ABI-compatible (Rust reference, function
        // pointer ABI compatibility), so this only changes how Rust types the arguments.
        let hal = unsafe { std::mem::transmute::<RawIoProc, HalIoProc>(proc_fn) };
        let mut id: AudioDeviceIOProcID = None;
        // SAFETY: valid device, IOProc, context and out pointer.
        let status = unsafe { AudioDeviceCreateIOProcID(device, Some(hal), context, NonNull::from(&mut id)) };
        if status != 0 || id.is_none() {
            // SAFETY: never registered, so nothing else holds the context.
            unsafe { free_box::<C>(context) };
            check(status, "AudioDeviceCreateIOProcID")?;
            bail!("AudioDeviceCreateIOProcID returned no IOProc");
        }
        Ok(Self { device, id, context, free: free_box::<C>, started: false, open: true })
    }

    /// Turns the device's streams on or off for this IOProc (`kAudioDevicePropertyIOProcStreamUsage`).
    pub fn set_input_stream_usage(&self, flags: &[u32]) -> anyhow::Result<()> {
        let ioproc = self.id.map_or(0, |f| f as usize);
        let usage = StreamUsage::new(ioproc, flags);
        let addr = address(kAudioDevicePropertyIOProcStreamUsage, kAudioObjectPropertyScopeInput);
        let data = NonNull::new(usage.as_ptr().cast_mut()).context("empty stream usage")?;
        // SAFETY: `usage` holds the variable-length struct in pointer-aligned storage.
        let status = unsafe {
            AudioObjectSetPropertyData(
                self.device,
                NonNull::from(&addr),
                0,
                ptr::null(),
                usage.byte_len() as u32,
                data.cast(),
            )
        };
        check(status, "AudioObjectSetPropertyData(IOProcStreamUsage)")?;
        Ok(())
    }

    /// Starts IO. On a tap aggregate the first start can also block on the permission prompt.
    pub fn start(&mut self) -> anyhow::Result<()> {
        // SAFETY: a registered IOProc on its device.
        let status = unsafe { AudioDeviceStart(self.device, self.id) };
        check(status, "AudioDeviceStart")?;
        self.started = true;
        Ok(())
    }

    /// Stops and unregisters the IOProc. Must not run on the IO thread.
    pub fn close(&mut self) -> anyhow::Result<()> {
        if !self.open {
            return Ok(());
        }
        self.open = false;
        let mut result = Ok(());
        if self.started {
            // Synchronous when it succeeds: the IOProc is not running once this returns.
            // SAFETY: a registered IOProc on its device.
            result = check(unsafe { AudioDeviceStop(self.device, self.id) }, "AudioDeviceStop");
        }
        // SAFETY: as above; destroyed exactly once.
        let destroyed =
            check(unsafe { AudioDeviceDestroyIOProcID(self.device, self.id) }, "AudioDeviceDestroyIOProcID");
        match (result, destroyed) {
            (Ok(()), Ok(())) => {
                // SAFETY: Core Audio no longer calls the IOProc, so nothing uses the context.
                unsafe { (self.free)(self.context) };
                Ok(())
            }
            (Err(e), _) | (_, Err(e)) => {
                log::warn!("leaking an IOProc context because Core Audio may still use it");
                Err(e.into())
            }
        }
    }
}

impl Drop for IoProc {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            log::warn!("stopping IOProc: {e:#}");
        }
    }
}

/// A property listener block on a private serial dispatch queue (Chromium uses a private queue
/// to avoid deadlocks with Core Audio while listeners are removed). Removed on drop.
pub struct Listener {
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
    queue: DispatchRetained<DispatchQueue>,
    block: RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)>,
}

impl Listener {
    /// Calls `notify` (on `queue`) whenever the property changes.
    pub fn add(
        object: AudioObjectID,
        selector: u32,
        scope: u32,
        queue: &DispatchRetained<DispatchQueue>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let address = address(selector, scope);
        let block: RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)> =
            RcBlock::new(move |_count: u32, _addresses: NonNull<AudioObjectPropertyAddress>| notify());
        // SAFETY: valid address, queue and block; Core Audio copies the block.
        let status = unsafe {
            AudioObjectAddPropertyListenerBlock(
                object,
                NonNull::from(&address),
                Some(&**queue),
                RcBlock::as_ptr(&block),
            )
        };
        check(status, "AudioObjectAddPropertyListenerBlock")
            .with_context(|| format!("listening to '{}' of audio object {object}", selector_name(selector)))?;
        Ok(Self { object, address, queue: queue.clone(), block })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: the same object, address, queue and block that were registered.
        let status = unsafe {
            AudioObjectRemovePropertyListenerBlock(
                self.object,
                NonNull::from(&self.address),
                Some(&*self.queue),
                RcBlock::as_ptr(&self.block),
            )
        };
        if status != 0 {
            // Expected when the object (an aggregate we destroyed, an unplugged device) is gone.
            log::debug!("removing listener from audio object {}: {}", self.object, fourcc(status));
        }
    }
}

/// A private serial queue for listener blocks.
pub fn listener_queue() -> DispatchRetained<DispatchQueue> {
    DispatchQueue::new("com.pixityy.sidestage.coreaudio-listeners", None)
}

/// Tells the HAL to handle its notifications (device and process list changes, which also
/// keep this process's view of those lists current) on a thread of its own. By default it
/// attaches them to the main thread's run loop, which a command-line run never spins and the
/// UI thread can hold up; listeners then stay silent and property reads go stale. RtAudio and
/// PortAudio set this for the same reason. Call before any other Core Audio use.
pub fn use_own_notification_thread() -> anyhow::Result<()> {
    let run_loop: *const c_void = ptr::null();
    // SAFETY: the property is a `CFRunLoopRef`; NULL selects the HAL's own thread.
    unsafe { set(SYSTEM_OBJECT, kAudioHardwarePropertyRunLoop, kAudioObjectPropertyScopeGlobal, &run_loop) }
}

#[cfg(test)]
mod tests {
    use super::super::plan::keys;
    use objc2_core_audio as ca;

    #[test]
    fn dictionary_keys_match_the_sdk() {
        let s = |k: &std::ffi::CStr| k.to_str().unwrap().to_string();
        assert_eq!(keys::NAME, s(ca::kAudioAggregateDeviceNameKey));
        assert_eq!(keys::UID, s(ca::kAudioAggregateDeviceUIDKey));
        assert_eq!(keys::MAIN_SUB_DEVICE, s(ca::kAudioAggregateDeviceMainSubDeviceKey));
        assert_eq!(keys::CLOCK_DEVICE, s(ca::kAudioAggregateDeviceClockDeviceKey));
        assert_eq!(keys::IS_PRIVATE, s(ca::kAudioAggregateDeviceIsPrivateKey));
        assert_eq!(keys::IS_STACKED, s(ca::kAudioAggregateDeviceIsStackedKey));
        assert_eq!(keys::TAP_AUTO_START, s(ca::kAudioAggregateDeviceTapAutoStartKey));
        assert_eq!(keys::SUB_DEVICE_LIST, s(ca::kAudioAggregateDeviceSubDeviceListKey));
        assert_eq!(keys::TAP_LIST, s(ca::kAudioAggregateDeviceTapListKey));
        assert_eq!(keys::SUB_DEVICE_UID, s(ca::kAudioSubDeviceUIDKey));
        assert_eq!(keys::SUB_DEVICE_DRIFT, s(ca::kAudioSubDeviceDriftCompensationKey));
        assert_eq!(keys::SUB_TAP_UID, s(ca::kAudioSubTapUIDKey));
        assert_eq!(keys::SUB_TAP_DRIFT, s(ca::kAudioSubTapDriftCompensationKey));
    }
}
