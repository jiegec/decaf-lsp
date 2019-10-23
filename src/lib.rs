use common;
use tower_lsp::lsp_types::*;

pub fn pos(loc: &common::Loc) -> Position {
    Position {
        line: loc.0 as u64 - 1,
        character: loc.1 as u64 - 1,
    }
}

pub fn range(loc: &common::Loc) -> Range {
    Range {
        start: pos(loc),
        end: pos(loc),
    }
}

pub fn range2(loc: &common::Loc, end: &common::Loc) -> Range {
    Range {
        start: pos(loc),
        end: pos(end),
    }
}
