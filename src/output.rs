use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const FRAME_INTERVAL: Duration = Duration::from_millis(80);
const LABEL: usize = 18;

struct Palette {
    bar: &'static str,
    accent: &'static str,
    green: &'static str,
    red: &'static str,
    reset: &'static str,
}

const COLOUR: Palette = Palette {
    bar: "\x1b[38;2;255;255;255m",
    accent: "\x1b[38;2;216;180;254m",
    green: "\x1b[38;2;122;189;0m",
    red: "\x1b[38;2;255;45;45m",
    reset: "\x1b[0m",
};

const PLAIN: Palette = Palette { bar: "", accent: "", green: "", red: "", reset: "" };

struct Spinner {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub struct Phases {
    palette: &'static Palette,
    tty: bool,
    open: bool,
    spinner: Option<Spinner>,
}

impl Phases {
    pub fn new() -> Self {
        let tty = std::io::stderr().is_terminal();
        let palette = if tty && std::env::var_os("NO_COLOR").is_none() { &COLOUR } else { &PLAIN };
        Self { palette, tty, open: false, spinner: None }
    }

    pub fn phase(&mut self, title: &str) {
        self.end();
        let p = self.palette;
        self.emit(&format!("\n{}┌──{} {}{title}{}", p.bar, p.reset, p.accent, p.reset));
        self.emit(&format!("{}│{}", p.bar, p.reset));
        self.open = true;
    }

    pub fn working(&mut self, label: &str) {
        self.settle();
        let p = self.palette;
        if !self.tty {
            self.emit(&format!("{}│{}   {label}", p.bar, p.reset));
            return;
        }
        let running = Arc::new(AtomicBool::new(true));
        let thread = std::thread::spawn({
            let running = Arc::clone(&running);
            let label = label.to_owned();
            move || {
                let mut frame = 0usize;
                while running.load(Ordering::Relaxed) {
                    let mut err = std::io::stderr();
                    let _ = write!(
                        err,
                        "\r{}│{}   {}{}{} {label} ",
                        p.bar, p.reset, p.accent, FRAMES[frame % FRAMES.len()], p.reset
                    );
                    let _ = err.flush();
                    frame += 1;
                    std::thread::sleep(FRAME_INTERVAL);
                }
                let mut err = std::io::stderr();
                let _ = write!(err, "\r\x1b[2K");
                let _ = err.flush();
            }
        });
        self.spinner = Some(Spinner { running, thread: Some(thread) });
    }

    pub fn done(&mut self, message: &str) {
        self.settle();
        let p = self.palette;
        self.emit(&format!("{}│{}   {}[ok]:{} {message}", p.bar, p.reset, p.green, p.reset));
    }

    pub fn field(&mut self, label: &str, value: &str) {
        self.settle();
        let p = self.palette;
        self.emit(&format!("{}│{}      {label:<LABEL$}{value}", p.bar, p.reset));
    }

    pub fn fail(&mut self, message: &str) {
        self.settle();
        let p = self.palette;
        self.emit(&format!("{}│{}   {}[FAIL]:{} {message}", p.bar, p.reset, p.red, p.reset));
        self.end();
    }

    pub fn end(&mut self) {
        self.settle();
        if std::mem::take(&mut self.open) {
            let p = self.palette;
            self.emit(&format!("{}│{}", p.bar, p.reset));
            self.emit(&format!("{}└{}", p.bar, p.reset));
        }
    }

    fn settle(&mut self) {
        if let Some(mut spinner) = self.spinner.take() {
            spinner.running.store(false, Ordering::Relaxed);
            if let Some(thread) = spinner.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn emit(&self, text: &str) {
        let mut err = std::io::stderr();
        let _ = writeln!(err, "{text}");
        let _ = err.flush();
    }
}

impl Default for Phases {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Phases {
    fn drop(&mut self) {
        self.end();
    }
}
