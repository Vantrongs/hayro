use crate::object::dict::keys::{ROOT, TYPE};
use crate::object::name::decoded_bytes;
use crate::object::{MAX_OBJECT_NESTING_DEPTH, Name, Object};
use crate::reader::{Ends, Reader, ReaderExt, skip_white_spaces_and_comments_in};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

const MAX_FAILED_CACHE: usize = 4 * MAX_OBJECT_NESTING_DEPTH;
const MAX_BATCH_BYTES: usize = 12 * 1024;
const MAX_BATCH_STEPS: usize = 8 * MAX_OBJECT_NESTING_DEPTH;

#[derive(Default)]
pub(super) struct Containers {
    failed: BTreeSet<usize>,
    tails: BTreeMap<(usize, bool), Option<Container>>,
    furthest: usize,
    last_ok: Option<(usize, Container)>,
    frames: Vec<Frame>,
}

#[derive(Clone, Copy)]
pub(super) struct Container {
    pub(super) end: usize,
    pub(super) has_root: bool,
    pub(super) has_type: bool,
    height: u16,
}

impl Container {
    const EMPTY: Self = Self {
        end: 0,
        has_root: false,
        has_type: false,
        height: 1,
    };

    fn append(&mut self, tail: Self) {
        self.end = tail.end;
        self.has_root |= tail.has_root;
        self.has_type |= tail.has_type;
        self.height = self.height.max(tail.height);
    }
}

struct Frame {
    start: usize,
    dict: bool,
    value: Container,
    after_join: bool,
    prefixes: Vec<(usize, Container)>,
}

impl Frame {
    fn open(r: &mut Reader<'_>) -> Option<Self> {
        let start = r.offset();
        let dict = if r.forward_tag(b"[").is_some() {
            false
        } else {
            r.forward_tag(b"<<")?;
            true
        };
        Some(Self {
            start,
            dict,
            value: Container::EMPTY,
            after_join: false,
            prefixes: Vec::new(),
        })
    }

    fn read_dict_key(&mut self, r: &mut Reader<'_>) -> bool {
        let Some(raw) = r.skip::<Name<'_>>(false) else {
            return false;
        };
        let mut prefix = [0; 5];
        let mut len = 0;
        for (slot, byte) in prefix.iter_mut().zip(decoded_bytes(&raw[1..])) {
            *slot = byte;
            len += 1;
        }
        self.value.has_root |= &prefix[..len] == ROOT;
        self.value.has_type |= &prefix[..len] == TYPE;
        true
    }
}

impl Containers {
    pub(super) fn skip_object(&mut self, ends: &impl Ends, r: &mut Reader<'_>) -> Option<()> {
        if r.peek_byte() == Some(b'[') || r.peek_bytes(2) == Some(b"<<") {
            self.resolve(ends, r.clone()).map(|value| r.jump(value.end))
        } else {
            Object::skip_in(r, false, 0, ends)
        }
    }

    pub(super) fn evict_before(&mut self, pos: usize) {
        while self
            .tails
            .first_key_value()
            .is_some_and(|(&(at, _), _)| at < pos)
        {
            self.tails.pop_first();
        }
        while self.failed.first().is_some_and(|&offset| offset < pos) {
            self.failed.pop_first();
        }
    }

