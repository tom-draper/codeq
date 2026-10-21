(* Analysis tools. *)
open List
include Shared
module Alias = Other
module type S = sig
  type t
  val apply : int -> int
end
type point = { x : int; y : int }
exception Invalid of string
let double x = x * 2
let name = "point"
class counter = object
  method next x = x + 1
end
