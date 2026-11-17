use std::sync::OnceLock;

use isohypse::hint::{Hint, RequestKind, AFTER};
use RequestKind::{Other, SoloRead};

type Observe = fn(Hint, RequestKind) -> (Hint, bool);

fn real(state: Hint, kind: RequestKind) -> (Hint, bool) {
    state.observe(kind)
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const EXHAUSTIVE_DEPTH: usize = 14;
const RANDOM_SEEDS: u64 = 2000;
const RANDOM_LEN: usize = 300;

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

fn random_sequences() -> &'static Vec<Vec<RequestKind>> {
    static CELL: OnceLock<Vec<Vec<RequestKind>>> = OnceLock::new();
    CELL.get_or_init(|| {
        let mut all = Vec::new();
        for seed in 0..RANDOM_SEEDS {
            let mut rng = Lcg(seed);
            let solo_tenths = seed % 10;
            all.push((0..RANDOM_LEN).map(|_| if rng.below(10) < solo_tenths { SoloRead } else { Other }).collect());
        }
        let mut rng = Lcg(42);
        all.push((0..100_000).map(|_| if rng.below(100) < 97 { SoloRead } else { Other }).collect());
        all
    })
}

struct Step {
    before: Hint,
    kind: RequestKind,
    after: Hint,
    emitted: bool,
}

