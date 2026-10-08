; Ruby definitions and `require`/`require_relative` imports. Ruby has no import
; statement — `require "x"` is a plain method call — so imports are captured as
; calls guarded by the method name (see the `@guard` handling in `extract`).
; Ruby method/class bodies aren't a strippable field here, so signatures are
; the whole declaration, capped.

(method name: (_) @name) @method
(singleton_method name: (_) @name) @method
(class name: (_) @name) @class
(module name: (_) @name) @module
(call
  method: (identifier) @guard
  arguments: (argument_list . (string (string_content) @name))) @import

; `attr_accessor :a, :b` — Ruby declares fields through a method call, so each
; symbol argument becomes a field. Guarded on the attr_* family.
(call
  method: (identifier) @guard
  arguments: (argument_list (simple_symbol) @name)) @field
