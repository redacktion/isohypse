//! Workspace enforcement as types. A `Policy` says which ops pass. `Enforcement`
//! is the one-way state machine around it: once active it only narrows and
//! never turns off. Everything here is pure. Storage and locking live in the
//! daemon.

use std::fmt;

use serde_json::{json, Value};

/// An op pattern. `context.read` is one op. `context.` is every op under
/// `context`. In a deny list a bare name also covers the ops under it, so
/// `mutate` denies `mutate` and `mutate.edit`. An allow list never widens a
/// bare name: `context.read` does not allow `context.read.evil`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpPattern {
    raw: String,
    base: String,
    exact: bool,
    subtree: bool,
}

impl OpPattern {
    fn parse(raw: &str, bare_covers_subtree: bool) -> Result<OpPattern, PolicyError> {
        let bad = || PolicyError::BadPattern(raw.to_string());
        if raw.is_empty() || raw.chars().any(char::is_whitespace) {
            return Err(bad());
        }
        let (base, dotted) = match raw.strip_suffix('.') {
            Some(base) if !base.is_empty() => (base, true),
            Some(_) => return Err(bad()),
            None => (raw, false),
        };
        Ok(OpPattern { raw: raw.to_string(), base: base.to_string(), exact: !dotted, subtree: dotted || bare_covers_subtree })
    }

    pub fn allow(raw: &str) -> Result<OpPattern, PolicyError> {
        OpPattern::parse(raw, false)
    }

    pub fn deny(raw: &str) -> Result<OpPattern, PolicyError> {
        OpPattern::parse(raw, true)
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn matches(&self, op: &str) -> bool {
        (self.exact && op == self.base) || (self.subtree && op.strip_prefix(&self.base).is_some_and(|rest| rest.starts_with('.') && rest.len() > 1))
    }

    /// True when every op `other` matches, `self` matches too.
    pub fn contains(&self, other: &OpPattern) -> bool {
        let covers_subtree = self.subtree && (other.base == self.base || other.base.strip_prefix(&self.base).is_some_and(|rest| rest.starts_with('.')));
        let exact_ok = !other.exact || self.matches(&other.base);
        let subtree_ok = !other.subtree || covers_subtree;
        exact_ok && subtree_ok
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyError {
    BadPattern(String),
    BadWire(String),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyError::BadPattern(raw) => write!(f, "invalid op pattern {raw:?}: must be non-empty without whitespace"),
            PolicyError::BadWire(key) => write!(f, "stored policy field {key:?} must be an array of strings"),
        }
    }
}

/// Why an op or a policy change was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Denied(String),
    NotAllowed(String),
    WidensToAll,
    AddsOp(String),
    DropsDenied(String),
    CannotDisable,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Denied(op) => write!(f, "enforce: op {op:?} is denied by workspace policy"),
            Refusal::NotAllowed(op) => write!(f, "enforce: op {op:?} is not in the workspace allowlist"),
            Refusal::WidensToAll => write!(f, "enforce: cannot widen permissions to all ops"),
            Refusal::AddsOp(op) => write!(f, "enforce: cannot add op {op:?} that was not in the current allowlist"),
            Refusal::DropsDenied(op) => write!(f, "enforce: cannot drop denied op {op:?}"),
            Refusal::CannotDisable => write!(f, "enforce: cannot disable workspace enforcement; only state.stop (password-gated) can clear it"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Allow {
    Any,
    /// Never empty.
    Only(Vec<OpPattern>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    allow: Allow,
    deny: Vec<OpPattern>,
}

const READ_OPS: [&str; 6] = ["context.read", "context.find", "context.explore", "context.log", "context.prompt", "state.status"];
const CHECK_OPS: [&str; 3] = ["verify.check", "verify.diagnose", "verify.build"];
const MUTATE_OPS: [&str; 4] = ["mutate.edit", "mutate.create", "mutate.delete", "mutate.undo"];
const OBSERVE_OPS: [&str; 2] = ["session.request", "session.requests"];

impl Policy {
    /// `allowed` empty means every op the deny list does not block.
    pub fn from_lists(allowed: &[String], denied: &[String]) -> Result<Policy, PolicyError> {
        let allow = match allowed.iter().map(|p| OpPattern::allow(p)).collect::<Result<Vec<_>, _>>()? {
            only if only.is_empty() => Allow::Any,
            only => Allow::Only(only),
        };
        let deny = denied.iter().map(|p| OpPattern::deny(p)).collect::<Result<Vec<_>, _>>()?;
        Ok(Policy { allow, deny })
    }

    fn only(ops: &[&str]) -> Policy {
        let patterns = ops.iter().filter_map(|op| OpPattern::allow(op).ok()).collect();
        Policy { allow: Allow::Only(patterns), deny: Vec::new() }
    }

    pub fn unrestricted() -> Policy {
        Policy { allow: Allow::Any, deny: Vec::new() }
    }

    /// The smallest policy: status only. Used when a stored policy cannot be read.
    pub fn lockdown() -> Policy {
        Policy::only(&["state.status"])
    }

    pub fn builtin(name: &str) -> Option<Policy> {
        fn joined<'a>(groups: &[&[&'a str]]) -> Vec<&'a str> {
            groups.iter().flat_map(|g| g.iter().copied()).collect()
        }
        Some(match name {
            "readonly" => Policy::only(&READ_OPS),
            "review" => Policy::only(&joined(&[&READ_OPS, &CHECK_OPS])),
            "edit" => Policy::only(&joined(&[&READ_OPS, &CHECK_OPS, &MUTATE_OPS])),
            "observe" => Policy::only(&joined(&[&READ_OPS, &OBSERVE_OPS])),
            "full" => Policy::unrestricted(),
            _ => return None,
        })
    }

    pub fn permits(&self, op: &str) -> Result<(), Refusal> {
        if self.deny.iter().any(|d| d.matches(op)) {
            return Err(Refusal::Denied(op.to_string()));
        }
        match &self.allow {
            Allow::Any => Ok(()),
            Allow::Only(patterns) if patterns.iter().any(|p| p.matches(op)) => Ok(()),
            Allow::Only(_) => Err(Refusal::NotAllowed(op.to_string())),
        }
    }

    /// Ok when `next` permits nothing this policy refuses.
    pub fn narrows_to(&self, next: &Policy) -> Result<(), Refusal> {
        if let Allow::Only(current) = &self.allow {
            let Allow::Only(proposed) = &next.allow else { return Err(Refusal::WidensToAll) };
            if let Some(extra) = proposed.iter().find(|q| !current.iter().any(|p| p.contains(q))) {
                return Err(Refusal::AddsOp(extra.as_str().to_string()));
            }
        }
        match self.deny.iter().find(|d| !next.deny.iter().any(|n| n.contains(d))) {
            Some(dropped) => Err(Refusal::DropsDenied(dropped.as_str().to_string())),
            None => Ok(()),
        }
    }

    pub fn allowed_count(&self) -> Option<usize> {
        match &self.allow {
            Allow::Any => None,
            Allow::Only(patterns) => Some(patterns.len()),
        }
    }

    pub fn to_wire(&self) -> Value {
        let names = |patterns: &[OpPattern]| patterns.iter().map(|p| p.as_str().to_string()).collect::<Vec<_>>();
        let allowed = match &self.allow {
            Allow::Any => Vec::new(),
            Allow::Only(patterns) => names(patterns),
        };
        json!({"allowed": allowed, "denied": names(&self.deny)})
    }

    /// Both keys must be present and hold arrays of strings. Anything else is an error.
    pub fn from_wire(value: &Value) -> Result<Policy, PolicyError> {
        let list = |key: &str| -> Result<Vec<String>, PolicyError> {
            let items = value.get(key).and_then(Value::as_array).ok_or_else(|| PolicyError::BadWire(key.to_string()))?;
            items.iter().map(|item| item.as_str().map(str::to_string).ok_or_else(|| PolicyError::BadWire(key.to_string()))).collect()
        };
        Policy::from_lists(&list("allowed")?, &list("denied")?)
    }
}

