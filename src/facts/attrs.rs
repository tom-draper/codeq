//! Attributes, decorators and annotations: what marks a definition as a test,
//! and what may be registering it somewhere codeq cannot see.

use tree_sitter::Node;

use super::collapse_ws;

/// Whether a definition carries a test attribute — the structural answer to a
/// question codeq had only ever answered from spelling.
///
/// Three shapes, because grammars disagree about where an annotation lives:
/// Rust puts `attribute_item` *before* the item as a sibling, Python wraps the
/// definition in a `decorated_definition` *above* it, and Java/Kotlin/C#/PHP hang
/// `modifiers`/`attribute_list` *inside* the declaration. All three are checked;
/// a language with none of them simply yields nothing, which leaves the existing
/// path- and name-based signals to do the work as before.
pub(super) fn test_attributed(lang: &str, node: Node, src: &[u8]) -> bool {
    let attrs = attributes_of(node, src);
    !attrs.is_empty() && is_test_marker(lang, &attrs.join("\n"))
}

/// Every attribute / decorator / annotation attached to a definition, as written.
///
/// Three shapes, because grammars disagree about where one lives: Rust puts
/// `attribute_item` *before* the item as a sibling, Python wraps the definition
/// in a `decorated_definition` *above* it, and Java/Kotlin/C#/PHP hang
/// `modifiers`/`attribute_list` *inside* the declaration. A language with none of
/// them yields nothing, which is a true answer rather than a failure.
fn attributes_of(node: Node, src: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |n: Node| {
        if let Ok(t) = n.utf8_text(src) {
            out.push(collapse_ws(t));
        }
    };

    // Rust: preceding siblings, innermost first — collected in reverse so the
    // list reads in source order.
    let mut before: Vec<Node> = Vec::new();
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if !p.kind().contains("attribute") {
            break;
        }
        before.push(p);
        prev = p.prev_named_sibling();
    }
    for n in before.into_iter().rev() {
        push(n);
    }
    // Python: on a wrapper node above the definition.
    if let Some(parent) = node.parent() {
        if parent.kind().contains("decorated") {
            let mut c = parent.walk();
            for ch in parent.named_children(&mut c) {
                if ch.kind() == "decorator" {
                    push(ch);
                }
            }
        }
    }
    // Java / Kotlin / C# / PHP: inside the declaration. Matched on the *list*
    // node each grammar wraps annotations in (`attribute_list` in C# and PHP,
    // `modifiers` in Java and Kotlin), never on a bare `attribute` — because in
    // Python that kind is attribute *access*, so `self._axes_visible = True`
    // came back tagged `decorator=self._axes_visible`. The `decorator` field
    // exists so a reader can tell a framework entry point from dead code, and a
    // tag on an ordinary assignment is worse than no tag: it means the tag no
    // longer means anything when it appears.
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        let k = ch.kind();
        if k == "modifiers" {
            // Java and Kotlin put annotations and access modifiers in one node,
            // so pushing it whole reported `decorator=public` on every plain
            // `public class` — a tag that says "a framework may be registering
            // this" on a declaration nothing registers. Take the annotations.
            let mut mc = ch.walk();
            for m in ch.named_children(&mut mc) {
                if m.kind().contains("annotation") {
                    push(m);
                }
            }
        } else if k == "attributes"
            || k.contains("annotation")
            || k.ends_with("attribute_list")
            || k.ends_with("attribute_group")
        {
            push(ch);
        }
    }
    out
}

