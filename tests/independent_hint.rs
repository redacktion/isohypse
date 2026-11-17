use std::sync::OnceLock;

use isohypse::hint::{Hint, RequestKind};
use RequestKind::{Other, SoloRead};

type Observe = fn(Hint, RequestKind) -> (Hint, bool);

fn real(state: Hint, kind: RequestKind) -> (Hint, bool) {
    state.observe(kind)
}

const EXHAUSTIVE_DEPTH: usize = 14;
fn exhaustive_sequences() -> &'static Vec<Vec<RequestKind>> {
    static CELL: OnceLock<Vec<Vec<RequestKind>>> = OnceLock::new();
    CELL.get_or_init(|| {
        let mut all = Vec::new();
        for len in 1..=EXHAUSTIVE_DEPTH {
            for mask in 0u32..(1 << len) {
                all.push((0..len).map(|i| if mask >> i & 1 == 1 { SoloRead } else { Other }).collect());
            }
        }
        all
    })
}

struct Step {
    emitted: bool,
}

fn walk(observe: Observe, sequence: &[RequestKind]) -> Vec<Step> {
    let mut state = Hint::start();
    sequence
        .iter()
        .map(|&kind| {
            let (after, emitted) = observe(state, kind);
            let step = Step { emitted };
            state = after;
            step
        })
        .collect()
}

fn label(sequence: &[RequestKind]) -> String {
    sequence.iter().map(|k| if *k == SoloRead { 'R' } else { 'O' }).collect::<String>().chars().take(40).collect()
}

fn expected_emits(sequence: &[RequestKind]) -> Vec<bool> {
    let first_window = (2..sequence.len()).find(|&i| sequence[i - 2..=i].iter().all(|k| *k == SoloRead));
    (0..sequence.len()).map(|i| Some(i) == first_window).collect()
}

fn run_checks(sequences: &[Vec<RequestKind>], observe: Observe, check: impl Fn(&[RequestKind], &[Step]) -> Option<String>) -> Vec<String> {
    sequences
        .iter()
        .filter_map(|sequence| check(sequence, &walk(observe, sequence)).map(|why| format!("{} ({}): {why}", label(sequence), sequence.len())))
        .collect()
}

fn oracle_violations(sequences: &[Vec<RequestKind>], observe: Observe) -> Vec<String> {
    run_checks(sequences, observe, |sequence, steps| {
        let actual: Vec<bool> = steps.iter().map(|s| s.emitted).collect();
        let expected = expected_emits(sequence);
        (actual != expected).then(|| format!("expected emits at {:?}, actual {:?}", positions(&expected), positions(&actual)))
    })
}

fn positions(flags: &[bool]) -> Vec<usize> {
    flags.iter().enumerate().filter(|(_, f)| **f).map(|(i, _)| i).collect()
}

#[test]
fn hint_exhaustive_sequences_emit_positions_match_first_three_solo_window_oracle() {
    assert_eq!(oracle_violations(exhaustive_sequences(), real), Vec::<String>::new());
}
















