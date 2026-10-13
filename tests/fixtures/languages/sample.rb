require 'json'
require_relative 'helper'

module Shapes
  LIMIT = 10

  class Shape
    attr_reader :sides

    def name
      'shape'
    end
  end

  # A circle is a shape.
  class Circle < Shape
    def area
      LIMIT * 3
    end

    def self.build
      Circle.new.name
    end
  end
end
