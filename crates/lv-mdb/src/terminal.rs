use chrono::Local;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::termios::{self, FlushArg, LocalFlags, SetArg, SpecialCharacterIndices, Termios};
use nix::unistd;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

#[derive(Default)]
struct ConsoleState {
    countdown: String,
}

#[derive(Clone, Default)]
pub struct Console {
    state: Arc<Mutex<ConsoleState>>,
}

impl Console {
    pub(crate) fn log(&self, message: impl AsRef<str>) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stdout = io::stdout().lock();
        erase_countdown(&mut stdout, &state.countdown);
        let _ = writeln!(stdout, "{}", message.as_ref());
        redraw_countdown(&mut stdout, &state.countdown);
    }

    pub(crate) fn set_countdown(&self, text: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stdout = io::stdout().lock();
        erase_countdown(&mut stdout, &state.countdown);
        state.countdown = text.into();
        redraw_countdown(&mut stdout, &state.countdown);
    }
}

fn erase_countdown(stdout: &mut impl Write, countdown: &str) {
    if !countdown.is_empty() {
        let _ = write!(stdout, "\r{}\r", " ".repeat(countdown.len() + 2));
    }
}

fn redraw_countdown(stdout: &mut impl Write, countdown: &str) {
    if !countdown.is_empty() {
        let _ = write!(stdout, "{countdown}");
    }
    let _ = stdout.flush();
}

pub fn timestamp() -> String {
    Local::now().format("%H:%M:%S%.3f").to_string()
}

pub struct Cbreak {
    saved: Termios,
}

impl Cbreak {
    pub(crate) fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let saved = termios::tcgetattr(&stdin).map_err(io::Error::from)?;
        let mut cbreak = saved.clone();
        cbreak
            .local_flags
            .remove(LocalFlags::ICANON | LocalFlags::ECHO);
        cbreak.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        cbreak.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        termios::tcsetattr(&stdin, SetArg::TCSANOW, &cbreak).map_err(io::Error::from)?;
        Ok(Self { saved })
    }
}

impl Drop for Cbreak {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin(), SetArg::TCSADRAIN, &self.saved);
    }
}

pub fn flush_input() {
    let _ = termios::tcflush(io::stdin(), FlushArg::TCIFLUSH);
}

pub fn read_key(timeout: Duration) -> io::Result<Option<u8>> {
    let stdin = io::stdin();
    let mut descriptors = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
    let timeout = PollTimeout::try_from(timeout).map_err(io::Error::other)?;

    match poll(&mut descriptors, timeout) {
        Ok(0) | Err(nix::errno::Errno::EINTR) => return Ok(None),
        Ok(_) => {}
        Err(error) => return Err(io::Error::from(error)),
    }
    if !descriptors[0]
        .revents()
        .is_some_and(|events| events.contains(PollFlags::POLLIN))
    {
        return Ok(None);
    }

    let mut byte = [0_u8];
    match unistd::read(&stdin, &mut byte) {
        Ok(1) => Ok(Some(byte[0])),
        Ok(_) | Err(nix::errno::Errno::EINTR) => Ok(None),
        Err(error) => Err(io::Error::from(error)),
    }
}
