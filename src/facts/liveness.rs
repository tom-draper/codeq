//! Definitions that are alive without a reference: declared for something
//! outside the repository, read by a runtime, or registered by a framework
//! convention. `unused` and `entrypoints` consult these.

/// Declarations that exist to be consumed from *outside* the repository, so
/// "nothing here references it" is their normal state rather than a finding.
/// `None` covers a whole language; `Some(kind)` narrows to one kind within it.
///
/// A table here rather than a hand-written clause in `cmd_unused`'s SQL, which
/// is where this policy used to live. It has grown once per language with an
/// external contract — GraphQL, protobuf, Terraform outputs, now SQL — and that
/// is a category that keeps appearing, so it belongs next to everything else
/// that describes a language instead of inside a query string in another file.
pub const UNREFERENCED_BY_DESIGN: &[(&str, Option<&str>)] = &[
    // A GraphQL type or a protobuf message is a contract for another codebase.
    ("graphql", None),
    ("proto", None),
    // A Terraform `output` exists precisely so something outside can read it.
    ("hcl", Some("output")),
    // A SQL index or trigger is named exactly once in the world, at its own
    // `CREATE`, and an enum variant is referenced by data rather than by code.
    // The rest of SQL is not here: a table, view, column or function *is* named
    // by whatever queries it, and codeq finds those names inside the SQL strings
    // in application code, so an unreferenced one is a real finding.
    ("sql", Some("index")),
    ("sql", Some("trigger")),
    ("sql", Some("variant")),
];

/// Names the *runtime* reads and code never references. `__all__` is declared in
/// every Python package that curates its exports and named nowhere else, by
/// construction — the same sentence that already excludes SQL indexes and
/// triggers. A short list rather than a blanket dunder rule: `__version__` *is*
/// referenced, routinely, and hiding it would be the opposite mistake.
pub const RUNTIME_READ_NAMES: &[(&str, &str)] = &[
    ("python", "__all__"),
    ("python", "__path__"),
    ("python", "__getattr__"),
];

/// Python protocol and framework hooks that execute through the interpreter,
/// dataclasses, or Pydantic rather than an ordinary name lookup. Keep this
/// explicit: `__version__` and arbitrary dunder names remain candidates unless
/// codeq observes a real use.
pub const PYTHON_RUNTIME_HOOKS: &[&str] = &[
    "__aenter__",
    "__aexit__",
    "__aiter__",
    "__anext__",
    "__await__",
    "__bool__",
    "__bytes__",
    "__call__",
    "__class_getitem__",
    "__contains__",
    "__delattr__",
    "__delitem__",
    "__delete__",
    "__dir__",
    "__enter__",
    "__eq__",
    "__exit__",
    "__format__",
    "__get__",
    "__get_pydantic_json_schema__",
    "__get_validators__",
    "__getattribute__",
    "__getitem__",
    "__hash__",
    "__init__",
    "__init_subclass__",
    "__iter__",
    "__len__",
    "__new__",
    "__post_init__",
    "__repr__",
    "__reversed__",
    "__set__",
    "__set_name__",
    "__setattr__",
    "__setitem__",
    "__slots__",
    "__str__",
];

/// JavaScript-family constructors are invoked as `new TypeName()`, not by the
/// method name `constructor`. A name-based reference scan therefore cannot
/// distinguish an instantiated class from one whose constructor is dead; the
/// class itself remains eligible for `unused`.
pub const UNREFERENCED_CONSTRUCTORS: &[(&str, &str, &str)] = &[
    ("javascript", "method", "constructor"),
    ("typescript", "method", "constructor"),
    ("tsx", "method", "constructor"),
];

