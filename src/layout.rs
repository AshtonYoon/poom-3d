//! Channel layout conversion between the HAL's buffers and the stereo blocks
//! the DSP works on.

/// One HAL input buffer: `channels` interleaved channels of samples.
#[derive(Default)]
pub struct Interleaved<'a> {
    pub data: &'a [f32],
    pub channels: usize,
}

/// One HAL output buffer: `channels` interleaved channels of samples.
#[derive(Default)]
pub struct InterleavedMut<'a> {
    pub data: &'a mut [f32],
    pub channels: usize,
}

/// Copies the last two channels across `buffers` into `left` and `right` and
/// returns the number of frames copied.
///
/// The tap is the last stream of the aggregate device's input, after any
/// input streams the output device itself has. A single channel feeds both
/// sides.
pub fn read_last_stereo(buffers: &[Interleaved], left: &mut [f32], right: &mut [f32]) -> usize {
    let total: usize = buffers.iter().map(|buffer| buffer.channels).sum();
    if total == 0 {
        return 0;
    }
    let (left_source, right_source) = (total.saturating_sub(2), total - 1);

    let mut frames = left.len().min(right.len());
    let mut first = 0;
    for buffer in buffers.iter().filter(|buffer| buffer.channels > 0) {
        let holds = |channel: usize| (first..first + buffer.channels).contains(&channel);
        if holds(left_source) || holds(right_source) {
            frames = frames.min(buffer.data.len() / buffer.channels);
        }
        first += buffer.channels;
    }

    let mut first = 0;
    for buffer in buffers.iter().filter(|buffer| buffer.channels > 0) {
        for (channel, target) in [(left_source, &mut *left), (right_source, &mut *right)] {
            if (first..first + buffer.channels).contains(&channel) {
                let samples = buffer
                    .data
                    .iter()
                    .skip(channel - first)
                    .step_by(buffer.channels);
                for (out, sample) in target[..frames].iter_mut().zip(samples) {
                    *out = *sample;
                }
            }
        }
        first += buffer.channels;
    }
    frames
}

/// Writes `left` and `right` to the first two channels across `buffers`.
///
/// Other channels and frames past the end of `left`/`right` are left as they
/// are. A single output channel gets the average of both sides.
pub fn write_first_stereo(buffers: &mut [InterleavedMut], left: &[f32], right: &[f32]) {
    let total: usize = buffers.iter().map(|buffer| buffer.channels).sum();
    let mono = total == 1;

    let mut first = 0;
    for buffer in buffers.iter_mut().filter(|buffer| buffer.channels > 0) {
        let channels = buffer.channels;
        for (frame, (l, r)) in buffer
            .data
            .chunks_exact_mut(channels)
            .zip(left.iter().zip(right))
        {
            if mono {
                frame[0] = 0.5 * (l + r);
                continue;
            }
            if first == 0 {
                frame[0] = *l;
            }
            if (first..first + channels).contains(&1) {
                frame[1 - first] = *r;
            }
        }
        first += channels;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(data: &[f32], channels: usize) -> Interleaved<'_> {
        Interleaved { data, channels }
    }

    fn output(data: &mut [f32], channels: usize) -> InterleavedMut<'_> {
        InterleavedMut { data, channels }
    }

    #[test]
    fn reads_interleaved_stereo() {
        let data = [1.0, -1.0, 2.0, -2.0, 3.0, -3.0];
        let (mut left, mut right) = ([0.0; 8], [0.0; 8]);
        let frames = read_last_stereo(&[input(&data, 2)], &mut left, &mut right);
        assert_eq!(frames, 3);
        assert_eq!(left[..3], [1.0, 2.0, 3.0]);
        assert_eq!(right[..3], [-1.0, -2.0, -3.0]);
    }

    #[test]
    fn reads_tap_after_device_input_streams() {
        let microphone = [9.0, 9.0, 9.0];
        let tap = [1.0, -1.0, 2.0, -2.0];
        let (mut left, mut right) = ([0.0; 8], [0.0; 8]);
        let frames = read_last_stereo(
            &[input(&microphone, 1), input(&tap, 2)],
            &mut left,
            &mut right,
        );
        assert_eq!(frames, 2);
        assert_eq!(left[..2], [1.0, 2.0]);
        assert_eq!(right[..2], [-1.0, -2.0]);
    }

    #[test]
    fn reads_non_interleaved_stereo() {
        let (l, r) = ([1.0, 2.0], [-1.0, -2.0]);
        let (mut left, mut right) = ([0.0; 8], [0.0; 8]);
        let frames = read_last_stereo(&[input(&l, 1), input(&r, 1)], &mut left, &mut right);
        assert_eq!(frames, 2);
        assert_eq!(left[..2], [1.0, 2.0]);
        assert_eq!(right[..2], [-1.0, -2.0]);
    }

    #[test]
    fn reads_mono_to_both_sides() {
        let data = [1.0, 2.0];
        let (mut left, mut right) = ([0.0; 8], [0.0; 8]);
        let frames = read_last_stereo(&[input(&data, 1)], &mut left, &mut right);
        assert_eq!(frames, 2);
        assert_eq!(left[..2], [1.0, 2.0]);
        assert_eq!(right[..2], [1.0, 2.0]);
    }

    #[test]
    fn read_is_limited_by_the_scratch_size() {
        let data = [1.0, -1.0, 2.0, -2.0, 3.0, -3.0];
        let (mut left, mut right) = ([0.0; 2], [0.0; 2]);
        assert_eq!(
            read_last_stereo(&[input(&data, 2)], &mut left, &mut right),
            2
        );
        assert_eq!(read_last_stereo(&[], &mut left, &mut right), 0);
    }

    #[test]
    fn writes_interleaved_stereo_and_leaves_other_channels() {
        let mut data = [7.0; 8];
        write_first_stereo(&mut [output(&mut data, 4)], &[1.0, 2.0], &[-1.0, -2.0]);
        assert_eq!(data, [1.0, -1.0, 7.0, 7.0, 2.0, -2.0, 7.0, 7.0]);
    }

    #[test]
    fn writes_non_interleaved_stereo() {
        let (mut l, mut r) = ([0.0; 2], [0.0; 2]);
        write_first_stereo(
            &mut [output(&mut l, 1), output(&mut r, 1)],
            &[1.0, 2.0],
            &[-1.0, -2.0],
        );
        assert_eq!(l, [1.0, 2.0]);
        assert_eq!(r, [-1.0, -2.0]);
    }

    #[test]
    fn writes_mono_as_the_average() {
        let mut data = [0.0; 2];
        write_first_stereo(&mut [output(&mut data, 1)], &[1.0, 2.0], &[0.0, 4.0]);
        assert_eq!(data, [0.5, 3.0]);
    }
}
