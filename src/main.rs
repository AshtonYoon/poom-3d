//! Boom3D: plays the Mac's system audio back through a concert hall.
//!
//! Stage 3 of the roadmap: system audio captured with a process tap is
//! convolved with a hall's impulse responses and mixed with the original,
//! with the stage width, the seat and the amount of hall chosen from a menu
//! bar item.
//!
//! The main thread runs the menu. The control thread owns everything Core
//! Audio and the hall's impulse responses, and is told what to do through
//! [`Event`]s. The audio thread runs the [`Engine`].

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
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use dsp::{Bank, Engine, EngineHandle};
use ir::{Library, SEATS, STAGES};
use state::{AtomicF32, Event, Params, Selection, Stats};
use sys::{
    hal::{Hal, MAX_FRAMES, Pipeline},
    tcc::{self, Permission},
    ui,
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
/// How long the change from one stage or seat to another takes.
const FADE_SECONDS: f64 = 0.3;

const ALLOW: &str = "Boom3D needs to be allowed in System Settings > Privacy & Security > \
                     Screen & System Audio Recording > System Audio Recording Only.";

/// A conservative share of hall: too much smears the music.
const DEFAULT_WET: f32 = 0.4;
/// The loudspeakers about 35 degrees to each side, heard from the front.
const DEFAULT_SELECTION: Selection = Selection {
    enabled: true,
    stage: 1,
    seat: 0,
};

const USAGE: &str = "usage: boom3d [--wet LEVEL] [--off] [--probe] [--seconds N]

  --wet LEVEL  how much of the output is the hall to start with, from 0 (the
               original sound only) to 1 (the hall only); 0.4 if not given
  --off        start with the hall turned off, until it's turned on in the menu
  --probe      build the tap and aggregate device, print them, and exit
               without starting audio
  --seconds N  quit after N seconds";

struct Args {
    wet: f32,
    off: bool,
    probe: bool,
    seconds: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        wet: DEFAULT_WET,
        off: false,
        probe: false,
        seconds: None,
    };
    let mut words = std::env::args().skip(1);
    while let Some(word) = words.next() {
        match word.as_str() {
            "--off" => args.off = true,
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

    if args.probe {
        let probed = Hal::new(events, stats).and_then(|hal| hal.probe());
        return match probed {
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

    let library = match Library::load() {
        Ok(library) => library,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let quit = events.clone();
    if let Err(error) = ctrlc::set_handler(move || {
        let _ = quit.send(Event::Shutdown);
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

    let params = Arc::new(Params {
        wet: AtomicF32::new(args.wet),
    });
    let deadline = args
        .seconds
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));

    let selection = Selection {
        enabled: !args.off,
        ..DEFAULT_SELECTION
    };
    let control = Control {
        events: events.clone(),
        inbox,
        stats,
        params: params.clone(),
        library,
        selection,
        deadline,
    };
    thread::spawn(move || std::process::exit(control.run()));

    ui::run(selection, events, params)
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

/// A running pipeline and the control thread's end of its engine.
struct Running {
    pipeline: Pipeline,
    engine: EngineHandle,
}

/// What the control thread works with.
struct Control {
    events: Sender<Event>,
    inbox: Receiver<Event>,
    stats: Arc<Stats>,
    params: Arc<Params>,
    library: Library,
    selection: Selection,
    deadline: Option<Instant>,
}

impl Control {
    /// Runs until asked to quit or the deadline passes. Returns the
    /// process's exit code.
    fn run(mut self) -> i32 {
        // Core Audio objects stay on this thread
        let hal = match Hal::new(self.events.clone(), self.stats.clone()) {
            Ok(hal) => hal,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        };
        self.keep_playing(&hal);
        self.report();
        0
    }

    /// Starts moving system audio through the chosen stage and seat.
    fn start(&mut self, hal: &Hal) -> Option<Running> {
        let Selection { stage, seat, .. } = self.selection;
        let (library, params) = (&mut self.library, &self.params);
        let started = hal.start(|info| {
            let responses = library.responses(stage, seat, info.output_rate);
            let bank = Bank::new(&responses, info.buffer_frames as usize);
            let fade = (FADE_SECONDS * info.output_rate) as usize;
            Engine::new(bank, MAX_FRAMES, fade, params.clone())
        });
        match started {
            Ok((pipeline, engine)) => {
                println!("playing to {}", pipeline.info());
                Some(Running { pipeline, engine })
            }
            Err(error) => {
                eprintln!("{error}; trying again");
                None
            }
        }
    }

    /// Has a running engine fade over to the chosen stage and seat.
    fn change_hall(&mut self, running: &mut Running) {
        let Selection { stage, seat, .. } = self.selection;
        println!("stage {}, seat {}", STAGES[stage].name, SEATS[seat].name);
        let info = running.pipeline.info();
        let responses = self.library.responses(stage, seat, info.output_rate);
        let bank = Bank::new(&responses, info.buffer_frames as usize);
        if running.engine.switch_to(bank).is_err() {
            eprintln!("too many changes are waiting; this one was dropped");
        }
    }

    /// Keeps a pipeline running on the default output device while the hall
    /// is turned on.
    fn keep_playing(&mut self, hal: &Hal) {
        let mut running: Option<Running> = None;
        let mut unheard: Option<Unheard> = None;
        // When to try starting again, and when to look at the output device
        let mut retry_at: Option<Instant> = None;
        let mut settled_at: Option<Instant> = None;

        loop {
            let now = Instant::now();
            if self.deadline.is_some_and(|deadline| deadline <= now) {
                return;
            }

            if settled_at.is_some_and(|at| at <= now) {
                settled_at = None;
                if running
                    .as_ref()
                    .is_some_and(|running| !hal.is_current(&running.pipeline))
                {
                    println!("output device changed");
                    running = None;
                }
            }

            if self.selection.enabled && running.is_none() && retry_at.is_none_or(|at| at <= now) {
                running = self.start(hal);
                retry_at = running.is_none().then(|| Instant::now() + RETRY);
                unheard = running.is_some().then(|| Unheard {
                    check_at: Instant::now() + CHECK,
                    cycles: self.stats.cycles.load(Ordering::Relaxed),
                    cycling_since: None,
                    warned: false,
                });
            }
            if running.is_none() {
                unheard = None;
            }

            if let Some(watch) = &mut unheard
                && watch.check_at <= now
            {
                watch.check_at = now + CHECK;
                // IO may not run at all until something that is tapped plays
                if self.stats.cycles.load(Ordering::Relaxed) != watch.cycles {
                    let since = *watch.cycling_since.get_or_insert(now);
                    if self.stats.input_peak.get() > 0.0 {
                        println!("system audio is passing through Boom3D");
                        unheard = None;
                    } else if !watch.warned && now - since >= SILENCE {
                        // A denied tap runs, delivers silence and mutes what it taps
                        eprintln!("The tap is silent. If something is playing, {ALLOW}");
                        watch.warned = true;
                    }
                }
            }

            // Wake up for whichever comes first
            let retry = if self.selection.enabled && running.is_none() {
                retry_at
            } else {
                None
            };
            let wake = [
                self.deadline,
                retry,
                settled_at,
                unheard.as_ref().map(|watch| watch.check_at),
            ]
            .into_iter()
            .flatten()
            .min();
            let event = match wake {
                Some(wake) => self
                    .inbox
                    .recv_timeout(wake.saturating_duration_since(Instant::now())),
                None => self
                    .inbox
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
            };

            match event {
                Ok(Event::OutputChanged) => settled_at = Some(Instant::now() + SETTLE),
                Ok(Event::Enabled(enabled)) => {
                    println!("{}", if enabled { "on" } else { "off" });
                    self.selection.enabled = enabled;
                    retry_at = None;
                    if !enabled {
                        // Without the pipeline the original sound plays as
                        // if the app weren't there
                        running = None;
                    }
                }
                Ok(Event::Stage(stage)) if stage < STAGES.len() => {
                    self.selection.stage = stage;
                    if let Some(running) = &mut running {
                        self.change_hall(running);
                    }
                }
                Ok(Event::Seat(seat)) if seat < SEATS.len() => {
                    self.selection.seat = seat;
                    if let Some(running) = &mut running {
                        self.change_hall(running);
                    }
                }
                Ok(Event::Stage(_) | Event::Seat(_)) => {}
                Ok(Event::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Prints what the audio thread counted.
    fn report(&self) {
        let stats = &self.stats;
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
    }
}
