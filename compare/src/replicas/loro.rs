use super::{Patch, Replica, TEXT};

pub struct Loro {
    document: loro::LoroDoc,
}

impl Loro {
    /// Wraps a document, giving it `peer` as its id.
    fn with_peer(document: loro::LoroDoc, peer: u64) -> Self {
        document.set_peer_id(peer).unwrap();
        Loro { document }
    }
}

impl Replica for Loro {
    type Update = Vec<u8>;

    fn new(peer: u64, _: bool) -> Self {
        Loro::with_peer(loro::LoroDoc::new(), peer)
    }

    fn fork(&mut self, peer: u64) -> Self {
        Loro::with_peer(self.document.fork(), peer)
    }

    fn apply(&mut self, updates: &[&Self::Update]) {
        let updates: Vec<Vec<u8>> = updates.iter().map(|&update| update.clone()).collect();
        self.document.import_batch(&updates).unwrap();
    }

    fn edit(&mut self, patches: &[Patch]) -> (Option<Self::Update>, usize) {
        let before = self.document.oplog_vv();
        let text = self.document.get_text(TEXT);
        let (mut moved, mut made) = (0, false);
        for patch in patches {
            let len = text.len_unicode();
            let at = patch.at.min(len);
            let count = if patch.ins { patch.len } else { patch.len.min(len - at) };
            moved += usize::from(at < patch.at || count < patch.len);
            if patch.ins {
                text.insert(at, &patch.text).unwrap();
            } else if count > 0 {
                text.delete(at, count).unwrap();
            }
            made |= count > 0;
        }
        self.document.commit();
        (made.then(|| self.document.export(loro::ExportMode::updates(&before)).unwrap()), moved)
    }

    fn bytes(update: &Self::Update) -> Vec<u8> {
        update.clone()
    }
}
