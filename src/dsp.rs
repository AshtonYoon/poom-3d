//! The signal path between the tap and the output device: the hall.
//!
//! Each input channel plays from its side of the stage and is convolved with
//! the hall's response at each ear, so the reverb and the direction of the
//! sound come from the same four dummy-head recordings.

use std::sync::Arc;

use fft_convolver::{TwoStageFFTConvolver, compute_tail_block_size};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::{ir::Responses, state::Params};

/// Halls that can wait for the audio thread, and for the control thread to
/// take them back.
const QUEUE: usize = 16;

/// One hall, ready to play: for each stage side (left, right), the convolver
/// for each ear (left, right).
pub struct Bank([[TwoStageFFTConvolver<f32>; 2]; 2]);

impl Bank {
    /// Allocates: for the control thread only. `block_frames` is the usual
    /// size of the blocks it will process.
    pub fn new(responses: &Responses, block_frames: usize) -> Box<Self> {
        // The head stage adds no latency at any size; at the block size it
        // runs once per block
        let head = block_frames.clamp(64, 2048);
        Box::new(Self(responses.each_ref().map(|ears| {
            ears.each_ref().map(|response| {
                let mut convolver = TwoStageFFTConvolver::default();
                let tail = compute_tail_block_size(head, response.len());
                convolver
                    .init(head, tail, response)
                    .expect("the block sizes are not zero");
                convolver
            })
        })))
    }

    /// Writes the hall's answer to the input to `out_left` and `out_right`,
    /// using `scratch` of the same length.
    fn process(
        &mut self,
        in_left: &[f32],
        in_right: &[f32],
        out_left: &mut [f32],
        out_right: &mut [f32],
        scratch: &mut [f32],
    ) {
        let [from_left, from_right] = &mut self.0;
        for (ear, out) in [out_left, out_right].into_iter().enumerate() {
            // Equal lengths can't fail
            let _ = from_left[ear].process(in_left, out);
            let _ = from_right[ear].process(in_right, scratch);
            for (out, hall) in out.iter_mut().zip(scratch.iter()) {
                *out += hall;
            }
        }
    }
}

/// The control thread's end of an [`Engine`].
pub struct EngineHandle {
    next: Producer<Box<Bank>>,
    retired: Consumer<Box<Bank>>,
}

impl EngineHandle {
    /// Has the engine fade over to `bank`. Returns it if too many are
    /// already waiting.
    pub fn switch_to(&mut self, bank: Box<Bank>) -> Result<(), Box<Bank>> {
        // Frees the halls the engine is done with, which it must not do itself
        while self.retired.pop().is_ok() {}
        self.next
            .push(bank)
            .map_err(|rtrb::PushError::Full(bank)| bank)
    }
}

/// Processes stereo blocks. Built on the control thread; [`Engine::process`]
/// runs on the audio thread and doesn't allocate, free, lock or log.
pub struct Engine {
    bank: Box<Bank>,
    /// The hall being faded to, and how many frames of the fade are done.
    incoming: Option<(Box<Bank>, usize)>,
    fade_frames: usize,
    next: Consumer<Box<Bank>>,
    retired: Producer<Box<Bank>>,
    /// The incoming hall's output for the block, and one convolver's.
    incoming_left: Vec<f32>,
    incoming_right: Vec<f32>,
    scratch: Vec<f32>,
    params: Arc<Params>,
    /// The wet level the previous block ended on.
    wet: f32,
}

impl Engine {
    /// An engine playing `bank`, for blocks of up to `max_frames` frames,
    /// that takes `fade_frames` to change to another hall.
    pub fn new(
        bank: Box<Bank>,
        max_frames: usize,
        fade_frames: usize,
        params: Arc<Params>,
    ) -> (Self, EngineHandle) {
        let (next_in, next_out) = RingBuffer::new(QUEUE);
        let (retired_in, retired_out) = RingBuffer::new(QUEUE);
        let wet = params.wet.get();
        let engine = Self {
            bank,
            incoming: None,
            fade_frames: fade_frames.max(1),
            next: next_out,
            retired: retired_in,
            incoming_left: vec![0.0; max_frames],
            incoming_right: vec![0.0; max_frames],
            scratch: vec![0.0; max_frames],
            params,
            wet,
        };
        let handle = EngineHandle {
            next: next_in,
            retired: retired_out,
        };
        (engine, handle)
    }

