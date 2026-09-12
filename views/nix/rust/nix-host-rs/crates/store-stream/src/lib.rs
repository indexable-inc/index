//! Streaming store reference detection, byte rewriting, and self-reference hashes.
//!
//! Replacements inspect original input exactly once. Matches are leftmost,
//! non-overlapping; keys at the same position are ordered lexicographically.
//! Chunk boundaries never change either the bytes or the match offsets.

#![forbid(unsafe_code)]

use sha2::Digest as _;
use std::collections::HashMap;

/// Encoded store-path hash length.
pub const REFERENCE_LENGTH: usize = 32;
fn is_reference_byte(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'd' | b'f'..=b'n' | b'p'..=b's' | b'v'..=b'z')
}

/// Invalid input, a closed stream, or a rejected output chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    Invalid,
    Failed,
    Sink,
}

pub type Result<T> = std::result::Result<T, StreamError>;

/// Detects a fixed set of encoded hashes across arbitrary input chunks.
pub struct RefScanner {
    remaining: HashMap<[u8; REFERENCE_LENGTH], usize>,
    found: Vec<usize>,
    tail: Vec<u8>,
}

impl RefScanner {
    /// Candidate indices found so far, in first-occurrence order.
    pub fn found(&self) -> &[usize] {
        &self.found
    }

    /// Candidates are packed 32-byte hashes; duplicates and invalid encodings fail.
    pub fn new(hashes: &[u8]) -> Result<Self> {
        if !hashes.len().is_multiple_of(REFERENCE_LENGTH) {
            return Err(StreamError::Invalid);
        }
        let mut remaining = HashMap::with_capacity(hashes.len() / REFERENCE_LENGTH);
        for (index, bytes) in hashes.chunks_exact(REFERENCE_LENGTH).enumerate() {
            let hash =
                <[u8; REFERENCE_LENGTH]>::try_from(bytes).map_err(|_| StreamError::Invalid)?;
            if !hash.iter().all(|byte| is_reference_byte(*byte))
                || remaining.insert(hash, index).is_some()
            {
                return Err(StreamError::Invalid);
            }
        }
        Ok(Self {
            remaining,
            found: Vec::new(),
            tail: Vec::with_capacity(REFERENCE_LENGTH - 1),
        })
    }

