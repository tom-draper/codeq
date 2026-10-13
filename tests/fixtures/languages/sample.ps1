# The label is fixed.
function Get-Label {
    param([string]$Text)
    return $Text.Trim()
}

class Circle {
    [double]$Radius

    [string] Name() {
        return (Get-Label "circle")
    }
}

Get-Label "x"
