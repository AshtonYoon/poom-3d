//! Core Audio HAL: the process tap, the private aggregate device that makes
//! it readable, and the IOProc that moves audio from the tap to the output.
//!
//! The aggregate holds the real output device and the tap, so one IOProc gets
//! the tapped audio as input and the device as output on the same clock.

use std::{
    ffi::{CStr, c_void},
    fmt,
    mem::{MaybeUninit, offset_of},
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use assert_no_alloc::assert_no_alloc;
use objc2::{AllocAnyThread, rc::Retained, runtime::AnyObject};
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener, CATapDescription,
    CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioDeviceProcessorOverload,
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreamConfiguration,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioObjectUnknown, kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey, kAudioTapPropertyFormat, kAudioTapPropertyUID,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
    kAudioFormatFlagIsFloat, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use crate::{
    dsp::Engine,
    layout::{self, Interleaved, InterleavedMut},
    state::{Event, Stats},
};

/// Frames of scratch space per channel, far above any IO buffer size in use.
pub const MAX_FRAMES: usize = 16384;

const SYSTEM: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

/// Counts the aggregates made by this process, so every one gets its own UID.
static NEXT_AGGREGATE: AtomicU32 = AtomicU32::new(0);

// ---- Errors ------------

#[derive(Debug)]
pub struct HalError(String);

impl fmt::Display for HalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HalError {}

/// Turns an `OSStatus` into a result, naming the step that failed.
fn check(status: i32, what: &str) -> Result<(), HalError> {
    if status == 0 {
        return Ok(());
    }
    let code = status.to_be_bytes();
    let status = if code.iter().all(|byte| byte.is_ascii_graphic()) {
        format!("'{}'", String::from_utf8_lossy(&code))
    } else {
        status.to_string()
    };
    Err(HalError(format!("{what} failed: {status}")))
}

// ---- Properties ------------

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads a fixed-size property.
///
/// # Safety
/// `T` must be the property's C type, and plain data.
unsafe fn get<T: Copy>(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
    what: &str,
) -> Result<T, HalError> {
    unsafe { get_qualified::<T, ()>(object, selector, scope, None, what) }
}

/// Reads a fixed-size property that takes a qualifier.
///
/// # Safety
/// `T` and `Q` must be the property's C types, and plain data.
unsafe fn get_qualified<T: Copy, Q>(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
    qualifier: Option<&Q>,
    what: &str,
) -> Result<T, HalError> {
    let mut address = address(selector, scope);
    let mut value = MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    let (qualifier_size, qualifier) = match qualifier {
        Some(qualifier) => (size_of::<Q>() as u32, ptr::from_ref(qualifier).cast()),
        None => (0, ptr::null()),
    };
    unsafe {
        check(
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&mut address),
                qualifier_size,
                qualifier,
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            ),
            what,
        )?;
        Ok(value.assume_init())
    }
}

fn get_string(object: AudioObjectID, selector: u32, what: &str) -> Result<String, HalError> {
    // The caller owns the string the HAL returns
    let string: *mut CFString =
        unsafe { get(object, selector, kAudioObjectPropertyScopeGlobal, what) }?;
    let string =
        NonNull::new(string).ok_or_else(|| HalError(format!("{what} failed: no value")))?;
    Ok(unsafe { CFRetained::from_raw(string) }.to_string())
}

/// Channels per buffer on one side of a device, as an IOProc receives them.
fn stream_layout(device: AudioObjectID, scope: u32, what: &str) -> Result<Vec<u32>, HalError> {
    let mut address = address(kAudioDevicePropertyStreamConfiguration, scope);
    let mut size = 0u32;
    unsafe {
        check(
            AudioObjectGetPropertyDataSize(
                device,
                NonNull::from(&mut address),
                0,
                ptr::null(),
                NonNull::from(&mut size),
            ),
            what,
        )?;
    }

    // An `AudioBufferList` with as many buffers as the device has streams
    let mut storage = vec![0u64; (size as usize).div_ceil(size_of::<u64>()).max(1)];
    unsafe {
        check(
            AudioObjectGetPropertyData(
                device,
                NonNull::from(&mut address),
                0,
                ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(storage.as_mut_slice()).cast(),
            ),
            what,
        )?;
    }

    let list = storage.as_ptr().cast::<AudioBufferList>();
    let fits = (size as usize).saturating_sub(offset_of!(AudioBufferList, mBuffers))
        / size_of::<AudioBuffer>();
    let buffers = unsafe {
        let count = ((*list).mNumberBuffers as usize).min(fits);
        std::slice::from_raw_parts((&raw const (*list).mBuffers).cast::<AudioBuffer>(), count)
    };
    Ok(buffers
        .iter()
        .map(|buffer| buffer.mNumberChannels)
        .collect())
}