    fn scan(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while !self.remaining.is_empty() {
            let Some(window) = bytes
                .get(offset..)
                .and_then(|rest| rest.get(..REFERENCE_LENGTH))
            else {
                break;
            };
            if let Some(invalid) = window.iter().rposition(|byte| !is_reference_byte(*byte)) {
                offset += invalid + 1;
                continue;
            }
            if let Ok(key) = <[u8; REFERENCE_LENGTH]>::try_from(window)
                && let Some(index) = self.remaining.remove(&key)
            {
                self.found.push(index);
            }
            offset += 1;
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.remaining.is_empty() {
            return;
        }
        // Only windows crossing this boundary need copying. The main chunk
        // is scanned in place, with no per-window allocation.
        let mut boundary = [0u8; 2 * (REFERENCE_LENGTH - 1)];
        let prefix = bytes.len().min(REFERENCE_LENGTH - 1);
        let count = self.tail.len() + prefix;
        if let Some(destination) = boundary.get_mut(..self.tail.len()) {
            destination.copy_from_slice(&self.tail);
        }
        if let (Some(destination), Some(source)) = (
            boundary.get_mut(self.tail.len()..count),
            bytes.get(..prefix),
        ) {
            destination.copy_from_slice(source);
        }
        if let Some(joined) = boundary.get(..count) {
            self.scan(joined);
        }
        self.scan(bytes);
        if bytes.len() >= REFERENCE_LENGTH - 1 {
            self.tail.clear();
            self.tail.extend_from_slice(
                bytes
                    .get(bytes.len() - (REFERENCE_LENGTH - 1)..)
                    .unwrap_or_default(),
            );
        } else {
            self.tail.extend_from_slice(bytes);
            let discard = self.tail.len().saturating_sub(REFERENCE_LENGTH - 1);
            self.tail.drain(..discard);
        }
    }
}

/// An equal-length replacement validated by `Rewriter::new`.
pub struct RewriteRule {
    pub from: Vec<u8>,
    pub to: Vec<u8>,
}

struct RuleNode {
    exemplar: usize,
    prefix_end: usize,
    terminal: Option<usize>,
    children: std::ops::Range<usize>,
}

struct RuleEdge {
    byte: u8,
    node: usize,
}

/// A compressed radix tree over the sorted keys. Prefix spans borrow the rule
/// bytes; the arena owns no copies of keys and needs no recursive traversal or
/// destruction. A lookup examines shared prefixes once, independently of how
/// many rules share them.
struct RuleIndex {
    nodes: Vec<RuleNode>,
    edges: Vec<RuleEdge>,
}

impl RuleIndex {
    fn new(rules: &[RewriteRule]) -> Result<Self> {
        struct PendingNode {
            node: usize,
            rules: std::ops::Range<usize>,
            prefix_start: usize,
        }

        let mut index = Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        if rules.is_empty() {
            return Ok(index);
        }
        index.nodes.push(RuleNode {
            exemplar: 0,
            prefix_end: 0,
            terminal: None,
            children: 0..0,
        });
        let mut pending = vec![PendingNode {
            node: 0,
            rules: 0..rules.len(),
            prefix_start: 0,
        }];
        while let Some(task) = pending.pop() {
            let group = rules.get(task.rules.clone()).ok_or(StreamError::Invalid)?;
            let first = &group.first().ok_or(StreamError::Invalid)?.from;
            let last = &group.last().ok_or(StreamError::Invalid)?.from;
            // In lexical order the first and last keys determine the prefix
            // shared by every key between them.
            let shared = first
                .get(task.prefix_start..)
                .ok_or(StreamError::Invalid)?
                .iter()
                .zip(last.get(task.prefix_start..).ok_or(StreamError::Invalid)?)
                .take_while(|(left, right)| left == right)
                .count();
            let prefix_end = task.prefix_start + shared;
            let terminal = (first.len() == prefix_end).then_some(task.rules.start);
            let children_start = index.edges.len();

            // Matching keys at one position can only be prefixes of each
            // other. The shortest is lexically first and shadows every longer
            // key below it, so terminal nodes need no outgoing edges.
            if terminal.is_none() {
                let mut start = task.rules.start;
                while start < task.rules.end {
                    let byte = *rules
                        .get(start)
                        .and_then(|rule| rule.from.get(prefix_end))
                        .ok_or(StreamError::Invalid)?;
                    let count = rules
                        .get(start..task.rules.end)
                        .ok_or(StreamError::Invalid)?
                        .iter()
                        .take_while(|rule| rule.from.get(prefix_end) == Some(&byte))
                        .count();
                    let end = start + count;
                    let node = index.nodes.len();
                    index.nodes.push(RuleNode {
                        exemplar: start,
                        prefix_end,
                        terminal: None,
                        children: 0..0,
                    });
                    index.edges.push(RuleEdge { byte, node });
                    pending.push(PendingNode {
                        node,
                        rules: start..end,
                        prefix_start: prefix_end,
                    });
                    start = end;
                }
            }
            *index.nodes.get_mut(task.node).ok_or(StreamError::Invalid)? = RuleNode {
                exemplar: task.rules.start,
                prefix_end,
                terminal,
                children: children_start..index.edges.len(),
            };
        }
        Ok(index)
    }

