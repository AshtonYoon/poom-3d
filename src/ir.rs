//! The hall's impulse responses, prepared for the engine.
//!
//! A hall is played in two parts. The direct sound comes from head-related
//! filters for the direction the stage is in, which can follow the head as it
//! turns. Everything after it, the reflections and the reverberation, comes
//! from the hall's recordings as they are.
//!
//! Everything here allocates and takes its time: it runs on the control
//! thread, never on the audio thread.

use std::{collections::HashMap, f64::consts::PI, fmt, io::Cursor, sync::Arc};

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
    /// The loudspeaker pair in the first row, and the one in the second row
    /// that is about as far to the sides as heard from the seat.
    rows: [Recordings; 2],
}

/// The narrow stage is a little over 20 degrees to each side, close to how
/// stereo loudspeakers are set up. The wide one is about 50 degrees from the
/// first row and a little under 40 from the second.
pub const STAGES: [Stage; 2] = [
    Stage {
        name: "좁게",
        rows: [[ears!("04"), ears!("05")], [ears!("08"), ears!("09")]],
    },
    Stage {
        name: "넓게",
        rows: [[ears!("03"), ears!("06")], [ears!("10"), ears!("13")]],
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

/// The KU100's head-related impulse responses for every whole degree around
/// the horizontal plane, from the SADIE II database (Apache-2.0, see
/// assets/hrir/sadie2), as build.rs packs them: for each azimuth the left
/// ear's samples, then the right ear's, as little-endian `f32`.
const HEAD_RESPONSES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hrir.f32"));
const HEAD_RESPONSE_RATE: f64 = 48000.0;
const HEAD_RESPONSE_FRAMES: usize = 256;
/// Directions the head filters cover: one for each whole degree.
pub const AZIMUTHS: usize = 360;

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
/// Below this the time between the ears tells where a sound comes from.
const DELAY_CUE_HERTZ: f64 = 1500.0;
/// The longest the sound can take from one ear to the other.
const LONGEST_DELAY_SECONDS: f64 = 0.001;

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

/// The direct sound from every direction around the head.
pub struct HeadFilters {
    taps: usize,
    /// By azimuth and ear, each filter reversed in time.
    filters: Vec<f32>,
}

impl HeadFilters {
    /// The length of every filter.
    pub fn taps(&self) -> usize {
        self.taps
    }

    /// The filter for a sound `azimuth` whole degrees to the left of straight
    /// ahead, at the left (0) or right (1) ear. It is reversed in time: its
    /// dot product with the latest `taps` input samples is an output sample.
    pub fn reversed(&self, azimuth: usize, ear: usize) -> &[f32] {
        let start = (2 * (azimuth % AZIMUTHS) + ear) * self.taps;
        &self.filters[start..start + self.taps]
    }

    fn energy(&self, azimuth: usize) -> f64 {
        energy(self.reversed(azimuth, 0)) + energy(self.reversed(azimuth, 1))
    }
}

/// Where one side of the stage's direct sound comes from, and how loud.
#[derive(Clone, Copy, Debug)]
pub struct Direct {
    /// Degrees to the left of straight ahead, with the head facing the stage.
    pub azimuth: f32,
    pub gain: f32,
}

/// A stage heard from a seat, ready for the engine. The whole is scaled so
/// the hall is about as loud as the sound put into it.
pub struct Hall {
    pub filters: Arc<HeadFilters>,
    /// For each stage side (left, right).
    pub direct: [Direct; 2],
    /// The hall's answer after the direct sound.
    pub reverb: Responses,
}

/// A row's recordings at one sample rate, which take the longest to compute.
struct Aligned {
    /// Resampled, and cut to start together at the direct sound.
    responses: Responses,
    /// Where each stage side's direct sound comes from, and its energy over
    /// both ears.
    azimuths: [f32; 2],
    direct_energy: [f64; 2],
}

/// Every recording of the hall, decoded, and what has been made of them.
pub struct Library {
    /// At the rate they were measured at.
    rate: f64,
    /// By stage and row.
    recordings: Vec<[Responses; 2]>,
    /// By sample rate.
    filters: HashMap<u64, Arc<HeadFilters>>,
    /// By stage, row and sample rate.
    aligned: HashMap<(usize, usize, u64), Aligned>,
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
            filters: HashMap::new(),
            aligned: HashMap::new(),
        })
    }

    /// The hall for a stage and a seat (indices into [`STAGES`] and
    /// [`SEATS`]) at `rate`.
    pub fn hall(&mut self, stage: usize, seat: usize, rate: f64) -> Hall {
        let seat = &SEATS[seat];
        let filters = self
            .filters
            .entry(rate.to_bits())
            .or_insert_with(|| Arc::new(head_filters(rate)))
            .clone();
        let aligned = self
            .aligned
            .entry((stage, seat.row, rate.to_bits()))
            .or_insert_with(|| align(&self.recordings[stage][seat.row], self.rate, rate, &filters));

        let mut reverb = aligned
            .responses
            .each_ref()
            .map(|ears| ears.each_ref().map(|ear| after_direct(ear, seat, rate)));
        // The head filters play the direct sound as loud as it was recorded
        let mut direct = [0, 1].map(|side| {
            let azimuth = aligned.azimuths[side];
            let filter_energy = filters.energy(index(azimuth));
            let gain = (aligned.direct_energy[side] / filter_energy).sqrt() as f32;
            Direct {
                azimuth,
                gain: seat.direct_gain * gain,
            }
        });

        // Uncorrelated left and right inputs of equal power come out of each
        // ear, on average, at the power they went in with
        let direct_energy: f64 = direct
            .iter()
            .map(|direct| f64::from(direct.gain).powi(2) * filters.energy(index(direct.azimuth)))
            .sum();
        let reverb_energy: f64 = reverb.iter().flatten().map(|ear| energy(ear)).sum();
        let gain = (2.0 / (direct_energy + reverb_energy)).sqrt() as f32;
        for sample in reverb.iter_mut().flatten().flatten() {
            *sample *= gain;
        }
        for direct in &mut direct {
            direct.gain *= gain;
        }

        Hall {
            filters,
            direct,
            reverb,
        }
    }
}

