; Kotlin: classes/objects/functions and imports. The grammar exposes no body
; field, so signatures are the (capped) declaration, like Ruby.

; `class A : B(), C` — a delegation specifier is a base class or interface;
; Kotlin does not distinguish them syntactically, so both read as `extends`.
(class_declaration name: (identifier) @sub
  (delegation_specifiers (delegation_specifier
    (constructor_invocation (user_type (identifier) @extends)))))
(class_declaration name: (identifier) @sub
  (delegation_specifiers (delegation_specifier
    (user_type (identifier) @extends))))
