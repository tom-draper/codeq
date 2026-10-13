// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import "./Named.sol";

interface INamed {
    function name() external view returns (string memory);
}

contract Shape {
    uint256 public sides;
}

// A circle is a named shape.
contract Circle is Shape, INamed {
    uint256 public radius;

    enum Colour { Red, Green }

    struct Point {
        uint256 x;
    }

    event Renamed(string label);

    function name() external pure returns (string memory) {
        return label("circle");
    }

    function label(string memory text) internal pure returns (string memory) {
        return text;
    }
}
