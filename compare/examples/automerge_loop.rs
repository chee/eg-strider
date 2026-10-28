//! Loads one of the paper's Automerge files over and over with the hashless branch, re-saved in
//! its own format, to be sampled.
fn main() {
    let name = std::env::args().nth(1).unwrap_or("S1".into());
    let paper = std::fs::read(format!("traces/paper/{name}.am")).unwrap();
    let doc = automerge_hashless::Automerge::load(&paper).unwrap();
    let saved = doc.save();
    let flat = doc.save_nocompress();
    let time = |bytes: &[u8]| {
        let t = std::time::Instant::now();
        for _ in 0..10 { std::hint::black_box(automerge_hashless::Automerge::load(bytes).unwrap()); }
        t.elapsed().as_secs_f64() * 100.0
    };
    eprintln!("{name}: bundle {:.1} ms ({} KB), uncompressed bundle {:.1} ms ({} KB)", time(&saved), saved.len() / 1024, time(&flat), flat.len() / 1024);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < until {
        std::hint::black_box(automerge_hashless::Automerge::load(&saved).unwrap());
    }
}