    pub(super) fn resolve(&mut self, ends: &impl Ends, mut r: Reader<'_>) -> Option<Container> {
        let start = r.offset();
        if self.failed.contains(&start) {
            return None;
        }
        if let Some((offset, value)) = self.last_ok
            && offset == start
        {
            return Some(value);
        }

        self.frames.push(Frame::open(&mut r)?);
        let mut doomed_prefix = 0;
        let mut batch_steps = 0;
        let mut batch_truncated = false;
        let mut syntax_error = false;
        while let Some(frame) = self.frames.last_mut() {
            if doomed_prefix > 0 {
                if doomed_prefix >= MAX_OBJECT_NESTING_DEPTH || batch_steps == MAX_BATCH_STEPS {
                    break;
                }
                if batch_steps == 0 {
                    let limit = r.offset().saturating_add(MAX_BATCH_BYTES);
                    if limit < r.data.len() {
                        r = Reader::new_with(&r.data[..limit], r.offset());
                        batch_truncated = true;
                    }
                }
                batch_steps += 1;
            }
            let after_comment = skip_trivia(&mut r, ends);
            let join = core::mem::take(&mut frame.after_join) || after_comment;
            let mut hit = false;
            // Separate probes can meet after a literal string, a comment, or a shared child.
            if join && r.offset() <= self.furthest && doomed_prefix == 0 {
                let key = (r.offset(), frame.dict);
                match self.tails.get(&key) {
                    Some(Some(tail)) => {
                        frame.value.append(*tail);
                        r.jump(tail.end);
                        hit = true;
                    }
                    Some(None) => {
                        syntax_error = true;
                        doomed_prefix = self.frames.len();
                        break;
                    }
                    None => frame.prefixes.push((
                        key.0,
                        core::mem::replace(&mut frame.value, Container::EMPTY),
                    )),
                }
            }
            let closer: &[u8] = if frame.dict { b">>" } else { b"]" };
            if hit || r.forward_tag(closer).is_some() {
                let frame = self.frames.pop().unwrap();
                let shared = hit || !frame.prefixes.is_empty();
                let mut closed = frame.value;
                closed.end = r.offset();
                for (at, mut prefix) in frame.prefixes.into_iter().rev() {
                    if !batch_truncated {
                        self.tails.insert((at, frame.dict), Some(closed));
                    }
                    prefix.append(closed);
                    closed = prefix;
                }
                let Some(parent) = self.frames.last_mut() else {
                    self.furthest = self.furthest.max(r.offset());
                    self.last_ok = Some((start, closed));
                    return Some(closed);
                };
                parent.after_join |= shared;
                parent.value.height = parent.value.height.max(closed.height + 1);
                if usize::from(parent.value.height) > MAX_OBJECT_NESTING_DEPTH {
                    doomed_prefix = self.frames.len();
                    break;
                }
                continue;
            }

            let has_key = frame.dict && frame.read_dict_key(&mut r);
            let shared_value = has_key && skip_trivia(&mut r, ends);
            frame.after_join = shared_value || r.peek_byte() == Some(b'(');
            if let Some(mut open) = Frame::open(&mut r) {
                open.after_join = shared_value;
                if self.failed.contains(&open.start) {
                    doomed_prefix = self.frames.len();
                    break;
                }
                self.frames.push(open);
                doomed_prefix =
                    doomed_prefix.max(self.frames.len().saturating_sub(MAX_OBJECT_NESTING_DEPTH));
                continue;
            }
            let scalar = if !frame.dict || has_key {
                Object::skip_maybe_ref_in(&mut r, false, 0, ends)
            } else {
                Object::skip_in(&mut r, false, 0, ends)
            };
            if scalar.is_none() {
                if !batch_truncated {
                    syntax_error = true;
                    doomed_prefix = self.frames.len();
                }
                break;
            }
        }
        self.furthest = self.furthest.max(r.offset());
        if syntax_error {
            for frame in &self.frames {
                for &(at, _) in &frame.prefixes {
                    self.tails.insert((at, frame.dict), None);
                }
            }
        }
        // Only the outer prefix is known to fail when the batch budget runs out.
        for frame in self.frames.drain(..doomed_prefix) {
            if self.failed.len() == MAX_FAILED_CACHE {
                if self.failed.last().is_some_and(|&last| last < frame.start) {
                    break;
                }
                self.failed.pop_last();
            }
            self.failed.insert(frame.start);
        }
        self.frames.clear();
        None
    }
}

fn skip_trivia(r: &mut Reader<'_>, ends: &impl Ends) -> bool {
    r.skip_white_spaces();
    if r.peek_byte() != Some(b'%') {
        return false;
    }
    skip_white_spaces_and_comments_in(r, ends);
    true
}

#[cfg(test)]
mod tests;
