# Basic analysis tools.
module Analysis
using Statistics: mean
include("helpers.jl")
struct Point{T}
    x::T
end
abstract type AbstractPoint end
primitive type Bits 8 end
function average(xs)
    mean(xs)
end
scale(x::T) where T = x * 2
macro greet(x)
    :(println($x))
end
end
