use isohypse::policy::{parse_stored, EnforceRequest, Enforcement, OpPattern, Policy, PolicyError, Refusal};

type Apply = fn(&Enforcement, EnforceRequest) -> Result<Enforcement, Refusal>;
type Narrows = fn(&Policy, &Policy) -> bool;
type Restore = fn(Option<&str>) -> Enforcement;

fn real_apply(state: &Enforcement, request: EnforceRequest) -> Result<Enforcement, Refusal> {
    state.apply(request)
}

fn real_narrows(current: &Policy, next: &Policy) -> bool {
    current.narrows_to(next).is_ok()
}

fn real_restore(text: Option<&str>) -> Enforcement {
    parse_stored(text).into_enforcement()
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn policy(allow: &[&str], deny: &[&str]) -> Policy {
    Policy::from_lists(&strings(allow), &strings(deny)).expect("valid patterns")
}

fn policy_of(spec: &Spec) -> Policy {
    Policy::from_lists(&spec.allow, &spec.deny).expect("valid patterns")
}

fn builtin(name: &str) -> Policy {
    Policy::builtin(name).expect("known profile")
}

#[derive(Clone, Debug)]
struct Spec {
    allow: Vec<String>,
    deny: Vec<String>,
}

const PROBES: [&str; 36] = [
    "context.read", "context.find", "context.explore", "context.log", "context.prompt", "state.status", "state.stop", "verify.check", "verify.diagnose",
    "verify.build", "mutate.edit", "mutate.create", "mutate.delete", "mutate.undo", "session.request", "session.requests", "session.enforce", "session.open",
    "context", "context.read.evil", "context.readx", "contextual", "contextual.read", "mutate", "mutate.edit.x", "mutates", "verify", "session",
    "state", "state.status.x", "x", "a.b.c", "context.zz", "mutate.edit.", "verify.build.deep", "other.op",
];

fn permitted(state: &Enforcement) -> Vec<bool> {
    PROBES.iter().map(|op| state.check(op).is_ok()).collect()
}

fn vocab_ops(segments: &[&str], depth: usize) -> Vec<String> {
    let mut all = Vec::new();
    let mut layer = vec![String::new()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for base in &layer {
            for segment in segments {
                next.push(if base.is_empty() { segment.to_string() } else { format!("{base}.{segment}") });
            }
        }
        all.extend(next.iter().cloned());
        layer = next;
    }
    all
}

fn vocab_patterns(ops: &[String]) -> Vec<String> {
    ops.iter().flat_map(|op| [op.clone(), format!("{op}.")]).collect()
}

fn wide_vocab() -> (Vec<String>, Vec<String>) {
    let ops = vocab_ops(&["a", "b", "ab"], 3);
    let patterns = vocab_patterns(&ops);
    (ops, patterns)
}

fn random_spec(rng: &mut Lcg, patterns: &[String]) -> Spec {
    let allow = (0..rng.below(4)).map(|_| rng.pick(patterns).clone()).collect();
    let deny = (0..rng.below(3)).map(|_| rng.pick(patterns).clone()).collect();
    Spec { allow, deny }
}

fn derived_spec(rng: &mut Lcg, current: &Spec, patterns: &[String]) -> Spec {
    let mut next = current.clone();
    match rng.below(5) {
        0 => next.deny.push(rng.pick(patterns).clone()),
        1 if next.allow.len() > 1 => {
            next.allow.remove(rng.below(next.allow.len()));
        }
        2 if !next.allow.is_empty() => {
            let i = rng.below(next.allow.len());
            let child = format!("{}zz", next.allow[i]);
            if next.allow[i].ends_with('.') {
                next.allow[i] = child;
            }
        }
        3 => {}
        _ => next.deny.push(rng.pick(patterns).clone()),
    }
    next
}

// ---------- patterns ----------

fn match_table(rows: &[(&str, &str, bool)], parse: fn(&str) -> Result<OpPattern, PolicyError>) -> Vec<(String, String, bool)> {
    rows.iter().map(|&(p, op, _)| (p.to_string(), op.to_string(), parse(p).expect("valid").matches(op))).collect()
}

fn expected_rows(rows: &[(&str, &str, bool)]) -> Vec<(String, String, bool)> {
    rows.iter().map(|&(p, op, e)| (p.to_string(), op.to_string(), e)).collect()
}

const ALLOW_MATCH_ROWS: [(&str, &str, bool); 16] = [
    ("context.read", "context.read", true),
    ("context.read", "context.read.evil", false),
    ("context.read", "context", false),
    ("context.read", "context.find", false),
    ("context.read", "context.readx", false),
    ("context.", "context.read", true),
    ("context.", "context.read.evil", true),
    ("context.", "context", false),
    ("context.", "contextual.read", false),
    ("context.", "mutate.edit", false),
    ("mutate", "mutate", true),
    ("mutate", "mutate.edit", false),
    ("mutate.edit.", "mutate.edit.x", true),
    ("mutate.edit.", "mutate.edit", false),
    ("mutate.edit.", "mutate.create", false),
    ("state.status", "state.status", true),
];

const DENY_MATCH_ROWS: [(&str, &str, bool); 14] = [
    ("mutate", "mutate", true),
    ("mutate", "mutate.edit", true),
    ("mutate", "mutate.edit.x", true),
    ("mutate", "mutates", false),
    ("mutate", "mutat", false),
    ("mutate.", "mutate", false),
    ("mutate.", "mutate.edit", true),
    ("mutate.", "mutate.edit.x", true),
    ("mutate.edit", "mutate.edit", true),
    ("mutate.edit", "mutate.create", false),
    ("mutate.edit", "mutate.edit.x", true),
    ("mutate.edit", "mutate", false),
    ("mutate.edit.", "mutate.edit", false),
    ("context.read", "context.find", false),
];

#[test]
fn allow_pattern_match_table_returns_hand_written_results() {
    assert_eq!(match_table(&ALLOW_MATCH_ROWS, OpPattern::allow), expected_rows(&ALLOW_MATCH_ROWS));
}

#[test]
fn deny_pattern_match_table_returns_hand_written_results() {
    assert_eq!(match_table(&DENY_MATCH_ROWS, OpPattern::deny), expected_rows(&DENY_MATCH_ROWS));
}

#[test]
fn pattern_parse_invalid_table_returns_bad_pattern_for_every_input() {
    let inputs = ["", " ", "a b", "a\tb", "\n", " a", "a ", "a .", "context. read"];
    let expected: Vec<Option<PolicyError>> = inputs.iter().map(|i| Some(PolicyError::BadPattern(i.to_string()))).collect();
    let allow: Vec<Option<PolicyError>> = inputs.iter().map(|i| OpPattern::allow(i).err()).collect();
    assert_eq!(allow, expected);
}




// ---------- policy ----------

#[test]
fn policy_builtin_table_permits_exactly_the_readme_ops() {
    let ops = [
        "context.read", "context.find", "context.explore", "context.log", "context.prompt", "state.status", "verify.check", "verify.diagnose", "verify.build",
        "mutate.edit", "mutate.create", "mutate.delete", "mutate.undo", "session.request", "session.requests", "session.enforce", "state.stop",
    ];
    let readonly = [true, true, true, true, true, true, false, false, false, false, false, false, false, false, false, false, false];
    let review = [true, true, true, true, true, true, true, true, true, false, false, false, false, false, false, false, false];
    let edit = [true, true, true, true, true, true, true, true, true, true, true, true, true, false, false, false, false];
    let observe = [true, true, true, true, true, true, false, false, false, false, false, false, false, true, true, false, false];
    let full = [true; 17];
    let expected: Vec<(String, Vec<bool>)> = [("readonly", readonly), ("review", review), ("edit", edit), ("observe", observe), ("full", full)]
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_vec()))
        .collect();
    let actual: Vec<(String, Vec<bool>)> = ["readonly", "review", "edit", "observe", "full"]
        .iter()
        .map(|n| (n.to_string(), ops.iter().map(|op| builtin(n).permits(op).is_ok()).collect()))
        .collect();
    assert_eq!(actual, expected);
}







