(function_definition name: (identifier) @name body: (block) @body) @function
(class_definition name: (identifier) @name body: (block) @body) @class

; Raw syntactic imports.
(import_statement (dotted_name) @name) @import
(import_statement (aliased_import (dotted_name) @name alias: (identifier) @binding)) @import
; `from X import a, b [as c]` — capture the module and each imported name; the
; extractor combines them into qualified `X.a` imports so the name (which the
; bare module string drops) resolves to its defining file.
(import_from_statement
  module_name: (dotted_name) @from_module
  name: (dotted_name (identifier) @from_name))
(import_from_statement
  module_name: (dotted_name) @from_module
  name: (aliased_import name: (dotted_name (identifier) @from_name) alias: (identifier) @binding))
; `from X import *` (wildcard) and relative `from . import x` — keep the coarse
; module string (no specific name to qualify with).
(import_from_statement module_name: (dotted_name) @name (wildcard_import)) @import
(import_from_statement module_name: (relative_import) @name) @import

; Members. Python declares attributes by assigning them, so both the class-body
; form and `self.x = …` count; the latter is guarded on its receiver so
; `other.x = 1` (mutating somebody else's object) is not a declaration.
; `@field` sits on the *assignment*, not on the class: the owner is resolved by
; walking up from the fact's own node, so capturing the class would look for the
; class's own parent and find no type at all.
(class_definition
  body: (block (expression_statement (assignment left: (identifier) @name) @field)))
(assignment left: (attribute object: (identifier) @guard attribute: (identifier) @name)) @field

; A module-scope binding is a definition, exactly as it is in JS/TS. `DATA_DIR =
; Path(...) / "data"`, a 52-entry `alias` dict, `ADULT_ONLY_FEATURES`, and every
; settings constant in every Python project were indexed as *nothing*:
; `definitions DATA_DIR` answered "no symbol named", with the message documented
; to mean "you misspelled it", while `references` could see the assignment fine.
; It degraded quietly rather than failing — `explore` returned an empty focus and
; no source at all, and `outline` on a constants module listed its functions and
; omitted the dict that is the point of the file.
;
; The two neighbouring cases were already handled, which is what makes this an
; oversight rather than a decision: JS module-scope bindings are definitions (the
; docs argue it at length) and Python *class-body* assignments are already
; extracted as fields. Assignment extraction existed; it just never ran at module
; scope.
;
; Anchored to `(module …)` so only module scope counts — matching every
; `assignment` would bury the index in locals. Destructuring (`a, b = ...`) binds
; several names at one site and is excluded by `left: (identifier)`; an augmented
; assignment (`x += 1`) is a different node and never a declaration.
; `right:`, not `value:` — Python's `assignment` names its fields left/type/right,
; and the annotated form `X: int = 5` is the same node with a `type` as well, so
; one pattern covers both. A bare annotation (`X: int`, no value) has no `right`
; and is deliberately not matched: it declares a type, not a binding.
(module
  (expression_statement
    (assignment left: (identifier) @name right: (_) @val)) @constant)