    fn find(
        &self,
        rules: &[RewriteRule],
        input: &[u8],
        mut examined: impl FnMut(usize),
    ) -> Result<Option<usize>> {
        let mut node = 0;
        let mut prefix_start = 0;
        loop {
            let current = self.nodes.get(node).ok_or(StreamError::Invalid)?;
            let prefix = rules
                .get(current.exemplar)
                .and_then(|rule| rule.from.get(prefix_start..current.prefix_end))
                .ok_or(StreamError::Invalid)?;
            // The observer compiles away for production calls. Tests use this
            // conservative byte-comparison bound to guard shared-prefix cost.
            examined(prefix.len());
            if !input
                .get(prefix_start..)
                .is_some_and(|rest| rest.starts_with(prefix))
            {
                return Ok(None);
            }
            if let Some(rule) = current.terminal {
                return Ok(Some(rule));
            }
            let Some(byte) = input.get(current.prefix_end) else {
                return Ok(None);
            };
            let children = self
                .edges
                .get(current.children.clone())
                .ok_or(StreamError::Invalid)?;
            let edge = children.binary_search_by(|edge| {
                examined(1);
                edge.byte.cmp(byte)
            });
            let Ok(edge) = edge else {
                return Ok(None);
            };
            node = children.get(edge).ok_or(StreamError::Invalid)?.node;
            prefix_start = current.prefix_end;
        }
    }
}

/// Rewrites original bytes once, preserving matches across chunk boundaries.
pub struct Rewriter {
    rules: Vec<RewriteRule>,
    index: RuleIndex,
    starts: [bool; 256],
    first_bytes: Vec<u8>,
    max_length: usize,
    pending: Vec<u8>,
    position: u64,
    matches: Vec<u64>,
    record_offsets: bool,
    finished: bool,
    failed: bool,
}

impl Rewriter {
    pub fn new(mut rules: Vec<RewriteRule>) -> Result<Self> {
        rules.sort_by(|left, right| left.from.cmp(&right.from));
        if rules
            .iter()
            .any(|rule| rule.from.is_empty() || rule.from.len() != rule.to.len())
            || rules
                .windows(2)
                .any(|pair| pair.first().map(|r| &r.from) == pair.last().map(|r| &r.from))
        {
            return Err(StreamError::Invalid);
        }
        let index = RuleIndex::new(&rules)?;
        let mut starts = [false; 256];
        for rule in &rules {
            let first = *rule.from.first().ok_or(StreamError::Invalid)?;
            *starts
                .get_mut(usize::from(first))
                .ok_or(StreamError::Invalid)? = true;
        }
        let max_length = rules.iter().map(|rule| rule.from.len()).max().unwrap_or(1);
        let first_bytes = (0u8..=255)
            .filter(|byte| starts.get(usize::from(*byte)).copied().unwrap_or(false))
            .collect();
        Ok(Self {
            rules,
            index,
            starts,
            first_bytes,
            max_length,
            pending: Vec::new(),
            position: 0,
            matches: Vec::new(),
            record_offsets: false,
            finished: false,
            failed: false,
        })
    }

    /// Emit borrowed unchanged spans and replacements; `finish` emits the suffix.
    ///
    /// After a callback error or panic, further writes fail without invoking
    /// the callback. Already emitted bytes cannot be rolled back.
    pub fn feed(
        &mut self,
        bytes: &[u8],
        finish: bool,
        mut emit: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if self.failed {
            return Err(StreamError::Failed);
        }
        self.failed = true;
        self.feed_inner(bytes, finish, &mut emit)?;
        self.failed = false;
        Ok(())
    }