/// The head filter for a sound `azimuth` degrees to the left.
pub fn index(azimuth: f32) -> usize {
    (azimuth.round() as i64).rem_euclid(AZIMUTHS as i64) as usize
}

fn energy(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum()
}

/// 0 before `from` seconds, 1 after `to`, a raised cosine between.
fn step(index: usize, rate: f64, (from, to): (f64, f64)) -> f32 {
    let position = ((index as f64 / rate - from) / (to - from)).clamp(0.0, 1.0);
    (0.5 - 0.5 * (PI * position).cos()) as f32
}

/// The head filters at `rate`, cut to start together like the hall's
/// responses.
fn head_filters(rate: f64) -> HeadFilters {
    let (samples, _) = HEAD_RESPONSES.as_chunks::<4>();
    let samples: Vec<f32> = samples
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    let mut responses: Vec<Vec<f32>> = samples
        .chunks(HEAD_RESPONSE_FRAMES)
        .map(|ear| resample(ear, HEAD_RESPONSE_RATE, rate))
        .collect();

    // Cutting every filter at the same point keeps the delays between the
    // ears, and how they change as the head turns
    let peak = responses
        .iter()
        .flatten()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    let onset = responses
        .iter()
        .filter_map(|ear| {
            ear.iter()
                .position(|sample| sample.abs() >= ONSET_THRESHOLD * peak)
        })
        .min()
        .unwrap_or(0);
    let start = onset.saturating_sub((LEAD_IN_SECONDS * rate) as usize);
    let taps = responses.iter().map(Vec::len).min().unwrap_or(0) - start;

    let mut filters = Vec::with_capacity(responses.len() * taps);
    for ear in &mut responses {
        filters.extend(ear[start..start + taps].iter().rev());
    }
    HeadFilters { taps, filters }
}

