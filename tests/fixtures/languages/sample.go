package sample

import (
	"fmt"
	"strings"
)

// Limit bounds a circle.
const Limit = 10

type Shape interface {
	Name() string
}

type Circle struct {
	Radius float64
	label  string
}

func (c Circle) Name() string {
	return strings.ToUpper("circle")
}

func Describe(s Shape) {
	fmt.Println(s.Name())
}
