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

/// Runs `f`, measuring its memory use.
pub(crate) fn memory_of<T>(f: impl FnOnce() -> T) -> (T, Memory) {
    let before = IN_USE.load(Relaxed);
    MOST.store(before, Relaxed);
    let result = f();
    (result, Memory { peak: MOST.load(Relaxed) - before, kept: IN_USE.load(Relaxed).saturating_sub(before) })
}

/// How long each trace's tasks run before any is timed.
const WARM_UP: std::time::Duration = std::time::Duration::from_millis(300);

/// A library's document, whatever form it takes.
pub(crate) trait Document {
    /// The document's text, read out as a string.
    fn text(&mut self) -> String;
}

/// A library's time and memory on the two tasks.
#[derive(Clone, Copy)]
pub(crate) struct Measured {
    pub(crate) merge_ms: f64,
    /// Size of the encoded history that merging starts from.
    pub(crate) history: usize,
    /// While merging: peak memory, the encoded history included, and what the merged document
    /// holds.
    pub(crate) merging: Memory,
    pub(crate) open_ms: f64,
    /// While reopening: peak memory, the saved document's bytes included, and what the reopened
    /// document holds.
    pub(crate) opening: Memory,
    /// Size of the saved document.
    pub(crate) saved: usize,
    /// Size of the saved document compressed by Zstandard at level 3, as eg-strider's own parts
    /// are.
    pub(crate) compressed: usize,
    /// Whether both tasks ended with diamond-types' text.
    pub(crate) same: bool,
}

/// A library's two tasks on one trace, each run once per call, so that every trace takes its
/// turn. Each returns the milliseconds it took.
pub(crate) struct Turns<'a> {
    pub(crate) merge: Box<dyn FnMut() -> f64 + 'a>,
    pub(crate) open: Box<dyn FnMut() -> f64 + 'a>,
}

/// Reads a document's text, as each task ends by doing, and keeps the document.
fn read<D: Document>(mut document: D) -> D {
    std::hint::black_box(document.text());
    document
}

/// Times `task` on its own copy of `bytes`, made untimed, until the document's text is read.
fn timed<D: Document>(bytes: &[u8], task: &impl Fn(Vec<u8>) -> D) -> f64 {
    let input = bytes.to_vec();
    let t = Instant::now();
    let mut document = task(input);
    let text = document.text();
    let ms = since(t);
    std::hint::black_box(&text);
    drop((text, document));
    ms
}

/// Checks a library on the two tasks and measures their memory, then sets them up to be timed in
/// turns with its tasks on the other traces. `merge` builds a document from the library's encoding
/// of the whole history, and `open` reopens its saved document. Every library is handed bytes and
/// every task ends once the text is read, so that decoding, merging and reading are counted alike.
pub(crate) fn prepare<'a, M: Document + 'a, O: Document + 'a>(
    expected: &str,
    history: Vec<u8>,
    merge: impl Fn(Vec<u8>) -> M + 'a,
    saved: Vec<u8>,
    open: impl Fn(Vec<u8>) -> O + 'a,
) -> (Measured, Turns<'a>) {
    let same = merge(history.clone()).text() == expected && open(saved.clone()).text() == expected;
    // The copy of the bytes each task takes is made inside, so that peak memory counts the input.
    let merging = memory_of(|| read(merge(history.clone()))).1;
    let opening = memory_of(|| read(open(saved.clone()))).1;
    let compressed = zstd::bulk::compress(&saved, 3).unwrap().len();
    let measured = Measured { merge_ms: f64::NAN, history: history.len(), merging, open_ms: f64::NAN, opening, saved: saved.len(), compressed, same };
    let turns = Turns { merge: Box::new(move || timed(&history, &merge)), open: Box::new(move || timed(&saved, &open)) };
    (measured, turns)
}

/// Runs one library's task on each trace in turn, round after round, until each has `reps` runs
/// counted. Rotating keeps the library's code warm without letting the branch predictor learn one
/// trace, and rounds begun within [`WARM_UP`] are not counted, while the processor speeds up.
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

/// Milliseconds since `start`.
pub(crate) fn since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

pub(crate) fn median(mut times: Vec<f64>) -> f64 {
    times.sort_by(f64::total_cmp);
    let (below, above) = (times.len().saturating_sub(1) / 2, times.len() / 2);
    times.get(below).map_or(f64::NAN, |low| (low + times[above]) / 2.0)
}

/// The median milliseconds of `reps` runs of `f`, each on what an untimed `setup` makes, and the
/// last result.
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
