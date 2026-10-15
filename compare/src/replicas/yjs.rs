use super::{Patch, Replica};

pub struct Yjs {
    document: yrs::Doc,
    text: yrs::TextRef,
    /// The text, kept only when it is not all ASCII, since yrs counts positions in bytes.
    chars: Option<Vec<char>>,
    /// The update yrs emitted for the last transaction, once committed: what a writer sends.
    sent: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

impl Replica for Yjs {
    type Update = Vec<u8>;

    fn new(peer: u64, ascii: bool) -> Self {
        let options = yrs::Options { client_id: yrs::block::ClientID::new(peer), skip_gc: true, offset_kind: yrs::OffsetKind::Bytes, ..Default::default() };
        let document = yrs::Doc::with_options(options);
        let text = document.get_or_insert_text("text");
        let sent = std::sync::Arc::new(std::sync::Mutex::new(None));
        let to = sent.clone();
        document.observe_update_v1("sent", move |_, event| *to.lock().unwrap() = Some(event.update.clone())).unwrap();
        Yjs { document, text, chars: (!ascii).then(Vec::new), sent }
    }

    fn fork(&mut self, peer: u64) -> Self {
        use yrs::updates::decoder::Decode;
        use yrs::{ReadTxn, StateVector, Transact};
        let state = self.document.transact().encode_state_as_update_v1(&StateVector::default());
        let copy = Yjs { chars: self.chars.clone(), ..Yjs::new(peer, true) };
        copy.document.transact_mut().apply_update(yrs::Update::decode_v1(&state).unwrap()).unwrap();
        copy
    }

    fn apply(&mut self, updates: &[&Self::Update]) {
        use yrs::updates::decoder::Decode;
        use yrs::{GetString, Transact};
        let mut txn = self.document.transact_mut();
        for update in updates {
            txn.apply_update(yrs::Update::decode_v1(update).unwrap()).unwrap();
        }
        if let Some(chars) = &mut self.chars {
            *chars = self.text.get_string(&txn).chars().collect();
        }
    }

    fn edit(&mut self, patches: &[Patch]) -> (Option<Self::Update>, usize) {
        use yrs::{Text, Transact};
        // Applying what other replicas made emits updates too, which are not this one's.
        self.sent.lock().unwrap().take();
        let mut txn = self.document.transact_mut();
        let (mut moved, mut made) = (0, false);
        for patch in patches {
            let len = self.chars.as_ref().map_or_else(|| self.text.len(&txn) as usize, Vec::len);
            let at = patch.at.min(len);
            let count = if patch.ins { patch.len } else { patch.len.min(len - at) };
            moved += usize::from(at < patch.at || count < patch.len);
            let bytes = |range: std::ops::Range<usize>| match &self.chars {
                Some(chars) => chars[range].iter().map(|ch| ch.len_utf8()).sum::<usize>() as u32,
                None => range.len() as u32,
            };
            let offset = bytes(0..at);
            if patch.ins {
                self.text.insert(&mut txn, offset, &patch.text);
                if let Some(chars) = &mut self.chars {
                    chars.splice(at..at, patch.text.chars());
                }
                made = true;
            } else if count > 0 {
                self.text.remove_range(&mut txn, offset, bytes(at..at + count));
                if let Some(chars) = &mut self.chars {
                    chars.drain(at..at + count);
                }
                made = true;
            }
        }
        // Committing joins up what the transaction made, and the update yrs then emits is what a
        // writer sends.
        drop(txn);
        let sent = self.sent.lock().unwrap().take();
        (sent.filter(|_| made), moved)
    }

    fn bytes(update: &Self::Update) -> Vec<u8> {
        update.clone()
    }
}
