//! The hall's impulse responses, prepared for the convolvers.
//!
//! Everything here allocates and takes its time: it runs on the control
//! thread, never on the audio thread.

use std::{collections::HashMap, f64::consts::PI, fmt, io::Cursor};

/// St. Paul's Hall, Huddersfield, from the MAIR library (CC-BY-4.0, see
/// assets/ir/mair): a KU100 dummy head 3.9 m from a first row of
/// loudspeakers on the stage floor, with a second row about 2.6 m behind it.
pub const HALL: &str = "St. Paul's Hall";

/// The dummy head's two ears for the loudspeaker with this number.
macro_rules! ears {
    ($speaker:literal) => {
        [
            include_bytes!(concat!(
                "../assets/ir/mair/Main_KU100_M01_S",
                $speaker,
                "_FL.wav"
            )),
            include_bytes!(concat!(
                "../assets/ir/mair/Main_KU100_M02_S",
                $speaker,
                "_FR.wav"
            )),
        ]
    };
}

/// For a stage side (left, right), the recording at each ear (left, right).
type Recordings = [[&'static [u8]; 2]; 2];

/// How far apart the left and the right of the music are on the stage.
pub struct Stage {
    pub name: &'static str,
    /// The loudspeaker pair in the first row and the matching one in the
    /// second.
    rows: [Recordings; 2],
}

pub const STAGES: [Stage; 3] = [
    Stage {
        name: "좁게",
        rows: [[ears!("04"), ears!("05")], [ears!("11"), ears!("12")]],
    },
    Stage {
        name: "표준",
        rows: [[ears!("03"), ears!("06")], [ears!("08"), ears!("09")]],
    },
    Stage {
        name: "넓게",
        rows: [[ears!("02"), ears!("07")], [ears!("10"), ears!("13")]],
    },
];

/// How far from the stage the listener sits.
///
/// The measured seat is the nearest one. The middle seat is the same seat
/// hearing the row of loudspeakers farther away; the back seat also has less
/// of the direct sound and the early reflections against the reverberation.
pub struct Seat {
    pub name: &'static str,
    row: usize,
    direct_gain: f32,
    early_gain: f32,
}

pub const SEATS: [Seat; 3] = [
    Seat {
        name: "앞줄",
        row: 0,
        direct_gain: 1.0,
        early_gain: 1.0,
    },
    Seat {
        name: "중간",
        row: 1,
        direct_gain: 1.0,
        early_gain: 1.0,
    },
    Seat {
        name: "뒷줄",
        row: 1,
        direct_gain: 0.5,
        early_gain: 0.8,
    },
];

/// Zero crossings of the resampling filter on each side of its centre.
const RESAMPLER_REACH: f64 = 32.0;
/// The responses start where the first of them reaches this share of the
/// highest peak, less the lead-in.
const ONSET_THRESHOLD: f32 = 0.1;
const LEAD_IN_SECONDS: f64 = 0.001;
/// The direct sound ends before the floor reflection, about 4 ms after it.
const DIRECT_SECONDS: (f64, f64) = (0.0045, 0.0055);
/// Where the early reflections give way to the reverberation.
const EARLY_SECONDS: (f64, f64) = (0.07, 0.09);
/// Everything but the direct sound loses its lows, which smear the music.
const LOW_CUT_HERTZ: f64 = 150.0;

#[derive(Debug)]
pub struct IrError(String);

impl fmt::Display for IrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot read the hall's impulse responses: {}", self.0)
    }
}

impl std::error::Error for IrError {}

/// For a stage side (left, right), the response at each ear (left, right).
pub type Responses = [[Vec<f32>; 2]; 2];

/// Every recording of the hall, decoded, and what has been made of them.
pub struct Library {
    /// At the rate they were measured at.
    rate: f64,
    /// By stage and row.
    recordings: Vec<[Responses; 2]>,
    /// By stage, row and sample rate: resampled and cut to start together at
    /// the direct sound, which takes the longest to compute.
    aligned: HashMap<(usize, usize, u64), Responses>,
}

