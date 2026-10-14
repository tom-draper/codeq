#include <string>
#include "sample.h"

namespace shapes {

class Shape {
public:
    virtual std::string name() const = 0;
    int sides = 0;
};

enum class Colour { Red, Green };

// A circle is a shape.
class Circle : public Shape {
public:
    std::string name() const override { return label("circle"); }

private:
    static std::string label(const std::string &text) { return text; }
    double radius = 1.0;
};

int describe(const Shape &s) {
    return s.name().size();
}

}  // namespace shapes

class Widget {
    int hidden;
    void secret() {}
public:
    static void utility() {}
    static int count;
protected:
    struct Nested { void guarded() {} };
};
struct Record {
    int value;
private:
    void internal() {}
public:
    void update() {}
};
