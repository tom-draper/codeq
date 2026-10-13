defmodule Shapes.Circle do
  import Enum
  alias Shapes.Shape
  require Logger

  @limit 10

  # The label is fixed.
  def name(circle) do
    label("circle")
  end

  defp label(text) do
    String.trim(text)
  end

  defmacro traced(expr) do
    quote do: unquote(expr)
  end
end
