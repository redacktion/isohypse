//! The one-time batching hint, as a state machine. A session counts consecutive
//! single-step read-only requests and emits the hint once, on the third.

pub const AFTER: u8 = 3;

pub const TEXT: &str = "hint: 3 single-step reads in a row. Send independent reads, finds and explores as one steps array: {\"steps\":[...]}.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    SoloRead,
    Other,
}

impl RequestKind {
    pub fn of(steps: usize, read_only: bool) -> RequestKind {
        if steps == 1 && read_only { RequestKind::SoloRead } else { RequestKind::Other }
    }
}

/// `Counting(n)` always has `n < AFTER`. `Sent` is absorbing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    Counting(u8),
    Sent,
}

impl Hint {
    pub fn start() -> Hint {
        Hint::Counting(0)
    }

    /// The next state and whether to emit the hint now.
    pub fn observe(self, kind: RequestKind) -> (Hint, bool) {
        match (self, kind) {
            (Hint::Sent, _) => (Hint::Sent, false),
            (Hint::Counting(_), RequestKind::Other) => (Hint::Counting(0), false),
            (Hint::Counting(n), RequestKind::SoloRead) if n + 1 >= AFTER => (Hint::Sent, true),
            (Hint::Counting(n), RequestKind::SoloRead) => (Hint::Counting(n + 1), false),
        }
    }
}