#[test]
fn policy_wire_roundtrip_table_restores_equal_policy() {
    let policies = vec![
        builtin("readonly"),
        builtin("review"),
        builtin("edit"),
        builtin("observe"),
        builtin("full"),
        Policy::lockdown(),
        policy(&["context."], &["context.read"]),
        policy(&[], &["mutate", "verify."]),
    ];
    let actual: Vec<bool> = policies.iter().map(|p| Policy::from_wire(&p.to_wire()).as_ref() == Ok(p)).collect();
    assert_eq!(actual, vec![true; 8]);
}


// ---------- narrows_to ----------

fn label_of(result: Result<(), Refusal>) -> &'static str {
    match result {
        Ok(()) => "ok",
        Err(Refusal::WidensToAll) => "widens_to_all",
        Err(Refusal::AddsOp(_)) => "adds_op",
        Err(Refusal::DropsDenied(_)) => "drops_denied",
        Err(Refusal::CannotDisable) => "cannot_disable",
        Err(Refusal::Denied(_)) => "denied",
        Err(Refusal::NotAllowed(_)) => "not_allowed",
    }
}


#[test]
fn policy_narrows_to_deny_table_returns_hand_written_labels() {
    let rows: [(Policy, Policy); 10] = [
        (policy(&[], &["mutate"]), policy(&[], &["mutate"])),
        (policy(&[], &["mutate"]), policy(&[], &["mutate", "verify"])),
        (policy(&[], &["mutate"]), policy(&[], &[])),
        (policy(&[], &["mutate"]), policy(&[], &["mutate."])),
        (policy(&[], &["mutate."]), policy(&[], &["mutate"])),
        (policy(&[], &["mutate.edit"]), policy(&[], &["mutate"])),
        (policy(&[], &["mutate"]), policy(&[], &["mutate.edit"])),
        (policy(&[], &[]), policy(&[], &["mutate"])),
        (policy(&["context."], &["context.read"]), policy(&["context.find"], &["context.read"])),
        (policy(&["context."], &["context.read"]), policy(&["context.find"], &[])),
    ];
    let actual: Vec<&str> = rows.iter().map(|(a, b)| label_of(a.narrows_to(b))).collect();
    assert_eq!(actual, vec!["ok", "ok", "drops_denied", "drops_denied", "ok", "ok", "drops_denied", "ok", "ok", "drops_denied"]);
}

