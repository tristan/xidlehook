#[cfg(feature = "nix")] extern crate nix;
#[cfg(feature = "pulse")] extern crate libpulse_sys;
#[macro_use] extern crate clap;
#[macro_use] extern crate failure;
extern crate mio;
extern crate x11;

#[cfg(feature = "pulse")] mod pulse;

#[cfg(feature = "pulse")] use pulse::PulseAudio;
#[cfg(feature = "pulse")] use std::sync::mpsc;
use clap::{App as ClapApp, Arg};
use failure::Error;
use mio::{*, unix::EventedFd};
#[cfg(feature = "nix")]
use nix::sys::{
    signal::{Signal, SigSet},
    signalfd::{SignalFd}
};
use std::{
    collections::HashMap,
    fs,
    io::prelude::*,
    mem,
    os::{
        raw::{c_char, c_void},
        unix::{
            io::AsRawFd,
            net::UnixListener
        }
    },
    path::Path,
    process::Command,
    ptr,
    time::Duration
};
use x11::{
    xlib::{Display, XA_ATOM, XCloseDisplay, XDefaultRootWindow, XFree, XGetInputFocus, XGetWindowProperty,
           XInternAtom, XOpenDisplay},
    xss::{XScreenSaverAllocInfo, XScreenSaverInfo, XScreenSaverQueryInfo}
};

struct DeferXClose(*mut Display);
impl Drop for DeferXClose {
    fn drop(&mut self) {
        unsafe { XCloseDisplay(self.0); }
    }
}
struct DeferXFree(*mut c_void);
impl Drop for DeferXFree {
    fn drop(&mut self) {
        unsafe { XFree(self.0); }
    }
}
struct DeferRemove<T: AsRef<Path>>(T);
impl<T: AsRef<Path>> Drop for DeferRemove<T> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

const SCALE: u64 = 60; // Second:minute scale. Can be changed for debugging purposes.
const NET_WM_STATE: &str = "_NET_WM_STATE\0";
const NET_WM_STATE_FULLSCREEN: &str = "_NET_WM_STATE_FULLSCREEN\0";

const COMMAND_DEACTIVATE: u8 = 0;
const COMMAND_ACTIVATE:   u8 = 1;
const COMMAND_TRIGGER:    u8 = 2;

#[cfg(feature = "nix")]
const TOKEN_SIGNAL: Token = Token(0);
const TOKEN_SERVER: Token = Token(1);
const TOKEN_CLIENT: Token = Token(2);

