; Foreign keys — the one genuinely structural edge a schema has, and the reason
; to index SQL beyond finding declarations. Deliberately *not* `implements` or
; `extends`: a foreign key is not inheritance, and forcing it into that
; vocabulary would make `supertypes` claim an is-a relationship that does not
; exist. It gets its own relation kind, `references`, so the two commands read
; directionally — `implementors orgs --relation references` is what points at
; `orgs`, `supertypes orders --relation references` is what `orders` points at.
;
; Three syntactic homes for the same edge, all of which real migrations use: an
; inline column `REFERENCES`, a table-level `CONSTRAINT … FOREIGN KEY` inside
; the `CREATE`, and an `ALTER TABLE … ADD CONSTRAINT` afterwards. Missing the
; third would lose exactly the foreign keys a migrations directory adds later,
; which is the same trap `ALTER TABLE … ADD COLUMN` set for members.

(create_table (keyword_table) . (object_reference) @sub
  (column_definitions
    (column_definition (keyword_references) . (object_reference) @references)))
(create_table (keyword_table) . (object_reference) @sub
  (column_definitions
    (constraints (constraint (keyword_references) . (object_reference) @references))))
(alter_table (keyword_table) . (object_reference) @sub
  (add_constraint
    (constraint (keyword_references) . (object_reference) @references)))
