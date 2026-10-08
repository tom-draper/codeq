; Scala: defs, classes/objects/traits/enums, type aliases, and imports.

; `class A extends B with C` — the extends clause lists every ancestor.
(class_definition name: (identifier) @sub
  (extends_clause (type_identifier) @extends))
(trait_definition name: (identifier) @sub
  (extends_clause (type_identifier) @extends))
(object_definition name: (identifier) @sub
  (extends_clause (type_identifier) @extends))