fn main() -> Result<(), Error> {
    let clap_app = ClapApp::new(crate_name!())
        .author(crate_authors!())
        .version(crate_version!())
        // Flags
        .arg(
            Arg::with_name("print")
                .help("Print the idle time to standard output. This is similar to xprintidle.")
                .long("print")
        )
        .arg(
            Arg::with_name("not-when-fullscreen")
                .help("Don't invoke the timer when the current application is fullscreen. \
                       Useful for preventing the lockscreen when watching videos")
                .long("not-when-fullscreen")
                .conflicts_with("print")
        )
        .arg(
            Arg::with_name("once")
                .help("Exit after timer command has been invoked once. \
                       This does not include manual invoking using the socket.")
                .long("once")
                .conflicts_with("print")
        )
        // Options
        .arg(
            Arg::with_name("time")
                .help("Set the required amount of idle minutes before invoking timer")
                .long("time")
                .takes_value(true)
                .required_unless("print")
                .conflicts_with("print")
        )
        .arg(
            Arg::with_name("timer")
                .help("Set command to run when the timer goes off")
                .long("timer")
                .takes_value(true)
                .required_unless("print")
                .conflicts_with("print")
        )
        .arg(
            Arg::with_name("notify")
                .help("Run the command passed by --notifier _ seconds before timer goes off")
                .long("notify")
                .takes_value(true)
                .requires("notifier")
                .conflicts_with("print")
        )
        .arg(
            Arg::with_name("notifier")
                .help("Set the command to run when notifier goes off (see --notify)")
                .long("notifier")
                .takes_value(true)
                .requires("notify")
                .conflicts_with("print")
        )
        .arg(
            Arg::with_name("canceller")
                .help("Set the command to run when user cancels the timer after the notifier has already gone off")
                .long("canceller")
                .takes_value(true)
                .requires("notify")
                .conflicts_with("print")
        )
        .arg(
            Arg::with_name("socket")
                .help("Listen to events over a unix socket")
                .long("socket")
                .takes_value(true)
                .conflicts_with("print")
        );
    #[cfg(feature = "pulse")]
    let mut clap_app = clap_app; // make mutable
    #[cfg(feature = "pulse")] {
        clap_app = clap_app
            .arg(
                Arg::with_name("not-when-audio")
                    .help("Don't invoke the timer when any audio is playing (PulseAudio specific)")
                    .long("not-when-audio")
                    .conflicts_with("print")
            );
    }
    let matches = clap_app.get_matches();

    let display = unsafe { XOpenDisplay(ptr::null()) };
    if display.is_null() {
        bail!("failed to open x server");
    }
    let _cleanup = DeferXClose(display);

    let info = unsafe { XScreenSaverAllocInfo() };
    let _cleanup = DeferXFree(info as *mut c_void);

    if matches.is_present("print") {
        println!("{}", get_idle(display, info)?);
        return Ok(());
    }

    #[cfg(feature = "nix")]
    let signal = {
        let mut mask = SigSet::empty();
        mask.add(Signal::SIGINT);
        mask.add(Signal::SIGTERM);

        // signalfd won't receive stuff unless
        // we make the signals be sent synchronously
        mask.thread_block()?;

        SignalFd::new(&mask)?
    };


    let time = value_t!(matches, "time", u32).unwrap_or_else(|err| err.exit()) as u64 * SCALE;
    let mut app = App {
        active: true,
        audio: false,
        delay: Duration::from_secs(SCALE),

        display,
        info,

        not_when_fullscreen: matches.is_present("not-when-fullscreen"),
        once: matches.is_present("once"),
        time,
        timer: matches.value_of("timer").unwrap().to_string(),
        notify: value_t!(matches, "notify", u32).ok().map(|notify| notify as u64),
        notifier: matches.value_of("notifier").map(String::from),
        canceller: matches.value_of("canceller").map(String::from),

        ran_notify: false,
        ran_timer:  false,

        fullscreen: None,

        time_new: time
    };

    #[cfg(feature = "pulse")]
    let (tx_pulse, rx_pulse) = mpsc::channel();
    #[cfg(feature = "pulse")]
    let mut _pulse = None;
    #[cfg(feature = "pulse")] {
        if matches.is_present("not-when-audio") {
            // be careful not to move the struct
            _pulse = Some(PulseAudio::default());
            unsafe {
                _pulse.as_mut().unwrap().connect(tx_pulse);
            }
        }
    }

    let poll = Poll::new()?;

    #[cfg(feature = "nix")]
    poll.register(&EventedFd(&signal.as_raw_fd()), TOKEN_SIGNAL, Ready::readable(), PollOpt::edge())?;

    let mut _socket = None;
    let mut listener = match matches.value_of("socket") {
        None => None,
        Some(socket) => {
            let listener = UnixListener::bind(&socket)?;
            _socket = Some(DeferRemove(socket));
            poll.register(&EventedFd(&listener.as_raw_fd()), TOKEN_SERVER, Ready::readable(), PollOpt::edge())?;
            Some(listener)
        }
    };
    let mut clients = HashMap::new();
    let mut next_client = TOKEN_CLIENT.into();

    let mut events = Events::with_capacity(1024);

    'main: loop {
        poll.poll(&mut events, Some(app.delay))?;

        for event in &events {
            match event.token() {
                #[cfg(feature = "nix")]
                TOKEN_SIGNAL => break 'main Ok(()),
                TOKEN_SERVER => {
                    let listener = listener.as_mut().expect("got event on non-existant socket");
                    let (socket, _) = listener.accept()?;
                    let token = Token(next_client);
                    poll.register(&EventedFd(&socket.as_raw_fd()), token,
                                  Ready::readable(), PollOpt::edge())?;
                    clients.insert(token, socket);
                    next_client += 1;
                },
                token => {
                    let mut byte = [0];
                    if clients.get_mut(&token).unwrap().read(&mut byte)? == 0 {
                        // EOF, drop client
                        let socket = clients.remove(&token).unwrap();
                        poll.deregister(&EventedFd(&socket.as_raw_fd()))?;
                    }

                    match byte[0] {
                        COMMAND_DEACTIVATE => app.active = false,
                        COMMAND_ACTIVATE => app.active = true,
                        COMMAND_TRIGGER => app.trigger(),
                        byte => eprintln!("socket: unknown command: {}", byte)
                    }
                }
            }
        }

        #[cfg(feature = "pulse")] {
            while let Ok(count) = rx_pulse.try_recv() {
                // If the number of active audio devices is more than 0
                app.audio = count > 0;
            }
        }

        if let Some(val) = app.step() {
            break val;
        }
    }
}
struct App {
    active: bool,
    audio: bool,
    delay: Duration,

    display: *mut Display,
    info: *mut XScreenSaverInfo,

    not_when_fullscreen: bool,
    once: bool,
    time: u64,
    timer: String,
    notify: Option<u64>,
    notifier: Option<String>,
    canceller: Option<String>,

    ran_notify: bool,
    ran_timer: bool,

    fullscreen: Option<bool>,