/// Resamples a row's recordings and removes the time the sound took to reach
/// the seat. Cutting all four at the same point keeps the delays between the
/// ears and between the stage sides.
fn align(recordings: &Responses, from: f64, to: f64, filters: &HeadFilters) -> Aligned {
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

    // The direct sound on its own, which tells where the loudspeaker stands
    let direct = responses.each_ref().map(|ears| {
        ears.each_ref().map(|ear| {
            let frames = ((DIRECT_SECONDS.1 * to) as usize).min(ear.len());
            ear[..frames]
                .iter()
                .enumerate()
                .map(|(index, sample)| sample * (1.0 - step(index, to, DIRECT_SECONDS)))
                .collect::<Vec<f32>>()
        })
    });
    let filter_delays: Vec<f64> = (0..AZIMUTHS)
        .map(|azimuth| {
            let forward = |ear| -> Vec<f32> {
                filters
                    .reversed(azimuth, ear)
                    .iter()
                    .rev()
                    .copied()
                    .collect()
            };
            interaural_delay(&forward(0), &forward(1), to)
        })
        .collect();
    let azimuths = direct.each_ref().map(|[left, right]| {
        let delay = interaural_delay(left, right, to);
        // The loudspeakers are in front: the same delay from behind is not
        // what was recorded
        (-90..=90)
            .min_by(|a, b| {
                let off = |azimuth: &i32| (filter_delays[index(*azimuth as f32)] - delay).abs();
                off(a).total_cmp(&off(b))
            })
            .unwrap_or(0) as f32
    });
    let direct_energy = direct
        .each_ref()
        .map(|[left, right]| energy(left) + energy(right));

    Aligned {
        responses,
        azimuths,
        direct_energy,
    }
}

