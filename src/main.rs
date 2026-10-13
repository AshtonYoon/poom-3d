//! Boom3D: plays the Mac's system audio back through a concert hall.
//!
//! Stage 2 of the roadmap: system audio captured with a process tap is
//! convolved with a hall's impulse responses and mixed with the original.

mod dsp;
mod ir;
mod layout;
mod state;
mod sys;

use std::{
    process::ExitCode,
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use dsp::Engine;
use ir::Hall;
use state::{AtomicF32, Event, Params, Stats};
use sys::{
    hal::{Hal, MAX_FRAMES, Pipeline, PipelineInfo},
    tcc::{self, Permission},
};

// Aborts debug builds on any allocation inside the audio callback
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// How long to wait before trying again after the pipeline failed to start.
const RETRY: Duration = Duration::from_secs(1);
/// Device switches arrive as a burst of notifications; wait for the last one.
const SETTLE: Duration = Duration::from_millis(300);
/// How long the control thread gets to shut down cleanly once asked to quit.
const QUIT_GRACE: Duration = Duration::from_secs(2);
/// How often a new pipeline is checked for audio.
const CHECK: Duration = Duration::from_secs(1);
/// How long IO cycles may carry only silence before it's reported.
const SILENCE: Duration = Duration::from_secs(5);

const ALLOW: &str = "Boom3D needs to be allowed in System Settings > Privacy & Security > \
                     Screen & System Audio Recording > System Audio Recording Only.";

/// A conservative share of hall: too much smears the music.
const DEFAULT_WET: f32 = 0.4;

const USAGE: &str = "usage: boom3d [--wet LEVEL] [--probe] [--seconds N]

  --wet LEVEL  how much of the output is the hall, from 0 (the original
               sound only) to 1 (the hall only); 0.4 if not given
  --probe      build the tap and aggregate device, print them, and exit
               without starting audio
  --seconds N  quit after N seconds";

struct Args {
    wet: f32,
    probe: bool,
    seconds: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        wet: DEFAULT_WET,
        probe: false,
        seconds: None,
    };
    let mut words = std::env::args().skip(1);
    while let Some(word) = words.next() {
        match word.as_str() {
            "--probe" => args.probe = true,
            "--wet" => {
                let value = words.next().ok_or("--wet needs a level")?;
                args.wet = match value.parse() {
                    Ok(wet) if (0.0..=1.0).contains(&wet) => wet,
                    _ => return Err(format!("not a level from 0 to 1: {value}")),
                };
            }
            "--seconds" => {
                let value = words.next().ok_or("--seconds needs a number")?;
                args.seconds = Some(
                    value
                        .parse()
                        .map_err(|_| format!("not a number: {value}"))?,
                );
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let permission = tcc::audio_capture_permission();
    println!("system audio recording permission: {permission:?}");

    let (events, inbox) = mpsc::channel();
    let stats = Arc::new(Stats::default());
    let hal = match Hal::new(events.clone(), stats.clone()) {
        Ok(hal) => hal,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    if args.probe {
        return match hal.probe() {
            Ok(info) => {
                println!("{info}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }

    // A denied tap delivers silence and still mutes everything it taps
    if permission == Permission::Denied {
        eprintln!("System audio recording is denied. {ALLOW}");
        return ExitCode::FAILURE;
    }

    if let Err(error) = ctrlc::set_handler(move || {
        let _ = events.send(Event::Shutdown);
        // The control thread can be stuck in the HAL, which blocks while the
        // permission prompt is up. macOS removes the tap and the aggregate
        // device of a process that exits without doing so itself.
        thread::sleep(QUIT_GRACE);
        std::process::exit(1);
    }) {
        eprintln!("cannot handle quit signals: {error}");
        return ExitCode::FAILURE;
    }

    if permission != Permission::Authorized {
        // Starting the tap blocks until the prompt is answered
        println!("If macOS asks to let Boom3D record system audio, choose Allow.");
    }

    let deadline = args
        .seconds
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let hall = match Hall::load() {
        Ok(hall) => hall,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let params = Arc::new(Params {
        wet: AtomicF32::new(args.wet),
    });
    run(&hal, &inbox, &stats, &hall, &params, deadline);

    let cycles = stats.cycles.load(Ordering::Relaxed);
    let decibels = |peak: &AtomicF32| 20.0 * peak.get().log10();
    println!(
        "stopped after {cycles} IO cycles, {} overloads, input peak {:.1} dBFS, output peak {:.1} dBFS",
        stats.overloads.load(Ordering::Relaxed),
        decibels(&stats.input_peak),
        decibels(&stats.output_peak),
    );
    let cycle = stats.cycle_nanos.load(Ordering::Relaxed) as f64;
    if cycles > 0 && cycle > 0.0 {
        let average = stats.render_nanos.load(Ordering::Relaxed) as f64 / cycles as f64;
        let slowest = stats.slowest_render_nanos.load(Ordering::Relaxed) as f64;
        println!(
            "DSP load: average {:.1}%, at most {:.1}% of a {:.1} ms IO cycle",
            100.0 * average / cycle,
            100.0 * slowest / cycle,
            cycle / 1e6,
        );
    }
    ExitCode::SUCCESS
}

/// A new pipeline that hasn't been seen to carry sound yet.
struct Unheard {
    check_at: Instant,
    /// The IO cycle count when the pipeline started.
    cycles: u64,
    /// When IO cycles were first seen running.
    cycling_since: Option<Instant>,
    /// Whether the silence has been reported.
    warned: bool,
}

/// Keeps a pipeline running on the default output device until asked to quit
/// or `deadline` passes.
fn run(
    hal: &Hal,
    inbox: &Receiver<Event>,
    stats: &Stats,
    hall: &Hall,
    params: &Arc<Params>,
    deadline: Option<Instant>,
) {
    let mut pipeline: Option<Pipeline> = None;
    let mut unheard: Option<Unheard> = None;
    loop {
        if pipeline.is_none() {
            let engine = |info: &PipelineInfo| {
                let block = info.buffer_frames as usize;
                Engine::new(hall, info.output_rate, block, MAX_FRAMES, params.clone())
            };
            match hal.start(engine) {
                Ok(started) => {
                    println!("playing to {}", started.info());
                    pipeline = Some(started);
                    unheard = Some(Unheard {
                        check_at: Instant::now() + CHECK,
                        cycles: stats.cycles.load(Ordering::Relaxed),
                        cycling_since: None,
                        warned: false,
                    });
                }
                Err(error) => eprintln!("{error}; trying again"),
            }
        }

        let now = Instant::now();
        if deadline.is_some_and(|deadline| deadline <= now) {
            return;
        }
        if let Some(watch) = &mut unheard
            && watch.check_at <= now
        {
            watch.check_at = now + CHECK;
            // IO may not run at all until something that is tapped plays
            if stats.cycles.load(Ordering::Relaxed) != watch.cycles {
                let since = *watch.cycling_since.get_or_insert(now);
                if stats.input_peak.get() > 0.0 {
                    println!("system audio is passing through Boom3D");
                    unheard = None;
                } else if !watch.warned && now - since >= SILENCE {
                    // A denied tap runs, delivers silence and mutes what it taps
                    eprintln!("The tap is silent. If something is playing, {ALLOW}");
                    watch.warned = true;
                }
            }
        }

        // Wake up for the deadline, the next retry or the audio check
        let retry = pipeline.is_none().then(|| now + RETRY);
        let wake = [
            deadline,
            retry,
            unheard.as_ref().map(|watch| watch.check_at),
        ]
        .into_iter()
        .flatten()
        .min();
        let event = match wake {
            Some(wake) => inbox.recv_timeout(wake.saturating_duration_since(now)),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };

        match event {
            Ok(Event::OutputChanged) => {
                thread::sleep(SETTLE);
                if inbox
                    .try_iter()
                    .any(|event| matches!(event, Event::Shutdown))
                {
                    return;
                }
                if pipeline
                    .as_ref()
                    .is_some_and(|pipeline| !hal.is_current(pipeline))
                {
                    println!("output device changed");
                    pipeline = None;
                    unheard = None;
                }
            }
            Ok(Event::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}