    fn retain(buffer: &mut Vec<u8>, bytes: &[u8], capacity_limit: usize) -> Result<()> {
        let required = buffer
            .len()
            .checked_add(bytes.len())
            .ok_or(StreamError::Invalid)?;
        if required > capacity_limit {
            return Err(StreamError::Invalid);
        }
        if required > buffer.capacity() {
            // Keep amortized growth for small feeds, but cap it before Vec's
            // default doubling can exceed the two-suffix allocation bound.
            // No rule-sized allocation is made until input needs that space.
            let target = required
                .max(buffer.capacity().saturating_mul(2))
                .min(capacity_limit);
            buffer.reserve_exact(target - buffer.len());
        }
        buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn feed_inner(
        &mut self,
        bytes: &[u8],
        finish: bool,
        emit: &mut impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if self.finished {
            return if finish && bytes.is_empty() {
                Ok(())
            } else {
                Err(StreamError::Failed)
            };
        }
        if self.rules.is_empty() {
            self.position = self
                .position
                .checked_add(u64::try_from(bytes.len()).map_err(|_| StreamError::Invalid)?)
                .ok_or(StreamError::Invalid)?;
            self.finished = finish;
            return if bytes.is_empty() {
                Ok(())
            } else {
                emit(bytes)
            };
        }

        let carry_limit = self.max_length - 1;
        let capacity_limit = carry_limit.checked_mul(2).ok_or(StreamError::Invalid)?;
        let mut input_offset = 0;
        if !self.pending.is_empty() {
            // Resolve only starts in the previous suffix. Even a whole NAR
            // feed copies at most this suffix plus max_length - 1 new bytes.
            let mut boundary = std::mem::take(&mut self.pending);
            let old_length = boundary.len();
            let prefix_length = bytes.len().min(carry_limit);
            Self::retain(
                &mut boundary,
                bytes.get(..prefix_length).ok_or(StreamError::Invalid)?,
                capacity_limit,
            )?;
            let limit = if finish && prefix_length == bytes.len() {
                boundary.len()
            } else {
                boundary.len().saturating_sub(carry_limit).min(old_length)
            };
            let consumed = self.process(&boundary, limit, emit)?;
            if consumed < old_length {
                // Not enough new bytes to decide all old starts. All input
                // is in boundary, and its undecidable suffix remains bounded.
                boundary.drain(..consumed);
                self.pending = boundary;
                return Ok(());
            }
            input_offset = consumed - old_length;
            boundary.clear();
            self.pending = boundary;
        }

        let remaining = bytes.get(input_offset..).ok_or(StreamError::Invalid)?;
        let limit = if finish {
            remaining.len()
        } else {
            remaining.len().saturating_sub(carry_limit)
        };
        let consumed = self.process(remaining, limit, emit)?;
        Self::retain(
            &mut self.pending,
            remaining.get(consumed..).ok_or(StreamError::Invalid)?,
            capacity_limit,
        )?;
        self.finished = finish;
        Ok(())
    }

    /// Process decidable starts, borrowing literals directly from input. A
    /// match may consume beyond limit, but never beyond the original slice.
    fn process(
        &mut self,
        bytes: &[u8],
        limit: usize,
        emit: &mut impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<usize> {
        let mut consumed = 0;
        let mut literal_start = 0;
        while consumed < limit {
            let searchable = bytes.get(consumed..limit).ok_or(StreamError::Invalid)?;
            let next = match self.first_bytes.as_slice() {
                [first] => memchr::memchr(*first, searchable),
                [first, second] => memchr::memchr2(*first, *second, searchable),
                [first, second, third] => memchr::memchr3(*first, *second, *third, searchable),
                _ => searchable.iter().position(|byte| {
                    self.starts
                        .get(usize::from(*byte))
                        .copied()
                        .unwrap_or(false)
                }),
            }
            .unwrap_or(searchable.len());
            consumed += next;
            if consumed == limit {
                break;
            }
            let remaining = bytes.get(consumed..).ok_or(StreamError::Invalid)?;
            let rule = if self.rules.len() == 1 {
                self.rules
                    .first()
                    .filter(|rule| remaining.starts_with(&rule.from))
            } else {
                match self.index.find(&self.rules, remaining, |_| {})? {
                    Some(index) => Some(self.rules.get(index).ok_or(StreamError::Invalid)?),
                    None => None,
                }
            };
            if let Some(rule) = rule {
                if literal_start < consumed {
                    emit(
                        bytes
                            .get(literal_start..consumed)
                            .ok_or(StreamError::Invalid)?,
                    )?;
                }
                if self.record_offsets {
                    self.matches.push(
                        self.position
                            .checked_add(u64::try_from(consumed).map_err(|_| StreamError::Invalid)?)
                            .ok_or(StreamError::Invalid)?,
                    );
                }
                emit(&rule.to)?;
                consumed += rule.from.len();
                literal_start = consumed;
            } else {
                consumed += 1;
            }
        }
        if literal_start < consumed {
            emit(
                bytes
                    .get(literal_start..consumed)
                    .ok_or(StreamError::Invalid)?,
            )?;
        }
        self.position = self
            .position
            .checked_add(u64::try_from(consumed).map_err(|_| StreamError::Invalid)?)
            .ok_or(StreamError::Invalid)?;
        Ok(consumed)
    }
}

/// Digest selected independently of its external ABI encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Blake3,
}

enum Hasher {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(Box<sha2::Sha512>),
    Blake3(Box<blake3::Hasher>),
}

impl Hasher {
    fn new(algorithm: HashAlgorithm) -> Self {
        match algorithm {
            HashAlgorithm::Md5 => Self::Md5(md5::Md5::new()),
            HashAlgorithm::Sha1 => Self::Sha1(sha1::Sha1::new()),
            HashAlgorithm::Sha256 => Self::Sha256(sha2::Sha256::new()),
            HashAlgorithm::Sha512 => Self::Sha512(Box::new(sha2::Sha512::new())),
            HashAlgorithm::Blake3 => Self::Blake3(Box::default()),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Md5(hash) => hash.update(bytes),
            Self::Sha1(hash) => hash.update(bytes),
            Self::Sha256(hash) => hash.update(bytes),
            Self::Sha512(hash) => hash.update(bytes),
            Self::Blake3(hash) => {
                hash.update(bytes);
            }
        }
    }