/// Whether `unused` should stay silent about a definition because nothing in
/// this repository was ever going to reference it. See [`UNREFERENCED_BY_DESIGN`].
pub fn unreferenced_by_design(language: Option<&str>, kind: &str, name: &str) -> bool {
    let Some(lang) = language else { return false };
    UNREFERENCED_BY_DESIGN
        .iter()
        .any(|(l, k)| *l == lang && k.is_none_or(|k| k == kind))
        || RUNTIME_READ_NAMES.iter().any(|(l, n)| *l == lang && *n == name)
        || (lang == "python" && PYTHON_RUNTIME_HOOKS.contains(&name))
        // A Jinja block is filled by template inheritance and dispatched by the
        // renderer. There is no source-level call site for an override, so it
        // is an entry point rather than a dead definition.
        || (lang == "jinja" && kind == "block")
        || UNREFERENCED_CONSTRUCTORS
            .iter()
            .any(|(l, k, n)| *l == lang && *k == kind && *n == name)
}

/// Languages that can carry a JavaScript framework's file conventions.
const WEB_LANGUAGES: &[&str] = &["javascript", "typescript", "tsx", "jsx"];

/// React class-component methods the runtime calls. The class names them in no
/// other place, and unlike a route there is no decorator to notice.
const REACT_LIFECYCLE: &[&str] = &[
    "render",
    "componentDidMount",
    "componentDidUpdate",
    "componentWillUnmount",
    "shouldComponentUpdate",
    "getSnapshotBeforeUpdate",
    "componentDidCatch",
    "getDerivedStateFromError",
    "getDerivedStateFromProps",
];

/// The framework that reaches this definition without any call naming it, or
/// `None`.
///
/// [`Fact::decorator`] covers registration codeq can *see* — `@app.get("/x")`
/// sits on the line above. A file convention leaves no such mark: Next.js loads
/// the default export of `app/**/page.tsx` because of where the file sits and
/// what it is called, so on an App Router repo every route, layout and metadata
/// export read as dead code and `entrypoints` reported none at all.
///
/// Tag, not suppress. Deleting a `page.tsx` export still compiles — the route
/// 404s — so this is the "looks dead, might not be" case the `decorator=` tag
/// exists for, and not the never-dead case of a trait implementation.
///
/// A convention file's whole public surface is the framework's contract
/// (`metadata` and `viewport` beside the component), and codeq does not record
/// which export was `default`, so the public surface is what gets tagged.
pub fn framework_registration(
    language: Option<&str>,
    kind: &str,
    name: &str,
    path: &str,
    visibility: Option<&str>,
) -> Option<&'static str> {
    if !language.is_some_and(|l| WEB_LANGUAGES.contains(&l)) {
        return None;
    }
    if kind == "method" && REACT_LIFECYCLE.contains(&name) {
        return Some("react/lifecycle");
    }
    // Everything below is a module-level export the framework loads by path.
    if visibility != Some("public") {
        return None;
    }
    let mut segments: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let file = segments.pop()?;
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let dirs = segments;

    // `middleware.ts` and `instrumentation.ts` are only conventional at the
    // project root, where `src/` is the one directory Next.js also allows.
    if dirs.is_empty() || dirs == ["src"] {
        match stem {
            "middleware" => return Some("next/middleware"),
            "instrumentation" => return Some("next/instrumentation"),
            _ => {}
        }
    }
    // The Pages Router routes *every* file under `pages/`, so the file name
    // carries no further information.
    if dirs.contains(&"pages") {
        return Some("next/pages-router");
    }
    if !dirs.contains(&"app") {
        return None;
    }
    Some(match stem {
        "page" => "next/page",
        "layout" => "next/layout",
        "route" => "next/route",
        "template" => "next/template",
        "loading" => "next/loading",
        "error" | "global-error" => "next/error",
        "not-found" => "next/not-found",
        "default" => "next/default",
        "sitemap" => "next/sitemap",
        "robots" => "next/robots",
        "manifest" => "next/manifest",
        "icon" | "apple-icon" | "opengraph-image" | "twitter-image" => "next/metadata-image",
        _ => return None,
    })
}
