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
/// identical" is checkable, and `diff` covers the rest. Walked with an explicit
/// stack: recursion here is bounded by source nesting, which is attacker-controlled.
pub(super) fn shape_of(node: Node) -> (Option<String>, i64) {
    let mut h = blake3::Hasher::new();
    let mut count = 0usize;
    // (node, entered) — the close marker is what makes sibling structure matter,
    // so `f(g(x))` and `f(g, x)` cannot collide.
    let mut stack: Vec<(Node, bool)> = vec![(node, false)];
    // One cursor for the whole walk, repositioned per node, rather than a child
    // `Vec` allocated at every visited node.
    let mut cursor = node.walk();
    while let Some((n, entered)) = stack.pop() {
        if entered {
            h.update(b")");
            continue;
        }
        h.update(&n.kind_id().to_le_bytes());
        h.update(b"(");
        count += 1;
        stack.push((n, true));
        // Anonymous children include punctuation and operators. Keep them in the
        // shape: dropping the operator tokens makes `a != b` and `a == b`
        // indistinguishable even though the two expressions do different work.
        // Pushed last to first so the first child is popped first.
        cursor.reset(n);
        if cursor.goto_last_child() {
            loop {
                stack.push((cursor.node(), false));
                if !cursor.goto_previous_sibling() {
                    break;
                }
            }
        }
    }
    if count < MIN_SHAPE_NODES {
        return (None, count as i64);
    }
    (Some(h.finalize().to_hex()[..16].to_string()), count as i64)
}
