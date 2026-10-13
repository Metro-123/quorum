//! Recursive-descent parser: tokens -> AST.

use crate::ast::*;
use crate::lexer::{tokenize, Token};
use quorum_types::{DataType, DbError, Result, Value};

/// Parse a SQL script into statements (split on `;`).
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let tokens = tokenize(sql)?;
    let mut p = Parser { tokens, pos: 0 };
    p.parse_script()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

/// Words that terminate an expression / start a clause.
const CLAUSE_KEYWORDS: &[&str] = &[
    "WHERE", "GROUP", "ORDER", "LIMIT", "OFFSET", "HAVING", "FROM", "SET", "BY",
];

impl Parser {
    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn peek_at(&self, off: usize) -> &Token {
        self.tokens.get(self.pos + off).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at_word(&self, w: &str) -> bool {
        matches!(self.peek(), Token::Word(s) if s.eq_ignore_ascii_case(w))
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Token::Eof)
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.at_word(w) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, w: &str) -> Result<()> {
        if self.eat_word(w) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {}", w)))
        }
    }

    fn expect(&mut self, t: &Token) -> Result<()> {
        if self.peek() == t {
            self.advance();
            Ok(())
        } else {
            Err(self.err(&format!("expected {:?}", t)))
        }
    }

    fn expect_ident(&mut self, what: &str) -> Result<String> {
        match self.peek().clone() {
            Token::Word(w) => {
                self.advance();
                Ok(w)
            }
            other => Err(self.err(&format!("expected {}, found {:?}", what, other))),
        }
    }

    fn err(&self, msg: &str) -> DbError {
        DbError::Parse(format!("{} (at token {:?})", msg, self.peek()))
    }

    fn parse_script(&mut self) -> Result<Vec<Statement>> {
        let mut out = Vec::new();
        while !self.at_eof() {
            if matches!(self.peek(), Token::Semicolon) {
                self.advance();
                continue;
            }
            out.push(self.parse_statement()?);
            if matches!(self.peek(), Token::Semicolon) {
                self.advance();
            }
        }
        if out.is_empty() {
            return Err(DbError::Parse("empty statement".into()));
        }
        Ok(out)
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        let word = match self.peek() {
            Token::Word(w) => w.to_ascii_uppercase(),
            other => return Err(self.err(&format!("expected a statement, found {:?}", other))),
        };
        match word.as_str() {
            "CREATE" => Ok(Statement::CreateTable(self.parse_create_table()?)),
            "DROP" => Ok(Statement::DropTable(self.parse_drop_table()?)),
            "INSERT" => Ok(Statement::Insert(self.parse_insert()?)),
            "SELECT" => Ok(Statement::Select(self.parse_select()?)),
            "UPDATE" => Ok(Statement::Update(self.parse_update()?)),
            "DELETE" => Ok(Statement::Delete(self.parse_delete()?)),
            other => Err(DbError::Parse(format!("unsupported statement: {}", other))),
        }
    }

    fn parse_create_table(&mut self) -> Result<CreateTableStmt> {
        self.expect_word("CREATE")?;
        if !self.eat_word("TABLE") {
            return Err(self.err("only CREATE TABLE is supported"));
        }
        let if_not_exists = if self.at_word("IF") {
            self.advance();
            self.expect_word("NOT")?;
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.expect_ident("table name")?;
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_column_spec()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        if columns.is_empty() {
            return Err(DbError::Parse("table must have at least one column".into()));
        }
        // duplicate column names
        for (i, c) in columns.iter().enumerate() {
            if columns[..i].iter().any(|x| x.name.eq_ignore_ascii_case(&c.name)) {
                return Err(DbError::Parse(format!("duplicate column {}", c.name)));
            }
        }
        Ok(CreateTableStmt {
            name,
            if_not_exists,
            columns,
        })
    }

    fn parse_column_spec(&mut self) -> Result<ColumnSpec> {
        let name = self.expect_ident("column name")?;
        let type_word = self.expect_ident("column type")?;
        let data_type = DataType::from_name(&type_word)
            .ok_or_else(|| DbError::Parse(format!("unknown type: {}", type_word)))?;
        let mut spec = ColumnSpec {
            name,
            data_type,
            nullable: true,
            primary_key: false,
        };
        loop {
            if self.at_word("NOT") {
                self.advance();
                self.expect_word("NULL")?;
                spec.nullable = false;
            } else if self.at_word("NULL") {
                self.advance();
                spec.nullable = true;
            } else if self.at_word("PRIMARY") {
                self.advance();
                self.expect_word("KEY")?;
                spec.primary_key = true;
                spec.nullable = false;
            } else {
                break;
            }
        }
        Ok(spec)
    }

    fn parse_drop_table(&mut self) -> Result<DropTableStmt> {
        self.expect_word("DROP")?;
        self.expect_word("TABLE")?;
        let if_exists = if self.at_word("IF") {
            self.advance();
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.expect_ident("table name")?;
        Ok(DropTableStmt { name, if_exists })
    }

    fn parse_insert(&mut self) -> Result<InsertStmt> {
        self.expect_word("INSERT")?;
        self.expect_word("INTO")?;
        let table = self.expect_ident("table name")?;
        let columns = if matches!(self.peek(), Token::LParen) {
            self.advance();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident("column name")?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            Some(cols)
        } else {
            None
        };
        self.expect_word("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Token::LParen)?;
            let mut row = Vec::new();
            loop {
                row.push(self.parse_expr()?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            rows.push(row);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(InsertStmt {
            table,
            columns,
            rows,
        })
    }

    fn parse_select(&mut self) -> Result<SelectStmt> {
        self.expect_word("SELECT")?;
        if self.at_word("DISTINCT") {
            return Err(self.err("DISTINCT is not supported yet"));
        }
        let mut items = Vec::new();
        loop {
            items.push(self.parse_select_item()?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }

        let mut from = None;
        if self.eat_word("FROM") {
            from = Some(self.expect_ident("table name")?);
            // Single-table FROM only (for now).
            if matches!(self.peek(), Token::Comma) || self.at_word("JOIN") {
                return Err(self.err("JOINs / multiple tables are not supported yet"));
            }
        }

        let selection = if self.eat_word("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat_word("GROUP") {
            self.expect_word("BY")?;
            loop {
                group_by.push(self.parse_expr()?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
        }

        let having = if self.eat_word("HAVING") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut order_by = Vec::new();
        if self.eat_word("ORDER") {
            self.expect_word("BY")?;
            loop {
                let expr = self.parse_expr()?;
                let asc = if self.eat_word("DESC") {
                    false
                } else {
                    self.eat_word("ASC");
                    true
                };
                order_by.push(OrderByItem { expr, asc });
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
        }

        let mut limit = None;
        let mut offset = None;
        if self.eat_word("LIMIT") {
            limit = Some(self.parse_int_literal("LIMIT")?);
        }
        if self.eat_word("OFFSET") {
            offset = Some(self.parse_int_literal("OFFSET")?);
        }
        if self.eat_word("LIMIT") {
            // OFFSET before LIMIT variant
            limit = Some(self.parse_int_literal("LIMIT")?);
        }

        Ok(SelectStmt {
            items,
            from,
            selection,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_int_literal(&mut self, what: &str) -> Result<i64> {
        match self.peek().clone() {
            Token::Number(n) => {
                self.advance();
                n.parse::<i64>()
                    .map_err(|_| DbError::Parse(format!("{} expects an integer", what)))
            }
            other => Err(self.err(&format!("{} expects a number, found {:?}", what, other))),
        }
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        if matches!(self.peek(), Token::Star) {
            self.advance();
            return Ok(SelectItem::Wildcard);
        }
        if let Token::Word(w) = self.peek().clone() {
            if matches!(self.peek_at(1), Token::Dot) && matches!(self.peek_at(2), Token::Star) {
                self.advance();
                self.advance();
                self.advance();
                return Ok(SelectItem::QualifiedWildcard { table: w });
            }
        }
        let expr = self.parse_expr()?;
        let alias = self.parse_optional_alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    /// Optional `AS name` or bare `name` alias (not for clause keywords).
    fn parse_optional_alias(&mut self) -> Result<Option<String>> {
        if self.eat_word("AS") {
            return self.expect_ident("alias").map(Some);
        }
        if let Token::Word(w) = self.peek().clone() {
            if !CLAUSE_KEYWORDS
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
            {
                self.advance();
                return Ok(Some(w));
            }
        }
        Ok(None)
    }

    fn parse_update(&mut self) -> Result<UpdateStmt> {
        self.expect_word("UPDATE")?;
        let table = self.expect_ident("table name")?;
        self.expect_word("SET")?;
        let mut assignments = Vec::new();
        loop {
            let column = self.expect_ident("column name")?;
            self.expect(&Token::Eq)?;
            let value = self.parse_expr()?;
            assignments.push(Assignment { column, value });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        let selection = if self.eat_word("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(UpdateStmt {
            table,
            assignments,
            selection,
        })
    }

    fn parse_delete(&mut self) -> Result<DeleteStmt> {
        self.expect_word("DELETE")?;
        self.expect_word("FROM")?;
        let table = self.expect_ident("table name")?;
        let selection = if self.eat_word("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(DeleteStmt {
            table,
            selection,
        })
    }

    // ------------------------------------------------------------------
    // Expressions, in precedence order:
    //   OR -> AND -> NOT -> comparison -> additive -> mult -> unary -> primary
    // ------------------------------------------------------------------

    pub fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while self.eat_word("OR") {
            let right = self.parse_and()?;
            left = Expr::Binary {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_not()?;
        while self.eat_word("AND") {
            let right = self.parse_not()?;
            left = Expr::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.eat_word("NOT") {
            let operand = self.parse_not()?;
            return Ok(Expr::Unary {
                op: UnOp::Not,
                operand: Box::new(operand),
            });
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let mut left = self.parse_additive()?;
        loop {
            // postfix operators: = <> < <= > >=, IS [NOT] NULL,
            // [NOT] IN (...), [NOT] BETWEEN x AND y
            let op = match self.peek() {
                Token::Eq => Some(BinOp::Eq),
                Token::Ne => Some(BinOp::Ne),
                Token::Lt => Some(BinOp::Lt),
                Token::Le => Some(BinOp::Le),
                Token::Gt => Some(BinOp::Gt),
                Token::Ge => Some(BinOp::Ge),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_additive()?;
                left = Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                };
                continue;
            }
            if self.at_word("IS") {
                self.advance();
                let negated = self.eat_word("NOT");
                self.expect_word("NULL")?;
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                };
                continue;
            }
            if self.at_word("IN") || (self.at_word("NOT") && matches!(self.peek_at(1), Token::Word(w) if w.eq_ignore_ascii_case("IN"))) {
                let negated = self.eat_word("NOT");
                self.expect_word("IN")?;
                self.expect(&Token::LParen)?;
                let mut list = Vec::new();
                loop {
                    list.push(self.parse_expr()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                }
                self.expect(&Token::RParen)?;
                left = Expr::InList {
                    expr: Box::new(left),
                    list,
                    negated,
                };
                continue;
            }
            if self.at_word("BETWEEN")
                || (self.at_word("NOT")
                    && matches!(self.peek_at(1), Token::Word(w) if w.eq_ignore_ascii_case("BETWEEN")))
            {
                let negated = self.eat_word("NOT");
                self.expect_word("BETWEEN")?;
                let low = self.parse_additive()?;
                self.expect_word("AND")?;
                let high = self.parse_additive()?;
                left = Expr::Between {
                    expr: Box::new(left),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated,
                };
                continue;
            }
            break;
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::Minus) {
            self.advance();
            let operand = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnOp::Neg,
                operand: Box::new(operand),
            });
        }
        if matches!(self.peek(), Token::Plus) {
            self.advance();
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Token::Number(n) => {
                self.advance();
                if n.contains('.') || n.contains('e') || n.contains('E') {
                    let f: f64 = n
                        .parse()
                        .map_err(|_| DbError::Parse(format!("invalid float: {}", n)))?;
                    Ok(Expr::Literal(Value::Float(f)))
                } else {
                    let i: i64 = n
                        .parse()
                        .map_err(|_| DbError::Parse(format!("invalid integer: {}", n)))?;
                    Ok(Expr::Literal(Value::Int(i)))
                }
            }
            Token::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Text(s)))
            }
            Token::LParen => {
                self.advance();
                let e = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(e)
            }
            Token::Word(w) => {
                let upper = w.to_ascii_uppercase();
                match upper.as_str() {
                    "TRUE" => {
                        self.advance();
                        Ok(Expr::Literal(Value::Bool(true)))
                    }
                    "FALSE" => {
                        self.advance();
                        Ok(Expr::Literal(Value::Bool(false)))
                    }
                    "NULL" => {
                        self.advance();
                        Ok(Expr::Literal(Value::Null))
                    }
                    _ => {
                        self.advance();
                        if matches!(self.peek(), Token::LParen) {
                            // function call
                            self.advance();
                            let mut args = Vec::new();
                            let mut distinct = false;
                            if matches!(self.peek(), Token::Star) {
                                self.advance();
                                // COUNT(*) modeled as a star "column"
                                args.push(Expr::Column {
                                    table: None,
                                    name: "*".to_string(),
                                });
                            } else if !matches!(self.peek(), Token::RParen) {
                                if self.eat_word("DISTINCT") {
                                    distinct = true;
                                }
                                loop {
                                    args.push(self.parse_expr()?);
                                    if matches!(self.peek(), Token::Comma) {
                                        self.advance();
                                        continue;
                                    }
                                    break;
                                }
                            }
                            self.expect(&Token::RParen)?;
                            Ok(Expr::Call {
                                name: upper,
                                args,
                                distinct,
                            })
                        } else if matches!(self.peek(), Token::Dot) {
                            self.advance();
                            let col = self.expect_ident("column name")?;
                            Ok(Expr::Column {
                                table: Some(w),
                                name: col,
                            })
                        } else {
                            Ok(Expr::Column {
                                table: None,
                                name: w,
                            })
                        }
                    }
                }
            }
            other => Err(self.err(&format!("expected an expression, found {:?}", other))),
        }
    }
}
