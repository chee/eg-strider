//! Replays a history into another library (Loro, Automerge, Yjs) so that all of them are
//! measured on the same thing. Each chunk of events has to be made by a document that holds
//! exactly what its author had seen, so there is one replica per peer, sent just the
//! updates it lacks before it makes its next edits. An agent with events concurrent to its
//! own becomes several peers, and a new peer starts as a copy of an existing replica.

use diamond_types::list::operation::ListOpKind;
use diamond_types::list::ListOpLog;
use std::time::{Duration, Instant};

/// The name of the text in every library's document.
pub const TEXT: &str = "text";

/// One edit, at a position in characters.
pub struct Edit {
    pub ins: bool,
    pub at: usize,
    pub len: usize,
    pub text: String,
}

/// One peer's document in some library.
pub trait Replica {
    /// What a batch of edits turns into, to pass to other replicas.
    type Update;

    /// Creates an empty replica for `peer`. `ascii` says whether all the text to come is ASCII.
    fn new(peer: u64, ascii: bool) -> Self;

    /// Copies this replica, for another peer to carry on from.
    fn fork(&mut self, peer: u64) -> Self;

    /// Takes in what other replicas made.
    fn apply(&mut self, updates: &[&Self::Update]);

    /// Makes the edits. Returns what they made, if anything, and how many could not be
    /// made where they were recorded and were moved to the end or cut short.
    fn edit(&mut self, edits: &[Edit]) -> (Option<Self::Update>, usize);

    /// Saves everything the replica holds, in the library's own format.
    fn save(&mut self) -> Vec<u8>;
}

pub struct Replayed {
    pub saved: Vec<u8>,
    pub peers: usize,
    pub moved: usize,
}

/// Events made one after another that no other event branches from part-way.
struct Block {
    start: usize,
    len: usize,
    agent: usize,
    parents: Vec<usize>,
    edits: Vec<Edit>,
}

/// Cuts a log into blocks: its chunks, split wherever an event is some other event's parent.
fn blocks(dt: &ListOpLog) -> Vec<Block> {
    let chunks = dt.as_chunked_operation_vec();
    let mut cuts: Vec<usize> = chunks.iter().flat_map(|chunk| chunk.parents.as_ref().iter().map(|&p| p + 1)).collect();
    cuts.sort_unstable();
    cuts.dedup();

    let mut blocks = Vec::new();
    for chunk in &chunks {
        let (start, end) = (chunk.span.start, chunk.span.end);
        let inside = &cuts[cuts.partition_point(|&cut| cut <= start)..cuts.partition_point(|&cut| cut < end)];
        let mut from = start;
        for &to in inside.iter().chain([&end]) {
            let mut edits = Vec::new();
            let mut first = start;
            for op in &chunk.ops {
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
                    edits.push(Edit { ins, at, len, text });
                }
                first += n;
            }
            let parents = if from == start { chunk.parents.as_ref().to_vec() } else { vec![from - 1] };
            blocks.push(Block { start: from, len: to - from, agent: chunk.agent_span.agent as usize, parents, edits });
            from = to;
        }
    }
    blocks
}

/// Which peer makes an block, and what its author had seen.
struct Plan {
    peer: usize,
    /// The peer's events before this block.
    counter: usize,
    /// For each peer known at the time, how many of its events the author had seen.
    seen: Vec<usize>,
}

