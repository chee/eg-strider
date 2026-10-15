use crate::History;
use diamond_types::list::operation::{ListOpKind, TextOperation};
use std::time::{Duration, Instant};

/// The name of the text in every library's document.
pub const TEXT: &str = "text";

/// One patch of a trace, at a position in characters.
pub struct Patch {
    pub ins: bool,
    pub at: usize,
    pub len: usize,
    pub text: String,
}

/// One peer's document in some library.
pub trait Replica {
    /// What a batch of patches turns into, to pass to other replicas.
    type Update;

    /// Creates an empty replica for `peer`. `ascii` says whether all the text to come is ASCII.
    fn new(peer: u64, ascii: bool) -> Self;

    /// Copies this replica, for another peer to carry on from.
    fn fork(&mut self, peer: u64) -> Self;

    /// Takes in what other replicas made.
    fn apply(&mut self, updates: &[&Self::Update]);

    /// Makes the patches. Returns what they made, if anything, and how many were moved to the end
    /// or cut short.
    fn edit(&mut self, patches: &[Patch]) -> (Option<Self::Update>, usize);

    /// Returns an update as the library sends it.
    fn bytes(update: &Self::Update) -> Vec<u8>;

    /// Returns what every replica starts from, as updates that a document merging the history
    /// needs first. Nothing, unless the library has the text made by an update of its own.
    fn start() -> Vec<Self::Update> {
        Vec::new()
    }
}

pub struct Replayed {
    /// What each writer's replica sent, batch by batch in the order made, each after its length.
    pub batches: Vec<u8>,
    pub peers: usize,
    pub moved: usize,
}

/// Consecutive events that nothing branches off partway, which their writer sends as one batch.
struct Batch {
    start: usize,
    len: usize,
    agent: usize,
    parents: Vec<usize>,
    patches: Vec<Patch>,
}

/// Cuts a history into batches: its transactions, split after every event that is another's parent.
fn batches(history: &History<TextOperation, usize>) -> Vec<Batch> {
    let txns = &history.txns;
    let mut cuts: Vec<usize> = txns.iter().flat_map(|txn| history.parents(txn).iter().map(|&p| p + 1)).collect();
    cuts.sort_unstable();
    cuts.dedup();

    let mut batches = Vec::new();
    for txn in txns {
        let (start, end) = (txn.events.start, txn.events.end);
        let inside = &cuts[cuts.partition_point(|&cut| cut <= start)..cuts.partition_point(|&cut| cut < end)];
        let mut from = start;
        for &to in inside.iter().chain([&end]) {
            let mut patches = Vec::new();
            let mut first = start;
            for op in history.ops(txn) {
                let span = op.loc.span;
                let n = span.end - span.start;
                let (a, b) = (from.max(first) - first, to.min(first + n).saturating_sub(first));
                if a < b {
                    let (ins, len) = (op.kind == ListOpKind::Ins, b - a);
                    let (at, text) = match (ins, op.loc.fwd) {
                        (true, _) => (span.start + a, op.content.as_ref().unwrap().chars().skip(a).take(len).collect()),
                        (false, true) => (span.start, String::new()),
                        (false, false) => (span.start + n - b, String::new()),
                    };
                    patches.push(Patch { ins, at, len, text });
                }
                first += n;
            }
            let parents = if from == start { history.parents(txn).to_vec() } else { vec![from - 1] };
            batches.push(Batch { start: from, len: to - from, agent: txn.agent, parents, patches });
            from = to;
        }
    }
    batches
}

/// Which peer makes a batch, and what its author had seen.
struct WriterAssignment {
    peer: usize,
    /// The peer's events before this batch.
    counter: usize,
    /// Per peer known then, how many of its events the author had seen.
    seen: Vec<usize>,
}

