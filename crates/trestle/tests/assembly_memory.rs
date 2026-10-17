//! ADR-014: a project assembled from runtime data (here a `trestle.toml`)
//! owns everything it registers. Assembling it again and again, and dropping
//! the registry and router each time, does not grow the heap.
//!
//! Its own test binary, because it installs a counting global allocator and
//! must not share the process with other tests' allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use serde_json::json;
use trestle::intent::{Ctx, Input, Row};
use trestle::{Problem, Result};

/// Live heap bytes.
static LIVE: AtomicIsize = AtomicIsize::new(0);

struct Counting;

// SAFETY: every call is forwarded unchanged to the system allocator; the
// wrapper only counts sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const NOTES: &str = include_str!("../examples/notes_intent/trestle.toml");

async fn pin(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.update("notes.note", input.id()?, json!({ "pinned": true }))
        .await
}

async fn tidy(_ctx: Ctx, note: Row) -> Result<Row, Problem> {
    Ok(note)
}

/// Assembles the notes intent, builds its router, and drops both.
fn assemble_once() {
    let registry = trestle::project()
        .named("notes", "1")
        .intent_str("trestle.toml", NOTES)
        .handler("notes::pin", pin)
        .hook("notes::tidy", tidy)
        .assemble()
        .expect("the notes intent assembles");
    assert!(registry.operation("notes.note.create").is_some());
    let router = registry.router();
    drop(router);
    drop(registry);
}

#[test]
fn repeated_assembly_from_an_intent_does_not_grow_the_heap() {
    // Warm up once-per-process state (lazy statics, tracing call sites).
    for _ in 0..3 {
        assemble_once();
    }
    let before = LIVE.load(Ordering::Relaxed);
    const ROUNDS: isize = 50;
    for _ in 0..ROUNDS {
        assemble_once();
    }
    let grown = LIVE.load(Ordering::Relaxed) - before;
    eprintln!("heap growth over {ROUNDS} assemblies: {grown} bytes");
    // Allow a little noise from lazily initialised runtime state; a leak of
    // the intent, its contracts or IDs costs kilobytes per round.
    assert!(
        grown < 4 * 1024,
        "the heap grew by {grown} bytes over {ROUNDS} assemblies ({} per assembly)",
        grown / ROUNDS
    );
}
