//! Packs the head-related impulse responses into one file for the binary:
//! for each whole degree of azimuth the left ear's samples, then the right
//! ear's, as little-endian `f32`.

use std::{env, fs, path::Path};

const DIRECTORY: &str = "assets/hrir/sadie2";
const FRAMES: usize = 256;
const FULL_SCALE: f32 = (1 << 23) as f32;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={DIRECTORY}");

    let mut packed = Vec::new();
    for azimuth in 0..360 {
        let path = format!("{DIRECTORY}/azi_{azimuth},0_ele_0,0.wav");
        let mut reader = hound::WavReader::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let spec = reader.spec();
        assert_eq!(
            (spec.channels, spec.sample_rate, spec.bits_per_sample),
            (2, 48000, 24),
            "{path}"
        );
        let samples: Vec<i32> = reader
            .samples::<i32>()
            .collect::<Result<_, _>>()
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(samples.len(), 2 * FRAMES, "{path}");

        let (frames, _) = samples.as_chunks::<2>();
        for ear in 0..2 {
            for frame in frames {
                packed.extend((frame[ear] as f32 / FULL_SCALE).to_le_bytes());
            }
        }
    }

    let out = env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    fs::write(Path::new(&out).join("hrir.f32"), packed).expect("the packed file can be written");
}
