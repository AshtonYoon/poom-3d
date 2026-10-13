//! What the control thread and the HAL's threads tell each other.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Something the control thread has to react to.
pub enum Event {
    /// The default output device or its sample rate may have changed.
    OutputChanged,
    /// The app was asked to quit.
    Shutdown,
}

/// An `f32` shared between threads without locking.
#[derive(Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    /// Raises the value to `value` if that is higher. Both must be
    /// non-negative: those order the same as their bit patterns.
    pub fn raise(&self, value: f32) {
        self.0.fetch_max(value.to_bits(), Ordering::Relaxed);
    }
}

/// What the user can change while audio is running. Written by the control
/// thread, read by the audio thread.
pub struct Params {
    /// How much of the output is the hall: 0 is the original sound only, 1 is
    /// the hall only.
    pub wet: AtomicF32,
}

/// Counters written by the audio thread and read by the control thread.
#[derive(Default)]
pub struct Stats {
    /// IO cycles rendered.
    pub cycles: AtomicU64,
    /// Processor overloads the HAL reported: each one is an audible dropout.
    pub overloads: AtomicU64,
    /// Highest absolute sample seen on the tap.
    pub input_peak: AtomicF32,
    /// Highest absolute sample sent to the device, before it's limited to 1.
    pub output_peak: AtomicF32,
    /// Time spent rendering, over all cycles and in the slowest one.
    pub render_nanos: AtomicU64,
    pub slowest_render_nanos: AtomicU64,
    /// How long one IO cycle of the running pipeline lasts.
    pub cycle_nanos: AtomicU64,
}