    time_new: u64
}
impl App {
    fn step(&mut self) -> Option<Result<(), Error>> {
        let active = self.active && !self.audio;
        // audio is always false when not-when-audio isn't set

        let default_delay = Duration::from_secs(SCALE); // TODO: const fn

        let idle = if active {
            Some(match get_idle(self.display, self.info) {
                Ok(idle) => idle / 1000, // Convert to seconds
                Err(err) => return Some(Err(err))
            })
        } else { None };

        if active &&
            self.notify.map(|notify| (idle.unwrap() + notify) >= self.time).unwrap_or(idle.unwrap() >= self.time) {
            let idle = idle.unwrap();
            if self.not_when_fullscreen && self.fullscreen.is_none() {
                let mut focus = 0u64;
                let mut revert = 0i32;

                let mut actual_type = 0u64;
                let mut actual_format = 0i32;
                let mut nitems = 0u64;
                let mut bytes = 0u64;
                let mut data: *mut u8 = unsafe { mem::uninitialized() };

                self.fullscreen = Some(unsafe {
                    XGetInputFocus(self.display, &mut focus, &mut revert);

                    if XGetWindowProperty(
                        self.display,
                        focus,
                        XInternAtom(self.display, NET_WM_STATE.as_ptr() as *const c_char, 0),
                        0,
                        !0,
                        0,
                        XA_ATOM,
                        &mut actual_type,
                        &mut actual_format,
                        &mut nitems,
                        &mut bytes,
                        &mut data
                    ) != 0 {
                        eprintln!("warning: failed to get window property");
                        false
                    } else {
                        let mut fullscreen = false;

                        #[derive(Clone, Copy, Debug)]
                        enum Ptr {
                            U8(*mut u8),
                            U16(*mut u16),
                            U32(*mut u32)
                        }
                        impl Ptr {
                            unsafe fn offset(self, i: isize) -> Ptr {
                                match self {
                                    Ptr::U8(ptr)  => Ptr::U8(ptr.offset(i)),
                                    Ptr::U16(ptr) => Ptr::U16(ptr.offset(i)),
                                    Ptr::U32(ptr) => Ptr::U32(ptr.offset(i)),
                                }
                            }
                            unsafe fn deref_u64(self) -> u64 {
                                match self {
                                    Ptr::U8(ptr)  => *ptr as u64,
                                    Ptr::U16(ptr) => *ptr as u64,
                                    Ptr::U32(ptr) => *ptr as u64,
                                }
                            }
                        }

                        let data_enum = match actual_format {
                            8  => Some(Ptr::U8(data)),
                            16 => Some(Ptr::U16(data as *mut u16)),
                            32 => Some(Ptr::U32(data as *mut u32)),
                            _  => None
                        };

                        let atom = XInternAtom(
                            self.display,
                            NET_WM_STATE_FULLSCREEN.as_ptr() as *const c_char,
                            0
                        );

                        for i in 0..nitems as isize {
                            if data_enum.unwrap().offset(i).deref_u64() == atom {
                                fullscreen = true;
                                break;
                            }
                        }

                        XFree(data as *mut c_void);

                        fullscreen
                    }
                });
            }
            if !self.not_when_fullscreen || !self.fullscreen.unwrap() {
                if self.notify.is_some() && !self.ran_notify {
                    invoke(self.notifier.as_ref().unwrap());

                    self.ran_notify = true;
                    self.delay = Duration::from_secs(1);

                    // Since the delay is usually a minute, I could've exceeded both the notify and the timer.
                    // The simple solution is to change the timer to a point where it's guaranteed
                    // it's been _ seconds since the notifier.
                    self.time_new = idle + self.notify.unwrap();
                } else if idle >= self.time_new && !self.ran_timer {
                    self.trigger();

                    if self.once {
                        return Some(Ok(()));
                    }

                    self.ran_timer = true;
                    self.delay = default_delay;
                }
            }
        } else {
            if self.ran_notify && !self.ran_timer {
                // In case the user goes back from being idle between the notify and timer
                if let Some(canceller) = self.canceller.as_ref() {
                    invoke(canceller);
                }
            }
            self.delay = default_delay;
            self.ran_notify = false;
            self.ran_timer  = false;
            self.fullscreen = None;
        }

        None
    }
    fn trigger(&self) {
        invoke(&self.timer);
    }
}
fn get_idle(display: *mut Display, info: *mut XScreenSaverInfo) -> Result<u64, Error> {
    if unsafe { XScreenSaverQueryInfo(display, XDefaultRootWindow(display), info) } == 0 {
        bail!("failed to query screen saver info");
    }

    Ok(unsafe { (*info).idle })
}
fn invoke(cmd: &str) {
    if let Err(err) =
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .status() {
        eprintln!("warning: failed to invoke command: {}", err);
    }
}
