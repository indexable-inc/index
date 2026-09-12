//! Deterministic attribute suggestions with constant-size edit-distance rows.

const MAX_DISTANCE: u8 = 2;
const OUTSIDE: u8 = MAX_DISTANCE + 1;
const WIDTH: usize = 2 * MAX_DISTANCE as usize + 1;
const LIMIT: usize = 5;

#[derive(Clone, Copy)]
struct Row {
    start: usize,
    values: [u8; WIDTH],
}

impl Row {
    fn at(&self, column: usize) -> u8 {
        column
            .checked_sub(self.start)
            .and_then(|index| self.values.get(index))
            .copied()
            .unwrap_or(OUTSIDE)
    }
}

/// Byte distance matches attribute-name ordering and needs no per-name UTF-8
/// allocation. Only the width-five diagonal band can contain a distance <= 2.
fn near_distance(query: &[u8], candidate: &[u8]) -> Option<u8> {
    if query.len().abs_diff(candidate.len()) > usize::from(MAX_DISTANCE) {
        return None;
    }
    let mut previous = Row {
        start: 0,
        values: [OUTSIDE; WIDTH],
    };
    for (column, value) in previous.values.iter_mut().enumerate() {
        if column <= candidate.len() && column <= usize::from(MAX_DISTANCE) {
            *value = u8::try_from(column).ok()?;
        }
    }
    for (offset, query_byte) in query.iter().enumerate() {
        let row = offset + 1;
        let start = row.saturating_sub(usize::from(MAX_DISTANCE));
        let end = candidate.len().min(row + usize::from(MAX_DISTANCE));
        let mut current = Row {
            start,
            values: [OUTSIDE; WIDTH],
        };
        let mut left = OUTSIDE;
        let mut minimum = OUTSIDE;
        for (offset, value) in current.values.iter_mut().enumerate() {
            let column = start + offset;
            if column > end {
                break;
            }
            *value = if column == 0 {
                u8::try_from(row).ok()?.min(OUTSIDE)
            } else {
                // column <= candidate.len(), so the preceding byte exists.
                let substitution = u8::from(query_byte != candidate.get(column - 1)?);
                (previous.at(column) + 1)
                    .min(left + 1)
                    .min(previous.at(column - 1) + substitution)
                    .min(OUTSIDE)
            };
            left = *value;
            minimum = minimum.min(*value);
        }
        if minimum > MAX_DISTANCE {
            return None;
        }
        previous = current;
    }
    let distance = previous.at(candidate.len());
    (distance <= MAX_DISTANCE).then_some(distance)
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Match<'a> {
    distance: u8,
    name: &'a str,
}

/// Keep only the five best names, ordered by distance then name. Iteration
/// order has no effect, and names are borrowed without forcing their values.
pub(crate) fn best_matches<'a>(
    query: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Vec<&'a str> {
    let mut best = Vec::with_capacity(LIMIT);
    for name in names {
        let Some(distance) = near_distance(query.as_bytes(), name.as_bytes()) else {
            continue;
        };
        let found = Match { distance, name };
        let index = best.partition_point(|existing| existing < &found);
        if best.get(index) == Some(&found) {
            continue;
        }
        if index < LIMIT {
            best.insert(index, found);
            best.truncate(LIMIT);
        }
    }
    best.into_iter().map(|found| found.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_distance(left: &[u8], right: &[u8]) -> usize {
        let mut row: Vec<usize> = (0..=right.len()).collect();
        for (i, a) in left.iter().enumerate() {
            let mut diagonal = i;
            let mut previous = i + 1;
            for (b, cell) in right.iter().zip(row.iter_mut().skip(1)) {
                let old = *cell;
                *cell = (old + 1)
                    .min(previous + 1)
                    .min(diagonal + usize::from(a != b));
                diagonal = old;
                previous = *cell;
            }
            if let Some(first) = row.first_mut() {
                *first = i + 1;
            }
        }
        row.last().copied().unwrap_or(left.len())
    }

    #[test]
    fn bounded_rows_match_full_distance_for_all_short_words() {
        let mut words = vec![String::new()];
        for length in 1..=5 {
            for bits in 0..(1 << length) {
                words.push(
                    (0..length)
                        .map(|bit| if bits & (1 << bit) == 0 { 'a' } else { 'b' })
                        .collect(),
                );
            }
        }
        for left in &words {
            for right in &words {
                let exact = exact_distance(left.as_bytes(), right.as_bytes());
                assert_eq!(
                    near_distance(left.as_bytes(), right.as_bytes()).map(usize::from),
                    (exact <= 2).then_some(exact),
                    "{left:?} / {right:?}"
                );
            }
        }
    }

    #[test]
    fn ranks_top_five_independently_of_enumeration_order() {
        let names = ["unrelated", "fooba", "xoo", "fox", "fooo", "fo"];
        let expected = vec!["fo", "fooo", "fox", "xoo", "fooba"];
        assert_eq!(best_matches("foo", names), expected);
        assert_eq!(best_matches("foo", names.into_iter().rev()), expected);
        assert!(best_matches("zzzznotarealname", ["alpha", "beta", "gamma"]).is_empty());
    }

    #[test]
    fn five_thousand_names_still_keep_only_top_five() {
        let names: Vec<String> = (0..5000).map(|i| format!("attr{i}")).collect();
        let forward = best_matches("attr499x", names.iter().map(String::as_str));
        assert_eq!(forward.len(), LIMIT);
        assert_eq!(
            forward,
            best_matches("attr499x", names.iter().rev().map(String::as_str))
        );
        assert_eq!(forward.first(), Some(&"attr499"));
    }

    #[test]
    fn awkward_and_long_names_preserve_bytes_without_unbounded_rows() {
        assert!(best_matches("ab", ["", "a b", "c\nd", "é"]).contains(&"a b"));
        assert_eq!(best_matches("é", ["é", "è", "e"]), vec!["é", "è", "e"]);
        let long = "x".repeat(100_000);
        assert_eq!(near_distance(long.as_bytes(), long.as_bytes()), Some(0));
        assert_eq!(near_distance(long.as_bytes(), b"short"), None);
    }
}
