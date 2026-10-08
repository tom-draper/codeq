; `marginalia` is this grammar's name for a `/* … */` block comment; `comment` is
; the `--` line form. String spans are *not* here: the grammar gives every scalar
; the same `literal` node, so a query cannot tell `'admin'` from `42` — see
; `sql_extras`.

(comment) @comment
(marginalia) @comment
