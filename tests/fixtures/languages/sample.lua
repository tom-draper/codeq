local json = require("json")

local M = {}

-- The label is fixed.
local function label(text)
  return text .. "!"
end

function M.describe(shape)
  return label("circle")
end

function M:name()
  return json.encode(self)
end

return M
