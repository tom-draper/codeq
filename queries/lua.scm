; Lua: named functions, incl. `M.f` (@function) and `obj:m` (@method). `require`
; is a plain call, so imports aren't captured.

(function_declaration name: (identifier) @name body: (block) @body) @function
(function_declaration name: (dot_index_expression) @name body: (block) @body) @function
(function_declaration name: (method_index_expression) @name body: (block) @body) @method

; `require("mod")` is a call, not syntax, so it is guarded on the callee name the
; same way Ruby's `require` is.
(function_call
  name: (identifier) @guard
  arguments: (arguments . (string (string_content) @name))) @import