    fn finish(&self) -> Vec<u8> {
        match self {
            Self::Md5(hash) => hash.clone().finalize().to_vec(),
            Self::Sha1(hash) => hash.clone().finalize().to_vec(),
            Self::Sha256(hash) => hash.clone().finalize().to_vec(),
            Self::Sha512(hash) => hash.as_ref().clone().finalize().to_vec(),
            Self::Blake3(hash) => hash.finalize().as_bytes().to_vec(),
        }
    }
}

/// Hashes zeroed self-references followed by their original byte offsets.
pub struct ModuloHasher {
    rewriter: Rewriter,
    hasher: Hasher,
    digest: Option<Vec<u8>>,
}

impl ModuloHasher {
    /// Original bytes consumed, excluding the self-reference offset trailer.
    pub fn input_bytes(&self) -> u64 {
        self.rewriter.position
    }

    pub fn new(algorithm: HashAlgorithm, modulus: &[u8]) -> Result<Self> {
        let mut rewriter = Rewriter::new(vec![RewriteRule {
            from: modulus.to_vec(),
            to: vec![0; modulus.len()],
        }])?;
        rewriter.record_offsets = true;
        Ok(Self {
            rewriter,
            hasher: Hasher::new(algorithm),
            digest: None,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.rewriter.feed(bytes, false, |output| {
            self.hasher.update(output);
            Ok(())
        })
    }

    /// Finish once and return the same digest on subsequent calls.
    pub fn finish(&mut self) -> Result<&[u8]> {
        if self.digest.is_none() {
            self.rewriter.feed(&[], true, |output| {
                self.hasher.update(output);
                Ok(())
            })?;
            // Bind actual occurrences to their original offsets. A literal
            // zero-filled region cannot masquerade as a self-reference.
            for position in &self.rewriter.matches {
                self.hasher.update(b"|");
                self.hasher.update(position.to_string().as_bytes());
            }
            self.digest = Some(self.hasher.finish());
        }
        self.digest.as_deref().ok_or(StreamError::Failed)
    }
}

#[cfg(test)]
mod tests;
