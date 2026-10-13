//! The signal path between the tap and the output device: the hall.
//!
//! Each input channel plays from its side of the stage. Its direct sound goes
//! through the head filters for the direction that side is in, seen from
//! where the head is turned, so the stage stays put when the head moves. What
//! follows the direct sound is convolved with the hall's response at each
//! ear, and is as loud as the wet level says: at none of it the stage stands
//! in the open, at all of it in the hall as it was measured.

use std::sync::Arc;

use fft_convolver::{TwoStageFFTConvolver, compute_tail_block_size};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::{
    ir::{self, Direct, Hall, HeadFilters},
    state::Params,
};

/// Halls that can wait for the audio thread, and for the control thread to
/// take them back.
const QUEUE: usize = 16;

/// One hall, ready to play.
pub struct Bank {
    filters: Arc<HeadFilters>,
    /// For each stage side (left, right): where its direct sound comes from,
    /// and the head filter it was last played through.
    direct: [Direct; 2],
    played: [Option<usize>; 2],
    /// For each stage side (left, right), the convolver for each ear (left,
    /// right) of what follows the direct sound.
    reverb: [[TwoStageFFTConvolver<f32>; 2]; 2],
}

impl Bank {
    /// Allocates: for the control thread only. `block_frames` is the usual
    /// size of the blocks it will process.
    pub fn new(hall: &Hall, block_frames: usize) -> Box<Self> {
        // The head stage adds no latency at any size; at the block size it
        // runs once per block
        let head = block_frames.clamp(64, 2048);
        let reverb = hall.reverb.each_ref().map(|ears| {
            ears.each_ref().map(|response| {
                let mut convolver = TwoStageFFTConvolver::default();
                let tail = compute_tail_block_size(head, response.len());
                convolver
                    .init(head, tail, response)
                    .expect("the block sizes are not zero");
                convolver
            })
        });
        Box::new(Self {
            filters: hall.filters.clone(),
            direct: hall.direct,
            played: [None; 2],
            reverb,
        })
    }

    /// Writes the hall's answer to `out_left` and `out_right`.
    ///
    /// `input` is each channel's block preceded by the `taps - 1` samples
    /// before it. `head` is how far the head is turned to the left, in
    /// degrees. `wet` is the level of what follows the direct sound at the
    /// start and at the end of the block. `scratch` is as long as the block.
    fn process(
        &mut self,
        input: [&[f32]; 2],
        head: f32,
        wet: (f32, f32),
        out_left: &mut [f32],
        out_right: &mut [f32],
        scratch: &mut [f32],
    ) {
        let before = self.filters.taps() - 1;
        let [from_left, from_right] = &mut self.reverb;
        // Moving the level over the block keeps a change from clicking
        let step = (wet.1 - wet.0) / scratch.len().max(1) as f32;
        for (ear, out) in [&mut *out_left, &mut *out_right].into_iter().enumerate() {
            // Equal lengths can't fail
            let _ = from_left[ear].process(&input[0][before..], out);
            let _ = from_right[ear].process(&input[1][before..], scratch);
            for (index, (out, hall)) in out.iter_mut().zip(scratch.iter()).enumerate() {
                *out = (wet.0 + step * (index + 1) as f32) * (*out + hall);
            }
        }

        let sides = self.direct.iter().zip(&mut self.played).zip(input);
        for ((Direct { azimuth, gain }, played), input) in sides {
            let now = ir::index(azimuth - head);
            let last = played.replace(now).unwrap_or(now);
            for (ear, out) in [&mut *out_left, &mut *out_right].into_iter().enumerate() {
                let filter = self.filters.reversed(last, ear);
                // Fading from the old direction's filter to the new one over
                // the block lets the head turn without a click
                let turned = (now != last).then(|| self.filters.reversed(now, ear));
                add_filtered(out, input, filter, turned, *gain);
            }
        }
    }
}

