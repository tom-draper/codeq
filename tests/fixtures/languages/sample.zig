const std = @import("std");

pub const limit = 10;

const Circle = struct {
    radius: f64,
    sides: u8,

    pub fn name(self: Circle) []const u8 {
        _ = self;
        return label();
    }
};

const Colour = enum { red, green };

// The label is fixed.
fn label() []const u8 {
    return "circle";
}

pub fn main() void {
    std.debug.print("{s}\n", .{label()});
}
