use super::Fact;

pub(super) struct Source<'a> {
    pub text: &'a str,
    lines: Vec<usize>,
}

impl<'a> Source<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            lines: crate::search::line_starts_of(text),
        }
    }

    pub fn fact(&self, kind: &str, name: &str, start: usize, end: usize, source: &str) -> Fact {
        let position = |offset| {
            let row = self
                .lines
                .partition_point(|&s| s <= offset)
                .saturating_sub(1);
            (row + 1, offset - self.lines[row])
        };
        let (start_line, start_col) = position(start);
        let (end_line, end_col) = position(end.saturating_sub(1).max(start));
        Fact {
            kind: kind.into(),
            is_derived: false,
            name: name.into(),
            start_byte: start,
            end_byte: end,
            start_line,
            start_col,
            end_line,
            end_col: end_col + usize::from(end > start),
            signature: name.into(),
            doc: None,
            source: source.into(),
            confidence: 1.0,
            visibility: None,
            owner: None,
            shape: None,
            shape_nodes: 0,
            is_test: false,
            decorator: None,
            contract: None,
        }
    }

    pub fn import(&self, target: &str, start: usize, end: usize, source: &str) -> Option<Fact> {
        let target = target.split(['#', '?']).next()?.trim();
        if target.is_empty() || target.starts_with('/') || target.contains([':', '{', '}', '$']) {
            return None;
        }
        let target = if target.starts_with('.') {
            target.to_string()
        } else {
            format!("./{target}")
        };
        Some(self.fact("import", &target, start, end, source))
    }
}
