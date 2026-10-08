; Elixir: modules and functions. These are macro `call`s (`defmodule`/`def`/
; `defp`), so the `def*` keyword is captured as a `@guard` that `extract` filters
; on (an ordinary call like `import Foo` is rejected). `defmodule` names an
; `alias`; `def`/`defp` name the inner call's (or bare) identifier.

; `alias Foo.Bar`, `import Baz`, `use Mod` — macro calls, like the def* family.
(call target: (identifier) @guard (arguments (alias) @name)) @import

(call target: (identifier) @guard (arguments (alias) @name)) @module
(call target: (identifier) @guard (arguments (call target: (identifier) @name))) @function
(call target: (identifier) @guard (arguments (identifier) @name)) @function