/// How much later a sound reaches the right ear than the left, in samples:
/// the lag at which the two ears' lows match best.
fn interaural_delay(left: &[f32], right: &[f32], rate: f64) -> f64 {
    let lows = |ear: &[f32]| {
        let mut lows = ear.to_vec();
        low_pass(&mut lows, DELAY_CUE_HERTZ, rate);
        lows
    };
    let (left, right) = (lows(left), lows(right));

    let reach = (LONGEST_DELAY_SECONDS * rate) as i64;
    let match_at = |lag: i64| -> f64 {
        left.iter()
            .enumerate()
            .filter_map(|(index, sample)| {
                let other = right.get(usize::try_from(index as i64 + lag).ok()?)?;
                Some(f64::from(*sample) * f64::from(*other))
            })
            .sum()
    };
    let best = (-reach..=reach)
        .max_by(|a, b| match_at(*a).total_cmp(&match_at(*b)))
        .unwrap_or(0);

    // A parabola through the best lag and its neighbours finds the peak
    // between samples
    let (before, peak, after) = (match_at(best - 1), match_at(best), match_at(best + 1));
    let curve = before - 2.0 * peak + after;
    let between = if curve < 0.0 {
        (0.5 * (before - after) / curve).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    best as f64 + between
}

/// What follows the direct sound in an aligned response, for a seat: the
/// early reflections and the reverberation each at their level, without
/// their lows.
fn after_direct(response: &[f32], seat: &Seat, rate: f64) -> Vec<f32> {
    let mut rest: Vec<f32> = response
        .iter()
        .enumerate()
        .map(|(index, sample)| sample * step(index, rate, DIRECT_SECONDS))
        .collect();
    high_pass(&mut rest, LOW_CUT_HERTZ, rate);

    for (index, sample) in rest.iter_mut().enumerate() {
        let late = step(index, rate, EARLY_SECONDS);
        *sample *= seat.early_gain + (1.0 - seat.early_gain) * late;
    }
    rest
}

/// A second-order Butterworth filter, in place: a high-pass, or a low-pass.
fn butterworth(samples: &mut [f32], cutoff: f64, rate: f64, high: bool) {
    let k = (PI * cutoff / rate).tan();
    let norm = 1.0 / (1.0 + std::f64::consts::SQRT_2 * k + k * k);
    let (b0, b1, b2) = if high {
        (norm, -2.0 * norm, norm)
    } else {
        (k * k * norm, 2.0 * k * k * norm, k * k * norm)
    };
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

fn high_pass(samples: &mut [f32], cutoff: f64, rate: f64) {
    butterworth(samples, cutoff, rate, true);
}

fn low_pass(samples: &mut [f32], cutoff: f64, rate: f64) {
    butterworth(samples, cutoff, rate, false);
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
    fn the_filters_pass_their_side_of_the_cutoff() {
        let tone = |frequency: f64| -> Vec<f32> {
            (0..RATE as usize)
                .map(|index| (2.0 * PI * frequency * index as f64 / RATE).sin() as f32)
                .collect()
        };
        let passed = |frequency: f64, filter: fn(&mut [f32], f64, f64)| {
            let mut samples = tone(frequency);
            filter(&mut samples, 150.0, RATE);
            (energy(&samples[24000..]) / energy(&tone(frequency)[24000..])).sqrt()
        };
        let half_power = std::f64::consts::FRAC_1_SQRT_2;

        assert!((passed(2000.0, high_pass) - 1.0).abs() < 0.01);
        assert!((passed(150.0, high_pass) - half_power).abs() < 0.01);
        assert!(passed(30.0, high_pass) < 0.05);

        assert!((passed(15.0, low_pass) - 1.0).abs() < 0.01);
        assert!((passed(150.0, low_pass) - half_power).abs() < 0.01);
        assert!(passed(1500.0, low_pass) < 0.05);
    }

    #[test]
    fn the_delay_between_the_ears_is_found_between_samples() {
        let click = |at: f64| -> Vec<f32> {
            (0..256)
                .map(|index| sinc(0.25 * (index as f64 - at)) as f32)
                .collect()
        };
        let delay = interaural_delay(&click(100.0), &click(112.4), RATE);
        assert!((delay - 12.4).abs() < 0.1, "{delay}");
        let delay = interaural_delay(&click(100.0), &click(93.0), RATE);
        assert!((delay + 7.0).abs() < 0.1, "{delay}");
    }

    #[test]
    fn head_filters_favour_the_ear_on_the_sound_s_side() {
        let filters = head_filters(RATE);
        assert!(filters.taps() > 128 && filters.taps() <= HEAD_RESPONSE_FRAMES);
        let louder_left = |azimuth: usize| {
            energy(filters.reversed(azimuth, 0)) / energy(filters.reversed(azimuth, 1))
        };
        assert!(louder_left(30) > 2.0);
        assert!(louder_left(330) < 0.5);
        assert!((louder_left(0) - 1.0).abs() < 0.5);
    }

    #[test]
    fn the_stage_is_found_in_front_and_wider_stages_are_wider() {
        let mut library = Library::load().unwrap();
        for seat in [0, 1] {
            let widths: Vec<[f32; 2]> = (0..STAGES.len())
                .map(|stage| {
                    library
                        .hall(stage, seat, RATE)
                        .direct
                        .map(|direct| direct.azimuth)
                })
                .collect();
            println!("seat {seat}: {widths:?}");
            let [narrow, wide] = widths[..] else {
                panic!("two stages");
            };
            assert!(
                (15.0..=30.0).contains(&narrow[0]),
                "seat {seat}: {widths:?}"
            );
            assert!(
                (-30.0..=-15.0).contains(&narrow[1]),
                "seat {seat}: {widths:?}"
            );
            assert!((35.0..=60.0).contains(&wide[0]), "seat {seat}: {widths:?}");
            assert!(
                (-60.0..=-35.0).contains(&wide[1]),
                "seat {seat}: {widths:?}"
            );
        }
    }

    #[test]
    fn halls_keep_power_and_have_nothing_before_the_reflections() {
        let mut library = Library::load().unwrap();
        for seat in 0..SEATS.len() {
            let hall = library.hall(1, seat, RATE);
            let direct: f64 = hall
                .direct
                .iter()
                .map(|direct| {
                    f64::from(direct.gain).powi(2) * hall.filters.energy(index(direct.azimuth))
                })
                .sum();
            let reverb: f64 = hall.reverb.iter().flatten().map(|ear| energy(ear)).sum();
            assert!((direct + reverb - 2.0).abs() < 1e-3, "seat {seat}");

            let before = (DIRECT_SECONDS.0 * RATE) as usize;
            for ear in hall.reverb.iter().flatten() {
                assert!(energy(&ear[..before]) < 1e-6 * energy(ear), "seat {seat}");
            }
        }
    }

    #[test]
    fn seats_farther_back_have_less_direct_sound() {
        let mut library = Library::load().unwrap();
        let shares: Vec<f64> = (0..SEATS.len())
            .map(|seat| {
                let hall = library.hall(1, seat, RATE);
                let direct = f64::from(hall.direct[0].gain).powi(2)
                    * hall.filters.energy(index(hall.direct[0].azimuth));
                direct / (direct + energy(&hall.reverb[0][0]) + energy(&hall.reverb[0][1]))
            })
            .collect();
        assert!(shares[0] > shares[1] && shares[1] > shares[2], "{shares:?}");
    }
}
