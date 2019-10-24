use common;
use syntax;
use tower_lsp::lsp_types::*;

pub fn pos(loc: &common::Loc) -> Position {
    if loc.0 == 0 || loc.1 == 0 {
        Position {
            line: 0,
            character: 0,
        }
    } else {
        Position {
            line: loc.0 as u64 - 1,
            character: loc.1 as u64 - 1,
        }
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

pub fn token(token: &syntax::parser::Token) -> Range {
    Range {
        start: Position {
            line: token.line as u64 - 1,
            character: token.col as u64 - 1,
        },
        end: Position {
            line: token.line as u64 - 1,
            character: (token.col as u64 + token.piece.len() as u64) - 1 - 1,
        },
    }
}
