//! The hall's impulse responses, prepared for the convolvers.
//!
//! Everything here allocates and takes its time: it runs on the control
//! thread, never on the audio thread.

use std::{f64::consts::PI, fmt, io::Cursor};

// St. Paul's Hall, Huddersfield, from the MAIR library (CC-BY-4.0, see
// assets/ir/mair): a KU100 dummy head 3.9 m from the stage, and the
// loudspeakers about 30 degrees to its left (S03) and right (S06).
const SOURCES: [[&[u8]; 2]; 2] = [
    [
        include_bytes!("../assets/ir/mair/Main_KU100_M01_S03_FL.wav"),
        include_bytes!("../assets/ir/mair/Main_KU100_M02_S03_FR.wav"),
    ],
    [
        include_bytes!("../assets/ir/mair/Main_KU100_M01_S06_FL.wav"),
        include_bytes!("../assets/ir/mair/Main_KU100_M02_S06_FR.wav"),
    ],
];

/// Zero crossings of the resampling filter on each side of its centre.
const RESAMPLER_REACH: f64 = 32.0;
/// The responses start where the first of them reaches this share of the
/// highest peak, less the lead-in.
const ONSET_THRESHOLD: f32 = 0.1;
const LEAD_IN_SECONDS: f64 = 0.001;

#[derive(Debug)]
pub struct IrError(String);

impl fmt::Display for IrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot read the hall's impulse responses: {}", self.0)
    }
}

impl std::error::Error for IrError {}

/// A hall heard from one seat: for each stage side (left, right), the
/// response at each ear (left, right).
pub struct Hall {
    rate: f64,
    responses: [[Vec<f32>; 2]; 2],
}

impl Hall {
    /// Decodes the built-in hall at the rate it was measured at.
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

        let [left, right] = SOURCES;
        let responses = [
            [decode(left[0])?, decode(left[1])?],
            [decode(right[0])?, decode(right[1])?],
        ];
        Ok(Self { rate, responses })
    }

    /// The responses at `rate`, without the time the sound took to reach the
    /// seat, scaled so the hall is about as loud as the sound put into it.
    pub fn prepare(&self, rate: f64) -> [[Vec<f32>; 2]; 2] {
        let mut responses = self
            .responses
            .each_ref()
            .map(|ears| ears.each_ref().map(|ear| resample(ear, self.rate, rate)));
        let all = || responses.iter().flatten();

        // Cutting all four at the same point keeps the delays between the
        // ears and between the stage sides
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
        let start = onset.saturating_sub((LEAD_IN_SECONDS * rate) as usize);

        // Uncorrelated left and right inputs of equal power come out of each
        // ear, on average, at the power they went in with
        let energy: f64 = all()
            .flatten()
            .map(|sample| f64::from(*sample).powi(2))
            .sum();
        let gain = if energy > 0.0 {
            (2.0 / energy).sqrt() as f32
        } else {
            0.0
        };

        for ear in responses.iter_mut().flatten() {
            ear.drain(..start.min(ear.len()));
            ear.iter_mut().for_each(|sample| *sample *= gain);
        }
        responses
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
    fn each_stage_side_is_louder_in_the_ear_on_its_side() {
        let [left, right] = Hall::load().unwrap().prepare(48000.0);
        assert!(energy(&left[0]) > energy(&left[1]));
        assert!(energy(&right[1]) > energy(&right[0]));
    }

    #[test]
    fn prepared_responses_start_at_the_direct_sound_and_keep_power() {
        let rate = 48000.0;
        let responses = Hall::load().unwrap().prepare(rate);
        let all = || responses.iter().flatten();

        let peak = all()
            .flatten()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let onset = all()
            .filter_map(|ear| ear.iter().position(|sample| sample.abs() >= 0.1 * peak))
            .min()
            .unwrap();
        assert!(onset as f64 <= LEAD_IN_SECONDS * rate, "onset at {onset}");

        let total: f64 = all().map(|ear| energy(ear)).sum();
        assert!((total - 2.0).abs() < 1e-3, "energy {total}");
    }
}