// ---- Listeners ------------

/// What the property listener needs. Leaked once, so a notification the HAL
/// delivers late can never find it freed.
struct ListenerContext {
    events: Sender<Event>,
    stats: Arc<Stats>,
}

unsafe extern "C-unwind" fn property_listener(
    _object: AudioObjectID,
    count: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    context: *mut c_void,
) -> i32 {
    let context = unsafe { &*context.cast::<ListenerContext>() };
    let addresses = unsafe { std::slice::from_raw_parts(addresses.as_ptr(), count as usize) };
    for address in addresses {
        #[allow(non_upper_case_globals)]
        match address.mSelector {
            // May arrive on the audio thread: only an atomic is touched
            kAudioDeviceProcessorOverload => {
                context.stats.overloads.fetch_add(1, Ordering::Relaxed);
            }
            kAudioHardwarePropertyDefaultOutputDevice | kAudioDevicePropertyNominalSampleRate => {
                let _ = context.events.send(Event::OutputChanged);
            }
            _ => {}
        }
    }
    0
}

/// A registered property listener, removed when dropped.
struct Listener {
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
    context: &'static ListenerContext,
}

impl Listener {
    fn add(
        object: AudioObjectID,
        selector: u32,
        context: &'static ListenerContext,
        what: &str,
    ) -> Result<Self, HalError> {
        let mut address = address(selector, kAudioObjectPropertyScopeGlobal);
        unsafe {
            check(
                AudioObjectAddPropertyListener(
                    object,
                    NonNull::from(&mut address),
                    Some(property_listener),
                    ptr::from_ref(context).cast_mut().cast(),
                ),
                what,
            )?;
        }
        Ok(Self {
            object,
            address,
            context,
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        unsafe {
            AudioObjectRemovePropertyListener(
                self.object,
                NonNull::from(&mut self.address),
                Some(property_listener),
                ptr::from_ref(self.context).cast_mut().cast(),
            );
        }
    }
}

// ---- Tap ------------

/// A stereo mixdown of every process but this one, destroyed when dropped.
///
/// The tapped processes are silent on their own device only while the tap is
/// being read, so they come back by themselves if this app stops reading.
struct Tap {
    id: AudioObjectID,
}

impl Tap {
    fn global_excluding_self() -> Result<Self, HalError> {
        // Tapping our own output would feed it back into the input
        let pid = std::process::id() as libc::pid_t;
        let own_process: AudioObjectID = unsafe {
            get_qualified(
                SYSTEM,
                kAudioHardwarePropertyTranslatePIDToProcessObject,
                kAudioObjectPropertyScopeGlobal,
                Some(&pid),
                "finding this process's audio object",
            )
        }?;
        if own_process == kAudioObjectUnknown {
            return Err(HalError(
                "this process has no audio object to exclude from the tap".into(),
            ));
        }

        let excluded = NSArray::from_retained_slice(&[NSNumber::new_u32(own_process)]);
        let mut id = kAudioObjectUnknown;
        unsafe {
            let description = CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &excluded,
            );
            description.setName(&NSString::from_str("Poom3D"));
            description.setPrivate(true);
            description.setMuteBehavior(CATapMuteBehavior::MutedWhenTapped);
            check(
                AudioHardwareCreateProcessTap(Some(&description), &mut id),
                "creating the tap",
            )?;
        }
        Ok(Self { id })
    }

    fn uid(&self) -> Result<String, HalError> {
        get_string(self.id, kAudioTapPropertyUID, "reading the tap's UID")
    }

    fn format(&self) -> Result<AudioStreamBasicDescription, HalError> {
        unsafe {
            get(
                self.id,
                kAudioTapPropertyFormat,
                kAudioObjectPropertyScopeGlobal,
                "reading the tap's format",
            )
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            AudioHardwareDestroyProcessTap(self.id);
        }
    }
}

// ---- Aggregate device ------------

/// A private aggregate of one output device and one tap, destroyed when
/// dropped. Only this process can see it.
struct Aggregate {
    id: AudioObjectID,
}

impl Aggregate {
    fn create(output_uid: &str, tap_uid: &str) -> Result<Self, HalError> {
        let uid = format!(
            "local.poom3d.aggregate.{}.{}",
            std::process::id(),
            NEXT_AGGREGATE.fetch_add(1, Ordering::Relaxed),
        );
        let yes = NSNumber::new_bool(true);
        let no = NSNumber::new_bool(false);

        let sub_device = dictionary(&[(kAudioSubDeviceUIDKey, string(output_uid).as_ref())]);
        let sub_tap = dictionary(&[
            (kAudioSubTapUIDKey, string(tap_uid).as_ref()),
            // The tap is resampled to follow the output device's clock
            (kAudioSubTapDriftCompensationKey, yes.as_ref()),
        ]);
        let description = dictionary(&[
            (kAudioAggregateDeviceNameKey, string("Poom3D").as_ref()),
            (kAudioAggregateDeviceUIDKey, string(&uid).as_ref()),
            (
                kAudioAggregateDeviceMainSubDeviceKey,
                string(output_uid).as_ref(),
            ),
            (kAudioAggregateDeviceIsPrivateKey, yes.as_ref()),
            (kAudioAggregateDeviceIsStackedKey, no.as_ref()),
            (kAudioAggregateDeviceTapAutoStartKey, yes.as_ref()),
            (
                kAudioAggregateDeviceSubDeviceListKey,
                NSArray::from_retained_slice(&[sub_device]).as_ref(),
            ),
            (
                kAudioAggregateDeviceTapListKey,
                NSArray::from_retained_slice(&[sub_tap]).as_ref(),
            ),
        ]);

        let mut id = kAudioObjectUnknown;
        unsafe {
            // NSDictionary is toll-free bridged to CFDictionary
            let description = &*Retained::as_ptr(&description).cast::<CFDictionary>();
            check(
                AudioHardwareCreateAggregateDevice(description, NonNull::from(&mut id)),
                "creating the aggregate device",
            )?;
        }
        Ok(Self { id })
    }

