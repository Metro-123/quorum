//! SQL front end: lexer, AST, and a recursive-descent parser.

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::Statement;
pub use parser::parse;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_statements() {
        let stmts = parse("SELECT 1; SELECT 2;").unwrap();
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("SELEKT 1").is_err());
        assert!(parse("").is_err());
    }
}
