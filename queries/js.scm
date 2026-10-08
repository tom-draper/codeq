; JavaScript definitions and ES imports. Node types here also exist in the
; TypeScript grammar, so this compiles against JS, TS, and TSX. `name: (_)`
; avoids depending on grammar-specific identifier node types.

(function_declaration name: (_) @name body: (statement_block) @body) @function
(generator_function_declaration name: (_) @name body: (statement_block) @body) @function
(class_declaration name: (_) @name body: (class_body) @body) @class
(method_definition name: (_) @name body: (statement_block) @body) @method
(variable_declarator name: (_) @name value: (arrow_function body: (_) @body)) @function
(variable_declarator name: (_) @name value: (function_expression body: (_) @body)) @function

; A top-level binding is a definition in its own right. The two patterns above
; only see a *directly* function-valued one, and a great deal of JS/TS that is
; conceptually a definition is not written that way: a React component wrapped in
; `memo(…)` or `forwardRef(…)`, a `styled.div\`…\``, a `createContext(…)`, a
; router from `express.Router()`, or a plain `export const VERSION = '1.0'`. All
; of those were invisible to every symbol command.
;
; Anchored to `(program …)` so only module-scope bindings count — matching every
; `variable_declarator` would bury the index in loop counters and locals.
; Destructuring (`const { a } = …`) is excluded by `name: (identifier)`: it binds
; several names at one site and none of them is declared there.
;
; `@val` is the bound value. The extractor uses it to drop the match when the
; value is already captured as `@function` above, and to keep the signature
; readable when it spans several lines.
(program
  (lexical_declaration
    (variable_declarator name: (identifier) @name value: (_) @val)) @constant)
(program
  (variable_declaration
    (variable_declarator name: (identifier) @name value: (_) @val)) @constant)
(program
  (export_statement
    (lexical_declaration
      (variable_declarator name: (identifier) @name value: (_) @val)) @constant))
(program
  (export_statement
    (variable_declaration
      (variable_declarator name: (identifier) @name value: (_) @val)) @constant))

; Pre-ES6 JavaScript declares a method by assigning to a property —
; `proto.use = function use(fn) {…}`, `Thing.prototype.render = function () {…}`,
; `exports.helper = function () {…}` — and none of it was a definition. Express's
; entire router API (`proto.param`/`handle`/`process_params`/`use`/`route`) was
; invisible to `symbols`, and that is also why a call at that scope had no
; enclosing definition for a `callers` row to name: the row came back blank.
;
; Module scope only, the same rule the `@constant` patterns above follow — a
; `this.x = function(){}` in a constructor, or any local object literal, would
; otherwise bury the index.
;
; The name is the property (`use`), not the path to it (`proto.use`): it is what
; the call site writes, and it is the choice made for a schema-qualified SQL
; table too. Common property names therefore collide across files — scope with
; `--path`, as with any name-based lookup.
(program
  (expression_statement
    (assignment_expression
      left: (member_expression property: (property_identifier) @name)
      right: (function_expression body: (statement_block) @body)) @method))
(program
  (expression_statement
    (assignment_expression
      left: (member_expression property: (property_identifier) @name)
      right: (generator_function body: (statement_block) @body)) @method))
(program
  (expression_statement
    (assignment_expression
      left: (member_expression property: (property_identifier) @name)
      right: (arrow_function body: (_) @body)) @method))

; ES module imports and re-exports (raw module string, quotes stripped).
(import_statement source: (string (string_fragment) @name)) @import
(export_statement source: (string (string_fragment) @name)) @import
; Named imports `import { a, b [as c] } from './x'` — qualified `./x.a` per name
; (the source name, not the alias, picks the exporting file) so the imported
; binding resolves. The coarse module capture above still covers default/namespace.
(import_statement
  (import_clause (named_imports (import_specifier name: (identifier) @from_name)))
  source: (string (string_fragment) @from_module))
(import_statement
  (import_clause (named_imports (import_specifier name: (identifier) @from_name alias: (identifier) @binding)))
  source: (string (string_fragment) @from_module))
; CommonJS require("x") — a call, not a syntactic import, so it's guarded by the
; callee name (`require`) via the @guard handling in `extract`.
(call_expression
  function: (identifier) @guard
  arguments: (arguments . (string (string_fragment) @name))) @import
