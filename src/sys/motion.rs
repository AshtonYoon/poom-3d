//! Head tracking: the headphones' motion sensors, read through Core Motion.

use std::{
    sync::{Arc, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

use objc2_core_motion::{CMAuthorizationStatus, CMHeadphoneMotionManager};
use objc2_foundation::{NSBundle, ns_string};

use crate::{head::Tracker, state::Params};

/// How often the head's heading is read.
const INTERVAL: Duration = Duration::from_millis(20);
/// How often, and after how much of a turn, the log says where the head is.
const LOG_INTERVAL: Duration = Duration::from_secs(1);
const LOG_DEGREES: f32 = 5.0;

/// Starts following the head on a thread of its own. While
/// `params.tracking` is set and the headphones report their motion,
/// `params.head_yaw` is how far the head is turned from the stage.
pub fn follow_head(params: Arc<Params>) {
    // macOS ends a process that touches motion data without saying why in
    // its Info.plist, as a binary run outside the app bundle would
    let described = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(ns_string!("NSMotionUsageDescription"))
        .is_some();
    if !described {
        println!("head tracking: unavailable outside the app bundle");
        return;
    }
    thread::spawn(move || run(&params));
}

fn run(params: &Params) {
    let status = unsafe { CMHeadphoneMotionManager::authorizationStatus() };
    if status == CMAuthorizationStatus::Denied || status == CMAuthorizationStatus::Restricted {
        println!(
            "head tracking: not allowed. Allow Poom3D in System Settings > Privacy & Security > \
             Motion & Fitness."
        );
        return;
    }
    let manager = unsafe { CMHeadphoneMotionManager::new() };
    if !unsafe { manager.isDeviceMotionAvailable() } {
        println!("head tracking: this Mac can't read headphone motion");
        return;
    }

    let mut tracker = Tracker::default();
    let mut updating = false;
    // Whether the headphones are reporting, to say so when it changes
    let mut reporting = false;
    let mut read_at = Instant::now();
    let (mut logged_at, mut logged) = (Instant::now(), 0.0f32);
    loop {
        let wanted = params.tracking.load(Ordering::Relaxed);
        if wanted != updating {
            unsafe {
                if wanted {
                    manager.startDeviceMotionUpdates();
                } else {
                    manager.stopDeviceMotionUpdates();
                }
            }
            updating = wanted;
        }

        // Headphones that are off, or not the kind that senses motion,
        // report nothing: the stage is then straight ahead
        let motion = updating
            .then(|| unsafe { manager.deviceMotion() })
            .flatten();
        let now = Instant::now();
        match &motion {
            Some(motion) => {
                // Core Motion counts yaw anti-clockwise seen from above:
                // positive is to the left
                let heading = unsafe { motion.attitude().yaw() }.to_degrees() as f32;
                let seconds = (now - read_at).as_secs_f32();
                let face_stage = params.face_stage.swap(false, Ordering::Relaxed);
                let turned = tracker.update(heading, seconds, face_stage);
                params.head_yaw.set(turned);

                // Shows in the log whether left and right are the right way round
                if now - logged_at >= LOG_INTERVAL && (turned - logged).abs() >= LOG_DEGREES {
                    let side = if turned >= 0.0 { "left" } else { "right" };
                    println!("head: {:.0} degrees to the {side}", turned.abs());
                    (logged_at, logged) = (now, turned);
                }
            }
            None => {
                tracker.reset();
                params.head_yaw.set(0.0);
            }
        }
        read_at = now;

        if motion.is_some() != reporting {
            reporting = motion.is_some();
            println!(
                "head tracking: {}",
                if reporting {
                    "following the head"
                } else {
                    "no motion from the headphones"
                }
            );
        }
        thread::sleep(INTERVAL);
    }
}
