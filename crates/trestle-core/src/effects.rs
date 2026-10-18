//! Declared operation effects (ADR-018): what an operation reads, writes,
//! enqueues and calls out to.
//!
//! Declaring is optional. `inspect operation` and `inspect effects` show
//! what was declared, and Trestle checks what it can:
//!
//! * **at assembly**, `reads` and `writes` name installed models, `emits`
//!   names registered operations or job kinds, and a read declares no writes
//!   or emits;
//! * **at run time**, a write that declared its effects may enqueue only
//!   the jobs listed in `emits` ([`crate::jobs::enqueue`] refuses others).
//!
//! Separately, every `Mode::Read` operation runs in a read-only
//! transaction, declared or not, so a read cannot write. `writes` within a
//! write and `egress` are recorded, not enforced: Trestle sees neither
//! which tables SQL touches nor the network.

use std::sync::Arc;

use serde::Serialize;

/// Effects as a typed operation declares them: a `const`
/// (`const EFFECTS: Option<EffectsInfo> = Some(EffectsInfo::new().writes(&["notes.note"]));`).
#[derive(Debug, Clone, Copy, Default)]
pub struct EffectsInfo {
    pub reads: &'static [&'static str],
    pub writes: &'static [&'static str],
    pub emits: &'static [&'static str],
    pub egress: &'static [&'static str],
}

impl EffectsInfo {
    pub const fn new() -> Self {
        Self {
            reads: &[],
            writes: &[],
            emits: &[],
            egress: &[],
        }
    }

    /// Models it reads (`"lessons.lesson"`).
    pub const fn reads(mut self, models: &'static [&'static str]) -> Self {
        self.reads = models;
        self
    }

    /// Models it inserts, updates or deletes; it may read them too.
    pub const fn writes(mut self, models: &'static [&'static str]) -> Self {
        self.writes = models;
        self
    }

    /// Jobs it enqueues: operation IDs (`NewJob::operation`) or handler
    /// kinds (`NewJob::handler`).
    pub const fn emits(mut self, jobs: &'static [&'static str]) -> Self {
        self.emits = jobs;
        self
    }

    /// External systems it calls (`"smtp"`, `"api.example.com"`).
    pub const fn egress(mut self, systems: &'static [&'static str]) -> Self {
        self.egress = systems;
        self
    }
}

/// The same declaration, owned: what the registry keeps, and what
/// operations assembled at run time declare on their
/// [`OperationDescriptor`](crate::operation::OperationDescriptor).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Effects {
    pub reads: Vec<Arc<str>>,
    pub writes: Vec<Arc<str>>,
    pub emits: Vec<Arc<str>>,
    pub egress: Vec<Arc<str>>,
}

impl Effects {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reads(mut self, model: impl Into<Arc<str>>) -> Self {
        add(&mut self.reads, model.into());
        self
    }

    pub fn writes(mut self, model: impl Into<Arc<str>>) -> Self {
        add(&mut self.writes, model.into());
        self
    }

    pub fn emits(mut self, job: impl Into<Arc<str>>) -> Self {
        add(&mut self.emits, job.into());
        self
    }

    pub fn egress(mut self, system: impl Into<Arc<str>>) -> Self {
        add(&mut self.egress, system.into());
        self
    }

    /// Whether it reads or writes `model`.
    pub fn touches(&self, model: &str) -> bool {
        self.reads.iter().chain(&self.writes).any(|m| &**m == model)
    }
}

/// Sorted and unique, so equal declarations compare and print equal.
fn add(list: &mut Vec<Arc<str>>, item: Arc<str>) {
    if let Err(at) = list.binary_search(&item) {
        list.insert(at, item);
    }
}

impl From<EffectsInfo> for Effects {
    fn from(info: EffectsInfo) -> Self {
        let mut effects = Effects::new();
        for m in info.reads {
            effects = effects.reads(*m);
        }
        for m in info.writes {
            effects = effects.writes(*m);
        }
        for j in info.emits {
            effects = effects.emits(*j);
        }
        for e in info.egress {
            effects = effects.egress(*e);
        }
        effects
    }
}

/// The jobs the running write may enqueue, set by the executor around
/// `run`. Typed operations borrow their `const`; nothing is allocated.
#[derive(Clone)]
pub(crate) enum Emits {
    Static(&'static str, &'static [&'static str]),
    Owned(Arc<str>, Arc<Effects>),
}

impl Emits {
    fn operation(&self) -> &str {
        match self {
            Emits::Static(op, _) => op,
            Emits::Owned(op, _) => op,
        }
    }

    fn allows(&self, job: &str) -> bool {
        match self {
            Emits::Static(_, emits) => emits.contains(&job),
            Emits::Owned(_, effects) => effects.emits.iter().any(|e| &**e == job),
        }
    }
}

tokio::task_local! {
    static EMITS: Option<Emits>;
}

/// Runs `fut` (an operation's `run`) with its declared emits in scope.
pub(crate) async fn scoped<F: Future>(emits: Option<Emits>, fut: F) -> F::Output {
    EMITS.scope(emits, fut).await
}

/// Called by `jobs::enqueue`. Outside an executor scope, or for an
/// operation that declared nothing, every job is allowed.
pub(crate) fn check_emit(job: &str) -> Result<(), crate::Problem> {
    let refused = EMITS
        .try_with(|scope| match scope {
            Some(emits) if !emits.allows(job) => Some(emits.operation().to_owned()),
            _ => None,
        })
        .ok()
        .flatten();
    match refused {
        Some(operation) => {
            tracing::error!(
                operation = %operation,
                job,
                "operation enqueued a job it does not declare in its effects"
            );
            Err(crate::Problem::internal())
        }
        None => Ok(()),
    }
}
