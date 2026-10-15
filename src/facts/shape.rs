//! A definition's syntax shape, the hash `clones` compares.

use tree_sitter::Node;

/// Minimum nodes before a shape is recorded at all. Below this every trivial
/// definition in a repository collides — truthfully, but uselessly — so the
/// cheapest place to stop is at extraction.
const MIN_SHAPE_NODES: usize = 12;

/// A definition's syntax shape: the tree of node *kinds*, with identifiers and
/// literals normalised away, hashed. Two definitions sharing a shape are the same
/// code up to renaming.
///
/// This deliberately answers a yes/no question rather than producing a similarity
/// score. A score would need a threshold nobody can justify, and "0.87" tells an
/// agent nothing it can act on — whereas "these four functions are structurally
/// identical" is checkable, and `diff` covers the rest. A cursor walks the tree
/// without recursion or a child allocation at every node.
pub(super) fn shape_of(node: Node) -> (Option<String>, i64) {
    let mut h = blake3::Hasher::new();
    let mut count = 0usize;
    let mut cursor = node.walk();
    'walk: loop {
        h.update(&cursor.node().kind_id().to_le_bytes());
        h.update(b"(");
        count += 1;
        // Anonymous children include punctuation and operators, so visit them
        // too. Close markers preserve the distinction between nested siblings.
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            h.update(b")");
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    if count < MIN_SHAPE_NODES {
        return (None, count as i64);
    }
    (Some(h.finalize().to_hex()[..16].to_string()), count as i64)
}

#[cfg(test)]
mod tests {
    use super::shape_of;

    #[test]
    fn shapes_keep_persisted_hashes_and_stop_at_the_requested_subtree() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser
            .parse("fn alpha(x: i32) -> bool { x != 3 }\nfn beta() {}\n", None)
            .unwrap();
        let root = tree.root_node();
        assert_eq!(
            shape_of(root.named_child(0).unwrap()),
            (Some("f4f1d6980b43d870".into()), 19)
        );
        assert_eq!(shape_of(root.named_child(1).unwrap()), (None, 9));

        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("class Box:\n    def first(self, x):\n        if x == 2:\n            return x + 1\n        return 0\n\ndef tail():\n    pass\n", None).unwrap();
        let class = tree.root_node().named_child(0).unwrap();
        assert_eq!(shape_of(class), (Some("924b1aca1dec5a53".into()), 33));
        let method = class
            .child_by_field_name("body")
            .unwrap()
            .named_child(0)
            .unwrap();
        assert_eq!(shape_of(method), (Some("e2afe351f39cca03".into()), 28));
    }
}
