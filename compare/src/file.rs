//! A file for a history that eg-strider can load without diamond-types, so that loading can
//! be measured on its own. It holds what `extend` hands over and nothing worked out from it;
//! loading appends the events to an empty log one run at a time. Every number is a LEB128
//! varint:
//!
//! - the agents: how many, then each name as its length in bytes and the bytes;
//! - all the inserted text, as its length in bytes and the bytes;
//! - the chunks: how many, then for each its agent, its first sequence number, its parents
//!   (how many, then how far back each is from the chunk's first event), and its ops;
//! - an op is `len << 3 | wide << 2 | backspace << 1 | delete`, then its position, then for a
//!   wide insert (one with characters longer than a byte) the length of its text in bytes.

use diamond_types::list::operation::ListOpKind;
use diamond_types::list::ListOpLog;
use eg_strider::OpLog;

/// Writes a diamond-types oplog out as a file that [`load`] reads.
pub fn save(dt: &ListOpLog) -> Vec<u8> {
    let chunks = dt.as_chunked_operation_vec();
    let (mut agents, mut text, mut body) = (Vec::new(), String::new(), Vec::new());
    write(&mut body, chunks.len());
    for chunk in &chunks {
        let agent = chunk.agent_span.agent;
        let known = agents.iter().position(|&a| a == agent).unwrap_or_else(|| {
            agents.push(agent);
            agents.len() - 1
        });
        write(&mut body, known);
        write(&mut body, chunk.agent_span.seq_range.start);
        let parents = chunk.parents.as_ref();
        write(&mut body, parents.len());
        for &parent in parents {
            write(&mut body, chunk.span.start - parent);
        }
        write(&mut body, chunk.ops.len());
        for op in &chunk.ops {
            let (span, delete) = (op.loc.span, op.kind == ListOpKind::Del);
            let len = span.end - span.start;
            assert!(delete || op.loc.fwd, "reversed inserts are not supported");
            let inserted = if delete { "" } else { op.content.as_ref().expect("insert without content").as_str() };
            let wide = !delete && inserted.len() != len;
            write(&mut body, len << 3 | usize::from(wide) << 2 | usize::from(!op.loc.fwd) << 1 | usize::from(delete));
            write(&mut body, span.start);
            if wide {
                write(&mut body, inserted.len());
            }
            text.push_str(inserted);
        }
    }

    let mut out = Vec::new();
    write(&mut out, agents.len());
    for agent in agents {
        let name = dt.get_agent_name(agent);
        write(&mut out, name.len());
        out.extend_from_slice(name.as_bytes());
    }
    write(&mut out, text.len());
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(&body);
    out
}

/// Loads a history written by [`save`]. Panics if `bytes` is not one.
pub fn load(mut bytes: &[u8]) -> OpLog {
    let from = &mut bytes;
    let mut log = OpLog::new();
    let agents: Vec<_> = (0..read(from)).map(|_| log.get_or_create_agent(string(from))).collect();
    let mut text = string(from);
    let mut parents = Vec::new();
    for _ in 0..read(from) {
        let agent = agents[read(from)];
        let mut seq = read(from);
        let start = log.len();
        parents.clear();
        parents.extend((0..read(from)).map(|_| start - read(from)));
        for _ in 0..read(from) {
            let head = read(from);
            let (len, pos) = (head >> 3, read(from));
            let events = if head & 1 == 1 {
                log.add_remote_delete(agent, seq, &parents, pos..pos + len, head & 2 == 0)
            } else {
                let inserted;
                (inserted, text) = text.split_at(if head & 4 == 0 { len } else { read(from) });
                log.add_remote_insert(agent, seq, &parents, pos, inserted)
            };
            // The next op in a chunk follows the last event of this one.
            seq += events.len();
            parents.clear();
            parents.push(events.end - 1);
        }
    }
    log
}

/// Appends a number as a varint.
fn write(out: &mut Vec<u8>, mut n: usize) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Takes a varint off the front of `from`.
fn read(from: &mut &[u8]) -> usize {
    let mut n = 0;
    for shift in (0..).step_by(7) {
        let byte = from[0];
        *from = &from[1..];
        n |= usize::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            break;
        }
    }
    n
}

/// Takes a string off the front of `from`: its length in bytes, then the bytes.
fn string<'a>(from: &mut &'a [u8]) -> &'a str {
    let len = read(from);
    let (bytes, rest) = from.split_at(len);
    *from = rest;
    std::str::from_utf8(bytes).expect("text in a history file is UTF-8")
}