fn walk(observe: Observe, sequence: &[RequestKind]) -> Vec<Step> {
    let mut state = Hint::start();
    sequence
        .iter()
        .map(|&kind| {
            let (after, emitted) = observe(state, kind);
            let step = Step { before: state, kind, after, emitted };
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

fn emit_once_violations(sequences: &[Vec<RequestKind>], observe: Observe) -> Vec<String> {
    run_checks(sequences, observe, |_, steps| {
        let count = steps.iter().filter(|s| s.emitted).count();
        (count > 1).then(|| format!("{count} emissions"))
    })
}

fn emit_on_other_violations(sequences: &[Vec<RequestKind>], observe: Observe) -> Vec<String> {
    run_checks(sequences, observe, |_, steps| steps.iter().position(|s| s.emitted && s.kind == Other).map(|i| format!("emitted on Other at {i}")))
}

fn counting_bound_violations(sequences: &[Vec<RequestKind>], observe: Observe) -> Vec<String> {
    run_checks(sequences, observe, |_, steps| {
        steps.iter().position(|s| matches!(s.after, Hint::Counting(n) if n >= AFTER)).map(|i| format!("Counting reached AFTER at {i}"))
    })
}

fn run_length_violations(sequences: &[Vec<RequestKind>], observe: Observe) -> Vec<String> {
    run_checks(sequences, observe, |_, steps| {
        let mut run = 0u8;
        for (i, s) in steps.iter().enumerate() {
            run = if s.kind == SoloRead { run.saturating_add(1) } else { 0 };
            if let Hint::Counting(n) = s.after {
                if n != run {
                    return Some(format!("step {i}: Counting({n}) but trailing solo run is {run}"));
                }
            }
        }
        None
    })
}

fn sent_iff_emitted_violations(sequences: &[Vec<RequestKind>], observe: Observe) -> Vec<String> {
    run_checks(sequences, observe, |_, steps| {
        let mut emitted_so_far = false;
        for (i, s) in steps.iter().enumerate() {
            emitted_so_far |= s.emitted;
            if (s.after == Hint::Sent) != emitted_so_far {
                return Some(format!("step {i}: state {:?} but emitted_so_far={emitted_so_far}", s.after));
            }
        }
        None
    })
}

fn absorbing_violations(sequences: &[Vec<RequestKind>], observe: Observe) -> Vec<String> {
    run_checks(sequences, observe, |_, steps| {
        steps
            .iter()
            .position(|s| s.before == Hint::Sent && (s.after != Hint::Sent || s.emitted))
            .map(|i| format!("step {i}: Sent left or emitted again"))
    })
}

fn quiet_after_emit_violations(sequences: &[Vec<RequestKind>], observe: Observe) -> Vec<String> {
    run_checks(sequences, observe, |_, steps| {
        let first = steps.iter().position(|s| s.emitted)?;
        steps[first + 1..].iter().position(|s| s.emitted).map(|i| format!("second emission at {}", first + 1 + i))
    })
}

fn emitted_len_counts(observe: Observe, lengths: std::ops::RangeInclusive<usize>) -> Vec<usize> {
    lengths
        .map(|len| exhaustive_sequences().iter().filter(|s| s.len() == len).filter(|s| walk(observe, s).iter().any(|st| st.emitted)).count())
        .collect()
}

#[test]
fn hint_after_constant_is_three() {
    assert_eq!(AFTER, 3);
}

#[test]
fn hint_start_state_is_counting_zero() {
    assert_eq!(Hint::start(), Hint::Counting(0));
}

#[test]
fn request_kind_of_table_returns_solo_read_only_for_one_read_only_step() {
    let rows = [(0usize, true), (0, false), (1, true), (1, false), (2, true), (2, false), (3, true), (usize::MAX, true)];
    let expected = vec![Other, Other, SoloRead, Other, Other, Other, Other, Other];
    let actual: Vec<RequestKind> = rows.iter().map(|&(steps, read_only)| RequestKind::of(steps, read_only)).collect();
    assert_eq!(actual, expected);
}

#[test]
fn hint_observe_scripted_sequences_table_emit_at_hand_written_positions() {
    let scripts = ["RRR", "RRRR", "ORRR", "RRORRR", "RRORR", "ROROROR", "RRRRRR", "RRRORRR", "R", "RR", "O", "OOORRO"];
    let expected: Vec<Vec<bool>> = vec![
        vec![false, false, true],
        vec![false, false, true, false],
        vec![false, false, false, true],
        vec![false, false, false, false, false, true],
        vec![false; 5],
        vec![false; 7],
        vec![false, false, true, false, false, false],
        vec![false, false, true, false, false, false, false],
        vec![false],
        vec![false, false],
        vec![false],
        vec![false; 6],
    ];
    let actual: Vec<Vec<bool>> = scripts
        .iter()
        .map(|script| {
            let kinds: Vec<RequestKind> = script.chars().map(|c| if c == 'R' { SoloRead } else { Other }).collect();
            walk(real, &kinds).iter().map(|s| s.emitted).collect()
        })
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn hint_observe_counting_states_table_returns_hand_written_transitions() {
    let rows = [
        (Hint::Counting(0), SoloRead),
        (Hint::Counting(1), SoloRead),
        (Hint::Counting(2), SoloRead),
        (Hint::Counting(0), Other),
        (Hint::Counting(1), Other),
        (Hint::Counting(2), Other),
    ];
    let expected = vec![
        (Hint::Counting(1), false),
        (Hint::Counting(2), false),
        (Hint::Sent, true),
        (Hint::Counting(0), false),
        (Hint::Counting(0), false),
        (Hint::Counting(0), false),
    ];
    let actual: Vec<(Hint, bool)> = rows.iter().map(|&(state, kind)| state.observe(kind)).collect();
    assert_eq!(actual, expected);
}

#[test]
fn hint_observe_sent_state_table_stays_sent_without_emission() {
    let expected = vec![(Hint::Sent, false), (Hint::Sent, false)];
    let actual: Vec<(Hint, bool)> = [SoloRead, Other].iter().map(|&kind| Hint::Sent.observe(kind)).collect();
    assert_eq!(actual, expected);
}

#[test]
fn hint_exhaustive_sequences_count_emitting_sequences_by_length_matches_hand_count() {
    assert_eq!(emitted_len_counts(real, 1..=6), vec![0, 0, 1, 3, 8, 20]);
}

#[test]
fn hint_exhaustive_sequences_total_count_matches_two_pow_sum() {
    assert_eq!(exhaustive_sequences().len(), (1usize << (EXHAUSTIVE_DEPTH + 1)) - 2);
}

#[test]
fn hint_exhaustive_sequences_emit_positions_match_first_three_solo_window_oracle() {
    assert_eq!(oracle_violations(exhaustive_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_emit_positions_match_first_three_solo_window_oracle() {
    assert_eq!(oracle_violations(random_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_exhaustive_sequences_emit_at_most_once() {
    assert_eq!(emit_once_violations(exhaustive_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_emit_at_most_once() {
    assert_eq!(emit_once_violations(random_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_exhaustive_sequences_never_emit_on_other_request() {
    assert_eq!(emit_on_other_violations(exhaustive_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_never_emit_on_other_request() {
    assert_eq!(emit_on_other_violations(random_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_exhaustive_sequences_counting_stays_below_after() {
    assert_eq!(counting_bound_violations(exhaustive_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_counting_stays_below_after() {
    assert_eq!(counting_bound_violations(random_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_exhaustive_sequences_counting_equals_trailing_solo_run() {
    assert_eq!(run_length_violations(exhaustive_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_counting_equals_trailing_solo_run() {
    assert_eq!(run_length_violations(random_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_exhaustive_sequences_sent_exactly_after_emission() {
    assert_eq!(sent_iff_emitted_violations(exhaustive_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_sent_exactly_after_emission() {
    assert_eq!(sent_iff_emitted_violations(random_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_exhaustive_sequences_sent_is_absorbing_and_silent() {
    assert_eq!(absorbing_violations(exhaustive_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_sent_is_absorbing_and_silent() {
    assert_eq!(absorbing_violations(random_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_exhaustive_sequences_no_emission_after_first() {
    assert_eq!(quiet_after_emit_violations(exhaustive_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_no_emission_after_first() {
    assert_eq!(quiet_after_emit_violations(random_sequences(), real), Vec::<String>::new());
}

#[test]
fn hint_random_sequences_reach_emission_in_most_seeds_so_checks_are_not_vacuous() {
    let emitting = random_sequences().iter().filter(|s| walk(real, s).iter().any(|st| st.emitted)).count();
    assert!(emitting > 1000, "only {emitting} random sequences emit");
}
