use super::{Patch, Replica};

pub struct Automerge {
    document: automerge_replay::AutoCommit,
    text: automerge_replay::ObjId,
}

/// The document every Automerge replica starts from, which holds the text object.
fn automerge_base() -> &'static [u8] {
    use automerge_replay::{transaction::Transactable, ActorId, AutoCommit, ObjType, ROOT};
    static BASE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let mut document = AutoCommit::new().with_actor(ActorId::from(vec![0u8; 8]));
        document.put_object(ROOT, "text", ObjType::Text).unwrap();
        document.save()
    })
}

impl Replica for Automerge {
    type Update = automerge_replay::Change;

    fn new(peer: u64, _: bool) -> Self {
        use automerge_replay::{ActorId, AutoCommit, ReadDoc, ROOT};
        let document = AutoCommit::load(automerge_base()).unwrap().with_actor(ActorId::from(peer.to_be_bytes().to_vec()));
        let (_, text) = document.get(ROOT, "text").unwrap().unwrap();
        Automerge { document, text }
    }

    fn start() -> Vec<Self::Update> {
        automerge_replay::AutoCommit::load(automerge_base()).unwrap().get_changes(&[]).into_iter().cloned().collect()
    }

    fn fork(&mut self, peer: u64) -> Self {
        let document = self.document.fork().with_actor(automerge_replay::ActorId::from(peer.to_be_bytes().to_vec()));
        Automerge { document, text: self.text.clone() }
    }

    fn apply(&mut self, updates: &[&Self::Update]) {
        self.document.apply_changes(updates.iter().map(|&change| change.clone())).unwrap();
    }

    fn edit(&mut self, patches: &[Patch]) -> (Option<Self::Update>, usize) {
        use automerge_replay::{transaction::Transactable, ReadDoc};
        let (mut moved, mut made) = (0, false);
        for patch in patches {
            let len = self.document.length(&self.text);
            let at = patch.at.min(len);
            let count = if patch.ins { patch.len } else { patch.len.min(len - at) };
            moved += usize::from(at < patch.at || count < patch.len);
            if count > 0 {
                let removed = if patch.ins { 0 } else { count as isize };
                self.document.splice_text(&self.text, at, removed, &patch.text).unwrap();
                made = true;
            }
        }
        self.document.commit();
        (made.then(|| self.document.get_last_local_change().unwrap().clone()), moved)
    }

    fn bytes(change: &Self::Update) -> Vec<u8> {
        change.raw_bytes().to_vec()
    }
}