    /// Processes one block. All four slices have the same length, which is
    /// at most the `max_frames` the engine was built for.
    pub fn process(
        &mut self,
        in_left: &[f32],
        in_right: &[f32],
        out_left: &mut [f32],
        out_right: &mut [f32],
    ) {
        let frames = in_left.len();
        let scratch = &mut self.scratch[..frames];

        // One fade at a time, and only when the old hall can be handed back
        if self.incoming.is_none()
            && !self.retired.is_full()
            && let Ok(bank) = self.next.pop()
        {
            self.incoming = Some((bank, 0));
        }

        self.bank
            .process(in_left, in_right, out_left, out_right, scratch);

        if let Some((mut incoming, done)) = self.incoming.take() {
            let incoming_left = &mut self.incoming_left[..frames];
            let incoming_right = &mut self.incoming_right[..frames];
            incoming.process(in_left, in_right, incoming_left, incoming_right, scratch);

            // The new hall starts with no reverberation of what was played
            // before; the fade is long enough to cover that
            for (old, new) in [
                (&mut *out_left, incoming_left),
                (&mut *out_right, incoming_right),
            ] {
                for (index, (old, new)) in old.iter_mut().zip(new.iter()).enumerate() {
                    let mix = ((done + index + 1) as f32 / self.fade_frames as f32).min(1.0);
                    *old += mix * (*new - *old);
                }
            }

            if done + frames >= self.fade_frames {
                let outgoing = std::mem::replace(&mut self.bank, incoming);
                // There is room: nothing else pushes, and it was checked
                // before the fade began
                let _ = self.retired.push(outgoing);
            } else {
                self.incoming = Some((incoming, done + frames));
            }
        }

        // Moving the wet level over the block keeps a change from clicking
        let target = self.params.wet.get().clamp(0.0, 1.0);
        let step = (target - self.wet) / frames.max(1) as f32;
        for (ear_in, ear_out) in [(in_left, out_left), (in_right, out_right)] {
            let mut wet = self.wet;
            for (out, dry) in ear_out.iter_mut().zip(ear_in) {
                wet += step;
                *out = (1.0 - wet) * dry + wet * *out;
            }
        }
        self.wet = target;
    }
}

#[cfg(test)]
mod tests {
    use std::{f32::consts::TAU, time::Instant};

    use super::*;
    use crate::{ir::Library, state::AtomicF32};

    const RATE: f64 = 48000.0;
    const BLOCK: usize = 512;
    const FADE: usize = 14400;

    fn bank(library: &mut Library, stage: usize, seat: usize) -> Box<Bank> {
        Bank::new(&library.responses(stage, seat, RATE), BLOCK)
    }