/// Replays a diamond-types log into replicas of another library, one for each peer, each
/// sent exactly what the author of its next edits had seen. `None` if `limit` runs out.
///
/// An agent whose events are concurrent with one another becomes several peers, since
/// these libraries need each peer's events to be made one after another.
pub fn replay<R: Replica>(dt: &ListOpLog, limit: Duration) -> Option<Replayed> {
    let began = Instant::now();
    let blocks = blocks(dt);
    let ascii = blocks.iter().all(|block| block.edits.iter().all(|edit| edit.text.is_ascii()));

    // Ties between concurrent inserts are broken by peer id where diamond-types uses
    // the agent's name: numbering peers in name order makes most of them agree.
    let agents = blocks.iter().map(|block| block.agent + 1).max().unwrap_or(0);
    let mut by_name: Vec<usize> = (0..agents).collect();
    by_name.sort_by_key(|&agent| dt.get_agent_name(agent as _));
    let mut rank = vec![0u64; agents];
    for (place, &agent) in by_name.iter().enumerate() {
        rank[agent] = place as u64 + 1;
    }

    let mut plans: Vec<Plan> = Vec::with_capacity(blocks.len());
    let mut ids: Vec<u64> = Vec::new();
    let mut next: Vec<usize> = Vec::new();
    let mut last_use: Vec<usize> = Vec::new();
    let mut peers_of: Vec<Vec<usize>> = vec![Vec::new(); agents];
    // After each block, the new peers that start out as copies of the replica that made it.
    // Starting them empty would mean replaying all they have seen, once for every peer.
    let mut copies: Vec<Vec<usize>> = vec![Vec::new(); blocks.len()];
    for (i, block) in blocks.iter().enumerate() {
        let mut seen = vec![0; next.len()];
        let made_by = |parent: usize| blocks.partition_point(|a| a.start <= parent) - 1;
        for k in block.parents.iter().map(|&parent| made_by(parent)) {
            for (mine, &theirs) in seen.iter_mut().zip(&plans[k].seen) {
                *mine = (*mine).max(theirs);
            }
            seen[plans[k].peer] = seen[plans[k].peer].max(plans[k].counter + blocks[k].len);
        }
        // A peer of this agent all of whose events the author had seen can carry on.
        let peer = match peers_of[block.agent].iter().find(|&&peer| seen[peer] == next[peer]) {
            Some(&peer) => peer,
            None => {
                // It starts from the parent that had seen the most.
                let had_seen = |k: &usize| plans[*k].seen.iter().sum::<usize>() + blocks[*k].len;
                if let Some(from) = block.parents.iter().map(|&parent| made_by(parent)).max_by_key(had_seen) {
                    copies[from].push(next.len());
                }
                ids.push(rank[block.agent] << 20 | peers_of[block.agent].len() as u64);
                peers_of[block.agent].push(next.len());
                next.push(0);
                last_use.push(0);
                seen.push(0);
                next.len() - 1
            }
        };
        plans.push(Plan { peer, counter: next[peer], seen });
        next[peer] += block.len;
        last_use[peer] = i;
    }

    let peers = next.len();
    eprintln!("  {} stretches of events, {peers} peers", blocks.len());
    let mut docs: Vec<Option<R>> = (0..peers).map(|_| None).collect();
    let mut held: Vec<Vec<usize>> = vec![Vec::new(); peers];
    let mut updates: Vec<Option<R::Update>> = Vec::with_capacity(blocks.len());
    // For each peer, the blocks it has made: its events before each, and the block.
    let mut made: Vec<Vec<(usize, usize)>> = vec![Vec::new(); peers];
    let mut moved = 0;
    for (i, (block, plan)) in blocks.iter().zip(&plans).enumerate() {
        if i % 64 == 0 && began.elapsed() > limit {
            return None;
        }
        let doc = docs[plan.peer].get_or_insert_with(|| R::new(ids[plan.peer], ascii));
        let mut wanted = Vec::new();
        for (other, &seen) in plan.seen.iter().enumerate() {
            let from = held[plan.peer].get(other).copied().unwrap_or(0);
            if other != plan.peer && seen > from {
                let theirs = &made[other];
                let range = theirs.partition_point(|&(counter, _)| counter < from)..theirs.partition_point(|&(counter, _)| counter < seen);
                wanted.extend(theirs[range].iter().filter_map(|&(_, k)| updates[k].as_ref()));
            }
        }
        if !wanted.is_empty() {
            doc.apply(&wanted);
        }
        let (update, shifted) = doc.edit(&block.edits);
        moved += shifted;
        updates.push(update);
        made[plan.peer].push((plan.counter, i));
        held[plan.peer].clone_from(&plan.seen);
        held[plan.peer][plan.peer] = plan.counter + block.len;
        for &new in &copies[i] {
            let copy = docs[plan.peer].as_mut().unwrap().fork(ids[new]);
            docs[new] = Some(copy);
            held[new] = held[plan.peer].clone();
        }
        if last_use[plan.peer] == i {
            docs[plan.peer] = None;
        }
    }

    let mut all = R::new(u32::MAX as u64, ascii);
    all.apply(&updates.iter().flatten().collect::<Vec<_>>());
    Some(Replayed { saved: all.save(), peers, moved })
}

pub struct Automerge {
    doc: automerge::AutoCommit,
    text: automerge::ObjId,
}

impl Replica for Automerge {
    type Update = automerge::Change;

    fn new(peer: u64, _: bool) -> Self {
        use automerge::{transaction::Transactable, ActorId, AutoCommit, ObjType, ReadDoc, ROOT};
        // Every replica starts from the same document, which holds the text object.
        static BASE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        let base = BASE.get_or_init(|| {
            let mut doc = AutoCommit::new().with_actor(ActorId::from(vec![0u8; 8]));
            doc.put_object(ROOT, "text", ObjType::Text).unwrap();
            doc.save()
        });
        let doc = AutoCommit::load(base).unwrap().with_actor(ActorId::from(peer.to_be_bytes().to_vec()));
        let (_, text) = doc.get(ROOT, "text").unwrap().unwrap();
        Automerge { doc, text }
    }

    fn fork(&mut self, peer: u64) -> Self {
        let doc = self.doc.fork().with_actor(automerge::ActorId::from(peer.to_be_bytes().to_vec()));
        Automerge { doc, text: self.text.clone() }
    }

    fn apply(&mut self, updates: &[&Self::Update]) {
        self.doc.apply_changes(updates.iter().map(|&change| change.clone())).unwrap();
    }