/// A request to change enforcement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnforceRequest {
    Activate(Policy),
    Disable,
}

/// Enforcement state. `Active` never returns to `Off`, and a successor policy
/// never permits more than its predecessor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Enforcement {
    #[default]
    Off,
    Active(Policy),
}

impl Enforcement {
    pub fn check(&self, op: &str) -> Result<(), Refusal> {
        match self {
            Enforcement::Off => Ok(()),
            Enforcement::Active(policy) => policy.permits(op),
        }
    }

    /// The next state, or a refusal that leaves `self` in force.
    pub fn apply(&self, request: EnforceRequest) -> Result<Enforcement, Refusal> {
        match (self, request) {
            (Enforcement::Off, EnforceRequest::Disable) => Ok(Enforcement::Off),
            (Enforcement::Off, EnforceRequest::Activate(next)) => Ok(Enforcement::Active(next)),
            (Enforcement::Active(_), EnforceRequest::Disable) => Err(Refusal::CannotDisable),
            (Enforcement::Active(current), EnforceRequest::Activate(next)) => {
                current.narrows_to(&next)?;
                Ok(Enforcement::Active(next))
            }
        }
    }
}

/// What a stored enforcement file holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stored {
    Absent,
    Valid(Policy),
    Corrupt,
}

/// `text` is None when the file does not exist.
pub fn parse_stored(text: Option<&str>) -> Stored {
    let Some(text) = text else { return Stored::Absent };
    match serde_json::from_str::<Value>(text).ok().filter(Value::is_object).map(|v| Policy::from_wire(&v)) {
        Some(Ok(policy)) => Stored::Valid(policy),
        _ => Stored::Corrupt,
    }
}

impl Stored {
    /// Corrupt storage locks the workspace down. It never removes enforcement.
    pub fn into_enforcement(self) -> Enforcement {
        match self {
            Stored::Absent => Enforcement::Off,
            Stored::Valid(policy) => Enforcement::Active(policy),
            Stored::Corrupt => Enforcement::Active(Policy::lockdown()),
        }
    }
}
