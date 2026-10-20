//! Per-thread stdout and stderr routing for commands and MCP tool calls.

use std::cell::RefCell;
use std::fmt;
use std::io::Write;

#[derive(Default)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

thread_local! {
    static CAPTURE: RefCell<Option<Captured>> = const { RefCell::new(None) };
}

struct CaptureGuard {
    previous: Option<Captured>,
    active: bool,
}

impl CaptureGuard {
    fn start() -> Self {
        let previous = CAPTURE.with(|slot| slot.replace(Some(Captured::default())));
        Self {
            previous,
            active: true,
        }
    }

    fn finish(mut self) -> Captured {
        let captured = CAPTURE.with(|slot| slot.replace(self.previous.take()));
        self.active = false;
        captured.unwrap_or_default()
    }
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        if self.active {
            CAPTURE.with(|slot| {
                slot.replace(self.previous.take());
            });
        }
    }
}

pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Captured) {
    let guard = CaptureGuard::start();
    let result = f();
    (result, guard.finish())
}

pub fn with_stdout<T>(f: impl FnOnce(&mut dyn Write) -> anyhow::Result<T>) -> anyhow::Result<T> {
    CAPTURE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(capture) = slot.as_mut() {
            f(&mut capture.stdout)
        } else {
            drop(slot);
            let stdout = std::io::stdout();
            let mut out = std::io::BufWriter::new(stdout.lock());
            let value = f(&mut out)?;
            out.flush()?;
            Ok(value)
        }
    })
}

pub fn stdout(args: fmt::Arguments<'_>) {
    write_formatted(args, false);
}

pub fn stderr(args: fmt::Arguments<'_>) {
    write_formatted(args, true);
}

fn write_formatted(args: fmt::Arguments<'_>, is_stderr: bool) {
    let captured = CAPTURE.with(|slot| {
        if let Some(capture) = slot.borrow_mut().as_mut() {
            let target = if is_stderr {
                &mut capture.stderr
            } else {
                &mut capture.stdout
            };
            target.write_fmt(args).expect("writing to an output buffer");
            true
        } else {
            false
        }
    });
    if !captured {
        if is_stderr {
            eprint!("{args}");
        } else {
            print!("{args}");
        }
    }
}
