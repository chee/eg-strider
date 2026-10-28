//! Times Automerge's load of the paper's files: released 0.12, and the hashless branch's default
//! (audited) and `next` documents, with the system allocator.
use std::time::Instant;

fn med(mut v: Vec<f64>) -> f64 { v.sort_by(f64::total_cmp); v[v.len() / 2] }
fn time<T>(reps: usize, mut f: impl FnMut() -> T) -> f64 {
    for _ in 0..2 { std::hint::black_box(f()); }
    med((0..reps).map(|_| { let t = Instant::now(); std::hint::black_box(f()); t.elapsed().as_secs_f64() * 1e3 }).collect())
}

fn main() {
    use automerge_hashless::next;
    for n in ["S1", "S2", "S3", "C1", "C2", "A1", "A2"] {
        let paper = std::fs::read(format!("traces/paper/{n}.am")).unwrap();
        let expected = {
            use automerge::ReadDoc;
            let d = automerge::Automerge::load(&paper).unwrap();
            let (_, t) = d.get(automerge::ROOT, "text").unwrap().unwrap();
            d.text(&t).unwrap()
        };
        let released = time(10, || automerge::Automerge::load(&paper).unwrap());
        let audited = time(10, || automerge_hashless::Automerge::load(&paper).unwrap());

        let text = |d: &next::Automerge| {
            use next::ReadDoc;
            let (_, t) = d.get(automerge_hashless::ROOT, "text").unwrap().unwrap();
            d.text(&t).unwrap()
        };
        let from_paper = time(10, || next::Automerge::load(&paper).unwrap());
        let doc = next::Automerge::load(&paper).unwrap();
        let bundle = doc.save();
        let from_bundle = time(10, || next::Automerge::load(&bundle).unwrap());
        let read = time(10, || text(&doc));
        let ok = text(&next::Automerge::load(&bundle).unwrap()) == expected && text(&doc) == expected;
        println!("{n}: 0.12 {released:.1} ms | hashless audited {audited:.1} ms | next from paper's file {from_paper:.1} ms | next from its bundle ({} KB) {from_bundle:.2} ms, then reading text {read:.2} ms | text ok {ok}",
            bundle.len() / 1024);
    }
}