#[test]
fn policy_narrows_to_allow_table_returns_hand_written_labels() {
    let rows: [(Policy, Policy); 9] = [
        (policy(&["context."], &[]), policy(&["context.read"], &[])),
        (policy(&["context."], &[]), policy(&["context.read."], &[])),
        (policy(&["context.read"], &[]), policy(&["context."], &[])),
        (policy(&["context.read"], &[]), policy(&["context.read."], &[])),
        (policy(&["context.read"], &[]), policy(&["context.read.evil"], &[])),
        (policy(&["context."], &[]), policy(&["contextual"], &[])),
        (policy(&["context.", "verify."], &[]), policy(&["verify.check", "context.find"], &[])),
        (policy(&["context.read"], &[]), policy(&[], &[])),
        (policy(&["context.read"], &[]), policy(&["context.read", "mutate.edit"], &[])),
    ];
    let actual: Vec<&str> = rows.iter().map(|(a, b)| label_of(a.narrows_to(b))).collect();
    assert_eq!(actual, vec!["ok", "ok", "adds_op", "adds_op", "adds_op", "adds_op", "ok", "widens_to_all", "adds_op"]);
}


fn narrows_soundness_random(narrows: Narrows) -> (Vec<String>, usize) {
    let (ops, patterns) = wide_vocab();
    let mut rng = Lcg(2024);
    let mut bad = Vec::new();
    let mut accepted = 0usize;
    for _ in 0..60_000 {
        let current = random_spec(&mut rng, &patterns);
        let next = if rng.below(2) == 0 { derived_spec(&mut rng, &current, &patterns) } else { random_spec(&mut rng, &patterns) };
        let (a, b) = (policy_of(&current), policy_of(&next));
        if !narrows(&a, &b) {
            continue;
        }
        accepted += 1;
        if let Some(op) = ops.iter().find(|op| b.permits(op).is_ok() && a.permits(op).is_err()) {
            bad.push(format!("{current:?} -> {next:?} accepted but op {op:?} newly permitted"));
        }
    }
    (bad, accepted)
}


#[test]
fn policy_narrows_to_random_multi_entry_pairs_never_permits_new_op() {
    assert_eq!(narrows_soundness_random(real_narrows).0, Vec::<String>::new());
}




// ---------- enforcement state machine ----------

fn request_policies() -> Vec<Policy> {
    vec![
        builtin("readonly"),
        builtin("review"),
        builtin("edit"),
        builtin("observe"),
        builtin("full"),
        Policy::lockdown(),
        policy(&["context."], &[]),
        policy(&["context.read"], &[]),
        policy(&[], &["mutate"]),
        policy(&["context."], &["context.read"]),
        policy(&[], &["mutate.edit"]),
        policy(&["mutate.edit."], &[]),
        policy(&["context.", "verify."], &["verify.build"]),
        policy(&[], &["mutate", "verify."]),
    ]
}

