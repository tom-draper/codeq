; Dart: top-level functions, methods, classes/mixins/enums, and imports.

; `class A extends B implements C` — the two *are* distinguishable.
(class_declaration name: (identifier) @sub
  (superclass (type (type_identifier) @extends)))
(class_declaration name: (identifier) @sub
  (interfaces (type (type_identifier) @implements)))