/// Replays a history into replicas of another library (Loro, Automerge, Yjs), so that all are
/// measured on the same history. Each peer has a replica, sent exactly what the author
/// of its next patches had seen. An agent with concurrent events becomes several peers, and a new
/// peer starts as a copy of an existing replica. `None` if `limit` runs out.
pub fn replay<R: Replica>(history: &History<TextOperation, usize>, limit: Duration) -> Option<Replayed> {
    let began = Instant::now();
    let batches = batches(history);
    let ascii = batches.iter().all(|batch| batch.patches.iter().all(|patch| patch.text.is_ascii()));

    // Ties between concurrent inserts are broken by peer id where diamond-types uses
    // the agent's name: numbering peers in name order makes most of them agree.
    let agents = batches.iter().map(|batch| batch.agent + 1).max().unwrap_or(0);
    let mut by_name: Vec<usize> = (0..agents).collect();
    by_name.sort_by_key(|&agent| &history.agents[agent]);
    let mut rank = vec![0u64; agents];
    for (place, &agent) in by_name.iter().enumerate() {
        rank[agent] = place as u64 + 1;
    }

    let mut assignments: Vec<WriterAssignment> = Vec::with_capacity(batches.len());
    let mut ids: Vec<u64> = Vec::new();
    let mut next: Vec<usize> = Vec::new();
    let mut last_use: Vec<usize> = Vec::new();
    let mut peers_of: Vec<Vec<usize>> = vec![Vec::new(); agents];
    // After each batch, the new peers that start out as copies of the replica that made it.
    // Starting them empty would mean replaying all they have seen, once for every peer.
    let mut copies: Vec<Vec<usize>> = vec![Vec::new(); batches.len()];
    for (i, batch) in batches.iter().enumerate() {
        let mut seen = vec![0; next.len()];
        let made_by = |parent: usize| batches.partition_point(|a| a.start <= parent) - 1;
        for parent_batch in batch.parents.iter().map(|&parent| made_by(parent)) {
            for (mine, &theirs) in seen.iter_mut().zip(&assignments[parent_batch].seen) {
                *mine = (*mine).max(theirs);
            }
            seen[assignments[parent_batch].peer] = seen[assignments[parent_batch].peer].max(assignments[parent_batch].counter + batches[parent_batch].len);
        }
        // A peer of this agent all of whose events the author had seen can carry on.
        let peer = match peers_of[batch.agent].iter().find(|&&peer| seen[peer] == next[peer]) {
            Some(&peer) => peer,
            None => {
                // It starts from the parent that had seen the most.
                let had_seen = |parent_batch: &usize| assignments[*parent_batch].seen.iter().sum::<usize>() + batches[*parent_batch].len;
                if let Some(from) = batch.parents.iter().map(|&parent| made_by(parent)).max_by_key(had_seen) {
                    copies[from].push(next.len());
                }
                ids.push(rank[batch.agent] << 20 | peers_of[batch.agent].len() as u64);
                peers_of[batch.agent].push(next.len());
                next.push(0);
                last_use.push(0);
                seen.push(0);
                next.len() - 1
            }
        };
        assignments.push(WriterAssignment { peer, counter: next[peer], seen });
        next[peer] += batch.len;
        last_use[peer] = i;
    }

    let peers = next.len();
    eprintln!("  {} batches of events, {peers} peers", batches.len());
    let mut docs: Vec<Option<R>> = (0..peers).map(|_| None).collect();
    let mut held: Vec<Vec<usize>> = vec![Vec::new(); peers];
    let mut updates: Vec<Option<R::Update>> = Vec::with_capacity(batches.len());
    // For each peer, the batches it has made: its events before each, and the batch.
    let mut made: Vec<Vec<(usize, usize)>> = vec![Vec::new(); peers];
    let mut moved = 0;
    for (i, (batch, assignment)) in batches.iter().zip(&assignments).enumerate() {
        if i % 64 == 0 && began.elapsed() > limit {
            return None;
        }
        let document = docs[assignment.peer].get_or_insert_with(|| R::new(ids[assignment.peer], ascii));
        let mut wanted = Vec::new();
        for (other, &seen) in assignment.seen.iter().enumerate() {
            let from = held[assignment.peer].get(other).copied().unwrap_or(0);
            if other != assignment.peer && seen > from {
                let theirs = &made[other];
                let range = theirs.partition_point(|&(counter, _)| counter < from)..theirs.partition_point(|&(counter, _)| counter < seen);
                wanted.extend(theirs[range].iter().filter_map(|&(_, update)| updates[update].as_ref()));
            }
        }
        if !wanted.is_empty() {
            document.apply(&wanted);
        }
        let (update, shifted) = document.edit(&batch.patches);
        moved += shifted;
        updates.push(update);
        made[assignment.peer].push((assignment.counter, i));
        held[assignment.peer].clone_from(&assignment.seen);
        held[assignment.peer][assignment.peer] = assignment.counter + batch.len;
        for &new in &copies[i] {
            let copy = docs[assignment.peer].as_mut().unwrap().fork(ids[new]);
            docs[new] = Some(copy);
            held[new] = held[assignment.peer].clone();
        }
        if last_use[assignment.peer] == i {
            docs[assignment.peer] = None;
        }
    }

    let mut sent = Vec::new();
    for update in R::start().iter().chain(updates.iter().flatten()) {
        let bytes = R::bytes(update);
        sent.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        sent.extend_from_slice(&bytes);
    }
    Some(Replayed { batches: sent, peers, moved })
}

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