    fn engine(wet: f32) -> (Engine, EngineHandle, Library) {
        let mut library = Library::load().unwrap();
        let params = Arc::new(Params {
            wet: AtomicF32::new(wet),
        });
        let (engine, handle) = Engine::new(bank(&mut library, 1, 0), 4096, FADE, params);
        (engine, handle, library)
    }

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (*seed >> 8) as f32 / (1 << 23) as f32 - 1.0
    }

    #[test]
    fn dry_only_is_the_input_unchanged() {
        let (mut engine, ..) = engine(0.0);
        let mut seed = 1;
        let left: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let right: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        engine.process(&left, &right, &mut out_left, &mut out_right);
        assert_eq!(out_left, left);
        assert_eq!(out_right, right);
    }

    #[test]
    fn the_hall_answers_within_two_milliseconds_and_from_the_right_side() {
        let (mut engine, ..) = engine(1.0);
        let mut left = vec![0.0; BLOCK];
        left[0] = 1.0;
        let right = vec![0.0; BLOCK];
        let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        engine.process(&left, &right, &mut out_left, &mut out_right);

        let peak = out_left
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let onset = out_left
            .iter()
            .position(|sample| sample.abs() >= 0.1 * peak)
            .unwrap();
        assert!(peak > 0.0);
        assert!((onset as f64) < 0.002 * RATE, "onset at {onset}");

        let energy = |samples: &[f32]| samples.iter().map(|sample| sample * sample).sum::<f32>();
        assert!(energy(&out_left) > energy(&out_right));
    }

    // The test build's allocator aborts on an allocation or a free in here,
    // as the debug build's does on the audio thread
    #[test]
    fn processing_and_switching_halls_do_not_allocate() {
        let (mut engine, mut handle, mut library) = engine(0.5);
        handle.switch_to(bank(&mut library, 2, 2)).ok().unwrap();
        let mut seed = 1;
        let left: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let right: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        assert_no_alloc::assert_no_alloc(|| {
            // Long enough for the fade and for every convolver stage to run
            // many times
            for _ in 0..400 {
                engine.process(&left, &right, &mut out_left, &mut out_right);
            }
        });
        assert!(engine.incoming.is_none());
        assert_eq!(handle.retired.slots(), 1);
    }

    #[test]
    fn switching_halls_does_not_click() {
        let (mut engine, mut handle, mut library) = engine(1.0);
        let mut phase = 0.0f32;
        let mut previous = 0.0f32;
        // The largest step between neighbouring samples of the left ear's
        // output over some blocks of a steady tone
        let mut largest_step = |engine: &mut Engine, blocks: usize| {
            let mut largest = 0.0f32;
            for _ in 0..blocks {
                let tone: Vec<f32> = (0..BLOCK)
                    .map(|_| {
                        phase = (phase + TAU * 220.0 / RATE as f32) % TAU;
                        0.5 * phase.sin()
                    })
                    .collect();
                let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
                engine.process(&tone, &tone, &mut out_left, &mut out_right);
                for sample in out_left {
                    largest = largest.max((sample - previous).abs());
                    previous = sample;
                }
            }
            largest
        };

        // Let the reverberation build up, then measure it undisturbed
        largest_step(&mut engine, 4 * RATE as usize / BLOCK);
        let before = largest_step(&mut engine, 50);

        for (stage, seat) in [(0, 2), (2, 1), (1, 0)] {
            handle
                .switch_to(bank(&mut library, stage, seat))
                .ok()
                .unwrap();
            let during = largest_step(&mut engine, FADE / BLOCK + 2);
            assert!(engine.incoming.is_none());
            let after = largest_step(&mut engine, 4 * RATE as usize / BLOCK);
            assert!(
                during <= 1.5 * before.max(after),
                "to stage {stage} seat {seat}: {during} during the fade, {before} before, {after} after"
            );
        }
    }

    /// Run with `cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn measure_load() {
        let (mut engine, mut handle, mut library) = engine(0.5);
        let mut seed = 1;
        let cycle = BLOCK as f64 / RATE;
        let mut measure = |engine: &mut Engine, blocks: usize| {
            let (mut total, mut slowest) = (0.0f64, 0.0f64);
            for _ in 0..blocks {
                let left: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
                let right: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
                let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
                let started = Instant::now();
                engine.process(&left, &right, &mut out_left, &mut out_right);
                let took = started.elapsed().as_secs_f64();
                total += took;
                slowest = slowest.max(took);
            }
            (
                100.0 * total / blocks as f64 / cycle,
                100.0 * slowest / cycle,
            )
        };

        let (average, slowest) = measure(&mut engine, (60.0 / cycle) as usize);
        println!(
            "one hall: average {average:.1}% and at most {slowest:.1}% of a {:.1} ms cycle",
            1000.0 * cycle
        );
        handle.switch_to(bank(&mut library, 2, 2)).ok().unwrap();
        let (average, slowest) = measure(&mut engine, FADE / BLOCK);
        println!("while switching halls: average {average:.1}% and at most {slowest:.1}%");
    }
}