/// Adds `input` through `filter` to `out`, at `gain`, fading over to the
/// filter `turned` if there is one. The filters are reversed in time, and
/// `input` is the block preceded by one sample fewer than a filter is long.
fn add_filtered(out: &mut [f32], input: &[f32], filter: &[f32], turned: Option<&[f32]>, gain: f32) {
    let dot = |window: &[f32], filter: &[f32]| -> f32 {
        window.iter().zip(filter).map(|(a, b)| a * b).sum()
    };
    let frames = out.len() as f32;
    for (index, (out, window)) in out.iter_mut().zip(input.windows(filter.len())).enumerate() {
        let mut sample = dot(window, filter);
        if let Some(turned) = turned {
            let mix = (index + 1) as f32 / frames;
            sample += mix * (dot(window, turned) - sample);
        }
        *out += gain * sample;
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
    /// Each input channel's block, preceded by the samples before it that
    /// the head filters still reach.
    input: [Vec<f32>; 2],
    /// The head filters' length, less one.
    before: usize,
    /// The incoming hall's output for the block, and one convolver's.
    incoming_left: Vec<f32>,
    incoming_right: Vec<f32>,
    scratch: Vec<f32>,
    params: Arc<Params>,
    /// The level of the hall the previous block ended on.
    wet: f32,
}

impl Engine {
    /// An engine playing `bank`, for blocks of up to `max_frames` frames,
    /// that takes `fade_frames` to change to another hall. Every hall it is
    /// given must be for the same sample rate.
    pub fn new(
        bank: Box<Bank>,
        max_frames: usize,
        fade_frames: usize,
        params: Arc<Params>,
    ) -> (Self, EngineHandle) {
        let (next_in, next_out) = RingBuffer::new(QUEUE);
        let (retired_in, retired_out) = RingBuffer::new(QUEUE);
        let before = bank.filters.taps() - 1;
        let wet = params.wet.get();
        let engine = Self {
            bank,
            incoming: None,
            fade_frames: fade_frames.max(1),
            next: next_out,
            retired: retired_in,
            input: [
                vec![0.0; before + max_frames],
                vec![0.0; before + max_frames],
            ],
            before,
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
        let (before, reach) = (self.before, self.before + frames);
        self.input[0][before..reach].copy_from_slice(in_left);
        self.input[1][before..reach].copy_from_slice(in_right);
        let input = [&self.input[0][..reach], &self.input[1][..reach]];
        let scratch = &mut self.scratch[..frames];
        let head = self.params.head_yaw.get();
        let wet = (self.wet, self.params.wet.get().clamp(0.0, 1.0));
        self.wet = wet.1;

        // One fade at a time, and only when the old hall can be handed back
        if self.incoming.is_none()
            && !self.retired.is_full()
            && let Ok(bank) = self.next.pop()
        {
            self.incoming = Some((bank, 0));
        }

        self.bank
            .process(input, head, wet, out_left, out_right, scratch);

        if let Some((mut incoming, done)) = self.incoming.take() {
            let incoming_left = &mut self.incoming_left[..frames];
            let incoming_right = &mut self.incoming_right[..frames];
            incoming.process(input, head, wet, incoming_left, incoming_right, scratch);

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

        // The end of this block is what the head filters reach back to next
        for channel in &mut self.input {
            channel.copy_within(frames..reach, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{f32::consts::TAU, time::Instant};

    use super::*;
    use crate::ir::Library;

    const RATE: f64 = 48000.0;
    const BLOCK: usize = 512;
    const FADE: usize = 14400;

    fn bank(library: &mut Library, stage: usize, seat: usize) -> Box<Bank> {
        Bank::new(&library.hall(stage, seat, RATE), BLOCK)
    }

    fn engine(wet: f32) -> (Engine, EngineHandle, Library, Arc<Params>) {
        let mut library = Library::load().unwrap();
        let params = Arc::new(Params::new(wet));
        let (engine, handle) = Engine::new(bank(&mut library, 1, 0), 4096, FADE, params.clone());
        (engine, handle, library, params)
    }

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (*seed >> 8) as f32 / (1 << 23) as f32 - 1.0
    }

    fn energy(samples: &[f32]) -> f32 {
        samples.iter().map(|sample| sample * sample).sum()
    }

    /// The largest step between neighbouring samples of the left ear's output
    /// over some blocks of a steady tone, with something done before each.
    struct Tone {
        phase: f32,
        previous: f32,
    }

    impl Tone {
        fn largest_step(
            &mut self,
            engine: &mut Engine,
            blocks: usize,
            mut before_each: impl FnMut(usize),
        ) -> f32 {
            let mut largest = 0.0f32;
            for block in 0..blocks {
                before_each(block);
                let tone: Vec<f32> = (0..BLOCK)
                    .map(|_| {
                        self.phase = (self.phase + TAU * 220.0 / RATE as f32) % TAU;
                        0.5 * self.phase.sin()
                    })
                    .collect();
                let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
                engine.process(&tone, &tone, &mut out_left, &mut out_right);
                for sample in out_left {
                    largest = largest.max((sample - self.previous).abs());
                    self.previous = sample;
                }
            }
            largest
        }
    }

    #[test]
    fn without_the_hall_only_the_direct_sound_is_left() {
        let (mut engine, ..) = engine(0.0);
        let mut left = vec![0.0; BLOCK];
        left[0] = 1.0;
        let right = vec![0.0; BLOCK];
        let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        engine.process(&left, &right, &mut out_left, &mut out_right);

        let direct = engine.before + 1;
        assert!(energy(&out_left[..direct]) > 0.0);
        assert_eq!(energy(&out_left[direct..]), 0.0);
        assert_eq!(energy(&out_right[direct..]), 0.0);
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
        assert!(energy(&out_left) > energy(&out_right));
    }

    #[test]
    fn a_block_split_in_two_sounds_the_same() {
        let mut seed = 1;
        let left: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let right: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();

        let (mut whole, ..) = engine(1.0);
        let (mut whole_left, mut whole_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        whole.process(&left, &right, &mut whole_left, &mut whole_right);

        let (mut halves, ..) = engine(1.0);
        let (mut split_left, mut split_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        for range in [0..200, 200..BLOCK] {
            halves.process(
                &left[range.clone()],
                &right[range.clone()],
                &mut split_left[range.clone()],
                &mut split_right[range],
            );
        }
        for (whole, split) in whole_left.iter().zip(&split_left) {
            assert!((whole - split).abs() < 1e-4);
        }
    }

    #[test]
    fn turning_the_head_moves_the_stage_the_other_way() {
        // The left of the wide stage is about 50 degrees to the left. With
        // the head turned 80 degrees to the left it is right of the nose.
        let direct_sound = |head: f32| {
            let (mut engine, _, _, params) = engine(1.0);
            params.head_yaw.set(head);
            let mut left = vec![0.0; BLOCK];
            left[0] = 1.0;
            let right = vec![0.0; BLOCK];
            let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
            engine.process(&left, &right, &mut out_left, &mut out_right);
            // Before the first reflection
            let direct = (0.004 * RATE) as usize;
            energy(&out_left[..direct]) / energy(&out_right[..direct])
        };
        assert!(direct_sound(0.0) > 2.0);
        assert!(direct_sound(80.0) < 0.5);
    }

    // The test build's allocator aborts on an allocation or a free in here,
    // as the debug build's does on the audio thread
    #[test]
    fn processing_turning_and_switching_halls_do_not_allocate() {
        let (mut engine, mut handle, mut library, params) = engine(0.5);
        handle.switch_to(bank(&mut library, 0, 2)).ok().unwrap();
        let mut seed = 1;
        let left: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let right: Vec<f32> = (0..BLOCK).map(|_| noise(&mut seed)).collect();
        let (mut out_left, mut out_right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        assert_no_alloc::assert_no_alloc(|| {
            // Long enough for the fade and for every convolver stage to run
            // many times
            for block in 0..400 {
                params.head_yaw.set(block as f32 - 200.0);
                engine.process(&left, &right, &mut out_left, &mut out_right);
            }
        });
        assert!(engine.incoming.is_none());
        assert_eq!(handle.retired.slots(), 1);
    }

    #[test]
    fn switching_halls_does_not_click() {
        let (mut engine, mut handle, mut library, _) = engine(1.0);
        let mut tone = Tone {
            phase: 0.0,
            previous: 0.0,
        };

        // Let the reverberation build up, then measure it undisturbed
        tone.largest_step(&mut engine, 4 * RATE as usize / BLOCK, |_| {});
        let mut before = tone.largest_step(&mut engine, 50, |_| {});

        for (stage, seat) in [(0, 2), (1, 1), (0, 0)] {
            handle
                .switch_to(bank(&mut library, stage, seat))
                .ok()
                .unwrap();
            let during = tone.largest_step(&mut engine, FADE / BLOCK + 2, |_| {});
            assert!(engine.incoming.is_none());
            // Settled in the new hall
            tone.largest_step(&mut engine, 4 * RATE as usize / BLOCK, |_| {});
            let after = tone.largest_step(&mut engine, 50, |_| {});
            assert!(
                during <= 1.5 * before.max(after),
                "to stage {stage} seat {seat}: {during} during the fade, {before} before, {after} after"
            );
            before = after;
        }
    }

    #[test]
    fn turning_the_head_does_not_click() {
        let (mut engine, _, _, params) = engine(1.0);
        let mut tone = Tone {
            phase: 0.0,
            previous: 0.0,
        };
        tone.largest_step(&mut engine, 4 * RATE as usize / BLOCK, |_| {});

        // The largest step a still head gives, anywhere it may be turned to
        let mut still = 0.0f32;
        for head in [-60.0, -30.0, 0.0, 30.0, 60.0] {
            params.head_yaw.set(head);
            tone.largest_step(&mut engine, 4, |_| {});
            still = still.max(tone.largest_step(&mut engine, 20, |_| {}));
        }

        // A fast turn: 60 degrees one way and back in about half a second
        let turning = tone.largest_step(&mut engine, 48, |block| {
            let swing = (block as f32 * TAU / 48.0).sin();
            params.head_yaw.set(60.0 * swing);
        });
        assert!(
            turning <= 1.5 * still,
            "{turning} while turning, {still} still"
        );
    }

    /// Run with `cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn measure_load() {
        let (mut engine, mut handle, mut library, params) = engine(0.5);
        let mut seed = 1;
        let cycle = BLOCK as f64 / RATE;
        let mut measure = |engine: &mut Engine, blocks: usize, turning: bool| {
            let (mut total, mut slowest) = (0.0f64, 0.0f64);
            for block in 0..blocks {
                if turning {
                    params.head_yaw.set((block % 120) as f32 - 60.0);
                }
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

        let (average, slowest) = measure(&mut engine, (60.0 / cycle) as usize, false);
        println!(
            "head still: average {average:.1}% and at most {slowest:.1}% of a {:.1} ms cycle",
            1000.0 * cycle
        );
        let (average, slowest) = measure(&mut engine, (60.0 / cycle) as usize, true);
        println!("head turning: average {average:.1}% and at most {slowest:.1}%");
        handle.switch_to(bank(&mut library, 0, 2)).ok().unwrap();
        let (average, slowest) = measure(&mut engine, FADE / BLOCK, true);
        println!("turning while switching halls: average {average:.1}% and at most {slowest:.1}%");
    }
}
