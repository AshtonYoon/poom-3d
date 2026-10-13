//! Where the head is turned, relative to the stage.

/// How long the stage takes to come most of the way round to where the
/// listener keeps facing.
const FOLLOW_SECONDS: f32 = 30.0;

/// Turns the headphones' heading into how far the head is turned away from
/// the stage. All angles are in degrees, counted to the left.
#[derive(Default)]
pub struct Tracker {
    /// The heading at which the listener faces the stage.
    front: Option<f32>,
}

impl Tracker {
    /// Takes the heading measured `seconds` after the previous one and
    /// returns how far the head is turned from the stage. `face_stage` puts
    /// the stage where the listener is facing now.
    pub fn update(&mut self, heading: f32, seconds: f32, face_stage: bool) -> f32 {
        let front = match self.front {
            Some(front) if !face_stage => front,
            _ => heading,
        };
        // The stage slowly follows the listener, so it ends up in front of
        // someone who has turned their chair, and the sensor's drift never
        // adds up
        let follow = (seconds / FOLLOW_SECONDS).clamp(0.0, 1.0);
        let front = wrap(front + follow * wrap(heading - front));
        self.front = Some(front);
        wrap(heading - front)
    }

    /// Forgets where the stage is: the next heading faces it.
    pub fn reset(&mut self) {
        self.front = None;
    }
}

/// The same angle within half a turn of zero.
fn wrap(degrees: f32) -> f32 {
    (degrees + 180.0).rem_euclid(360.0) - 180.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_heading_faces_the_stage() {
        let mut tracker = Tracker::default();
        assert_eq!(tracker.update(130.0, 0.02, false), 0.0);
        assert!((tracker.update(150.0, 0.02, false) - 20.0).abs() < 0.1);
        assert!((tracker.update(100.0, 0.02, false) + 30.0).abs() < 0.1);
    }

    #[test]
    fn turns_are_measured_across_the_wrap() {
        let mut tracker = Tracker::default();
        tracker.update(170.0, 0.02, false);
        assert!((tracker.update(-170.0, 0.02, false) - 20.0).abs() < 0.1);
    }

    #[test]
    fn facing_the_stage_again_recentres() {
        let mut tracker = Tracker::default();
        tracker.update(0.0, 0.02, false);
        assert!(tracker.update(40.0, 0.02, false) > 39.0);
        assert_eq!(tracker.update(40.0, 0.02, true), 0.0);

        tracker.reset();
        assert_eq!(tracker.update(-75.0, 0.02, false), 0.0);
    }

    #[test]
    fn the_stage_follows_a_listener_who_stays_turned() {
        let mut tracker = Tracker::default();
        tracker.update(0.0, 0.02, false);
        let mut turned = 0.0;
        for _ in 0..(2.0 * FOLLOW_SECONDS / 0.02) as usize {
            turned = tracker.update(40.0, 0.02, false);
        }
        assert!(turned < 8.0, "{turned}");
    }
}