fn request_alphabet() -> Vec<EnforceRequest> {
    std::iter::once(EnforceRequest::Disable).chain(request_policies().into_iter().map(EnforceRequest::Activate)).collect()
}

const SEQUENCE_DEPTH: usize = 4;

fn for_each_sequence(alphabet: usize, depth: usize, mut visit: impl FnMut(&[usize])) {
    for len in 1..=depth {
        let mut digits = vec![0usize; len];
        loop {
            visit(&digits);
            let mut pos = len;
            let mut carried_out = true;
            while pos > 0 {
                pos -= 1;
                digits[pos] += 1;
                if digits[pos] < alphabet {
                    carried_out = false;
                    break;
                }
                digits[pos] = 0;
            }
            if carried_out {
                break;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Inv {
    StaysActive,
    NeverGrows,
    OkEqualsRequest,
    DisableRefused,
    OffDisableIsOff,
    OffChecksPass,
}

type Found = Vec<(Inv, String)>;

fn check_step(before: &Enforcement, request: &EnforceRequest, result: &Result<Enforcement, Refusal>, trace: &dyn Fn() -> String, found: &mut Found) {
    let mut flag = |inv: Inv, why: String| found.push((inv, format!("{}: {why}", trace())));
    if let Enforcement::Off = before {
        if PROBES.iter().any(|op| before.check(op).is_err()) {
            flag(Inv::OffChecksPass, "Off refused an op".to_string());
        }
    }
    if let (Enforcement::Active(_), Ok(Enforcement::Off)) = (before, result) {
        flag(Inv::StaysActive, "Active became Off".to_string());
    }
    match (before, request, result) {
        (Enforcement::Active(_), EnforceRequest::Disable, Ok(_)) => flag(Inv::DisableRefused, "Disable accepted while Active".to_string()),
        (Enforcement::Active(_), EnforceRequest::Disable, Err(r)) if *r != Refusal::CannotDisable => flag(Inv::DisableRefused, format!("Disable refused with {r:?}")),
        (Enforcement::Off, EnforceRequest::Disable, other) if *other != Ok(Enforcement::Off) => flag(Inv::OffDisableIsOff, format!("Off+Disable gave {other:?}")),
        _ => {}
    }
    if let (Enforcement::Active(_), EnforceRequest::Activate(p), Ok(after)) = (before, request, result) {
        if *after != Enforcement::Active(p.clone()) {
            flag(Inv::OkEqualsRequest, format!("accepted request {p:?} but state is {after:?}"));
        }
    }
    if let (Enforcement::Off, EnforceRequest::Activate(p), other) = (before, request, result) {
        if *other != Ok(Enforcement::Active(p.clone())) {
            flag(Inv::OkEqualsRequest, format!("Off+Activate({p:?}) gave {other:?}"));
        }
    }
    if let (Enforcement::Active(_), Ok(after)) = (before, result) {
        let (was, now) = (permitted(before), permitted(after));
        if let Some(i) = (0..PROBES.len()).find(|&i| now[i] && !was[i]) {
            flag(Inv::NeverGrows, format!("op {:?} newly permitted", PROBES[i]));
        }
    }
}

fn describe(requests: &[EnforceRequest], seq: &[usize]) -> String {
    seq.iter()
        .map(|&i| match &requests[i] {
            EnforceRequest::Disable => "Disable".to_string(),
            EnforceRequest::Activate(p) => format!("Activate{}", p.to_wire()),
        })
        .collect::<Vec<_>>()
        .join(" ; ")
}

fn cached_exhaustive() -> Found {
    static CELL: std::sync::OnceLock<Found> = std::sync::OnceLock::new();
    CELL.get_or_init(|| exhaustive_found(real_apply, SEQUENCE_DEPTH)).clone()
}

fn exhaustive_found(apply: Apply, depth: usize) -> Found {
    let requests = request_alphabet();
    let mut found = Vec::new();
    for_each_sequence(requests.len(), depth, |seq| {
        let mut state = Enforcement::Off;
        for (n, &i) in seq.iter().enumerate() {
            let result = apply(&state, requests[i].clone());
            check_step(&state, &requests[i], &result, &|| describe(&requests, &seq[..=n]), &mut found);
            if let Ok(next) = result {
                state = next;
            }
        }
    });
    found
}

fn only(found: Found, inv: Inv) -> Vec<String> {
    found.into_iter().filter(|(i, _)| *i == inv).map(|(_, why)| why).collect()
}




#[test]
fn enforcement_apply_single_step_table_returns_hand_written_results() {
    let review = Enforcement::Active(builtin("review"));
    let readonly = Enforcement::Active(builtin("readonly"));
    let cases: Vec<(Enforcement, EnforceRequest)> = vec![
        (Enforcement::Off, EnforceRequest::Disable),
        (Enforcement::Off, EnforceRequest::Activate(builtin("review"))),
        (Enforcement::Off, EnforceRequest::Activate(Policy::unrestricted())),
        (review.clone(), EnforceRequest::Disable),
        (review.clone(), EnforceRequest::Activate(builtin("readonly"))),
        (review.clone(), EnforceRequest::Activate(builtin("review"))),
        (review.clone(), EnforceRequest::Activate(builtin("edit"))),
        (review.clone(), EnforceRequest::Activate(Policy::unrestricted())),
        (readonly.clone(), EnforceRequest::Activate(builtin("review"))),
        (Enforcement::Active(Policy::unrestricted()), EnforceRequest::Activate(builtin("edit"))),
        (Enforcement::Active(Policy::unrestricted()), EnforceRequest::Disable),
    ];
    let actual: Vec<Result<Enforcement, &'static str>> = cases
        .into_iter()
        .map(|(state, request)| state.apply(request).map_err(|r| label_of(Err(r))))
        .collect();
    let expected: Vec<Result<Enforcement, &'static str>> = vec![
        Ok(Enforcement::Off),
        Ok(Enforcement::Active(builtin("review"))),
        Ok(Enforcement::Active(Policy::unrestricted())),
        Err("cannot_disable"),
        Ok(Enforcement::Active(builtin("readonly"))),
        Ok(Enforcement::Active(builtin("review"))),
        Err("adds_op"),
        Err("widens_to_all"),
        Err("adds_op"),
        Ok(Enforcement::Active(builtin("edit"))),
        Err("cannot_disable"),
    ];
    assert_eq!(actual, expected);
}





#[test]
fn enforcement_exhaustive_sequences_accepted_successor_never_permits_more() {
    assert_eq!(only(cached_exhaustive(), Inv::NeverGrows), Vec::<String>::new());
}









// ---------- stored policy ----------

fn lockdown_state() -> Enforcement {
    Enforcement::Active(Policy::lockdown())
}

fn restore_table(restore: Restore, inputs: &[Option<&str>]) -> Vec<Enforcement> {
    inputs.iter().map(|text| restore(*text)).collect()
}

const CORRUPT_TEXTS: [&str; 14] = [
    "",
    " ",
    "not json",
    "[]",
    "null",
    "42",
    "\"allowed\"",
    "true",
    "{",
    "{\"allowed\":[",
    "{\"allowed\":[\"has space\"]}",
    "{\"allowed\":[\"\"]}",
    "{\"denied\":[\"bad op\"]}",
    "{\"allowed\":[\"context.read\"],\"denied\":[\"\"]}",
];

const WRONG_TYPE_TEXTS: [&str; 8] = [
    "{\"allowed\":\"context.read\"}",
    "{\"allowed\":[1]}",
    "{\"allowed\":[null]}",
    "{\"allowed\":{\"a\":1}}",
    "{\"allowed\":null,\"denied\":[]}",
    "{\"allowed\":[\"context.read\"],\"denied\":\"mutate\"}",
    "{\"allowed\":[\"context.read\"],\"denied\":[5]}",
    "{\"allowed\":[\"context.read\",7]}",
];

fn some(texts: &[&'static str]) -> Vec<Option<&'static str>> {
    texts.iter().map(|t| Some(*t)).collect()
}

#[test]
fn restore_corrupt_text_table_returns_lockdown() {
    assert_eq!(restore_table(real_restore, &some(&CORRUPT_TEXTS)), vec![lockdown_state(); CORRUPT_TEXTS.len()]);
}

#[test]
fn restore_wrong_type_field_table_returns_lockdown() {
    assert_eq!(restore_table(real_restore, &some(&WRONG_TYPE_TEXTS)), vec![lockdown_state(); WRONG_TYPE_TEXTS.len()]);
}








