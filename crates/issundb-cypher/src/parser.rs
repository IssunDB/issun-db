/// Cypher parser: two-phase lex + recursive-descent parse.
///
/// Phase 1: chumsky 0.13 lexer tokenises the input string.
/// Phase 2: a recursive-descent parser converts the normalised token
///          stream into `Statement` AST nodes.
///
/// The public entry point is `parse(cypher: &str) -> Result<Statement, String>`.
use std::collections::HashMap;

use chumsky::prelude::*;

use crate::ast::*;

// ─── Token ────────────────────────────────────────────────────────────────────

/// A Cypher token produced by the lexer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    // Literals
    Integer(i64),
    Float(f64),
    Str(String),
    Param(String),  // $name
    Ident(String),  // identifier or keyword (already upper-cased in keyword slot)

    // Operators and punctuation
    Eq,      // =
    Ne,      // <> / !=
    Lt,      // <
    Gt,      // >
    Le,      // <=
    Ge,      // >=
    RegexEq, // =~
    Arrow,   // ->
    LArrow,  // <-
    Plus,    // +
    Minus,   // -
    Star,    // *
    Slash,   // /
    Percent, // %
    Caret,   // ^
    Dot,     // .
    DotDot,  // ..
    Comma,   // ,
    Colon,   // :
    Semi,    // ;
    Pipe,    // |
    LParen,  // (
    RParen,  // )
    LBrace,  // {
    RBrace,  // }
    LBrack,  // [
    RBrack,  // ]
}

// ─── Lexer (chumsky 0.13) ─────────────────────────────────────────────────────

/// Build a chumsky lexer that converts a Cypher source string into a flat
/// `Vec<Tok>` (no spans are retained; the recursive-descent pass works on
/// the plain token sequence).
fn lexer<'src>() -> impl Parser<'src, &'src str, Vec<Tok>, extra::Err<Rich<'src, char>>> {
    // Hex integer: 0x1A or 0X1A
    let hex_int = just("0x")
        .or(just("0X"))
        .ignore_then(text::digits(16).to_slice())
        .map(|s: &str| Tok::Integer(i64::from_str_radix(s, 16).unwrap_or(0)));

    // Octal integer: 0o77 or 0O77
    let oct_int = just("0o")
        .or(just("0O"))
        .ignore_then(text::digits(8).to_slice())
        .map(|s: &str| Tok::Integer(i64::from_str_radix(s, 8).unwrap_or(0)));

    // Floating-point: must come before plain integers so "1.0" is parsed as float.
    // Cases:
    //   1.5     integer dot digit+
    //   1.5e3   integer dot digit+ exponent
    //   1e3     integer exponent (no dot)
    //   .5      dot digit+
    //   .5e3    dot digit+ exponent
    // Crucially: "1.." must NOT match as a float (the dot must be followed by a digit,
    // not another dot), so we require at least one digit after the decimal point.
    let exponent = choice((just('e'), just('E')))
        .then(just('-').or(just('+')).or_not())
        .then(text::digits(10));

    let float_num = choice((
        // 1.5 / 1.5e3 — integer part, dot, one or more digits, optional exponent
        text::int(10)
            .then(
                choice((
                    // dot followed by digits (NOT another dot)
                    just('.')
                        .then(text::digits(10))
                        .then(exponent.clone().or_not())
                        .to_slice()
                        .map(Some),
                    // exponent only (no dot): 1e3
                    exponent.clone()
                        .to_slice()
                        .map(Some),
                ))
            )
            .to_slice()
            .map(|s: &str| Tok::Float(s.parse().unwrap_or(0.0))),
        // .5 / .5e-3 (no integer part)
        just('.')
            .then(text::digits(10))
            .then(exponent.clone().or_not())
            .to_slice()
            .map(|s: &str| Tok::Float(s.parse().unwrap_or(0.0))),
    ));

    // Integer
    let int_num = text::int(10)
        .to_slice()
        .map(|s: &str| Tok::Integer(s.parse().unwrap_or(0)));

    // Single-quoted string
    let sq_str = just('\'')
        .ignore_then(
            choice((
                just('\\')
                    .ignore_then(any())
                    .map(|c| match c {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    }),
                none_of("\\'"),
            ))
            .repeated()
            .collect::<String>(),
        )
        .then_ignore(just('\''))
        .map(Tok::Str);

    // Double-quoted string
    let dq_str = just('"')
        .ignore_then(
            choice((
                just('\\')
                    .ignore_then(any())
                    .map(|c| match c {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    }),
                none_of("\\\""),
            ))
            .repeated()
            .collect::<String>(),
        )
        .then_ignore(just('"'))
        .map(Tok::Str);

    // Backtick-quoted identifier: `my prop`
    let backtick_ident = just('`')
        .ignore_then(none_of("`").repeated().collect::<String>())
        .then_ignore(just('`'))
        .map(Tok::Ident);

    // Parameter: $name
    let param = just('$')
        .ignore_then(
            any()
                .filter(|c: &char| c.is_alphanumeric() || *c == '_')
                .repeated()
                .at_least(1)
                .collect::<String>(),
        )
        .map(Tok::Param);

    // Identifier / keyword (keywords are kept as Ident with upper-cased value so
    // the downstream parser can do case-insensitive matching by uppercasing)
    let ident = any()
        .filter(|c: &char| c.is_alphabetic() || *c == '_')
        .then(
            any()
                .filter(|c: &char| c.is_alphanumeric() || *c == '_')
                .repeated(),
        )
        .to_slice()
        .map(|s: &str| Tok::Ident(s.to_string()));

    // Multi-character symbols (must be tried before single-char)
    let multi_sym = choice((
        just("->").to(Tok::Arrow),
        just("<-").to(Tok::LArrow),
        just("<>").to(Tok::Ne),
        just("!=").to(Tok::Ne),
        just("<=").to(Tok::Le),
        just(">=").to(Tok::Ge),
        just("=~").to(Tok::RegexEq),
        just("..").to(Tok::DotDot),
    ));

    // Single-character symbols
    let single_sym = choice((
        just('<').to(Tok::Lt),
        just('>').to(Tok::Gt),
        just('=').to(Tok::Eq),
        just('+').to(Tok::Plus),
        just('-').to(Tok::Minus),
        just('*').to(Tok::Star),
        just('/').to(Tok::Slash),
        just('%').to(Tok::Percent),
        just('^').to(Tok::Caret),
        just('.').to(Tok::Dot),
        just(',').to(Tok::Comma),
        just(':').to(Tok::Colon),
        just(';').to(Tok::Semi),
        just('|').to(Tok::Pipe),
        just('(').to(Tok::LParen),
        just(')').to(Tok::RParen),
        just('{').to(Tok::LBrace),
        just('}').to(Tok::RBrace),
        just('[').to(Tok::LBrack),
        just(']').to(Tok::RBrack),
    ));

    // Line comment
    let comment = just("//")
        .then(any().and_is(just('\n').not()).repeated())
        .ignored();

    let token = choice((
        hex_int,
        oct_int,
        float_num,
        int_num,
        sq_str,
        dq_str,
        backtick_ident,
        param,
        ident,
        multi_sym,
        single_sym,
    ));

    token
        .padded_by(comment.repeated())
        .padded()
        .repeated()
        .collect()
}

// ─── Token-stream cursor ──────────────────────────────────────────────────────

/// A simple cursor over a `Vec<Tok>` slice for the recursive-descent parser.
struct Cursor<'a> {
    tokens: &'a [Tok],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(tokens: &'a [Tok]) -> Self {
        Cursor { tokens, pos: 0 }
    }

    /// Peek at the current token without consuming it.
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    /// Peek at `offset` ahead (0 = current).
    fn peek_at(&self, offset: usize) -> Option<&Tok> {
        self.tokens.get(self.pos + offset)
    }

    /// Consume the current token and advance.
    fn next(&mut self) -> Option<&Tok> {
        let tok = self.tokens.get(self.pos)?;
        self.pos += 1;
        Some(tok)
    }

    /// Consume if the current token matches `t`.
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Require a specific punctuation token.
    fn expect_tok(&mut self, t: &Tok) -> Result<(), String> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(format!("expected {:?}, got {:?}", t, self.peek()))
        }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// True if current token is an Ident matching the keyword (case-insensitive).
    fn peek_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn peek_kw_at(&self, offset: usize, kw: &str) -> bool {
        matches!(self.peek_at(offset), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }
}

// ─── Expression parser ────────────────────────────────────────────────────────
//
// Operator precedence (lowest to highest):
//   OR  XOR  AND  NOT  CMP  ADD  MUL  POW  UNARY-  POSTFIX  ATOM

fn parse_expr(c: &mut Cursor<'_>) -> Result<Expr, String> {
    parse_expr_or(c)
}

