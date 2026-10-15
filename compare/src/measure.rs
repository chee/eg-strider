use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

/// The system allocator, counting the bytes in use.
struct Counting;

static IN_USE: AtomicUsize = AtomicUsize::new(0);
static MOST: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Counts `bytes` more in use, raising the high-water mark if it is passed.
fn took(bytes: usize) {
    MOST.fetch_max(IN_USE.fetch_add(bytes, Relaxed) + bytes, Relaxed);
}

// SAFETY: every request is passed to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        took(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        took(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        IN_USE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        IN_USE.fetch_sub(layout.size(), Relaxed);
        took(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// What running something did to memory use, in bytes.
#[derive(Clone, Copy)]
pub(crate) struct Memory {
    /// The most in use at once, beyond what was in use at the start.
    pub(crate) peak: usize,
    /// How much more is in use afterwards, while the result is held.
    pub(crate) kept: usize,
}

/// Runs `f` and reports what it did to the memory in use.
pub(crate) fn memory_of<T>(f: impl FnOnce() -> T) -> (T, Memory) {
    let before = IN_USE.load(Relaxed);
    MOST.store(before, Relaxed);
    let result = f();
    (result, Memory { peak: MOST.load(Relaxed) - before, kept: IN_USE.load(Relaxed).saturating_sub(before) })
}

/// How long each trace's merges run before any is timed.
const WARM_UP: std::time::Duration = std::time::Duration::from_millis(300);

/// What a system took on the two tasks.
#[derive(Clone, Copy)]
pub(crate) struct Measured {
    pub(crate) merge_ms: f64,
    /// Memory the history's ops take, in the form the system gets them.
    pub(crate) ops: usize,
    /// While merging: peak memory, ops included, and what the merged document holds.
    pub(crate) merging: Memory,
    pub(crate) open_ms: f64,
    /// While reopening: peak memory, and what the reopened document holds.
    pub(crate) opening: Memory,
    /// Size of the saved document.
    pub(crate) saved: usize,
    /// Whether both tasks ended with diamond-types' text.
    pub(crate) same: bool,
}

/// One run of merging: the milliseconds taken, then for eg-strider and diamond-types those spent
/// appending, then for eg-strider those one more edit at the end takes. And whether the text was
/// diamond-types'.
pub(crate) type MergeRun = ([f64; 3], bool);

/// A library's two tasks on one trace, each run once per call, so that every trace takes its
/// turn.
pub(crate) struct Turns<'a> {
    pub(crate) merge: Box<dyn FnMut() -> MergeRun + 'a>,
    pub(crate) open: Box<dyn FnMut() -> f64 + 'a>,
}

/// Checks a library on the two tasks and measures their memory, then sets them up to be timed
/// in turns with its tasks on the other traces. `ops` puts a history's ops in memory, untimed but
/// counted. A document's text is read only to check it, outside the times and memory.
pub(crate) fn prepare<'a, O: 'a, D: 'a>(
    expected: &str,
    ops: impl Fn() -> O + 'a,
    merge: impl Fn(O) -> D + 'a,
    text: impl Fn(&D) -> String,
    save: impl Fn(&mut D) -> Vec<u8>,
    open: impl Fn(&[u8]) -> D + 'a,
) -> (Measured, Turns<'a>) {
    let mut document = merge(ops());
    let saved = save(&mut document);
    let same = text(&document) == expected && text(&open(&saved)) == expected;
    drop(document);
    let held_by_ops = memory_of(&ops).1.kept;
    let merging = memory_of(|| merge(ops())).1;
    let opening = memory_of(|| open(&saved)).1;
    let measured = Measured { merge_ms: f64::NAN, ops: held_by_ops, merging, open_ms: f64::NAN, opening, saved: saved.len(), same };
    let turns = Turns {
        merge: Box::new(move || {
            let input = ops();
            let t = Instant::now();
            let document = merge(input);
            let ms = since(t);
            drop(document);
            ([ms, f64::NAN, f64::NAN], true)
        }),
        open: Box::new(move || {
            let t = Instant::now();
            let document = open(&saved);
            let ms = since(t);
            drop(document);
            ms
        }),
    };
    (measured, turns)
}

/// Runs one library's task on every trace in turns, round after round, until each has `reps`
/// runs that count. The library's code stays warm, as in an application that merges often,
/// but no run follows one on the same trace, whose branches the processor would have learnt:
/// that makes some libraries twice as fast. The first runs of a short task are up to twice as
/// slow as the rest, until the processor has sped up, so only rounds begun once some time has
/// gone by count.
pub(crate) fn in_turns<T>(reps: u32, tasks: &mut [&mut dyn FnMut() -> T]) -> Vec<Vec<T>> {
    let mut runs: Vec<Vec<T>> = tasks.iter().map(|_| Vec::with_capacity(reps as usize)).collect();
    let started = Instant::now();
    while runs.first().is_some_and(|runs| runs.len() < reps as usize) {
        let counts = started.elapsed() >= WARM_UP;
        for (task, runs) in tasks.iter_mut().zip(&mut runs) {
            let run = task();
            if counts {
                runs.push(run);
            }
        }
    }
    runs
}

/// Returns the milliseconds since `start`.
pub(crate) fn since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

/// Returns the median: the middle time, or the mean of the middle two.
pub(crate) fn median(mut times: Vec<f64>) -> f64 {
    times.sort_by(f64::total_cmp);
    let (below, above) = (times.len().saturating_sub(1) / 2, times.len() / 2);
    times.get(below).map_or(f64::NAN, |low| (low + times[above]) / 2.0)
}

/// Returns the median milliseconds of `reps` runs of `f`, each on what an untimed `setup`
/// makes, and the last result.
pub(crate) fn median_of<S, T>(reps: u32, mut setup: impl FnMut() -> S, mut f: impl FnMut(S) -> T) -> (f64, T) {
    let mut times = Vec::with_capacity(reps as usize);
    loop {
        let input = setup();
        let t = Instant::now();
        let result = f(input);
        times.push(since(t));
        if times.len() >= reps as usize {
            return (median(times), result);
        }
    }
}