    /// Input and output layouts, once the HAL has brought both sides up.
    fn layouts(&self) -> Result<(Vec<u32>, Vec<u32>), HalError> {
        let read = || -> Result<_, HalError> {
            Ok((
                stream_layout(
                    self.id,
                    kAudioObjectPropertyScopeInput,
                    "reading the input layout",
                )?,
                stream_layout(
                    self.id,
                    kAudioObjectPropertyScopeOutput,
                    "reading the output layout",
                )?,
            ))
        };
        // A new aggregate's streams appear a moment after it's created
        for _ in 0..40 {
            let (input, output) = read()?;
            if !input.is_empty() && !output.is_empty() {
                return Ok((input, output));
            }
            thread::sleep(Duration::from_millis(50));
        }
        let (input, output) = read()?;
        if input.is_empty() {
            return Err(HalError("the aggregate device has no tap input".into()));
        }
        if output.is_empty() {
            return Err(HalError("the aggregate device has no output".into()));
        }
        Ok((input, output))
    }
}

impl Drop for Aggregate {
    fn drop(&mut self) {
        unsafe {
            AudioHardwareDestroyAggregateDevice(self.id);
        }
    }
}

fn string(value: &str) -> Retained<NSString> {
    NSString::from_str(value)
}

/// A dictionary keyed by Core Audio's key constants.
fn dictionary(pairs: &[(&CStr, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<_> = pairs
        .iter()
        .map(|(key, _)| string(&key.to_string_lossy()))
        .collect();
    let keys: Vec<&NSString> = keys.iter().map(|key| &**key).collect();
    let values: Vec<&AnyObject> = pairs.iter().map(|(_, value)| *value).collect();
    NSDictionary::from_slices(&keys, &values)
}

// ---- IOProc ------------

/// Everything the audio thread owns. Built on the control thread, so the
/// audio thread never allocates.
struct IoContext {
    engine: Engine,
    in_left: Vec<f32>,
    in_right: Vec<f32>,
    out_left: Vec<f32>,
    out_right: Vec<f32>,
    stats: Arc<Stats>,
}

/// The IOProc's signature with raw pointers: either buffer list may be null.
type RawIoProc = unsafe extern "C-unwind" fn(
    AudioObjectID,
    *const AudioTimeStamp,
    *const AudioBufferList,
    *const AudioTimeStamp,
    *mut AudioBufferList,
    *const AudioTimeStamp,
    *mut c_void,
) -> i32;

/// The same signature as the bindings declare it.
type BoundIoProc = unsafe extern "C-unwind" fn(
    AudioObjectID,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    *mut c_void,
) -> i32;

unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    context: *mut c_void,
) -> i32 {
    let context = unsafe { &mut *context.cast::<IoContext>() };
    let started = Instant::now();
    assert_no_alloc(|| unsafe { render(context, input, output) });
    let nanos = started.elapsed().as_nanos() as u64;
    let stats = &context.stats;
    stats.render_nanos.fetch_add(nanos, Ordering::Relaxed);
    stats
        .slowest_render_nanos
        .fetch_max(nanos, Ordering::Relaxed);
    0
}

/// One IO cycle: tap in, DSP, device out.
///
/// # Safety
/// `input` and `output` must be null or the buffer lists of the current IO
/// cycle, holding 32-bit float samples.
unsafe fn render(
    context: &mut IoContext,
    input: *const AudioBufferList,
    output: *mut AudioBufferList,
) {
    let input: &[AudioBuffer] = if input.is_null() {
        &[]
    } else {
        unsafe {
            std::slice::from_raw_parts(
                (&raw const (*input).mBuffers).cast(),
                (*input).mNumberBuffers as usize,
            )
        }
    };
    let output: &mut [AudioBuffer] = if output.is_null() {
        &mut []
    } else {
        unsafe {
            std::slice::from_raw_parts_mut(
                (&raw mut (*output).mBuffers).cast(),
                (*output).mNumberBuffers as usize,
            )
        }
    };

    // Whatever isn't written below plays as silence
    for buffer in output.iter() {
        if !buffer.mData.is_null() {
            unsafe {
                ptr::write_bytes(buffer.mData.cast::<u8>(), 0, buffer.mDataByteSize as usize)
            };
        }
    }

    // The tap is in the last two buffers at most, the main output in the first two
    let mut tap: [Interleaved; 2] = Default::default();
    let last = &input[input.len().saturating_sub(2)..];
    for (view, buffer) in tap.iter_mut().zip(last) {
        if !buffer.mData.is_null() {
            view.channels = buffer.mNumberChannels as usize;
            view.data = unsafe {
                std::slice::from_raw_parts(
                    buffer.mData.cast(),
                    buffer.mDataByteSize as usize / size_of::<f32>(),
                )
            };
        }
    }
    let mut device: [InterleavedMut; 2] = Default::default();
    let first = output.len().min(2);
    for (view, buffer) in device.iter_mut().zip(&output[..first]) {
        if !buffer.mData.is_null() {
            view.channels = buffer.mNumberChannels as usize;
            view.data = unsafe {
                std::slice::from_raw_parts_mut(
                    buffer.mData.cast(),
                    buffer.mDataByteSize as usize / size_of::<f32>(),
                )
            };
        }
    }

    let frames = layout::read_last_stereo(
        &tap[..last.len()],
        &mut context.in_left,
        &mut context.in_right,
    );
    let (in_left, in_right) = (&context.in_left[..frames], &context.in_right[..frames]);
    let (out_left, out_right) = (
        &mut context.out_left[..frames],
        &mut context.out_right[..frames],
    );
    context
        .engine
        .process(in_left, in_right, out_left, out_right);

    // Samples over full scale are left to the system, as they are for any
    // other app: the tap itself delivers them, and the volume comes after
    let output_peak = out_left
        .iter()
        .chain(out_right.iter())
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    layout::write_first_stereo(&mut device[..first], out_left, out_right);

    let input_peak = in_left
        .iter()
        .chain(in_right)
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    context.stats.input_peak.raise(input_peak);
    context.stats.output_peak.raise(output_peak);
    context.stats.cycles.fetch_add(1, Ordering::Relaxed);
}

/// A running IOProc, stopped and unregistered when dropped.
struct IoProc {
    device: AudioObjectID,
    id: AudioDeviceIOProcID,
    context: *mut IoContext,
}

impl IoProc {
    fn start(device: AudioObjectID, engine: Engine, stats: Arc<Stats>) -> Result<Self, HalError> {
        let context = Box::into_raw(Box::new(IoContext {
            engine,
            in_left: vec![0.0; MAX_FRAMES],
            in_right: vec![0.0; MAX_FRAMES],
            out_left: vec![0.0; MAX_FRAMES],
            out_right: vec![0.0; MAX_FRAMES],
            stats,
        }));

        let mut id: AudioDeviceIOProcID = None;
        let created = unsafe {
            check(
                AudioDeviceCreateIOProcID(
                    device,
                    Some(std::mem::transmute::<RawIoProc, BoundIoProc>(io_proc)),
                    context.cast(),
                    NonNull::from(&mut id),
                ),
                "registering the IOProc",
            )
        };
        if let Err(error) = created {
            // Never registered: nothing else holds the context
            drop(unsafe { Box::from_raw(context) });
            return Err(error);
        }

        // From here dropping the proc unregisters it and frees the context
        let proc = Self {
            device,
            id,
            context,
        };
        unsafe { check(AudioDeviceStart(device, id), "starting the device")? };
        Ok(proc)
    }
}

impl Drop for IoProc {
    fn drop(&mut self) {
        unsafe {
            // Returns once the callback is no longer running
            AudioDeviceStop(self.device, self.id);
            AudioDeviceDestroyIOProcID(self.device, self.id);
            drop(Box::from_raw(self.context));
        }
    }
}

// ---- Pipeline ------------

/// What a pipeline is built on, for the log.
pub struct PipelineInfo {
    output: AudioObjectID,
    pub output_name: String,
    pub output_rate: f64,
    pub tap_rate: f64,
    pub buffer_frames: u32,
    /// Channels per buffer of the aggregate's input: the output device's own
    /// inputs, if it has any, and the tap.
    pub input_layout: Vec<u32>,
    /// Channels per buffer of the aggregate's output.
    pub output_layout: Vec<u32>,
}

impl fmt::Display for PipelineInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {} Hz, {} frames per cycle ({:.1} ms), tap at {} Hz, input {:?}, output {:?}",
            self.output_name,
            self.output_rate,
            self.buffer_frames,
            f64::from(self.buffer_frames) / self.output_rate * 1000.0,
            self.tap_rate,
            self.input_layout,
            self.output_layout,
        )
    }
}

