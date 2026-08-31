//! Typo tolerance for the Library filter box.
//!
//! One edit, and no more. The Library page already holds the whole artist list
//! in memory (`/Artists` is unpaged), so matching over it locally costs no round
//! trip and works with the server down; albums and tracks are never loaded in
//! full and stay exact server matching.
//!
//! **Deliberately an edit budget, not a similarity score.** A score ranks every
//! name in the library and then needs a cut-off chosen by taste, which is how a
//! three-letter query ends up confidently offering something that shares no
//! letters with it. A budget of one edit either matches or it does not, and what
//! it lets through is describable in a sentence: one wrong, missing or extra
//! character.

/// Shortest term that is fuzzy-matched at all.
///
/// One edit inside three characters reaches a large share of any library, so
/// below this the filter stays exact - which costs nothing, since a term that
/// short is answered by the server's own prefix match anyway.
const MIN_TERM_CHARS: usize = 4;

/// Edits allowed. Not a knob: the whole design of this module is that the budget
/// is small enough to reason about, and two edits on a six-letter word is a
/// different feature (see the module docs).
const MAX_EDITS: usize = 1;

/// Edit distance between `term` and the closest prefix of `name` or of one of
/// its words, or `None` if that is further than one edit.
///
/// Prefixes rather than whole words, because a filter box is read while it is
/// still being typed: `kimel` has to reach `Kimera` before the name is finished.
/// Compared as whole words that is two edits (substitute the `l`, then add the
/// missing `ra`), and against prefixes it is the one the user actually made.
///
/// Words as well as the whole name, so a surname matches: the term is compared
/// against `kimera candela`, `kimera` and `candela` separately, and the best
/// result wins.
pub fn prefix_distance(term: &str, name: &str) -> Option<u8> {
    let term: Vec<char> = term.trim().to_lowercase().chars().collect();
    if term.len() < MIN_TERM_CHARS {
        return None;
    }

    let name = name.to_lowercase();
    std::iter::once(name.as_str())
        .chain(name.split_whitespace())
        .filter_map(|candidate| prefix_distance_capped(&term, candidate))
        .min()
}

/// Levenshtein distance from `term` to the closest prefix of `candidate`,
/// abandoned as soon as it cannot come in under `MAX_EDITS`.
fn prefix_distance_capped(term: &[char], candidate: &str) -> Option<u8> {
    // A prefix longer than the term plus the budget cannot be within it, so the
    // rest of the name is not worth aligning against.
    let candidate: Vec<char> = candidate.chars().take(term.len() + MAX_EDITS).collect();

    // Standard two-row Levenshtein. `row[j]` is the distance between the term
    // consumed so far and `candidate[..j]`.
    let mut prev: Vec<usize> = (0..=candidate.len()).collect();
    let mut cur = vec![0usize; candidate.len() + 1];

    for (i, tc) in term.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cc) in candidate.iter().enumerate() {
            let substitute = prev[j] + usize::from(tc != cc);
            cur[j + 1] = substitute.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        // Distances only grow further down the rows, so a row whose every entry
        // is already over budget settles the whole comparison.
        if cur.iter().all(|&d| d > MAX_EDITS) {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }

    // The remaining characters of the candidate are free: the term is being
    // matched against a prefix, not against the whole of it. That is what taking
    // the minimum of the final row expresses.
    prev.iter()
        .min()
        .copied()
        .filter(|&d| d <= MAX_EDITS)
        .map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_edit_reaches_a_name_being_typed() {
        // Substitution mid-word, with the name unfinished.
        assert_eq!(prefix_distance("kimela", "Kimera Candela"), Some(1));
        // Exact prefix, which the server would also have found.
        assert_eq!(prefix_distance("kimera", "Kimera Candela"), Some(0));
        // A later word matches on its own.
        assert_eq!(prefix_distance("candel", "Kimera Candela"), Some(0));
        // Missing and extra characters, one each.
        assert_eq!(prefix_distance("kimra", "Kimera Candela"), Some(1));
        assert_eq!(prefix_distance("kimmera", "Kimera Candela"), Some(1));
    }

    #[test]
    fn two_edits_do_not() {
        assert_eq!(prefix_distance("kimla", "Kimera Candela"), None);
        assert_eq!(prefix_distance("bill", "Kimera Candela"), None);
        // A transposition is two edits under plain Levenshtein, and is left out
        // on purpose rather than by oversight.
        assert_eq!(prefix_distance("kimrea", "Kimera Candela"), None);
    }

    #[test]
    fn short_terms_stay_exact() {
        assert_eq!(prefix_distance("kim", "Kimera Candela"), None);
        assert_eq!(prefix_distance("bil", "Kimera Candela"), None);
    }
}