impl Library {
    pub fn load() -> Result<Self, IrError> {
        let mut rate = 0.0;
        let mut decode = |bytes: &[u8]| -> Result<Vec<f32>, IrError> {
            let reader =
                hound::WavReader::new(Cursor::new(bytes)).map_err(|e| IrError(e.to_string()))?;
            let spec = reader.spec();
            if spec.channels != 1 || spec.sample_format != hound::SampleFormat::Float {
                return Err(IrError("expected mono float files".into()));
            }
            if rate != 0.0 && rate != f64::from(spec.sample_rate) {
                return Err(IrError("the files differ in sample rate".into()));
            }
            rate = f64::from(spec.sample_rate);
            reader
                .into_samples::<f32>()
                .collect::<Result<_, _>>()
                .map_err(|e| IrError(e.to_string()))
        };

        let mut recordings = Vec::new();
        for stage in &STAGES {
            let mut rows = Vec::new();
            for row in &stage.rows {
                let [left, right] = row;
                rows.push([
                    [decode(left[0])?, decode(left[1])?],
                    [decode(right[0])?, decode(right[1])?],
                ]);
            }
            let rows: [Responses; 2] = rows.try_into().expect("a stage has two rows");
            recordings.push(rows);
        }
        Ok(Self {
            rate,
            recordings,
            aligned: HashMap::new(),
        })
    }

    /// The responses for a stage and a seat (indices into [`STAGES`] and
    /// [`SEATS`]) at `rate`, scaled so the hall is about as loud as the sound
    /// put into it.
    pub fn responses(&mut self, stage: usize, seat: usize, rate: f64) -> Responses {
        let seat = &SEATS[seat];
        let aligned = self
            .aligned
            .entry((stage, seat.row, rate.to_bits()))
            .or_insert_with(|| align(&self.recordings[stage][seat.row], self.rate, rate));

        let mut responses = aligned
            .each_ref()
            .map(|ears| ears.each_ref().map(|ear| shape(ear, seat, rate)));

        // Uncorrelated left and right inputs of equal power come out of each
        // ear, on average, at the power they went in with
        let energy: f64 = responses
            .iter()
            .flatten()
            .flatten()
            .map(|sample| f64::from(*sample).powi(2))
            .sum();
        let gain = if energy > 0.0 {
            (2.0 / energy).sqrt() as f32
        } else {
            0.0
        };
        for sample in responses.iter_mut().flatten().flatten() {
            *sample *= gain;
        }
        responses
    }
}

/// Resamples the recordings and removes the time the sound took to reach the
/// seat. Cutting all four at the same point keeps the delays between the ears
/// and between the stage sides.
fn align(recordings: &Responses, from: f64, to: f64) -> Responses {
    let mut responses = recordings
        .each_ref()
        .map(|ears| ears.each_ref().map(|ear| resample(ear, from, to)));
    let all = || responses.iter().flatten();

    let peak = all()
        .flatten()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    let onset = all()
        .filter_map(|ear| {
            ear.iter()
                .position(|sample| sample.abs() >= ONSET_THRESHOLD * peak)
        })
        .min()
        .unwrap_or(0);
    let start = onset.saturating_sub((LEAD_IN_SECONDS * to) as usize);

    for ear in responses.iter_mut().flatten() {
        ear.drain(..start.min(ear.len()));
    }
    responses
}

/// Rebalances an aligned response for a seat: the direct sound, the early
/// reflections and the reverberation each get their level, and all but the
/// direct sound lose their lows.
fn shape(response: &[f32], seat: &Seat, rate: f64) -> Vec<f32> {
    // 0 before `from`, 1 after `to`, a raised cosine between
    let step = |index: usize, (from, to): (f64, f64)| {
        let position = ((index as f64 / rate - from) / (to - from)).clamp(0.0, 1.0);
        (0.5 - 0.5 * (PI * position).cos()) as f32
    };

    let mut rest: Vec<f32> = response
        .iter()
        .enumerate()
        .map(|(index, sample)| sample * step(index, DIRECT_SECONDS))
        .collect();
    high_pass(&mut rest, LOW_CUT_HERTZ, rate);

    response
        .iter()
        .zip(rest)
        .enumerate()
        .map(|(index, (sample, rest))| {
            let direct = sample * (1.0 - step(index, DIRECT_SECONDS));
            let late = step(index, EARLY_SECONDS);
            let rest_gain = seat.early_gain + (1.0 - seat.early_gain) * late;
            seat.direct_gain * direct + rest_gain * rest
        })
        .collect()
}