/// The tap, its aggregate device and their description, not yet running.
struct Parts {
    tap: Tap,
    aggregate: Aggregate,
    info: PipelineInfo,
}

/// System audio flowing from the tap to the output device until dropped.
pub struct Pipeline {
    // Dropped in this order: stop IO, then take the devices apart
    _io: IoProc,
    _listeners: [Listener; 2],
    _aggregate: Aggregate,
    _tap: Tap,
    info: PipelineInfo,
}

impl Pipeline {
    pub fn info(&self) -> &PipelineInfo {
        &self.info
    }
}

/// The app's connection to the HAL.
pub struct Hal {
    context: &'static ListenerContext,
    _default_output: Listener,
}

impl Hal {
    /// `events` gets an [`Event::OutputChanged`] whenever the pipeline may
    /// need rebuilding; `stats` is what running pipelines report into.
    pub fn new(events: Sender<Event>, stats: Arc<Stats>) -> Result<Self, HalError> {
        let context: &'static ListenerContext =
            Box::leak(Box::new(ListenerContext { events, stats }));
        let default_output = Listener::add(
            SYSTEM,
            kAudioHardwarePropertyDefaultOutputDevice,
            context,
            "watching the default output device",
        )?;
        Ok(Self {
            context,
            _default_output: default_output,
        })
    }

