#include <stdio.h>
#include "sample.h"

#define LIMIT 10

struct circle {
    double radius;
    int sides;
};

enum colour { RED, GREEN };

typedef struct circle circle_t;

/* The label is fixed. */
static const char *label(void) {
    return "circle";
}

int describe(circle_t *c) {
    printf("%s\n", label());
    return LIMIT;
}
