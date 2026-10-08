; Swift: functions, class/struct/enum/actor (all `class_declaration`), protocols,
; and imports. `name:` is pinned to the identifier node because a
; `function_declaration` also exposes its return type under a `name` field.

; `class A: B, C` — a base class and a protocol look the same here.
(class_declaration name: (type_identifier) @sub
  (inheritance_specifier (user_type (type_identifier) @extends)))
