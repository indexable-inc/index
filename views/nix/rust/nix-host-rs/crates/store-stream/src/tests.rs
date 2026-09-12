use super::*;

fn rule(from: &[u8], to: &[u8]) -> RewriteRule {
    RewriteRule {
        from: from.to_vec(),
        to: to.to_vec(),
    }
}

fn rewrite(chunks: &[&[u8]]) -> (Vec<u8>, Vec<u64>, u64) {
    let mut writer =
        Rewriter::new(vec![rule(b"abc", b"xyz"), rule(b"xyz", b"123")]).expect("rules");
    writer.record_offsets = true;
    let mut output = Vec::new();
    for chunk in chunks {
        writer
            .feed(chunk, false, |bytes| {
                output.extend_from_slice(bytes);
                Ok(())
            })
            .expect("feed");
    }
    writer
        .feed(&[], true, |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("finish");
    (output, writer.matches, writer.position)
}

#[test]
fn rewrite_is_independent_of_every_pair_of_chunk_boundaries() {
    let input = b"0abc1xyz2abc3ab";
    let expected = (
        b"0xyz11232xyz3ab".to_vec(),
        vec![1, 5, 9],
        input.len() as u64,
    );
    for first in 0..=input.len() {
        for second in first..=input.len() {
            assert_eq!(
                rewrite(&[&input[..first], &input[first..second], &input[second..]]),
                expected
            );
        }
    }
    let one_byte: Vec<&[u8]> = input.chunks(1).collect();
    assert_eq!(rewrite(&one_byte), expected);
}

#[test]
fn rewrite_uses_original_bytes_and_never_rewrites_its_carry() {
    assert_eq!(rewrite(&[b"abc", b"!"]).0, b"xyz!");
    assert_eq!(rewrite(&[b"abc!"]).0, b"xyz!");
    // The C++ carry retained transformed bytes and could change xyz to 123
    // when the same original input arrived in multiple writes.
}

#[test]
fn overlapping_rules_have_explicit_leftmost_lexicographic_precedence() {
    let mut writer = Rewriter::new(vec![rule(b"ab", b"XY"), rule(b"a", b"Z")]).expect("rules");
    writer.record_offsets = true;
    let mut output = Vec::new();
    writer
        .feed(b"abab", true, |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("feed");
    assert_eq!(output, b"ZbZb");
    assert_eq!(writer.matches, [0, 2]);
    assert_eq!(writer.position, 4);
}

#[test]
fn replacement_is_not_reentered_even_when_it_contains_the_key() {
    let mut writer = Rewriter::new(vec![rule(b"aa", b"aa")]).expect("rules");
    writer.record_offsets = true;
    let mut output = Vec::new();
    writer
        .feed(b"aaaaa", true, |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("feed");
    assert_eq!(output, b"aaaaa");
    assert_eq!(writer.matches, [0, 2]);
}

#[test]
fn empty_rule_set_streams_every_byte_and_invalid_rules_are_rejected() {
    let mut writer = Rewriter::new(Vec::new()).expect("empty rules");
    let mut output = Vec::new();
    writer
        .feed(b"abc\0\xff", false, |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("feed");
    assert_eq!(output, b"abc\0\xff");
    assert_eq!(writer.position, 5);
    assert!(Rewriter::new(vec![rule(b"", b"")]).is_err());
    assert!(Rewriter::new(vec![rule(b"a", b"bb")]).is_err());
    assert!(Rewriter::new(vec![rule(b"a", b"b"), rule(b"a", b"c")]).is_err());
}

const FIRST: &[u8; 32] = b"dc04vv14dak1c1r48qa0m23vr9jy8sm0";
const SECOND: &[u8; 32] = b"zc842j0rz61mjsp3h3wp5ly71ak6qgdn";

#[test]
fn scanner_handles_all_boundaries_binary_input_and_duplicate_occurrences() {
    let hashes = [FIRST.as_slice(), SECOND.as_slice()].concat();
    let data = [
        b"\0\xffprefix".as_slice(),
        FIRST,
        b"xyz",
        SECOND,
        FIRST,
        b"\0",
    ]
    .concat();
    for split in 0..=data.len() {
        let mut scanner = RefScanner::new(&hashes).expect("hashes");
        scanner.feed(&data[..split]);
        scanner.feed(&[]);
        scanner.feed(&data[split..]);
        scanner.found.sort_unstable();
        assert_eq!(scanner.found, [0, 1]);
    }
    let mut scanner = RefScanner::new(&hashes).expect("hashes");
    for byte in &data {
        scanner.feed(std::slice::from_ref(byte));
    }
    scanner.found.sort_unstable();
    assert_eq!(scanner.found, [0, 1]);
}

#[test]
fn scanner_rejects_malformed_candidates_and_does_not_match_partial_hashes() {
    assert!(RefScanner::new(b"short").is_err());
    assert!(RefScanner::new(&[b'e'; 32]).is_err());
    assert!(RefScanner::new(&[FIRST.as_slice(), FIRST.as_slice()].concat()).is_err());
    let mut scanner = RefScanner::new(FIRST).expect("hash");
    scanner.feed(&FIRST[..31]);
    assert!(scanner.found.is_empty());
    scanner.feed(b"!");
    scanner.feed(&FIRST[31..]);
    assert!(scanner.found.is_empty());
}

fn modulo(algorithm: HashAlgorithm, chunks: &[&[u8]], modulus: &[u8]) -> (Vec<u8>, u64) {
    let mut hasher = ModuloHasher::new(algorithm, modulus).expect("modulus");
    for chunk in chunks {
        hasher.feed(chunk).expect("feed");
    }
    let digest = hasher.finish().expect("finish").to_vec();
    assert_eq!(hasher.finish().expect("idempotent finish"), digest);
    (digest, hasher.rewriter.position)
}

#[test]
fn modulo_hash_binds_original_positions_and_excludes_trailer_from_size() {
    let input = b"xabc\0abc!";
    let zeroed_and_offsets = b"x\0\0\0\0\0\0\0!|1|5";
    for algorithm in [
        HashAlgorithm::Md5,
        HashAlgorithm::Sha1,
        HashAlgorithm::Sha256,
        HashAlgorithm::Sha512,
        HashAlgorithm::Blake3,
    ] {
        let mut expected = Hasher::new(algorithm);
        expected.update(zeroed_and_offsets);
        for split in 0..=input.len() {
            assert_eq!(
                modulo(algorithm, &[&input[..split], &input[split..]], b"abc"),
                (expected.finish(), 9)
            );
        }
        assert_ne!(
            modulo(algorithm, &[b"abc"], b"abc").0,
            modulo(algorithm, &[b"\0\0\0"], b"abc").0
        );
        assert_ne!(
            modulo(algorithm, &[b"abc\0\0\0"], b"abc").0,
            modulo(algorithm, &[b"\0\0\0abc"], b"abc").0
        );
    }
}

#[test]
fn modulo_without_matches_is_the_plain_digest_and_overlaps_are_not_double_counted() {
    assert_eq!(
        modulo(HashAlgorithm::Sha256, &[b"abc"], b"missing").0,
        sha2::Sha256::digest(b"abc").to_vec()
    );
    // Only offset 0 is replaced: overlapping occurrences at 1 and 2 must
    // not be counted, even when a chunk boundary falls inside the match.
    let actual = modulo(HashAlgorithm::Sha256, &[b"aa", b"aaa"], b"aaa");
    assert_eq!(actual.0, sha2::Sha256::digest(b"\0\0\0aa|0").to_vec());
    assert!(ModuloHasher::new(HashAlgorithm::Sha256, b"").is_err());
}

#[derive(Debug, PartialEq, Eq)]
struct RewriteObservation {
    bytes: Vec<u8>,
    offsets: Vec<u64>,
    input_bytes: u64,
}

fn rewrite_oracle(input: &[u8], rules: &[RewriteRule]) -> RewriteObservation {
    let mut ordered: Vec<&RewriteRule> = rules.iter().collect();
    ordered.sort_by(|left, right| left.from.cmp(&right.from));
    let mut observation = RewriteObservation {
        bytes: Vec::new(),
        offsets: Vec::new(),
        input_bytes: input.len() as u64,
    };
    let mut position = 0;
    while position < input.len() {
        if let Some(rule) = ordered
            .iter()
            .find(|rule| input[position..].starts_with(&rule.from))
        {
            observation.offsets.push(position as u64);
            observation.bytes.extend_from_slice(&rule.to);
            position += rule.from.len();
        } else {
            observation.bytes.push(input[position]);
            position += 1;
        }
    }
    observation
}

#[test]
fn borrowed_rewrite_matches_oracle_for_every_partition_of_overlap_and_binary_cases() {
    let cases = [
        (
            b"abababcab".as_slice(),
            vec![rule(b"ab", b"XY"), rule(b"abc", b"123"), rule(b"bc", b"uv")],
        ),
        (
            b"aaaaaaaaa".as_slice(),
            vec![rule(b"aaaa", b"zzzz"), rule(b"aaa", b"yyy")],
        ),
        (
            b"abxyzab!".as_slice(),
            vec![rule(b"ab", b"xy"), rule(b"xyz", b"123")],
        ),
        (
            b"\0\xffx\0\xff\0x".as_slice(),
            vec![rule(b"\0\xff", b"xy"), rule(b"x", b"x")],
        ),
        (
            b"acabbabbe".as_slice(),
            vec![
                rule(b"bb", b"BB"),
                rule(b"ac", b"AC"),
                rule(b"ab", b"AB"),
                rule(b"ba", b"BA"),
            ],
        ),
        (
            b"abacabab".as_slice(),
            vec![
                rule(b"abac", b"1234"),
                rule(b"ab", b"XY"),
                rule(b"ac", b"ZZ"),
            ],
        ),
        (b"!a!a!a!".as_slice(), vec![rule(b"a", b"b")]),
    ];
    for (input, rules) in cases {
        let expected = rewrite_oracle(input, &rules);
        for cuts in 0..(1usize << (input.len() - 1)) {
            let mut writer =
                Rewriter::new(rules.iter().map(|r| rule(&r.from, &r.to)).collect()).expect("rules");
            writer.record_offsets = true;
            let mut actual = Vec::new();
            let mut start = 0;
            for end in 1..=input.len() {
                if end == input.len() || cuts & (1 << (end - 1)) != 0 {
                    writer
                        .feed(&input[start..end], false, |bytes| {
                            actual.extend_from_slice(bytes);
                            Ok(())
                        })
                        .expect("chunk");
                    writer.feed(&[], false, |_| Ok(())).expect("empty chunk");
                    assert!(writer.pending.len() < writer.max_length);
                    start = end;
                }
            }
            writer
                .feed(&[], true, |bytes| {
                    actual.extend_from_slice(bytes);
                    Ok(())
                })
                .expect("finish");
            assert_eq!(
                RewriteObservation {
                    bytes: actual,
                    offsets: writer.matches,
                    input_bytes: writer.position,
                },
                expected,
                "input {input:?}, partition {cuts}"
            );
        }
    }
}

#[test]
fn large_feed_borrows_literals_and_retains_only_a_rule_length_suffix() {
    let mut input = vec![b'!'; 8192];
    input[4096..4128].copy_from_slice(FIRST);
    let mut writer = Rewriter::new(vec![rule(FIRST, &[0; 32])]).expect("rule");
    let mut output = Vec::new();
    let mut borrowed_literal_bytes = 0;
    writer
        .feed(&input, false, |bytes| {
            if bytes.first() == Some(&b'!') {
                let offset = if output.is_empty() { 0 } else { 4128 };
                assert_eq!(bytes.as_ptr(), input[offset..].as_ptr());
                borrowed_literal_bytes += bytes.len();
            }
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("large feed");
    assert_eq!(borrowed_literal_bytes, input.len() - 32 - 31);
    assert_eq!(writer.pending.len(), 31);
    assert!(writer.pending.capacity() <= 62);
    writer
        .feed(b"!", true, |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .expect("finish boundary");
    assert!(writer.pending.is_empty());
    assert!(writer.pending.capacity() <= 62);
    let mut expected = input;
    expected[4096..4128].fill(0);
    expected.push(b'!');
    assert_eq!(output, expected);
}

#[test]
fn callback_failure_after_a_borrowed_prefix_poisons_every_later_write() {
    let mut writer =
        Rewriter::new(vec![rule(b"abc", b"xyz"), rule(b"abd", b"123")]).expect("rules");
    let mut accepted = Vec::new();
    let mut calls = 0;
    assert_eq!(
        writer.feed(b"!abc?", true, |bytes| {
            calls += 1;
            if calls == 2 {
                return Err(StreamError::Sink);
            }
            accepted.extend_from_slice(bytes);
            Ok(())
        }),
        Err(StreamError::Sink)
    );
    assert_eq!(accepted, b"!");
    for finish in [false, true] {
        assert_eq!(
            writer.feed(&[], finish, |_| {
                calls += 1;
                Ok(())
            }),
            Err(StreamError::Failed)
        );
    }
    assert_eq!(calls, 2);
}

#[test]
fn callback_failure_on_boundary_or_empty_rules_also_poisons_the_stream() {
    for rules in [vec![rule(b"abc", b"xyz")], Vec::new()] {
        let mut writer = Rewriter::new(rules).expect("rules");
        writer
            .feed(b"ab", false, |_| Ok(()))
            .expect("initial chunk");
        assert_eq!(
            writer.feed(b"c!", true, |_| Err(StreamError::Sink)),
            Err(StreamError::Sink)
        );
        assert_eq!(
            writer.feed(b"more", true, |_| panic!("poisoned callback")),
            Err(StreamError::Failed)
        );
    }
}

#[test]
fn modulo_digest_and_offsets_match_oracle_across_every_partition() {
    for (input, modulus) in [
        (b"aaaaaaaa".as_slice(), b"aaa".as_slice()),
        (b"xabcabc!".as_slice(), b"abc".as_slice()),
        (b"\0ab\0ab!".as_slice(), b"ab".as_slice()),
    ] {
        let expected = rewrite_oracle(input, &[rule(modulus, &vec![0; modulus.len()])]);
        let mut encoded = expected.bytes;
        for offset in expected.offsets {
            encoded.extend_from_slice(format!("|{offset}").as_bytes());
        }
        let digest = sha2::Sha256::digest(&encoded).to_vec();
        for cuts in 0..(1usize << (input.len() - 1)) {
            let mut hasher = ModuloHasher::new(HashAlgorithm::Sha256, modulus).expect("modulus");
            let mut start = 0;
            for end in 1..=input.len() {
                if end == input.len() || cuts & (1 << (end - 1)) != 0 {
                    hasher.feed(&input[start..end]).expect("chunk");
                    assert!(hasher.rewriter.pending.len() < modulus.len());
                    start = end;
                }
            }
            assert_eq!(hasher.finish().expect("digest"), digest);
            assert_eq!(hasher.input_bytes(), input.len() as u64);
        }
    }
}

#[test]
fn callback_panic_leaves_the_safe_stream_poisoned() {
    let mut writer =
        Rewriter::new(vec![rule(b"abc", b"xyz"), rule(b"abd", b"123")]).expect("rules");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = writer.feed(b"!abc", true, |_| panic!("injected sink panic"));
    }));
    assert!(result.is_err());
    assert_eq!(
        writer.feed(&[], true, |_| panic!("poisoned callback")),
        Err(StreamError::Failed)
    );
}

fn shared_prefix_rules(count: usize, prefix_length: usize) -> Vec<RewriteRule> {
    (0..count)
        .map(|index| {
            let mut key = vec![b'a'; prefix_length];
            key.extend_from_slice(&(index as u64).to_be_bytes());
            rule(&key, &vec![b'B'; key.len()])
        })
        .collect()
}

#[test]
fn shared_prefix_lookup_cost_depends_on_key_length_not_candidate_count() {
    for count in [1, 32, 1024, 4096] {
        let rules = shared_prefix_rules(count, 24);
        let index = RuleIndex::new(&rules).expect("index");
        let miss = [b'a'; 32];
        for input in [
            &rules.first().expect("first").from[..],
            &rules.last().expect("last").from[..],
            &miss,
        ] {
            let expected = rules.iter().position(|rule| input.starts_with(&rule.from));
            let mut comparisons = 0;
            let actual = index
                .find(&rules, input, |bytes| comparisons += bytes)
                .expect("lookup");
            assert_eq!(actual, expected);
            // Includes complete compressed spans and binary-search byte
            // probes. The old 4096-candidate bucket needed over 98,000 shared
            // prefix comparisons for this miss; the bound stays constant.
            assert!(
                comparisons <= 64,
                "{count} rules examined {comparisons} bytes"
            );
        }
    }
}

#[test]
fn radix_byte_edges_resolve_binary_keys_and_terminal_prefixes() {
    let mut rules = vec![rule(b"ab", b"XY"), rule(b"abcd", b"WXYZ")];
    for first in 0..=7u8 {
        for second in 0..=7u8 {
            rules.push(rule(&[first, second], &[b'Z'; 2]));
        }
    }
    rules.sort_by(|left, right| left.from.cmp(&right.from));
    let index = RuleIndex::new(&rules).expect("index");
    for first in 0..=255u8 {
        for second in [0, 7, 8, b'b', 255] {
            let input = [first, second, b'c', b'd'];
            let expected = rules.iter().position(|rule| input.starts_with(&rule.from));
            assert_eq!(
                index.find(&rules, &input, |_| {}).expect("lookup"),
                expected
            );
        }
    }
}

#[test]
fn indexed_binary_rules_match_oracle_across_small_and_large_feeds() {
    let rules = shared_prefix_rules(1024, 24);
    let mut input = vec![b'a'; 65];
    input.extend_from_slice(&rules.last().expect("last").from);
    input.push(b'!');
    input.extend_from_slice(&rules.first().expect("first").from);
    input.extend_from_slice(b"aaaa!");
    let expected = rewrite_oracle(&input, &rules);
    for size in [1, 3, 31, 32, 33, 4096] {
        let mut writer =
            Rewriter::new(rules.iter().map(|r| rule(&r.from, &r.to)).collect()).expect("rules");
        writer.record_offsets = true;
        let mut output = Vec::new();
        for chunk in input.chunks(size) {
            writer
                .feed(chunk, false, |bytes| {
                    output.extend_from_slice(bytes);
                    Ok(())
                })
                .expect("feed");
            assert!(writer.pending.len() < writer.max_length);
            assert!(writer.pending.capacity() <= 2 * (writer.max_length - 1));
        }
        writer
            .feed(&[], true, |bytes| {
                output.extend_from_slice(bytes);
                Ok(())
            })
            .expect("finish");
        assert_eq!(
            RewriteObservation {
                bytes: output,
                offsets: writer.matches,
                input_bytes: writer.position
            },
            expected
        );
    }
}

#[test]
fn carry_allocation_is_lazy_amortized_and_strictly_bounded() {
    let mut writer =
        Rewriter::new(vec![rule(&vec![b'a'; 4096], &vec![b'b'; 4096])]).expect("long rule");
    assert_eq!(writer.pending.capacity(), 0);
    let mut growths = 0;
    for length in 1..=128 {
        let previous = writer.pending.capacity();
        writer.feed(b"x", false, |_| Ok(())).expect("small feed");
        if writer.pending.capacity() != previous {
            growths += 1;
        }
        assert_eq!(writer.pending.len(), length);
        assert!(writer.pending.capacity() <= 2 * length);
    }
    assert!(growths <= 8, "small feeds must not allocate for every byte");

    // Vec's default minimum allocation can itself exceed the complete carry
    // budget of short rules. Exact reservation must cover this case too.
    for length in 1..=8 {
        for chunk_size in 1..=8 {
            let mut writer = Rewriter::new(vec![rule(&vec![b'a'; length], &vec![b'b'; length])])
                .expect("short rule");
            for chunk in [b'x'; 32].chunks(chunk_size) {
                writer.feed(chunk, false, |_| Ok(())).expect("feed");
                assert!(writer.pending.len() < length);
                assert!(writer.pending.capacity() <= 2 * (length - 1));
            }
        }
    }
}

#[test]
fn radix_storage_compresses_long_prefixes_and_traverses_deep_branches_iteratively() {
    let rules = shared_prefix_rules(64, 256);
    let index = RuleIndex::new(&rules).expect("index");
    assert!(index.nodes.len() <= 2 * rules.len());
    assert!(index.edges.len() < index.nodes.len());

    let mut rules: Vec<RewriteRule> = (0..128)
        .map(|length| {
            let mut key = vec![b'a'; length];
            key.push(b'b');
            rule(&key, &vec![b'Z'; key.len()])
        })
        .collect();
    rules.sort_by(|left, right| left.from.cmp(&right.from));
    let index = RuleIndex::new(&rules).expect("deep index");
    for (expected, rule) in rules.iter().enumerate() {
        assert_eq!(
            index.find(&rules, &rule.from, |_| {}).expect("deep lookup"),
            Some(expected)
        );
    }
    // A terminal prefix shadows longer keys irrespective of construction
    // order; the public rewriter sorts before creating its index.
    let writer = Rewriter::new(vec![
        rule(b"abcd", b"1234"),
        rule(b"a", b"Z"),
        rule(b"ab", b"XY"),
    ])
    .expect("prefixes");
    assert_eq!(writer.index.nodes.len(), 1);
    assert_eq!(
        writer
            .index
            .find(&writer.rules, b"abcd", |_| {})
            .expect("prefix lookup"),
        Some(0)
    );
}
