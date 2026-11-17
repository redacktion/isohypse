use isohypse::policy::{parse_stored, EnforceRequest, Enforcement, OpPattern, Policy, PolicyError, Refusal, Stored};
use serde_json::{json, Value};

type Apply = fn(&Enforcement, EnforceRequest) -> Result<Enforcement, Refusal>;
type Permits = fn(&Policy, &str) -> bool;
type Narrows = fn(&Policy, &Policy) -> bool;
type Contains = fn(&OpPattern, &OpPattern) -> bool;
type Restore = fn(Option<&str>) -> Enforcement;

fn real_apply(state: &Enforcement, request: EnforceRequest) -> Result<Enforcement, Refusal> {
    state.apply(request)
}

fn real_permits(policy: &Policy, op: &str) -> bool {
    policy.permits(op).is_ok()
}

fn real_narrows(current: &Policy, next: &Policy) -> bool {
    current.narrows_to(next).is_ok()
}

fn real_contains(outer: &OpPattern, inner: &OpPattern) -> bool {
    outer.contains(inner)
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

fn wire_lists(policy: &Policy) -> Spec {
    let wire = policy.to_wire();
    let list = |key: &str| wire[key].as_array().expect("array").iter().map(|v| v.as_str().expect("string").to_string()).collect();
    Spec { allow: list("allowed"), deny: list("denied") }
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

fn vocab_specs(patterns: &[String]) -> Vec<Spec> {
    let options: Vec<Option<&String>> = std::iter::once(None).chain(patterns.iter().map(Some)).collect();
    let mut specs = Vec::new();
    for allow in &options {
        for deny in &options {
            specs.push(Spec { allow: allow.iter().map(|p| p.to_string()).collect(), deny: deny.iter().map(|p| p.to_string()).collect() });
        }
    }
    specs
}

fn small_vocab() -> (Vec<String>, Vec<String>) {
    let ops = vocab_ops(&["a", "ab"], 3);
    let patterns = vocab_patterns(&ops);
    (ops, patterns)
}

fn wide_vocab() -> (Vec<String>, Vec<String>) {
    let ops = vocab_ops(&["a", "b", "ab"], 3);
    let patterns = vocab_patterns(&ops);
    (ops, patterns)
}

fn reference_match(pattern: &str, bare_covers_subtree: bool, op: &str) -> bool {
    let (base, dotted) = match pattern.strip_suffix('.') {
        Some(base) => (base, true),
        None => (pattern, false),
    };
    let under = op.len() > base.len() + 1 && op.starts_with(base) && op.as_bytes()[base.len()] == b'.';
    if dotted {
        under
    } else if bare_covers_subtree {
        op == base || under
    } else {
        op == base
    }
}

fn reference_permits(spec: &Spec, op: &str) -> bool {
    let denied = spec.deny.iter().any(|d| reference_match(d, true, op));
    let allowed = spec.allow.is_empty() || spec.allow.iter().any(|a| reference_match(a, false, op));
    !denied && allowed
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

#[test]
fn deny_pattern_parse_invalid_table_returns_bad_pattern_for_every_input() {
    let inputs = ["", " ", "a b", "a\tb", "\n", " a", "a ", "a .", "context. read"];
    let expected: Vec<Option<PolicyError>> = inputs.iter().map(|i| Some(PolicyError::BadPattern(i.to_string()))).collect();
    let deny: Vec<Option<PolicyError>> = inputs.iter().map(|i| OpPattern::deny(i).err()).collect();
    assert_eq!(deny, expected);
}

#[test]
fn pattern_parse_valid_table_returns_ok_for_every_input() {
    let inputs = ["a", "a.", "context.read", "context.", "mutate.edit", "state.status", "x.y.z."];
    let actual: Vec<(bool, bool)> = inputs.iter().map(|i| (OpPattern::allow(i).is_ok(), OpPattern::deny(i).is_ok())).collect();
    assert_eq!(actual, vec![(true, true); 7]);
}

#[test]
fn pattern_as_str_table_returns_input_text() {
    let inputs = ["a", "a.", "context.read", "context.", "mutate.edit"];
    let actual: Vec<String> = inputs.iter().map(|i| OpPattern::allow(i).expect("valid").as_str().to_string()).collect();
    assert_eq!(actual, strings(&inputs));
}

fn pattern_match_violations(parse: fn(&str) -> Result<OpPattern, PolicyError>, bare_covers_subtree: bool, matches: fn(&OpPattern, &str) -> bool) -> Vec<String> {
    let (ops, patterns) = small_vocab();
    let mut bad = Vec::new();
    for pattern in &patterns {
        let parsed = parse(pattern).expect("valid");
        for op in &ops {
            let expected = reference_match(pattern, bare_covers_subtree, op);
            if matches(&parsed, op) != expected {
                bad.push(format!("pattern {pattern:?} op {op:?}: expected {expected}"));
            }
        }
    }
    bad
}

fn real_pattern_matches(pattern: &OpPattern, op: &str) -> bool {
    pattern.matches(op)
}

#[test]
fn allow_pattern_matches_all_vocab_pairs_agrees_with_reference() {
    assert_eq!(pattern_match_violations(OpPattern::allow, false, real_pattern_matches), Vec::<String>::new());
}

#[test]
fn deny_pattern_matches_all_vocab_pairs_agrees_with_reference() {
    assert_eq!(pattern_match_violations(OpPattern::deny, true, real_pattern_matches), Vec::<String>::new());
}

fn contains_violations(contains: Contains) -> Vec<String> {
    let (ops, patterns) = small_vocab();
    let kinds: [fn(&str) -> Result<OpPattern, PolicyError>; 2] = [OpPattern::allow, OpPattern::deny];
    let mut bad = Vec::new();
    for outer_kind in kinds {
        for inner_kind in kinds {
            for outer_raw in &patterns {
                let outer = outer_kind(outer_raw).expect("valid");
                for inner_raw in &patterns {
                    let inner = inner_kind(inner_raw).expect("valid");
                    if !contains(&outer, &inner) {
                        continue;
                    }
                    if let Some(op) = ops.iter().find(|op| inner.matches(op) && !outer.matches(op)) {
                        bad.push(format!("{outer_raw:?} contains {inner_raw:?} but misses op {op:?}"));
                    }
                }
            }
        }
    }
    bad
}

fn contains_reflexive_violations(contains: Contains) -> Vec<String> {
    let (_, patterns) = small_vocab();
    let mut bad = Vec::new();
    for raw in &patterns {
        for parse in [OpPattern::allow, OpPattern::deny] {
            let pattern = parse(raw).expect("valid");
            if !contains(&pattern, &pattern) {
                bad.push(format!("{raw:?} does not contain itself"));
            }
        }
    }
    bad
}

#[test]
fn pattern_contains_all_vocab_pairs_implies_op_subset() {
    assert_eq!(contains_violations(real_contains), Vec::<String>::new());
}

#[test]
fn pattern_contains_each_vocab_pattern_contains_itself() {
    assert_eq!(contains_reflexive_violations(real_contains), Vec::<String>::new());
}

#[test]
fn pattern_contains_table_returns_hand_written_results() {
    // (outer is deny, outer, inner is deny, inner, expected)
    let rows: [(bool, &str, bool, &str, bool); 14] = [
        (false, "context.", false, "context.read", true),
        (false, "context.read", false, "context.", false),
        (false, "context.", false, "context", false),
        (false, "context.", false, "context.read.", true),
        (false, "context.read.", false, "context.", false),
        (false, "context.", false, "contextual", false),
        (true, "mutate", true, "mutate.edit", true),
        (true, "mutate.edit", true, "mutate", false),
        (true, "mutate.", true, "mutate", false),
        (true, "mutate", true, "mutate.", true),
        (true, "mutate", true, "mutate", true),
        (false, "context.read", false, "context.read", true),
        (false, "context.read", false, "context.read.", false),
        (true, "mutate", true, "mutates", false),
    ];
    let parse = |deny: bool, raw: &str| if deny { OpPattern::deny(raw) } else { OpPattern::allow(raw) }.expect("valid");
    let actual: Vec<bool> = rows.iter().map(|&(od, o, id, i, _)| parse(od, o).contains(&parse(id, i))).collect();
    let expected: Vec<bool> = rows.iter().map(|r| r.4).collect();
    assert_eq!(actual, expected);
}

// ---------- policy ----------

fn permits_table(rows: &[(Policy, &str)]) -> Vec<(String, bool)> {
    rows.iter().map(|(p, op)| (op.to_string(), p.permits(op).is_ok())).collect()
}

#[test]
fn policy_permits_table_returns_hand_written_results() {
    let rows = [
        (policy(&["context.read"], &[]), "context.read", true),
        (policy(&["context.read"], &[]), "context.read.evil", false),
        (policy(&["context.read"], &[]), "context.find", false),
        (policy(&["context."], &[]), "context.read", true),
        (policy(&["context."], &[]), "context.read.evil", true),
        (policy(&["context."], &[]), "context", false),
        (policy(&["context."], &[]), "mutate.edit", false),
        (policy(&[], &["mutate"]), "context.read", true),
        (policy(&[], &["mutate"]), "mutate", false),
        (policy(&[], &["mutate"]), "mutate.edit", false),
        (policy(&[], &["mutate"]), "mutates", true),
        (policy(&[], &["mutate."]), "mutate", true),
        (policy(&[], &["mutate."]), "mutate.edit", false),
        (policy(&[], &[]), "anything.at.all", true),
        (policy(&["context.", "state.status"], &[]), "state.status", true),
        (policy(&["context.", "state.status"], &[]), "state.stop", false),
    ];
    let expected: Vec<(String, bool)> = [
        ("context.read", true),
        ("context.read.evil", false),
        ("context.find", false),
        ("context.read", true),
        ("context.read.evil", true),
        ("context", false),
        ("mutate.edit", false),
        ("context.read", true),
        ("mutate", false),
        ("mutate.edit", false),
        ("mutates", true),
        ("mutate", true),
        ("mutate.edit", false),
        ("anything.at.all", true),
        ("state.status", true),
        ("state.stop", false),
    ]
    .iter()
    .map(|(op, e)| (op.to_string(), *e))
    .collect();
    let pairs: Vec<(Policy, &str)> = rows.iter().map(|(p, op, _)| (p.clone(), *op)).collect();
    assert_eq!(permits_table(&pairs), expected);
}

#[test]
fn policy_permits_deny_beats_allow_table_returns_denied_for_overlap() {
    let cases = [
        (policy(&["context."], &["context.read"]), "context.read"),
        (policy(&["context.read"], &["context.read"]), "context.read"),
        (policy(&["context.read"], &["context"]), "context.read"),
        (policy(&["context.", "mutate.edit"], &["mutate"]), "mutate.edit"),
        (policy(&["context.read.evil"], &["context."]), "context.read.evil"),
    ];
    let actual: Vec<Result<(), Refusal>> = cases.iter().map(|(p, op)| p.permits(op)).collect();
    let expected: Vec<Result<(), Refusal>> = ["context.read", "context.read", "context.read", "mutate.edit", "context.read.evil"]
        .iter()
        .map(|op| Err(Refusal::Denied(op.to_string())))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn policy_permits_deny_beats_allow_second_allowed_op_still_passes() {
    assert_eq!(policy(&["context."], &["context.read"]).permits("context.find"), Ok(()));
}

#[test]
fn policy_permits_op_outside_allowlist_returns_not_allowed_with_op() {
    assert_eq!(policy(&["context.read"], &[]).permits("mutate.edit"), Err(Refusal::NotAllowed("mutate.edit".to_string())));
}

#[test]
fn policy_from_lists_empty_allow_empty_deny_permits_every_probe() {
    let p = policy(&[], &[]);
    assert_eq!(PROBES.iter().filter(|op| p.permits(op).is_err()).count(), 0);
}

#[test]
fn policy_from_lists_bad_allow_pattern_returns_bad_pattern() {
    assert_eq!(Policy::from_lists(&strings(&["context.read", "bad op"]), &[]), Err(PolicyError::BadPattern("bad op".to_string())));
}

#[test]
fn policy_from_lists_bad_deny_pattern_returns_bad_pattern() {
    assert_eq!(Policy::from_lists(&[], &strings(&["mutate", ""])), Err(PolicyError::BadPattern(String::new())));
}

fn permits_violations(permits: Permits) -> Vec<String> {
    let (ops, patterns) = small_vocab();
    let mut bad = Vec::new();
    for spec in vocab_specs(&patterns) {
        let p = policy_of(&spec);
        for op in &ops {
            let expected = reference_permits(&spec, op);
            if permits(&p, op) != expected {
                bad.push(format!("allow {:?} deny {:?} op {op:?}: expected {expected}", spec.allow, spec.deny));
            }
        }
    }
    bad
}

fn permits_random_violations(permits: Permits) -> Vec<String> {
    let (ops, patterns) = wide_vocab();
    let mut rng = Lcg(7);
    let mut bad = Vec::new();
    for _ in 0..20_000 {
        let spec = random_spec(&mut rng, &patterns);
        let p = policy_of(&spec);
        for op in &ops {
            let expected = reference_permits(&spec, op);
            if permits(&p, op) != expected {
                bad.push(format!("allow {:?} deny {:?} op {op:?}: expected {expected}", spec.allow, spec.deny));
            }
        }
    }
    bad
}

#[test]
fn policy_permits_all_single_entry_vocab_policies_agrees_with_reference() {
    assert_eq!(permits_violations(real_permits), Vec::<String>::new());
}

#[test]
fn policy_permits_random_multi_entry_policies_agrees_with_reference() {
    assert_eq!(permits_random_violations(real_permits), Vec::<String>::new());
}

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
fn policy_builtin_unknown_names_return_none() {
    let names = ["", "Full", "admin", "readonly ", "READONLY", "none"];
    let actual: Vec<bool> = names.iter().map(|n| Policy::builtin(n).is_none()).collect();
    assert_eq!(actual, vec![true; 6]);
}

#[test]
fn policy_builtin_full_permits_every_probe() {
    assert_eq!(PROBES.iter().filter(|op| builtin("full").permits(op).is_err()).count(), 0);
}

#[test]
fn policy_unrestricted_permits_every_probe() {
    assert_eq!(PROBES.iter().filter(|op| Policy::unrestricted().permits(op).is_err()).count(), 0);
}

#[test]
fn policy_lockdown_permits_only_state_status_among_probes() {
    let permitted_ops: Vec<&str> = PROBES.iter().copied().filter(|op| Policy::lockdown().permits(op).is_ok()).collect();
    assert_eq!(permitted_ops, vec!["state.status"]);
}

#[test]
fn policy_allowed_count_table_returns_hand_counted_sizes() {
    let policies = [builtin("readonly"), builtin("review"), builtin("edit"), builtin("observe"), builtin("full"), Policy::lockdown(), Policy::unrestricted()];
    let actual: Vec<Option<usize>> = policies.iter().map(Policy::allowed_count).collect();
    assert_eq!(actual, vec![Some(6), Some(9), Some(13), Some(8), None, Some(1), None]);
}

#[test]
fn policy_to_wire_table_returns_hand_written_json() {
    let actual: Vec<Value> = vec![
        policy(&["context.read"], &["mutate"]).to_wire(),
        Policy::unrestricted().to_wire(),
        policy(&["context.", "state.status"], &[]).to_wire(),
        Policy::lockdown().to_wire(),
    ];
    let expected = vec![
        json!({"allowed": ["context.read"], "denied": ["mutate"]}),
        json!({"allowed": [], "denied": []}),
        json!({"allowed": ["context.", "state.status"], "denied": []}),
        json!({"allowed": ["state.status"], "denied": []}),
    ];
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

#[test]
fn policy_wire_roundtrip_through_stored_text_restores_active_enforcement() {
    let policies = [builtin("review"), policy(&["context."], &["context.read"]), policy(&[], &["mutate"])];
    let actual: Vec<Enforcement> = policies.iter().map(|p| real_restore(Some(&p.to_wire().to_string()))).collect();
    let expected: Vec<Enforcement> = policies.iter().cloned().map(Enforcement::Active).collect();
    assert_eq!(actual, expected);
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
fn policy_narrows_to_profile_chain_table_returns_hand_written_labels() {
    let rows: [(Policy, Policy); 13] = [
        (builtin("edit"), builtin("review")),
        (builtin("review"), builtin("readonly")),
        (builtin("edit"), builtin("readonly")),
        (builtin("observe"), builtin("readonly")),
        (builtin("readonly"), builtin("review")),
        (builtin("review"), builtin("edit")),
        (builtin("review"), builtin("observe")),
        (builtin("readonly"), builtin("full")),
        (builtin("full"), builtin("edit")),
        (builtin("full"), builtin("full")),
        (builtin("review"), builtin("review")),
        (builtin("edit"), Policy::lockdown()),
        (Policy::lockdown(), builtin("readonly")),
    ];
    let actual: Vec<&str> = rows.iter().map(|(a, b)| label_of(a.narrows_to(b))).collect();
    assert_eq!(
        actual,
        vec!["ok", "ok", "ok", "ok", "adds_op", "adds_op", "adds_op", "widens_to_all", "ok", "ok", "ok", "ok", "adds_op"]
    );
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

#[test]
fn policy_narrows_to_any_to_any_with_added_deny_is_ok() {
    assert_eq!(policy(&[], &[]).narrows_to(&policy(&[], &["mutate.edit"])), Ok(()));
}

fn spec_permitted(policy: &Policy, ops: &[String]) -> Vec<bool> {
    ops.iter().map(|op| policy.permits(op).is_ok()).collect()
}

fn cached_soundness_exhaustive() -> (Vec<String>, usize) {
    static CELL: std::sync::OnceLock<(Vec<String>, usize)> = std::sync::OnceLock::new();
    CELL.get_or_init(|| narrows_soundness_exhaustive(real_narrows, 3)).clone()
}

fn narrows_soundness_exhaustive(narrows: Narrows, depth: usize) -> (Vec<String>, usize) {
    let ops = vocab_ops(&["a", "ab"], depth);
    let patterns = vocab_patterns(&ops);
    let specs = vocab_specs(&patterns);
    let policies: Vec<Policy> = specs.iter().map(policy_of).collect();
    let matrix: Vec<Vec<bool>> = policies.iter().map(|p| spec_permitted(p, &ops)).collect();
    let mut bad = Vec::new();
    let mut accepted = 0usize;
    for (i, current) in policies.iter().enumerate() {
        for (j, next) in policies.iter().enumerate() {
            if !narrows(current, next) {
                continue;
            }
            accepted += 1;
            if let Some(k) = (0..ops.len()).find(|&k| matrix[j][k] && !matrix[i][k]) {
                bad.push(format!("{:?} -> {:?} accepted but op {:?} newly permitted", specs[i], specs[j], ops[k]));
            }
        }
    }
    (bad, accepted)
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
fn policy_narrows_to_all_single_entry_vocab_pairs_never_permits_new_op() {
    assert_eq!(cached_soundness_exhaustive().0, Vec::<String>::new());
}

#[test]
fn policy_narrows_to_random_multi_entry_pairs_never_permits_new_op() {
    assert_eq!(narrows_soundness_random(real_narrows).0, Vec::<String>::new());
}

#[test]
fn policy_narrows_to_all_single_entry_vocab_pairs_accepts_many_pairs() {
    assert!(cached_soundness_exhaustive().1 > 5000);
}

#[test]
fn policy_narrows_to_random_multi_entry_pairs_accepts_many_pairs() {
    assert!(narrows_soundness_random(real_narrows).1 > 1000);
}

#[test]
fn policy_narrows_to_each_vocab_policy_to_itself_is_ok() {
    let (_, patterns) = small_vocab();
    let refused: Vec<String> = vocab_specs(&patterns)
        .iter()
        .filter(|s| {
            let p = policy_of(s);
            p.narrows_to(&p).is_err()
        })
        .map(|s| format!("{s:?}"))
        .collect();
    assert_eq!(refused, Vec::<String>::new());
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
    DerivedNarrowingAccepted,
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

fn cached_random() -> Found {
    static CELL: std::sync::OnceLock<Found> = std::sync::OnceLock::new();
    CELL.get_or_init(|| random_found(real_apply, 100)).clone()
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

fn random_found(apply: Apply, seeds: u64) -> Found {
    let patterns = pool_patterns();
    let pool = request_policies();
    let mut found = Vec::new();
    for seed in 0..seeds {
        let mut rng = Lcg(seed);
        let mut state = Enforcement::Off;
        let mut history: Vec<EnforceRequest> = Vec::new();
        for _ in 0..300 {
            let derived_from = match &state {
                Enforcement::Active(p) if rng.below(10) < 6 => Some(p.clone()),
                _ => None,
            };
            let (request, derived) = match derived_from {
                Some(current) => {
                    let spec = derived_spec(&mut rng, &wire_lists(&current), &patterns);
                    (EnforceRequest::Activate(policy_of(&spec)), true)
                }
                None if rng.below(10) == 0 => (EnforceRequest::Disable, false),
                None if rng.below(2) == 0 => (EnforceRequest::Activate(rng.pick(&pool).clone()), false),
                None => (EnforceRequest::Activate(policy_of(&random_spec(&mut rng, &patterns))), false),
            };
            history.push(request.clone());
            let trace = || {
                let last: Vec<String> = history[history.len().saturating_sub(3)..]
                    .iter()
                    .map(|r| match r {
                        EnforceRequest::Disable => "Disable".to_string(),
                        EnforceRequest::Activate(p) => format!("Activate{}", p.to_wire()),
                    })
                    .collect();
                format!("seed {seed} step {}: last requests {last:?}", history.len())
            };
            let result = apply(&state, request.clone());
            check_step(&state, &request, &result, &trace, &mut found);
            if derived && result.is_err() {
                found.push((Inv::DerivedNarrowingAccepted, format!("{}: narrower request refused with {:?}", trace(), result)));
            }
            if let Ok(next) = result {
                state = next;
            }
        }
    }
    found
}

fn pool_patterns() -> Vec<String> {
    let mut all: Vec<String> = PROBES.iter().filter(|p| !p.ends_with('.')).map(|p| p.to_string()).collect();
    let dotted: Vec<String> = all.iter().map(|p| format!("{p}.")).collect();
    all.extend(dotted);
    all
}

fn only(found: Found, inv: Inv) -> Vec<String> {
    found.into_iter().filter(|(i, _)| *i == inv).map(|(_, why)| why).collect()
}

#[test]
fn enforcement_default_is_off() {
    assert_eq!(Enforcement::default(), Enforcement::Off);
}

#[test]
fn enforcement_off_check_every_probe_passes() {
    assert_eq!(PROBES.iter().filter(|op| Enforcement::Off.check(op).is_err()).count(), 0);
}

#[test]
fn enforcement_active_check_table_matches_policy_permits() {
    let state = Enforcement::Active(policy(&["context."], &["context.read"]));
    let actual: Vec<Result<(), Refusal>> = ["context.find", "context.read", "mutate.edit"].iter().map(|op| state.check(op)).collect();
    let expected = vec![
        Ok(()),
        Err(Refusal::Denied("context.read".to_string())),
        Err(Refusal::NotAllowed("mutate.edit".to_string())),
    ];
    assert_eq!(actual, expected);
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
fn enforcement_apply_refusal_variants_table_carry_expected_payloads() {
    let current = Enforcement::Active(policy(&["context.read"], &["mutate"]));
    let attempts = [
        EnforceRequest::Disable,
        EnforceRequest::Activate(Policy::unrestricted()),
        EnforceRequest::Activate(policy(&["context.read", "verify.check"], &["mutate"])),
        EnforceRequest::Activate(policy(&["context.read"], &[])),
    ];
    let actual: Vec<Result<Enforcement, Refusal>> = attempts.into_iter().map(|r| current.apply(r)).collect();
    let expected = vec![
        Err(Refusal::CannotDisable),
        Err(Refusal::WidensToAll),
        Err(Refusal::AddsOp("verify.check".to_string())),
        Err(Refusal::DropsDenied("mutate".to_string())),
    ];
    assert_eq!(actual, expected);
}

#[test]
fn enforcement_apply_refused_request_after_chain_leaves_original_policy_in_force() {
    let edit = Enforcement::Active(builtin("edit"));
    let narrowed = edit.apply(EnforceRequest::Activate(builtin("review"))).expect("narrowing accepted");
    let refused = narrowed.apply(EnforceRequest::Activate(builtin("edit")));
    assert_eq!((refused.is_err(), narrowed.check("mutate.edit").is_err(), narrowed.check("verify.check").is_ok()), (true, true, true));
}

#[test]
fn enforcement_exhaustive_sequences_active_never_becomes_off() {
    assert_eq!(only(cached_exhaustive(), Inv::StaysActive), Vec::<String>::new());
}

#[test]
fn enforcement_random_sequences_active_never_becomes_off() {
    assert_eq!(only(cached_random(), Inv::StaysActive), Vec::<String>::new());
}

#[test]
fn enforcement_exhaustive_sequences_accepted_successor_never_permits_more() {
    assert_eq!(only(cached_exhaustive(), Inv::NeverGrows), Vec::<String>::new());
}

#[test]
fn enforcement_random_sequences_accepted_successor_never_permits_more() {
    assert_eq!(only(cached_random(), Inv::NeverGrows), Vec::<String>::new());
}

#[test]
fn enforcement_exhaustive_sequences_accepted_activate_installs_requested_policy() {
    assert_eq!(only(cached_exhaustive(), Inv::OkEqualsRequest), Vec::<String>::new());
}

#[test]
fn enforcement_random_sequences_accepted_activate_installs_requested_policy() {
    assert_eq!(only(cached_random(), Inv::OkEqualsRequest), Vec::<String>::new());
}

#[test]
fn enforcement_exhaustive_sequences_disable_while_active_is_cannot_disable() {
    assert_eq!(only(cached_exhaustive(), Inv::DisableRefused), Vec::<String>::new());
}

#[test]
fn enforcement_random_sequences_disable_while_active_is_cannot_disable() {
    assert_eq!(only(cached_random(), Inv::DisableRefused), Vec::<String>::new());
}

#[test]
fn enforcement_exhaustive_sequences_off_disable_stays_off() {
    assert_eq!(only(cached_exhaustive(), Inv::OffDisableIsOff), Vec::<String>::new());
}

#[test]
fn enforcement_exhaustive_sequences_off_state_checks_pass() {
    assert_eq!(only(cached_exhaustive(), Inv::OffChecksPass), Vec::<String>::new());
}

#[test]
fn enforcement_random_sequences_narrower_derived_request_is_accepted() {
    assert_eq!(only(cached_random(), Inv::DerivedNarrowingAccepted), Vec::<String>::new());
}

fn reachable_accepts(apply: Apply) -> usize {
    let requests = request_alphabet();
    let mut accepted = 0usize;
    for_each_sequence(requests.len(), 2, |seq| {
        let mut state = Enforcement::Off;
        for &i in seq {
            if let Ok(next) = apply(&state, requests[i].clone()) {
                accepted += 1;
                state = next;
            }
        }
    });
    accepted
}

#[test]
fn enforcement_exhaustive_sequences_accept_many_transitions_so_checks_are_not_vacuous() {
    assert!(reachable_accepts(real_apply) > 150);
}

#[test]
fn enforcement_exhaustive_sequences_count_matches_alphabet_power_sum() {
    let mut visited = 0usize;
    for_each_sequence(request_alphabet().len(), SEQUENCE_DEPTH, |_| visited += 1);
    let n = request_alphabet().len();
    assert_eq!(visited, (1..=SEQUENCE_DEPTH).map(|k| n.pow(k as u32)).sum::<usize>());
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

const MISSING_KEY_TEXTS: [&str; 3] = ["{}", "{\"denied\":[\"mutate\"]}", "{\"allowed\":[\"context.read\"]}"];

fn some(texts: &[&'static str]) -> Vec<Option<&'static str>> {
    texts.iter().map(|t| Some(*t)).collect()
}

fn never_off_violations(restore: Restore) -> Vec<String> {
    let pieces = [
        "{", "}", "[", "]", ",", ":", "\"allowed\"", "\"denied\"", "\"context.read\"", "\"x y\"", "null", "1", "true", " ", "\"a.\"", "\"\"", "{\"allowed\":[]}",
    ];
    let mut rng = Lcg(99);
    let mut bad = Vec::new();
    for _ in 0..20_000 {
        let text: String = (0..rng.below(14)).map(|_| *rng.pick(&pieces)).collect();
        if restore(Some(&text)) == Enforcement::Off {
            bad.push(format!("{text:?} restored to Off"));
        }
    }
    for text in CORRUPT_TEXTS.iter().chain(WRONG_TYPE_TEXTS.iter()).chain(MISSING_KEY_TEXTS.iter()) {
        if restore(Some(text)) == Enforcement::Off {
            bad.push(format!("{text:?} restored to Off"));
        }
    }
    bad
}

fn truncation_violations(restore: Restore) -> Vec<String> {
    let policies = [builtin("edit"), policy(&["context."], &["context.read"]), policy(&[], &["mutate"]), Policy::lockdown()];
    let mut bad = Vec::new();
    for p in &policies {
        let text = p.to_wire().to_string();
        for cut in 0..text.len() {
            if restore(Some(&text[..cut])) != lockdown_state() {
                bad.push(format!("prefix {:?} did not restore to lockdown", &text[..cut]));
            }
        }
    }
    bad
}

fn byte_flip_violations(restore: Restore) -> Vec<String> {
    let base = builtin("edit").to_wire().to_string().into_bytes();
    let mut rng = Lcg(5);
    let mut bad = Vec::new();
    for _ in 0..20_000 {
        let mut bytes = base.clone();
        let at = rng.below(bytes.len());
        bytes[at] = b' ' + rng.below(95) as u8;
        let text = String::from_utf8(bytes).expect("ascii");
        if restore(Some(&text)) == Enforcement::Off {
            bad.push(format!("{text:?} restored to Off"));
        }
    }
    bad
}

#[test]
fn parse_stored_absent_file_returns_absent() {
    assert_eq!(parse_stored(None), Stored::Absent);
}

#[test]
fn parse_stored_valid_policy_text_table_returns_valid_policy() {
    let texts = [
        "{\"allowed\":[\"context.read\"],\"denied\":[\"mutate\"]}",
        "{\"allowed\":[],\"denied\":[]}",
        "{\"allowed\":[\"context.\",\"state.status\"],\"denied\":[]}",
        "{\"allowed\":[],\"denied\":[\"mutate.edit\"]}",
    ];
    let actual: Vec<Stored> = texts.iter().map(|t| parse_stored(Some(t))).collect();
    let expected = vec![
        Stored::Valid(policy(&["context.read"], &["mutate"])),
        Stored::Valid(Policy::unrestricted()),
        Stored::Valid(policy(&["context.", "state.status"], &[])),
        Stored::Valid(policy(&[], &["mutate.edit"])),
    ];
    assert_eq!(actual, expected);
}

#[test]
fn stored_into_enforcement_table_maps_each_variant() {
    let actual = vec![Stored::Absent.into_enforcement(), Stored::Valid(builtin("review")).into_enforcement(), Stored::Corrupt.into_enforcement()];
    let expected = vec![Enforcement::Off, Enforcement::Active(builtin("review")), lockdown_state()];
    assert_eq!(actual, expected);
}

#[test]
fn restore_absent_file_returns_off() {
    assert_eq!(real_restore(None), Enforcement::Off);
}

#[test]
fn restore_corrupt_text_table_returns_lockdown() {
    assert_eq!(restore_table(real_restore, &some(&CORRUPT_TEXTS)), vec![lockdown_state(); CORRUPT_TEXTS.len()]);
}

#[test]
fn restore_wrong_type_field_table_returns_lockdown() {
    assert_eq!(restore_table(real_restore, &some(&WRONG_TYPE_TEXTS)), vec![lockdown_state(); WRONG_TYPE_TEXTS.len()]);
}

#[test]
fn restore_missing_key_table_returns_lockdown() {
    assert_eq!(restore_table(real_restore, &some(&MISSING_KEY_TEXTS)), vec![lockdown_state(); MISSING_KEY_TEXTS.len()]);
}

#[test]
fn restore_wrong_type_allowed_field_table_refuses_mutate_edit() {
    let refused: Vec<bool> = WRONG_TYPE_TEXTS[..5].iter().map(|t| real_restore(Some(t)).check("mutate.edit").is_err()).collect();
    assert_eq!(refused, vec![true; 5]);
}

#[test]
fn restore_random_json_fragments_never_returns_off() {
    assert_eq!(never_off_violations(real_restore), Vec::<String>::new());
}

#[test]
fn restore_every_truncated_valid_text_returns_lockdown() {
    assert_eq!(truncation_violations(real_restore), Vec::<String>::new());
}

#[test]
fn restore_single_byte_flips_of_valid_text_never_return_off() {
    assert_eq!(byte_flip_violations(real_restore), Vec::<String>::new());
}

#[test]
fn restore_lockdown_state_permits_only_state_status_among_probes() {
    let permitted_ops: Vec<&str> = PROBES.iter().copied().filter(|op| lockdown_state().check(op).is_ok()).collect();
    assert_eq!(permitted_ops, vec!["state.status"]);
}

#[test]
fn restore_lockdown_then_unrestricted_request_is_refused_as_widening() {
    assert_eq!(lockdown_state().apply(EnforceRequest::Activate(Policy::unrestricted())), Err(Refusal::WidensToAll));
}

#[test]
fn restore_lockdown_then_disable_request_is_refused() {
    assert_eq!(lockdown_state().apply(EnforceRequest::Disable), Err(Refusal::CannotDisable));
}