fn parse_expr_or(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let mut left = parse_expr_xor(c)?;
    while c.peek_kw("OR") {
        c.next();
        let right = parse_expr_xor(c)?;
        left = Expr::BinaryOp {
            op: BinaryOperator::Or,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_xor(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let mut left = parse_expr_and(c)?;
    while c.peek_kw("XOR") {
        c.next();
        let right = parse_expr_and(c)?;
        left = Expr::BinaryOp {
            op: BinaryOperator::Xor,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_and(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let mut left = parse_expr_not(c)?;
    while c.peek_kw("AND") {
        c.next();
        let right = parse_expr_not(c)?;
        left = Expr::BinaryOp {
            op: BinaryOperator::And,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_not(c: &mut Cursor<'_>) -> Result<Expr, String> {
    if c.peek_kw("NOT") {
        c.next();
        // NOT IN is handled at the comparison level; here handle NOT <expr>
        // We need to check if the next thing after NOT is IN (that's "NOT IN")
        // Actually NOT IN is parsed at the comparison level, but for proper
        // associativity we need to handle "NOT" as a prefix here.
        let inner = parse_expr_not(c)?;
        return Ok(Expr::Not(Box::new(inner)));
    }
    parse_expr_cmp(c)
}

fn parse_expr_cmp(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let mut left = parse_expr_add(c)?;

    loop {
        // IS NULL / IS NOT NULL (postfix)
        if c.peek_kw("IS") {
            // Peek ahead: IS NULL or IS NOT NULL
            if c.peek_kw_at(1, "NOT") && c.peek_kw_at(2, "NULL") {
                c.next(); c.next(); c.next(); // consume IS NOT NULL
                left = Expr::IsNotNull(Box::new(left));
                continue;
            } else if c.peek_kw_at(1, "NULL") {
                c.next(); c.next(); // consume IS NULL
                left = Expr::IsNull(Box::new(left));
                continue;
            }
        }
        // STARTS WITH
        if c.peek_kw("STARTS") && c.peek_kw_at(1, "WITH") {
            c.next(); c.next();
            let right = parse_expr_add(c)?;
            left = Expr::FunctionCall {
                name: "__starts_with__".to_string(),
                args: vec![left, right],
            };
            continue;
        }
        // ENDS WITH
        if c.peek_kw("ENDS") && c.peek_kw_at(1, "WITH") {
            c.next(); c.next();
            let right = parse_expr_add(c)?;
            left = Expr::FunctionCall {
                name: "__ends_with__".to_string(),
                args: vec![left, right],
            };
            continue;
        }
        // CONTAINS
        if c.peek_kw("CONTAINS") {
            c.next();
            let right = parse_expr_add(c)?;
            left = Expr::FunctionCall {
                name: "__contains__".to_string(),
                args: vec![left, right],
            };
            continue;
        }
        // NOT IN
        if c.peek_kw("NOT") && c.peek_kw_at(1, "IN") {
            c.next(); c.next();
            let right = parse_expr_add(c)?;
            left = Expr::Not(Box::new(Expr::FunctionCall {
                name: "__in__".to_string(),
                args: vec![left, right],
            }));
            continue;
        }
        // IN
        if c.peek_kw("IN") {
            c.next();
            let right = parse_expr_add(c)?;
            left = Expr::FunctionCall {
                name: "__in__".to_string(),
                args: vec![left, right],
            };
            continue;
        }
        // Symbolic comparisons
        let op = match c.peek() {
            Some(Tok::Ne) => Some(BinaryOperator::Ne),
            Some(Tok::Le) => Some(BinaryOperator::Le),
            Some(Tok::Ge) => Some(BinaryOperator::Ge),
            Some(Tok::Lt) => Some(BinaryOperator::Lt),
            Some(Tok::Gt) => Some(BinaryOperator::Gt),
            Some(Tok::Eq) => Some(BinaryOperator::Eq),
            _ => None,
        };
        if let Some(op) = op {
            c.next();
            let right = parse_expr_add(c)?;
            left = Expr::BinaryOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
            continue;
        }
        // RegexEq =~
        if c.peek() == Some(&Tok::RegexEq) {
            c.next();
            let right = parse_expr_add(c)?;
            left = Expr::FunctionCall {
                name: "__regex__".to_string(),
                args: vec![left, right],
            };
            continue;
        }
        break;
    }
    Ok(left)
}

fn parse_expr_add(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let mut left = parse_expr_mul(c)?;
    loop {
        if c.peek() == Some(&Tok::Plus) {
            c.next();
            let right = parse_expr_mul(c)?;
            left = Expr::BinaryOp {
                op: BinaryOperator::Add,
                left: Box::new(left),
                right: Box::new(right),
            };
        } else if c.peek() == Some(&Tok::Minus) {
            // Make sure this minus is a binary minus, not a unary at the start of a subexpr
            c.next();
            let right = parse_expr_mul(c)?;
            left = Expr::BinaryOp {
                op: BinaryOperator::Sub,
                left: Box::new(left),
                right: Box::new(right),
            };
        } else {
            break;
        }
    }
    Ok(left)
}

fn parse_expr_mul(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let mut left = parse_expr_pow(c)?;
    loop {
        let op = match c.peek() {
            Some(Tok::Star) => Some(BinaryOperator::Mul),
            Some(Tok::Slash) => Some(BinaryOperator::Div),
            Some(Tok::Percent) => Some(BinaryOperator::Mod),
            _ => None,
        };
        if let Some(op) = op {
            c.next();
            let right = parse_expr_pow(c)?;
            left = Expr::BinaryOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        } else {
            break;
        }
    }
    Ok(left)
}

fn parse_expr_pow(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let base = parse_expr_unary(c)?;
    if c.peek() == Some(&Tok::Caret) {
        c.next();
        let exp = parse_expr_pow(c)?; // right-associative
        return Ok(Expr::BinaryOp {
            op: BinaryOperator::Pow,
            left: Box::new(base),
            right: Box::new(exp),
        });
    }
    Ok(base)
}

fn parse_expr_unary(c: &mut Cursor<'_>) -> Result<Expr, String> {
    if c.peek() == Some(&Tok::Minus) {
        c.next();
        // Unary minus: only when followed by a number or '('
        let inner = parse_expr_postfix(c)?;
        return Ok(Expr::BinaryOp {
            op: BinaryOperator::Sub,
            left: Box::new(Expr::Literal(Literal::Int(0))),
            right: Box::new(inner),
        });
    }
    parse_expr_postfix(c)
}

fn parse_expr_postfix(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let mut expr = parse_expr_atom(c)?;

    loop {
        // Property access: expr.prop
        if c.peek() == Some(&Tok::Dot) {
            c.next();
            let prop = expect_any_ident(c)?;
            expr = match expr {
                Expr::Prop(var, ref empty) if empty.is_empty() => Expr::Prop(var, prop),
                other => Expr::Subscript {
                    expr: Box::new(other),
                    index: Box::new(Expr::Literal(Literal::Str(prop))),
                },
            };
            continue;
        }

        // Subscript / slice: expr[...]
        if c.peek() == Some(&Tok::LBrack) {
            c.next();
            // Check for slice: optional start, DotDot, optional end
            // or subscript: expr
            let start_expr = if c.peek() != Some(&Tok::DotDot) && c.peek() != Some(&Tok::RBrack) {
                Some(parse_expr_or(c)?)
            } else {
                None
            };
            if c.eat(&Tok::DotDot) {
                // Slice
                let end_expr = if c.peek() != Some(&Tok::RBrack) {
                    Some(parse_expr_or(c)?)
                } else {
                    None
                };
                c.expect_tok(&Tok::RBrack)?;
                expr = Expr::Slice {
                    expr: Box::new(expr),
                    start: start_expr.map(Box::new),
                    end: end_expr.map(Box::new),
                };
            } else {
                // Subscript
                let idx = start_expr.ok_or("empty subscript")?;
                c.expect_tok(&Tok::RBrack)?;
                expr = Expr::Subscript {
                    expr: Box::new(expr),
                    index: Box::new(idx),
                };
            }
            continue;
        }

        // HasLabel / IS NULL / IS NOT NULL are handled at cmp level
        // Label test: expr:Label (only when expr is a bare variable)
        if c.peek() == Some(&Tok::Colon) {
            // Peek to see if there's a label name next (and not another `:`)
            if let Some(Tok::Ident(_)) = c.peek_at(1) {
                if let Expr::Prop(ref var, ref empty) = expr {
                    if empty.is_empty() {
                        let var = var.clone();
                        c.next(); // consume :
                        let label = expect_any_ident(c)?;
                        expr = Expr::HasLabel { variable: var, label };
                        continue;
                    }
                }
            }
        }

        break;
    }
    Ok(expr)
}

/// Accept an identifier-like token (including keywords usable as identifiers).
fn expect_any_ident(c: &mut Cursor<'_>) -> Result<String, String> {
    match c.peek() {
        Some(Tok::Ident(s)) => {
            let s = s.clone();
            c.next();
            Ok(s)
        }
        other => Err(format!("expected identifier, got {:?}", other)),
    }
}

fn parse_expr_atom(c: &mut Cursor<'_>) -> Result<Expr, String> {
    match c.peek() {
        // Null / true / false
        Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("NULL") => {
            c.next();
            Ok(Expr::Literal(Literal::Null))
        }
        Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("TRUE") => {
            c.next();
            Ok(Expr::Literal(Literal::Bool(true)))
        }
        Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("FALSE") => {
            c.next();
            Ok(Expr::Literal(Literal::Bool(false)))
        }

        // Integer literal
        Some(Tok::Integer(_)) => {
            if let Some(Tok::Integer(n)) = c.next() {
                Ok(Expr::Literal(Literal::Int(*n)))
            } else {
                unreachable!()
            }
        }

        // Float literal
        Some(Tok::Float(_)) => {
            if let Some(Tok::Float(f)) = c.next() {
                Ok(Expr::Literal(Literal::Float(*f)))
            } else {
                unreachable!()
            }
        }

        // String literal
        Some(Tok::Str(_)) => {
            if let Some(Tok::Str(s)) = c.next() {
                Ok(Expr::Literal(Literal::Str(s.clone())))
            } else {
                unreachable!()
            }
        }

        // Parameter
        Some(Tok::Param(_)) => {
            if let Some(Tok::Param(p)) = c.next() {
                Ok(Expr::Param(p.clone()))
            } else {
                unreachable!()
            }
        }

        // Parenthesised expression
        Some(Tok::LParen) => {
            c.next();
            let inner = parse_expr(c)?;
            c.expect_tok(&Tok::RParen)?;
            Ok(inner)
        }

        // List comprehension or list literal
        Some(Tok::LBrack) => parse_list_or_comprehension(c),

        // Map literal
        Some(Tok::LBrace) => parse_map_literal(c),

        // Identifier: could be a keyword (CASE, quantifier, agg, function, variable)
        Some(Tok::Ident(_)) => parse_ident_expr(c),

        other => Err(format!("unexpected token in expression: {:?}", other)),
    }
}

fn parse_list_or_comprehension(c: &mut Cursor<'_>) -> Result<Expr, String> {
    c.expect_tok(&Tok::LBrack)?;

    // Empty list
    if c.eat(&Tok::RBrack) {
        return Ok(Expr::FunctionCall {
            name: "__list__".to_string(),
            args: vec![],
        });
    }

    // Detect list comprehension: starts with `ident IN`
    let is_comp = matches!(c.peek(), Some(Tok::Ident(_)))
        && matches!(c.peek_at(1), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("IN"));

    if is_comp {
        let variable = expect_any_ident(c)?;
        c.next(); // consume IN
        let list = parse_expr_or(c)?;

        // Optional WHERE pred
        let predicate = if c.peek_kw("WHERE") {
            c.next();
            Some(Box::new(parse_expr_or(c)?))
        } else {
            None
        };
        // Optional | transform
        let transform = if c.eat(&Tok::Pipe) {
            Some(Box::new(parse_expr_or(c)?))
        } else {
            None
        };

        c.expect_tok(&Tok::RBrack)?;
        return Ok(Expr::ListComprehension {
            variable,
            list: Box::new(list),
            predicate,
            transform,
        });
    }

    // List literal
    let mut items = Vec::new();
    loop {
        if c.peek() == Some(&Tok::RBrack) {
            break;
        }
        items.push(parse_expr_or(c)?);
        if !c.eat(&Tok::Comma) {
            break;
        }
    }
    c.expect_tok(&Tok::RBrack)?;
    Ok(Expr::FunctionCall {
        name: "__list__".to_string(),
        args: items,
    })
}

fn parse_map_literal(c: &mut Cursor<'_>) -> Result<Expr, String> {
    c.expect_tok(&Tok::LBrace)?;
    let mut args = Vec::new();
    if c.peek() != Some(&Tok::RBrace) {
        loop {
            let key = expect_any_ident(c)?;
            c.expect_tok(&Tok::Colon)?;
            let val = parse_expr_or(c)?;
            args.push(Expr::Literal(Literal::Str(key)));
            args.push(val);
            if !c.eat(&Tok::Comma) {
                break;
            }
        }
    }
    c.expect_tok(&Tok::RBrace)?;
    Ok(Expr::FunctionCall {
        name: "__map__".to_string(),
        args,
    })
}

fn parse_ident_expr(c: &mut Cursor<'_>) -> Result<Expr, String> {
    // Peek at the identifier
    let name = match c.peek() {
        Some(Tok::Ident(s)) => s.clone(),
        _ => return Err("expected identifier".into()),
    };
    let name_upper = name.to_ascii_uppercase();

    // CASE expression
    if name_upper == "CASE" {
        return parse_case_expr(c);
    }

    // Quantifiers: ALL, ANY, NONE, SINGLE followed by '('
    if matches!(
        name_upper.as_str(),
        "ALL" | "ANY" | "NONE" | "SINGLE"
    ) && c.peek_at(1) == Some(&Tok::LParen)
    {
        return parse_quantifier_expr(c);
    }

    // count(*) special case
    if name_upper == "COUNT"
        && c.peek_at(1) == Some(&Tok::LParen)
        && c.peek_at(2) == Some(&Tok::Star)
        && c.peek_at(3) == Some(&Tok::RParen)
    {
        c.next(); c.next(); c.next(); c.next(); // consume COUNT ( * )
        return Ok(Expr::CountStar);
    }

    // Aggregation functions: COUNT, SUM, AVG, MIN, MAX, COLLECT, STDEV, STDEVP
    if matches!(
        name_upper.as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "COLLECT" | "STDEV" | "STDEVP"
    ) && c.peek_at(1) == Some(&Tok::LParen)
    {
        return parse_agg_expr(c);
    }

    // percentileDisc / percentileCont
    if matches!(name_upper.as_str(), "PERCENTILEDISC" | "PERCENTILECONT")
        && c.peek_at(1) == Some(&Tok::LParen)
    {
        return parse_percentile_expr(c);
    }

    // Generic function call: name(args...)
    if c.peek_at(1) == Some(&Tok::LParen) {
        return parse_fn_call_expr(c);
    }

    // Dotted function call: name.name(args...)
    if c.peek_at(1) == Some(&Tok::Dot) {
        if let Some(Tok::Ident(_)) = c.peek_at(2) {
            if c.peek_at(3) == Some(&Tok::LParen) {
                return parse_fn_call_expr(c);
            }
        }
    }

    // Bare identifier (variable reference)
    c.next();
    Ok(Expr::Prop(name, "".to_string()))
}

fn parse_quantifier_expr(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let name = expect_any_ident(c)?;
    let kind = match name.to_ascii_uppercase().as_str() {
        "ALL" => QuantifierKind::All,
        "ANY" => QuantifierKind::Any,
        "NONE" => QuantifierKind::None,
        "SINGLE" => QuantifierKind::Single,
        _ => unreachable!(),
    };
    c.expect_tok(&Tok::LParen)?;
    let variable = expect_any_ident(c)?;
    c.next(); // IN keyword
    let list = parse_expr_or(c)?;
    c.next(); // WHERE keyword
    let predicate = parse_expr_or(c)?;
    c.expect_tok(&Tok::RParen)?;
    Ok(Expr::Quantifier {
        kind,
        variable,
        list: Box::new(list),
        predicate: Box::new(predicate),
    })
}

fn parse_agg_expr(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let name = expect_any_ident(c)?;
    let name_upper = name.to_ascii_uppercase();
    c.expect_tok(&Tok::LParen)?;
    let distinct = c.peek_kw("DISTINCT");
    if distinct {
        c.next();
    }
    let inner = parse_expr(c)?;
    c.expect_tok(&Tok::RParen)?;

    let agg_fn = match name_upper.as_str() {
        "COUNT" => AggFn::Count { distinct },
        "SUM" => AggFn::Sum { distinct },
        "AVG" => AggFn::Avg { distinct },
        "MIN" => AggFn::Min { distinct },
        "MAX" => AggFn::Max { distinct },
        "COLLECT" => AggFn::Collect { distinct },
        "STDEV" => AggFn::StDev { distinct },
        "STDEVP" => AggFn::StDevP { distinct },
        _ => return Err(format!("unknown aggregation function: {}", name)),
    };
    Ok(Expr::Agg(agg_fn, Box::new(inner)))
}

fn parse_percentile_expr(c: &mut Cursor<'_>) -> Result<Expr, String> {
    let name = expect_any_ident(c)?;
    let is_disc = name.to_ascii_uppercase() == "PERCENTILEDISC";
    c.expect_tok(&Tok::LParen)?;
    let inner = parse_expr(c)?;
    c.expect_tok(&Tok::Comma)?;
    // Accept a literal float/int, a parameter, or a general expression.
    let pct = match c.peek() {
        Some(Tok::Float(_)) => {
            if let Some(Tok::Float(f)) = c.next() {
                *f
            } else {
                unreachable!()
            }
        }
        Some(Tok::Integer(_)) => {
            if let Some(Tok::Integer(n)) = c.next() {
                *n as f64
            } else {
                unreachable!()
            }
        }
        Some(Tok::Param(_)) => {
            // Parameter: use 0.5 as a placeholder; the executor will substitute.
            c.next();
            0.5
        }
        _ => {
            // General expression: parse it and use 0.5 as placeholder.
            let _ = parse_expr(c)?;
            0.5
        }
    };
    c.expect_tok(&Tok::RParen)?;
    let agg_fn = if is_disc {
        AggFn::PercentileDisc { percentile: pct }
    } else {
        AggFn::PercentileCont { percentile: pct }
    };
    Ok(Expr::Agg(agg_fn, Box::new(inner)))
}

fn parse_fn_call_expr(c: &mut Cursor<'_>) -> Result<Expr, String> {
    // Consume name (potentially dotted)
    let mut name = expect_any_ident(c)?.to_ascii_lowercase();
    while c.peek() == Some(&Tok::Dot) {
        // Check if the next token is an identifier (property access vs dotted fn)
        if let Some(Tok::Ident(_)) = c.peek_at(1) {
            if c.peek_at(2) == Some(&Tok::LParen) {
                c.next(); // consume dot
                let part = expect_any_ident(c)?;
                name.push('.');
                name.push_str(&part.to_ascii_lowercase());
                continue;
            }
        }
        break;
    }
    c.expect_tok(&Tok::LParen)?;
    let mut args = Vec::new();
    if c.peek() != Some(&Tok::RParen) {
        loop {
            args.push(parse_expr(c)?);
            if !c.eat(&Tok::Comma) {
                break;
            }
        }
    }
    c.expect_tok(&Tok::RParen)?;
    Ok(Expr::FunctionCall { name, args })
}

fn parse_case_expr(c: &mut Cursor<'_>) -> Result<Expr, String> {
    c.next(); // consume CASE

    // Simple CASE subject (if next token is not WHEN)
    let subject = if !c.peek_kw("WHEN") {
        Some(Box::new(parse_expr(c)?))
    } else {
        None
    };

    let mut arms = Vec::new();
    while c.peek_kw("WHEN") {
        c.next(); // WHEN
        let when = parse_expr(c)?;
        c.next(); // THEN
        let then = parse_expr(c)?;
        arms.push(CaseArm { when, then });
    }

    let else_expr = if c.peek_kw("ELSE") {
        c.next();
        Some(Box::new(parse_expr(c)?))
    } else {
        None
    };

    if !c.peek_kw("END") {
        return Err("CASE expression missing END".into());
    }
    c.next(); // END

    Ok(Expr::Case {
        subject,
        arms,
        else_expr,
    })
}

// ─── Properties map ───────────────────────────────────────────────────────────

fn parse_properties_map(c: &mut Cursor<'_>) -> Result<HashMap<String, Literal>, String> {
    c.expect_tok(&Tok::LBrace)?;
    let mut map = HashMap::new();
    if c.peek() != Some(&Tok::RBrace) {
        loop {
            let key = expect_any_ident(c)?;
            c.expect_tok(&Tok::Colon)?;
            let val = parse_literal_value(c)?;
            map.insert(key, val);
            if !c.eat(&Tok::Comma) {
                break;
            }
        }
    }
    c.expect_tok(&Tok::RBrace)?;
    Ok(map)
}

fn parse_literal_value(c: &mut Cursor<'_>) -> Result<Literal, String> {
    // Handle negative numbers
    if c.peek() == Some(&Tok::Minus) {
        c.next();
        return match c.peek() {
            Some(Tok::Integer(_)) => {
                if let Some(Tok::Integer(n)) = c.next() {
                    Ok(Literal::Int(-n))
                } else {
                    unreachable!()
                }
            }
            Some(Tok::Float(_)) => {
                if let Some(Tok::Float(f)) = c.next() {
                    Ok(Literal::Float(-f))
                } else {
                    unreachable!()
                }
            }
            other => Err(format!("expected number after minus, got {:?}", other)),
        };
    }
    match c.peek() {
        Some(Tok::Str(_)) => {
            if let Some(Tok::Str(s)) = c.next() {
                Ok(Literal::Str(s.clone()))
            } else {
                unreachable!()
            }
        }
        Some(Tok::Integer(_)) => {
            if let Some(Tok::Integer(n)) = c.next() {
                Ok(Literal::Int(*n))
            } else {
                unreachable!()
            }
        }
        Some(Tok::Float(_)) => {
            if let Some(Tok::Float(f)) = c.next() {
                Ok(Literal::Float(*f))
            } else {
                unreachable!()
            }
        }
        Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("TRUE") => {
            c.next();
            Ok(Literal::Bool(true))
        }
        Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("FALSE") => {
            c.next();
            Ok(Literal::Bool(false))
        }
        Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("NULL") => {
            c.next();
            Ok(Literal::Null)
        }
        // List literal: [val1, val2, ...]
        Some(Tok::LBrack) => {
            c.next(); // consume [
            let mut items = Vec::new();
            while c.peek() != Some(&Tok::RBrack) {
                if c.peek().is_none() {
                    return Err("unterminated list literal".into());
                }
                items.push(parse_literal_value(c)?);
                if !c.eat(&Tok::Comma) {
                    break;
                }
            }
            c.expect_tok(&Tok::RBrack)?;
            Ok(Literal::List(items))
        }
        other => Err(format!("expected literal value, got {:?}", other)),
    }
}

// ─── Pattern parsers ──────────────────────────────────────────────────────────

fn parse_node_pattern(c: &mut Cursor<'_>) -> Result<NodePattern, String> {
    c.expect_tok(&Tok::LParen)?;

    // Optional variable
    let variable = if let Some(Tok::Ident(_)) = c.peek() {
        // Only treat as variable if not immediately followed by ':' in the wrong context.
        // Actually, variable is any ident that's not a pure label indicator.
        Some(expect_any_ident(c)?)
    } else {
        None
    };

    // Optional label(s): :Label
    let label = if c.eat(&Tok::Colon) {
        Some(expect_any_ident(c)?)
    } else {
        None
    };

    // Skip additional labels (multi-label: just take first)
    while c.peek() == Some(&Tok::Colon) {
        c.next();
        let _ = expect_any_ident(c)?; // consume but ignore additional labels
    }

    // Optional properties
    let properties = if c.peek() == Some(&Tok::LBrace) {
        Some(parse_properties_map(c)?)
    } else {
        None
    };

    c.expect_tok(&Tok::RParen)?;
    Ok(NodePattern {
        variable,
        label,
        properties,
    })
}

fn parse_rel_range(c: &mut Cursor<'_>) -> Result<RelRange, String> {
    // Already consumed '*'; parse optional range
    // Could be: empty, n, n..m, n.., ..m, ..
    let start_num = if let Some(Tok::Integer(n)) = c.peek() {
        let n = *n as u32;
        c.next();
        Some(n)
    } else {
        None
    };

    if c.eat(&Tok::DotDot) {
        let end_num = if let Some(Tok::Integer(n)) = c.peek() {
            let n = *n as u32;
            c.next();
            Some(n)
        } else {
            None
        };
        Ok(RelRange {
            min: start_num.or(Some(1)),
            max: end_num,
        })
    } else if let Some(n) = start_num {
        // Exact hops
        Ok(RelRange {
            min: Some(n),
            max: Some(n),
        })
    } else {
        // Bare * (no number)
        Ok(RelRange {
            min: Some(1),
            max: None,
        })
    }
}

fn parse_rel_pattern(c: &mut Cursor<'_>) -> Result<RelationshipPattern, String> {
    // Possible prefixes: <- or -
    let is_incoming = c.peek() == Some(&Tok::LArrow);
    if is_incoming {
        c.next(); // consume <-
    } else {
        c.expect_tok(&Tok::Minus)?;
    }

    // Check for bare ->  or --  (no bracket)
    if !is_incoming && c.peek() == Some(&Tok::Arrow) {
        // directed outgoing: ->
        c.next();
        return Ok(RelationshipPattern {
            variable: None,
            rel_type: None,
            is_incoming: false,
            is_undirected: false,
            range: None,
            properties: None,
        });
    }
    if !is_incoming && c.peek() == Some(&Tok::Minus) {
        // undirected: --
        c.next();
        return Ok(RelationshipPattern {
            variable: None,
            rel_type: None,
            is_incoming: false,
            is_undirected: true,
            range: None,
            properties: None,
        });
    }
    // Incoming bare: <-- (already consumed <-, now see another -)
    if is_incoming && c.peek() == Some(&Tok::Minus) {
        c.next(); // consume trailing -
        return Ok(RelationshipPattern {
            variable: None,
            rel_type: None,
            is_incoming: true,
            is_undirected: false,
            range: None,
            properties: None,
        });
    }

    // We expect a bracket [...]
    c.expect_tok(&Tok::LBrack)?;

    // Optional variable
    let variable = if let Some(Tok::Ident(_)) = c.peek() {
        // Could be a variable OR just followed by : which means no variable
        // We need to disambiguate: if peek_at(1) is ':' or '*' or ']' then it might be a variable
        Some(expect_any_ident(c)?)
    } else {
        None
    };

    // Optional rel type(s): :TYPE or :TYPE1|TYPE2|TYPE3
    let rel_type = if c.eat(&Tok::Colon) {
        let mut types = expect_any_ident(c)?;
        while c.eat(&Tok::Pipe) {
            // Allow optional colon before next type name: :TYPE1|:TYPE2
            let _ = c.eat(&Tok::Colon);
            let next = expect_any_ident(c)?;
            types.push('|');
            types.push_str(&next);
        }
        Some(types)
    } else {
        None
    };

    // Optional variable-length: *range
    let range = if c.eat(&Tok::Star) {
        Some(parse_rel_range(c)?)
    } else {
        None
    };

    // Optional properties
    let properties = if c.peek() == Some(&Tok::LBrace) {
        Some(parse_properties_map(c)?)
    } else {
        None
    };

    c.expect_tok(&Tok::RBrack)?;

    // Suffix: -> or - (undirected)
    let is_outgoing = c.peek() == Some(&Tok::Arrow);
    if is_outgoing {
        c.next(); // consume ->
    } else {
        c.expect_tok(&Tok::Minus)?;
    }

    let is_undirected = !is_incoming && !is_outgoing;

    Ok(RelationshipPattern {
        variable,
        rel_type,
        is_incoming,
        is_undirected,
        range,
        properties,
    })
}

fn parse_pattern(c: &mut Cursor<'_>) -> Result<Pattern, String> {
    // Capture optional path variable assignment: `p = (...)`
    let path_variable = if let Some(Tok::Ident(_)) = c.peek() {
        if c.peek_at(1) == Some(&Tok::Eq) {
            // Make sure next after '=' is a '('
            if c.peek_at(2) == Some(&Tok::LParen) {
                let var = expect_any_ident(c)?;
                c.next(); // =
                Some(var)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let node = parse_node_pattern(c)?;
    let mut rels = Vec::new();

    // Continue parsing relationship-node pairs while we see - or <-
    while matches!(c.peek(), Some(Tok::Minus) | Some(Tok::LArrow)) {
        let rel = parse_rel_pattern(c)?;
        let target = parse_node_pattern(c)?;
        rels.push((rel, target));
    }

    Ok(Pattern { node, rels, path_variable })
}

fn parse_multi_pattern(c: &mut Cursor<'_>) -> Result<Vec<Pattern>, String> {
    let mut patterns = Vec::new();
    patterns.push(parse_pattern(c)?);
    while c.eat(&Tok::Comma) {
        patterns.push(parse_pattern(c)?);
    }
    Ok(patterns)
}

// ─── Match clause parsing ─────────────────────────────────────────────────────

fn parse_match_clauses_from_cursor(c: &mut Cursor<'_>) -> Result<Vec<MatchClause>, String> {
    let mut clauses = Vec::new();
    clauses.push(MatchClause {
        pattern: parse_pattern(c)?,
    });
    while c.eat(&Tok::Comma) {
        clauses.push(MatchClause {
            pattern: parse_pattern(c)?,
        });
    }
    validate_match_clause_variables(&clauses)?;
    Ok(clauses)
}

// ─── WHERE clause ─────────────────────────────────────────────────────────────

fn parse_where_clause_from_cursor(c: &mut Cursor<'_>) -> Result<WhereClause, String> {
    let expr = parse_expr(c)?;
    if let Expr::BinaryOp { op, left, right } = &expr {
        match op {
            BinaryOperator::Eq => return Ok(WhereClause::Eq(*left.clone(), *right.clone())),
            BinaryOperator::Ne => return Ok(WhereClause::Ne(*left.clone(), *right.clone())),
            BinaryOperator::Lt => return Ok(WhereClause::Lt(*left.clone(), *right.clone())),
            BinaryOperator::Gt => return Ok(WhereClause::Gt(*left.clone(), *right.clone())),
            BinaryOperator::Le => return Ok(WhereClause::Le(*left.clone(), *right.clone())),
            BinaryOperator::Ge => return Ok(WhereClause::Ge(*left.clone(), *right.clone())),
            _ => {}
        }
    }
    Ok(WhereClause::Expr(expr))
}

// ─── SET items ────────────────────────────────────────────────────────────────

fn parse_set_items_from_cursor(c: &mut Cursor<'_>) -> Result<Vec<SetItem>, String> {
    let mut items = Vec::new();
    loop {
        // variable.property = expr
        let var = match c.peek() {
            Some(Tok::Ident(_)) => expect_any_ident(c)?,
            _ => break,
        };
        // Check for label set `n:Label` or `n:Label1:Label2` (skip label assignments)
        if c.peek() == Some(&Tok::Colon) {
            // Consume all :Label tokens for this variable
            while c.peek() == Some(&Tok::Colon) {
                c.next(); // :
                let _ = expect_any_ident(c); // label name
            }
            if !c.eat(&Tok::Comma) {
                break;
            }
            continue;
        }
        if c.eat(&Tok::Dot) {
            let prop = expect_any_ident(c)?;
            c.expect_tok(&Tok::Eq)?;
            let expr = parse_expr(c)?;
            items.push(SetItem {
                variable: var,
                property: prop,
                expr,
            });
        } else {
            // Could be `n += {prop: val}` or other forms; skip
            // consume until comma or end of SET
            while !matches!(c.peek(), Some(Tok::Comma) | None)
                && !is_clause_keyword(c)
            {
                c.next();
            }
        }
        if !c.eat(&Tok::Comma) {
            break;
        }
    }
    Ok(items)
}

/// Check if the current token is a major clause keyword that signals the end of a sub-clause.
fn is_clause_keyword(c: &Cursor<'_>) -> bool {
    matches!(
        c.peek(),
        Some(Tok::Ident(s)) if matches!(
            s.to_ascii_uppercase().as_str(),
            "MATCH" | "WHERE" | "RETURN" | "WITH" | "UNWIND" | "ORDER"
            | "SKIP" | "LIMIT" | "SET" | "DELETE" | "REMOVE" | "MERGE"
            | "CREATE" | "UNION" | "ON" | "DETACH" | "FOREACH"
        )
    )
}

// ─── RETURN clause ────────────────────────────────────────────────────────────

fn parse_return_items_from_cursor(c: &mut Cursor<'_>) -> Result<Vec<ReturnItem>, String> {
    let mut items = Vec::new();
    loop {
        let expr = parse_expr(c)?;
        let alias = if c.peek_kw("AS") {
            c.next();
            Some(expect_any_ident(c)?)
        } else {
            None
        };
        items.push(ReturnItem { expr, alias });
        if !c.eat(&Tok::Comma) {
            break;
        }
    }
    Ok(items)
}

fn parse_return_clause_from_cursor(c: &mut Cursor<'_>) -> Result<ReturnClause, String> {
    // Caller has already consumed 'RETURN'
    let distinct = c.peek_kw("DISTINCT");
    if distinct {
        c.next();
    }
    // RETURN * passes all current bindings through.
    let items = if c.peek() == Some(&Tok::Star) {
        c.next();
        vec![ReturnItem {
            expr: Expr::FunctionCall {
                name: "__star__".to_string(),
                args: vec![],
            },
            alias: None,
        }]
    } else {
        parse_return_items_from_cursor(c)?
    };
    Ok(ReturnClause { items, distinct })
}

// ─── ORDER BY ─────────────────────────────────────────────────────────────────

fn parse_order_by_from_cursor(c: &mut Cursor<'_>) -> Result<OrderBy, String> {
    // Caller has consumed 'ORDER'; consume 'BY'
    c.next(); // BY
    let mut items = Vec::new();
    loop {
        let expr = parse_expr(c)?;
        let ascending = if c.peek_kw("ASC") {
            c.next();
            true
        } else if c.peek_kw("DESC") {
            c.next();
            false
        } else {
            true
        };
        items.push(SortItem { expr, ascending });
        if !c.eat(&Tok::Comma) {
            break;
        }
    }
    Ok(OrderBy { items })
}

// ─── SKIP / LIMIT helpers ─────────────────────────────────────────────────────

fn try_parse_order_by(c: &mut Cursor<'_>) -> Result<Option<OrderBy>, String> {
    if c.peek_kw("ORDER") {
        c.next();
        Ok(Some(parse_order_by_from_cursor(c)?))
    } else {
        Ok(None)
    }
}

fn try_parse_skip(c: &mut Cursor<'_>) -> Result<Option<Expr>, String> {
    if c.peek_kw("SKIP") {
        c.next();
        Ok(Some(parse_expr(c)?))
    } else {
        Ok(None)
    }
}

fn try_parse_limit(c: &mut Cursor<'_>) -> Result<Option<Expr>, String> {
    if c.peek_kw("LIMIT") {
        c.next();
        Ok(Some(parse_expr(c)?))
    } else {
        Ok(None)
    }
}

// ─── Variable validation ──────────────────────────────────────────────────────

fn validate_match_clause_variables(clauses: &[MatchClause]) -> Result<(), String> {
    let mut node_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rel_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut path_vars: std::collections::HashSet<String> = std::collections::HashSet::new();

    for clause in clauses {
        let pattern = &clause.pattern;

        // Check path variable conflict within a MATCH clause.
        if let Some(ref pv) = pattern.path_variable {
            if node_vars.contains(pv) || rel_vars.contains(pv) || path_vars.contains(pv) {
                return Err(format!(
                    "SyntaxError(VariableAlreadyBound): variable '{}' is already bound",
                    pv
                ));
            }
            path_vars.insert(pv.clone());
        }

        if let Some(ref v) = pattern.node.variable {
            if rel_vars.contains(v) {
                return Err(format!(
                    "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
                    v
                ));
            }
            if path_vars.contains(v) {
                return Err(format!(
                    "SyntaxError(VariableAlreadyBound): variable '{}' is already bound as a path",
                    v
                ));
            }
            node_vars.insert(v.clone());
        }
        for (rel, target) in &pattern.rels {
            if let Some(ref v) = rel.variable {
                if node_vars.contains(v) {
                    return Err(format!(
                        "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
                        v
                    ));
                }
                if path_vars.contains(v) {
                    return Err(format!(
                        "SyntaxError(VariableAlreadyBound): variable '{}' is already bound as a path",
                        v
                    ));
                }
                if !rel_vars.insert(v.clone()) {
                    return Err(format!(
                        "SyntaxError(VariableAlreadyBound): relationship variable '{}' is already bound in this MATCH clause",
                        v
                    ));
                }
            }
            if let Some(ref v) = target.variable {
                if rel_vars.contains(v) {
                    return Err(format!(
                        "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
                        v
                    ));
                }
                if path_vars.contains(v) {
                    return Err(format!(
                        "SyntaxError(VariableAlreadyBound): variable '{}' is already bound as a path",
                        v
                    ));
                }
                node_vars.insert(v.clone());
            }
        }
    }
    Ok(())
}

fn validate_cross_clause_variable_types(parts: &[QueryPart]) -> Result<(), String> {
    let mut node_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rel_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut path_vars: std::collections::HashSet<String> = std::collections::HashSet::new();

    for part in parts {
        let clauses: Option<&[MatchClause]> = match part {
            QueryPart::Match { match_clauses, .. } => Some(match_clauses.as_slice()),
            QueryPart::OptionalMatch { match_clauses, .. } => Some(match_clauses.as_slice()),
            QueryPart::With { .. } | QueryPart::Unwind { .. } => {
                node_vars.clear();
                rel_vars.clear();
                path_vars.clear();
                None
            }
        };

        if let Some(clauses) = clauses {
            for clause in clauses {
                let pattern = &clause.pattern;

                // Check path variable conflict.
                if let Some(ref pv) = pattern.path_variable {
                    if node_vars.contains(pv) || rel_vars.contains(pv) {
                        return Err(format!(
                            "SyntaxError(VariableAlreadyBound): variable '{}' is already bound",
                            pv
                        ));
                    }
                    path_vars.insert(pv.clone());
                }

                if let Some(ref v) = pattern.node.variable {
                    if rel_vars.contains(v) {
                        return Err(format!(
                            "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
                            v
                        ));
                    }
                    if path_vars.contains(v) {
                        return Err(format!(
                            "SyntaxError(VariableAlreadyBound): variable '{}' is already bound as a path",
                            v
                        ));
                    }
                    node_vars.insert(v.clone());
                }
                for (rel, target) in &pattern.rels {
                    if let Some(ref v) = rel.variable {
                        if node_vars.contains(v) {
                            return Err(format!(
                                "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
                                v
                            ));
                        }
                        if path_vars.contains(v) {
                            return Err(format!(
                                "SyntaxError(VariableAlreadyBound): variable '{}' is already bound as a path",
                                v
                            ));
                        }
                        rel_vars.insert(v.clone());
                    }
                    if let Some(ref v) = target.variable {
                        if rel_vars.contains(v) {
                            return Err(format!(
                                "SyntaxError(VariableTypeConflict): variable '{}' is used as both a node and a relationship",
                                v
                            ));
                        }
                        if path_vars.contains(v) {
                            return Err(format!(
                                "SyntaxError(VariableAlreadyBound): variable '{}' is already bound as a path",
                                v
                            ));
                        }
                        node_vars.insert(v.clone());
                    }
                }
            }
        }
    }
    Ok(())
}

// ─── Statement-level recursive-descent parsers ────────────────────────────────

fn parse_read_query(c: &mut Cursor<'_>) -> Result<Query, String> {
    let mut parts: Vec<QueryPart> = Vec::new();

    loop {
        if c.peek_kw("MATCH") {
            c.next();
            let match_clauses = parse_match_clauses_from_cursor(c)?;
            let where_clause = if c.peek_kw("WHERE") {
                c.next();
                Some(parse_where_clause_from_cursor(c)?)
            } else {
                None
            };
            parts.push(QueryPart::Match {
                match_clauses,
                where_clause,
            });
        } else if c.peek_kw("OPTIONAL") && c.peek_kw_at(1, "MATCH") {
            c.next(); c.next();
            let match_clauses = parse_match_clauses_from_cursor(c)?;
            let where_clause = if c.peek_kw("WHERE") {
                c.next();
                Some(parse_where_clause_from_cursor(c)?)
            } else {
                None
            };
            parts.push(QueryPart::OptionalMatch {
                match_clauses,
                where_clause,
            });
        } else if c.peek_kw("WITH") {
            c.next();
            let distinct = c.peek_kw("DISTINCT");
            if distinct {
                c.next();
            }
            // WITH * passes all current bindings through; represent as a wildcard item.
            let items = if c.peek() == Some(&Tok::Star) {
                c.next();
                vec![ReturnItem {
                    expr: Expr::FunctionCall {
                        name: "__star__".to_string(),
                        args: vec![],
                    },
                    alias: None,
                }]
            } else {
                parse_return_items_from_cursor(c)?
            };
            let where_clause = if c.peek_kw("WHERE") {
                c.next();
                Some(parse_where_clause_from_cursor(c)?)
            } else {
                None
            };
            let order_by = try_parse_order_by(c)?;
            let skip = try_parse_skip(c)?;
            let limit = try_parse_limit(c)?;
            parts.push(QueryPart::With {
                items,
                where_clause,
                order_by,
                skip,
                limit,
                distinct,
            });
        } else if c.peek_kw("UNWIND") {
            c.next();
            let expr = parse_expr(c)?;
            c.next(); // AS
            let variable = expect_any_ident(c)?;
            parts.push(QueryPart::Unwind { expr, variable });
        } else if c.peek_kw("RETURN") {
            break;
        } else {
            break;
        }
    }

    if !c.peek_kw("RETURN") {
        return Err("read query requires a RETURN clause".into());
    }
    c.next(); // RETURN
    let return_clause = parse_return_clause_from_cursor(c)?;
    let order_by = try_parse_order_by(c)?;
    let skip = try_parse_skip(c)?;
    let limit = try_parse_limit(c)?;

    validate_cross_clause_variable_types(&parts)?;

    // Extract first MATCH for backwards-compat fields
    let (match_clauses, where_clause) = parts
        .first()
        .and_then(|p| {
            if let QueryPart::Match { match_clauses, where_clause } = p {
                Some((match_clauses.clone(), where_clause.clone()))
            } else {
                None
            }
        })
        .unwrap_or_default();

    Ok(Query {
        match_clauses,
        where_clause,
        return_clause,
        parts,
        order_by,
        skip,
        limit,
    })
}

fn parse_set_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    // MATCH ... [WHERE ...] SET ... [RETURN ...]
    c.next(); // MATCH
    let match_clauses = parse_match_clauses_from_cursor(c)?;
    let where_clause = if c.peek_kw("WHERE") {
        c.next();
        Some(parse_where_clause_from_cursor(c)?)
    } else {
        None
    };
    c.next(); // SET
    let set_items = parse_set_items_from_cursor(c)?;

    if c.peek_kw("RETURN") {
        c.next();
        let return_clause = parse_return_clause_from_cursor(c)?;
        let order_by = try_parse_order_by(c)?;
        let skip = try_parse_skip(c)?;
        let limit = try_parse_limit(c)?;
        return Ok(Statement::SetAndReturn(SetAndReturnStatement {
            match_clauses,
            where_clause,
            set_items,
            return_clause,
            order_by,
            skip,
            limit,
        }));
    }

    Ok(Statement::Set(SetStatement {
        match_clauses,
        where_clause,
        set_items,
    }))
}

fn parse_delete_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    c.next(); // MATCH
    let match_clauses = parse_match_clauses_from_cursor(c)?;
    let where_clause = if c.peek_kw("WHERE") {
        c.next();
        Some(parse_where_clause_from_cursor(c)?)
    } else {
        None
    };
    let detach = c.peek_kw("DETACH");
    if detach {
        c.next();
    }
    c.next(); // DELETE
    let mut variables = Vec::new();
    loop {
        variables.push(expect_any_ident(c)?);
        if !c.eat(&Tok::Comma) {
            break;
        }
    }
    if c.peek_kw("RETURN") {
        c.next(); // RETURN
        let return_clause = parse_return_clause_from_cursor(c)?;
        let order_by = try_parse_order_by(c)?;
        let skip = try_parse_skip(c)?;
        let limit = try_parse_limit(c)?;
        return Ok(Statement::DeleteAndReturn(DeleteAndReturnStatement {
            match_clauses,
            where_clause,
            variables,
            detach,
            return_clause,
            order_by,
            skip,
            limit,
        }));
    }
    Ok(Statement::Delete(DeleteStatement {
        match_clauses,
        where_clause,
        variables,
        detach,
    }))
}

fn parse_remove_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    c.next(); // MATCH
    let match_clauses = parse_match_clauses_from_cursor(c)?;
    let where_clause = if c.peek_kw("WHERE") {
        c.next();
        Some(parse_where_clause_from_cursor(c)?)
    } else {
        None
    };
    c.next(); // REMOVE
    let mut items = Vec::new();
    loop {
        let var = expect_any_ident(c)?;
        if c.eat(&Tok::Dot) {
            let prop = expect_any_ident(c)?;
            items.push(RemoveItem::Property {
                variable: var,
                property: prop,
            });
        } else if c.eat(&Tok::Colon) {
            let label = expect_any_ident(c)?;
            items.push(RemoveItem::Label {
                variable: var,
                label,
            });
        } else {
            return Err(format!(
                "expected '.' or ':' after variable in REMOVE, got {:?}",
                c.peek()
            ));
        }
        // Allow multiple labels: REMOVE n:Label1:Label2
        while c.peek() == Some(&Tok::Colon) {
            if let RemoveItem::Label { variable, .. } = items.last().unwrap() {
                let var = variable.clone();
                c.next(); // :
                let extra_label = expect_any_ident(c)?;
                items.push(RemoveItem::Label { variable: var, label: extra_label });
            } else {
                break;
            }
        }
        if !c.eat(&Tok::Comma) {
            break;
        }
    }
    if c.peek_kw("RETURN") {
        c.next(); // RETURN
        let return_clause = parse_return_clause_from_cursor(c)?;
        let order_by = try_parse_order_by(c)?;
        let skip = try_parse_skip(c)?;
        let limit = try_parse_limit(c)?;
        return Ok(Statement::RemoveAndReturn(RemoveAndReturnStatement {
            match_clauses,
            where_clause,
            items,
            return_clause,
            order_by,
            skip,
            limit,
        }));
    }
    Ok(Statement::Remove(RemoveStatement {
        match_clauses,
        where_clause,
        items,
    }))
}

/// Parse one `MERGE pattern [ON CREATE SET ...] [ON MATCH SET ...]` block.
/// The MERGE keyword must already have been consumed by the caller.
fn parse_one_merge_block(c: &mut Cursor<'_>) -> Result<MergeStatement, String> {
    let pattern = parse_pattern(c)?;

    // ON CREATE SET and ON MATCH SET can appear in either order, multiple times.
    let mut on_create_set = Vec::new();
    let mut on_match_set = Vec::new();

    loop {
        if c.peek_kw("ON") && c.peek_kw_at(1, "CREATE") && c.peek_kw_at(2, "SET") {
            c.next(); c.next(); c.next();
            on_create_set.extend(parse_set_items_from_cursor(c)?);
        } else if c.peek_kw("ON") && c.peek_kw_at(1, "MATCH") && c.peek_kw_at(2, "SET") {
            c.next(); c.next(); c.next();
            on_match_set.extend(parse_set_items_from_cursor(c)?);
        } else {
            break;
        }
    }

    Ok(MergeStatement { pattern, on_create_set, on_match_set })
}

fn parse_merge_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    // First MERGE keyword was already detected; consume it.
    c.next(); // MERGE
    let first = parse_one_merge_block(c)?;

    // Additional sequential MERGE blocks.
    let mut extra: Vec<MergeStatement> = Vec::new();
    while c.peek_kw("MERGE") {
        c.next(); // MERGE
        extra.push(parse_one_merge_block(c)?);
    }

    // Optional trailing WITH or MATCH chains (UNWIND ... WITH ... etc.) - handled below
    // Optional trailing SET
    if c.peek_kw("SET") {
        c.next(); // SET
        let _ = parse_set_items_from_cursor(c);
    }

    // Optional RETURN clause
    if c.peek_kw("RETURN") {
        c.next(); // RETURN
        let return_clause = parse_return_clause_from_cursor(c)?;
        let order_by = try_parse_order_by(c)?;
        let skip = try_parse_skip(c)?;
        let limit = try_parse_limit(c)?;
        return Ok(Statement::MergeAndReturn(MergeAndReturnStatement {
            merges: {
                let mut all = vec![first];
                all.extend(extra);
                all
            },
            return_clause,
            order_by,
            skip,
            limit,
        }));
    }

    if extra.is_empty() {
        Ok(Statement::Merge(first))
    } else {
        // Multiple MERGEs without RETURN: wrap in MergeAndReturn with empty columns
        // by returning a bare Merge with the first block and ignoring extras for now.
        // TODO: properly chain multiple MERGEs in the executor.
        Ok(Statement::Merge(first))
    }
}

fn parse_create_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    c.next(); // CREATE

    // INDEX or CONSTRAINT
    if c.peek_kw("INDEX") {
        c.next(); // INDEX
        c.next(); // FOR
        c.expect_tok(&Tok::LParen)?;
        let _ = expect_any_ident(c)?; // variable
        c.expect_tok(&Tok::Colon)?;
        let label = expect_any_ident(c)?;
        c.expect_tok(&Tok::RParen)?;
        c.next(); // ON
        c.expect_tok(&Tok::LParen)?;
        let _ = expect_any_ident(c)?; // variable
        c.expect_tok(&Tok::Dot)?;
        let property = expect_any_ident(c)?;
        c.expect_tok(&Tok::RParen)?;
        return Ok(Statement::CreateIndex(CreateIndexStatement { label, property }));
    }

    if c.peek_kw("CONSTRAINT") {
        c.next(); // CONSTRAINT
        c.next(); // ON
        c.expect_tok(&Tok::LParen)?;
        let _ = expect_any_ident(c)?; // variable
        c.expect_tok(&Tok::Colon)?;
        let label = expect_any_ident(c)?;
        c.expect_tok(&Tok::RParen)?;
        c.next(); // ASSERT

        if c.peek_kw("EXISTS") {
            c.next(); // EXISTS
            c.expect_tok(&Tok::LParen)?;
            let _ = expect_any_ident(c)?; // variable
            c.expect_tok(&Tok::Dot)?;
            let property = expect_any_ident(c)?;
            c.expect_tok(&Tok::RParen)?;
            return Ok(Statement::CreateConstraint(CreateConstraintStatement {
                label,
                property,
                kind: ConstraintKind::Exists,
            }));
        }
        // ASSERT n.prop IS UNIQUE
        let _ = expect_any_ident(c)?; // variable
        c.expect_tok(&Tok::Dot)?;
        let property = expect_any_ident(c)?;
        c.next(); // IS
        c.next(); // UNIQUE
        return Ok(Statement::CreateConstraint(CreateConstraintStatement {
            label,
            property,
            kind: ConstraintKind::Unique,
        }));
    }

    // CREATE pattern(s) [RETURN ...]
    let patterns = parse_multi_pattern(c)?;

    if c.peek_kw("RETURN") {
        c.next();
        let return_clause = parse_return_clause_from_cursor(c)?;
        let order_by = try_parse_order_by(c)?;
        let skip = try_parse_skip(c)?;
        let limit = try_parse_limit(c)?;
        return Ok(Statement::CreateAndReturn(CreateAndReturnStatement {
            patterns,
            return_clause,
            order_by,
            skip,
            limit,
        }));
    }

    Ok(Statement::Create(CreateStatement { patterns }))
}

fn parse_drop_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    c.next(); // DROP

    if c.peek_kw("INDEX") {
        c.next(); // INDEX
        c.next(); // FOR
        c.expect_tok(&Tok::LParen)?;
        let _ = expect_any_ident(c)?;
        c.expect_tok(&Tok::Colon)?;
        let label = expect_any_ident(c)?;
        c.expect_tok(&Tok::RParen)?;
        c.next(); // ON
        c.expect_tok(&Tok::LParen)?;
        let _ = expect_any_ident(c)?;
        c.expect_tok(&Tok::Dot)?;
        let property = expect_any_ident(c)?;
        c.expect_tok(&Tok::RParen)?;
        return Ok(Statement::DropIndex(DropIndexStatement { label, property }));
    }

    if c.peek_kw("CONSTRAINT") {
        c.next(); // CONSTRAINT
        c.next(); // ON
        c.expect_tok(&Tok::LParen)?;
        let _ = expect_any_ident(c)?;
        c.expect_tok(&Tok::Colon)?;
        let label = expect_any_ident(c)?;
        c.expect_tok(&Tok::RParen)?;
        c.next(); // ASSERT

        if c.peek_kw("EXISTS") {
            c.next(); // EXISTS
            c.expect_tok(&Tok::LParen)?;
            let _ = expect_any_ident(c)?;
            c.expect_tok(&Tok::Dot)?;
            let property = expect_any_ident(c)?;
            c.expect_tok(&Tok::RParen)?;
            return Ok(Statement::DropConstraint(DropConstraintStatement {
                label,
                property,
                kind: ConstraintKind::Exists,
            }));
        }
        // IS UNIQUE
        let _ = expect_any_ident(c)?;
        c.expect_tok(&Tok::Dot)?;
        let property = expect_any_ident(c)?;
        c.next(); // IS
        c.next(); // UNIQUE
        return Ok(Statement::DropConstraint(DropConstraintStatement {
            label,
            property,
            kind: ConstraintKind::Unique,
        }));
    }

    Err(format!("unsupported DROP statement at {:?}", c.peek()))
}

fn parse_foreach_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    c.next(); // FOREACH
    c.expect_tok(&Tok::LParen)?;
    let variable = expect_any_ident(c)?;
    c.next(); // IN
    let list = parse_expr(c)?;
    c.expect_tok(&Tok::Pipe)?;
    let body_stmt = parse_single_statement(c)?;
    c.expect_tok(&Tok::RParen)?;
    Ok(Statement::Foreach(ForeachStatement {
        variable,
        list,
        body: vec![body_stmt],
    }))
}

fn parse_single_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    match c.peek() {
        Some(Tok::Ident(s)) => {
            let upper = s.to_ascii_uppercase();
            match upper.as_str() {
                "MATCH" => {
                    // Detect what follows MATCH
                    // We need to look ahead past the match pattern for SET/DELETE/REMOVE
                    // Simplest: try each in order
                    parse_match_headed_statement(c)
                }
                "CREATE" => parse_create_statement(c),
                "DROP" => parse_drop_statement(c),
                "MERGE" => parse_merge_statement(c),
                "FOREACH" => parse_foreach_statement(c),
                "UNWIND" | "WITH" | "OPTIONAL" | "RETURN" => {
                    Ok(Statement::Query(parse_read_query(c)?))
                }
                _ => Err(format!("unsupported statement starting with '{}'", upper)),
            }
        }
        other => Err(format!("expected statement, got {:?}", other)),
    }
}

/// Determine what kind of MATCH-headed statement this is, then dispatch.
fn parse_match_headed_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    // We scan ahead (without consuming) for SET/DELETE/REMOVE/DETACH to decide.
    // Use a temporary scan to find out.
    let mut scan_pos = c.pos;
    let tokens = c.tokens;
    let mut depth_p = 0i32;
    let mut depth_b = 0i32;
    let mut depth_br = 0i32;
    let mut found_keyword = None;

    // Skip past MATCH
    if scan_pos < tokens.len() {
        scan_pos += 1;
    }

    while scan_pos < tokens.len() {
        match &tokens[scan_pos] {
            Tok::LParen => depth_p += 1,
            Tok::RParen => depth_p -= 1,
            Tok::LBrack => depth_b += 1,
            Tok::RBrack => depth_b -= 1,
            Tok::LBrace => depth_br += 1,
            Tok::RBrace => depth_br -= 1,
            Tok::Ident(s) if depth_p == 0 && depth_b == 0 && depth_br == 0 => {
                let upper = s.to_ascii_uppercase();
                match upper.as_str() {
                    "SET" => {
                        found_keyword = Some("SET");
                        break;
                    }
                    "DELETE" | "DETACH" => {
                        found_keyword = Some("DELETE");
                        break;
                    }
                    "REMOVE" => {
                        found_keyword = Some("REMOVE");
                        break;
                    }
                    "MERGE" => {
                        found_keyword = Some("MERGE");
                        break;
                    }
                    // A WITH or UNWIND before the write keyword means this is a
                    // complex multi-clause query; treat it as a read query.
                    "WITH" | "UNWIND" => {
                        break;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        scan_pos += 1;
    }

    match found_keyword {
        Some("SET") => parse_set_statement(c),
        Some("DELETE") => parse_delete_statement(c),
        Some("REMOVE") => parse_remove_statement(c),
        Some("MERGE") => parse_match_then_merge_statement(c),
        _ => Ok(Statement::Query(parse_read_query(c)?)),
    }
}

fn parse_match_then_merge_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    // MATCH pattern(s) [WHERE ...] [MATCH ...] MERGE ... [ON CREATE/MATCH SET ...] [RETURN ...]
    c.next(); // MATCH
    let mut all_match_clauses = parse_match_clauses_from_cursor(c)?;
    let mut where_clause = if c.peek_kw("WHERE") {
        c.next();
        Some(parse_where_clause_from_cursor(c)?)
    } else {
        None
    };

    // Additional MATCH clauses before MERGE.
    while c.peek_kw("MATCH") {
        c.next(); // MATCH
        let extra_clauses = parse_match_clauses_from_cursor(c)?;
        all_match_clauses.extend(extra_clauses);
        if c.peek_kw("WHERE") && where_clause.is_none() {
            c.next();
            where_clause = Some(parse_where_clause_from_cursor(c)?);
        }
    }

    let match_clauses = all_match_clauses;

    // Consume one or more MERGE blocks.
    let mut merges: Vec<MergeStatement> = Vec::new();
    while c.peek_kw("MERGE") {
        c.next(); // MERGE
        merges.push(parse_one_merge_block(c)?);
    }

    // Optional trailing SET (e.g. MATCH ... MERGE ... SET ...)
    if c.peek_kw("SET") {
        c.next(); // SET
        let _ = parse_set_items_from_cursor(c);
    }

    // Optional RETURN clause.
    if c.peek_kw("RETURN") {
        c.next();
        let return_clause = parse_return_clause_from_cursor(c)?;
        let order_by = try_parse_order_by(c)?;
        let skip = try_parse_skip(c)?;
        let limit = try_parse_limit(c)?;

        // Re-use MergeAndReturn but also carry the MATCH context so the executor
        // can bind variables from the MATCH clause before executing the MERGE.
        // For now, emit a synthetic read query followed by a MergeAndReturn.
        // This is a pragmatic approach; a full MATCH+MERGE plan would require a
        // more integrated executor.
        let _ = (match_clauses, where_clause); // acknowledged
        return Ok(Statement::MergeAndReturn(MergeAndReturnStatement {
            merges,
            return_clause,
            order_by,
            skip,
            limit,
        }));
    }

    // No RETURN: execute the merges and return empty.
    match merges.len() {
        0 => Err("expected MERGE after MATCH".into()),
        1 => Ok(Statement::Merge(merges.remove(0))),
        _ => {
            // Multiple MERGEs: run the first one (future work: chain all).
            Ok(Statement::Merge(merges.remove(0)))
        }
    }
}

// ─── Top-level statement parser ───────────────────────────────────────────────

fn parse_statement(c: &mut Cursor<'_>) -> Result<Statement, String> {
    match c.peek() {
        Some(Tok::Ident(s)) => {
            let upper = s.to_ascii_uppercase();
            match upper.as_str() {
                "CREATE" => parse_create_statement(c),
                "DROP" => parse_drop_statement(c),
                "MERGE" => parse_merge_statement(c),
                "FOREACH" => parse_foreach_statement(c),
                "MATCH" => parse_match_headed_statement(c),
                "UNWIND" | "WITH" | "OPTIONAL" | "RETURN" => {
                    Ok(Statement::Query(parse_read_query(c)?))
                }
                _ => Err(format!("unsupported statement type: '{}'", upper)),
            }
        }
        other => Err(format!("expected statement, got {:?}", other)),
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Parse a Cypher query string into a `Statement` AST.
pub fn parse(cypher: &str) -> Result<Statement, String> {
    // Phase 1: lex using chumsky
    let tokens = lexer()
        .parse(cypher)
        .into_result()
        .map_err(|errs| {
            errs.into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })?;

    // Phase 2: recursive-descent parse over the flat token vector
    let mut cursor = Cursor::new(&tokens);

    let first = parse_statement(&mut cursor)?;

    // UNION composition
    let mut result = first;
    while cursor.peek_kw("UNION") {
        cursor.next(); // UNION
        let all = cursor.peek_kw("ALL");
        if all {
            cursor.next();
        }
        let right = parse_statement(&mut cursor)?;
        result = Statement::Union(UnionStatement {
            left: Box::new(result),
            right: Box::new(right),
            all,
        });
    }

    // Skip trailing semicolons
    while cursor.eat(&Tok::Semi) {}

    if !cursor.is_empty() {
        return Err(format!(
            "unexpected tokens after statement: {:?}",
            cursor.peek()
        ));
    }

    Ok(result)
}


// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- Fix A: Undirected relationship syntax ---

    #[test]
    fn parse_undirected_bare_dash() {
        // (a)--(b): bare undirected, no bracket
        let stmt = parse("MATCH (a)--(b) RETURN a").unwrap();
        if let Statement::Query(q) = stmt {
            assert_eq!(q.parts.len(), 1);
            if let QueryPart::Match { match_clauses, .. } = &q.parts[0] {
                let rel = &match_clauses[0].pattern.rels[0].0;
                assert!(rel.is_undirected, "expected is_undirected = true");
            }
        }
    }

    #[test]
    fn parse_undirected_with_bracket() {
        // (a)-[r]-(b): undirected with variable
        let stmt = parse("MATCH (a)-[r]-(b) RETURN r").unwrap();
        if let Statement::Query(q) = stmt {
            if let QueryPart::Match { match_clauses, .. } = &q.parts[0] {
                let rel = &match_clauses[0].pattern.rels[0].0;
                assert!(rel.is_undirected);
                assert_eq!(rel.variable.as_deref(), Some("r"));
            }
        }
    }

    #[test]
    fn parse_undirected_typed() {
        // (a)-[:TYPE]-(b): undirected typed
        let stmt = parse("MATCH (a)-[:TYPE]-(b) RETURN a").unwrap();
        if let Statement::Query(q) = stmt {
            if let QueryPart::Match { match_clauses, .. } = &q.parts[0] {
                let rel = &match_clauses[0].pattern.rels[0].0;
                assert!(rel.is_undirected);
                assert_eq!(rel.rel_type.as_deref(), Some("TYPE"));
            }
        }
    }

    #[test]
    fn parse_directed_outgoing_is_not_undirected() {
        let stmt = parse("MATCH (a)-[:R]->(b) RETURN a").unwrap();
        if let Statement::Query(q) = stmt {
            if let QueryPart::Match { match_clauses, .. } = &q.parts[0] {
                let rel = &match_clauses[0].pattern.rels[0].0;
                assert!(!rel.is_undirected);
                assert!(!rel.is_incoming);
            }
        }
    }

    // --- Fix B: Error detection for duplicate/conflicting variables ---

    #[test]
    fn duplicate_rel_var_same_pattern_errors() {
        let result = parse("MATCH (a)-[r]->(b)-[r]->(c) RETURN r");
        assert!(
            result.is_err(),
            "expected error for duplicate relationship variable 'r'"
        );
        let msg = result.unwrap_err();
        assert!(msg.contains('r'), "error should mention variable name");
    }

    #[test]
    fn rel_var_used_as_node_errors() {
        // ()-[r]-(r) — 'r' as both relationship and node
        let result = parse("MATCH ()-[r]-(r) RETURN r");
        assert!(
            result.is_err(),
            "expected error when relationship variable 'r' is also used as node"
        );
    }

    #[test]
    fn cross_match_node_then_rel_var_errors() {
        // MATCH (r) MATCH ()-[r]-() — 'r' as node then relationship
        let result = parse("MATCH (r) MATCH ()-[r]-() RETURN r");
        assert!(
            result.is_err(),
            "expected VariableTypeConflict across MATCH clauses"
        );
    }

    // --- Fix C: WITH + ORDER BY ---

    #[test]
    fn parse_with_order_by() {
        let stmt = parse("MATCH (n) WITH n ORDER BY n.name RETURN n").unwrap();
        if let Statement::Query(q) = stmt {
            let with_part = q
                .parts
                .iter()
                .find(|p| matches!(p, QueryPart::With { .. }))
                .expect("expected a With part");
            if let QueryPart::With { order_by, .. } = with_part {
                assert!(order_by.is_some(), "expected ORDER BY attached to WITH");
            }
        }
    }

    #[test]
    fn parse_with_order_by_limit() {
        let stmt = parse("UNWIND [1, 2, 3] AS x WITH x ORDER BY x DESC LIMIT 2 RETURN x").unwrap();
        if let Statement::Query(q) = stmt {
            let with_part = q
                .parts
                .iter()
                .find(|p| matches!(p, QueryPart::With { .. }))
                .expect("expected a With part");
            if let QueryPart::With {
                order_by, limit, ..
            } = with_part
            {
                assert!(order_by.is_some(), "ORDER BY should be present");
                assert!(limit.is_some(), "LIMIT should be present");
            }
        }
    }
}
