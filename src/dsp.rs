//! The signal path between the tap and the output device: the hall.
//!
//! Each input channel plays from its side of the stage and is convolved with
//! the hall's response at each ear, so the reverb and the direction of the
//! sound come from the same four dummy-head recordings.

use std::sync::Arc;

use fft_convolver::{TwoStageFFTConvolver, compute_tail_block_size};

use crate::{ir::Hall, state::Params};

/// Processes stereo blocks. Built on the control thread; [`Engine::process`]
/// runs on the audio thread and doesn't allocate, lock or log.
pub struct Engine {
    /// For each stage side (left, right), the convolver for each ear.
    convolvers: [[TwoStageFFTConvolver<f32>; 2]; 2],
    /// One convolver's output for the block.
    scratch: Vec<f32>,
    params: Arc<Params>,
    /// The wet level the previous block ended on.
    wet: f32,
}

impl Engine {
    /// An engine for `hall` at `rate`, for blocks of up to `max_frames`
    /// frames that usually have `block_frames`.
    pub fn new(
        hall: &Hall,
        rate: f64,
        block_frames: usize,
        max_frames: usize,
        params: Arc<Params>,
    ) -> Self {
        // The head stage adds no latency at any size; at the block size it
        // runs once per block
        let head = block_frames.clamp(64, 2048);
        let convolvers = hall.prepare(rate).map(|ears| {
            ears.map(|response| {
                let mut convolver = TwoStageFFTConvolver::default();
                let tail = compute_tail_block_size(head, response.len());
                convolver
                    .init(head, tail, &response)
                    .expect("the block sizes are not zero");
                convolver
            })
        });
        let wet = params.wet.get();
        Self {
            convolvers,
            scratch: vec![0.0; max_frames],
            params,
            wet,
        }
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
        let [from_left, from_right] = &mut self.convolvers;

        for (ear, out) in [&mut *out_left, &mut *out_right].into_iter().enumerate() {
            // Equal lengths can't fail
            let _ = from_left[ear].process(in_left, out);
            let _ = from_right[ear].process(in_right, scratch);
            for (out, hall) in out.iter_mut().zip(scratch.iter()) {
                *out += hall;
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
    use std::time::Instant;

    use super::*;
    use crate::state::AtomicF32;

    const RATE: f64 = 48000.0;
    const BLOCK: usize = 512;

    fn engine(wet: f32) -> Engine {
        let params = Arc::new(Params {
            wet: AtomicF32::new(wet),
        });
        Engine::new(&Hall::load().unwrap(), RATE, BLOCK, 4096, params)
    }

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (*seed >> 8) as f32 / (1 << 23) as f32 - 1.0
    }

    #[test]
    fn dry_only_is_the_input_unchanged() {
        let mut engine = engine(0.0);
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
        let mut engine = engine(1.0);
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

    // The test build's allocator aborts on an allocation in here, as the
    // debug build's does on the audio thread
    #[test]
    fn processing_does_not_allocate() {
        let mut engine = engine(0.5);
        let mut seed = 1;
        let left: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let right: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        assert_no_alloc::assert_no_alloc(|| {
            // Long enough for every convolver stage to run many times
            for _ in 0..400 {
                engine.process(&left, &right, &mut out_left, &mut out_right);
            }
        });
    }

    /// Run with `cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn measure_load() {
        let mut engine = engine(0.5);
        let mut seed = 1;
        let cycle = BLOCK as f64 / RATE;
        let (mut total, mut slowest) = (0.0f64, 0.0f64);
        let blocks = (60.0 / cycle) as usize;
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
        println!(
            "average {:.1}% and at most {:.1}% of a {:.1} ms cycle",
            100.0 * total / blocks as f64 / cycle,
            100.0 * slowest / cycle,
            1000.0 * cycle,
        );
    }
}
