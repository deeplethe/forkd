//! What may be used as a snapshot tag.
//!
//! A tag becomes a directory name on a node, so it has to be a plain name.
//! This lives in one place because the last time it did not, the result was
//! an advisory: `create_sandbox` joined the caller's tag into a path while
//! every sibling handler validated it, and the asymmetry was the bug. The
//! node checks again before it touches the filesystem -- that is the layer
//! that actually stops traversal -- but a bad tag should be refused at the
//! API with a 400 rather than accepted, written to the database, and failed
//! on a node with a message about a missing directory.

/// Whether `tag` may be used as a snapshot tag.
///
/// Deliberately a deny-list of path syntax rather than an allow-list of
/// characters: tags carry user-chosen names and restricting them to ASCII
/// would be a product decision, not a security one. What must not get
/// through is anything that changes which directory a tag names.
pub fn is_safe(tag: &str) -> bool {
    !tag.is_empty()
        && !tag.contains('/')
        && !tag.contains('\\')
        && !tag.contains("..")
        && !tag.starts_with('.')
        && !tag.contains('\0')
        && tag.len() <= 128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_pass() {
        for t in ["python-numpy", "v1.2.3", "feature_branch-2", "模板", "a"] {
            assert!(is_safe(t), "{t:?} should be allowed");
        }
    }

    #[test]
    fn anything_that_renames_the_directory_is_refused() {
        for t in [
            "",
            "..",
            "../etc",
            "a/../../etc/passwd",
            "sub/dir",
            "a\\b",
            ".hidden",
            "a\0b",
            // The 2026-05-17 advisory's shape: a tag that walks out of the
            // snapshot root and names a file the attacker wrote.
            "../../tmp/evil",
        ] {
            assert!(!is_safe(t), "{t:?} must be refused");
        }
    }

    #[test]
    fn a_tag_has_a_length_bound() {
        assert!(is_safe(&"a".repeat(128)));
        assert!(!is_safe(&"a".repeat(129)));
    }
}
