//! Store path hashes rewritten inside a NAR, as make-content-addressed.cc
//! does it: util.cc:rewriteStrings and references.cc:HashModuloSink.

use std::collections::BTreeMap;

use super::hash::{Hash, HashAlgo, hash_bytes};

/// util.cc:rewriteStrings: the pairs in key order, each match replaced and the
/// search resumed where the replacement starts. Every pair keeps the length.
pub fn rewrite_strings(mut s: Vec<u8>, rewrites: &BTreeMap<String, String>) -> Vec<u8> {
    for (from, to) in rewrites {
        let (from, to) = (from.as_bytes(), to.as_bytes());
        assert_eq!(from.len(), to.len());
        if from == to || from.is_empty() {
            continue;
        }
        let mut j = 0;
        while let Some(i) = find(&s[j..], from) {
            j += i;
            s[j..j + to.len()].copy_from_slice(to);
        }
    }
    s
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let first = needle[0];
    let mut at = 0;
    while let Some(i) = hay[at..].iter().position(|&b| b == first) {
        let start = at + i;
        if hay.len() - start < needle.len() {
            return None;
        }
        if &hay[start..start + needle.len()] == needle {
            return Some(start);
        }
        at = start + 1;
    }
    None
}

/// references.cc:HashModuloSink over a whole NAR: the sha256 of `nar` with
/// every `modulus` turned into zero bytes. Nix 2.34.8 records no match
/// positions, so none are hashed after the bytes.
pub fn hash_modulo(nar: &[u8], modulus: &str) -> Hash {
    let zeros = "\0".repeat(modulus.len());
    let rewrites = BTreeMap::from([(modulus.to_string(), zeros)]);
    hash_bytes(HashAlgo::Sha256, &rewrite_strings(nar.to_vec(), &rewrites))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn every_match_is_rewritten() {
        let s = b"xabcyabcabc".to_vec();
        assert_eq!(rewrite_strings(s, &map(&[("abc", "ABC")])), b"xABCyABCABC");
    }

    #[test]
    fn pairs_apply_in_key_order() {
        // "ab" -> "bc" first, then "bc" -> "cd" sees its output
        let s = b"ab".to_vec();
        assert_eq!(
            rewrite_strings(s, &map(&[("bc", "cd"), ("ab", "bc")])),
            b"cd"
        );
    }

    #[test]
    fn a_match_inside_a_replacement_is_found() {
        // util.cc resumes at the start of the replacement, not after it
        let s = b"aaa".to_vec();
        assert_eq!(rewrite_strings(s, &map(&[("aa", "ba")])), b"bba");
    }

    #[test]
    fn hash_modulo_zeroes_the_modulus() {
        let nar = b"..hash..x..hash".to_vec();
        let zeroed = b"..\0\0\0\0..x..\0\0\0\0".to_vec();
        assert_eq!(
            hash_modulo(&nar, "hash"),
            hash_bytes(HashAlgo::Sha256, &zeroed)
        );
    }
}