    /// The default output device and its sample rate.
    fn output(&self) -> Result<(AudioObjectID, f64), HalError> {
        let device: AudioObjectID = unsafe {
            get(
                SYSTEM,
                kAudioHardwarePropertyDefaultOutputDevice,
                kAudioObjectPropertyScopeGlobal,
                "finding the default output device",
            )
        }?;
        if device == kAudioObjectUnknown {
            return Err(HalError("there is no output device".into()));
        }
        let rate = unsafe {
            get(
                device,
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeGlobal,
                "reading the output device's sample rate",
            )
        }?;
        Ok((device, rate))
    }

    fn build(&self) -> Result<Parts, HalError> {
        let (output, output_rate) = self.output()?;
        let output_uid = get_string(
            output,
            kAudioDevicePropertyDeviceUID,
            "reading the output device's UID",
        )?;
        let output_name = get_string(
            output,
            kAudioObjectPropertyName,
            "reading the output device's name",
        )?;

        let tap = Tap::global_excluding_self()?;
        let format = tap.format()?;
        if format.mFormatID != kAudioFormatLinearPCM
            || format.mFormatFlags & kAudioFormatFlagIsFloat == 0
            || format.mBitsPerChannel != 32
        {
            return Err(HalError("the tap's format is not 32-bit float".into()));
        }

        let aggregate = Aggregate::create(&output_uid, &tap.uid()?)?;
        let (input_layout, output_layout) = aggregate.layouts()?;
        let buffer_frames = unsafe {
            get(
                aggregate.id,
                kAudioDevicePropertyBufferFrameSize,
                kAudioObjectPropertyScopeGlobal,
                "reading the buffer size",
            )
        }?;

        let info = PipelineInfo {
            output,
            output_name,
            output_rate,
            tap_rate: format.mSampleRate,
            buffer_frames,
            input_layout,
            output_layout,
        };
        Ok(Parts {
            tap,
            aggregate,
            info,
        })
    }