/// The decorator that might be *registering* this definition somewhere codeq
/// cannot see — a FastAPI `@app.get(...)`, a `@click.command()`, a plugin
/// `@register`. `None` when the definition carries none, or carries only inert
/// ones.
///
/// This exists because `unused` was 100% noise on a decorator-driven codebase: a
/// route's only "reference" is the line directly above it, so every entry point
/// in every Python web app, JS decorator framework, plugin registry and `click`
/// CLI was reported as dead. The row is *tagged*, not suppressed — suppressing
/// anything decorated would hide real dead code, which is the failure `unused`
/// exists to avoid, whereas `decorator=@app.get` lets a reader discount it in one
/// glance. Same call codeq makes for `on=` receivers: show it, don't filter on it.
///
/// The inert list is the small half of the judgement. `@property`,
/// `@staticmethod`, `#[derive(Debug)]` and friends change how a definition
/// behaves; they do not hand it to anything. Tagging those would put a
/// meaningless annotation on most rows and cost the signal this adds.
pub(super) fn registering_decorator(node: Node, src: &[u8]) -> Option<String> {
    // Whole namespaces that only ever change how a definition behaves. Nothing
    // in `functools`, `contextlib`, `typing`, `abc`, `dataclasses` or `enum`
    // hands a definition to a framework, so the module answers for all of them.
    const INERT_MODULES: &[&str] = &[
        "functools",
        "contextlib",
        "typing",
        "abc",
        "dataclasses",
        "enum",
        "serde",
    ];
    const INERT: &[&str] = &[
        "property",
        "setter",
        "getter",
        "deleter",
        "staticmethod",
        "classmethod",
        "abstractmethod",
        "cached_property",
        "dataclass",
        "overload",
        "override",
        "wraps",
        "final",
        // Imported bare as often as they are qualified — `from functools import
        // cache` — so the module list alone would miss them.
        "cache",
        "lru_cache",
        "singledispatch",
        "singledispatchmethod",
        "total_ordering",
        "contextmanager",
        "asynccontextmanager",
        "runtime_checkable",
        "no_type_check",
        "unique",
        "LocalProxy",
        // Wrap or mark a definition without registering it: Click's context
        // injection, Flask's setup guard, a deprecation marker.
        "deprecated",
        "pass_context",
        "pass_obj",
        "make_pass_decorator",
        "setupmethod",
        "functools",
        "derive",
        "inline",
        "repr",
        "allow",
        "deny",
        "warn",
        "cfg",
        "must_use",
        "non_exhaustive",
        "doc",
        "Serializable",
        "Override",
        "Deprecated",
        "NotNull",
        "Nullable",
        "SuppressWarnings",
        "Obsolete",
    ];
    attributes_of(node, src).into_iter().find(|a| {
        // The bare name, minus the syntax each language wraps it in, so
        // `@app.get("/x")`, `#[derive(Debug)]` and `[Fact]` are all comparable.
        let head = a
            .trim_start_matches(['@', '#', '[', ']', ' '])
            .split(['(', '[', ' ', '=', ':'])
            .next()
            .unwrap_or("");
        // `@functools.cache` is neither `functools` nor `cache` as written, so
        // the dotted name has to be judged by its parts: the module it came
        // from and the name it binds.
        let module = head.split('.').next().unwrap_or(head);
        let last = head.rsplit('.').next().unwrap_or(head);
        !head.is_empty()
            && !INERT.contains(&head)
            && !INERT.contains(&last)
            && !INERT_MODULES.contains(&module)
    })
}

/// Does this attribute text name a test? A fixed per-language list rather than a
/// pattern, because "contains the word test" would catch `#[cfg(test)]` on a
/// module (which marks a *container*, already handled) and `@TestOnly` (which
/// marks production code). Deliberately covers the setup/teardown annotations
/// too: those are test scaffolding, which is what `unused` and `untested` need to
/// know, and they share this predicate with `tests`.
fn is_test_marker(lang: &str, attrs: &str) -> bool {
    match lang {
        // `#[test]`, `#[bench]`, and the framework forms — including any path
        // ending in `::test`, which covers `#[tokio::test]`, `#[async_std::test]`
        // and every other runtime's wrapper without listing them.
        "rust" => {
            attrs.contains("[test]")
                || attrs.contains("::test]")
                || attrs.contains("[bench]")
                || [
                    "rstest",
                    "proptest",
                    "quickcheck",
                    "test_case",
                    "googletest",
                ]
                .iter()
                .any(|m| attrs.contains(m))
        }
        // `@pytest.mark.…`, `@pytest.fixture`, `@unittest.…`, bare `@parametrize`.
        "python" => [
            "pytest.",
            "unittest",
            "parametrize",
            "@fixture",
            "hypothesis",
        ]
        .iter()
        .any(|m| attrs.contains(m)),
        "java" | "kotlin" | "scala" | "csharp" | "php" => [
            "@Test",
            "@ParameterizedTest",
            "@RepeatedTest",
            "@TestFactory",
            "@BeforeEach",
            "@AfterEach",
            "@BeforeAll",
            "@AfterAll",
            "@Before",
            "@After",
            "[Test]",
            "[TestMethod]",
            "[Fact]",
            "[Theory]",
            "[SetUp]",
            "[TearDown]",
            "[TestCase]",
            "#[Test]",
        ]
        .iter()
        .any(|m| attrs.contains(m)),
        _ => false,
    }
}