    fn edit(&mut self, edits: &[Edit]) -> (Option<Self::Update>, usize) {
        use automerge::{transaction::Transactable, ReadDoc};
        let (mut moved, mut made) = (0, false);
        for edit in edits {
            let len = self.doc.length(&self.text);
            let at = edit.at.min(len);
            let count = if edit.ins { edit.len } else { edit.len.min(len - at) };
            moved += usize::from(at < edit.at || count < edit.len);
            if count > 0 {
                let removed = if edit.ins { 0 } else { count as isize };
                self.doc.splice_text(&self.text, at, removed, &edit.text).unwrap();
                made = true;
            }
        }
        self.doc.commit();
        (made.then(|| self.doc.get_last_local_change().unwrap().clone()), moved)
    }

    fn save(&mut self) -> Vec<u8> {
        self.doc.save_nocompress()
    }
}

pub struct Yjs {
    doc: yrs::Doc,
    text: yrs::TextRef,
    /// The text, kept only when it is not all ASCII: yrs counts positions in bytes.
    chars: Option<Vec<char>>,
}

impl Replica for Yjs {
    type Update = Vec<u8>;

    fn new(peer: u64, ascii: bool) -> Self {
        let options = yrs::Options { client_id: yrs::block::ClientID::new(peer), skip_gc: true, offset_kind: yrs::OffsetKind::Bytes, ..Default::default() };
        let doc = yrs::Doc::with_options(options);
        let text = doc.get_or_insert_text("text");
        Yjs { doc, text, chars: (!ascii).then(Vec::new) }
    }

    fn fork(&mut self, peer: u64) -> Self {
        use yrs::updates::decoder::Decode;
        use yrs::{ReadTxn, StateVector, Transact};
        let state = self.doc.transact().encode_state_as_update_v1(&StateVector::default());
        let copy = Yjs { chars: self.chars.clone(), ..Yjs::new(peer, true) };
        copy.doc.transact_mut().apply_update(yrs::Update::decode_v1(&state).unwrap()).unwrap();
        copy
    }

    fn apply(&mut self, updates: &[&Self::Update]) {
        use yrs::updates::decoder::Decode;
        use yrs::{GetString, Transact};
        let mut txn = self.doc.transact_mut();
        for update in updates {
            txn.apply_update(yrs::Update::decode_v1(update).unwrap()).unwrap();
        }
        if let Some(chars) = &mut self.chars {
            *chars = self.text.get_string(&txn).chars().collect();
        }
    }

    fn edit(&mut self, edits: &[Edit]) -> (Option<Self::Update>, usize) {
        use yrs::{Text, Transact};
        let mut txn = self.doc.transact_mut();
        let (mut moved, mut made) = (0, false);
        for edit in edits {
            let len = self.chars.as_ref().map_or_else(|| self.text.len(&txn) as usize, Vec::len);
            let at = edit.at.min(len);
            let count = if edit.ins { edit.len } else { edit.len.min(len - at) };
            moved += usize::from(at < edit.at || count < edit.len);
            let bytes = |range: std::ops::Range<usize>| match &self.chars {
                Some(chars) => chars[range].iter().map(|ch| ch.len_utf8()).sum::<usize>() as u32,
                None => range.len() as u32,
            };
            let offset = bytes(0..at);
            if edit.ins {
                self.text.insert(&mut txn, offset, &edit.text);
                if let Some(chars) = &mut self.chars {
                    chars.splice(at..at, edit.text.chars());
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
        (made.then(|| txn.encode_update_v1()), moved)
    }

    fn save(&mut self) -> Vec<u8> {
        use yrs::{ReadTxn, StateVector, Transact};
        self.doc.transact().encode_state_as_update_v2(&StateVector::default())
    }
}

pub struct Loro {
    doc: loro::LoroDoc,
}

impl Loro {
    /// Wraps a document, giving it `peer` as its id.
    fn with_peer(doc: loro::LoroDoc, peer: u64) -> Self {
        doc.set_peer_id(peer).unwrap();
        Loro { doc }
    }
}

impl Replica for Loro {
    type Update = Vec<u8>;

    fn new(peer: u64, _: bool) -> Self {
        Loro::with_peer(loro::LoroDoc::new(), peer)
    }

    fn fork(&mut self, peer: u64) -> Self {
        Loro::with_peer(self.doc.fork(), peer)
    }

    fn apply(&mut self, updates: &[&Self::Update]) {
        let updates: Vec<Vec<u8>> = updates.iter().map(|&update| update.clone()).collect();
        self.doc.import_batch(&updates).unwrap();
    }

    fn edit(&mut self, edits: &[Edit]) -> (Option<Self::Update>, usize) {
        let before = self.doc.oplog_vv();
        let text = self.doc.get_text(TEXT);
        let (mut moved, mut made) = (0, false);
        for edit in edits {
            let len = text.len_unicode();
            let at = edit.at.min(len);
            let count = if edit.ins { edit.len } else { edit.len.min(len - at) };
            moved += usize::from(at < edit.at || count < edit.len);
            if edit.ins {
                text.insert(at, &edit.text).unwrap();
            } else if count > 0 {
                text.delete(at, count).unwrap();
            }
            made |= count > 0;
        }
        self.doc.commit();
        (made.then(|| self.doc.export(loro::ExportMode::updates(&before)).unwrap()), moved)
    }

    fn save(&mut self) -> Vec<u8> {
        self.doc.export(loro::ExportMode::all_updates()).unwrap()
    }
}