/// A second-order Butterworth high-pass, in place.
fn high_pass(samples: &mut [f32], cutoff: f64, rate: f64) {
    let k = (PI * cutoff / rate).tan();
    let norm = 1.0 / (1.0 + std::f64::consts::SQRT_2 * k + k * k);
    let (b0, b1, b2) = (norm, -2.0 * norm, norm);
    let a1 = 2.0 * (k * k - 1.0) * norm;
    let a2 = (1.0 - std::f64::consts::SQRT_2 * k + k * k) * norm;

    let (mut z1, mut z2) = (0.0, 0.0);
    for sample in samples {
        let input = f64::from(*sample);
        let output = b0 * input + z1;
        z1 = b1 * input - a1 * output + z2;
        z2 = b2 * input - a2 * output;
        *sample = output as f32;
    }
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Band-limited resampling with a Hann-windowed sinc.
fn resample(input: &[f32], from: f64, to: f64) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to / from;
    // As a share of the input's Nyquist frequency
    let cutoff = ratio.min(1.0);
    let reach = RESAMPLER_REACH / cutoff;

    let length = (input.len() as f64 * ratio).ceil() as usize;
    (0..length)
        .map(|index| {
            let centre = index as f64 / ratio;
            let first = (centre - reach).ceil().max(0.0) as usize;
            let last = ((centre + reach).floor() as usize).min(input.len() - 1);
            let sum: f64 = (first..=last)
                .map(|source| {
                    let offset = source as f64 - centre;
                    let window = 0.5 * (1.0 + (PI * offset / reach).cos());
                    f64::from(input[source]) * cutoff * sinc(cutoff * offset) * window
                })
                .sum();
            sum as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48000.0;

    fn energy(samples: &[f32]) -> f64 {
        samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum()
    }

    #[test]
    fn resampling_keeps_a_tone() {
        let (from, to, frequency) = (44100.0, 48000.0, 1000.0);
        let tone = |rate: f64, index: usize| (2.0 * PI * frequency * index as f64 / rate).sin();
        let input: Vec<f32> = (0..4410).map(|index| tone(from, index) as f32).collect();
        let output = resample(&input, from, to);

        assert_eq!(output.len(), 4800);
        // Away from the ends, where the filter runs off the input
        for (index, sample) in output.iter().enumerate().take(4600).skip(200) {
            assert!(
                (f64::from(*sample) - tone(to, index)).abs() < 1e-3,
                "at {index}"
            );
        }
    }

    #[test]
    fn the_high_pass_keeps_highs_and_removes_lows() {
        let tone = |frequency: f64| -> Vec<f32> {
            (0..RATE as usize)
                .map(|index| (2.0 * PI * frequency * index as f64 / RATE).sin() as f32)
                .collect()
        };
        let passed = |frequency: f64| {
            let mut samples = tone(frequency);
            high_pass(&mut samples, 150.0, RATE);
            (energy(&samples[24000..]) / energy(&tone(frequency)[24000..])).sqrt()
        };
        assert!((passed(2000.0) - 1.0).abs() < 0.01);
        assert!((passed(150.0) - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.01);
        assert!(passed(30.0) < 0.05);
    }

    #[test]
    fn every_stage_side_is_louder_in_the_ear_on_its_side() {
        let mut library = Library::load().unwrap();
        for stage in 0..STAGES.len() {
            for seat in 0..SEATS.len() {
                let [left, right] = library.responses(stage, seat, RATE);
                assert!(
                    energy(&left[0]) > energy(&left[1]),
                    "stage {stage} seat {seat}"
                );
                assert!(
                    energy(&right[1]) > energy(&right[0]),
                    "stage {stage} seat {seat}"
                );
            }
        }
    }

    #[test]
    fn responses_start_at_the_direct_sound_and_keep_power() {
        let mut library = Library::load().unwrap();
        for seat in 0..SEATS.len() {
            let responses = library.responses(1, seat, RATE);
            let all = || responses.iter().flatten();

            let peak = all()
                .flatten()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            let onset = all()
                .filter_map(|ear| ear.iter().position(|sample| sample.abs() >= 0.1 * peak))
                .min()
                .unwrap();
            assert!(
                onset as f64 <= 0.002 * RATE,
                "seat {seat}: onset at {onset}"
            );

            let total: f64 = all().map(|ear| energy(ear)).sum();
            assert!((total - 2.0).abs() < 1e-3, "seat {seat}: energy {total}");
        }
    }

    #[test]
    fn seats_farther_back_have_less_direct_sound() {
        let mut library = Library::load().unwrap();
        let direct_share = |library: &mut Library, seat: usize| {
            let [left, _] = library.responses(1, seat, RATE);
            let direct = (DIRECT_SECONDS.0 * RATE) as usize;
            energy(&left[0][..direct]) / energy(&left[0])
        };
        let shares: Vec<f64> = (0..SEATS.len())
            .map(|seat| direct_share(&mut library, seat))
            .collect();
        assert!(shares[0] > shares[1] && shares[1] > shares[2], "{shares:?}");
    }
}