    /// Builds the tap and its aggregate device and takes them apart again,
    /// without starting IO, so nothing is muted and nothing is recorded.
    pub fn probe(&self) -> Result<PipelineInfo, HalError> {
        Ok(self.build()?.info)
    }

    /// Starts moving system audio from a new tap, through the engine
    /// `engine` builds for the pipeline, to the default output device.
    /// Whatever `engine` returns beside the engine is passed on.
    pub fn start<T>(
        &self,
        engine: impl FnOnce(&PipelineInfo) -> (Engine, T),
    ) -> Result<(Pipeline, T), HalError> {
        let Parts {
            tap,
            aggregate,
            info,
        } = self.build()?;
        let listeners = [
            Listener::add(
                info.output,
                kAudioDevicePropertyNominalSampleRate,
                self.context,
                "watching the output device's sample rate",
            )?,
            Listener::add(
                aggregate.id,
                kAudioDeviceProcessorOverload,
                self.context,
                "watching for overloads",
            )?,
        ];
        let stats = &self.context.stats;
        let cycle = f64::from(info.buffer_frames) / info.output_rate;
        stats
            .cycle_nanos
            .store((cycle * 1e9) as u64, Ordering::Relaxed);
        let (engine, rest) = engine(&info);
        let io = IoProc::start(aggregate.id, engine, stats.clone())?;
        let pipeline = Pipeline {
            _io: io,
            _listeners: listeners,
            _aggregate: aggregate,
            _tap: tap,
            info,
        };
        Ok((pipeline, rest))
    }

    /// Whether `pipeline` still plays to the default output device at its
    /// current sample rate.
    pub fn is_current(&self, pipeline: &Pipeline) -> bool {
        self.output().is_ok_and(|(device, rate)| {
            device == pipeline.info.output && rate == pipeline.info.output_rate
        })
    }
}
